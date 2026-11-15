#!/usr/bin/env python3
"""Exercise the production failure handler without starting a desktop."""
import ast
from pathlib import Path
import unittest


SOURCE = Path(__file__).with_name("test-shared-sidebar-shell-x11.py")
TREE = ast.parse(SOURCE.read_text())
HANDLER = next(node for node in ast.walk(TREE) if isinstance(node, ast.ExceptHandler)
               and 'failure_diagnostics' in ast.unparse(node))


class FailureEvidenceTests(unittest.TestCase):
    def check_capture(self, capture_fails):
        original = AssertionError("original readiness failure")
        calls = []
        result = {}

        def capture(name, final_display=False):
            calls.append((name, final_display))
            if capture_fails:
                raise RuntimeError("capture unavailable")

        # Run the exact production except body with a genuine active exception.
        module = ast.Module(body=[ast.Try(body=[ast.Raise(exc=ast.Name(id='original', ctx=ast.Load()))],
                                         handlers=[HANDLER], orelse=[], finalbody=[])], type_ignores=[])
        scope = dict(original=original, result=result, capture=capture,
                     state=lambda: {}, msg=lambda *args: {}, run=lambda *args: "")
        with self.assertRaises(AssertionError) as caught:
            exec(compile(ast.fix_missing_locations(module), str(SOURCE), 'exec'), scope)
        self.assertIs(caught.exception, original)
        self.assertEqual(calls, [("failure-desktop", True)])
        self.assertEqual(result['reason'], str(original))
        self.assertEqual('screenshot_error' in result['failure_diagnostics'], capture_fails)

    def test_capture_preserves_original_failure(self):
        self.check_capture(False)

    def test_capture_failure_preserves_original_failure(self):
        self.check_capture(True)


if __name__ == '__main__':
    unittest.main()
