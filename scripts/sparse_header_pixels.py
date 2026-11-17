"""Strict geometry for the controlled Xilem sparse-header acceptance fixture.

Geometry never consults selected/blue colors or the expected tag count. The
shared desktop header has a launcher followed by a contiguous numeric strip;
font metrics and inherited density can change that strip's physical widths.
This is not a general wallpaper/theme locator: ambiguous material fails closed,
and other frontends retain their existing sampler.
"""
from statistics import median


def distance(a, b):
    return max(abs(x - y) for x, y in zip(a, b))


def tag_geometry(image, rect):
    x, y, width, height = rect
    assert width >= 600 and 28 <= height <= 100, rect
    scan_y = round(y + height / 2 - 8)
    # The shared header's leading gutter is outside the launcher. Refuse a
    # noisy gutter instead of guessing a material or using selection color.
    gutter = [image.getpixel((x + dx, scan_y + dy)) for dx in range(3) for dy in range(3)]
    background = tuple(round(median(p[c] for p in gutter)) for c in range(3))
    assert all(distance(p, background) <= 3 for p in gutter), 'nonuniform header gutter'
    runs = []
    for px in range(x, x + min(round(width * .4), 512)):
        color = tuple(round(median(image.getpixel((px, scan_y + dy))[c] for dy in range(3))) for c in range(3))
        if distance(color, background) >= 4:
            if not runs or px != runs[-1][1] + 1:
                runs.append([px, px])
            else:
                runs[-1][1] = px
    assert runs, 'missing launcher and tags'
    launcher = runs[0]
    assert 4 <= launcher[0] - x <= 32 and 16 <= launcher[1] - launcher[0] + 1 <= 40, ('launcher bounds', launcher)
    pills = []
    previous = launcher
    for run in runs[1:]:
        gap = run[0] - previous[1] - 1
        # A distant central title/status group is not part of the tag strip.
        # Missing interior tags terminate the strip and fail exact count below.
        if gap > 24:
            break
        assert 2 <= gap <= 24, ('unseparated pills', gap)
        assert 20 <= run[1] - run[0] + 1 <= 96, ('invalid pill width', run)
        pills.append({'center': (run[0] + run[1]) / 2, 'left': run[0], 'right': run[1]})
        previous = run
    assert pills, 'missing numeric strip'
    return pills


def selected_pixels(image, rect, selected, expected_count):
    pills = tag_geometry(image, rect)
    assert len(pills) == expected_count, ('pill count', pills, expected_count)
    readings = []
    y = rect[1] + rect[3] / 2
    for slot, pill in enumerate(pills):
        center = pill['center']
        pixels = [image.getpixel((round(center - 10 + dx), round(y - 8 + dy))) for dx in range(3) for dy in range(3)]
        rgb = tuple(round(median(pixel[channel] for pixel in pixels)) for channel in range(3))
        blue = rgb[0] < 90 and 45 < rgb[1] < 165 and rgb[2] > 175
        next_left = pills[slot + 1]['left'] if slot + 1 < len(pills) else pill['right'] + 10
        gap_center = (pill['right'] + next_left) / 2
        gap_pixels = [image.getpixel((round(gap_center + dx), round(y - 8 + dy))) for dx in range(-1, 2) for dy in range(3)]
        gap_rgb = tuple(round(median(pixel[channel] for pixel in gap_pixels)) for channel in range(3))
        material_difference = distance(rgb, gap_rgb)
        glyph_pixels = [image.getpixel((round(center + dx), round(y + dy))) for dx in range(-7, 8) for dy in range(-7, 8)]
        glyph_ink = sum(distance(pixel, rgb) >= 40 for pixel in glyph_pixels)
        present = material_difference >= 4 and glyph_ink >= 2
        readings.append(dict(pill, slot=slot, rgb=rgb, gap_rgb=gap_rgb,
                             material_difference=material_difference, glyph_ink=glyph_ink,
                             present=present, selected_blue=blue))
    assert [row['slot'] for row in readings if row['present']] == list(range(expected_count)), readings
    assert [row['slot'] for row in readings if row['selected_blue']] == selected, readings
    return readings
