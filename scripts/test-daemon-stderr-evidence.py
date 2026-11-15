#!/usr/bin/env python3
"""Offline checks for daemon diagnostics; never starts a display or a daemon."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "daemon_fixture", Path(__file__).with_name("test-shared-sidebar-shell-daemon.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class StderrEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = self.root / "runtime"
        self.evidence = self.root / "evidence"
        self.runtime.mkdir()
        self.evidence.mkdir()

    def archive(self, phase="before-cleanup"):
        return fixture.archive_child_stderr(self.runtime, self.evidence, phase)

    def test_missing_generations_are_reported_without_inventing_logs(self):
        result = self.archive()
        self.assertEqual([entry["status"] for entry in result.values()], ["missing", "missing"])
        self.assertEqual(list(self.evidence.iterdir()), [])

    def test_both_generations_preserve_exact_bytes_and_fingerprints(self):
        for name, data in (("jwm_child_stderr.log", b"restart cancelled\n\xff\x00"),
                           ("jwm_child_stderr.log.1", b"previous generation\n")):
            (self.runtime / name).write_bytes(data)
        for name, record in self.archive().items():
            data = (self.runtime / name).read_bytes()
            self.assertEqual(record["status"], "saved")
            self.assertEqual((self.evidence / record["file"]).read_bytes(), data)
            self.assertEqual(record["bytes"], len(data))
            self.assertEqual(record["sha256"], hashlib.sha256(data).hexdigest())

    def test_cleanup_rotation_cannot_overwrite_pre_cleanup_evidence(self):
        live = self.runtime / "jwm_child_stderr.log"
        live.write_bytes(b"original failure")
        before = self.archive()
        live.rename(self.runtime / "jwm_child_stderr.log.1")
        live.write_bytes(b"cleanup diagnostics")
        after = self.archive("after-cleanup")
        self.assertEqual((self.evidence / before[live.name]["file"]).read_bytes(), b"original failure")
        self.assertEqual((self.evidence / after[live.name]["file"]).read_bytes(), b"cleanup diagnostics")
        self.assertEqual((self.evidence / after[live.name + ".1"]["file"]).read_bytes(), b"original failure")

    def test_io_errors_are_explicit_and_do_not_mask_original_failure(self):
        with patch.object(Path, "read_bytes", side_effect=PermissionError("read denied")):
            self.assertTrue(all(row["status"] == "error" and "read denied" in row["error"]
                                for row in self.archive().values()))
        (self.runtime / "jwm_child_stderr.log").write_bytes(b"failure")
        with patch.object(Path, "write_bytes", side_effect=OSError("disk full")):
            result = self.archive()
        self.assertEqual(result["jwm_child_stderr.log"]["status"], "error")
        self.assertIn("disk full", result["jwm_child_stderr.log"]["error"])


if __name__ == "__main__":
    unittest.main()
