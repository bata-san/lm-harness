"""Thin Hugging Face Hub API client (search + file listing)."""

from __future__ import annotations

import re

import httpx

from .config import settings


def headers() -> dict[str, str]:
    h = {"User-Agent": "llm-harness/0.1"}
    if settings().hf_token:
        h["Authorization"] = f"Bearer {settings().hf_token}"
    return h


def resolve_url(repo: str, path: str, revision: str = "main") -> str:
    return f"{settings().hf_endpoint}/{repo}/resolve/{revision}/{path}"


async def search(query: str, gguf_only: bool = True, limit: int = 30) -> list[dict]:
    params: dict = {"search": query, "sort": "downloads", "direction": -1, "limit": limit}
    if gguf_only:
        params["filter"] = "gguf"
    async with httpx.AsyncClient(headers=headers(), timeout=20) as c:
        r = await c.get(f"{settings().hf_endpoint}/api/models", params=params)
        r.raise_for_status()
        return [
            {
                "id": m["id"],
                "downloads": m.get("downloads", 0),
                "likes": m.get("likes", 0),
                "updated": m.get("lastModified"),
                "pipeline_tag": m.get("pipeline_tag"),
                "tags": m.get("tags", [])[:12],
            }
            for m in r.json()
        ]


_QUANT = re.compile(r"(IQ\d_[A-Z]+|Q\d_K(?:_[SML]|_XL)?|Q\d_\d|Q\d_K|BF16|F16|F32|MXFP4)", re.I)
_SPLIT = re.compile(r"-(\d{5})-of-(\d{5})\.gguf$")


async def files(repo: str, revision: str = "main") -> list[dict]:
    async with httpx.AsyncClient(headers=headers(), timeout=30) as c:
        r = await c.get(f"{settings().hf_endpoint}/api/models/{repo}/tree/{revision}", params={"recursive": "true"})
        r.raise_for_status()
        out = []
        for f in r.json():
            if f.get("type") != "file":
                continue
            size = (f.get("lfs") or {}).get("size") or f.get("size") or 0
            path = f["path"]
            q = _QUANT.search(path.rsplit("/", 1)[-1])
            out.append(
                {
                    "path": path,
                    "size": size,
                    "quant": q.group(1).upper() if q else None,
                    "split": bool(_SPLIT.search(path)),
                    "sha256": (f.get("lfs") or {}).get("oid"),
                }
            )
        return out
