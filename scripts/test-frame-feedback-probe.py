#!/usr/bin/env python3
import unittest
from unittest.mock import patch
from fixtures import frame_feedback_probe as probe


class ProbeTests(unittest.TestCase):
    def test_filter_is_scoped_and_preserves_filters_and_regex(self):
        env = {'RUST_LOG': 'warn,other=trace/keep'}
        probe.enable(env, 'tauri_solid_bar')
        self.assertEqual(env['RUST_LOG'], 'warn,other=trace,' + probe.MODULE + '=debug/keep')
        other = {'RUST_LOG': 'warn'}
        probe.enable(other, 'xilem_bar')
        self.assertEqual(other, {'RUST_LOG': 'warn'})

    def test_counter_property_preserves_both_ids(self):
        self.assertEqual(probe.counter_ids('_NET_WM_SYNC_REQUEST_COUNTER(CARDINAL) = 123, 456\n'), [123, 456])
        self.assertEqual(probe.counter_ids('_NET_WM_SYNC_REQUEST_COUNTER: not found.\n'), [])

    def test_collect_runs_only_exact_windows_in_bounded_fixture_runner(self):
        calls = []
        def run(*args):
            calls.append(args)
            return '{"windows": {}}'
        self.assertEqual(probe.collect(run, ['14680068']), {'windows': {}})
        self.assertEqual(calls[0][2:], ('14680068',))

    def test_log_archive_failure_is_recorded_and_closes_pins(self):
        class Logs:
            closed = False
            def discover(self): raise OSError('test failure')
            def close(self): self.closed = True
        logs, result = Logs(), {}
        probe.archive(logs, None, result)
        self.assertTrue(logs.closed)
        self.assertEqual(result['frame_feedback_native_log_error'], 'test failure')

    def test_query_counter_values_with_only_read_apis(self):
        class Function:
            def __init__(self, action): self.action = action
            def __call__(self, *args): return self.action(*args)
        class X11:
            XOpenDisplay = Function(lambda _: 123)
            XCloseDisplay = Function(lambda _: 0)
        values = {11: (0, 8), 22: (1, 2)}
        def query(display, counter, out):
            self.assertEqual(display, 123)
            out._obj.hi, out._obj.lo = values[counter]
            return 1
        def initialize(display, major, minor):
            major._obj.value, minor._obj.value = 3, 1
            return 1
        class XExt:
            XSyncInitialize = Function(initialize)
            XSyncQueryCounter = Function(query)
        def xprop(args, **kwargs):
            return '_NET_SUPPORTED(ATOM) = _NET_WM_FRAME_DRAWN' if '-root' in args else 'WM_PROTOCOLS(ATOM) = _NET_WM_SYNC_REQUEST\n_NET_WM_SYNC_REQUEST_COUNTER(CARDINAL) = 11, 22\n'
        with patch.object(probe.ctypes, 'CDLL', side_effect=[X11(), XExt()]), patch.object(probe.subprocess, 'check_output', side_effect=xprop), patch.dict(probe.os.environ, DISPLAY=':99'):
            result = probe.snapshot([14680068])
        self.assertEqual(result['sync_version'], [3, 1])
        self.assertEqual(result['windows']['14680068']['counters'], [{'id': 11, 'value': 8}, {'id': 22, 'value': 4294967298}])



    def test_initialize_failure_closes_display_without_query(self):
        from unittest.mock import Mock
        x11, xext = Mock(), Mock()
        x11.XOpenDisplay.return_value = 123
        xext.XSyncInitialize.return_value = 0
        with patch.object(probe.ctypes, 'CDLL', side_effect=[x11, xext]), patch.object(probe.subprocess, 'check_output', return_value='root'), patch.dict(probe.os.environ, DISPLAY=':99'):
            with self.assertRaisesRegex(RuntimeError, 'does not support XSync'):
                probe.snapshot([14680068])
        xext.XSyncQueryCounter.assert_not_called()
        x11.XCloseDisplay.assert_called_once_with(123)

    def test_identity_rejects_zombie(self):
        with patch.object(probe.Path, 'read_text', return_value='10 (fixture) Z ' + ' '.join(['0'] * 19)):
            self.assertIsNone(probe.identity(10))
        with patch.object(probe.Path, 'read_text', return_value='10 (fixture) S ' + ' '.join(['0'] * 18) + ' 123'):
            self.assertEqual(probe.identity(10), (10, '123'))


if __name__ == '__main__': unittest.main()
