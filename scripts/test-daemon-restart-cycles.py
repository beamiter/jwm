#!/usr/bin/env python3
"""Offline orchestration contracts; these are not native acceptance results."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('daemon_fixture', ROOT / 'scripts/test-shared-sidebar-shell-daemon.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CycleTests(unittest.TestCase):
    def setUp(self):
        self.apps = [{'name': f'app{i}', 'window': -1} for i in range(5)]
        self.rows = [{'name': app['name'], 'id': i + 100, 'is_minimized': False}
                     for i, app in enumerate(self.apps)]
        self.focused = None
        self.commands = []

    def msg(self, command, args=None):
        self.commands.append((command, args))
        if command == 'get_windows':
            return [dict(row) for row in self.rows]
        if command == 'focus_window':
            self.focused = next(row for row in self.rows if row['id'] == args['id'])
            self.focused['is_minimized'] = False
        elif command == 'minimize':
            self.focused['is_minimized'] = True
        else:
            raise AssertionError(command)

    def wait(self, condition, label):
        self.assertTrue(condition(), label)

    def test_default_cycle_keeps_original_last_two_minimized(self):
        self.assertEqual(fixture.prepare_restart_cycle(self.apps, 1, self.msg, self.wait), ['app3', 'app4'])
        self.assertEqual(sum(c == 'focus_window' for c, _ in self.commands), 2)

    def test_three_cycles_refresh_ids_restore_and_rotate(self):
        pairs = []
        for cycle in range(1, 4):
            if cycle > 1:
                for row in self.rows:
                    row['id'] += 100
            pairs.append(fixture.prepare_restart_cycle(self.apps, cycle, self.msg, self.wait))
            self.assertEqual([app['window'] for app in self.apps], [row['id'] for row in self.rows])
            self.assertEqual(sum(row['is_minimized'] for row in self.rows), 2)
        self.assertEqual(pairs, [['app3', 'app4'], ['app0', 'app1'], ['app2', 'app3']])

    def test_ten_cycles_remain_bounded_with_no_extra_apps(self):
        for cycle in range(1, 11):
            fixture.prepare_restart_cycle(self.apps, cycle, self.msg, self.wait)
        self.assertEqual(sum(c == 'minimize' for c, _ in self.commands), 20)
        self.assertEqual(len(self.apps), 5)

    def test_missing_application_is_a_failure_not_a_skip(self):
        self.rows.pop()
        with self.assertRaises(KeyError):
            fixture.prepare_restart_cycle(self.apps, 1, self.msg, self.wait)

    def test_failed_minimize_is_a_failure_not_a_retry(self):
        def msg(command, args=None):
            if command != 'minimize':
                return self.msg(command, args)
        with self.assertRaises(AssertionError):
            fixture.prepare_restart_cycle(self.apps, 1, msg, self.wait)

    def test_failed_restore_is_a_failure(self):
        fixture.prepare_restart_cycle(self.apps, 1, self.msg, self.wait)
        def msg(command, args=None):
            if command == 'focus_window':
                return None
            return self.msg(command, args)
        with self.assertRaises(AssertionError):
            fixture.prepare_restart_cycle(self.apps, 2, msg, self.wait)

    def test_cycle_bounds_rejected_before_binary_or_display_access(self):
        for value in ['0', '-1', '11', 'abc']:
            with self.subTest(value=value):
                process = subprocess.run([sys.executable, '-B', str(ROOT / 'scripts/test-shared-sidebar-shell-daemon.py'), '--cycles', value], capture_output=True, text=True)
                self.assertEqual(process.returncode, 2)
                self.assertIn('invalid', process.stderr)
                self.assertNotIn('FileNotFoundError', process.stderr)


if __name__ == '__main__':
    unittest.main()
