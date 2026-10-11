"""Host telemetry: GPU (nvidia-smi), CPU, memory, disk and WSL sanity checks."""

from __future__ import annotations

import os
import platform
import shutil
import subprocess
from functools import lru_cache
from pathlib import Path

import psutil

from .config import models_dir

GPU_FIELDS = [
    "index",
    "name",
    "memory.total",
    "memory.used",
    "memory.free",
    "utilization.gpu",
    "utilization.memory",
    "temperature.gpu",
    "power.draw",
    "power.limit",
    "clocks.sm",
    "clocks.max.sm",
    "pcie.link.gen.current",
    "pcie.link.width.current",
    "driver_version",
]


@lru_cache(maxsize=1)
def nvidia_smi() -> str | None:
    # WSL exposes the Windows driver's tools under /usr/lib/wsl/lib
    for cand in (shutil.which("nvidia-smi"), "/usr/lib/wsl/lib/nvidia-smi"):
        if cand and Path(cand).exists():
            return cand
    return None


def _num(v: str) -> float | None:
    try:
        return float(v)
    except ValueError:
        return None


def gpus() -> list[dict]:
    exe = nvidia_smi()
    if not exe:
        return []
    try:
        out = subprocess.run(
            [exe, f"--query-gpu={','.join(GPU_FIELDS)}", "--format=csv,noheader,nounits"],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout
    except Exception:
        return []
    result = []
    for line in out.strip().splitlines():
        vals = [v.strip() for v in line.split(",")]
        if len(vals) != len(GPU_FIELDS):
            continue
        row = dict(zip(GPU_FIELDS, vals))
        result.append(
            {
                "index": int(row["index"]),
                "name": row["name"],
                "memory_total_mb": _num(row["memory.total"]),
                "memory_used_mb": _num(row["memory.used"]),
                "memory_free_mb": _num(row["memory.free"]),
                "util_gpu": _num(row["utilization.gpu"]),
                "util_mem": _num(row["utilization.memory"]),
                "temperature": _num(row["temperature.gpu"]),
                "power_draw": _num(row["power.draw"]),
                "power_limit": _num(row["power.limit"]),
                "clock_sm": _num(row["clocks.sm"]),
                "clock_sm_max": _num(row["clocks.max.sm"]),
                "pcie_gen": row["pcie.link.gen.current"],
                "pcie_width": row["pcie.link.width.current"],
                "driver": row["driver_version"],
            }
        )
    return result


@lru_cache(maxsize=1)
def is_wsl() -> bool:
    try:
        return "microsoft" in Path("/proc/version").read_text().lower()
    except OSError:
        return False


@lru_cache(maxsize=1)
def cpu_model() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def cpu_topology() -> dict:
    return {
        "model": cpu_model(),
        "physical_cores": psutil.cpu_count(logical=False) or os.cpu_count() or 1,
        "logical_cores": psutil.cpu_count(logical=True) or os.cpu_count() or 1,
    }


def snapshot() -> dict:
    vm = psutil.virtual_memory()
    sw = psutil.swap_memory()
    mdir = models_dir()
    du = shutil.disk_usage(mdir)
    freq = psutil.cpu_freq()
    return {
        "gpus": gpus(),
        "cpu": {
            **cpu_topology(),
            "percent": psutil.cpu_percent(interval=None),
            "per_core": psutil.cpu_percent(interval=None, percpu=True),
            "freq_mhz": freq.current if freq else None,
            "load_avg": list(os.getloadavg()),
        },
        "memory": {"total": vm.total, "used": vm.total - vm.available, "available": vm.available, "percent": vm.percent},
        "swap": {"total": sw.total, "used": sw.used},
        "disk": {"path": str(mdir), "total": du.total, "used": du.used, "free": du.free},
    }


def health_checks() -> list[dict]:
    """Things that silently cost performance on WSL2."""
    checks: list[dict] = []
    wsl = is_wsl()
    checks.append({"id": "wsl", "ok": wsl, "level": "info", "message": "WSL2 上で動作中" if wsl else "WSL ではないネイティブ Linux で動作中"})

    gs = gpus()
    checks.append(
        {
            "id": "gpu",
            "ok": bool(gs),
            "level": "error",
            "message": f"GPU 検出: {', '.join(g['name'] for g in gs)}" if gs else "nvidia-smi が見つからない / GPU 未検出 (Windows 側 NVIDIA ドライバを確認)",
        }
    )
    distro_libcuda = Path("/usr/lib/x86_64-linux-gnu/libcuda.so.1")
    if wsl and distro_libcuda.exists() and not distro_libcuda.resolve().as_posix().startswith("/usr/lib/wsl/"):
        checks.append(
            {"id": "libcuda", "ok": False, "level": "warn", "message": "WSL 内に Linux 用 libcuda が入っています。WSL では Windows ドライバの /usr/lib/wsl/lib を使うべきです"}
        )

    nvcc = shutil.which("nvcc") or (Path("/usr/local/cuda/bin/nvcc").exists() and "/usr/local/cuda/bin/nvcc")
    checks.append({"id": "nvcc", "ok": bool(nvcc), "level": "warn", "message": "CUDA Toolkit (nvcc) あり" if nvcc else "nvcc なし: llama.cpp の CUDA ビルドに必要 (scripts/wsl/setup.sh)"})

    mdir = str(models_dir())
    on_mnt = mdir.startswith("/mnt/")
    checks.append(
        {
            "id": "models_fs",
            "ok": not on_mnt,
            "level": "warn",
            "message": f"モデル置き場 {mdir} は Windows ドライブ (9P) 上で遅い。~/models など ext4 側を推奨" if on_mnt else f"モデル置き場 {mdir} は Linux FS 上 (高速)",
        }
    )

    vm = psutil.virtual_memory()
    checks.append(
        {
            "id": "memory",
            "ok": vm.total >= 48 * 1024**3,
            "level": "warn",
            "message": f"WSL に割り当て済みメモリ {vm.total / 1024**3:.0f} GiB"
            + ("" if vm.total >= 48 * 1024**3 else " — .wslconfig の memory= を増やすと大きいモデルの CPU オフロードが安定"),
        }
    )
    topo = cpu_topology()
    checks.append({"id": "cpu", "ok": True, "level": "info", "message": f"{topo['model']} / {topo['physical_cores']}C{topo['logical_cores']}T が WSL から見えています"})

    gov = Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    if gov.exists():
        g = gov.read_text().strip()
        checks.append({"id": "governor", "ok": g == "performance", "level": "info", "message": f"CPU governor: {g}"})
    return checks
