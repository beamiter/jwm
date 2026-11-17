#!/usr/bin/env python3
"""Geometry/pixel oracle tests; these synthetic images are not GUI acceptance."""
import unittest
import ast
from pathlib import Path
from types import SimpleNamespace
from PIL import Image, ImageDraw
from sparse_header_pixels import selected_pixels, tag_geometry

RECT = (5, 5, 1270, 42)
BLUE = (10, 103, 227)
TINT = (215, 228, 240)


def fixture(widths=(32, 32, 32, 32), gap=8, selected=(2,), glyphs=True, background=(0, 0, 0)):
    image = Image.new('RGB', (1280, 800), background)
    draw = ImageDraw.Draw(image)
    # Independent vector fixture: no production geometry or sampler calls.
    draw.rounded_rectangle((17, 12, 48, 39), radius=14, fill=(60, 60, 60))
    x = 49 + gap
    boxes = []
    for slot, width in enumerate(widths):
        box = (x, 12, x + width - 1, 39)
        draw.rounded_rectangle(box, radius=14, fill=BLUE if slot in selected else TINT)
        if glyphs:
            cx = (box[0] + box[2]) // 2
            draw.line((cx, 22, cx, 30), fill=(70, 70, 70), width=2)
        boxes.append(box)
        x += width + gap
    return image, boxes


class Pixels(unittest.TestCase):
    def test_varied_font_width_and_density_spacing(self):
        for widths in [(32,) * 4, (39,) * 4, (34, 41, 37, 44), (48,) * 4]:
            for gap in [6, 8, 12]:
                with self.subTest(widths=widths, gap=gap):
                    image, boxes = fixture(widths, gap)
                    rows = selected_pixels(image, RECT, [2], 4)
                    for row, box in zip(rows, boxes):
                        self.assertLessEqual(abs(row['center'] - (box[0] + box[2]) / 2), 1)

    def test_three_pill_after_click(self):
        image, _ = fixture((39,) * 3, selected=(1,))
        selected_pixels(image, RECT, [1], 3)

    def test_multiselection(self):
        image, _ = fixture(selected=(1, 2))
        selected_pixels(image, RECT, [1, 2], 4)

    def test_wrong_selection_rejected(self):
        image, _ = fixture(selected=(3,))
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_stale_three_pills_rejected_even_with_blue_slot_two(self):
        image, _ = fixture((39,) * 3)
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_extra_pill_rejected(self):
        image, _ = fixture((39,) * 5)
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_missing_interior_pill_rejected(self):
        image, boxes = fixture()
        ImageDraw.Draw(image).rectangle(boxes[1], fill='black')
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_blank_pills_rejected(self):
        image, _ = fixture(glyphs=False)
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_merged_pills_rejected(self):
        image, boxes = fixture()
        ImageDraw.Draw(image).rectangle((boxes[0][2], 12, boxes[1][0], 39), fill=TINT)
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_no_launcher_rejected(self):
        image, _ = fixture()
        ImageDraw.Draw(image).rectangle((17, 12, 48, 39), fill='black')
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)

    def test_geometry_independent_of_selection(self):
        a, _ = fixture(selected=(0,))
        b, _ = fixture(selected=(1, 2, 3))
        self.assertEqual(tag_geometry(a, RECT), tag_geometry(b, RECT))

    def test_background_tint(self):
        image, _ = fixture(background=(20, 22, 25))
        selected_pixels(image, RECT, [2], 4)

    def test_noisy_gutter_rejected(self):
        image, _ = fixture()
        image.putpixel((5, 18), (255, 255, 255))
        with self.assertRaises(AssertionError): selected_pixels(image, RECT, [2], 4)


