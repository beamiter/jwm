"""Read-only process metrics used by shared-shell acceptance fixtures."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time


def process_sample(pid):
    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    status = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
    return {'start_time_ticks': int(stat[19]), 'cpu_ticks': int(stat[11]) + int(stat[12]),
            'rss_bytes': int(status['VmRSS'].split()[0]) * 1024,
            'peak_rss_bytes': int(status['VmHWM'].split()[0]) * 1024,
            'voluntary_context_switches': int(status['voluntary_ctxt_switches'])}


def wakeups(pid, duration=1.2):
    """Count kernel wake events *targeting* every current thread of this process.

    Per-process perf attachment would instead count the process as the waking
    caller, so use a system-wide tracepoint with a wakee-pid filter. No kernel
    configuration, scheduling settings or process state is changed.
    """
    if not shutil.which('perf'):
        return {'available': False, 'reason': 'perf is unavailable'}
    tids = sorted(int(p.name) for p in Path(f'/proc/{pid}/task').iterdir())
    event_filter = ' || '.join(f'pid == {tid}' for tid in tids)
    with tempfile.TemporaryDirectory(prefix='jwm-shell-perf-') as directory:
        output = Path(directory) / 'perf.csv'
        command = ['perf', 'stat', '-a', '-e', 'sched:sched_wakeup', '--filter', event_filter,
                   '--timeout', str(round(duration * 1000)), '-x,', '-o', str(output)]
        if os.geteuid() != 0:
            command = ['sudo', '-n'] + command
        started = time.monotonic()
        result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, timeout=duration + 5)
        elapsed = time.monotonic() - started
        raw = output.read_text() if output.exists() else result.stderr
        if result.returncode:
            return {'available': False, 'reason': raw[-1000:].strip(), 'exit': result.returncode}
        line = next((line for line in raw.splitlines() if 'sched:sched_wakeup' in line), '')
        fields = line.split(',')
        if not fields or not fields[0].strip().isdigit():
            return {'available': False, 'reason': raw.strip()}
        count = int(fields[0].strip())
        return {'available': True, 'event': 'sched:sched_wakeup', 'wakee_thread_ids': tids,
                'count': count, 'sample_seconds': round(elapsed, 3),
                'wakeups_per_second': round(count / elapsed, 3)}


def measure(pid, duration=3):
    start = process_sample(pid)
    started = time.monotonic()
    traced = wakeups(pid, duration)
    remaining = duration - (time.monotonic() - started)
    if remaining > 0:
        time.sleep(remaining)
    elapsed = time.monotonic() - started
    end = process_sample(pid)
    if end['start_time_ticks'] != start['start_time_ticks']:
        raise RuntimeError('process identity changed during performance sampling')
    return {'pid': pid, 'sample_seconds': round(elapsed, 3),
            'cpu_percent_one_core': round((end['cpu_ticks'] - start['cpu_ticks']) / os.sysconf('SC_CLK_TCK') / elapsed * 100, 3),
            'rss_bytes': end['rss_bytes'], 'peak_rss_bytes': end['peak_rss_bytes'],
            'voluntary_context_switches': end['voluntary_context_switches'] - start['voluntary_context_switches'],
            'wakeups': traced}


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pid', type=int)
    parser.add_argument('--seconds', type=float, default=3)
    args = parser.parse_args()
    print(json.dumps(measure(args.pid, args.seconds), indent=2))
