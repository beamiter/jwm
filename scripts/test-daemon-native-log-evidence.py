#!/usr/bin/env python3
"""Offline, process-local provenance tests; no desktop or daemon is started."""
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from fixtures.daemon_native_log import NativeLogEvidence


class NativeLogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.logs = self.root / 'logs'
        self.logs.mkdir(mode=0o700)
        self.evidence = self.root / 'evidence'
        self.evidence.mkdir()
        self.pid = os.getpid()
        self.started = int(time.time())
        self.identity = {'pid': self.pid, 'start_time': 123, 'parent': 1}
        self.collector = self.make_collector()
        self.addCleanup(self.collector.close)

    def make_collector(self, **overrides):
        args = dict(pid=self.pid, expected_identity=self.identity,
                    executable=Path('/proc/self/exe').resolve(), identity=lambda pid: self.identity,
                    started_at=self.started, directories={self.logs})
        args.update(overrides)
        return NativeLogEvidence(**args)

    def source(self, name=None, content=b'restart cancelled\n'):
        path = self.logs / (name or f'jwm_jwm_bar_global_{self.started}_p{self.pid}_rCURRENT.log')
        path.write_bytes(content)
        stream = path.open('ab')
        self.addCleanup(stream.close)
        return path, stream

    def test_exact_open_inode_survives_rotation_and_cleanup(self):
        path, writer = self.source()
        self.collector.discover()
        self.assertEqual(len(self.collector.pinned), 1)
        self.assertFalse(os.get_inheritable(next(iter(self.collector.pinned.values()))[1]))
        before = self.collector.archive(self.evidence, 'before')
        path.rename(path.with_suffix('.rotated'))
        path.write_bytes(b'not the pinned inode')
        writer.write(b'cleanup tail\n')
        writer.flush()
        self.collector.identity = lambda pid: None
        after = self.collector.archive(self.evidence, 'after')
        self.assertEqual((self.evidence / before['files'][0]['file']).read_bytes(), b'restart cancelled\n')
        self.assertEqual((self.evidence / after['files'][0]['file']).read_bytes(), b'restart cancelled\ncleanup tail\n')
        fd = next(iter(self.collector.pinned.values()))[1]
        self.collector.close()
        with self.assertRaises(OSError):
            os.fstat(fd)

    def test_unopened_wrong_pid_and_stale_files_are_not_collected(self):
        (self.logs / f'jwm_jwm_bar_global_{self.started}_p{self.pid}_rCURRENT.log').write_bytes(b'unopened')
        self.source(f'jwm_jwm_bar_global_{self.started}_p{self.pid + 1}_rCURRENT.log')
        self.source(f'jwm_jwm_bar_global_{self.started - 10}_p{self.pid}_rCURRENT.log')
        self.source(f'other_{self.started}_p{self.pid}_rCURRENT.log')
        self.collector.discover()
        self.assertEqual(self.collector.pinned, {})

    def test_replaced_symlink_cannot_select_unrelated_bytes(self):
        path, _ = self.source()
        path.rename(self.root / 'original')
        other = self.root / 'private-other'
        other.write_bytes(b'not ours')
        path.symlink_to(other)
        self.collector.discover()
        self.assertEqual(self.collector.pinned, {})

    def test_unsafe_directory_and_hardlinked_file_are_rejected(self):
        path, _ = self.source()
        self.logs.chmod(0o755)
        self.collector.discover()
        self.assertEqual(self.collector.pinned, {})
        self.logs.chmod(0o700)
        os.link(path, self.root / 'link')
        self.collector.discover()
        self.assertEqual(self.collector.pinned, {})

    def test_directory_symlink_is_not_followed(self):
        path, _ = self.source()
        alias = self.root / 'alias'
        alias.symlink_to(self.logs, target_is_directory=True)
        self.collector.directories = {alias}
        original = os.readlink

        def readlink(candidate, *args, **kwargs):
            actual = original(candidate, *args, **kwargs)
            if actual == str(path):
                return str(alias / path.name)
            return actual

        with patch('os.readlink', side_effect=readlink):
            self.collector.discover()
        self.assertEqual(self.collector.pinned, {})
        self.assertTrue(self.collector.errors)

    def test_wrong_identity_or_executable_is_rejected(self):
        self.source()
        for changes in ({'identity': lambda pid: None}, {'executable': self.root / 'wrong-exe'}):
            collector = self.make_collector(**changes)
            self.addCleanup(collector.close)
            collector.discover()
            self.assertEqual(collector.pinned, {})
            self.assertTrue(collector.errors)

    def test_identity_change_during_discovery_rejects_file(self):
        self.source()
        calls = iter([self.identity, None])
        self.collector.identity = lambda pid: next(calls, None)
        self.collector.discover()
        self.assertEqual(self.collector.pinned, {})
        self.assertTrue(self.collector.errors)

    def test_absent_output_directory_reports_error_without_raising(self):
        self.source()
        self.collector.discover()
        result = self.collector.archive(self.root / 'absent', 'after')
        self.assertEqual(result['files'][0]['status'], 'error')


if __name__ == '__main__':
    unittest.main()
