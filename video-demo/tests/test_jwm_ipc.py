import json
import socket
import threading
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from runner.jwm_ipc import IpcError, JwmIpc


class JwmIpcTests(unittest.TestCase):
    def exchange(self, response: dict, call):
        with TemporaryDirectory() as directory:
            path = Path(directory) / "ipc.sock"
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


if __name__ == "__main__":
    unittest.main()
