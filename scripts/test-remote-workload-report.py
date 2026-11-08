#!/usr/bin/env python3
"""Offline fixtures for workload reports. No screen/network/hardware measurement."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = load('remote-workload-report')
t = load('remote-terminal-workload')


def window(count=10, mean=4):
    stages = {s: {'mean': mean, 'quantiles': {'samples': min(count, 128),
              'p50_ms': mean, 'p95_ms': mean, 'p99_ms': mean}} for s in m.STAGES}
    return dict(type='jwm_remote_host_stats', schema_version=1, version='0.2.0',
                commit='abcdef0', window_seconds=5, mode='PRIVATE_HOST',
                source_dimensions=[1920, 1080], encoded_dimensions=[1920, 1080],
                quantile_policy=m.POLICY, stages=stages,
                **{field: count for field in m.COUNTERS})


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / 'stats'

    def read(self, values):
        self.path.write_text('\n'.join(json.dumps(v) for v in values), encoding='utf-8')
        return m.read_windows(self.path)

    def test_real_schema_shape_partial_window_weighting(self):
        a, b = window(1000, 4), window(1, 100)
        b['window_seconds'] = .25
        windows, ignored = self.read([a, b])
        report = m.summarize(windows, ignored, 'terminal', 'run1')
        self.assertAlmostEqual(report['stages']['capture_ms']['whole_run_weighted_mean_ms'], 4100 / 1001)
        self.assertIsNone(report['stages']['jpeg_ms']['whole_run_weighted_mean_ms'])
        self.assertIsNone(report['stages']['capture_ms']['whole_run_percentiles'])
        self.assertEqual(report['stages']['capture_ms']['max_reported_window_recent_p95_ms'], 100)
        self.assertEqual(report['reported_seconds'], 5.25)
        self.assertEqual(report['totals']['sent'], 1001)
        self.assertIsNone(report['physical_input_to_display'])
        self.assertNotIn('PRIVATE_HOST', json.dumps(report))

    def test_malformed_nested_types_rejected(self):
        for field, bad in [('stages', []), ('version', []), ('commit', {}), ('schema_version', True)]:
            value = window()
            value[field] = bad
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.read([value])
        for bad in [[], None, 1]:
            value = window()
            value['stages']['capture_ms'] = bad
            with self.assertRaises(ValueError):
                self.read([value])
            value = window()
            value['stages']['capture_ms']['quantiles'] = bad
            with self.assertRaises(ValueError):
                self.read([value])

    def test_stderr_diagnostics_not_echoed(self):
        self.path.write_text('SECRET address\n{"type":"other","secret":"PRIVATE"}\n' + json.dumps(window()))
        windows, ignored = m.read_windows(self.path)
        report = m.summarize(windows, ignored, 'browser', 'a')
        self.assertEqual(ignored, 2)
        self.assertNotIn('SECRET', json.dumps(report))
        self.assertNotIn('PRIVATE', json.dumps(report))

    def test_empty_stage_means_not_misreported(self):
        value = window(0)
        for stage in value['stages'].values():
            stage.update(mean=0, quantiles=dict(samples=0, p50_ms=None, p95_ms=None, p99_ms=None))
        report = m.summarize(*self.read([value]), 'terminal', 'idle')
        self.assertEqual(report['sent_frames_per_reported_second'], 0)
        self.assertIsNone(report['stages']['capture_ms']['whole_run_weighted_mean_ms'])
        self.assertIsNone(report['stages']['capture_ms']['window_means_range_ms'])

    def test_reject_bad_schema_and_bad_sample_values(self):
        mutations = [
            lambda x: x.update(schema_version=2),
            lambda x: x.update(quantile_policy='old capture complete'),
            lambda x: x.update(window_seconds=0),
            lambda x: x.update(sent=True),
            lambda x: x.update(bytes=-1),
            lambda x: x.update(source_dimensions=[0]),
            lambda x: x['stages']['capture_ms'].update(mean=float('nan')),
            lambda x: x['stages']['capture_ms'].update(mean=float('inf')),
            lambda x: x['stages']['capture_ms']['quantiles'].update(samples=129),
            lambda x: x['stages']['capture_ms']['quantiles'].update(samples=0),
            lambda x: x['stages']['capture_ms']['quantiles'].update(p95_ms=1),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                value = window()
                mutate(value)
                with self.assertRaises(ValueError):
                    self.read([value])

    def test_reject_duplicate_keys_malformed_or_missing_host_records(self):
        for content in ('{"type":"jwm_remote_host_stats","type":"other"}', '{bad', 'just diagnostic\n'):
            self.path.write_text(content)
            with self.assertRaises(ValueError):
                m.read_windows(self.path)

    def test_line_and_file_bounds(self):
        self.path.write_bytes(b'x' * (m.MAX_LINE + 1))
        with self.assertRaises(ValueError):
            list(m.bounded_lines(self.path))
        old = m.MAX_BYTES
        try:
            m.MAX_BYTES = 5
            self.path.write_text('abc\ndef\n')
            with self.assertRaises(ValueError):
                list(m.bounded_lines(self.path))
        finally:
            m.MAX_BYTES = old

    def test_builds_cannot_be_mixed(self):
        a, b = window(), window()
        b['commit'] = '1234567'
        with self.assertRaises(ValueError):
            m.summarize([a, b], 0, 'browser', 'mix')

    def test_dimension_changes_remain_visible(self):
        a, b = window(), window()
        b['encoded_dimensions'] = [960, 540]
        r = m.summarize([a, b], 0, 'browser', 'resize')
        self.assertEqual(r['dimensions_seen']['encoded_dimensions'], [(960, 540), (1920, 1080)])

    def csv(self, data):
        self.path.write_text(data)
        return m.physical_samples(self.path)

    def test_physical_three_trials_nearest_rank_not_ack(self):
        data = 'trial,sample,latency_ms\n' + ''.join(f'{trial},{i},{i}\n' for trial in ('a', 'b', 'c') for i in range(1, 101))
        r = self.csv(data)
        self.assertTrue(r['minimum_sampling_complete'])
        self.assertFalse(r['all_trial_p95_within_target'])
        self.assertFalse(r['measurement_method_verified'])
        self.assertEqual(r['trials']['a']['p95_ms'], 95)

    def test_physical_small_sample_never_complete(self):
        r = self.csv('trial,sample,latency_ms\na,1,2\n')
        self.assertTrue(r['all_trial_p95_within_target'])
        self.assertFalse(r['minimum_sampling_complete'])

    def test_physical_invalid_samples_and_duplicate_ids(self):
        for body in ('a,1,nan\n', 'a,1,-1\n', 'a,1,inf\n', 'a,1,4\na,1,5\n',
                     'a,0,4\n', 'a,1,4,5\n', 'a,1\n'):
            with self.subTest(body=body), self.assertRaises(ValueError):
                self.csv('trial,sample,latency_ms\n' + body)
        with self.assertRaises(ValueError):
            self.csv('time,ms\n1,2\n')

    def test_cli_does_not_emit_partial_report_on_failure(self):
        self.path.write_text('bad')
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            result = m.main(['--telemetry', str(self.path), '--workload', 'terminal', '--run-id', 'a'])
        self.assertEqual(result, 2)
        self.assertEqual(out.getvalue(), '')
        self.assertIn('rejected', err.getvalue())


class WorkloadTests(unittest.TestCase):
    def test_terminal_deterministic_frame(self):
        self.assertEqual(t.frame(3, 2, 20), t.frame(3, 2, 20))
        self.assertIn('000003', t.frame(3, 2, 20))
        self.assertIn('000004', t.frame(3, 2, 20))
        self.assertNotEqual(t.frame(3, 2, 20), t.frame(4, 2, 20))

    def test_bounded_terminal_and_restore(self):
        output = io.StringIO()
        now = [0.0]
        def sleep(seconds):
            now[0] += seconds
        t.run(1, 10, 2, 8, output, lambda: now[0], sleep)
        self.assertEqual(output.getvalue().count('\x1b[H'), 10)
        self.assertTrue(output.getvalue().endswith('\x1b[0m\x1b[?25h\x1b[?1049l'))

    def test_interrupt_restores_terminal(self):
        output = io.StringIO()
        def interrupt(_):
            raise KeyboardInterrupt
        with self.assertRaises(KeyboardInterrupt):
            t.run(1, 10, 2, 8, output, lambda: 0, interrupt)
        self.assertTrue(output.getvalue().endswith('\x1b[0m\x1b[?25h\x1b[?1049l'))


if __name__ == '__main__':
    unittest.main()
