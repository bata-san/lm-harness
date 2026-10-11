"""Running inference servers + saved launch profiles."""

from __future__ import annotations

import asyncio
import socket
import uuid

import httpx
from pydantic import BaseModel, Field

from . import models, runtimes
from .config import read_json, settings, write_json
from .procs import Proc


class LaunchSpec(BaseModel):
    name: str = ""
    runtime: str = "llama.cpp"
    model: str = ""  # model id under models_dir (llama.cpp / vllm) or empty for ollama
    host: str | None = None
    port: int | None = None
    args: list[str] = Field(default_factory=list)
    env: dict[str, str] = Field(default_factory=dict)
    autostart: bool = False


class Profile(LaunchSpec):
    id: str = Field(default_factory=lambda: uuid.uuid4().hex[:8])


def _port_free(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) != 0


class InstanceManager:
    def __init__(self) -> None:
        self.procs: dict[str, Proc] = {}
        self.jobs: dict[str, Proc] = {}
        self.metrics: dict[str, dict] = {}
        self.profiles: dict[str, Profile] = {p["id"]: Profile(**p) for p in read_json("profiles.json", [])}

    # -------------------------------------------------------------- profiles
    def save_profile(self, p: Profile) -> Profile:
        self.profiles[p.id] = p
        write_json("profiles.json", [x.model_dump() for x in self.profiles.values()])
        return p

    def delete_profile(self, pid: str) -> None:
        self.profiles.pop(pid, None)
        write_json("profiles.json", [x.model_dump() for x in self.profiles.values()])

    # ------------------------------------------------------------- instances
    def _pick_port(self) -> int:
        used = {p.meta.get("port") for p in self.procs.values() if p.alive}
        port = settings().port_range_start
        while port in used or not _port_free(port):
            port += 1
        return port

    async def launch(self, spec: LaunchSpec) -> Proc:
        host = spec.host or settings().default_host
        port = spec.port or (11435 if spec.runtime == "ollama" else self._pick_port())
        if not _port_free(port):
            raise RuntimeError(f"port {port} は使用中です")
        model_path = str(models.resolve(spec.model)) if spec.model else ""
        if spec.runtime != "ollama" and not model_path:
            raise RuntimeError("モデルを選択してください")
        cmd, env = runtimes.build_command(spec.runtime, model_path, host, port, spec.args)
        env.update(spec.env)
        name = spec.name or (spec.model.rsplit("/", 1)[-1] if spec.model else spec.runtime)
        p = Proc(name, cmd, env, meta={"runtime": spec.runtime, "model": spec.model, "port": port, "host": host, "args": spec.args, "ready": False})
        self.procs[p.id] = p
        await p.start()
        return p

    async def stop(self, iid: str) -> None:
        await self.procs[iid].stop()

    def remove(self, iid: str) -> None:
        p = self.procs.get(iid)
        if p and not p.alive:
            self.procs.pop(iid)
            self.metrics.pop(iid, None)

    def list(self) -> list[dict]:
        return [{**p.to_dict(), "metrics": self.metrics.get(p.id, {})} for p in sorted(self.procs.values(), key=lambda p: p.started_at, reverse=True)]

    async def stop_all(self) -> None:
        await asyncio.gather(*(p.stop() for p in self.procs.values() if p.alive), *(j.stop() for j in self.jobs.values() if j.alive))

    # ------------------------------------------------------------------ jobs
    async def run_job(self, job_id: str) -> Proc:
        for j in self.jobs.values():
            if j.meta.get("job") == job_id and j.alive:
                return j
        cmd, env = runtimes.job_command(job_id)
        j = Proc(runtimes.JOBS[job_id]["title"], cmd, env, kind="job", meta={"job": job_id})
        self.jobs[j.id] = j
        await j.start()
        return j

    # ------------------------------------------------------------ monitoring
    async def monitor(self) -> None:
        async with httpx.AsyncClient(timeout=2) as c:
            while True:
                for p in list(self.procs.values()):
                    if not p.alive:
                        p.meta["ready"] = False
                        continue
                    base = f"http://127.0.0.1:{p.meta['port']}"
                    rt = p.meta["runtime"]
                    try:
                        r = await c.get(base + runtimes.health_path(rt))
                        ready = r.status_code == 200
                    except httpx.HTTPError:
                        ready = False
                    if ready and not p.meta["ready"]:
                        p.log(f"[harness] ready: {base}  (OpenAI 互換: {base}/v1)")
                    p.meta["ready"] = ready
                    p.status = "running" if ready else "starting"
                    if ready and rt in ("llama.cpp", "vllm"):
                        try:
                            r = await c.get(base + "/metrics")
                            if r.status_code == 200:
                                self.metrics[p.id] = runtimes.parse_metrics(r.text)
                        except httpx.HTTPError:
                            pass
                await asyncio.sleep(1.5)

    async def autostart(self) -> None:
        for prof in self.profiles.values():
            if prof.autostart:
                try:
                    await self.launch(prof)
                except Exception as e:  # noqa: BLE001
                    print(f"[harness] autostart {prof.name} failed: {e}")


manager: InstanceManager | None = None


def get() -> InstanceManager:
    assert manager is not None
    return manager
