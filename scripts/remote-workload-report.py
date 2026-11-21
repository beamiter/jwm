#!/usr/bin/env python3
"""Analyze one controlled remote workload run; never measures physical input itself."""
import argparse
import csv
import json
import math
from pathlib import Path
import re
import sys

MAX_BYTES = 32 * 1024 * 1024
MAX_LINE = 65536
MAX_WINDOWS = 10000
MAX_PHYSICAL_SAMPLES = 10000
UNOBSERVED_PHYSICAL_OUTCOMES = ('timeout', 'no_response')
STAGES = (
    'capture_ms', 'mailbox_age_ms', 'credit_wait_ms', 'dirty_scan_ms',
    'atlas_pack_ms', 'jpeg_ms', 'atlas_pack_and_jpeg_ms', 'writer_wait_ms',
    'socket_write_ms', 'capture_start_to_ack_ms', 'send_to_ack_ms',
)
# Only these denominators are actually present in host schema 1. In particular,
# the bounded quantile sample count must NOT weight whole-window means.
COUNTS = {'capture_ms': 'captured', 'writer_wait_ms': 'sent',
          'socket_write_ms': 'sent', 'capture_start_to_ack_ms': 'drawn_acks',
          'send_to_ack_ms': 'drawn_acks'}
COUNTERS = ('scheduled', 'captured', 'sent', 'bytes', 'replaced',
            'unchanged_suppressed', 'dirty_tiles', 'total_tiles',
            'drawn_acks', 'viewer_superseded')
OPTIONAL_COUNTERS = ('backpressure_skipped', 'damage_skipped', 'h264_frames', 'hevc444_frames')
OPTIONAL_STAGES = {'h264_ms': 'h264_frames', 'hevc444_ms': 'hevc444_frames'}
X11_CAPTURE_COUNTERS = ('damage_notifications', 'forced_refreshes')
X11_CAPTURE_STAGES = ('monitor_geometry_ms', 'core_readback_ms',
                      'pixel_conversion_ms', 'cpu_resize_ms')
OPTIONAL_X11_CAPTURE_STAGES = ('shm_readback_ms',)
POLICY = 'nearest-rank, most recent 128 observations per stage per window'
VIEWER_COUNTERS = ('received', 'decoded', 'replaced', 'drawn', 'acked')
VIEWER_STAGES = (
    'decode_ms', 'presentation_queue_ms', 'presentation_completion_ms',
    'hardware_decode_ms', 'hardware_download_ms', 'hardware_conversion_ms',
    'hardware_rgb_copy_ms', 'first_presentation_ms', 'source_switch_presentation_ms',
    'decode_queue_ms',
    'cursor_interval_ms', 'sender_control_queue_ms', 'sender_input_queue_ms',
    'sender_bulk_queue_ms', 'sender_control_write_ms', 'sender_bulk_write_ms',
    'sender_oldest_queued_ms',
)
VIEWER_QUEUE_FIELDS = ('control_records', 'bulk_records', 'control_bytes', 'bulk_bytes')
NATIVE_VIEWER_STAGE = 'native_preparation_and_submit_ms'
DIRECT_MAP_STAGE = 'hardware_direct_map_ms'
X11_PREPARATION_STAGES = ('x11_cpu_prepare_ms', 'x11_presenter_submit_ms')
X11_PREPARATION_COUNTS = ('scheduled', 'prepared', 'submitted', 'fallbacks', 'stale')
OPTIONAL_VIEWER_STAGES = (NATIVE_VIEWER_STAGE, 'frame_header_wait_ms',
                          'frame_body_receive_ms', 'frame_authentication_ms',
                          DIRECT_MAP_STAGE, *X11_PREPARATION_STAGES)
