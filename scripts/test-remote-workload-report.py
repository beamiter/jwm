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


def x11_capture(observations=0, mean=0):
    def stage(count, ms):
        percentile = ms if count else None
        return {'observations': count, 'mean': ms,
                'quantiles': {'samples': min(count, 128), 'p50_ms': percentile,
                              'p95_ms': percentile, 'p99_ms': percentile}}
    capture = {name: stage(0, 0) for name in m.X11_CAPTURE_STAGES}
    capture['core_readback_ms'] = stage(observations, mean)
    return {'damage_notifications': 0, 'forced_refreshes': 0, 'stages': capture}


def viewer_window(session_id, count, mean):
    empty = dict(mean=None, observations=0,
                 quantiles=dict(samples=0, p50_ms=None, p95_ms=None, p99_ms=None))
    sample = dict(mean=mean, observations=count,
                  quantiles=dict(samples=min(128, count), p50_ms=mean, p95_ms=mean, p99_ms=mean))
    stages = dict.fromkeys(m.VIEWER_STAGES, empty)
    stages['decode_ms'] = sample
    return dict(
        type='jwm_remote_viewer_stats', schema_version=1, version='0.2.0',
        commit='fedcba0', session_id=session_id, window_seconds=5,
        viewer_backend='x11', negotiated_codec='H.264', quantile_policy=m.POLICY,
        last_frame=dict(codec='JPEG', ignored='PRIVATE_FRAME_DATA'),
        sender_max_sampled_at_dequeue=dict.fromkeys(m.VIEWER_QUEUE_FIELDS, 3),
        stages=stages, **dict.fromkeys(m.VIEWER_COUNTERS, count),
    )


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / 'stats'

    def read(self, values):
        self.path.write_text('\n'.join(json.dumps(v) for v in values), encoding='utf-8')
        return m.read_windows(self.path)

    def test_paired_viewer_windows_preserve_counts_and_unmeasured_stages(self):
        session_id = 'a' * 32
        host = window()
        host['session_id'] = session_id
        viewers = [viewer_window(session_id, 1000, 4), viewer_window(session_id, 1, 100)]
        windows, _ = self.read([host, *viewers])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), windows)
        self.assertEqual(report['session_matching']['matched'], 1)
        self.assertEqual(report['last_frame_codecs_seen'], ['JPEG'])
        self.assertEqual(report['negotiated_codecs'], ['H.264'])
        self.assertAlmostEqual(report['stages']['decode_ms']['whole_run_weighted_mean_ms'], 4100 / 1001)
        self.assertEqual(report['stages']['decode_ms']['max_reported_window_recent_p95_ms'], 100)
        self.assertIsNone(report['stages']['hardware_download_ms']['whole_run_weighted_mean_ms'])
        self.assertNotIn('PRIVATE_FRAME_DATA', json.dumps(report))
        host['session_id'] = 'b' * 32
        with self.assertRaisesRegex(ValueError, 'different sessions'):
            m.summarize_viewer(viewers, [host])

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
        self.assertNotIn('counter_windows', report)
        self.assertNotIn('x11_capture', report)
        self.assertNotIn('PRIVATE_HOST', json.dumps(report))

    def test_frame_read_timings_use_only_instrumented_windows(self):
        session = 'e' * 32
        old, new = viewer_window(session, 100, 5), viewer_window(session, 4, 2)
        new['stages']['frame_body_receive_ms'] = new['stages']['decode_ms']
        self.read([window(), old, new])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
        stages = report['stages']
        self.assertEqual(stages['frame_body_receive_ms']['observations'], 4)
        self.assertEqual(stages['frame_body_receive_ms']['whole_run_weighted_mean_ms'], 2)
        self.assertNotIn('frame_header_wait_ms', stages)
        self.assertNotIn('frame_authentication_ms', stages)

    def test_hardware_rgb_work_aggregates_only_reported_materializations(self):
        session = 'f' * 32
        old = viewer_window(session, 10, 4)
        full, banded = viewer_window(session, 0, None), viewer_window(session, 5, 2)
        full['hardware_rgb_work'] = {
            'pool': {'fitting_checkouts': 1, 'fresh_reserves': 1, 'growth_reserves': 0,
                     'requested_bytes': 96, 'added_capacity_bytes': 48},
            'download': {'empty_targets_populated': 1, 'retained_targets_reused': 1,
                         'populated_targets_replaced': 0},
            'copy': {'full_frame_copies': 2, 'full_frame_bytes': 96,
                     'banded_copies': 0, 'banded_bytes': 0, 'private': 'IGNORE'},
        }
        banded['hardware_rgb_work'] = {
            'pool': {'fitting_checkouts': 0, 'fresh_reserves': 0, 'growth_reserves': 1,
                     'requested_bytes': 72, 'added_capacity_bytes': 24},
            'download': {'empty_targets_populated': 0, 'retained_targets_reused': 0,
                         'populated_targets_replaced': 1},
            'copy': {'full_frame_copies': 0, 'full_frame_bytes': 0,
                     'banded_copies': 1, 'banded_bytes': 72},
        }
        self.read([window(), old])
        self.assertNotIn('hardware_rgb_work', m.summarize_viewer(m.read_viewer_windows(self.path), []))
        self.read([window(), old, full, banded])
        work = m.summarize_viewer(m.read_viewer_windows(self.path), [])['hardware_rgb_work']
        self.assertEqual(work['windows'], 2)
        self.assertEqual(work['totals']['pool']['requested_bytes'], 168)
        self.assertEqual(work['totals']['pool']['added_capacity_bytes'], 72)
        self.assertEqual(work['totals']['copy'], {'full_frame_copies': 2, 'full_frame_bytes': 96,
                                                 'banded_copies': 1, 'banded_bytes': 72})
        self.assertEqual(set(work['totals']['download'].values()), {1})

        banded['hardware_rgb_work']['download']['populated_targets_replaced'] = 0
        self.read([window(), banded])
        with self.assertRaisesRegex(ValueError, 'inconsistent hardware RGB work counters'):
            m.read_viewer_windows(self.path)

        banded['hardware_rgb_work']['vaapi_map'] = {
            'mapped_frames': 1, 'staged_bytes': 36, 'fallback_frames': 0,
        }
        banded['stages'][m.DIRECT_MAP_STAGE] = viewer_window(session, 1, 1)['stages']['decode_ms']
        self.read([window(), old, banded])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
        self.assertEqual(report['stages'][m.DIRECT_MAP_STAGE]['observations'], 1)
        self.assertEqual(report['hardware_rgb_work']['vaapi_map'], {
            'windows': 1,
            'totals': {'mapped_frames': 1, 'staged_bytes': 36, 'fallback_frames': 0},
        })

        banded['stages'][m.DIRECT_MAP_STAGE] = old['stages']['decode_ms']
        self.read([window(), banded])
        with self.assertRaisesRegex(ValueError, 'inconsistent hardware direct mapping observations'):
            m.read_viewer_windows(self.path)

    def test_x11_preparation_counts_across_reporting_windows(self):
        session = 'b' * 32
        old, scheduled, submitted = (viewer_window(session, 1, 2) for _ in range(3))
        for record in (scheduled, submitted):
            record['x11_cpu_preparation'] = dict.fromkeys(m.X11_PREPARATION_COUNTS, 0)
            record['x11_cpu_preparation']['last_submitted_viewport_revision'] = None
        scheduled['x11_cpu_preparation']['scheduled'] = 1
        submitted['x11_cpu_preparation'].update(
            prepared=1, submitted=1, last_submitted_viewport_revision=4)
        submitted['stages']['x11_cpu_prepare_ms'] = submitted['stages']['decode_ms']
        self.read([window(), old, scheduled, submitted])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
        self.assertEqual(report['x11_cpu_preparation']['windows'], 2)
        self.assertEqual(report['x11_cpu_preparation']['totals']['submitted'], 1)
        self.assertEqual(report['x11_cpu_preparation']['last_submitted_viewport_revisions'], [4])
        self.assertEqual(report['stages']['x11_cpu_prepare_ms']['observations'], 1)

        submitted['x11_cpu_preparation']['prepared'] = 2
        self.read([window(), submitted])
        with self.assertRaisesRegex(ValueError, 'preparation counts exceed work observations'):
            m.read_viewer_windows(self.path)

    def test_native_presentation_reports_only_instrumented_windows(self):
        session = 'c' * 32
        old = viewer_window(session, 10, 2)
        for backend, path in (('x11', 'x11-vaapi-dmabuf-dri3'), ('wayland', 'wayland-vaapi-dmabuf')):
            with self.subTest(backend=backend):
                native = viewer_window(session, 5, 3)
                native.update(viewer_backend=backend, native_presented=4, last_presentation_path=path)
                native['stages'][m.NATIVE_VIEWER_STAGE] = native['stages']['decode_ms']
                self.read([window(), old, native])
                report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
                self.assertEqual(report['native_presentation'], {
                    'windows': 1,
                    'presented_frames': 4,
                    'last_paths_seen': [path],
                })
                self.assertEqual(report['stages'][m.NATIVE_VIEWER_STAGE]['observations'], 5)

    def test_native_wayland_stages_use_only_reported_observations(self):
        session = '7' * 32
        old, busy, brief = (viewer_window(session, count, mean)
                            for count, mean in [(100, 50), (1000, 4), (1, 100)])
        for item in (old, busy, brief):
            item['viewer_backend'] = 'wayland'
        self.read([window(), old])
        self.assertNotIn('native_wayland_stages', m.summarize_viewer(m.read_viewer_windows(self.path), []))

        busy_fence = copy.deepcopy(busy['stages']['decode_ms'])
        busy_fence['quantiles'].update(p95_ms=120, p99_ms=125)
        brief['window_seconds'] = .25
        busy['native_wayland_stages'] = {'egl_fence_wait_ms': busy_fence}
        brief['native_wayland_stages'] = {
            'egl_fence_wait_ms': brief['stages']['decode_ms'],
            'va_surface_wait_ms': viewer_window(session, 1, 0)['stages']['decode_ms'],
        }
        self.read([window(), old, busy, brief])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
        stages = report['native_wayland_stages']
        self.assertEqual(report['windows'], 3)
        self.assertEqual(set(stages), {'egl_fence_wait_ms', 'va_surface_wait_ms'})
        fence = stages['egl_fence_wait_ms']
        self.assertEqual(fence['windows'], 2)
        self.assertEqual(fence['observations'], 1001)
        self.assertAlmostEqual(fence['whole_run_weighted_mean_ms'], 4100 / 1001)
        self.assertEqual(fence['max_reported_window_recent_p95_ms'], 120)
        self.assertIsNone(fence['whole_run_percentiles'])
        wait = stages['va_surface_wait_ms']
        self.assertEqual(wait['windows'], 1)
        self.assertEqual(wait['observations'], 1)
        self.assertEqual(wait['whole_run_weighted_mean_ms'], 0)

    def test_native_wayland_stage_malformed_or_wrong_backend_rejected(self):
        session = '8' * 32
        viewer = viewer_window(session, 5, 3)
        measured = viewer['stages']['decode_ms']
        empty = viewer['stages']['hardware_download_ms']
        malformed = (
            None, [], {}, {'unknown_ms': measured}, {'egl_fence_wait_ms': []},
            {'egl_fence_wait_ms': empty},
            {'egl_fence_wait_ms': dict(measured, observations=True)},
            {'egl_fence_wait_ms': dict(measured, observations=4)},
            {'egl_fence_wait_ms': dict(measured, mean=None)},
            {'egl_fence_wait_ms': dict(measured, mean=float('nan'))},
            {'egl_fence_wait_ms': dict(measured, quantiles=dict(measured['quantiles'], p95_ms=0))},
        )
        viewer['viewer_backend'] = 'wayland'
        for stages in malformed:
            with self.subTest(stages=stages), self.assertRaises(ValueError):
                self.read([window(), dict(viewer, native_wayland_stages=stages)])
                m.read_viewer_windows(self.path)
        valid = {'egl_fence_wait_ms': measured}
        self.read([window(), dict(viewer, viewer_backend='x11', native_wayland_stages=valid)])
        with self.assertRaisesRegex(ValueError, 'require the Wayland viewer backend'):
            m.read_viewer_windows(self.path)

    def test_native_x11_stages_keep_deferred_picture_setup_absent(self):
        session = '9' * 32
        exact = viewer_window(session, 25, 3)
        scaled = viewer_window(session, 2, 6)
        exact['native_x11_stages'] = {'x11_backing_populate_ms': exact['stages']['decode_ms']}
        self.read([window(), exact])
        report = m.summarize_viewer(m.read_viewer_windows(self.path), [])
        self.assertNotIn('x11_picture_setup_ms', report['native_x11_stages'])
        scaled['native_x11_stages'] = {
            'x11_backing_populate_ms': scaled['stages']['decode_ms'],
            'x11_picture_setup_ms': scaled['stages']['decode_ms'],
        }
        self.read([window(), exact, scaled])
        stages = m.summarize_viewer(m.read_viewer_windows(self.path), [])['native_x11_stages']
        self.assertEqual(stages['x11_picture_setup_ms']['observations'], 2)
        self.assertEqual(stages['x11_backing_populate_ms']['observations'], 27)
        self.read([window(), dict(exact, viewer_backend='wayland')])
        with self.assertRaisesRegex(ValueError, 'require the X11 viewer backend'):
            m.read_viewer_windows(self.path)

    def test_x11_capture_uses_all_observations_and_reports_coverage(self):
        older, a, b = window(), window(), window()
        a.update(backpressure_skipped=0, damage_skipped=30, x11_capture=x11_capture(1000, 4))
        a['x11_capture'].update(damage_notifications=43, forced_refreshes=1, secret='PRIVATE')
        b.update(damage_skipped=2, x11_capture=x11_capture(1, 100))
        b['x11_capture']['stages']['shm_readback_ms'] = x11_capture(23, 3)['stages']['core_readback_ms']
        b['window_seconds'] = .25
        report = m.summarize(*self.read([older, a, b]), 'terminal', 'x11')
        self.assertEqual(report['totals']['backpressure_skipped'], 0)
        self.assertEqual(report['totals']['damage_skipped'], 32)
        self.assertEqual(report['counter_windows'], {'backpressure_skipped': 1, 'damage_skipped': 2})
        capture = report['x11_capture']
        self.assertEqual(capture['windows'], 2)
        self.assertEqual(capture['reported_seconds'], 5.25)
        self.assertEqual(capture['totals'], {'damage_notifications': 43, 'forced_refreshes': 1})
        readback = capture['stages']['core_readback_ms']
        self.assertEqual(readback['observations'], 1001)
        self.assertAlmostEqual(readback['whole_run_weighted_mean_ms'], 4100 / 1001)
        self.assertEqual(readback['window_means_range_ms'], [4, 100])
        self.assertEqual(readback['max_reported_window_recent_p95_ms'], 100)
        self.assertIsNone(readback['whole_run_percentiles'])
        shm = capture['stages']['shm_readback_ms']
        self.assertEqual(shm['observations'], 23)
        self.assertEqual(shm['whole_run_weighted_mean_ms'], 3)
        resize = capture['stages']['cpu_resize_ms']
        self.assertEqual(resize['observations'], 0)
        self.assertIsNone(resize['whole_run_weighted_mean_ms'])
        self.assertIsNone(resize['max_reported_window_recent_p95_ms'])
        self.assertNotIn('PRIVATE', json.dumps(report))

    def test_invalid_x11_capture_observations_and_skip_counts_are_rejected(self):
        mutations = [
            lambda x: x.update(backpressure_skipped=-1),
            lambda x: x.update(damage_skipped=True),
            lambda x: x.update(x11_capture=None),
            lambda x: x['x11_capture'].update(damage_notifications=float('inf')),
            lambda x: x['x11_capture'].update(forced_refreshes=.5),
            lambda x: x['x11_capture']['stages'].pop('cpu_resize_ms'),
            lambda x: x['x11_capture']['stages']['core_readback_ms'].update(observations=129),
            lambda x: x['x11_capture']['stages']['core_readback_ms'].update(observations=2**64),
            lambda x: x['x11_capture']['stages']['cpu_resize_ms'].update(mean=1),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                record = window()
                record['x11_capture'] = x11_capture(1, 4)
                mutate(record)
                with self.assertRaises(ValueError):
                    self.read([record])

    def test_optional_h264_timing_uses_encoded_frames(self):
        a, b = window(1000, 4), window(1, 100)
        for record in (a, b):
            record['h264_frames'] = record['captured']
            record['stages']['h264_ms'] = record['stages']['capture_ms']
        report = m.summarize(*self.read([window(), a, b]), 'terminal', 'h264')
        self.assertEqual(report['totals']['h264_frames'], 1001)
        self.assertEqual(report['counter_windows']['h264_frames'], 2)
        self.assertAlmostEqual(report['stages']['h264_ms']['whole_run_weighted_mean_ms'], 4100 / 1001)
        b['h264_frames'] = 0
        with self.assertRaisesRegex(ValueError, 'h264_ms and h264_frames disagree'):
            self.read([b])

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

    def test_physical_p95_counts_unobserved_attempts(self):
        data = 'trial,sample,latency_ms\n'
        for trial, observed in (('a', 95), ('b', 94), ('c', 0)):
            data += ''.join(f'{trial},{i},40\n' for i in range(1, observed + 1))
            data += f'{trial},100,timeout\n'
            data += ''.join(f'{trial},{i},no_response\n' for i in range(observed + 1, 100))
        r = self.csv(data)
        self.assertTrue(r['minimum_sampling_complete'])
        self.assertFalse(r['all_trial_p95_within_target'])
        a, b, c = (r['trials'][trial] for trial in ('a', 'b', 'c'))
        self.assertEqual((a['samples'], a['attempts'], a['p95_ms']), (95, 100, 40))
        self.assertTrue(a['p95_is_upper_bound'])
        self.assertEqual(a['unobserved_sample_ids'], {'timeout': [100], 'no_response': [96, 97, 98, 99]})
        self.assertIsNone(b['p95_ms'])
        self.assertIsNone(c['mean_ms'])

    def test_physical_invalid_samples_and_duplicate_ids(self):
        for body in ('a,1,nan\n', 'a,1,-1\n', 'a,1,inf\n', 'a,1,4\na,1,5\n',
                     'a,1,timeout\na,1,5\n', 'a,1,\n', 'a,0,4\n', 'a,1,4,5\n', 'a,1\n'):
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
