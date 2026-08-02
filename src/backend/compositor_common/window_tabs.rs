//! Geometry for the window tab bar.
//!
//! The bar is a strip the window manager reserves across the top of a
//! monitor's tiling area, holding one equal-width cell per tiled window on
//! that monitor. The window manager reserves the strip and hit-tests clicks
//! against it, and the two compositors draw it; all three derive every
//! rectangle from this module, so a click lands on the cell it looks like it
//! lands on and the reserved strip is exactly the strip that gets painted.

/// A rectangle in screen pixels: `[x, y, w, h]`.
pub type Rect = [f32; 4];

/// One cell of the bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    /// The window's title, drawn centred in the cell.
    pub title: String,
    /// Whether this is the focused window.
    pub active: bool,
}

/// One monitor's tab bar: the strip and what is in it.
#[derive(Clone, Debug, PartialEq)]
pub struct TabGroup {
    /// The strip the window manager reserved, in screen pixels.
    pub bar: Rect,
    /// Cells, left to right, in the monitor's client order.
    pub tabs: Vec<Tab>,
}

/// Below this many windows there is nothing to choose between, so no strip is
/// reserved and none is drawn. Both sides test against this constant rather
/// than spelling out `> 1`, because a strip reserved without being painted
/// leaves a band of wallpaper no window can reach.
pub const MIN_TABS: usize = 2;

/// Bar height bounds. The lower one keeps a configured `0` from reserving a
/// zero-height strip the renderer would then skip; the upper one keeps a typo
/// from eating the screen.
pub const MIN_BAR_HEIGHT: f32 = 1.0;
pub const MAX_BAR_HEIGHT: f32 = 256.0;
/// Used when the configured height is not a finite number.
pub const DEFAULT_BAR_HEIGHT: f32 = 24.0;

/// Resolve `behavior.tab_bar_height` into the height everyone uses. The window
/// manager reserves this many pixels and the compositors paint exactly that
/// many, so the clamp cannot live in only one of them.
#[must_use]
pub fn bar_height(configured: f32) -> f32 {
    if configured.is_finite() {
        configured.clamp(MIN_BAR_HEIGHT, MAX_BAR_HEIGHT)
    } else {
        DEFAULT_BAR_HEIGHT
    }
}

/// Whether `count` windows are worth a bar.
#[must_use]
pub fn wants_bar(count: usize) -> bool {
    count >= MIN_TABS
}

/// How much of a cell its title may use: the cell minus breathing room, never
/// below a floor that still fits a couple of characters.
pub const TITLE_PADDING: f32 = 12.0;
pub const TITLE_MIN_WIDTH: f32 = 20.0;

/// Pixel budget for the title in a cell this wide.
#[must_use]
pub fn title_budget(cell_width: f32) -> u32 {
    if !cell_width.is_finite() {
        return TITLE_MIN_WIDTH as u32;
    }
    (cell_width - TITLE_PADDING).max(TITLE_MIN_WIDTH) as u32
}

fn bar_is_drawable(bar: Rect) -> bool {
    let [x, y, w, h] = bar;
    x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0
}

/// The cell for `index`. Cells tile the bar exactly: each one starts where the
/// previous ended, and the last one ends on the bar's right edge, so no seam
/// of background shows through between them.
#[must_use]
pub fn tab_rect(bar: Rect, count: usize, index: usize) -> Option<Rect> {
    if index >= count || !bar_is_drawable(bar) {
        return None;
    }
    let [x, y, w, h] = bar;
    let n = count as f32;
    let left = x + w * index as f32 / n;
    let right = x + w * (index + 1) as f32 / n;
    Some([left, y, (right - left).max(0.0), h])
}