NATIVE_RENDER_STAGES = (
    'va_surface_wait_ms', 'va_frame_export_ms', 'rgb_target_allocation_ms',
    'egl_prepare_and_submit_ms', 'egl_fence_wait_ms',
)
NATIVE_BACKEND_STAGES = {
    'wayland': (*NATIVE_RENDER_STAGES, 'wayland_dmabuf_import_ms'),
    'x11': (*NATIVE_RENDER_STAGES, 'x11_dmabuf_import_ms', 'x11_picture_setup_ms',
            'x11_backing_populate_ms', 'x11_window_submit_ms', 'x11_prefetch_rgb_allocation_ms'),
}
HARDWARE_RGB_FIELDS = {
    'pool': ('fitting_checkouts', 'fresh_reserves', 'growth_reserves',
             'requested_bytes', 'added_capacity_bytes'),
    'download': ('empty_targets_populated', 'retained_targets_reused',
                 'populated_targets_replaced'),
    'copy': ('full_frame_copies', 'full_frame_bytes', 'banded_copies', 'banded_bytes'),
}
VAAPI_MAP_FIELDS = ('mapped_frames', 'staged_bytes', 'fallback_frames')
VIDEO_CODECS = ('JPEG', 'H.264', 'HEVC 4:4:4')
HOST_OBSERVED_STAGES = {
    'hardware_encode_stages': ('padding_ms', 'rgb_copy_ms', 'conversion_ms', 'submit_ms', 'output_ms'),
    'cursor_stages': ('poll_ms', 'poll_interval_ms'),
    'wayland_capture': ('readback_ms', 'conversion_ms', 'resize_ms'),
}


