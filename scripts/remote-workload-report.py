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
OPTIONAL_COUNTERS = ('backpressure_skipped', 'damage_skipped')
X11_CAPTURE_COUNTERS = ('damage_notifications', 'forced_refreshes')
X11_CAPTURE_STAGES = ('monitor_geometry_ms', 'core_readback_ms',
                      'pixel_conversion_ms', 'cpu_resize_ms')
POLICY = 'nearest-rank, most recent 128 observations per stage per window'


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


def stage_quantile_count(item, stage):
    if not isinstance(item, dict):
        raise ValueError('invalid stage object')
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


def validate_x11_capture(capture):
    if not isinstance(capture, dict) or not isinstance(capture.get('stages'), dict):
        raise ValueError('invalid X11 capture object')
    for field in X11_CAPTURE_COUNTERS:
        number(capture.get(field), 'x11_capture.' + field, integer=True)
    for stage in X11_CAPTURE_STAGES:
        item = capture['stages'].get(stage)
        count = stage_quantile_count(item, 'x11_capture.' + stage)
        observations = number(item.get('observations'), stage + '.observations', integer=True)
        if observations > 2**64 - 1 or count != min(observations, 128):
            raise ValueError('inconsistent X11 capture observation counts')
        if observations == 0 and item['mean'] != 0:
            raise ValueError('nonzero X11 capture mean without observations')


def read_windows(path):
    windows = []
    ignored = 0
    for line in bounded_lines(path):
        # Stderr diagnostics are not copied to the report: they can contain
        # addresses or arbitrary process messages. Only typed records count.
        if not line.lstrip().startswith('{'):
            ignored += 1
            continue
        try:
            value = json.loads(line, object_pairs_hook=no_duplicates)
        except (json.JSONDecodeError, ValueError) as exc:
            raise ValueError('invalid JSON object in telemetry input') from exc
        if not isinstance(value, dict) or value.get('type') != 'jwm_remote_host_stats':
            ignored += 1
            continue
        if type(value.get('schema_version')) is not int or value.get('schema_version') != 1 or value.get('quantile_policy') != POLICY:
            raise ValueError('unsupported telemetry schema or timing policy')
        if not isinstance(value.get('version'), str) or not re.fullmatch(r'[A-Za-z0-9.+_-]{1,64}', value['version']):
            raise ValueError('invalid build version')
        commit = value.get('commit')
        if commit is not None and (not isinstance(commit, str) or not re.fullmatch(r'[0-9a-fA-F]{7,64}', commit)):
            raise ValueError('invalid build commit')
        if not isinstance(value.get('stages'), dict):
            raise ValueError('invalid stages object')
        for field in COUNTERS:
            number(value.get(field), field, integer=True)
        for field in OPTIONAL_COUNTERS:
            if field in value:
                number(value[field], field, integer=True)
        if 'x11_capture' in value:
            validate_x11_capture(value['x11_capture'])
        if number(value.get('window_seconds'), 'window_seconds') <= 0:
            raise ValueError('window_seconds must be positive')
        for field in ('source_dimensions', 'encoded_dimensions'):
            dims = value.get(field)
            if not isinstance(dims, list) or len(dims) != 2:
                raise ValueError('invalid dimensions')
            for dim in dims:
                if number(dim, field, integer=True) > 65535:
                    raise ValueError('invalid dimensions')
        for stage in STAGES:
            stage_quantile_count(value['stages'].get(stage), stage)
        windows.append(value)
        if len(windows) > MAX_WINDOWS:
            raise ValueError('too many telemetry windows')
    if not windows:
        raise ValueError('no host telemetry records')
    return windows, ignored


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
        latency = number(float(row['latency_ms']), 'latency_ms')
        trials.setdefault(row['trial'], []).append(latency)
    if not trials:
        raise ValueError('empty physical measurements')
    result = {}
    for trial, values in sorted(trials.items()):
        values.sort()
        result[trial] = {'samples': len(values), 'mean_ms': sum(values) / len(values),
                         'p95_ms': values[math.ceil(len(values) * .95) - 1]}
    return {'source': 'operator_supplied_physical_measurements_not_host_ack',
            'trials': result,
            'minimum_sampling_complete': len(trials) >= 3 and all(len(v) >= 100 for v in trials.values()),
            'target_ms': 50,
            'all_trial_p95_within_target': all(t['p95_ms'] <= 50 for t in result.values()),
            'measurement_method_verified': False}


def summarize_x11_capture(windows):
    capture_windows = [window for window in windows if 'x11_capture' in window]
    if not capture_windows:
        return None
    captures = [window['x11_capture'] for window in capture_windows]
    stages = {}
    for stage in X11_CAPTURE_STAGES:
        observed = [capture['stages'][stage] for capture in captures
                    if capture['stages'][stage]['observations'] > 0]
        count = sum(item['observations'] for item in observed)
        means = [item['mean'] for item in observed]
        stages[stage] = {
            'observations': count,
            'whole_run_weighted_mean_ms': (sum(item['mean'] * (item['observations'] / count)
                                               for item in observed) if count else None),
            'mean_denominator': 'observations',
            'window_means_range_ms': [min(means), max(means)] if means else None,
            'max_reported_window_recent_p95_ms': max(
                (item['quantiles']['p95_ms'] for item in observed), default=None),
            'whole_run_percentiles': None,
        }
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
    for stage in STAGES:
        means = [w['stages'][stage]['mean'] for w in windows
                 if w['stages'][stage]['quantiles']['samples'] > 0]
        q95 = [w['stages'][stage]['quantiles']['p95_ms'] for w in windows
               if w['stages'][stage]['quantiles']['samples'] > 0]
        counter = COUNTS.get(stage)
        count = totals[counter] if counter else None
        stages[stage] = {
            'whole_run_weighted_mean_ms': (sum(w['stages'][stage]['mean'] * w[counter] for w in windows) / count
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
    return report


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--telemetry', type=Path, required=True)
    parser.add_argument('--workload', choices=('terminal', 'browser', 'isaac-sim'), required=True)
    parser.add_argument('--run-id', required=True, help='public non-personal label, 1-64 ASCII letters/digits/_/-')
    parser.add_argument('--physical-csv', type=Path)
    args = parser.parse_args(argv)
    try:
        if not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', args.run_id):
            raise ValueError('invalid run ID')
        windows, ignored = read_windows(args.telemetry)
        result = summarize(windows, ignored, args.workload, args.run_id)
        if args.physical_csv:
            result['physical_input_to_display'] = physical_samples(args.physical_csv)
        print(json.dumps(result, indent=2, sort_keys=True, allow_nan=False))
        return 0
    except (ValueError, OSError, UnicodeError, TypeError, KeyError, OverflowError, csv.Error) as exc:
        print('benchmark input rejected: ' + str(exc), file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
