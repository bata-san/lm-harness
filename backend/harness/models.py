"""Local model inventory: GGUF files (llama.cpp) and HF safetensors dirs (vLLM)."""

from __future__ import annotations

import re
import shutil
from pathlib import Path

from . import gguf
from .config import models_dir

_SPLIT = re.compile(r"^(?P<base>.+)-(?P<idx>\d{5})-of-(?P<total>\d{5})\.gguf$")
_meta_cache: dict[tuple[str, float], dict] = {}


def _gguf_info(path: Path) -> dict | None:
    key = (str(path), path.stat().st_mtime)
    if key not in _meta_cache:
        try:
            _meta_cache[key] = gguf.summarize(gguf.read_metadata(path))
        except Exception as e:  # truncated / still downloading
            _meta_cache[key] = {"error": str(e)}
    return _meta_cache[key]


def scan() -> list[dict]:
    root = models_dir()
    out: list[dict] = []
    splits: dict[str, list[Path]] = {}

    for p in sorted(root.rglob("*.gguf")):
        if p.name.endswith(".part"):
            continue
        m = _SPLIT.match(p.name)
        if m:
            splits.setdefault(str(p.parent / m["base"]), []).append(p)
            continue
        if "mmproj" in p.name.lower():
            continue
        out.append(_gguf_entry(root, p, [p]))

    for _, parts in splits.items():
        parts.sort()
        out.append(_gguf_entry(root, parts[0], parts))

    for cfg in root.rglob("config.json"):
        d = cfg.parent
        weights = list(d.glob("*.safetensors"))
        if not weights:
            continue
        out.append(
            {
                "id": str(d.relative_to(root)),
                "format": "safetensors",
                "path": str(d),
                "name": d.name,
                "size": sum(w.stat().st_size for w in weights),
                "files": len(weights),
                "runtimes": ["vllm"],
                "info": {},
            }
        )
    return sorted(out, key=lambda m: m["id"])


def _gguf_entry(root: Path, first: Path, parts: list[Path]) -> dict:
    mmproj = sorted(q for q in first.parent.glob("*.gguf") if "mmproj" in q.name.lower())
    return {
        "id": str(first.relative_to(root)),
        "format": "gguf",
        "path": str(first),
        "name": first.name,
        "size": sum(q.stat().st_size for q in parts),
        "files": len(parts),
        "mmproj": str(mmproj[0]) if mmproj else None,
        "runtimes": ["llama.cpp", "ollama"],
        "info": _gguf_info(first),
    }


def resolve(model_id: str) -> Path:
    root = models_dir().resolve()
    p = (root / model_id).resolve()
    if root not in p.parents and p != root:
        raise ValueError("path outside models dir")
    return p


def delete(model_id: str) -> None:
    p = resolve(model_id)
    if p.is_dir():
        shutil.rmtree(p)
        return
    m = _SPLIT.match(p.name)
    targets = list(p.parent.glob(f"{m['base']}-*-of-{m['total']}.gguf")) if m else [p]
    for t in targets:
        t.unlink(missing_ok=True)
