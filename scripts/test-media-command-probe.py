#!/usr/bin/env python3
"""Offline regression checks for media command completion acceptance."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from fixtures.media_command_probe import invocations, new_invocation, successful_result

PROBE = Path(__file__).with_name('fixtures') / 'media_command_probe.py'


class CommandProbeTests(unittest.TestCase):
    def run_probe(self, directory, code, *args):
        return subprocess.Popen([sys.executable, str(PROBE), str(directory),
                                 str(directory / 'stderr.log'), 'both',
                                 sys.executable, '-c', code, *args])

    def test_success_preserves_args_and_records_actual_child(self):
        with tempfile.TemporaryDirectory(prefix="media ' space ") as temporary:
            directory = Path(temporary)
            args = ['', 'space value', "'quote;$(false)"]
            child = self.run_probe(directory, 'import os,sys,json; print(json.dumps([os.getpid(),sys.argv[1:]]))', *args)
            self.assertEqual(child.wait(timeout=5), 0)
            path = new_invocation(directory, set())
            record = successful_result(path)
            pid, received = json.loads((directory / 'stderr.log').read_text())
            self.assertEqual(record['observer_pid'], child.pid)
            self.assertEqual(record['child_pid'], pid)
            self.assertNotEqual(pid, child.pid)
            self.assertEqual(received, args)

    def test_nonzero_cannot_pass_as_closed_window(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            child = self.run_probe(directory, 'import sys; sys.exit(7)')
            self.assertEqual(child.wait(timeout=5), 7)
            with self.assertRaisesRegex(RuntimeError, 'media command failed'):
                successful_result(new_invocation(directory, set()))

    def test_late_previous_completion_cannot_satisfy_new_action(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            gate = directory / 'release'
            child = self.run_probe(directory,
                'import pathlib,sys,time; p=pathlib.Path(sys.argv[1]);\nwhile not p.exists(): time.sleep(.01)', str(gate))
            try:
                deadline = time.monotonic() + 5
                while not invocations(directory) and time.monotonic() < deadline:
                    time.sleep(.01)
                before = invocations(directory)
                self.assertEqual(len(before), 1)
                self.assertIsNone(successful_result(next(iter(before))))
                gate.touch()
                self.assertEqual(child.wait(timeout=5), 0)
                self.assertIsNone(new_invocation(directory, before))
                second = self.run_probe(directory, 'pass')
                self.assertEqual(second.wait(timeout=5), 0)
                record = successful_result(new_invocation(directory, before))
                self.assertEqual(record['observer_pid'], second.pid)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait()


if __name__ == '__main__':
    unittest.main()
