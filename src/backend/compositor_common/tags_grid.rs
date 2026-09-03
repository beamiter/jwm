//! Geometry for the tags overview's workspace grid.
//!
//! The overview is drawn as a card holding one cell per tag of the selected
//! monitor, each cell carrying a line-drawn wireframe of that tag's windows
//! and the tag's number. It is the two-dimensional sibling of the layout
//! picker's film strip ([`crate::backend::compositor_common::layout_strip`]),
//! whose thumbnail mapping ([`layout_strip::window_rect`]) the renderers apply
//! inside each cell's frame.
//!
//! Everything here is pure arithmetic on a viewport, a cell count and a
//! column count, which is what lets the two compositors draw the same grid
//! and the window manager hit-test against it without any of them exchanging
//! coordinates.

use crate::backend::compositor_common::layout_strip::{self, Rect};

/// One grid cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridCell {
    /// The cell card itself.
    pub cell: Rect,
    /// The frame inside it the tag's window wireframes are drawn in.
    pub frame: Rect,
    /// Offset from the cell's origin to the tag label's text origin
    /// (top-left). Kept relative so the selected cell's presentation-only
    /// scale can carry the label along.
    pub label_offset: [f32; 2],
}

/// Where every part of the overview panel goes.
#[derive(Clone, Debug, PartialEq)]
pub struct TagsGridGeometry {
    /// The card the whole overview sits on.
    pub panel: Rect,
    /// Text origin (top-left) of the panel title.
    pub title: [f32; 2],
    /// Centre of the caption band under the grid, where the selected tag's
    /// name goes.
    pub caption_center: [f32; 2],
    /// Text origin (top-left) of the footer hint.
    pub hint: [f32; 2],
    pub cells: Vec<GridCell>,
    /// The column count actually used (clamped to the cell count).
    pub cols: u32,
    pub rows: u32,
}

/// Padding between the panel edge and its contents.
const PAD: f32 = 26.0;
/// Band reserved for the title line.
const TITLE_H: f32 = 30.0;
/// Band reserved for the selected tag's name, under the grid.
const CAPTION_H: f32 = 30.0;
/// Band reserved for the footer hint.
const HINT_H: f32 = 24.0;
/// Vertical breathing room between bands.
const GAP_Y: f32 = 14.0;
/// Gap between two cells.
const CELL_GAP: f32 = 10.0;
/// Band at the top of a cell holding the tag number.
const LABEL_H: f32 = 20.0;
/// Inset between a cell's edge and its wireframe frame.
const FRAME_PAD: f32 = 7.0;

/// Cell width bounds. The lower bound keeps a wireframe readable on a small
/// screen with many tags; the upper one keeps a handful of tags on a large
/// screen from turning into posters.
const CELL_W_MIN: f32 = 96.0;
const CELL_W_MAX: f32 = 260.0;
/// Smallest cell height the vertical clamp may produce before the panel is
/// allowed to reach toward the screen edges instead.
const CELL_H_MIN: f32 = 64.0;

