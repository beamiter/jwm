//! Expose / Mission Control grid layout and animation.
//!
//! Platform-neutral: the entry's window identifier is generic so the X11
//! compositor can instantiate it with `u32` XIDs and the Wayland compositor
//! with its `u64` window ids. Previously the layout and tick logic existed
//! twice (X11 tree and an inline Wayland copy) and had already drifted; this
//! module is the single canonical implementation.

use crate::backend::api::ExposeNavDirection;

/// Entry for Expose/Mission Control mode.
pub struct ExposeEntry<Id> {
    pub id: Id,
    pub orig_x: f32,
    pub orig_y: f32,
    pub orig_w: f32,
    pub orig_h: f32,
    pub target_x: f32,
    pub target_y: f32,
    pub target_w: f32,
    pub target_h: f32,
    pub current_x: f32,
    pub current_y: f32,
    pub current_w: f32,
    pub current_h: f32,
    pub is_hovered: bool,
}

pub struct ExposeTickResult {
    pub keep_animating: bool,
    pub clear_entries: bool,
}

/// Column count of the expose grid for `n` entries on a `screen_w`×`screen_h`
/// output. Shared by [`build_expose_entries`] and keyboard navigation so the
/// hit geometry and the arrow-key walk can never disagree on the grid shape.
pub(crate) fn expose_grid_cols(n: usize, screen_w: f32, screen_h: f32) -> u32 {
    let screen_w = if screen_w.is_finite() {
        screen_w.max(1.0)
    } else {
        1.0
    };
    let screen_h = if screen_h.is_finite() {
        screen_h.max(1.0)
    } else {
        1.0
    };
    let screen_aspect = screen_w / screen_h.max(1.0);
    (((n as f32 * screen_aspect).sqrt()).ceil() as u32).max(1)
}

/// Move the keyboard selection one step through the expose grid laid out by
/// [`build_expose_entries`] (row-major order).
///
/// No selection yet picks the first entry regardless of direction; movement
/// clamps at row and grid edges instead of wrapping, and Down onto a short
/// last row stays put rather than landing past the end.
pub(crate) fn move_expose_selection(
    current: Option<usize>,
    dir: ExposeNavDirection,
    len: usize,
    cols: u32,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cols = cols.max(1) as usize;
    let idx = match current {
        None => return Some(0),
        Some(i) => i.min(len - 1),
    };
    let col = idx % cols;
    let next = match dir {
        ExposeNavDirection::Left if col == 0 => idx,
        ExposeNavDirection::Left => idx - 1,
        ExposeNavDirection::Right if col + 1 >= cols => idx,
        ExposeNavDirection::Right if idx + 1 >= len => idx,
        ExposeNavDirection::Right => idx + 1,
        ExposeNavDirection::Up if idx < cols => idx,
        ExposeNavDirection::Up => idx - cols,
        ExposeNavDirection::Down if idx + cols >= len => idx,
        ExposeNavDirection::Down => idx + cols,
    };
    Some(next)
}

