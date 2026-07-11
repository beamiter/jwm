from __future__ import annotations

import json
import os
import socket
from pathlib import Path
from typing import Any


class IpcError(RuntimeError):
    pass


class JwmIpc:
    def __init__(self, path: Path | None = None, timeout: float = 5.0) -> None:
        runtime = Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/jwm-{os.getuid()}"))
        self.path = path or runtime / "jwm-ipc.sock"
        self.timeout = timeout

    def _send(self, payload: dict[str, Any]) -> Any:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(self.timeout)
            stream.connect(str(self.path))
            stream.sendall((json.dumps(payload, separators=(",", ":")) + "\n").encode())
            data = bytearray()
            while b"\n" not in data:
                chunk = stream.recv(65536)
                if not chunk:
                    raise IpcError("JWM closed the IPC connection")
                data.extend(chunk)
        response = json.loads(data.split(b"\n", 1)[0])
        if not response.get("success"):
            raise IpcError(response.get("error", "JWM IPC request failed"))
        return response.get("data")

    def command(self, name: str, args: dict[str, Any] | None = None) -> Any:
        return self._send({"command": name, "args": args or {}})

    def query(self, name: str, args: dict[str, Any] | None = None) -> Any:
        return self._send({"query": name, "args": args or {}})