/// Lay out an overview of `count` tags in `cols` columns inside a global
/// output `viewport`. The returned rectangles remain in global compositor
/// coordinates, so the WM hit-test and both renderers can consume this exact
/// geometry even when an output has a negative or non-zero origin.
pub fn grid_geometry(viewport: Rect, count: usize, cols: u32) -> TagsGridGeometry {
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
    let cols = (cols.max(1) as usize).min(count);
    let rows = count.div_ceil(cols);

    // A cell mirrors the output's aspect, the way a workspace thumbnail
    // should; extreme (or degenerate) viewports are pinned to a sane band.
    let aspect = (screen_w / screen_h).clamp(0.4, 3.0);

    let outer = (screen_w * 0.94).min(1560.0).max(320.0);
    let available_w = (outer - 2.0 * PAD - CELL_GAP * (cols as f32 - 1.0)).max(CELL_W_MIN);
    let mut cell_w = (available_w / cols as f32).clamp(CELL_W_MIN, CELL_W_MAX);
    let mut cell_h = cell_w / aspect;

    // The grid must also leave room for the text bands; on short screens the
    // height, not the width, decides the cell size.
    let bands_h = 2.0 * PAD + TITLE_H + GAP_Y + CAPTION_H + GAP_Y + HINT_H;
    let available_h =
        (screen_h * 0.9 - bands_h - CELL_GAP * (rows as f32 - 1.0)).max(CELL_H_MIN * rows as f32);
    let max_cell_h = (available_h / rows as f32).max(CELL_H_MIN);
    if cell_h > max_cell_h {
        cell_h = max_cell_h;
        cell_w = cell_h * aspect;
    }

    let grid_w = cols as f32 * cell_w + (cols as f32 - 1.0) * CELL_GAP;
    let grid_h = rows as f32 * cell_h + (rows as f32 - 1.0) * CELL_GAP;
    let panel_w = grid_w + 2.0 * PAD;
    let panel_h = bands_h + grid_h + GAP_Y;

    let panel_x = viewport_x + ((screen_w - panel_w) * 0.5).round();
    // Slightly above centre, same reading as the film strip: the grid is
    // about the desktop behind it, and a floating band reads better a little
    // high.
    let panel_y = viewport_y + ((screen_h - panel_h) * 0.42).round().max(16.0);

    let grid_x = panel_x + PAD;
    let grid_y = panel_y + PAD + TITLE_H + GAP_Y;

    let mut cells = Vec::with_capacity(count);
    for i in 0..count {
        let col = (i % cols) as f32;
        let row = (i / cols) as f32;
        let x = grid_x + col * (cell_w + CELL_GAP);
        let y = grid_y + row * (cell_h + CELL_GAP);
        cells.push(GridCell {
            cell: [x, y, cell_w, cell_h],
            frame: [
                x + FRAME_PAD,
                y + LABEL_H,
                (cell_w - 2.0 * FRAME_PAD).max(1.0),
                (cell_h - LABEL_H - FRAME_PAD).max(1.0),
            ],
            label_offset: [FRAME_PAD, (LABEL_H - 14.0).max(2.0) * 0.5],
        });
    }

    let caption_y = grid_y + grid_h + GAP_Y;
    let hint_y = caption_y + CAPTION_H + GAP_Y;

    TagsGridGeometry {
        panel: [panel_x, panel_y, panel_w, panel_h],
        title: [panel_x + PAD, panel_y + PAD],
        caption_center: [panel_x + panel_w * 0.5, caption_y + CAPTION_H * 0.5],
        hint: [panel_x + PAD, hint_y],
        cells,
        cols: cols as u32,
        rows: rows as u32,
    }
}

/// Which cell contains `(x, y)`, if any.
///
/// The whole cell is the target, not just its frame, so the gaps between
/// cells are the only dead space.
pub fn cell_at(geometry: &TagsGridGeometry, x: f32, y: f32) -> Option<usize> {
    geometry.cells.iter().position(|cell| {
        let [cx, cy, cw, ch] = cell.cell;
        x >= cx && x < cx + cw && y >= cy && y < cy + ch
    })
}

/// Whether `(x, y)` falls inside the panel card at all. The hit-test splits
/// the card from the modal scrim around it: a click in the title, caption or
/// hint bands is inside the panel without being on a cell.
pub fn panel_contains(geometry: &TagsGridGeometry, x: f32, y: f32) -> bool {
    let [px, py, pw, ph] = geometry.panel;
    x >= px && x < px + pw && y >= py && y < py + ph
}

/// Which of a cell's window wireframes `(x, y)` lands on, if any.
///
/// `windows` are the cell's normalized outlines in back-to-front draw order,
/// so the last hit wins — the wireframe the eye sees on top is the one the
/// pointer grabs. Every outline is mapped through the same
/// [`layout_strip::window_rect`] the renderers draw with, minimum line size
/// included, so the clickable shape is the drawn shape. `frame` and the
/// point share one (global) coordinate space; the frame's inset from its
/// cell is already accounted for by the caller passing `frame`, not `cell`.
pub fn frame_window_at(frame: Rect, windows: &[[f32; 4]], x: f32, y: f32) -> Option<usize> {
    windows.iter().rposition(|window| {
        let [wx, wy, ww, wh] = layout_strip::window_rect(frame, *window);
        x >= wx && x < wx + ww && y >= wy && y < wy + wh
    })
}

/// Text origin of a cell's tag label once the cell's presentation scale
/// (see [`crate::backend::compositor_common::layout_strip::SELECTED_SCALE`])
/// is applied. The label scales with its cell so the selected cell's lift
/// does not leave the number behind.
pub fn label_origin(cell: &GridCell, scaled_cell: Rect, scale: f32) -> [f32; 2] {
    [
        scaled_cell[0] + cell.label_offset[0] * scale,
        scaled_cell[1] + cell.label_offset[1] * scale,
    ]
}

/// Diameter of a cell's urgency badge.
const BADGE_D: f32 = 8.0;

