#!/usr/bin/env python3
"""Pure diagnostics contract tests, not native player acceptance."""
import ast
from pathlib import Path
import unittest

source = Path(__file__).with_name("test-shared-sidebar-shell-x11.py").read_text()
tree = ast.parse(source)
function = next(node for node in tree.body
                if isinstance(node, ast.FunctionDef)
                and node.name == "player_reuse_observation")
namespace = {}
exec(compile(ast.Module(body=[function], type_ignores=[]), str(__file__), "exec"), namespace)
observe = namespace["player_reuse_observation"]


class ObservationTests(unittest.TestCase):
    def test_zero_is_distinct_from_duplicates(self):
        self.assertEqual(observe([])["window_count"], 0)

    def test_one_does_not_copy_title(self):
        self.assertEqual(observe([{"id": 19, "name": "private"}]),
                         {"window_count": 1, "window_ids": [19], "truncated": False})

    def test_duplicates_keep_both_ids(self):
        self.assertEqual(observe([{"id": 19}, {"id": 27}])["window_ids"], [19, 27])

    def test_output_is_bounded_but_count_is_exact(self):
        result = observe([{"id": i} for i in range(100)])
        self.assertEqual(result["window_count"], 100)
        self.assertEqual(result["window_ids"], list(range(8)))
        self.assertTrue(result["truncated"])

    def test_noninteger_identifier_is_not_copied(self):
        self.assertEqual(observe([{"id": "private"}])["window_ids"], [])


if __name__ == "__main__":
    unittest.main()
