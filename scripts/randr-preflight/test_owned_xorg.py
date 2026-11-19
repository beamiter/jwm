#!/usr/bin/env python3
"""Failure-contract tests only; never start Xorg or create a socket."""
import json
import os
import struct
import time
from pathlib import Path
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import owned_xorg as m


class FailureContract(unittest.TestCase):
    def case(self, setup=None, start=None, body=None, close=None, cleanup=None):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            server = MagicMock()
            server.phase = "mock-phase"
            server.operation = "mock-operation"
            server.resource_path = "/mock-resource"
            server.start.side_effect = start
            server.close.side_effect = close
            private = MagicMock(name='private_directory')
            private.name = '/never-opened-by-mocked-server'
            private.cleanup.side_effect = cleanup
            with patch.object(m, 'inputs', side_effect=setup, return_value=(Path('/fake'),Path('/fake'))), \
                 patch.object(m, 'OwnedXorg', return_value=server), \
                 patch.object(m.tempfile, 'TemporaryDirectory', return_value=private), \
                 patch.object(m.preflight, 'run', side_effect=body, return_value={'status':'passed'}):
                rc = m.execute(out)
            report = json.loads((out/'preflight.json').read_text())
            return rc, report

    def test_success_only_after_cleanup(self):
        self.assertEqual(self.case()[0], 0)

    def test_setup_missing_binary_recorded(self):
        rc, r = self.case(setup=FileNotFoundError('Xorg'))
        self.assertEqual((rc, r['status'], r['errors'][0]['stage']), (1,'failed','setup'))

    def test_start_failure_recorded(self):
        rc, r = self.case(start=RuntimeError('displayfd'))
        self.assertEqual((rc,r['status']), (1,'failed'))

    def test_cleanup_cannot_leave_pass(self):
        rc, r = self.case(close=RuntimeError('kill'))
        self.assertEqual((rc,r['status'],r['errors'][0]['stage']), (1,'failed','server-cleanup'))

    def test_interrupt_cannot_leave_pass(self):
        rc, r = self.case(body=KeyboardInterrupt())
        self.assertEqual((rc,r['status']), (1,'failed'))

    def test_directory_cleanup_cannot_leave_pass(self):
        rc, r = self.case(cleanup=RuntimeError('unlink'))
        self.assertEqual((rc,r['status']), (1,'failed'))

    def test_multiple_errors_retained(self):
        rc, r = self.case(body=RuntimeError('test'),close=RuntimeError('kill'),cleanup=RuntimeError('unlink'))
        self.assertEqual((rc,r['status'],len(r['errors'])), (1,'failed',3))

    def test_filename_and_traceback_retained(self):
        rc, r = self.case(setup=FileNotFoundError(2, 'missing', '/explicit/Xorg'))
        self.assertEqual(r['errors'][0]['filename'], '/explicit/Xorg')
        self.assertIn('FileNotFoundError', r['errors'][0]['traceback'])
        self.assertEqual(rc, 1)


class OwnershipContract(unittest.TestCase):
    def probe(self, peer_pid=1234, peer_uid=None, connect_error=None, exe=None):
        server = m.OwnedXorg.__new__(m.OwnedXorg)
        server.proc = MagicMock(pid=1234)
        server.proc.poll.return_value = None
        server.display = '177'
        server.deadline = time.monotonic() + 10
        server.executable = Path('/expected/Xorg')
        peer = MagicMock()
        peer.getsockopt.return_value = struct.pack('3i', peer_pid,
            os.getuid() if peer_uid is None else peer_uid, os.getgid())
        peer.connect.side_effect = connect_error
        with patch.object(m.socket, 'socket') as factory, \
             patch.object(Path, 'resolve', return_value=exe or server.executable), \
             patch.object(Path, 'read_text', side_effect=AssertionError('must not read displayfd lock')):
            factory.return_value.__enter__.return_value = peer
            server.verify_owner()
        self.assertEqual(peer.connect.call_args.args, ('/tmp/.X11-unix/X177',))

    def test_displayfd_contract_does_not_require_lock(self):
        self.probe()

    def test_other_pid_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'not owned child'):
            self.probe(peer_pid=9999)

    def test_other_uid_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'not owned child'):
            self.probe(peer_uid=os.getuid()+1)

    def test_other_executable_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'executable identity changed'):
            self.probe(exe=Path('/other/Xorg'))

    def test_missing_socket_is_not_skipped(self):
        with self.assertRaises(FileNotFoundError):
            self.probe(connect_error=FileNotFoundError(2, 'missing', '/tmp/.X11-unix/X177'))


if __name__ == '__main__':
    unittest.main()