/// Which cell contains `(px, py)`, if any. Walks the same partition
/// [`tab_rect`] produces rather than recomputing an index from the fraction,
/// so the hit test cannot round to a different cell than the one drawn.
#[must_use]
pub fn tab_at(bar: Rect, count: usize, px: f32, py: f32) -> Option<usize> {
    if count == 0 || !bar_is_drawable(bar) {
        return None;
    }
    let [x, y, w, h] = bar;
    if px < x || px > x + w || py < y || py > y + h {
        return None;
    }
    let n = count as f32;
    for index in 0..count {
        if px <= x + w * (index + 1) as f32 / n {
            return Some(index);
        }
    }
    Some(count - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: Rect = [100.0, 40.0, 900.0, 28.0];

    #[test]
    fn cells_tile_the_bar_without_seams() {
        for count in 1..=9usize {
            let mut previous_right = BAR[0];
            for index in 0..count {
                let [x, y, w, h] = tab_rect(BAR, count, index).expect("cell in range");
                assert!((x - previous_right).abs() < 1e-3, "count={count} seam");
                assert_eq!([y, h], [BAR[1], BAR[3]]);
                assert!(w > 0.0);
                previous_right = x + w;
            }
            assert!(
                (previous_right - (BAR[0] + BAR[2])).abs() < 1e-3,
                "count={count} last cell must end on the bar's right edge"
            );
        }
    }

    #[test]
    fn out_of_range_and_degenerate_bars_have_no_cells() {
        assert_eq!(tab_rect(BAR, 3, 3), None);
        assert_eq!(tab_rect(BAR, 0, 0), None);
        assert_eq!(tab_rect([100.0, 40.0, 0.0, 28.0], 2, 0), None);
        assert_eq!(tab_rect([100.0, 40.0, 900.0, 0.0], 2, 0), None);
        assert_eq!(tab_rect([f32::NAN, 40.0, 900.0, 28.0], 2, 0), None);
    }

    #[test]
    fn a_click_lands_on_the_cell_it_looks_like() {
        let count = 4;
        for index in 0..count {
            let [x, y, w, h] = tab_rect(BAR, count, index).expect("cell in range");
            for (px, py) in [
                (x + 0.5, y + 0.5),
                (x + w * 0.5, y + h * 0.5),
                (x + w - 0.5, y + h - 0.5),
            ] {
                assert_eq!(tab_at(BAR, count, px, py), Some(index), "cell {index}");
            }
        }
    }

    #[test]
    fn clicks_outside_the_bar_hit_nothing() {
        let [x, y, w, h] = BAR;
        assert_eq!(tab_at(BAR, 3, x - 1.0, y + 1.0), None);
        assert_eq!(tab_at(BAR, 3, x + w + 1.0, y + 1.0), None);
        assert_eq!(tab_at(BAR, 3, x + 1.0, y - 1.0), None);
        assert_eq!(tab_at(BAR, 3, x + 1.0, y + h + 1.0), None);
        assert_eq!(tab_at(BAR, 0, x + 1.0, y + 1.0), None);
    }

    #[test]
    fn height_is_clamped_the_same_way_everywhere() {
        assert_eq!(bar_height(28.0), 28.0);
        assert_eq!(bar_height(0.0), MIN_BAR_HEIGHT);
        assert_eq!(bar_height(-5.0), MIN_BAR_HEIGHT);
        assert_eq!(bar_height(9_000.0), MAX_BAR_HEIGHT);
        assert_eq!(bar_height(f32::NAN), DEFAULT_BAR_HEIGHT);
        assert_eq!(bar_height(f32::INFINITY), DEFAULT_BAR_HEIGHT);
    }

    #[test]
    fn a_title_never_gets_a_negative_budget() {
        assert_eq!(title_budget(200.0), 188);
        assert_eq!(title_budget(10.0), TITLE_MIN_WIDTH as u32);
        assert_eq!(title_budget(0.0), TITLE_MIN_WIDTH as u32);
        assert_eq!(title_budget(f32::NAN), TITLE_MIN_WIDTH as u32);
    }

    #[test]
    fn a_lone_window_gets_no_bar() {
        assert!(!wants_bar(0));
        assert!(!wants_bar(1));
        assert!(wants_bar(2));
    }
}