class FrontendIsolation(unittest.TestCase):
    def test_non_xilem_fixed_sampler_keeps_existing_coordinates(self):
        source = Path(__file__).with_name('test-sparse-header.py').read_text()
        tree = ast.parse(source)
        fn = next(n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef)
                  and n.name == 'fixed_selected_pixels')
        namespace = {'rect': RECT, 'args': SimpleNamespace(first_tag_center=68, tag_step=40)}
        exec(compile(ast.Module(body=[fn], type_ignores=[]), 'fixed sampler', 'exec'), namespace)
        image, _ = fixture()
        self.assertEqual([r['slot'] for r in namespace['fixed_selected_pixels'](image, [2], 4)
                          if r['selected_blue']], [2])
        with self.assertRaises(AssertionError):
            namespace['fixed_selected_pixels'](image, [3], 4)

    def test_dynamic_branch_is_explicitly_xilem_only(self):
        tree = ast.parse(Path(__file__).with_name('test-sparse-header.py').read_text())
        sample = next(n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef)
                      and n.name == 'sample')
        branch = next(n for n in sample.body if isinstance(n, ast.If))
        self.assertEqual(ast.unparse(branch.test), "args.bar == 'xilem_bar'")
        self.assertIn('fixed_selected_pixels(image, selected, expected_count)', ast.unparse(branch.orelse[0]))
        self.assertTrue(any(isinstance(n, ast.Nonlocal) and 'current_pills' in n.names
                            for n in sample.body))


class FrameGeometry(unittest.TestCase):
    def setUp(self):
        self.tree = ast.parse(Path(__file__).with_name('test-sparse-header.py').read_text())
        functions = [n for n in ast.walk(self.tree) if isinstance(n, ast.FunctionDef)
                     and n.name in ('capture_with_geometry', 'fixed_selected_pixels')]
        self.namespace = {'rect': (0, 0, 1280, 42),
                          'args': SimpleNamespace(first_tag_center=68, tag_step=40)}
        exec(compile(ast.Module(body=functions, type_ignores=[]), 'actual geometry sampler', 'exec'),
             self.namespace)

    def test_mapped_window_moves_after_startup_before_capture(self):
        image, _ = fixture()
        sampler = self.namespace['fixed_selected_pixels']
        with self.assertRaises(AssertionError):
            sampler(image, [2], 4)  # Original cached startup rectangle is wrong.
        frame, rect = self.namespace['capture_with_geometry'](lambda: image, lambda: RECT)
        self.namespace['rect'] = rect
        rows = sampler(frame, [2], 4)
        self.assertEqual([r['slot'] for r in rows if r['selected_blue']], [2])
        # Correct geometry must not excuse a wrong selection or a missing pill.
        with self.assertRaises(AssertionError): sampler(frame, [3], 4)
        with self.assertRaises(AssertionError): sampler(frame, [2], 3)

    def test_geometry_change_during_capture_is_rejected_without_internal_retry(self):
        events = []
        rectangles = iter([(0, 0, 1280, 42), RECT])
        def geometry():
            events.append('geometry')
            return next(rectangles)
        def capture():
            events.append('capture')
            return fixture()[0]
        with self.assertRaisesRegex(AssertionError, 'changed during capture'):
            self.namespace['capture_with_geometry'](capture, geometry)
        self.assertEqual(events, ['geometry', 'capture', 'geometry'])

    def test_capture_updates_enclosing_rect_from_same_window(self):
        capture = next(n for n in ast.walk(self.tree) if isinstance(n, ast.FunctionDef)
                       and n.name == 'capture')
        self.assertTrue(any(isinstance(n, ast.Nonlocal) and 'rect' in n.names
                            for n in capture.body))
        source = ast.unparse(capture)
        self.assertIn('image, rect = capture_with_geometry(read_image, lambda: geometry(xid))', source)
        self.assertIn("result.setdefault('frame_rects', {})[name] = rect", source)

    def test_pointer_rechecks_validated_geometry_before_move_and_click(self):
        pointer = next(n for n in ast.walk(self.tree) if isinstance(n, ast.FunctionDef)
                       and n.name == 'pointer_click')
        guards = [n for n in pointer.body if isinstance(n, ast.Assert)]
        self.assertEqual([ast.unparse(n.test) for n in guards], ['geometry(xid) == rect'] * 2)
        statements = [ast.unparse(n) for n in pointer.body]
        move = statements.index("run('xdotool', 'mousemove', *point)")
        click = statements.index("run('xdotool', 'click', button)")
        self.assertTrue(statements[move - 1].startswith('assert geometry(xid) == rect'))
        self.assertTrue(statements[click - 1].startswith('assert geometry(xid) == rect'))


if __name__ == '__main__':
    unittest.main()
