"""Runtime adapters: detection, command construction, health and metrics."""

from __future__ import annotations

import asyncio
import os
import re
import shutil
import subprocess
from pathlib import Path

import httpx

from .config import SCRIPTS_DIR, settings

RUNTIMES = ("llama.cpp", "vllm", "ollama")


def llama_server_bin() -> str | None:
    d = Path(settings().llama_cpp_dir).expanduser()
    for cand in (d / "build" / "bin" / "llama-server", d / "bin" / "llama-server"):
        if cand.exists():
            return str(cand)
    return shutil.which("llama-server")


def vllm_bin() -> str | None:
    cand = Path(settings().vllm_venv).expanduser() / "bin" / "vllm"
    return str(cand) if cand.exists() else shutil.which("vllm")


def ollama_bin() -> str | None:
    return shutil.which("ollama")


def _run(cmd: list[str], timeout: float = 10) -> str:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return (r.stdout + r.stderr).strip()
    except Exception as e:
        return f"error: {e}"


def _llama_version(binary: str) -> str:
    out = _run([binary, "--version"])
    m = re.search(r"version:\s*(\S+(?:\s*\([0-9a-f]+\))?)", out)
    cuda = "CUDA" if re.search(r"CUDA|ggml_cuda", out) else ""
    return " ".join(x for x in (m.group(1) if m else out.splitlines()[-1] if out else "?", cuda) if x)


def _vllm_version() -> str | None:
    py = Path(settings().vllm_venv).expanduser() / "bin" / "python"
    if not py.exists():
        return None
    out = _run([str(py), "-c", "import importlib.metadata as m; print(m.version('vllm'))"])
    return out.splitlines()[-1] if out else None


async def ollama_api(path: str, method: str = "GET", json: dict | None = None, timeout: float = 3) -> dict | None:
    try:
        async with httpx.AsyncClient(timeout=timeout) as c:
            r = await c.request(method, settings().ollama_url + path, json=json)
            r.raise_for_status()
            return r.json() if r.content else {}
    except Exception:
        return None


async def status() -> list[dict]:
    loop = asyncio.get_running_loop()
    llama = llama_server_bin()
    vllm = vllm_bin()
    ollama = ollama_bin()
    llama_ver = await loop.run_in_executor(None, _llama_version, llama) if llama else None
    vllm_ver = await loop.run_in_executor(None, _vllm_version) if vllm else None
    ollama_ver = await ollama_api("/api/version")
    return [
        {
            "id": "llama.cpp",
            "installed": bool(llama),
            "path": llama,
            "version": llama_ver,
            "install_job": "build-llama-cpp",
            "description": "GGUF 推論の本命。CUDA (sm_86) ネイティブビルドで最速。",
        },
        {
            "id": "vllm",
            "installed": bool(vllm),
            "path": vllm,
            "version": vllm_ver,
            "install_job": "install-vllm",
            "description": "safetensors / AWQ / GPTQ 向けの高スループットサーバ。並列リクエストに強い。",
        },
        {
            "id": "ollama",
            "installed": bool(ollama),
            "path": ollama,
            "version": (ollama_ver or {}).get("version"),
            "running": ollama_ver is not None,
            "install_job": "install-ollama",
            "description": "手軽さ重視。モデル管理込み。中身は llama.cpp ベース。",
        },
    ]


def build_command(runtime: str, model_path: str, host: str, port: int, args: list[str]) -> tuple[list[str], dict[str, str]]:
    env: dict[str, str] = dict(settings().extra_env)
    if runtime == "llama.cpp":
        exe = llama_server_bin()
        if not exe:
            raise RuntimeError("llama-server が見つかりません。Runtimes から llama.cpp をビルドしてください")
        return [exe, "-m", model_path, "--host", host, "--port", str(port), *args], env
    if runtime == "vllm":
        exe = vllm_bin()
        if not exe:
            raise RuntimeError("vLLM が見つかりません。Runtimes からインストールしてください")
        env.setdefault("VLLM_USE_V1", "1")
        return [exe, "serve", model_path, "--host", host, "--port", str(port), *args], env
    if runtime == "ollama":
        exe = ollama_bin()
        if not exe:
            raise RuntimeError("ollama が見つかりません")
        env.update(
            {
                "OLLAMA_HOST": f"{host}:{port}",
                "OLLAMA_FLASH_ATTENTION": "1",
                "OLLAMA_KV_CACHE_TYPE": "q8_0",
                "OLLAMA_NUM_PARALLEL": "2",
                "OLLAMA_KEEP_ALIVE": "30m",
            }
        )
        return [exe, "serve", *args], env
    raise ValueError(f"unknown runtime {runtime}")


def default_args(runtime: str) -> list[str]:
    if runtime == "vllm":
        return ["--gpu-memory-utilization", "0.92", "--max-model-len", "32768", "--enable-prefix-caching"]
    return []


def health_path(runtime: str) -> str:
    return {"llama.cpp": "/health", "vllm": "/health", "ollama": "/"}[runtime]


_METRIC_RE = re.compile(r"^([a-zA-Z_:]+)(?:\{[^}]*\})?\s+([0-9.eE+-]+|NaN)$")
_WANTED = {
    "llamacpp:prompt_tokens_seconds": "prompt_tps",
    "llamacpp:predicted_tokens_seconds": "gen_tps",
    "llamacpp:requests_processing": "processing",
    "llamacpp:requests_deferred": "deferred",
    "llamacpp:tokens_predicted_total": "tokens_total",
    "llamacpp:kv_cache_usage_ratio": "kv_usage",
    "vllm:num_requests_running": "processing",
    "vllm:num_requests_waiting": "deferred",
    "vllm:generation_tokens_total": "tokens_total",
    "vllm:gpu_cache_usage_perc": "kv_usage",
    "vllm:kv_cache_usage_perc": "kv_usage",
}


def parse_metrics(text: str) -> dict[str, float]:
    out: dict[str, float] = {}
    for line in text.splitlines():
        m = _METRIC_RE.match(line)
        if m and m.group(1) in _WANTED:
            try:
                out[_WANTED[m.group(1)]] = out.get(_WANTED[m.group(1)], 0) + float(m.group(2))
            except ValueError:
                pass
    return out


JOBS = {
    "setup": {
        "title": "WSL 基本セットアップ (apt / CUDA Toolkit / uv)",
        "script": "wsl/setup.sh",
        "sudo": True,
    },
    "build-llama-cpp": {
        "title": "llama.cpp を CUDA でビルド / 更新",
        "script": "wsl/build-llama-cpp.sh",
        "sudo": False,
    },
    "install-vllm": {
        "title": "vLLM を専用 venv にインストール / 更新",
        "script": "wsl/install-vllm.sh",
        "sudo": False,
    },
    "install-ollama": {
        "title": "Ollama をインストール / 更新",
        "script": "wsl/install-ollama.sh",
        "sudo": True,
    },
}


def job_command(job_id: str) -> tuple[list[str], dict[str, str]]:
    job = JOBS[job_id]
    s = settings()
    env = {
        "LLAMA_CPP_DIR": str(Path(s.llama_cpp_dir).expanduser()),
        "VLLM_VENV": str(Path(s.vllm_venv).expanduser()),
        "PATH": os.environ.get("PATH", "") + ":/usr/local/cuda/bin",
    }
    return ["bash", str(SCRIPTS_DIR / job["script"])], env
