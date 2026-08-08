import json
import socket
import tempfile
import threading
import unittest
from pathlib import Path

from runner.demo_windows import DemoWindows


class DemoWindowsTests(unittest.TestCase):
    def test_control_round_trip_uses_the_client_unix_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            path = base / "demo.sock"
            received = []
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(path))
            listener.listen()

            def serve():
                with listener:
                    for _ in range(2):
                        connection, _ = listener.accept()
                        with connection:
                            line = connection.makefile("r", encoding="utf-8").readline()
                            received.append(json.loads(line))
                            connection.sendall(b'{"success":true}\n')

            thread = threading.Thread(target=serve)
            thread.start()
            windows = DemoWindows(Path("unused"), None, base)
            windows.control_sockets[42] = path

            self.assertTrue(windows.control(42, "minimize")["success"])
            self.assertEqual(windows.last_minimized_window_id, 42)
            self.assertTrue(windows.control(42, "restore")["success"])
            self.assertIsNone(windows.last_minimized_window_id)

            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(
                received,
                [{"command": "minimize"}, {"command": "restore"}],
            )

    def test_control_rejects_commands_outside_the_scene_contract(self):
        windows = DemoWindows(Path("unused"), None, Path("/tmp"))
        with self.assertRaisesRegex(ValueError, "unsupported demo window control"):
            windows.control(42, "close")


if __name__ == "__main__":
    unittest.main()
