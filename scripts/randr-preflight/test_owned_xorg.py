#!/usr/bin/env python3
"""Failure-contract tests only; never start Xorg or create a socket."""
import json
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


if __name__ == '__main__':
    unittest.main()