def number(value, name, integer=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(name + ' must be a number')
    if not math.isfinite(value) or value < 0 or (integer and not isinstance(value, int)):
        raise ValueError(name + ' must be finite and nonnegative')
    return value


def no_duplicates(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON key')
        result[key] = value
    return result


def bounded_lines(path):
    total = 0
    with path.open('rb') as stream:
        while True:
            raw = stream.readline(MAX_LINE + 1)
            if not raw:
                break
            total += len(raw)
            if len(raw) > MAX_LINE or total > MAX_BYTES:
                raise ValueError('input exceeds byte/line limit')
            yield raw.decode('utf-8', errors='strict')


def stage_quantile_count(item, stage, nullable_mean=False):
    if not isinstance(item, dict):
        raise ValueError('invalid stage object')
    if not nullable_mean or item.get('mean') is not None:
        number(item.get('mean'), stage)
    quantiles = item.get('quantiles')
    if not isinstance(quantiles, dict):
        raise ValueError('invalid quantiles object')
    count = number(quantiles.get('samples'), stage + '.samples', integer=True)
    if count > 128:
        raise ValueError('quantile sample limit exceeded')
    values = [quantiles.get(k) for k in ('p50_ms', 'p95_ms', 'p99_ms')]
    if count == 0:
        if values != [None, None, None]:
            raise ValueError('empty quantiles must be null')
    else:
        for value in values:
            number(value, stage + '.quantile')
        if values != sorted(values):
            raise ValueError('quantiles out of order')
    return count


def validate_observed_stage(item, stage):
    count = stage_quantile_count(item, stage, nullable_mean=True)
    observations = number(item.get('observations'), stage + '.observations', integer=True)
    if (observations > 2**64 - 1 or count != min(observations, 128)
            or (observations == 0) != (item.get('mean') is None)):
        raise ValueError(f'inconsistent stage observation counts: {stage}')


def validate_native_stages(stages, backend, group):
    if not isinstance(stages, dict) or not stages:
        raise ValueError('invalid ' + group)
    for name, item in stages.items():
        if name not in NATIVE_BACKEND_STAGES[backend]:
            raise ValueError(f'unknown stage in {group}: {name}')
        validate_observed_stage(item, group + '.' + name)
        if item['observations'] == 0:
            raise ValueError(f'{group} stage has no observations: {name}')


def validate_hardware_rgb_work(work):
    if not isinstance(work, dict):
        raise ValueError('invalid hardware RGB work object')
    groups = HARDWARE_RGB_FIELDS
    if 'vaapi_map' in work:
        groups = groups | {'vaapi_map': VAAPI_MAP_FIELDS}
    for group, fields in groups.items():
        data = work.get(group)
        if not isinstance(data, dict):
            raise ValueError(f'invalid hardware RGB work group: {group}')
        for field in fields:
            value = data.get(field)
            if type(value) is not int or not 0 <= value <= 2**64 - 1:
                raise ValueError(f'invalid hardware RGB work counter: {group}.{field}')
    pool, download, copy = (work[key] for key in ('pool', 'download', 'copy'))
    checkouts = pool['fitting_checkouts'] + pool['fresh_reserves'] + pool['growth_reserves']
    reserves = pool['fresh_reserves'] + pool['growth_reserves']
    transfers = sum(download[field] for field in HARDWARE_RGB_FIELDS['download'])
    copies = copy['full_frame_copies'] + copy['banded_copies']
    copied_bytes = copy['full_frame_bytes'] + copy['banded_bytes']
    mapped = work.get('vaapi_map', {})
    mapped_frames = mapped.get('mapped_frames', 0)
    if (checkouts != transfers + mapped_frames or checkouts != copies
            or pool['requested_bytes'] != copied_bytes
            or mapped.get('fallback_frames', 0) > transfers
            or (mapped_frames == 0) != (mapped.get('staged_bytes', 0) == 0)
            or (copy['full_frame_copies'] == 0) != (copy['full_frame_bytes'] == 0)
            or (copy['banded_copies'] == 0) != (copy['banded_bytes'] == 0)
            or (reserves == 0) != (pool['added_capacity_bytes'] == 0)):
        raise ValueError('inconsistent hardware RGB work counters')


def telemetry_windows(path, kind):
    for line in bounded_lines(path):
        if not line.lstrip().startswith('{'):
            yield None
            continue
        try:
            value = json.loads(line, object_pairs_hook=no_duplicates)
        except (json.JSONDecodeError, ValueError) as exc:
            raise ValueError('invalid JSON object in telemetry input') from exc
        if not isinstance(value, dict) or value.get('type') != kind:
            yield None
            continue
        if (type(value.get('schema_version')) is not int or value.get('schema_version') != 1
                or value.get('quantile_policy') != POLICY):
            raise ValueError('unsupported telemetry schema or timing policy')
        if not isinstance(value.get('version'), str) or not re.fullmatch(r'[A-Za-z0-9.+_-]{1,64}', value['version']):
            raise ValueError('invalid build version')
        commit = value.get('commit')
        if commit is not None and (not isinstance(commit, str) or not re.fullmatch(r'[0-9a-fA-F]{7,64}', commit)):
            raise ValueError('invalid build commit')
        session = value.get('session_id')
        if session is not None and (not isinstance(session, str) or not re.fullmatch(r'[0-9a-f]{32}', session)):
            raise ValueError('invalid session ID')
        if not isinstance(value.get('stages'), dict):
            raise ValueError('invalid stages object')
        if number(value.get('window_seconds'), 'window_seconds') <= 0:
            raise ValueError('window_seconds must be positive')
        yield value


def validate_x11_capture(capture):
    if not isinstance(capture, dict) or not isinstance(capture.get('stages'), dict):
        raise ValueError('invalid X11 capture object')
    for field in X11_CAPTURE_COUNTERS:
        number(capture.get(field), 'x11_capture.' + field, integer=True)
    items = capture['stages']
    for stage in X11_CAPTURE_STAGES + OPTIONAL_X11_CAPTURE_STAGES:
        if stage in OPTIONAL_X11_CAPTURE_STAGES and stage not in items:
            continue
        item = items.get(stage)
        count = stage_quantile_count(item, 'x11_capture.' + stage)
        observations = number(item.get('observations'), stage + '.observations', integer=True)
        if observations > 2**64 - 1 or count != min(observations, 128):
            raise ValueError('inconsistent X11 capture observation counts')
        if observations == 0 and item['mean'] != 0:
            raise ValueError('nonzero X11 capture mean without observations')


def read_windows(path):
    windows = []
    ignored = 0
    for value in telemetry_windows(path, 'jwm_remote_host_stats'):
        if value is None:
            ignored += 1
            continue
        for field in COUNTERS:
            number(value.get(field), field, integer=True)
        for field in OPTIONAL_COUNTERS:
            if field in value:
                number(value[field], field, integer=True)
        if 'x11_capture' in value:
            validate_x11_capture(value['x11_capture'])
        for group, stages in HOST_OBSERVED_STAGES.items():
            if group not in value:
                continue
            item = value[group]
            if not isinstance(item, dict):
                raise ValueError(f'invalid host stage group: {group}')
            if group == 'wayland_capture':
                for field in ('readback_bytes', 'staging_growths'):
                    number(item.get(field), f'{group}.{field}', integer=True)
                item = item.get('stages')
                if not isinstance(item, dict):
                    raise ValueError('invalid Wayland capture stages')
            for stage in stages:
                validate_observed_stage(item.get(stage), f'{group}.{stage}')
        for field in ('source_dimensions', 'encoded_dimensions'):
            dims = value.get(field)
            if not isinstance(dims, list) or len(dims) != 2:
                raise ValueError('invalid dimensions')
            for dim in dims:
                if number(dim, field, integer=True) > 65535:
                    raise ValueError('invalid dimensions')
        for stage in STAGES:
            stage_quantile_count(value['stages'].get(stage), stage)
        for stage, counter in OPTIONAL_STAGES.items():
            if stage in value['stages'] or counter in value:
                samples = stage_quantile_count(value['stages'].get(stage), stage)
                if counter not in value or samples != min(128, value[counter]):
                    raise ValueError(f'{stage} and {counter} disagree')
        windows.append(value)
        if len(windows) > MAX_WINDOWS:
            raise ValueError('too many telemetry windows')
    if not windows:
        raise ValueError('no host telemetry records')
    return windows, ignored


def read_viewer_windows(path):
    windows = []
    for value in telemetry_windows(path, 'jwm_remote_viewer_stats'):
        if value is None:
            continue
        if value.get('session_id') is None:
            raise ValueError('viewer session ID is missing')
        if value.get('viewer_backend') not in ('x11', 'wayland'):
            raise ValueError('invalid viewer backend')
        if value.get('negotiated_codec') not in VIDEO_CODECS:
            raise ValueError('invalid negotiated viewer codec')
        last_frame = value.get('last_frame')
        if last_frame is not None and (not isinstance(last_frame, dict) or last_frame.get('codec') not in VIDEO_CODECS):
            raise ValueError('invalid viewer frame codec')
        for field in VIEWER_COUNTERS:
            number(value.get(field), field, integer=True)
        queue = value.get('sender_max_sampled_at_dequeue')
        if not isinstance(queue, dict):
            raise ValueError('invalid sender queue object')
        for field in VIEWER_QUEUE_FIELDS:
            number(queue.get(field), 'sender.' + field, integer=True)
        for stage in VIEWER_STAGES:
            validate_observed_stage(value['stages'].get(stage), stage)
        for stage in OPTIONAL_VIEWER_STAGES:
            if stage in value['stages']:
                validate_observed_stage(value['stages'][stage], stage)
        for backend in NATIVE_BACKEND_STAGES:
            group = f'native_{backend}_stages'
            if group in value:
                if value['viewer_backend'] != backend:
                    label = backend.capitalize()
                    raise ValueError(f'native {label} stages require the {label} viewer backend')
                validate_native_stages(value[group], backend, group)
        if 'x11_cpu_preparation' in value:
            preparation = value['x11_cpu_preparation']
            if not isinstance(preparation, dict):
                raise ValueError('invalid X11 CPU preparation counts')
            for field in X11_PREPARATION_COUNTS:
                number(preparation.get(field), 'x11_cpu_preparation.' + field, integer=True)
            revision = preparation.get('last_submitted_viewport_revision')
            if revision is not None:
                number(revision, 'x11 CPU preparation revision', integer=True)
            if bool(preparation['submitted']) != (revision is not None):
                raise ValueError('X11 CPU preparation submission revision disagrees with its count')
            work = value['stages'].get('x11_cpu_prepare_ms')
            if work and max(preparation['prepared'], preparation['stale']) > work['observations']:
                raise ValueError('X11 CPU preparation counts exceed work observations')
        if 'hardware_rgb_work' in value:
            validate_hardware_rgb_work(value['hardware_rgb_work'])
            mapping = value['hardware_rgb_work'].get('vaapi_map')
            timing = value['stages'].get(DIRECT_MAP_STAGE)
            if mapping is not None and timing is not None and (
                    mapping['mapped_frames'] != timing['observations']):
                raise ValueError('inconsistent hardware direct mapping observations')
        if 'native_presented' in value:
            native = number(value['native_presented'], 'native_presented', integer=True)
            if native > value['drawn']:
                raise ValueError('native presentation count exceeds drawing count')
            if value.get('last_presentation_path') not in (
                None, 'rgb', 'x11-vaapi-dmabuf-dri3', 'wayland-vaapi-dmabuf'
            ):
                raise ValueError('invalid viewer presentation path')
        windows.append(value)
        if len(windows) > MAX_WINDOWS:
            raise ValueError('too many viewer telemetry windows')
    if not windows:
        raise ValueError('no viewer telemetry records')
    return windows


def summarize_observations(reported):
    observed = [item for item in reported if item['observations'] > 0]
    count = sum(item['observations'] for item in observed)
    means = [item['mean'] for item in observed]
    return {
        'observations': count,
        'whole_run_weighted_mean_ms': (sum(item['mean'] * (item['observations'] / count)
                                           for item in observed) if count else None),
        'mean_denominator': 'observations',
        'window_means_range_ms': [min(means), max(means)] if means else None,
        'max_reported_window_recent_p95_ms': max(
            (item['quantiles']['p95_ms'] for item in observed), default=None),
        'whole_run_percentiles': None,
    }


def summarize_viewer(windows, host):
    builds = {(window['version'], window.get('commit')) for window in windows}
    if len(builds) != 1:
        raise ValueError('mixed viewer builds: analyze separate runs')
    version, commit = next(iter(builds))
    viewer_ids = {window['session_id'] for window in windows}
    host_ids = {window['session_id'] for window in host if window.get('session_id') is not None}
    if host_ids and not (viewer_ids & host_ids):
        raise ValueError('viewer and host telemetry describe different sessions')
    summary = {
        'version': version,
        'commit': commit,
        'windows': len(windows),
        'reported_seconds': sum(window['window_seconds'] for window in windows),
        'totals': {field: sum(window[field] for window in windows) for field in VIEWER_COUNTERS},
        'backends': sorted({window['viewer_backend'] for window in windows}),
        'negotiated_codecs': sorted({window['negotiated_codec'] for window in windows}),
        'last_frame_codecs_seen': sorted({window['last_frame']['codec']
                                         for window in windows if window.get('last_frame')}),
        'sender_max_sampled_at_dequeue': {
            field: max(window['sender_max_sampled_at_dequeue'][field] for window in windows)
            for field in VIEWER_QUEUE_FIELDS
        },
        'session_matching': {
            'matched': len(viewer_ids & host_ids),
            'viewer_only': len(viewer_ids - host_ids),
            'host_only': len(host_ids - viewer_ids),
            'host_windows_without_id': sum(window.get('session_id') is None for window in host),
        },
        'stages': {stage: summarize_observations(window['stages'][stage] for window in windows)
                   for stage in VIEWER_STAGES},
    }
    native_windows = [window for window in windows if 'native_presented' in window]
    if native_windows:
        summary['native_presentation'] = {
            'windows': len(native_windows),
            'presented_frames': sum(window['native_presented'] for window in native_windows),
            'last_paths_seen': sorted({window['last_presentation_path'] for window in native_windows
                                       if window.get('last_presentation_path') is not None}),
        }
    for stage in OPTIONAL_VIEWER_STAGES:
        reported = [window['stages'][stage] for window in windows if stage in window['stages']]
        if reported:
            summary['stages'][stage] = summarize_observations(reported)
    for backend, stages in NATIVE_BACKEND_STAGES.items():
        group = f'native_{backend}_stages'
        native_stage_windows = [window[group] for window in windows if group in window]
        if not native_stage_windows:
            continue
        summary[group] = {}
        for stage in stages:
            reported = [window[stage] for window in native_stage_windows if stage in window]
            if reported:
                summary[group][stage] = {
                    'windows': len(reported), **summarize_observations(reported),
                }
    rgb_windows = [window['hardware_rgb_work'] for window in windows if 'hardware_rgb_work' in window]
    if rgb_windows:
        work_summary = {
            'windows': len(rgb_windows),
            'totals': {group: {field: sum(work[group][field] for work in rgb_windows)
                               for field in fields}
                       for group, fields in HARDWARE_RGB_FIELDS.items()},
        }
        maps = [work['vaapi_map'] for work in rgb_windows if 'vaapi_map' in work]
        if maps:
            work_summary['vaapi_map'] = {
                'windows': len(maps),
                'totals': {field: sum(mapping[field] for mapping in maps)
                           for field in VAAPI_MAP_FIELDS},
            }
        summary['hardware_rgb_work'] = work_summary
    x11_windows = [window['x11_cpu_preparation'] for window in windows
                   if window['viewer_backend'] == 'x11' and 'x11_cpu_preparation' in window]
    if x11_windows:
        summary['x11_cpu_preparation'] = {
            'windows': len(x11_windows),
            'totals': {key: sum(window[key] for window in x11_windows)
                       for key in X11_PREPARATION_COUNTS},
            'last_submitted_viewport_revisions': sorted({
                window['last_submitted_viewport_revision'] for window in x11_windows
                if window['last_submitted_viewport_revision'] is not None
            }),
        }
    return summary


def physical_samples(path):
    rows = csv.DictReader(bounded_lines(path))
    if rows.fieldnames != ['trial', 'sample', 'latency_ms']:
        raise ValueError('physical CSV header must be trial,sample,latency_ms')
    trials = {}
    seen = set()
    for row in rows:
        if None in row or any(v is None for v in row.values()):
            raise ValueError('invalid physical CSV row')
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,32}', row['trial']):
            raise ValueError('invalid trial ID')
        if not re.fullmatch(r'[1-9][0-9]{0,8}', row['sample']):
            raise ValueError('invalid sample ID')
        key = (row['trial'], int(row['sample']))
        if key in seen:
            raise ValueError('duplicate physical sample')
        seen.add(key)
        if len(seen) > MAX_PHYSICAL_SAMPLES:
            raise ValueError('too many physical samples')
        samples, unobserved = trials.setdefault(row['trial'], ([], {}))
        latency = row['latency_ms']
        if latency in UNOBSERVED_PHYSICAL_OUTCOMES:
            unobserved.setdefault(latency, []).append(key[1])
        else:
            samples.append(number(float(latency), 'latency_ms'))
    if not trials:
        raise ValueError('empty physical measurements')
    result = {}
    for trial, (values, unobserved) in sorted(trials.items()):
        values.sort()
        attempts = len(values) + sum(len(ids) for ids in unobserved.values())
        # Place unresolved observations beyond all measured values to bound p95.
        rank = (95 * attempts + 99) // 100
        p95 = values[rank - 1] if rank <= len(values) else None
        result[trial] = {
            'samples': len(values),
            'attempts': attempts,
            'unobserved_sample_ids': {outcome: sorted(ids) for outcome, ids in sorted(unobserved.items())},
            'mean_ms': sum(values) / len(values) if values else None,
            'p95_ms': p95,
            'p95_is_upper_bound': bool(unobserved) and p95 is not None,
        }
    return {'source': 'operator_supplied_physical_measurements_not_host_ack',
            'trials': result,
            'minimum_sampling_complete': len(result) >= 3 and all(t['attempts'] >= 100 for t in result.values()),
            'target_ms': 50,
            'all_trial_p95_within_target': all(t['p95_ms'] is not None and t['p95_ms'] <= 50
                                               for t in result.values()),
            'measurement_method_verified': False}


def summarize_x11_capture(windows):
    capture_windows = [window for window in windows if 'x11_capture' in window]
    if not capture_windows:
        return None
    captures = [window['x11_capture'] for window in capture_windows]
    stages = {}
    for stage in X11_CAPTURE_STAGES + OPTIONAL_X11_CAPTURE_STAGES:
        reported = [capture['stages'][stage] for capture in captures if stage in capture['stages']]
        if not reported and stage in OPTIONAL_X11_CAPTURE_STAGES:
            continue
        stages[stage] = summarize_observations(reported)
    return {
        'windows': len(capture_windows),
        'reported_seconds': sum(window['window_seconds'] for window in capture_windows),
        'totals': {field: sum(capture[field] for capture in captures)
                   for field in X11_CAPTURE_COUNTERS},
        'stages': stages,
    }


def summarize(windows, ignored, workload, run_id):
    # Mixing different builds in one run can make a regression look better.
    versions = {(w.get('version'), w.get('commit')) for w in windows}
    if len(versions) != 1:
        raise ValueError('mixed builds: analyze separate runs')
    version, commit = next(iter(versions))
    if not isinstance(version, str) or not re.fullmatch(r'[A-Za-z0-9.+_-]{1,64}', version):
        raise ValueError('invalid build version')
    if commit is not None and (not isinstance(commit, str) or not re.fullmatch(r'[0-9a-fA-F]{7,64}', commit)):
        raise ValueError('invalid build commit')
    seconds = sum(w['window_seconds'] for w in windows)
    totals = {field: sum(w[field] for w in windows) for field in COUNTERS}
    counter_windows = {}
    for field in OPTIONAL_COUNTERS:
        values = [w[field] for w in windows if field in w]
        if values:
            counter_windows[field] = len(values)
            totals[field] = sum(values)
    stages = {}
    for stage in (*STAGES, *OPTIONAL_STAGES):
        applicable = [w for w in windows if stage in w['stages']]
        if not applicable:
            continue
        means = [w['stages'][stage]['mean'] for w in applicable
                 if w['stages'][stage]['quantiles']['samples'] > 0]
        q95 = [w['stages'][stage]['quantiles']['p95_ms'] for w in applicable
               if w['stages'][stage]['quantiles']['samples'] > 0]
        counter = COUNTS.get(stage) or OPTIONAL_STAGES.get(stage)
        count = totals[counter] if counter else None
        stages[stage] = {
            'whole_run_weighted_mean_ms': (sum(w['stages'][stage]['mean'] * w[counter] for w in applicable) / count
                                           if count else None),
            'mean_denominator': counter,
            'window_means_range_ms': [min(means), max(means)] if means else None,
            'max_reported_window_recent_p95_ms': max(q95) if q95 else None,
            'whole_run_percentiles': None,
        }
    report = {
        'schema_version': 1, 'kind': 'jwm_remote_workload_report',
        'run_id': run_id, 'workload': workload, 'version': version, 'commit': commit,
        'windows': len(windows), 'ignored_diagnostic_lines': ignored,
        'reported_seconds': seconds, 'totals': totals,
        'sent_frames_per_reported_second': totals['sent'] / seconds,
        'payload_mbit_per_reported_second': totals['bytes'] * 8 / seconds / 1_000_000,
        'stages': stages,
        'dimensions_seen': {f: sorted({tuple(w[f]) for w in windows})
                            for f in ('source_dimensions', 'encoded_dimensions')},
        'physical_input_to_display': None,
        'limitations': [
            'ACK timing is not physical key-to-photon latency or network RTT.',
            'Recent per-window quantiles cannot reconstruct whole-run percentiles.',
            'Stages overlap and have different denominators; do not add their means.',
            'Mode, GPU synchronization and readback subdivision require separate capture traces.',
            'No competitor ranking, hardware target or end-to-end acceptance is inferred.',
        ],
    }
    if counter_windows:
        report['counter_windows'] = counter_windows
    if (x11_capture := summarize_x11_capture(windows)) is not None:
        report['x11_capture'] = x11_capture
    for group, stages in HOST_OBSERVED_STAGES.items():
        recorded = [window[group] for window in windows if group in window]
        if not recorded:
            continue
        if group == 'wayland_capture':
            report[group] = {
                'stages': {stage: summarize_observations(item['stages'][stage] for item in recorded)
                           for stage in stages},
                'readback_bytes': sum(item['readback_bytes'] for item in recorded),
                'staging_growths': sum(item['staging_growths'] for item in recorded),
            }
        else:
            report[group] = {stage: summarize_observations(item[stage] for item in recorded)
                             for stage in stages}
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--telemetry', type=Path, required=True)
    parser.add_argument('--viewer-telemetry', type=Path,
                        help='viewer JSON stderr log (may be the same SSH log)')
    parser.add_argument('--workload', choices=('terminal', 'browser', 'blender', 'isaac-sim'), required=True)
    parser.add_argument('--run-id', required=True, help='public non-personal label, 1-64 ASCII letters/digits/_/-')
    parser.add_argument('--physical-csv', type=Path)
    args = parser.parse_args(argv)
    try:
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', args.run_id):
            raise ValueError('invalid run ID')
        windows, ignored = read_windows(args.telemetry)
        result = summarize(windows, ignored, args.workload, args.run_id)
        if args.viewer_telemetry:
            result['viewer'] = summarize_viewer(read_viewer_windows(args.viewer_telemetry), windows)
        if args.physical_csv:
            result['physical_input_to_display'] = physical_samples(args.physical_csv)
        print(json.dumps(result, indent=2, sort_keys=True, allow_nan=False))
        return 0
    except (ValueError, OSError, UnicodeError, TypeError, KeyError, OverflowError, csv.Error) as exc:
        print('benchmark input rejected: ' + str(exc), file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
