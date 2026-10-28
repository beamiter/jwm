#!/usr/bin/env python3
"""Check reproducible catalogue generation without downloading or rendering."""
import importlib.util
import json
import math
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "prepare_nebulae", ROOT / "scripts/prepare-astronomy-nebulae.py"
)
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class CatalogueGeneration(unittest.TestCase):
    def setUp(self):
        self.targets = json.loads((prepare.ASSETS / "deep-sky-catalog.json").read_text())
        self.expected = (
            prepare.RUST.read_text(),
            (prepare.SHADERS / "deep_sky_catalog.glsl").read_text(),
        )

    def test_committed_cpu_and_gpu_catalogues_match(self):
        self.assertEqual(prepare.generated(self.targets), self.expected)

    def test_float_sum_algorithm_does_not_change_catalogue_bytes(self):
        # Model the interpreter's change in float summation without requiring
        # two installed Python versions or changing the process's builtins.
        prepare.sum = math.fsum
        try:
            self.assertEqual(prepare.generated(self.targets), self.expected)
        finally:
            del prepare.sum


if __name__ == "__main__":
    unittest.main()