/// Compute Expose/Mission Control targets for a set of window rectangles.
pub fn build_expose_entries<Id: Copy>(
    screen_w: f32,
    screen_h: f32,
    gap: f32,
    windows: &[(Id, i32, i32, u32, u32)],
) -> Vec<ExposeEntry<Id>> {
    let n = windows.len();
    if n == 0 {
        return Vec::new();
    }

    let screen_w = if screen_w.is_finite() {
        screen_w.max(1.0)
    } else {
        1.0
    };
    let screen_h = if screen_h.is_finite() {
        screen_h.max(1.0)
    } else {
        1.0
    };
    let cols = expose_grid_cols(n, screen_w, screen_h);
    let rows = (n as u32).div_ceil(cols).max(1);

    // Leave at least one pixel for every row and column. A configured gap is
    // user input and may be much larger than a small output; allowing it to
    // consume the whole grid produces negative thumbnails and reversed hit
    // boxes on both compositors.
    let requested_gap = if gap.is_finite() { gap.max(0.0) } else { 0.0 };
    let max_gap_w = ((screen_w - cols as f32) / (cols as f32 + 1.0)).max(0.0);
    let max_gap_h = ((screen_h - rows as f32) / (rows as f32 + 1.0)).max(0.0);
    let gap = requested_gap.min(max_gap_w.min(max_gap_h));
    let cell_w = ((screen_w - gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
    let cell_h = ((screen_h - gap * (rows as f32 + 1.0)) / rows as f32).max(1.0);

    windows
        .iter()
        .enumerate()
        .map(|(i, &(id, x, y, w, h))| {
            let col = i as u32 % cols;
            let row = i as u32 / cols;

            let cell_x = gap + col as f32 * (cell_w + gap);
            let cell_y = gap + row as f32 * (cell_h + gap);

            let win_aspect = w as f32 / h.max(1) as f32;
            let cell_aspect = cell_w / cell_h.max(1.0);
            let (tw, th) = if win_aspect > cell_aspect {
                (cell_w, cell_w / win_aspect)
            } else {
                (cell_h * win_aspect, cell_h)
            };
            let tx = cell_x + (cell_w - tw) * 0.5;
            let ty = cell_y + (cell_h - th) * 0.5;

            ExposeEntry {
                id,
                orig_x: x as f32,
                orig_y: y as f32,
                orig_w: w as f32,
                orig_h: h as f32,
                target_x: tx,
                target_y: ty,
                target_w: tw,
                target_h: th,
                current_x: x as f32,
                current_y: y as f32,
                current_w: w as f32,
                current_h: h as f32,
                is_hovered: false,
            }
        })
        .collect()
}

pub fn tick_expose_entries<Id>(
    entries: &mut Vec<ExposeEntry<Id>>,
    active: bool,
    opacity: &mut f32,
    dt: f32,
) -> ExposeTickResult {
    if entries.is_empty() {
        return ExposeTickResult {
            keep_animating: false,
            clear_entries: false,
        };
    }

    let ease_speed = 12.0_f32;
    let t = 1.0 - (-ease_speed * dt).exp();
    let mut any_moving = false;

    if active {
        *opacity = (*opacity + dt * 4.0).min(1.0);
        if *opacity < 1.0 {
            any_moving = true;
        }

        for entry in entries.iter_mut() {
            any_moving |= tick_entry_towards(
                entry,
                entry.target_x,
                entry.target_y,
                entry.target_w,
                entry.target_h,
                t,
            );
        }
    } else {
        for entry in entries.iter_mut() {
            any_moving |= tick_entry_towards(
                entry,
                entry.orig_x,
                entry.orig_y,
                entry.orig_w,
                entry.orig_h,
                t,
            );
        }

        let fade_speed = if any_moving { 8.0 } else { 20.0 };
        *opacity = (*opacity - dt * fade_speed).max(0.0);
        if *opacity > 0.0 {
            any_moving = true;
        }

        if *opacity <= 0.0 && !any_moving {
            return ExposeTickResult {
                keep_animating: false,
                clear_entries: true,
            };
        }
    }

    ExposeTickResult {
        keep_animating: any_moving || (*opacity > 0.0 && *opacity < 1.0),
        clear_entries: false,
    }
}

fn tick_entry_towards<Id>(
    entry: &mut ExposeEntry<Id>,
    target_x: f32,
    target_y: f32,
    target_w: f32,
    target_h: f32,
    t: f32,
) -> bool {
    let dx = target_x - entry.current_x;
    let dy = target_y - entry.current_y;
    let dw = target_w - entry.current_w;
    let dh = target_h - entry.current_h;

    if dx.abs() > 0.5 || dy.abs() > 0.5 || dw.abs() > 0.5 || dh.abs() > 0.5 {
        entry.current_x += dx * t;
        entry.current_y += dy * t;
        entry.current_w += dw * t;
        entry.current_h += dh * t;
        true
    } else {
        entry.current_x = target_x;
        entry.current_y = target_y;
        entry.current_w = target_w;
        entry.current_h = target_h;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_layout_preserves_aspect_and_count() {
        let windows = vec![(1u32, 10, 20, 800, 400), (2, 50, 60, 300, 600)];
        let entries = build_expose_entries(1920.0, 1080.0, 20.0, &windows);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert_eq!(entries[0].orig_x, 10.0);
        assert_eq!(entries[0].orig_y, 20.0);
        assert!(entries[0].target_w > entries[0].target_h);
        assert!(entries[1].target_h > entries[1].target_w);
    }

    #[test]
    fn expose_layout_is_id_type_agnostic() {
        let windows = vec![(u64::MAX, 0, 0, 640, 480)];
        let entries = build_expose_entries(1920.0, 1080.0, 20.0, &windows);
        assert_eq!(entries[0].id, u64::MAX);
    }

    #[test]
    fn expose_layout_clamps_extreme_gap_before_cells_can_invert() {
        let windows = vec![(1u32, 0, 0, 640, 480)];
        for gap in [512.0, f32::INFINITY, f32::NAN, -20.0] {
            let entries = build_expose_entries(1280.0, 720.0, gap, &windows);
            let entry = &entries[0];
            assert!(entry.target_x.is_finite() && entry.target_y.is_finite());
            assert!(entry.target_w.is_finite() && entry.target_w >= 1.0);
            assert!(entry.target_h.is_finite() && entry.target_h >= 1.0);
            assert!(entry.target_x >= 0.0 && entry.target_y >= 0.0);
            assert!(entry.target_x + entry.target_w <= 1280.0);
            assert!(entry.target_y + entry.target_h <= 720.0);
        }
    }

    #[test]
    fn expose_move_with_no_selection_starts_at_first_entry() {
        for dir in [
            ExposeNavDirection::Left,
            ExposeNavDirection::Right,
            ExposeNavDirection::Up,
            ExposeNavDirection::Down,
        ] {
            assert_eq!(move_expose_selection(None, dir, 5, 2), Some(0));
        }
        assert_eq!(
            move_expose_selection(None, ExposeNavDirection::Right, 0, 3),
            None
        );
    }

    #[test]
    fn expose_move_left_right_clamps_at_row_edges() {
        // cols=2, len=5: grid is `0 1 / 2 3 / 4`.
        assert_eq!(
            move_expose_selection(Some(0), ExposeNavDirection::Left, 5, 2),
            Some(0)
        );
        assert_eq!(
            move_expose_selection(Some(2), ExposeNavDirection::Left, 5, 2),
            Some(2)
        );
        assert_eq!(
            move_expose_selection(Some(1), ExposeNavDirection::Left, 5, 2),
            Some(0)
        );
        assert_eq!(
            move_expose_selection(Some(0), ExposeNavDirection::Right, 5, 2),
            Some(1)
        );
        // Last column of a row stops instead of wrapping to the next row.
        assert_eq!(
            move_expose_selection(Some(1), ExposeNavDirection::Right, 5, 2),
            Some(1)
        );
        assert_eq!(
            move_expose_selection(Some(3), ExposeNavDirection::Right, 5, 2),
            Some(3)
        );
        // Right from the final entry never leaves the grid.
        assert_eq!(
            move_expose_selection(Some(4), ExposeNavDirection::Right, 5, 2),
            Some(4)
        );
    }

    #[test]
    fn expose_move_up_down_clamps_at_grid_edges() {
        // cols=2, len=5: grid is `0 1 / 2 3 / 4`.
        assert_eq!(
            move_expose_selection(Some(1), ExposeNavDirection::Up, 5, 2),
            Some(1)
        );
        assert_eq!(
            move_expose_selection(Some(3), ExposeNavDirection::Up, 5, 2),
            Some(1)
        );
        assert_eq!(
            move_expose_selection(Some(2), ExposeNavDirection::Down, 5, 2),
            Some(4)
        );
        // The short last row has no cell under 3 or 4, so Down stays put.
        assert_eq!(
            move_expose_selection(Some(3), ExposeNavDirection::Down, 5, 2),
            Some(3)
        );
        assert_eq!(
            move_expose_selection(Some(4), ExposeNavDirection::Down, 5, 2),
            Some(4)
        );
    }

    #[test]
    fn expose_move_single_entry_stays_put() {
        for dir in [
            ExposeNavDirection::Left,
            ExposeNavDirection::Right,
            ExposeNavDirection::Up,
            ExposeNavDirection::Down,
        ] {
            assert_eq!(move_expose_selection(Some(0), dir, 1, 1), Some(0));
        }
    }

    #[test]
    fn expose_move_treats_zero_cols_as_one_column() {
        assert_eq!(
            move_expose_selection(Some(0), ExposeNavDirection::Down, 3, 0),
            Some(1)
        );
        assert_eq!(
            move_expose_selection(Some(1), ExposeNavDirection::Right, 3, 0),
            Some(1)
        );
    }

    #[test]
    fn expose_grid_cols_matches_build_layout() {
        for (n, screen_w, screen_h) in [
            (1usize, 1920.0f32, 1080.0f32),
            (2, 1920.0, 1080.0),
            (3, 1920.0, 1080.0),
            (5, 1920.0, 1080.0),
            (7, 1080.0, 1920.0),
            (11, 3440.0, 1440.0),
        ] {
            // Identical windows keep every cell of a row/column at the same
            // target coordinate, so the grid shape is readable off the targets.
            let windows: Vec<(u32, i32, i32, u32, u32)> =
                (0..n).map(|i| (i as u32, 0, 0, 100, 100)).collect();
            let entries = build_expose_entries(screen_w, screen_h, 20.0, &windows);
            let cols = expose_grid_cols(n, screen_w, screen_h);

            let first_row = entries
                .iter()
                .filter(|entry| entry.target_y == entries[0].target_y)
                .count();
            assert_eq!(first_row as u32, cols.min(n as u32));

            let mut row_ys: Vec<f32> = entries.iter().map(|entry| entry.target_y).collect();
            row_ys.sort_by(f32::total_cmp);
            row_ys.dedup();
            assert_eq!(row_ys.len() as u32, (n as u32).div_ceil(cols));

            // Vertical moves stay in the column: entry i and i±cols share target_x.
            if n > cols as usize {
                let down = move_expose_selection(Some(0), ExposeNavDirection::Down, n, cols)
                    .expect("non-empty grid keeps a selection");
                assert_eq!(entries[0].target_x, entries[down].target_x);
            }
        }
    }

    #[test]
    fn expose_tick_moves_towards_target_and_fades_in() {
        let windows = vec![(1u32, 0, 0, 800, 400)];
        let mut entries = build_expose_entries(1920.0, 1080.0, 20.0, &windows);
        let mut opacity = 0.0;

        let result = tick_expose_entries(&mut entries, true, &mut opacity, 1.0 / 60.0);

        assert!(result.keep_animating);
        assert!(!result.clear_entries);
        assert!(opacity > 0.0);
        assert!(entries[0].current_x > 0.0 || entries[0].current_y > 0.0);
    }

    #[test]
    fn expose_tick_requests_clear_after_fade_out_finishes() {
        let mut entries = vec![ExposeEntry {
            id: 1u32,
            orig_x: 0.0,
            orig_y: 0.0,
            orig_w: 100.0,
            orig_h: 100.0,
            target_x: 0.0,
            target_y: 0.0,
            target_w: 100.0,
            target_h: 100.0,
            current_x: 0.0,
            current_y: 0.0,
            current_w: 100.0,
            current_h: 100.0,
            is_hovered: false,
        }];
        let mut opacity = 0.01;

        let result = tick_expose_entries(&mut entries, false, &mut opacity, 1.0 / 60.0);

        assert!(!result.keep_animating);
        assert!(result.clear_entries);
    }

    #[test]
    fn expose_fade_out_accelerates_once_geometry_converges() {
        // Entry still far from its origin: slow fade while windows fly back.
        let mut moving = vec![ExposeEntry {
            id: 7u32,
            orig_x: 0.0,
            orig_y: 0.0,
            orig_w: 100.0,
            orig_h: 100.0,
            target_x: 500.0,
            target_y: 500.0,
            target_w: 200.0,
            target_h: 200.0,
            current_x: 500.0,
            current_y: 500.0,
            current_w: 200.0,
            current_h: 200.0,
            is_hovered: false,
        }];
        let mut slow_opacity = 1.0;
        tick_expose_entries(&mut moving, false, &mut slow_opacity, 1.0 / 60.0);

        let mut settled = vec![ExposeEntry {
            id: 7u32,
            orig_x: 0.0,
            orig_y: 0.0,
            orig_w: 100.0,
            orig_h: 100.0,
            target_x: 500.0,
            target_y: 500.0,
            target_w: 200.0,
            target_h: 200.0,
            current_x: 0.0,
            current_y: 0.0,
            current_w: 100.0,
            current_h: 100.0,
            is_hovered: false,
        }];
        let mut fast_opacity = 1.0;
        tick_expose_entries(&mut settled, false, &mut fast_opacity, 1.0 / 60.0);

        assert!(
            fast_opacity < slow_opacity,
            "settled geometry should fade faster ({fast_opacity} vs {slow_opacity})"
        );
    }
}
