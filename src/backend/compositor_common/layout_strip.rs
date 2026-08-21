//! Geometry for the layout picker's film strip.
//!
//! The picker is drawn as a strip of 35mm film: one cell per layout, each cell
//! holding a line-drawn thumbnail of what that layout does with a screenful of
//! windows, with sprocket holes running along the top and bottom edges.
//!
//! Everything here is pure arithmetic on a screen size and a cell count, which
//! is what lets the two compositors draw the same strip and the window manager
//! hit-test clicks against it without any of them exchanging coordinates.

/// A rectangle in screen pixels: `[x, y, w, h]`.
pub type Rect = [f32; 4];

/// One film cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// The cell itself, i.e. the piece of film.
    pub cell: Rect,
    /// The exposed frame inside it, which the thumbnail is drawn in.
    pub frame: Rect,
}

/// Where every part of the picker panel goes.
#[derive(Clone, Debug, PartialEq)]
pub struct StripGeometry {
    /// The card the whole picker sits on.
    pub panel: Rect,
    /// Text origin (top-left) of the panel title.
    pub title: [f32; 2],
    /// Centre of the caption band under the strip, where the selected
    /// layout's name goes.
    pub caption_center: [f32; 2],
    /// Text origin (top-left) of the footer hint.
    pub hint: [f32; 2],
    /// The film base the cells are punched out of.
    pub strip: Rect,
    pub cells: Vec<Cell>,
    /// Sprocket holes along the film's top and bottom edges.
    pub sprockets: Vec<Rect>,
    /// Full auto-confirm countdown track; the renderer fills a fraction of it.
    pub countdown: Rect,
}

/// Corner radius of the panel.
pub const PANEL_RADIUS: f32 = 22.0;
/// Corner radius of one film cell.
pub const CELL_RADIUS: f32 = 6.0;
/// Corner radius of a thumbnail's window outline.
pub const WINDOW_RADIUS: f32 = 2.5;
/// Stroke width of the thumbnail outlines.
pub const LINE_WIDTH: f32 = 1.25;
/// How much larger the selected cell is drawn. Hit rectangles stay uniform, so
/// this is presentation only.
pub const SELECTED_SCALE: f32 = 1.12;

/// Padding between the panel edge and its contents.
const PAD: f32 = 26.0;
/// Band reserved for the title line.
const TITLE_H: f32 = 30.0;
/// Band reserved for the selected layout's name, under the strip.
const CAPTION_H: f32 = 30.0;
/// Band reserved for the footer hint.
const HINT_H: f32 = 24.0;
/// Vertical breathing room between bands.
const GAP_Y: f32 = 14.0;
/// Gap between two cells.
const CELL_GAP: f32 = 10.0;
/// Film margin above and below a cell's exposed frame, where the sprocket
/// holes live. Proportional to the cell so the perforation keeps its look
/// from a crowded 1024px screen up to a 4K one.
fn film_margin(cell_w: f32) -> f32 {
    (cell_w * 0.13).clamp(6.0, 12.0)
}
/// Height of the countdown track.
const COUNTDOWN_H: f32 = 3.0;

/// Cell width bounds. The lower bound keeps a thumbnail readable on a small
/// screen with many layouts; the upper one keeps a handful of layouts on a
/// large screen from turning into posters.
const CELL_W_MIN: f32 = 54.0;
const CELL_W_MAX: f32 = 148.0;
/// Exposed frame aspect: a 16:10 screen.
const FRAME_ASPECT: f32 = 0.625;