/// Where a cell's urgency badge goes: a dot at the label band's right end,
/// vertically centred in the band, so it reads as part of the tag's title row
/// and never covers a wireframe. Like the label it is placed from the scaled
/// cell, so the selected cell's presentation lift carries it along.
pub fn urgent_badge_rect(scaled_cell: Rect, scale: f32) -> Rect {
    [
        scaled_cell[0] + scaled_cell[2] - (FRAME_PAD + BADGE_D) * scale,
        scaled_cell[1] + (LABEL_H - BADGE_D) * 0.5 * scale,
        BADGE_D * scale,
        BADGE_D * scale,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::compositor_common::layout_strip::center;

    fn geom(count: usize, cols: u32) -> TagsGridGeometry {
        grid_geometry([0.0, 0.0, 1920.0, 1080.0], count, cols)
    }

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
    }

    #[test]
    fn cells_stay_inside_the_panel_and_never_overlap() {
        for (count, cols) in [(1usize, 1u32), (2, 2), (7, 3), (9, 3), (13, 4), (31, 7)] {
            let g = geom(count, cols);
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
            for (i, a) in g.cells.iter().enumerate() {
                for b in g.cells.iter().skip(i + 1) {
                    assert!(
                        !rects_overlap(a.cell, b.cell),
                        "count={count} cells overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn panel_fits_the_screen_even_with_every_tag() {
        for (w, h) in [
            (1024.0, 768.0),
            (1366.0, 768.0),
            (1920.0, 1080.0),
            (3840.0, 2160.0),
        ] {
            let g = grid_geometry([0.0, 0.0, w, h], 31, 7);
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
    fn the_grid_shape_matches_the_requested_columns() {
        let one = geom(1, 1);
        assert_eq!((one.cols, one.rows), (1, 1));

        let nine = geom(9, 3);
        assert_eq!((nine.cols, nine.rows), (3, 3));
        // Row-major order: cell 4 sits one column right and one row down
        // from cell 3, and exactly below cell 1.
        let row_y = nine.cells[1].cell[1];
        assert!(nine.cells[4].cell[1] > row_y);
        assert_eq!(nine.cells[4].cell[0], nine.cells[1].cell[0]);

        let thirty_one = geom(31, 7);
        assert_eq!((thirty_one.cols, thirty_one.rows), (7, 5));

        // More columns than cells collapses to the cell count.
        let two = geom(2, 9);
        assert_eq!((two.cols, two.rows), (2, 1));
    }

    #[test]
    fn a_click_in_a_cell_finds_that_cell() {
        let g = geom(13, 4);
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
    fn the_panel_card_is_hit_separately_from_its_cells() {
        let g = geom(13, 4);
        let [px, py, pw, ph] = g.panel;
        // The title band is panel but no cell; so is every cell's interior.
        assert!(panel_contains(&g, px + 1.0, py + 1.0));
        let [x, y] = center(g.cells[0].cell);
        assert!(panel_contains(&g, x, y));
        // The scrim around the card is not.
        assert!(!panel_contains(&g, px - 1.0, py - 1.0));
        assert!(!panel_contains(&g, px + pw + 1.0, py + ph + 1.0));
        assert!(
            !panel_contains(&g, px + pw, py + ph),
            "far edge is exclusive"
        );
    }

    #[test]
    fn negative_and_nonzero_origins_offset_render_and_hit_test_together() {
        let viewport = [-1920.0, 180.0, 1920.0, 1080.0];
        let g = grid_geometry(viewport, 9, 3);
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
            "monitor-local coordinates must not hit global grid geometry"
        );
    }

    #[test]
    fn the_label_rides_the_selected_cells_scale() {
        let g = geom(9, 3);
        let cell = g.cells[4];
        let scaled = [
            cell.cell[0] + 3.0,
            cell.cell[1] + 2.0,
            cell.cell[2] * 1.12,
            cell.cell[3] * 1.12,
        ];
        let origin = label_origin(&cell, scaled, 1.12);
        assert_eq!(origin[0], scaled[0] + cell.label_offset[0] * 1.12);
        assert_eq!(origin[1], scaled[1] + cell.label_offset[1] * 1.12);
    }

    #[test]
    fn the_urgent_badge_sits_in_the_label_bands_right_end() {
        let g = geom(9, 3);
        for cell in &g.cells {
            let badge = urgent_badge_rect(cell.cell, 1.0);
            let [x, y, w, _] = cell.cell;
            assert_eq!(badge[2], badge[3], "the badge is a circle");
            assert!(badge[0] > x && badge[0] + badge[2] < x + w);
            assert!(
                badge[1] >= y && badge[1] + badge[3] <= y + LABEL_H + 0.01,
                "the badge must stay inside the label band, clear of the frame"
            );
            // Right-aligned against the same inset the frame uses.
            assert!((badge[0] + badge[2] - (x + w - FRAME_PAD)).abs() < 0.001);
        }
    }

    #[test]
    fn the_urgent_badge_rides_the_selected_cells_scale() {
        let g = geom(9, 3);
        let cell = g.cells[4];
        let pivot = center(cell.cell);
        let scaled =
            crate::backend::compositor_common::layout_strip::scaled_about(cell.cell, pivot, 1.12);
        let badge = urgent_badge_rect(scaled, 1.12);
        assert!((badge[2] - BADGE_D * 1.12).abs() < 0.001);
        // Scaled about the pivot, the badge's own centre lands where the
        // unscaled badge's centre was carried by the same transform.
        let unscaled = urgent_badge_rect(cell.cell, 1.0);
        let [ux, uy] = center(unscaled);
        let expect_x = pivot[0] + (ux - pivot[0]) * 1.12;
        let expect_y = pivot[1] + (uy - pivot[1]) * 1.12;
        let [bx, by] = center(badge);
        assert!((bx - expect_x).abs() < 0.01, "{bx} vs {expect_x}");
        assert!((by - expect_y).abs() < 0.01, "{by} vs {expect_y}");
    }

    #[test]
    fn a_wireframe_hit_picks_the_topmost_outline() {
        let g = geom(9, 3);
        let frame = g.cells[0].frame;
        // Two overlapping outlines, the second drawn (and stacked) on top,
        // plus a third off in its own corner.
        let windows = [
            [0.1, 0.1, 0.5, 0.5],
            [0.3, 0.3, 0.5, 0.5],
            [0.7, 0.7, 0.2, 0.2],
        ];
        let point_in = |window: [f32; 4]| {
            let rect = layout_strip::window_rect(frame, window);
            center(rect)
        };

        // The overlap belongs to the topmost wireframe.
        let [x, y] = point_in([0.35, 0.35, 0.2, 0.2]);
        assert_eq!(frame_window_at(frame, &windows, x, y), Some(1));
        // Where only the lower wireframe sits, the lower one answers.
        let [x, y] = point_in([0.12, 0.12, 0.1, 0.1]);
        assert_eq!(frame_window_at(frame, &windows, x, y), Some(0));
        let [x, y] = point_in(windows[2]);
        assert_eq!(frame_window_at(frame, &windows, x, y), Some(2));
    }

    #[test]
    fn a_wireframe_hit_respects_the_frame_and_misses_empty_space() {
        let g = geom(9, 3);
        let cell = g.cells[0];
        let [fx, fy, fw, fh] = cell.frame;
        let windows = [[0.0, 0.0, 0.5, 0.5]];

        // The frame's padding and label band are not the wireframe: the same
        // normalized outline starts at the frame origin, not the cell origin.
        assert_eq!(
            frame_window_at(cell.frame, &windows, fx + 1.0, fy + 1.0),
            Some(0)
        );
        let [cx, cy, _, _] = cell.cell;
        assert_eq!(
            frame_window_at(cell.frame, &windows, cx + 1.0, cy + 1.0),
            None,
            "the cell's label band must not hit a wireframe drawn in the frame"
        );
        // Inside the frame but past the outline, and outside the grid
        // entirely: no hit.
        assert_eq!(
            frame_window_at(cell.frame, &windows, fx + fw * 0.9, fy + fh * 0.9),
            None
        );
        assert_eq!(frame_window_at(cell.frame, &windows, -50.0, -50.0), None);
        // An empty cell hits nothing.
        assert_eq!(frame_window_at(cell.frame, &[], fx + 1.0, fy + 1.0), None);
    }

    #[test]
    fn a_tiny_wireframe_is_hit_at_its_drawn_minimum_size() {
        let g = geom(9, 3);
        let [fx, fy, _, _] = g.cells[0].frame;
        // Smaller than the line-width floor the renderer clamps to; the hit
        // shape follows the drawn shape, not the raw normalized numbers.
        let windows = [[0.0, 0.0, 0.0001, 0.0001]];
        let floor = layout_strip::LINE_WIDTH * 2.0;
        assert_eq!(
            frame_window_at(
                g.cells[0].frame,
                &windows,
                fx + floor * 0.75,
                fy + floor * 0.75
            ),
            Some(0),
            "inside the clamped outline"
        );
        assert_eq!(
            frame_window_at(
                g.cells[0].frame,
                &windows,
                fx + floor * 2.0,
                fy + floor * 2.0
            ),
            None,
            "past the clamped outline"
        );
    }
}
