#!/usr/bin/env python3
"""Supplementary fixed-denominator/freshness diagnostics; never replaces old gates."""
import argparse
import json
import math
from pathlib import Path


def integer(value, name):
    if type(value) is not int or not 0 <= value < 2**63:
        raise ValueError(f'invalid {name}')
    return value


def distribution(values):
    values = sorted(values)
    return {'count': len(values), 'p95_ms': values[math.ceil(.95 * len(values)) - 1] / 1e6 if values else None,
            'max_ms': max(values) / 1e6 if values else None}


def session_metrics(session, budget, period, window):
    markers = {}
    for row in session['markers']:
        generation = integer(row['generation'], 'generation')
        if generation >= budget or generation in markers:
            raise ValueError('duplicate or outside-budget marker')
        if integer(row['scheduled_ns'], 'scheduled') != generation * period:
            raise ValueError('changed source schedule')
        commit = row.get('commit_issued_ns')
        if commit is not None:
            commit = integer(commit, 'commit')
            if commit < generation * period:
                raise ValueError('commit before schedule')
        markers[generation] = commit
    baseline = session.get('baseline_sequence')
    events, sequences = [], set()
    for row in session['frames']:
        sequence = integer(row['sequence'], 'sequence')
        if sequence in sequences:
            raise ValueError('duplicate sequence')
        sequences.add(sequence)
        if baseline is not None and sequence <= baseline:
            continue
        at = row.get('presented_ns')
        if at is None:
            continue
        at = integer(at, 'presented')
        gl = integer(row['gl_started_ns'], 'gl start')
        if at < gl:
            raise ValueError('presentation before readback')
        generation = row.get('marker_generation')
        if generation is not None:
            generation = integer(generation, 'marker identity')
        commit = markers.get(generation)
        # Pixels read before this source commit cannot prove its identity.
        known = commit is not None and commit <= gl
        events.append((at, sequence, generation if known else None, commit if known else None))
    events.sort()
    first = {}
    for at, _, generation, _ in events:
        if generation is not None:
            first.setdefault(generation, at)
    on_time = sum(at <= window and at <= generation * period + period for generation, at in first.items())
    late = sum(at <= window and at > generation * period + period for generation, at in first.items())
    missing = budget - on_time - late
    cleanup_only = sum(at > window for at in first.values())
    active = [event for event in events if event[0] <= window]
    known_ns = 0
    # Twice the exact integral of display age over its known intervals.
    twice_age = 0
    previous_generation = None
    repeats = regressions = 0
    refresh = []
    for i, (at, _, generation, commit) in enumerate(active):
        until = active[i + 1][0] if i + 1 < len(active) else window
        if generation is not None:
            duration = until - at
            known_ns += duration
            twice_age += duration * (at + until - 2 * commit)
            if generation == previous_generation:
                repeats += 1
            elif previous_generation is not None and generation < previous_generation:
                regressions += 1
        previous_generation = generation
        if i:
            refresh.append(at - active[i - 1][0])
    return {'arm': session.get('arm'), 'order': session.get('order'),
            'session_success': session.get('success'), 'failure': session.get('failure'),
            'planned_generations': budget, 'on_time': on_time, 'late_within_window': late,
            'missing_within_window': missing, 'cleanup_only_generations': cleanup_only,
            'not_submitted': budget - sum(commit is not None for commit in markers.values()),
            'deadline_miss_fraction': (late + missing) / budget,
            'known_display_ns': known_ns, 'unknown_display_ns': window - known_ns,
            'known_display_fraction': known_ns / window,
            'known_display_mean_age_ms': twice_age / (2 * known_ns * 1e6) if known_ns else None,
            'age_integral_twice_ns2': twice_age,
            'repeat_presentations': repeats, 'backward_presentations': regressions,
            'unknown_active_presentations': sum(event[2] is None for event in active),
            'unknown_cleanup_presentations': sum(event[2] is None for event in events if event[0] > window),
            'refresh_intervals': distribution(refresh),
            'startup_wait_ns': active[0][0] if active else window,
            'tail_wait_ns': window - active[-1][0] if active else window,
            'active_presentations': len(active)}


def summarize(report):
    budget = integer(report['generation_budget'], 'budget')
    period = integer(report['scheduled_interval_ns'], 'period')
    window = integer(report['active_window_ns'], 'window')
    if not 0 < budget <= 1024 or not period or not window:
        raise ValueError('invalid bounded workload')
    return {'diagnostic_schema': 1, 'benchmark': report.get('benchmark'),
            'case': report.get('case'), 'debug_assertions': report.get('debug_assertions'),
            'note': 'Supplementary only. Fixed source denominator; age conditional on known displayed identity. No acceptance verdict.',
            'sessions': [session_metrics(s, budget, period, window) for s in report['sessions']]}


def self_test():
    import random
    def fixture(events, n=4):
        return {'generation_budget': n, 'scheduled_interval_ns': 10, 'active_window_ns': n * 10,
                'sessions': [{'baseline_sequence': 0, 'markers': [dict(generation=g, scheduled_ns=g*10, commit_issued_ns=g*10) for g in range(n)],
                              'frames': [dict(sequence=i+1, marker_generation=g, gl_started_ns=at, presented_ns=at) for i, (at, g) in enumerate(events)]}]}
    r = summarize(fixture([(5, 0), (15, 0), (25, 2), (45, 3)]))['sessions'][0]
    assert (r['on_time'], r['late_within_window'], r['missing_within_window'], r['cleanup_only_generations']) == (2, 0, 2, 1)
    assert r['repeat_presentations'] == 1 and r['known_display_ns'] == 35
    # Repeats never reset source age; unknown identities interrupt coverage.
    r = summarize(fixture([(5, 0), (15, None), (25, 2), (30, 1)]))['sessions'][0]
    assert r['known_display_ns'] == 25 and r['backward_presentations'] == 1
    bad = fixture([(25, 2)])
    bad['sessions'][0]['frames'][0]['gl_started_ns'] = 19
    assert summarize(bad)['sessions'][0]['known_display_ns'] == 0
    for seed in range(100):
        rng = random.Random(seed)
        events = sorted((at, rng.choice([None] + list(range(at // 10 + 1)))) for at in rng.sample(range(40), 15))
        r = summarize(fixture(events))['sessions'][0]
        integral_twice = known = 0
        for t in range(40):
            seen = [(at, g) for at, g in events if at <= t]
            g = seen[-1][1] if seen else None
            if g is not None:
                known += 1
                integral_twice += 2 * t + 1 - 2 * g * 10
        assert r['known_display_ns'] == known
        assert r['age_integral_twice_ns2'] == integral_twice
        assert r['on_time'] + r['late_within_window'] + r['missing_within_window'] == 4
    print('freshness self-tests and 100 independent integral checks passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('input', nargs='?')
    parser.add_argument('--output')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    if args.input:
        raw = Path(args.input).read_bytes()
        if len(raw) > 16 * 1024 * 1024:
            raise ValueError('report too large')
        result = json.dumps(summarize(json.loads(raw)), indent=2) + '\n'
        if args.output:
            Path(args.output).write_text(result)
        else:
            print(result, end='')


if __name__ == '__main__':
    main()