/// Lay out a picker for `count` layouts inside a global output `viewport`.
/// The returned rectangles remain in global compositor coordinates, so the WM
/// hit-test and both renderers can consume this exact geometry even when an
/// output has a negative or non-zero origin.
pub fn strip_geometry(viewport: Rect, count: usize) -> StripGeometry {
    let [viewport_x, viewport_y, screen_w, screen_h] = viewport;
    let viewport_x = if viewport_x.is_finite() {
        viewport_x
    } else {
        0.0
    };
    let viewport_y = if viewport_y.is_finite() {
        viewport_y
    } else {
        0.0
    };
    let screen_w = if screen_w.is_finite() && screen_w > 0.0 {
        screen_w
    } else {
        1.0
    };
    let screen_h = if screen_h.is_finite() && screen_h > 0.0 {
        screen_h
    } else {
        1.0
    };
    let count = count.max(1);
    let n = count as f32;

    // Cells shrink until the whole strip fits the screen; below the minimum
    // width the panel is simply allowed to reach the screen edges, which only
    // happens on a screen far narrower than any the picker is aimed at.
    let outer = (screen_w * 0.94).min(1560.0).max(320.0);
    let available = (outer - 2.0 * PAD - CELL_GAP * (n - 1.0)).max(CELL_W_MIN);
    let cell_w = (available / n).clamp(CELL_W_MIN, CELL_W_MAX);
    let margin = film_margin(cell_w);
    let frame_w = cell_w - 2.0 * margin;
    let frame_h = (frame_w * FRAME_ASPECT).round();
    let cell_h = frame_h + 2.0 * margin;

    let strip_w = n * cell_w + (n - 1.0) * CELL_GAP;
    let panel_w = strip_w + 2.0 * PAD;
    let panel_h =
        2.0 * PAD + TITLE_H + GAP_Y + cell_h + GAP_Y + CAPTION_H + COUNTDOWN_H + GAP_Y + HINT_H;

    let panel_x = viewport_x + ((screen_w - panel_w) * 0.5).round();
    // Slightly above centre: the strip is about the desktop behind it, and the
    // eye reads a floating band better a little high.
    let panel_y = viewport_y + ((screen_h - panel_h) * 0.42).round().max(16.0);

    let strip_x = panel_x + PAD;
    let strip_y = panel_y + PAD + TITLE_H + GAP_Y;

    let mut cells = Vec::with_capacity(count);
    for i in 0..count {
        let x = strip_x + i as f32 * (cell_w + CELL_GAP);
        cells.push(Cell {
            cell: [x, strip_y, cell_w, cell_h],
            frame: [x + margin, strip_y + margin, frame_w, frame_h],
        });
    }

    let caption_y = strip_y + cell_h + GAP_Y;
    let countdown_y = caption_y + CAPTION_H;

    StripGeometry {
        panel: [panel_x, panel_y, panel_w, panel_h],
        title: [panel_x + PAD, panel_y + PAD],
        caption_center: [panel_x + panel_w * 0.5, caption_y + CAPTION_H * 0.5],
        hint: [panel_x + PAD, countdown_y + COUNTDOWN_H + GAP_Y],
        strip: [strip_x, strip_y, strip_w, cell_h],
        sprockets: sprockets(strip_x, strip_y, strip_w, cell_h, margin),
        cells,
        countdown: [strip_x, countdown_y, strip_w, COUNTDOWN_H],
    }
}

/// Sprocket holes: two rows of rounded slots at a fixed pitch, inset into the
/// film margins. They run the length of the strip rather than per cell, so the
/// perforation stays continuous across the cell gaps like real film.
fn sprockets(x: f32, y: f32, w: f32, h: f32, margin: f32) -> Vec<Rect> {
    let hole_h = (margin * 0.46).max(3.0);
    let hole_w = hole_h * 1.7;
    let pitch = hole_w * 2.1;
    let inset = (margin - hole_h) * 0.5;
    let count = ((w - hole_w) / pitch).floor().max(0.0) as usize;
    // Centre the run so the strip does not end on a half-cut hole.
    let run = count as f32 * pitch;
    let start = x + ((w - run - hole_w) * 0.5).max(0.0);

    let mut holes = Vec::with_capacity((count + 1) * 2);
    for i in 0..=count {
        let hx = start + i as f32 * pitch;
        holes.push([hx, y + inset, hole_w, hole_h]);
        holes.push([hx, y + h - inset - hole_h, hole_w, hole_h]);
    }
    holes
}

/// Which cell contains `(x, y)`, if any.
///
/// The whole cell is the target, not just its exposed frame, so the gaps
/// between cells are the only dead space.
pub fn cell_at(geometry: &StripGeometry, x: f32, y: f32) -> Option<usize> {
    geometry.cells.iter().position(|cell| {
        let [cx, cy, cw, ch] = cell.cell;
        x >= cx && x < cx + cw && y >= cy && y < cy + ch
    })
}

/// Place one thumbnail window inside a cell's exposed frame.
///
/// `window` is `[x, y, w, h]` in `0.0..=1.0` of the frame, as produced by
/// [`crate::core::layout::preview_frames`].
pub fn window_rect(frame: Rect, window: [f32; 4]) -> Rect {
    let [fx, fy, fw, fh] = frame;
    [
        fx + window[0] * fw,
        fy + window[1] * fh,
        (window[2] * fw).max(LINE_WIDTH * 2.0),
        (window[3] * fh).max(LINE_WIDTH * 2.0),
    ]
}

/// Scale a cell's rectangles about the cell centre, for the selected cell's
/// slight lift out of the strip.
pub fn scaled_about(rect: Rect, center: [f32; 2], scale: f32) -> Rect {
    [
        center[0] + (rect[0] - center[0]) * scale,
        center[1] + (rect[1] - center[1]) * scale,
        rect[2] * scale,
        rect[3] * scale,
    ]
}

