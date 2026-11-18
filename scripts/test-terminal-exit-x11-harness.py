#!/usr/bin/env python3
"""Offline contracts for the owned terminal-exit fixture; launches no X server."""
import contextlib
import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch
from PIL import Image

SPEC = importlib.util.spec_from_file_location("exit_fixture", Path(__file__).with_name("test-terminal-exit-x11.py"))
FIXTURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURE)


class ExitHarnessTests(unittest.TestCase):
    @staticmethod
    def ximage_metadata(**changes):
        metadata = {"width": 2, "height": 2, "xoffset": 0, "format": 2,
                    "byte_order": 0, "bitmap_unit": 32, "bitmap_bit_order": 0,
                    "bitmap_pad": 32, "depth": 24, "bytes_per_line": 8,
                    "bits_per_pixel": 32, "red_mask": 0xff0000,
                    "green_mask": 0xff00, "blue_mask": 0xff}
        return metadata | changes

    def test_ximage_little_endian_bgrx_and_big_endian_xrgb(self):
        expected = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)]
        little = bytes([0, 0, 255, 77, 0, 255, 0, 77, 255, 0, 0, 77, 255, 255, 255, 77])
        big = bytes([77, 255, 0, 0, 77, 0, 255, 0, 77, 0, 0, 255, 77, 255, 255, 255])
        self.assertEqual(FIXTURE.decode_ximage(little, self.ximage_metadata()).tobytes(), bytes(channel for rgb in expected for channel in rgb))
        self.assertEqual(FIXTURE.decode_ximage(big, self.ximage_metadata(byte_order=1)).tobytes(), bytes(channel for rgb in expected for channel in rgb))

    def test_ximage_24bit_row_padding_and_alternate_masks(self):
        data = bytes([0, 0, 255, 0, 255, 0, 99, 99, 255, 0, 0, 255, 255, 255, 99, 99])
        image = FIXTURE.decode_ximage(data, self.ximage_metadata(bits_per_pixel=24))
        self.assertEqual(image.tobytes(), bytes([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]))
        image = FIXTURE.decode_ximage(bytes([255, 0, 0, 9, 0, 0, 255, 9]),
                                      self.ximage_metadata(height=1, red_mask=0xff, blue_mask=0xff0000))
        self.assertEqual(image.tobytes(), bytes([255, 0, 0, 0, 0, 255]))

    def test_ximage_rejects_unknown_formats_instead_of_guessing(self):
        for changes in ({"bits_per_pixel": 16}, {"byte_order": 2}, {"bitmap_bit_order": 3},
                        {"bytes_per_line": 7}, {"bytes_per_line": 100}, {"red_mask": 0xf800},
                        {"red_mask": 0xff}, {"xoffset": 1}, {"format": 1}, {"depth": 30},
                        {"bitmap_pad": 64}, {"width": 0}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                FIXTURE.decode_ximage(bytes(16), self.ximage_metadata(**changes))
        with self.assertRaisesRegex(ValueError, "buffer length"):
            FIXTURE.decode_ximage(bytes(15), self.ximage_metadata())

    def test_producer_resolution_separates_native_xid_from_managed_id(self):
        producer = {"pid": 123, "xid": 8388644}
        windows = [{"id": 8388644, "pid": 999}, {"id": 4, "pid": 123}]
        self.assertEqual(FIXTURE.producer_row(windows, producer)["id"], 4)
        producer["id"] = 4
        self.assertEqual(FIXTURE.producer_row(windows, producer)["pid"], 123)
        self.assertIsNone(FIXTURE.producer_row([], producer))
        with self.assertRaisesRegex(AssertionError, "changed its managed"):
            FIXTURE.producer_row([{"id": 5, "pid": 123}], producer)
        with self.assertRaisesRegex(AssertionError, "multiple managed"):
            FIXTURE.producer_row([{"id": 4, "pid": 123}, {"id": 5, "pid": 123}], producer)

    def test_color_masks_distinguish_foreground_background_and_or(self):
        image = Image.new("RGB", (30, 10), (0, 0, 0))
        image.paste((224, 32, 32), (0, 0, 10, 10))
        image.paste((32, 64, 224), (10, 0, 20, 10))
        image.paste((32, 208, 64), (20, 0, 30, 10))
        for kind in ("term", "background", "or"):
            self.assertEqual(FIXTURE.color_pixels(image, kind), 100)
        self.assertEqual(FIXTURE.color_pixels(image, "term", (10, 0, 30, 10)), 0)

    def test_projection_tracks_cubic_exit_and_excludes_natural_roi_departure(self):
        terminal = {"x": 200, "y": 100, "w": 600, "h": 400, "border_w": 2}
        fullscreen = {"x": 0, "y": 0, "w": 1280, "h": 800}
        early = FIXTURE.overlap_during_capture(terminal, fullscreen, .01, .05)
        late = FIXTURE.overlap_during_capture(terminal, fullscreen, .45, .5)
        self.assertIsNotNone(early)
        self.assertIsNotNone(late)
        self.assertLess(late[3], early[3])
        self.assertIsNone(FIXTURE.overlap_during_capture(terminal, fullscreen, .9, 1.0))
        pip = {"x": 200, "y": 350, "w": 600, "h": 200}
        self.assertIsNotNone(FIXTURE.overlap_during_capture(terminal, pip, .01, .05))
        self.assertIsNone(FIXTURE.overlap_during_capture(terminal, pip, .45, .5))

    def test_measured_key_dispatch_interval_narrows_guaranteed_overlap(self):
        terminal = {"x": 200, "y": 100, "w": 600, "h": 400, "border_w": 2}
        background = {"x": 0, "y": 0, "w": 1280, "h": 800}
        instantaneous = FIXTURE.overlap_during_capture(terminal, background, .4, .45, 0)
        delayed = FIXTURE.overlap_during_capture(terminal, background, .4, .45, .2)
        self.assertGreaterEqual(delayed[1], instantaneous[1])
        self.assertEqual(delayed[3], instantaneous[3])
        # A very long key dispatch cannot fabricate late-frame coverage.
        self.assertIsNone(FIXTURE.overlap_during_capture(terminal, background, .8, .85, .7))

    def test_nonoverlapping_fixture_does_not_claim_sample(self):
        terminal = {"x": 20, "y": 50, "w": 300, "h": 400, "border_w": 2}
        background = {"x": 900, "y": 0, "w": 300, "h": 800}
        self.assertIsNone(FIXTURE.overlap_during_capture(terminal, background, 0, .1))

    def test_coverage_requires_early_middle_final_visible_and_completion(self):
        def samples(times):
            return [{"seconds": time, "checked": time < .75} for time in times]
        FIXTURE.assert_full_coverage(samples([.1, .4, .65, .8, 1.1]), .7, "full")
        for values in ([.1, .2, .3], [.1, .2, .3, 1.1], [.4, .5, .65, 1.1], [.1, .15, .65, 1.1]):
            with self.subTest(values=values), self.assertRaises(AssertionError):
                FIXTURE.assert_full_coverage(samples(values), .7, "incomplete")

    def test_overlap_horizon_adjusts_to_actual_pip_geometry(self):
        terminal = {"x": 200, "y": 100, "w": 600, "h": 400, "border_w": 2}
        full = {"x": 0, "y": 0, "w": 1280, "h": 800}
        pip = {"x": 200, "y": 350, "w": 600, "h": 200}
        self.assertLess(FIXTURE.overlap_horizon(terminal, pip), FIXTURE.overlap_horizon(terminal, full))

    def test_missing_display_tool_fails_before_any_process(self):
        with (patch.object(FIXTURE.shutil, "which", side_effect=lambda tool: None if tool == "Xephyr" else "/bin/tool"),
              patch.object(FIXTURE.C.util, "find_library", return_value="libX11.so.6"),
              patch.object(FIXTURE.sys, "argv", ["test-terminal-exit-x11.py", "--preflight"]),
              patch.object(FIXTURE.subprocess, "Popen") as spawn,
              contextlib.redirect_stderr(io.StringIO())):
            with self.assertRaises(SystemExit):
                FIXTURE.main()
            spawn.assert_not_called()

    def test_preflight_launches_nothing(self):
        with (patch.object(FIXTURE.shutil, "which", return_value="/bin/tool"),
              patch.object(FIXTURE.C.util, "find_library", return_value="libX11.so.6"),
              patch.object(FIXTURE.sys, "argv", ["test-terminal-exit-x11.py", "--preflight", "--jwm", "/bin/true", "--output", "/tmp/unused-exit-preflight"]),
              patch.object(FIXTURE.subprocess, "Popen") as spawn,
              patch.object(FIXTURE, "run_case") as run,
              contextlib.redirect_stdout(io.StringIO())):
            FIXTURE.main()
            spawn.assert_not_called()
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
