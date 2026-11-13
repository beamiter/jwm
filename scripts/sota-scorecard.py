#!/usr/bin/env python3
"""Offline, fail-closed summaries of explicitly labeled event-latency evidence."""
import argparse
import hashlib
import json
import math
import os
import stat
from pathlib import Path

MAX_BYTES = 16 * 1024 * 1024
CONDITIONS = ('host_hardware', 'client_hardware', 'drivers', 'kernel', 'outputs',
              'network', 'workload', 'measurement_method', 'clock_domain',
              'timestamp_uncertainty_us', 'deadline_us')
KINDS = ('physical_input_to_photon', 'software_input_to_present')


def require(ok, message):
    if not ok:
        raise ValueError(message)


def integer(value, name, minimum=0):
    require(type(value) is int and value >= minimum, 'invalid ' + name)
    return value


def label(value, name):
    require(isinstance(value, str) and 0 < len(value) <= 512 and value.strip() == value
            and value.lower() not in ('unknown', 'unspecified'), 'invalid ' + name)
    return value


def digest(value, name):
    require(isinstance(value, str) and len(value) == 64
            and all(c in '0123456789abcdef' for c in value), 'invalid ' + name)


def percentile(values, p):
    return sorted(values)[max(0, math.ceil(len(values) * p) - 1)] if values else None


def summarize(document):
    require(type(document.get('schema_version')) is int and document['schema_version'] == 1,
            'unsupported schema')
    label(document.get('product'), 'product')
    label(document.get('revision'), 'revision')
    digest(document.get('binary_sha256'), 'binary_sha256')
    digest(document.get('config_sha256'), 'config_sha256')
    conditions = document.get('conditions')
    require(isinstance(conditions, dict) and set(conditions) == set(CONDITIONS),
            'complete exact condition fields required')
    for key in CONDITIONS[:-2]:
        label(conditions[key], key)
    require(conditions['measurement_method'] in KINDS, 'unsupported measurement method')
    integer(conditions['timestamp_uncertainty_us'], 'uncertainty')
    deadline = integer(conditions['deadline_us'], 'deadline', 1)
    runs = document.get('runs')
    require(isinstance(runs, list) and 3 <= len(runs) <= 100, 'need 3..100 repetitions')
    output, ids = [], set()
    for run in runs:
        key = label(run.get('repetition'), 'repetition')
        require(key not in ids, 'duplicate repetition')
        ids.add(key)
        digest(run.get('evidence_sha256'), 'evidence_sha256')
        start = integer(run.get('window_start_us'), 'window start')
        end = integer(run.get('window_end_us'), 'window end')
        require(end > start, 'empty window')
        samples = run.get('events')
        require(isinstance(samples, list) and 1 <= len(samples) <= 100000,
                'need 1..100000 planned events')
        latencies, seen, inputs = [], set(), []
        missing = late = 0
        for event in samples:
            eid = label(event.get('id'), 'event id')
            require(eid not in seen, 'duplicate event id')
            seen.add(eid)
            t = integer(event.get('input_us'), 'input')
            require(start <= t < end and t + deadline <= end,
                    'event lacks full deadline observation window')
            inputs.append((eid, t - start))
            require('present_us' in event, 'present_us must be explicit, null for missing')
            presented = event['present_us']
            if presented is None:
                missing += 1
                continue
            integer(presented, 'present')
            require(t <= presented <= end, 'presentation outside observation window')
            latency = presented - t
            latencies.append(latency)
            late += latency > deadline
        output.append({'repetition': key, 'planned': len(samples), 'delivered': len(latencies),
                       'missing': missing, 'late_delivered': late,
                       'deadline_missed': missing + late,
                       'deadline_missed_fraction': (missing + late) / len(samples),
                       'latency_p50_us': percentile(latencies, .50),
                       'latency_p95_us': percentile(latencies, .95),
                       'latency_p99_us': percentile(latencies, .99),
                       'latency_max_us': max(latencies) if latencies else None,
                       'observation_us': end - start,
                       'schedule_sha256': hashlib.sha256(json.dumps(sorted(inputs),
                           separators=(',', ':')).encode()).hexdigest(),
                       'evidence_sha256': run['evidence_sha256']})
    return {'schema_version': 1, 'product': document['product'], 'revision': document['revision'],
            'binary_sha256': document['binary_sha256'], 'config_sha256': document['config_sha256'],
            'conditions': conditions, 'runs': sorted(output, key=lambda r: r['repetition']),
            'verdict': 'descriptive_only_not_sota_or_non_regression_proof'}


def compare(baseline, candidate):
    a, b = summarize(baseline), summarize(candidate)
    require(a['conditions'] == b['conditions'], 'measurement conditions differ')
    require([r['repetition'] for r in a['runs']] == [r['repetition'] for r in b['runs']],
            'repetitions differ')
    pairs = []
    for left, right in zip(a['runs'], b['runs']):
        require(left['schedule_sha256'] == right['schedule_sha256'] and
                left['observation_us'] == right['observation_us'], 'planned event schedules differ')
        delta = {}
        for metric in ('deadline_missed_fraction', 'latency_p50_us', 'latency_p95_us',
                       'latency_p99_us', 'latency_max_us'):
            delta[metric] = (right[metric] - left[metric]
                             if left[metric] is not None and right[metric] is not None else None)
        pairs.append({'repetition': left['repetition'], 'candidate_minus_baseline': delta})
    return {'baseline': a, 'candidate': b, 'paired_deltas': pairs,
            'verdict': 'descriptive_only_not_sota_or_non_regression_proof'}


def read(path):
    fd = os.open(Path(path), os.O_RDONLY | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as handle:
        require(stat.S_ISREG(os.fstat(handle.fileno()).st_mode), 'input must be a regular file')
        raw = handle.read(MAX_BYTES + 1)
    require(len(raw) <= MAX_BYTES, 'input exceeds 16 MiB')
    def unique(items):
        result = {}
        for key, value in items:
            require(key not in result, 'duplicate JSON key')
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError('nonfinite JSON')))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('baseline')
    parser.add_argument('candidate', nargs='?')
    args = parser.parse_args()
    try:
        baseline = read(args.baseline)
        result = compare(baseline, read(args.candidate)) if args.candidate else summarize(baseline)
        print(json.dumps(result, indent=2, allow_nan=False))
    except (ValueError, TypeError, KeyError, AttributeError, OSError, RecursionError) as exc:
        parser.exit(2, 'invalid evidence: ' + str(exc) + '\n')


if __name__ == '__main__':
    main()
