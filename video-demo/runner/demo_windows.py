from __future__ import annotations

import json
import select
import subprocess
import time
from pathlib import Path

from .jwm_ipc import JwmIpc


THEMES = ("blue", "green", "purple", "orange", "red", "gray")


class DemoWindows:
    def __init__(self, binary: Path, ipc: JwmIpc, tmp: Path) -> None:
        self.binary, self.ipc, self.tmp = binary, ipc, tmp
        self.processes: list[subprocess.Popen[str]] = []

    @property
    def pids(self) -> list[int]:
        return [process.pid for process in self.processes if process.poll() is None]

    def spawn(self, count: int, content: str = "grid") -> None:
        names = ["MASTER"] + [f"STACK {index}" for index in range(1, count)]
        for index, title in enumerate(names):
            socket = self.tmp / f"demo-{index}.sock"
            process = subprocess.Popen([
                str(self.binary), "--title", title, "--instance", f"demo-{index}",
                "--theme", THEMES[index % len(THEMES)], "--content", content,
                "--animate", "--socket", str(socket),
            ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            ready, _, _ = select.select([process.stdout], [], [], 5.0) if process.stdout else ([], [], [])
            line = process.stdout.readline().strip() if ready else ""
            if not line:
                raise RuntimeError(f"demo client failed to start: {process.stderr.read() if process.stderr else ''}")
            json.loads(line)
            self.processes.append(process)

    def wait_managed(self, count: int, timeout: float = 10.0) -> list[dict]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            windows = [item for item in (self.ipc.query("get_windows") or []) if str(item.get("class", "")).lower() == "jwmdemo"]
            if len(windows) >= count:
                return windows
            time.sleep(0.05)
        raise TimeoutError(f"only {len(windows)} of {count} demo windows became managed")

    def close(self) -> None:
        for process in self.processes:
            if process.poll() is None: process.terminate()
        for process in self.processes:
            try: process.wait(timeout=2)
            except subprocess.TimeoutExpired: process.kill()
        self.processes.clear()
