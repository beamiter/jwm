#!/usr/bin/env python3
"""Offline checks for the deep acceptance fixture's captured-client oracle."""
import importlib.util
from pathlib import Path
import unittest
from PIL import Image

spec = importlib.util.spec_from_file_location(
    "shared_shell", Path(__file__).with_name("test-shared-sidebar-shell-x11.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class PreviewOracleTests(unittest.TestCase):
    def test_unchanged_marker_does_not_count(self):
        marker = Image.new("RGB", (100, 100), (224, 48, 64))
        self.assertEqual(fixture.changed_preview_marker_pixels(marker, marker, (224, 48, 64)), 0)

    def test_unrelated_hover_or_wrong_client_damage_does_not_count(self):
        before = Image.new("RGB", (100, 100), "white")
        for color in [(0, 0, 0), (32, 192, 96), (160, 160, 160)]:
            after = Image.new("RGB", before.size, color)
            self.assertEqual(fixture.changed_preview_marker_pixels(before, after, (224, 48, 64)), 0)

    def test_only_new_matching_client_pixels_count(self):
        before = Image.new("RGB", (100, 100), "white")
        after = before.copy()
        after.paste((224, 48, 64), (0, 0, 50, 100))
        count = fixture.changed_preview_marker_pixels(before, after, (224, 48, 64))
        self.assertEqual(count, 5000)
        self.assertFalse(count > 5000)
        after.putpixel((50, 0), (224, 48, 64))
        self.assertEqual(fixture.changed_preview_marker_pixels(before, after, (224, 48, 64)), 5001)


if __name__ == "__main__":
    unittest.main()
