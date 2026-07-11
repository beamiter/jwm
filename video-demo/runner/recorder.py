from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path

from .jwm_ipc import JwmIpc


class Recorder:
    def __init__(self, ipc: JwmIpc) -> None:
        self.ipc = ipc

    def start(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self.ipc.command("start_recording", {"path": str(path.resolve())})

    def stop_and_wait(self, path: Path, timeout: float = 15.0) -> dict:
        self.ipc.command("stop_recording")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            status = self.ipc.query("get_recording_status") or {}
            if status.get("finalized"):
                return self.probe(path)
            time.sleep(0.1)
        raise TimeoutError(f"recording was not finalized: {path}")

    @staticmethod
    def probe(path: Path) -> dict:
        if not path.exists() or path.stat().st_size < 1024:
            raise RuntimeError(f"recording is absent or too small: {path}")
        result = subprocess.run([
            "ffprobe", "-v", "error", "-show_entries", "format=duration",
            "-show_entries", "stream=width,height,r_frame_rate,codec_name", "-of", "json", str(path),
        ], text=True, capture_output=True, check=False, timeout=10)
        if result.returncode:
            raise RuntimeError(f"ffprobe rejected {path}: {result.stderr.strip()}")
        data = json.loads(result.stdout)
        streams = data.get("streams", [])
        if not streams or float(data.get("format", {}).get("duration", 0)) <= 0:
            raise RuntimeError(f"recording has no decodable video duration: {path}")
        return data
