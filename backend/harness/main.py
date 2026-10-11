from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from typing import Any

import psutil
from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from . import downloads, hf, instances, models, runtimes, system, tuning
from .config import FRONTEND_DIST, store
from .instances import LaunchSpec, Profile


@asynccontextmanager
async def lifespan(app: FastAPI):
    psutil.cpu_percent(interval=None)  # prime the counters
    downloads.manager = downloads.DownloadManager()
    instances.manager = instances.InstanceManager()
    bg = [asyncio.create_task(downloads.manager.run()), asyncio.create_task(instances.manager.monitor())]
    await instances.manager.autostart()
    yield
    await instances.manager.stop_all()
    for t in bg:
        t.cancel()
    downloads.manager._persist()


app = FastAPI(title="LLM Harness", lifespan=lifespan)


def _404(fn):
    try:
        return fn()
    except KeyError:
        raise HTTPException(404, "not found")


# ------------------------------------------------------------------ system
@app.get("/api/system")
def get_system() -> dict:
    return system.snapshot()


@app.get("/api/system/checks")
def get_checks() -> list[dict]:
    return system.health_checks()


@app.get("/api/settings")
def get_settings() -> dict:
    s = store.get().model_dump()
    s["hf_token"] = "********" if s["hf_token"] else ""
    return s


@app.put("/api/settings")
def put_settings(patch: dict[str, Any]) -> dict:
    if patch.get("hf_token") == "********":
        patch.pop("hf_token")
    store.update(patch)
    return get_settings()


# ------------------------------------------------------------------ models
@app.get("/api/models")
def list_models() -> list[dict]:
    return models.scan()


@app.delete("/api/models/{model_id:path}")
def delete_model(model_id: str) -> dict:
    models.delete(model_id)
    return {"ok": True}


class TuneReq(BaseModel):
    model: str
    ctx: int | None = None
    kv_type: str = "q8_0"
    parallel: int = 1


@app.post("/api/tune")
def tune(req: TuneReq) -> dict:
    m = next((m for m in models.scan() if m["id"] == req.model), None)
    if not m:
        raise HTTPException(404, "model not found")
    if m["format"] != "gguf":
        return {"args": runtimes.default_args("vllm"), "notes": ["safetensors モデルは vLLM 既定値を提案"], "estimate": {}}
    return tuning.recommend(m, req.ctx, req.kv_type, req.parallel)


# ------------------------------------------------------------------ hub
@app.get("/api/hf/search")
async def hf_search(q: str, gguf: bool = True) -> list[dict]:
    try:
        return await hf.search(q, gguf)
    except Exception as e:
        raise HTTPException(502, str(e))


@app.get("/api/hf/files/{repo:path}")
async def hf_files(repo: str, revision: str = "main") -> list[dict]:
    try:
        return await hf.files(repo, revision)
    except Exception as e:
        raise HTTPException(502, str(e))


# ------------------------------------------------------------- downloads
class HfDownloadReq(BaseModel):
    repo: str
    files: list[dict]
    revision: str = "main"


class OllamaPullReq(BaseModel):
    model: str


@app.get("/api/downloads")
def list_downloads() -> list[dict]:
    return downloads.get().list()


@app.post("/api/downloads/hf")
def add_hf(req: HfDownloadReq) -> dict:
    if not req.files:
        raise HTTPException(400, "no files")
    return downloads.get().add_hf(req.repo, req.files, req.revision).to_dict()


@app.post("/api/downloads/ollama")
def add_ollama(req: OllamaPullReq) -> dict:
    return downloads.get().add_ollama(req.model).to_dict()


@app.post("/api/downloads/{tid}/{action}")
def download_action(tid: str, action: str) -> dict:
    m = downloads.get()
    fn = {"pause": m.pause, "resume": m.resume, "cancel": m.cancel}.get(action)
    if not fn:
        raise HTTPException(400, "bad action")
    _404(lambda: fn(tid))
    return {"ok": True}


@app.delete("/api/downloads/{tid}")
def delete_download(tid: str) -> dict:
    downloads.get().remove(tid)
    return {"ok": True}


