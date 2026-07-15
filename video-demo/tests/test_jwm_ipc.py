import json
import os
import socket
import stat
import threading
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

from runner.jwm_ipc import (
    IpcError,
    JwmIpc,
    _socket_location,
    _validate_runtime_directory,
)


class JwmIpcTests(unittest.TestCase):
    def exchange(self, response: dict, call, *, use_default_path: bool = False):
        with TemporaryDirectory() as directory:
            path = Path(directory) / ("jwm-ipc.sock" if use_default_path else "ipc.sock")
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(path))
            listener.listen(1)
            received = []

            def serve():
                connection, _ = listener.accept()
                with connection:
                    received.append(json.loads(connection.makefile().readline()))
                    connection.sendall((json.dumps(response) + "\n").encode())

            thread = threading.Thread(target=serve)
            thread.start()
            try:
                if use_default_path:
                    with patch.dict(os.environ, {"XDG_RUNTIME_DIR": directory}):
                        result = call(JwmIpc())
                else:
                    result = call(JwmIpc(path))
            finally:
                thread.join(timeout=2)
                listener.close()
            return result, received[0]

    def test_command_uses_newline_json_protocol(self):
        result, request = self.exchange(
            {"success": True, "data": {"active": True}},
            lambda ipc: ipc.command("start_recording", {"path": "/tmp/demo.mp4"}),
        )
        self.assertEqual(result, {"active": True})
        self.assertEqual(request["command"], "start_recording")
        self.assertEqual(request["args"]["path"], "/tmp/demo.mp4")

    def test_failed_response_raises(self):
        with self.assertRaisesRegex(IpcError, "already active"):
            self.exchange(
                {"success": False, "error": "recording already active"},
                lambda ipc: ipc.command("start_recording", {"path": "/tmp/demo.mp4"}),
            )

    def test_default_path_connects_inside_private_xdg_runtime(self):
        result, request = self.exchange(
            {"success": True, "data": {"version": "test"}},
            lambda ipc: ipc.query("get_version"),
            use_default_path=True,
        )
        self.assertEqual(result, {"version": "test"})
        self.assertEqual(request["query"], "get_version")

    def test_socket_location_rejects_empty_or_relative_xdg_path(self):
        fallback = Path("/tmp/jwm-4242/jwm-ipc.sock")
        self.assertEqual(_socket_location(None, 4242), (fallback, True))
        self.assertEqual(_socket_location("", 4242), (fallback, True))
        self.assertEqual(_socket_location("relative/runtime", 4242), (fallback, True))
        self.assertEqual(
            _socket_location("/run/user/4242", 4242),
            (Path("/run/user/4242/jwm-ipc.sock"), False),
        )

    def test_fake_socket_in_unsafe_xdg_runtime_is_rejected(self):
        with TemporaryDirectory() as parent:
            runtime = Path(parent) / "runtime"
            runtime.mkdir()
            runtime.chmod(0o755)
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(runtime / "jwm-ipc.sock"))
            listener.listen(1)
            try:
                with patch.dict(os.environ, {"XDG_RUNTIME_DIR": str(runtime)}):
                    with self.assertRaisesRegex(IpcError, "group or other users"):
                        JwmIpc()
                self.assertEqual(stat.S_IMODE(runtime.stat().st_mode), 0o755)
            finally:
                listener.close()

    def test_fallback_directory_is_tightened_before_use(self):
        with TemporaryDirectory() as parent:
            runtime = Path(parent) / "runtime"
            runtime.mkdir()
            runtime.chmod(0o777)

            _validate_runtime_directory(runtime, fallback=True)

            self.assertEqual(stat.S_IMODE(runtime.stat().st_mode), 0o700)

    def test_fallback_runtime_symlink_is_rejected(self):
        with TemporaryDirectory() as parent:
            target = Path(parent) / "target"
            target.mkdir(mode=0o700)
            runtime = Path(parent) / "runtime"
            runtime.symlink_to(target, target_is_directory=True)

            with self.assertRaisesRegex(IpcError, "real directory"):
                _validate_runtime_directory(runtime, fallback=True)

    def test_non_socket_endpoint_is_rejected(self):
        with TemporaryDirectory() as directory:
            path = Path(directory) / "ipc.sock"
            path.write_text("not a socket")

            with self.assertRaisesRegex(IpcError, "not a real Unix socket"):
                JwmIpc(path).query("get_version")


if __name__ == "__main__":
    unittest.main()
