#!/usr/bin/env python3
"""Offline displayfd diagnostics contracts using real fake child processes."""
import importlib.util
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "shared_shell_x11", Path(__file__).with_name("test-shared-sidebar-shell-x11.py")
)
harness = importlib.util.module_from_spec(spec)
spec.loader.exec_module(harness)

FAKE = r"""
import os
import sys
mode, display, started = sys.argv[1:]
print("fake server stdout", flush=True)
print("fake server stderr", file=sys.stderr, flush=True)
if mode in ("ready", "partial", "invalid"):
    os.write(int(display), {"ready": b"71\n", "partial": b"7", "invalid": b"err\n"}[mode])
os.write(int(started), b"1")
os.close(int(started))
if mode == "exit":
    sys.exit(23)
sys.stdin.buffer.read()
"""


class DisplayfdEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.evidence = Path(self.folder.name)
        self.observed = {}

    def spawn_for(self, mode):
        def spawn(*argv, pass_fds, stdout, stderr):
            self.assertEqual(argv[:2], ("Xvfb", "-displayfd"))
            display = argv[2]
            self.assertEqual(pass_fds, (display,))
            self.assertEqual(argv[3:], ("-screen", "0", "1280x800x24", "-nolisten", "tcp", "-noreset"))
            startup_read, startup_write = os.pipe()
            try:
                child = subprocess.Popen(
                    [sys.executable, "-c", FAKE, mode, str(display), str(startup_write)],
                    pass_fds=(*pass_fds, startup_write), stdout=stdout, stderr=stderr,
                    stdin=subprocess.PIPE,
                )
                self.addCleanup(self.reap, child)
                os.close(startup_write)
                startup_write = None
                # Synchronization independent of displayfd. The test deadline
                # is not spending time starting Python on a busy CI machine.
                self.assertTrue(select.select([startup_read], [], [], 5)[0])
                self.assertEqual(os.read(startup_read, 1), b"1")
                if mode == "exit":
                    self.assertEqual(child.wait(timeout=5), 23)
                return child
            finally:
                os.close(startup_read)
                if startup_write is not None:
                    os.close(startup_write)
        return spawn

    @staticmethod
    def reap(child):
        # No signals needed, even in a cloud sandbox: the owned process waits
        # on our anonymous stdin pipe and exits when we close it.
        child.stdin.close()
        child.wait(timeout=5)

    def start(self, mode):
        return harness.start_private_xvfb(self.spawn_for(mode), 1280, 800,
                                          self.evidence, self.observed, ready_timeout=0.15)

    def assert_observed(self, phase, byte_count, returncode):
        data = json.loads((self.evidence / "xvfb-startup.json").read_text())
        self.assertEqual(data, self.observed)
        self.assertEqual(data["phase"], phase)
        self.assertEqual(data["displayfd_bytes"], byte_count)
        self.assertEqual(data["child_returncode_at_boundary"], returncode)
        self.assertGreaterEqual(data["elapsed_ms"], data["displayfd_wait_ms"])
        self.assertEqual((self.evidence / data["stdout"]).read_text(), "fake server stdout\n")
        self.assertEqual((self.evidence / data["stderr"]).read_text(), "fake server stderr\n")
        return data

    def test_ready_keeps_logs_and_uses_the_complete_display(self):
        self.assertEqual(self.start("ready"), ":71")
        self.assertEqual(self.assert_observed("ready", 3, None)["display"], ":71")

    def test_early_exited_child_does_not_get_mislabeled_as_timeout(self):
        with self.assertRaisesRegex(RuntimeError, "pipe closed"):
            self.start("exit")
        self.assert_observed("displayfd-eof", 0, 23)

    def test_hung_child_preserves_partial_bytes_and_live_exit_snapshot(self):
        with self.assertRaisesRegex(TimeoutError, "before deadline"):
            self.start("partial")
        data = self.assert_observed("displayfd-timeout", 1, None)
        self.assertEqual(data["readiness_timeout_seconds"], 0.15)
        self.assertGreaterEqual(data["displayfd_wait_ms"], 140)
        self.assertNotIn("display", data)

    def test_invalid_payload_does_not_become_a_display(self):
        with self.assertRaisesRegex(RuntimeError, "invalid display number"):
            self.start("invalid")
        data = self.assert_observed("displayfd-invalid", 4, None)
        self.assertNotIn("display", data)


if __name__ == "__main__":
    unittest.main()