/// Centre point of a rectangle.
pub fn center(rect: Rect) -> [f32; 2] {
    [rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.5]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(count: usize) -> StripGeometry {
        strip_geometry([0.0, 0.0, 1920.0, 1080.0], count)
    }

    #[test]
    fn cells_stay_inside_the_panel_and_never_overlap() {
        for count in [1usize, 2, 7, 13, 20] {
            let g = geom(count);
            assert_eq!(g.cells.len(), count);
            let [px, py, pw, ph] = g.panel;
            for cell in &g.cells {
                let [x, y, w, h] = cell.cell;
                assert!(
                    x >= px && x + w <= px + pw + 0.5,
                    "count={count} cell escapes panel"
                );
                assert!(
                    y >= py && y + h <= py + ph + 0.5,
                    "count={count} cell escapes panel"
                );
                let [fx, fy, fw, fh] = cell.frame;
                assert!(fx >= x && fx + fw <= x + w + 0.01);
                assert!(fy >= y && fy + fh <= y + h + 0.01);
            }
            for pair in g.cells.windows(2) {
                let left = pair[0].cell;
                let right = pair[1].cell;
                assert!(left[0] + left[2] <= right[0] + 0.01, "cells overlap");
            }
        }
    }

    #[test]
    fn panel_fits_the_screen_even_with_every_layout() {
        for (w, h) in [
            (1024.0, 768.0),
            (1366.0, 768.0),
            (1920.0, 1080.0),
            (3840.0, 2160.0),
        ] {
            let g = strip_geometry([0.0, 0.0, w, h], 13);
            assert!(g.panel[0] >= 0.0, "{w}x{h}: panel starts off-screen");
            assert!(
                g.panel[0] + g.panel[2] <= w + 0.5,
                "{w}x{h}: panel runs off the right"
            );
            assert!(
                g.panel[1] + g.panel[3] <= h + 0.5,
                "{w}x{h}: panel runs off the bottom"
            );
        }
    }

    #[test]
    fn a_click_in_a_cell_finds_that_cell() {
        let g = geom(13);
        for (index, cell) in g.cells.iter().enumerate() {
            let [x, y] = center(cell.cell);
            assert_eq!(cell_at(&g, x, y), Some(index));
        }
        let [px, py, _, _] = g.panel;
        assert_eq!(
            cell_at(&g, px + 1.0, py + 1.0),
            None,
            "the title band is not a cell"
        );
    }

    #[test]
    fn negative_and_nonzero_origins_offset_render_and_hit_test_together() {
        let viewport = [-1920.0, 180.0, 1920.0, 1080.0];
        let g = strip_geometry(viewport, 7);
        let [px, py, pw, ph] = g.panel;
        assert!(px >= viewport[0]);
        assert!(py >= viewport[1]);
        assert!(px + pw <= viewport[0] + viewport[2] + 0.5);
        assert!(py + ph <= viewport[1] + viewport[3] + 0.5);

        let [x, y] = center(g.cells[3].cell);
        assert_eq!(cell_at(&g, x, y), Some(3));
        assert_eq!(
            cell_at(&g, x - viewport[0], y - viewport[1]),
            None,
            "monitor-local coordinates must not hit global film geometry"
        );
    }

    #[test]
    fn sprockets_line_both_film_edges() {
        let g = geom(6);
        assert!(!g.sprockets.is_empty());
        let [sx, sy, sw, sh] = g.strip;
        let top = g
            .sprockets
            .iter()
            .filter(|hole| hole[1] < sy + sh * 0.5)
            .count();
        let bottom = g.sprockets.len() - top;
        assert_eq!(top, bottom, "perforation is symmetric");
        for hole in &g.sprockets {
            assert!(hole[0] >= sx - 0.5 && hole[0] + hole[2] <= sx + sw + 0.5);
            assert!(hole[1] >= sy && hole[1] + hole[3] <= sy + sh);
        }
    }

    #[test]
    fn thumbnail_windows_land_inside_their_frame() {
        let g = geom(4);
        let frame = g.cells[0].frame;
        let full = window_rect(frame, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(full, frame);
        let quarter = window_rect(frame, [0.5, 0.5, 0.5, 0.5]);
        assert!(quarter[0] >= frame[0] && quarter[0] + quarter[2] <= frame[0] + frame[2] + 0.01);
        assert!(quarter[1] >= frame[1] && quarter[1] + quarter[3] <= frame[1] + frame[3] + 0.01);
    }
}
