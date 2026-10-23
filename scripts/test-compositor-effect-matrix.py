#!/usr/bin/env python3
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("matrix", Path(__file__).with_name("compositor-effect-matrix.py"))
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


class MatrixTests(unittest.TestCase):
    def test_matrix_has_unique_cases_and_every_workload(self):
        cases = matrix.cases()
        self.assertEqual(len(cases), 144)
        self.assertEqual(matrix.case_by_id("1080p-astronomy-idle-none")["settings"]["behavior.wallpaper_effect"], "astronomy")
        self.assertEqual(len({case["id"] for case in cases}), 144)
        for resolution in matrix.RESOLUTIONS:
            for effect in matrix.EFFECTS:
                self.assertEqual({c["workload"] for c in cases if c["id"].startswith(f"{resolution}-{effect}-")},
                                 {"idle", "animation", "tag-switch", "window-move"})

    def test_config_preparation_preserves_unrelated_tables_and_arrays(self):
        text = '[behavior]\nwallpaper_effect = "off"\nfrosted_glass_rules = [\n "old",\n]\n[layout]\ntags_length = 9\n[[layout.tags]]\ntag = 2\nlayout = "tile"\n'
        changed = matrix.update_toml(text, {"behavior.wallpaper_effect": "astronomy", "behavior.frosted_glass_rules": ["1:Xmessage"]})
        before, after = matrix.tomllib.loads(text), matrix.tomllib.loads(changed)
        self.assertEqual(before["layout"], after["layout"])
        self.assertEqual(after["behavior"]["frosted_glass_rules"], ["1:Xmessage"])

    def test_wayland_fallback_wrong_topology_and_live_override_are_rejected(self):
        case = matrix.case_by_id("1080p-astronomy-idle-none")
        config = {"behavior": {k.split(".")[1]: v for k, v in case["settings"].items()}}
        monitors = [{"x": 0, "y": 0, "w": 1920, "h": 1080}]
        with self.assertRaisesRegex(ValueError, "X11-only"):
            matrix.verify_case(case, config, {}, monitors, "wayland-udev")
        with self.assertRaisesRegex(ValueError, "live override"):
            matrix.verify_case(case, config, {"border_glow_enabled": True}, monitors, "x11rb")
        with self.assertRaisesRegex(ValueError, "extent"):
            matrix.verify_case(case, config, {}, [{**monitors[0], "w": 1280}], "x11rb")

    def test_same_raw_perf_label_does_not_make_different_workloads_comparable(self):
        left = {"status": "recorded", "case": {"id": "A"}, "effective_config_sha256": "x", "topology": [], "workload_command": None}
        for key, changed in [("case", {"id": "B"}), ("effective_config_sha256", "y"), ("status", "incomplete")]:
            right = copy.deepcopy(left)
            right[key] = changed
            with self.assertRaises(ValueError):
                matrix.require_same_case(left, right)


if __name__ == "__main__":
    unittest.main()
