from __future__ import annotations

import json
import os
import socket
import stat
import struct
from pathlib import Path
from typing import Any


class IpcError(RuntimeError):
    pass


def _socket_location(xdg_runtime_dir: str | None, uid: int) -> tuple[Path, bool]:
    if xdg_runtime_dir:
        runtime = Path(xdg_runtime_dir)
        if runtime.is_absolute():
            return runtime / "jwm-ipc.sock", False
    return Path(f"/tmp/jwm-{uid}") / "jwm-ipc.sock", True


def _validate_runtime_directory(path: Path, *, fallback: bool) -> None:
    if fallback:
        try:
            path.mkdir(mode=0o700)
        except FileExistsError:
            pass
        except OSError as exc:
            raise IpcError(f"cannot create JWM IPC fallback directory {path}: {exc}") from exc

    try:
        metadata = path.lstat()
    except OSError as exc:
        raise IpcError(f"cannot inspect JWM IPC runtime directory {path}: {exc}") from exc
    if not stat.S_ISDIR(metadata.st_mode):
        raise IpcError(f"JWM IPC runtime path must be a real directory, not a symlink: {path}")
    if metadata.st_uid != os.geteuid():
        raise IpcError(f"JWM IPC runtime directory is not owned by the current user: {path}")

    if stat.S_IMODE(metadata.st_mode) & 0o077:
        if not fallback:
            raise IpcError(
                f"JWM IPC runtime directory is accessible by group or other users: {path}"
            )
        try:
            path.chmod(0o700)
            metadata = path.lstat()
        except OSError as exc:
            raise IpcError(f"failed to secure JWM IPC fallback directory {path}: {exc}") from exc
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != os.geteuid()
            or stat.S_IMODE(metadata.st_mode) & 0o077
        ):
            raise IpcError(f"failed to secure JWM IPC fallback directory: {path}")


def _validate_socket_endpoint(path: Path) -> None:
    try:
        metadata = path.lstat()
    except OSError as exc:
        raise IpcError(f"cannot inspect JWM IPC socket {path}: {exc}") from exc
    if not stat.S_ISSOCK(metadata.st_mode):
        raise IpcError(f"JWM IPC endpoint is not a real Unix socket: {path}")
    if metadata.st_uid != os.geteuid():
        raise IpcError(f"JWM IPC socket is not owned by the current user: {path}")


def _validate_peer(stream: socket.socket, path: Path) -> None:
    if not hasattr(socket, "SO_PEERCRED"):
        return
    credentials = stream.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i"))
    _pid, uid, _gid = struct.unpack("3i", credentials)
    if uid != os.geteuid():
        raise IpcError(f"JWM IPC peer for {path} is not owned by the current user")


class JwmIpc:
    def __init__(self, path: Path | None = None, timeout: float = 5.0) -> None:
        self._runtime: tuple[Path, bool] | None = None
        if path is None:
            self.path, fallback = _socket_location(
                os.environ.get("XDG_RUNTIME_DIR"), os.geteuid()
            )
            runtime = self.path.parent
            _validate_runtime_directory(runtime, fallback=fallback)
            self._runtime = (runtime, fallback)
        else:
            # An explicit endpoint remains supported for tests and isolated
            # runners. It is caller-selected, but its type and ownership are
            # still checked immediately before connecting.
            self.path = Path(path)
        self.timeout = timeout

    def _send(self, payload: dict[str, Any]) -> Any:
        if self._runtime is not None:
            runtime, fallback = self._runtime
            _validate_runtime_directory(runtime, fallback=fallback)
        _validate_socket_endpoint(self.path)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(self.timeout)
            stream.connect(str(self.path))
            _validate_peer(stream, self.path)
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
