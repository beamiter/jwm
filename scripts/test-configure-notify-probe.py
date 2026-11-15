#!/usr/bin/env python3
"""Offline contracts: the diagnostic cannot turn a failed sample green."""
import ast
import ctypes
from types import SimpleNamespace
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from fixtures import configure_notify_probe as probe


class ProbeTests(unittest.TestCase):
    def test_event_fields(self):
        event = probe.make_event(123, 456, [5, 5, 1270, 42, 0]).configure
        self.assertEqual([event.type, event.send_event, event.event, event.window,
                          event.x, event.y, event.width, event.height, event.border_width,
                          event.above, event.override_redirect], [22,1,456,456,5,5,1270,42,0,0,0])
        self.assertEqual(ctypes.sizeof(probe.XEvent), 24 * ctypes.sizeof(ctypes.c_long))

    def sample(self, name):
        painted = name == 'after'
        return {'geometry': [5,5,1270,42,0],  'phases': ('_JWM_FRONTEND_READY(CARDINAL) = 1\n'
            '_JWM_FRONTEND_DOM_READY(UTF8_STRING) = "DOMContentLoaded: visible"\n' +
            ('_JWM_FRONTEND_RAF_FIRST(UTF8_STRING) = "first frame callback"\n'
             '_JWM_FRONTEND_RAF_SECOND(UTF8_STRING) = "second frame callback"\n'
             '_JWM_FRONTEND_RENDERED(CARDINAL) = 1' if painted else
             '_JWM_FRONTEND_RAF_FIRST:  not found.')),
            'counters': {'windows': {'42': {'counters': [{'id':1,'value':0},
                {'id':2,'value':4 if painted else 1}]}}}}

    def test_one_send_two_equal_windows_original_failure(self):
        sent, waits = [], []
        with tempfile.TemporaryDirectory() as directory:
            result = probe.experiment({'xid':42}, Path(directory),
                take=lambda t,d,n:self.sample(n), send=lambda *a:sent.append(a),
                sleep=waits.append, diff=lambda a,b:{'top_changed_pixels':5001})
        self.assertEqual(len(sent),1)
        self.assertEqual(waits,[3,3])
        self.assertEqual(result['original_acceptance'],'failed')
        self.assertTrue(result['rescue_observed'])

    def test_spontaneous_sham_recovery_never_sends(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(probe, 'send_once') as unused:
                sent = []
                result = probe.experiment({'xid':42}, Path(directory),
                    take=lambda t,d,n:self.sample('before' if n == 'before' else 'after'), send=lambda *a:sent.append(a),
                    sleep=lambda _:None, diff=lambda a,b:{'top_changed_pixels':0})
        self.assertEqual(sent, [])
        self.assertEqual(result['not_sent_reason'], 'first-frame stall changed during sham')
        self.assertFalse(result['rescue_observed'])

    def test_probe_exception_preserves_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            result = probe.experiment({},Path(directory),take=lambda *a:(_ for _ in ()).throw(ValueError('identity')))
        self.assertEqual(result['error'],'identity')
        self.assertEqual(result['sent_count'],0)
        self.assertEqual(result['original_acceptance'],'failed')
        self.assertFalse(result['rescue_observed'])

    def test_identity_change_stops_before_x(self):
        with patch.object(probe,'process',return_value={'identity':(10,'new'),'exe':'/bin/test'}), patch.object(probe,'command') as command:
            with self.assertRaisesRegex(ValueError,'identity'):
                probe.validate({'pid':10,'identity':[10,'old'],'exe':'/bin/test','xid':42})
            command.assert_not_called()

    def test_geometry_race_stops_before_send(self):
        with patch.object(probe,'validate',return_value=[1,2,3,4,5]), patch.object(probe.C,'CDLL') as library:
            with self.assertRaisesRegex(ValueError,'geometry'):
                probe.send_once({},[5,5,1270,42,0])
            library.assert_not_called()

    def test_only_original_first_frame_stall_is_eligible(self):
        sample = self.sample('before')
        self.assertTrue(probe.stalled(sample['phases'], sample['counters'], 42))
        for raw in (sample['phases'].replace('DOM_READY', 'OTHER'),
                    sample['phases'] + '\n_JWM_FRONTEND_RAF_FIRST(UTF8_STRING) = "frame callback"'):
            self.assertFalse(probe.stalled(raw, sample['counters'], 42))
        sample['counters']['windows']['42']['counters'][1]['value'] = 2
        self.assertFalse(probe.stalled(sample['phases'], sample['counters'], 42))
        self.assertFalse(probe.phase('_JWM_FRONTEND_RENDERED(UTF8_STRING) = "1"', 'RENDERED'))
        self.assertFalse(probe.phase('_JWM_FRONTEND_RAF_FIRST(UTF8_STRING) = ""', 'RAF_FIRST'))

    def test_process_probe_read_failure_is_diagnostic_only(self):
        source = Path(__file__).with_name('test-shared-sidebar-shell-x11.py')
        tree = ast.parse(source.read_text())
        guarded = next(node for node in ast.walk(tree) if isinstance(node, ast.If)
                       and ast.unparse(node.test) == 'args.configure_notify_probe'
                       and 'configure_notify_probe.process' in ast.unparse(node))
        scope = dict(args=SimpleNamespace(configure_notify_probe=True), primary_pid=10,
                     probe_identity=None, result={}, configure_notify_probe=probe)
        with patch.object(probe, 'process', side_effect=OSError('vanished')):
            exec(compile(ast.fix_missing_locations(ast.Module(body=[guarded], type_ignores=[])), str(source), 'exec'), scope)
        self.assertIsNone(scope['probe_identity'])
        self.assertEqual(scope['result']['configure_notify_probe_unavailable'], 'vanished')

    def test_integration_default_off_and_reraises(self):
        source=Path(__file__).with_name('test-shared-sidebar-shell-x11.py').read_text()
        self.assertIn('"--configure-notify-probe", action="store_true"',source)
        self.assertIn('if args.configure_notify_probe and str(error).startswith',source)
        self.assertIn('result.update(status="failed", reason=str(error))',source)
        self.assertIn('result["failure_diagnostics"] = diagnostics\n            raise',source)


if __name__=='__main__':
    unittest.main()
