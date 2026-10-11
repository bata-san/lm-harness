"""Supervised child processes with captured logs (used by instances and jobs)."""

from __future__ import annotations

import asyncio
import os
import signal
import time
import uuid
from collections import deque


class Proc:
    def __init__(self, name: str, cmd: list[str], env: dict[str, str] | None = None, cwd: str | None = None, kind: str = "instance", meta: dict | None = None) -> None:
        self.id = uuid.uuid4().hex[:10]
        self.name = name
        self.cmd = cmd
        self.env = env or {}
        self.cwd = cwd
        self.kind = kind
        self.meta = meta or {}
        self.status = "starting"
        self.pid: int | None = None
        self.returncode: int | None = None
        self.started_at = time.time()
        self.ended_at: float | None = None
        self.logs: deque[tuple[int, str]] = deque(maxlen=4000)
        self._seq = 0
        self._proc: asyncio.subprocess.Process | None = None
        self._stopping = False

    def log(self, line: str) -> None:
        self._seq += 1
        self.logs.append((self._seq, line))

    async def start(self) -> None:
        env = {**os.environ, **self.env}
        self.log("$ " + " ".join(self.cmd))
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *self.cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
                env=env,
                cwd=self.cwd,
                start_new_session=True,  # own process group so stop() kills children too
            )
        except (FileNotFoundError, PermissionError) as e:
            self.status, self.ended_at = "failed", time.time()
            self.log(f"[harness] 起動失敗: {e}")
            return
        self.pid = self._proc.pid
        asyncio.create_task(self._pump())

    async def _pump(self) -> None:
        assert self._proc and self._proc.stdout
        buf = b""
        while True:
            chunk = await self._proc.stdout.read(65536)
            if not chunk:
                break
            buf += chunk
            # llama.cpp / pip progress bars use \r; treat it as a line break
            *lines, buf = buf.replace(b"\r", b"\n").split(b"\n")
            for ln in lines:
                if ln.strip():
                    self.log(ln.decode("utf-8", errors="replace"))
        if buf.strip():
            self.log(buf.decode("utf-8", errors="replace"))
        self.returncode = await self._proc.wait()
        self.ended_at = time.time()
        if self._stopping:
            self.status = "stopped"
        else:
            self.status = "exited" if self.returncode == 0 else "failed"
        self.log(f"[harness] プロセス終了 code={self.returncode}")

    @property
    def alive(self) -> bool:
        return self._proc is not None and self._proc.returncode is None

    async def stop(self, timeout: float = 15) -> None:
        if not self.alive or not self.pid:
            return
        self._stopping = True
        try:
            os.killpg(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        for _ in range(int(timeout * 10)):
            if not self.alive:
                return
            await asyncio.sleep(0.1)
        try:
            os.killpg(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    def tail(self, since: int = 0, limit: int = 1000) -> dict:
        lines = [(s, l) for s, l in self.logs if s > since][-limit:]
        return {"lines": [{"seq": s, "text": l} for s, l in lines], "last": self._seq}

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "cmd": self.cmd,
            "status": self.status,
            "pid": self.pid,
            "returncode": self.returncode,
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            **self.meta,
        }
