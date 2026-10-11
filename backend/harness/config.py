"""Persistent settings and paths.

Everything lives under HARNESS_HOME (default ~/.llm-harness). Models default to
~/models on the WSL ext4 filesystem: keeping weights off /mnt/c avoids the 9P
bridge, which is an order of magnitude slower for multi-GB reads.
"""

from __future__ import annotations

import json
import os
import threading
from pathlib import Path
from typing import Any

from pydantic import BaseModel, Field

HARNESS_HOME = Path(os.environ.get("HARNESS_HOME", Path.home() / ".llm-harness")).expanduser()
REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_DIR = REPO_ROOT / "scripts"
FRONTEND_DIST = REPO_ROOT / "frontend" / "dist"


class Settings(BaseModel):
    models_dir: str = str(Path.home() / "models")
    hf_token: str = ""
    hf_endpoint: str = "https://huggingface.co"
    # llama.cpp build produced by scripts/wsl/build-llama-cpp.sh
    llama_cpp_dir: str = str(HARNESS_HOME / "llama.cpp")
    vllm_venv: str = str(HARNESS_HOME / "venvs" / "vllm")
    ollama_url: str = "http://127.0.0.1:11434"
    max_concurrent_downloads: int = 2
    # parallel HTTP range connections per file; HF's CDN caps per-connection speed
    connections_per_file: int = 8
    default_host: str = "127.0.0.1"  # LAN へ公開するなら 0.0.0.0
    port_range_start: int = 8080
    extra_env: dict[str, str] = Field(default_factory=dict)


class _Store:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        HARNESS_HOME.mkdir(parents=True, exist_ok=True)
        self.path = HARNESS_HOME / "settings.json"
        self._settings = self._load()

    def _load(self) -> Settings:
        if self.path.exists():
            try:
                return Settings(**json.loads(self.path.read_text()))
            except Exception:
                pass
        s = Settings()
        if not s.hf_token:
            s.hf_token = os.environ.get("HF_TOKEN", "")
        return s

    def get(self) -> Settings:
        return self._settings

    def update(self, patch: dict[str, Any]) -> Settings:
        with self._lock:
            data = self._settings.model_dump()
            data.update({k: v for k, v in patch.items() if k in data})
            self._settings = Settings(**data)
            self.path.write_text(json.dumps(self._settings.model_dump(), indent=2))
            Path(self._settings.models_dir).expanduser().mkdir(parents=True, exist_ok=True)
            return self._settings


store = _Store()


def settings() -> Settings:
    return store.get()


def models_dir() -> Path:
    p = Path(settings().models_dir).expanduser()
    p.mkdir(parents=True, exist_ok=True)
    return p


def read_json(name: str, default: Any) -> Any:
    p = HARNESS_HOME / name
    if not p.exists():
        return default
    try:
        return json.loads(p.read_text())
    except Exception:
        return default


def write_json(name: str, data: Any) -> None:
    p = HARNESS_HOME / name
    tmp = p.with_suffix(p.suffix + ".tmp")
    tmp.write_text(json.dumps(data, indent=2))
    tmp.replace(p)
