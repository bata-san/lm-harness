"""Download manager.

* Hugging Face files are fetched with several parallel HTTP Range connections
  per file (the HF/Xet CDN throttles single connections) into a pre-allocated
  ``.part`` file. Per-segment progress is persisted, so pause / restart of the
  harness / WSL shutdown all resume where they left off.
* Ollama pulls are proxied through the Ollama API so their progress shows up
  in the same queue.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import time
import uuid
from collections import deque
from pathlib import Path
from typing import Literal

import httpx

from . import hf
from .config import models_dir, read_json, settings, write_json

Status = Literal["queued", "downloading", "verifying", "paused", "completed", "failed", "cancelled"]
CHUNK = 4 * 1024 * 1024
MIN_SEGMENT = 64 * 1024 * 1024


class Task:
    def __init__(self, **kw) -> None:
        self.id: str = kw.get("id") or uuid.uuid4().hex[:12]
        self.kind: str = kw.get("kind", "hf")
        self.repo: str = kw["repo"]
        self.revision: str = kw.get("revision", "main")
        self.files: list[dict] = kw.get("files", [])  # {path,size,done,segments:[[start,end,done],...]}
        self.dest: str = kw.get("dest", "")
        self.status: Status = kw.get("status", "queued")
        self.error: str | None = kw.get("error")
        self.created_at: float = kw.get("created_at", time.time())
        self.finished_at: float | None = kw.get("finished_at")
        self.ollama_total: int = kw.get("ollama_total", 0)
        self.ollama_done: int = kw.get("ollama_done", 0)
        self.ollama_status: str = kw.get("ollama_status", "")
        self._samples: deque[tuple[float, int]] = deque(maxlen=40)

    # ------------------------------------------------------------------ stats
    @property
    def total(self) -> int:
        return self.ollama_total if self.kind == "ollama" else sum(f["size"] for f in self.files)

    @property
    def downloaded(self) -> int:
        if self.kind == "ollama":
            return self.ollama_done
        return sum(f["size"] if f.get("complete") else sum(s[2] for s in f.get("segments", [])) for f in self.files)

    def sample(self) -> None:
        self._samples.append((time.monotonic(), self.downloaded))

    @property
    def speed(self) -> float:
        if self.status != "downloading" or len(self._samples) < 2:
            return 0.0
        (t0, b0), (t1, b1) = self._samples[0], self._samples[-1]
        return (b1 - b0) / (t1 - t0) if t1 > t0 else 0.0

    def to_dict(self, public: bool = True) -> dict:
        d = {
            "id": self.id,
            "kind": self.kind,
            "repo": self.repo,
            "revision": self.revision,
            "dest": self.dest,
            "status": self.status,
            "error": self.error,
            "created_at": self.created_at,
            "finished_at": self.finished_at,
            "ollama_total": self.ollama_total,
            "ollama_done": self.ollama_done,
            "ollama_status": self.ollama_status,
        }
        if public:
            sp = self.speed
            remaining = max(self.total - self.downloaded, 0)
            d.update(
                total=self.total,
                downloaded=self.downloaded,
                speed=sp,
                eta=remaining / sp if sp > 0 else None,
                files=[
                    {
                        "path": f["path"],
                        "size": f["size"],
                        "downloaded": f["size"] if f.get("complete") else sum(s[2] for s in f.get("segments", [])),
                        "complete": bool(f.get("complete")),
                        "connections": len(f.get("segments", [])),
                    }
                    for f in self.files
                ],
            )
        else:
            d["files"] = self.files
        return d


class DownloadManager:
    def __init__(self) -> None:
        self.tasks: dict[str, Task] = {}
        self._running: dict[str, asyncio.Task] = {}
        self._wake = asyncio.Event()
        for raw in read_json("downloads.json", []):
            t = Task(**raw)
            if t.status in ("downloading", "verifying"):
                t.status = "queued"  # resume after restart
            self.tasks[t.id] = t

    # --------------------------------------------------------------- public API
    def list(self) -> list[dict]:
        return [t.to_dict() for t in sorted(self.tasks.values(), key=lambda t: t.created_at, reverse=True)]

    def add_hf(self, repo: str, paths: list[dict], revision: str = "main") -> Task:
        dest = models_dir() / repo
        files = [{"path": p["path"], "size": int(p.get("size") or 0), "sha256": p.get("sha256"), "segments": []} for p in paths]
        t = Task(kind="hf", repo=repo, revision=revision, files=files, dest=str(dest))
        self.tasks[t.id] = t
        self._persist()
        self._wake.set()
        return t

    def add_ollama(self, model: str) -> Task:
        t = Task(kind="ollama", repo=model, dest="ollama")
        self.tasks[t.id] = t
        self._persist()
        self._wake.set()
        return t

    def pause(self, tid: str) -> None:
        t = self.tasks[tid]
        if t.status in ("queued", "downloading", "verifying"):
            t.status = "paused"
            self._abort(tid)
            self._persist()

    def resume(self, tid: str) -> None:
        t = self.tasks[tid]
        if t.status in ("paused", "failed", "cancelled"):
            t.status, t.error = "queued", None
            self._persist()
            self._wake.set()

    def cancel(self, tid: str) -> None:
        t = self.tasks[tid]
        t.status = "cancelled"
        self._abort(tid)
        if t.kind == "hf":
            for f in t.files:
                if not f.get("complete"):
                    Path(t.dest, f["path"] + ".part").unlink(missing_ok=True)
                    f["segments"] = []
        self._persist()

    def remove(self, tid: str) -> None:
        t = self.tasks.get(tid)
        if t and t.status in ("queued", "downloading", "verifying"):
            self.cancel(tid)
        self.tasks.pop(tid, None)
        self._persist()

    # ----------------------------------------------------------------- engine
    def _abort(self, tid: str) -> None:
        # Cancelling the asyncio task interrupts in-flight socket reads immediately;
        # segment offsets are only advanced after a successful write, so state stays consistent.
        job = self._running.pop(tid, None)
        if job:
            job.cancel()

    def _persist(self) -> None:
        write_json("downloads.json", [t.to_dict(public=False) for t in self.tasks.values()])

    async def run(self) -> None:
        last_save = 0.0
        while True:
            for tid, job in list(self._running.items()):
                if job.done():
                    self._running.pop(tid)
            limit = max(1, settings().max_concurrent_downloads)
            for t in sorted(self.tasks.values(), key=lambda t: t.created_at):
                if len(self._running) >= limit:
                    break
                if t.status == "queued" and t.id not in self._running:
                    t.status = "downloading"
                    self._running[t.id] = asyncio.create_task(self._execute(t))
            for t in self.tasks.values():
                if t.status in ("downloading", "verifying"):
                    t.sample()
            if time.monotonic() - last_save > 2:
                self._persist()
                last_save = time.monotonic()
            try:
                await asyncio.wait_for(self._wake.wait(), timeout=0.5)
            except asyncio.TimeoutError:
                pass
            self._wake.clear()

    async def _execute(self, t: Task) -> None:
        try:
            if t.kind == "ollama":
                await self._ollama_pull(t)
            else:
                Path(t.dest).mkdir(parents=True, exist_ok=True)
                for f in t.files:
                    if not f.get("complete"):
                        await self._fetch_file(t, f)
            t.status, t.finished_at = "completed", time.time()
        except asyncio.CancelledError:
            raise
        except Exception as e:
            t.status, t.error = "failed", f"{type(e).__name__}: {e}"
        finally:
            self._persist()
            self._wake.set()

    async def _fetch_file(self, t: Task, f: dict) -> None:
        url = hf.resolve_url(t.repo, f["path"], t.revision)
        final = Path(t.dest) / f["path"]
        part = final.with_name(final.name + ".part")
        final.parent.mkdir(parents=True, exist_ok=True)
        limits = httpx.Limits(max_connections=64)
        timeout = httpx.Timeout(30, read=120)
        async with httpx.AsyncClient(headers=hf.headers(), follow_redirects=True, timeout=timeout, limits=limits) as client:
            if not f["size"]:
                r = await client.head(url)
                f["size"] = int(r.headers.get("x-linked-size") or r.headers.get("content-length") or 0)
            size = f["size"]
            if not f["segments"] or not part.exists():
                n = max(1, min(settings().connections_per_file, size // MIN_SEGMENT or 1))
                step = -(-size // n)
                f["segments"] = [[i * step, min((i + 1) * step, size) - 1, 0] for i in range(n)]
                with open(part, "wb") as fh:
                    fh.truncate(size)
            workers = [asyncio.create_task(self._fetch_segment(client, url, part, seg)) for seg in f["segments"]]
            try:
                done, _ = await asyncio.wait(workers, return_when=asyncio.FIRST_EXCEPTION)
                for w in done:
                    w.result()  # re-raise the first failure
            finally:
                for w in workers:
                    w.cancel()
        if sum(s[2] for s in f["segments"]) < size:
            raise IOError("incomplete download")
        if f.get("sha256"):
            t.status = "verifying"
            digest = await asyncio.get_running_loop().run_in_executor(None, _sha256, part)
            t.status = "downloading"
            if digest != f["sha256"]:
                f["segments"] = []
                part.unlink(missing_ok=True)
                raise IOError(f"sha256 不一致 ({f['path']}) — 再開すると最初から取り直します")
        os.replace(part, final)
        f["complete"] = True

    async def _fetch_segment(self, client: httpx.AsyncClient, url: str, part: Path, seg: list[int]) -> None:
        start, end = seg[0], seg[1]
        attempts = 0
        while start + seg[2] <= end:
            pos = start + seg[2]
            try:
                async with client.stream("GET", url, headers={"Range": f"bytes={pos}-{end}"}) as r:
                    if r.status_code == 200 and pos != 0:
                        raise IOError("server ignored Range header")
                    r.raise_for_status()
                    with open(part, "r+b") as fh:
                        fh.seek(pos)
                        async for chunk in r.aiter_bytes(CHUNK):
                            fh.write(chunk)
                            seg[2] += len(chunk)
                            attempts = 0
            except (httpx.TransportError, httpx.HTTPStatusError) as e:
                if isinstance(e, httpx.HTTPStatusError) and e.response.status_code in (401, 403, 404):
                    raise IOError(f"HTTP {e.response.status_code} (gated repo なら HF token を設定)") from e
                attempts += 1
                if attempts > 8:
                    raise
                await asyncio.sleep(min(2**attempts, 30))

    async def _ollama_pull(self, t: Task) -> None:
        layers: dict[str, tuple[int, int]] = {}
        async with httpx.AsyncClient(timeout=httpx.Timeout(30, read=None)) as c:
            async with c.stream("POST", f"{settings().ollama_url}/api/pull", json={"model": t.repo, "stream": True}) as r:
                r.raise_for_status()
                async for line in r.aiter_lines():
                    if not line.strip():
                        continue
                    ev = json.loads(line)
                    if "error" in ev:
                        raise IOError(ev["error"])
                    t.ollama_status = ev.get("status", "")
                    if ev.get("digest") and ev.get("total"):
                        layers[ev["digest"]] = (ev["total"], ev.get("completed", 0))
                        t.ollama_total = sum(v[0] for v in layers.values())
                        t.ollama_done = sum(v[1] for v in layers.values())
        t.ollama_done = t.ollama_total


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        while chunk := fh.read(16 * 1024 * 1024):
            h.update(chunk)
    return h.hexdigest()


manager: DownloadManager | None = None


def get() -> DownloadManager:
    assert manager is not None
    return manager
