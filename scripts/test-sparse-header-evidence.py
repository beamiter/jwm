#!/usr/bin/env python3
"""Run sparse fixture diagnostics offline, including its exact exception path."""
import ast
from pathlib import Path
import re
import tempfile
import unittest

TREE = ast.parse(Path(__file__).with_name('test-sparse-header.py').read_text())
HANDLER = next(node for node in ast.walk(TREE) if isinstance(node, ast.ExceptHandler)
               and 'failure_state' in ast.unparse(node))
RENDERED = next(node for node in ast.walk(TREE) if isinstance(node, ast.FunctionDef)
                and node.name == 'rendered')


class SparseEvidenceTests(unittest.TestCase):
    def check_capture(self, outcome):
        original = AssertionError('original frontend readiness failure')
        result, calls = {}, []
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)

            def run(*args):
                calls.append(args)
                self.assertEqual(args[:3], ('import', '-window', 'root'))
                self.assertEqual(args[3], evidence / 'failure-display.png')
                if outcome == 'error':
                    raise RuntimeError('capture unavailable')
                if outcome == 'success':
                    args[3].write_bytes(b'fake screenshot')

            module = ast.Module(body=[ast.Try(
                body=[ast.Raise(exc=ast.Name(id='original', ctx=ast.Load()))],
                handlers=[HANDLER], orelse=[], finalbody=[])], type_ignores=[])
            scope = dict(original=original, result=result, run=run, evidence=evidence, env={} if outcome == 'uninitialized' else {'DISPLAY': ':private-test'},
                         state=lambda: {'session': 123})
            with self.assertRaises(AssertionError) as caught:
                exec(compile(ast.fix_missing_locations(module), '<sparse-handler>', 'exec'), scope)
            self.assertIs(caught.exception, original)
            self.assertEqual(len(calls), 0 if outcome == 'uninitialized' else 1)
            self.assertEqual(result['failure_state'], {'session': 123})
            self.assertEqual('failure_screenshot' in result, outcome == 'success')
            self.assertEqual('failure_screenshot_error' in result, outcome != 'success')

    def test_successful_capture_preserves_original_error(self):
        self.check_capture('success')

    def test_capture_error_preserves_original_error(self):
        self.check_capture('error')

    def test_missing_capture_is_not_reported_as_saved(self):
        self.check_capture('missing')

    def test_uninitialized_owned_display_never_attempts_capture(self):
        self.check_capture('uninitialized')

    def test_all_phases_are_collected_but_only_real_render_ack_passes(self):
        phases = ['BOOT', 'ERROR', 'DOM_READY', 'RAF_FIRST', 'RAF_SECOND', 'RENDER_REPORT_REJECTED']
        for rendered in [False, True]:
            result, calls = {}, []
            raw = '_JWM_FRONTEND_READY(CARDINAL) = 1\n'
            raw += '\n'.join('_JWM_FRONTEND_' + phase + '(UTF8_STRING) = "phase"' for phase in phases)
            if rendered:
                raw += '\n_JWM_FRONTEND_RENDERED(CARDINAL) = 1\n'

            def run(*args):
                calls.append(args)
                return raw

            scope = dict(run=run, result=result, xid='123', re=re)
            exec(compile(ast.fix_missing_locations(ast.Module(body=[RENDERED], type_ignores=[])),
                         '<sparse-rendered>', 'exec'), scope)
            self.assertEqual(scope['rendered'](), rendered)
            self.assertEqual(result['frontend_diagnostics'], raw)
            self.assertEqual(calls[0][:3], ('xprop', '-id', '123'))
            for phase in phases + ['READY', 'RENDERED']:
                self.assertIn('_JWM_FRONTEND_' + phase, calls[0])


if __name__ == '__main__':
    unittest.main()
