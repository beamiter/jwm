#!/usr/bin/env python3
"""Bounded OFF/ON/ON/OFF diagnosis; no retries or relaxed acceptance."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import unittest

SEQUENCE = (False, True, True, False)


def snapshot():
    result = {'monotonic_ns': time.monotonic_ns(), 'uname': list(os.uname()),
              'runner': {key: os.environ.get(key) for key in
                         ('RUNNER_NAME', 'RUNNER_OS', 'RUNNER_ARCH', 'ImageOS', 'ImageVersion', 'GITHUB_SHA')}}
    for name in ('/proc/pressure/cpu', '/proc/loadavg', '/proc/stat'):
        try:
            result[name] = Path(name).read_text()
        except OSError as error:
            result[name] = str(error)
    return result


def owned_descendants(parent):
    """Snapshot PID/starttime identities before asking the fixture to clean up."""
    rows = {}
    for path in Path('/proc').glob('[0-9]*/stat'):
        try:
            fields = path.read_text().rsplit(')', 1)[1].split()
            rows[int(path.parent.name)] = (int(fields[1]), fields[19])
        except (OSError, ValueError, IndexError):
            continue
    owned = {parent}
    while True:
        new = {pid for pid, (ppid, _) in rows.items() if ppid in owned}
        if new <= owned:
            break
        owned |= new
    return {pid: rows[pid][1] for pid in owned - {parent} if pid in rows}


def surviving(identities):
    result = []
    for pid, started in identities.items():
        try:
            fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
            if fields[19] == started and fields[0] != 'Z':
                result.append(pid)
        except (OSError, IndexError):
            continue
    return result


def classify(code, report, timed_out, trace_enabled=False):
    if timed_out:
        return 'timeout'
    if report is None:
        return 'startup-or-harness-failure'
    if report.get('traceback'):
        return 'startup-or-harness-failure'
    if report.get('failures'):
        return 'behavior-or-diagnostic-failure'
    if {case.get('case') for case in report.get('cases', [])} != {'floating', 'pip', 'fullscreen'}:
        return 'harness-failure'
    if trace_enabled and report.get('clock_trace_status') != 'valid':
        return 'diagnostic-incomplete'
    return 'pass' if code == 0 and report.get('status') == 'success' else 'harness-failure'


def aggregate(rows):
    return len(rows) == len(SEQUENCE) and all(row['outcome'] == 'pass' for row in rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--output', type=Path, default=Path('artifacts/native-wayland-terminal'))
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Contracts)).wasSuccessful() else 1
    args.output.mkdir(parents=True, exist_ok=True)
    rows = []
    for index, enabled in enumerate(SEQUENCE):
        directory = args.output / f'{index + 1:02}-{"on" if enabled else "off"}'
        directory.mkdir()
        before = snapshot()
        (directory / 'conditions-before.json').write_text(json.dumps(before, indent=2))
        command = [sys.executable, 'scripts/test-terminal-exit-wayland.py', '--backend', 'wayland-winit',
                   '--bin-dir', 'target/release', '--client-bin', 'crates/jwm_shell/target/debug/examples/native_shell_client',
                   '--evidence-dir', str(directory), '--diagnose']
        if enabled:
            command.append('--trace-clock')
        timed_out = False
        cleanup_incomplete = False
        survivors = []
        env = os.environ.copy()
        env.pop('JWM_TERMINAL_EXIT_TRACE', None)
        with (directory / 'driver.log').open('wb') as log:
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
            try:
                code = process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                timed_out = True
                # The fixture owns children in separate sessions. SIGINT lets
                # its finally block retire those groups before the next trial.
                owned = owned_descendants(process.pid)
                process.send_signal(signal.SIGINT)
                try:
                    code = process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    cleanup_incomplete = True
                    os.killpg(process.pid, signal.SIGKILL)
                    code = process.wait()
                survivors = surviving(owned)
                cleanup_incomplete = cleanup_incomplete or bool(survivors)
        after = snapshot()
        (directory / 'conditions-after.json').write_text(json.dumps(after, indent=2))
        try:
            report = json.loads((directory / 'report.json').read_text())
        except (OSError, ValueError):
            report = None
        rows.append({'round': index + 1, 'trace': enabled, 'returncode': code,
                     'cleanup_incomplete': cleanup_incomplete, 'surviving_owned_pids': survivors,
                     'seconds': (after['monotonic_ns'] - before['monotonic_ns']) / 1e9,
                     'outcome': classify(code, report, timed_out, enabled)})
        (args.output / 'control-summary.json').write_text(json.dumps({'sequence': ['off', 'on', 'on', 'off'],
            'rounds': rows, 'success': aggregate(rows), 'limits': 'Four rounds are not statistical proof; submit return is not presentation.'}, indent=2))
        if timed_out or cleanup_incomplete:
            # Do not run later trials in a possibly contaminated runner.
            break
    return 0 if aggregate(rows) else 1


class Contracts(unittest.TestCase):
    def test_order_and_any_failure(self):
        self.assertEqual(SEQUENCE, (False, True, True, False))
        rows = [{'outcome': 'pass'} for _ in SEQUENCE]
        self.assertTrue(aggregate(rows))
        for index in range(4):
            modified = [dict(row) for row in rows]
            modified[index]['outcome'] = 'behavior-or-diagnostic-failure'
            self.assertFalse(aggregate(modified))
        self.assertFalse(aggregate(rows[:3]))
    def test_failure_types(self):
        self.assertEqual(classify(1, None, False), 'startup-or-harness-failure')
        self.assertEqual(classify(-9, None, True), 'timeout')
        self.assertEqual(classify(1, {'failures': ['occluded']}, False), 'behavior-or-diagnostic-failure')
        complete = {'status': 'success', 'cases': [{'case': name} for name in ('floating', 'pip', 'fullscreen')]}
        self.assertEqual(classify(0, complete, False), 'pass')
        self.assertEqual(classify(0, complete, False, True), 'diagnostic-incomplete')
        complete['clock_trace_status'] = 'valid'
        self.assertEqual(classify(0, complete, False, True), 'pass')
        self.assertNotEqual(classify(1, {'status': 'success'}, False), 'pass')


if __name__ == '__main__':
    raise SystemExit(main())
