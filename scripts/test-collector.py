#!/usr/bin/env python3
"""Isolated collector regressions; no display, service, or privileged access."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(sys.argv.pop(1)).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "collect_files.py"


class CollectorTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="jwm-collector-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        (self.source / "example.rs").write_text("fixture contents\n")
        self.output = self.root / "output.txt"

    def run_collector(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *map(str, args)],
            cwd=self.root, capture_output=True, text=True, timeout=5,
        )

    def test_regular_file_success(self):
        result = self.run_collector("-s", self.source, "-o", self.output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fixture contents", self.output.read_text())

    def test_fifo_is_rejected_without_waiting(self):
        os.mkfifo(self.source / "pipe")
        result = self.run_collector("-s", self.source, "-o", self.output)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture contents", self.output.read_text())

    def test_output_error_is_nonzero(self):
        result = self.run_collector("-s", self.source, "-o", self.root / "missing" / "output")
        self.assertNotEqual(result.returncode, 0)

    def test_missing_source_is_nonzero(self):
        result = self.run_collector("-s", self.root / "missing", "-o", self.output)
        self.assertNotEqual(result.returncode, 0)

    def test_partial_source_error_is_nonzero(self):
        result = self.run_collector("-s", self.source, self.root / "missing", "-o", self.output)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fixture contents", self.output.read_text())

    def test_regular_symlink_and_explicit_files(self):
        link = self.root / "link.rs"
        link.symlink_to(self.source / "example.rs")
        result = self.run_collector("-F", link, "-o", self.output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fixture contents", self.output.read_text())

    def test_directory_symlink_loop_keeps_collecting_readable_files(self):
        (self.source / "loop.rs").symlink_to("loop.rs")
        result = self.run_collector("-s", self.source, "-o", self.output)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.output.exists(), result.stderr)
        self.assertIn("fixture contents", self.output.read_text())
        self.assertNotIn("Traceback", result.stderr)

    def test_explicit_symlink_loop_keeps_collecting_readable_files(self):
        loop = self.root / "loop.rs"
        loop.symlink_to(loop.name)
        result = self.run_collector(
            "-F", loop, self.source / "example.rs", "-o", self.output,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.output.exists(), result.stderr)
        self.assertIn("fixture contents", self.output.read_text())
        self.assertNotIn("Traceback", result.stderr)

    def test_source_symlink_loop_keeps_other_sources(self):
        loop = self.root / "loop"
        loop.symlink_to(loop.name)
        result = self.run_collector("-s", loop, self.source, "-o", self.output)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.output.exists(), result.stderr)
        self.assertIn("fixture contents", self.output.read_text())
        self.assertNotIn("Traceback", result.stderr)

    def test_output_basename_does_not_exclude_unrelated_input(self):
        matching_name = self.source / self.output.name
        matching_name.write_text("same basename must be collected\n")
        for input_args in (("-s", self.source), ("-F", matching_name)):
            with self.subTest(input_args=input_args):
                result = self.run_collector(*input_args, "-o", self.output)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("same basename must be collected", self.output.read_text())

    def test_existing_output_and_its_symlink_alias_are_excluded(self):
        output = self.source / "output.txt"
        output.write_text("previous output must not be collected\n")
        (self.source / "alias.txt").symlink_to(output.name)
        nested = self.source / "nested"
        nested.mkdir()
        (nested / output.name).write_text("distinct nested input\n")
        result = self.run_collector("-s", self.source, "-o", output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fixture contents", output.read_text())
        self.assertIn("distinct nested input", output.read_text())
        self.assertNotIn("previous output must not be collected", output.read_text())

    def test_explicit_filename_exclusions_still_apply(self):
        result = self.run_collector(
            "-s", self.source, "-f", "example.rs", "-o", self.output,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("fixture contents", self.output.read_text())

    def test_nonregular_rejection_closes_descriptor(self):
        spec = importlib.util.spec_from_file_location("collector_under_test", SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        fifo = self.root / "pipe"
        os.mkfifo(fifo)
        before = len(list(Path("/proc/self/fd").iterdir()))
        for _ in range(32):
            with self.assertRaises(ValueError):
                module.read_regular_text(fifo)
        self.assertEqual(len(list(Path("/proc/self/fd").iterdir())), before)


if __name__ == "__main__":
    unittest.main()