# -------------------------------------------------------------- runtimes
@app.get("/api/runtimes")
async def list_runtimes() -> list[dict]:
    return await runtimes.status()


@app.get("/api/ollama/models")
async def ollama_models() -> dict:
    tags = await runtimes.ollama_api("/api/tags")
    ps = await runtimes.ollama_api("/api/ps")
    return {"available": tags is not None, "models": (tags or {}).get("models", []), "running": (ps or {}).get("models", [])}


@app.post("/api/ollama/{action}")
async def ollama_action(action: str, body: dict) -> dict:
    model = body.get("model", "")
    if action == "load":
        r = await runtimes.ollama_api("/api/generate", "POST", {"model": model, "keep_alive": "30m"}, timeout=600)
    elif action == "unload":
        r = await runtimes.ollama_api("/api/generate", "POST", {"model": model, "keep_alive": 0}, timeout=60)
    elif action == "delete":
        r = await runtimes.ollama_api("/api/delete", "DELETE", {"model": model}, timeout=60)
    else:
        raise HTTPException(400, "bad action")
    if r is None:
        raise HTTPException(502, "ollama request failed")
    return {"ok": True}


@app.get("/api/jobs")
def list_jobs() -> dict:
    m = instances.get()
    return {
        "available": [{"id": k, **v, "command": f"bash scripts/{v['script']}"} for k, v in runtimes.JOBS.items()],
        "runs": [j.to_dict() for j in sorted(m.jobs.values(), key=lambda j: j.started_at, reverse=True)],
    }


@app.post("/api/jobs/{job_id}")
async def run_job(job_id: str) -> dict:
    if job_id not in runtimes.JOBS:
        raise HTTPException(404, "unknown job")
    if runtimes.JOBS[job_id]["sudo"]:
        raise HTTPException(400, "sudo が必要なためターミナルで実行してください")
    return (await instances.get().run_job(job_id)).to_dict()


@app.get("/api/jobs/runs/{rid}/logs")
def job_logs(rid: str, since: int = 0) -> dict:
    return _404(lambda: instances.get().jobs[rid].tail(since))


@app.post("/api/jobs/runs/{rid}/stop")
async def stop_job(rid: str) -> dict:
    await _404(lambda: instances.get().jobs[rid]).stop()
    return {"ok": True}


# ------------------------------------------------------------- instances
@app.get("/api/instances")
def list_instances() -> list[dict]:
    return instances.get().list()


@app.post("/api/instances")
async def launch(spec: LaunchSpec) -> dict:
    try:
        return (await instances.get().launch(spec)).to_dict()
    except (RuntimeError, ValueError) as e:
        raise HTTPException(400, str(e))


@app.post("/api/instances/{iid}/stop")
async def stop_instance(iid: str) -> dict:
    await _404(lambda: instances.get().stop(iid))
    return {"ok": True}


@app.delete("/api/instances/{iid}")
def remove_instance(iid: str) -> dict:
    instances.get().remove(iid)
    return {"ok": True}


@app.get("/api/instances/{iid}/logs")
def instance_logs(iid: str, since: int = 0) -> dict:
    return _404(lambda: instances.get().procs[iid].tail(since))


@app.get("/api/profiles")
def list_profiles() -> list[dict]:
    return [p.model_dump() for p in instances.get().profiles.values()]


@app.post("/api/profiles")
def save_profile(p: Profile) -> dict:
    return instances.get().save_profile(p).model_dump()


@app.delete("/api/profiles/{pid}")
def delete_profile(pid: str) -> dict:
    instances.get().delete_profile(pid)
    return {"ok": True}


# ---------------------------------------------------------------- frontend
if FRONTEND_DIST.exists():
    app.mount("/assets", StaticFiles(directory=FRONTEND_DIST / "assets"), name="assets")

    @app.get("/{full_path:path}")
    def spa(full_path: str):
        f = FRONTEND_DIST / full_path
        if full_path and f.is_file() and FRONTEND_DIST in f.resolve().parents:
            return FileResponse(f)
        return FileResponse(FRONTEND_DIST / "index.html")
