from __future__ import annotations

import json
import os
import select
import socket
import subprocess
import time
from pathlib import Path

from .jwm_ipc import JwmIpc


THEMES = ("blue", "green", "purple", "orange", "red", "gray")


def _read_ready_line(stream, timeout: float = 5.0, max_bytes: int = 65536) -> str:
    deadline = time.monotonic() + timeout
    data = bytearray()
    while b"\n" not in data:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([stream], [], [], remaining)[0]:
            raise TimeoutError("demo client did not publish readiness before the deadline")
        chunk = os.read(stream.fileno(), min(4096, max_bytes + 1 - len(data)))
        if not chunk:
            raise RuntimeError("demo client closed stdout before publishing readiness")
        data.extend(chunk)
        if len(data.split(b"\n", 1)[0]) > max_bytes:
            raise RuntimeError("demo client readiness line exceeds the byte limit")
    return data.split(b"\n", 1)[0].decode("utf-8").strip()


def _stop_process(process) -> None:
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=2)
    finally:
        if process.stdout:
            process.stdout.close()
        if process.stderr:
            process.stderr.close()


class DemoWindows:
    def __init__(self, binary: Path, ipc: JwmIpc, tmp: Path) -> None:
        self.binary, self.ipc, self.tmp = binary, ipc, tmp
        self.processes: list[subprocess.Popen[str]] = []
        self.control_sockets: dict[int, Path] = {}
        self.last_minimized_window_id: int | None = None

    @property
    def pids(self) -> list[int]:
        return [process.pid for process in self.processes if process.poll() is None]

    def spawn(self, count: int, content: str = "grid", opacity: float = 1.0) -> None:
        offset = len(self.processes)
        names = ["MASTER" if offset == 0 and index == 0 else f"STACK {offset + index}" for index in range(count)]
        for local_index, title in enumerate(names):
            index = offset + local_index
            socket = self.tmp / f"demo-{index}.sock"
            process = subprocess.Popen([
                str(self.binary), "--title", title, "--instance", f"demo-{index}",
                "--theme", THEMES[index % len(THEMES)], "--content", content,
                "--opacity", str(opacity), "--animate", "--socket", str(socket),
            ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            # Own the child before any fallible handshake/parsing operation.
            self.processes.append(process)
            try:
                if process.stdout is None:
                    raise RuntimeError("demo client stdout pipe is unavailable")
                metadata = json.loads(_read_ready_line(process.stdout))
                window_id = int(metadata["window_id"])
                reported_socket = Path(metadata["socket"])
                if reported_socket != socket:
                    raise RuntimeError(
                        f"demo client reported unexpected control socket: {reported_socket}"
                    )
                self.control_sockets[window_id] = socket
            except BaseException:
                try:
                    _stop_process(process)
                except Exception:
                    # Keep ownership so outer scene cleanup can retry.
                    pass
                else:
                    self.processes.remove(process)
                raise

    def control(self, window_id: int, command: str, timeout: float = 3.0) -> dict:
        if command not in ("minimize", "restore"):
            raise ValueError(f"unsupported demo window control: {command}")
        path = self.control_sockets.get(window_id)
        if path is None:
            raise RuntimeError(f"demo window {window_id} has no control socket")

        payload = (json.dumps({"command": command}, separators=(",", ":")) + "\n").encode()
        deadline = time.monotonic() + timeout
        last_error: OSError | None = None
        while time.monotonic() < deadline:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                    stream.settimeout(max(0.1, deadline - time.monotonic()))
                    stream.connect(str(path))
                    stream.sendall(payload)
                    response = stream.makefile("r", encoding="utf-8").readline()
                result = json.loads(response)
                if not result.get("success"):
                    raise RuntimeError(
                        f"demo window {window_id} rejected {command}: {result}"
                    )
                if command == "minimize":
                    self.last_minimized_window_id = window_id
                elif self.last_minimized_window_id == window_id:
                    self.last_minimized_window_id = None
                return result
            except (FileNotFoundError, ConnectionRefusedError) as exc:
                last_error = exc
                time.sleep(0.02)
        raise TimeoutError(
            f"demo window {window_id} control socket did not accept {command}: {last_error}"
        )

    def wait_managed(self, count: int, timeout: float = 10.0) -> list[dict]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            windows = [item for item in (self.ipc.query("get_windows") or []) if str(item.get("class", "")).lower() == "jwmdemo"]
            if len(windows) >= count:
                return windows
            time.sleep(0.05)
        raise TimeoutError(f"only {len(windows)} of {count} demo windows became managed")

    def close(self) -> None:
        first_error: Exception | None = None
        for process in list(self.processes):
            try:
                _stop_process(process)
            except Exception as exc:
                if first_error is None:
                    first_error = exc
            else:
                self.processes.remove(process)
        self.control_sockets.clear()
        self.last_minimized_window_id = None
        if first_error is not None:
            raise first_error
