#!/usr/bin/env python3
"""Static fail-closed contracts for the independent Leptos acceptance steps."""
import itertools
from pathlib import Path
import re
import unittest

SOURCE = (Path(__file__).resolve().parents[1] / '.github/workflows/shared-shell.yml').read_text()
BLOCKS = re.split(r'(?=^      - name: )', SOURCE, flags=re.M)
STEPS = {match[1]: block for block in BLOCKS
         if (match := re.search(r'^        id: (\w+)$', block, re.M))}


def condition(step):
    return re.search(r'^        if: \$\{\{ (.+) \}\}$', STEPS[step], re.M)[1]


def evaluate(expression, cancelled, bar, outcomes):
    """Evaluate only the deliberately narrow predicates used by these steps."""
    clauses = expression.split(' && ')
    values = []
    for clause in clauses:
        if clause == '!cancelled()':
            values.append(not cancelled)
        elif clause == "matrix.bar == 'tauri_leptos_bar'":
            values.append(bar == 'tauri_leptos_bar')
        else:
            match = re.fullmatch(r"steps\.(\w+)\.outcome == 'success'", clause)
            if not match:
                raise AssertionError(f'unreviewed condition: {clause}')
            values.append(outcomes.get(match[1]) == 'success')
    return all(values)


class LeptosWorkflowTests(unittest.TestCase):
    def test_build_requires_real_prerequisites_not_prior_runtime_success(self):
        self.assertIn('Build JWM and the independent host', STEPS['native_build'])
        self.assertIn('Install web frontend tools', STEPS['web_tools'])
        self.assertEqual(condition('leptos_production_build'),
                         "!cancelled() && matrix.bar == 'tauri_leptos_bar' && steps.native_build.outcome == 'success' && steps.web_tools.outcome == 'success'")
        for native, tools in itertools.product(['success', 'failure', 'skipped'], repeat=2):
            outcomes = {'native_build': native, 'web_tools': tools, 'earlier_runtime': 'failure'}
            self.assertEqual(evaluate(condition('leptos_production_build'), False, 'tauri_leptos_bar', outcomes),
                             native == tools == 'success')
            self.assertFalse(evaluate(condition('leptos_production_build'), True, 'tauri_leptos_bar', outcomes))
            self.assertFalse(evaluate(condition('leptos_production_build'), False, 'tauri_yew_bar', outcomes))
        self.assertIn('bash scripts/build-leptos-bar.sh "$PWD/target/default-release"', STEPS['leptos_production_build'])

    def test_each_acceptance_runs_after_build_even_if_other_acceptance_failed(self):
        for step in ['leptos_production_xcb', 'leptos_production_x11rb', 'leptos_daemon']:
            self.assertEqual(condition(step),
                             "!cancelled() && matrix.bar == 'tauri_leptos_bar' && steps.leptos_production_build.outcome == 'success'")
            for build in ['success', 'failure', 'skipped']:
                outcomes = {'leptos_production_build': build, 'leptos_production_xcb': 'failure',
                            'leptos_production_x11rb': 'failure', 'earlier_runtime': 'failure'}
                self.assertEqual(evaluate(condition(step), False, 'tauri_leptos_bar', outcomes), build == 'success')
                self.assertFalse(evaluate(condition(step), True, 'tauri_leptos_bar', outcomes))
                self.assertFalse(evaluate(condition(step), False, 'tauri_react_bar', outcomes))

    def test_original_acceptance_commands_and_failure_exit_semantics_remain(self):
        for step, command in [('leptos_production_xcb', 'test-shared-sidebar-shell-x11.py'),
                              ('leptos_production_x11rb', 'test-shared-sidebar-shell-x11.py'),
                              ('leptos_daemon', 'test-shared-sidebar-shell-daemon.py')]:
            block = STEPS[step]
            self.assertEqual(block.count('python3 scripts/' + command), 1)
            self.assertIn('--bar-bin "$PWD/target/default-release/release/tauri_leptos_bar"', block)
            self.assertIn('--bin-dir target/release', block)
            self.assertNotIn('continue-on-error', block)
            self.assertNotIn('||', block)
            self.assertNotIn('set +e', block)
        self.assertIn('--backend xcb', STEPS['leptos_production_xcb'])
        self.assertIn('--backend x11rb', STEPS['leptos_production_x11rb'])
        self.assertIn('--shell-bin crates/jwm_shell/target/debug/jwm-shell', STEPS['leptos_daemon'])
        self.assertNotIn('continue-on-error:', SOURCE)
        self.assertIn('python3 scripts/validate-shared-shell-matrix.py', SOURCE)
        self.assertIn('python3 scripts/test-sparse-header.py', SOURCE)


if __name__ == '__main__':
    unittest.main()
