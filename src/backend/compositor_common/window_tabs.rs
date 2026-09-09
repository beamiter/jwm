//! Geometry for the window tab bar.
//!
//! The bar is a strip the window manager reserves across the top of a
//! monitor's tiling area, holding one equal-width cell per tiled window on
//! that monitor. The window manager reserves the strip and hit-tests clicks
//! against it, and the two compositors draw it; all three derive every
//! rectangle from this module, so a click lands on the cell it looks like it
//! lands on and the reserved strip is exactly the strip that gets painted.
//!
//! There are two families of rectangle here, and the split is deliberate:
//!
//! * [`tab_rect`] is the *slot*, the share of the reserved strip that belongs
//!   to one window. Slots tile the whole band edge to edge, which is what
//!   [`tab_at`] hit-tests, so no pixel of a strip the layout paid for is dead.
//! * [`track_rect`] and [`cell_rect`] are what actually gets *painted*: a
//!   rounded track inset from the band, and inside it one rounded cell per
//!   slot with a gap between neighbours — the segmented control the rest of
//!   JWM's self-drawn UI (`ui_theme`) is styled like. Their tones come from
//!   the active `UiTheme` palette rather than from any tab-specific color, so
//!   the strip is frosted glass or a Material card exactly when every other
//!   JWM surface is.
//!
//! Painting inside the slot instead of over it is what lets the cells be pills
//! with air around them while a click anywhere in the band — the gaps and the
//! inset margins included — still lands on the window it looks nearest to.
//!
//! The strip's other half here is the dwell tooltip: when the cells shrink
//! far enough that their titles ellipsize, resting the pointer on one floats
//! the full title in a chip off the strip. [`TooltipDwell`] keeps the
//! rest-then-show state and [`tooltip_rect`] the chip placement, both pure,
//! so the two compositors cannot drift apart on either.

use std::time::{Duration, Instant};

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

/// Margin between the reserved band and the painted track, sideways and
/// vertically. The band is what the layout gave up; the track is what the eye
/// sees, and leaving air around it is what makes it read as one control
/// floating in the work area rather than a second status bar.
pub const TRACK_INSET_X: f32 = 8.0;
pub const TRACK_INSET_Y: f32 = 2.0;
/// Margin between the track and a cell inside it.
pub const CELL_INSET_X: f32 = 2.0;
pub const CELL_INSET_Y: f32 = 2.0;
/// Gap between two neighbouring cells.
pub const CELL_GAP: f32 = 4.0;

/// Air around the tooltip chip's one line of text.
pub const TOOLTIP_PAD_X: f32 = 10.0;
pub const TOOLTIP_PAD_Y: f32 = 5.0;
/// What keeps the chip floating off the strip rather than on it.
pub const TOOLTIP_GAP: f32 = 6.0;
/// Pixel budget for the chip's line: a title longer than this is ellipsized
/// again, this time against the screen instead of against the cell.
pub const TOOLTIP_MAX_TEXT_WIDTH: u32 = 480;
/// How long the pointer must rest on one truncated cell before the chip with
/// its full title floats up. The dwell is information policy, not animation —
/// it applies with motion effects on or off — so it lives with the geometry
/// everyone shares rather than in either compositor's animation kit.
pub const TOOLTIP_DWELL: Duration = Duration::from_millis(500);

/// The margins the track keeps from the band it is painted in. A band too
/// short or narrow for the constants keeps a proportional margin instead,
/// rather than losing its track to an inverted rectangle.
fn track_insets(bar: Rect) -> (f32, f32) {
    let [_, _, w, h] = bar;
    (TRACK_INSET_X.min(w * 0.25), TRACK_INSET_Y.min(h * 0.25))
}

/// The painted track: the reserved band minus its margins. `None` when the
/// band is too small to hold one, in which case the compositors paint nothing
/// and the strip is simply a bit of empty work area.
#[must_use]
pub fn track_rect(bar: Rect) -> Option<Rect> {
    if !bar_is_drawable(bar) {
        return None;
    }
    let [x, y, w, h] = bar;
    let (inset_x, inset_y) = track_insets(bar);
    let track = [
        x + inset_x,
        y + inset_y,
        w - 2.0 * inset_x,
        h - 2.0 * inset_y,
    ];
    bar_is_drawable(track).then_some(track)
}

/// The painted cell for `index`: its slot, less half a gap towards each
/// neighbour and the track's own margin on the outside.
///
/// Anchoring on the slot rather than on a second partition of the track is
/// what guarantees the nesting [`tab_at`] depends on — a cell painted a
/// fraction of a pixel into its neighbour's slot would be a cell a click on it
/// focuses the wrong window from.
#[must_use]
pub fn cell_rect(bar: Rect, count: usize, index: usize) -> Option<Rect> {
    let [slot_x, _, slot_w, _] = tab_rect(bar, count, index)?;
    let [_, track_y, _, track_h] = track_rect(bar)?;
    let (inset_x, _) = track_insets(bar);

    // Outer edges follow the track; inner edges share a gap with the
    // neighbour they face.
    let outer = inset_x + CELL_INSET_X;
    let left = if index == 0 { outer } else { CELL_GAP * 0.5 };
    let right = if index + 1 == count {
        outer
    } else {
        CELL_GAP * 0.5
    };
    // A slot too narrow for both margins keeps half its width regardless.
    let margins = left + right;
    let scale = if margins > slot_w * 0.5 {
        slot_w * 0.5 / margins
    } else {
        1.0
    };
    let (left, right) = (left * scale, right * scale);

    let inset_y = CELL_INSET_Y.min(track_h * 0.25);
    let cell = [
        slot_x + left,
        track_y + inset_y,
        slot_w - left - right,
        track_h - 2.0 * inset_y,
    ];
    bar_is_drawable(cell).then_some(cell)
}

/// Corner radius that turns a rectangle this tall into a pill.
#[must_use]
pub fn pill_radius(height: f32) -> f32 {
    if height.is_finite() {
        (height * 0.5).max(0.0)
    } else {
        0.0
    }
}

/// Point size for a title in a cell this tall.
///
/// The strip's height is configurable, so the type has to follow it: a fixed
/// system-UI size would overflow the 28px default outright. The fraction
/// leaves room for the ascender, the descender and the two pixels the
/// rasterizer pads with, so the texture is always shorter than the cell that
/// centres it.
#[must_use]
pub fn title_font_size(cell_height: f32) -> f32 {
    if cell_height.is_finite() {
        (cell_height * 0.58).clamp(8.0, 22.0)
    } else {
        DEFAULT_TITLE_FONT_SIZE
    }
}

/// Used when the cell height is not a finite number.
pub const DEFAULT_TITLE_FONT_SIZE: f32 = 12.0;

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

/// Which tab of which group contains `(px, py)`, if any, as
/// `(group_index, tab_index)`. The compositors keep the answer as their own
/// hover state — putting it in [`TabGroup`] would make every motion event
/// rebuild the baked title textures — so this hit test takes the whole slice
/// and walks each group with [`tab_at`].
#[must_use]
pub fn tab_hover_at(groups: &[TabGroup], px: f32, py: f32) -> Option<(usize, usize)> {
    groups.iter().enumerate().find_map(|(group_index, group)| {
        tab_at(group.bar, group.tabs.len(), px, py).map(|tab_index| (group_index, tab_index))
    })
}

/// Which cell contains `(px, py)`, if any. Walks the same partition
/// [`tab_rect`] produces rather than recomputing an index from the fraction,
/// so the hit test cannot round to a different cell than the one drawn.
#[must_use]
pub fn tab_at(bar: Rect, count: usize, px: f32, py: f32) -> Option<usize> {
    // A NaN coordinate fails every comparison, so the bounds test below
    // would let it through and the walk would fall off the end — reporting
    // a hover on the *last* tab of the first group, for a pointer that is
    // nowhere. `bar_is_drawable` guards the rectangle's half of the same
    // input; this is the point's half.
    if count == 0 || !bar_is_drawable(bar) || !px.is_finite() || !py.is_finite() {
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

/// Dwell state for the tab-strip tooltip: which cell the pointer is resting
/// on, since when, and whether the chip is up.
///
/// The tracker is pure time and keys: each frame the compositor feeds it the
/// hovered cell and whether that cell's title was ellipsized at the last
/// title refresh, and it answers whether the chip is up and whether frames
/// must keep coming. Like the hover state, it lives outside [`TabGroup`] so
/// following the pointer never rebuilds the baked title textures.
#[derive(Clone, Copy, Debug, Default)]
pub struct TooltipDwell {
    key: Option<(usize, usize)>,
    eligible: bool,
    since: Option<Instant>,
    visible: bool,
}

impl TooltipDwell {
    /// Advance to `now`: `hover` is the cell under the pointer, `eligible`
    /// whether its title was ellipsized at the last title refresh. Returns
    /// whether the chip is up this frame.
    ///
    /// Moving to another cell — or off the strip — restarts the rest and
    /// drops the chip on the spot: the dwell is information policy, not an
    /// animation, so nothing eases out. A cell whose title fits whole never
    /// raises a chip at all; one that stops being truncated under a resting
    /// pointer (a retitle, a relayout) drops it the same frame.
    pub fn advance(&mut self, now: Instant, hover: Option<(usize, usize)>, eligible: bool) -> bool {
        if self.key != hover {
            self.key = hover;
            self.since = hover.map(|_| now);
            self.visible = false;
        }
        self.eligible = eligible;
        self.visible = eligible
            && hover.is_some()
            && self
                .since
                .is_some_and(|since| now.saturating_duration_since(since) >= TOOLTIP_DWELL);
        self.visible
    }

    /// The cell the chip is up for, if it is up.
    #[must_use]
    pub fn visible_key(&self) -> Option<(usize, usize)> {
        if self.visible { self.key } else { None }
    }

    /// Whether a chip is due but not up yet. The compositor keeps
    /// `needs_render` set while this holds: the dwell lapses on a clock, not
    /// on an event, so an otherwise idle screen must keep drawing frames or
    /// the chip appears only when something else happens to wake the
    /// renderer.
    #[must_use]
    pub fn needs_frame(&self) -> bool {
        self.eligible && self.key.is_some() && !self.visible
    }
}

/// Where the tooltip chip for `cell` goes: centred on it, clamped onto the
/// screen sideways, floating [`TOOLTIP_GAP`] above the strip — or the same
/// gap below it when the strip touches the screen's top edge and the chip
/// would clip. Either way the gap keeps the chip off the band, so it can
/// never cover the cell whose clicks must still land. `None` for degenerate
/// input, matching [`track_rect`]'s rule that a rectangle nobody can draw is
/// nobody's to paint.
#[must_use]
pub fn tooltip_rect(
    bar: Rect,
    cell: Rect,
    chip_w: f32,
    chip_h: f32,
    screen_w: f32,
    screen_h: f32,
) -> Option<Rect> {
    if !bar_is_drawable(bar)
        || !bar_is_drawable(cell)
        || [chip_w, chip_h, screen_w, screen_h]
            .into_iter()
            .any(|v| !v.is_finite() || v <= 0.0)
    {
        return None;
    }
    let [cx, _, cw, _] = cell;
    let [_, by, _, bh] = bar;
    let x = (cx + cw * 0.5 - chip_w * 0.5).clamp(0.0, (screen_w - chip_w).max(0.0));
    let above = by - TOOLTIP_GAP - chip_h;
    let y = if above >= 0.0 {
        above
    } else {
        // A strip on the screen's top edge has no room overhead; the chip
        // hangs below the band instead of clipping.
        by + bh + TOOLTIP_GAP
    };
    // Last resort for a screen shorter than the chip: keep it on the screen
    // even where that means covering the strip.
    let y = y.clamp(0.0, (screen_h - chip_h).max(0.0));
    Some([x, y, chip_w, chip_h])
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
    fn a_pointer_that_is_nowhere_hits_nothing() {
        // NaN loses every comparison, so an unguarded hit test would walk
        // past the last cell and answer `Some(count - 1)`.
        let [x, y, ..] = BAR;
        for (px, py) in [
            (f32::NAN, y + 1.0),
            (x + 1.0, f32::NAN),
            (f32::NAN, f32::NAN),
            (f32::INFINITY, y + 1.0),
            (x + 1.0, f32::NEG_INFINITY),
        ] {
            assert_eq!(tab_at(BAR, 3, px, py), None, "({px}, {py})");
        }

        let groups = vec![TabGroup {
            bar: BAR,
            tabs: vec![Tab {
                title: "only".to_string(),
                active: true,
            }],
        }];
        assert_eq!(tab_hover_at(&groups, f32::NAN, f32::NAN), None);
    }

    #[test]
    fn hover_hit_testing_spans_every_group() {
        let group = |bar: Rect, count: usize| TabGroup {
            bar,
            tabs: (0..count)
                .map(|index| Tab {
                    title: format!("tab {index}"),
                    active: index == 0,
                })
                .collect(),
        };
        let groups = vec![
            group([0.0, 0.0, 600.0, 28.0], 3),
            group([0.0, 100.0, 400.0, 28.0], 2),
        ];

        // A hit in the first group and in the second, edges included.
        assert_eq!(tab_hover_at(&groups, 10.0, 10.0), Some((0, 0)));
        assert_eq!(tab_hover_at(&groups, 550.0, 10.0), Some((0, 2)));
        assert_eq!(tab_hover_at(&groups, 399.0, 110.0), Some((1, 1)));

        // The gap between the bars, past the second bar's end, and an empty
        // slice all hit nothing.
        assert_eq!(tab_hover_at(&groups, 10.0, 50.0), None);
        assert_eq!(tab_hover_at(&groups, 500.0, 110.0), None);
        assert_eq!(tab_hover_at(&[], 10.0, 10.0), None);
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

    #[test]
    fn the_painted_track_stays_inside_the_reserved_band() {
        let [tx, ty, tw, th] = track_rect(BAR).expect("a 28px band has room for a track");
        assert!(tx > BAR[0] && ty > BAR[1]);
        assert!(tx + tw < BAR[0] + BAR[2]);
        assert!(ty + th < BAR[1] + BAR[3]);
        assert_eq!(track_rect([100.0, 40.0, 900.0, 0.0]), None);
        assert_eq!(track_rect([f32::NAN, 40.0, 900.0, 28.0]), None);
    }

    /// The two families must not drift: a painted cell that poked out of its
    /// slot would sit under the neighbour a click there focuses.
    #[test]
    fn every_painted_cell_sits_inside_the_slot_that_hit_tests_it() {
        for count in 1..=9usize {
            for index in 0..count {
                let [sx, sy, sw, sh] = tab_rect(BAR, count, index).expect("slot in range");
                let [cx, cy, cw, ch] = cell_rect(BAR, count, index).expect("cell in range");
                assert!(cx >= sx && cx + cw <= sx + sw, "count={count} cell {index}");
                assert!(cy >= sy && cy + ch <= sy + sh, "count={count} cell {index}");
                assert!(cw > 0.0 && ch > 0.0);
                // The cell's centre is what the title is drawn around, so it
                // has to be the slot a click there resolves to.
                assert_eq!(
                    tab_at(BAR, count, cx + cw * 0.5, cy + ch * 0.5),
                    Some(index)
                );
            }
            assert_eq!(cell_rect(BAR, count, count), None);
        }
    }

    #[test]
    fn neighbouring_cells_never_touch() {
        let count = 5;
        for index in 1..count {
            let [px, _, pw, _] = cell_rect(BAR, count, index - 1).expect("cell in range");
            let [x, ..] = cell_rect(BAR, count, index).expect("cell in range");
            assert!(x - (px + pw) > 0.0, "cells {} and {index} touch", index - 1);
        }
    }

    /// A band narrow enough that the constants would invert it still yields a
    /// drawable track and cells, or none at all — never a negative rectangle.
    #[test]
    fn a_cramped_band_degrades_instead_of_inverting() {
        for bar in [
            [0.0, 0.0, 12.0, 6.0],
            [0.0, 0.0, 40.0, 3.0],
            [0.0, 0.0, 1.0, 1.0],
        ] {
            if let Some([_, _, w, h]) = track_rect(bar) {
                assert!(w > 0.0 && h > 0.0, "track {bar:?}");
            }
            for index in 0..3 {
                if let Some([_, _, w, h]) = cell_rect(bar, 3, index) {
                    assert!(w > 0.0 && h > 0.0, "cell {index} of {bar:?}");
                }
            }
        }
    }

    #[test]
    fn a_pill_is_exactly_half_as_round_as_it_is_tall() {
        assert_eq!(pill_radius(22.0), 11.0);
        assert_eq!(pill_radius(0.0), 0.0);
        assert_eq!(pill_radius(-4.0), 0.0);
        assert_eq!(pill_radius(f32::NAN), 0.0);
    }

    /// Whatever the configured bar height, the title texture the rasterizer
    /// produces has to be shorter than the cell that centres it, or it spills
    /// over the windows below.
    #[test]
    fn titles_are_sized_to_fit_the_cell_they_are_centred_in() {
        for configured in [8.0, 20.0, 28.0, 44.0, 96.0, 256.0] {
            let bar = [0.0, 0.0, 800.0, bar_height(configured)];
            let Some([_, _, _, cell_h]) = cell_rect(bar, 3, 1) else {
                continue;
            };
            let size = title_font_size(cell_h);
            // What `render_ui_text_to_rgba` produces: a line box of roughly
            // 1.25x the pixel size, plus its two-pixel pad top and bottom.
            let texture_h = size * 1.25 + 4.0;
            assert!(
                texture_h <= cell_h || size <= 8.0,
                "a {configured}px bar leaves a {cell_h}px cell holding {texture_h}px of text"
            );
        }
        assert_eq!(title_font_size(f32::NAN), DEFAULT_TITLE_FONT_SIZE);
    }

    #[test]
    fn a_rested_pointer_raises_the_chip_exactly_at_the_dwell() {
        let mut dwell = TooltipDwell::default();
        let t0 = Instant::now();

        // The frame the pointer lands consumes no rest: the chip is not up,
        // but frames must keep coming or an idle screen sleeps through the
        // moment it is due.
        assert!(!dwell.advance(t0, Some((0, 1)), true));
        assert!(dwell.needs_frame());
        assert_eq!(dwell.visible_key(), None);

        assert!(!dwell.advance(
            t0 + TOOLTIP_DWELL - Duration::from_millis(1),
            Some((0, 1)),
            true
        ));
        assert!(dwell.needs_frame());

        assert!(dwell.advance(t0 + TOOLTIP_DWELL, Some((0, 1)), true));
        assert_eq!(dwell.visible_key(), Some((0, 1)));
        // Up is up: nothing about the dwell still needs the renderer.
        assert!(!dwell.needs_frame());

        // Staying put keeps the chip; a clock tick that runs backwards
        // saturates instead of panicking — at that instant the rest simply
        // has not lapsed yet.
        assert!(dwell.advance(t0 + TOOLTIP_DWELL * 4, Some((0, 1)), true));
        assert!(!dwell.advance(t0 - Duration::from_millis(1), Some((0, 1)), true));
    }

    #[test]
    fn moving_to_another_cell_restarts_the_rest_there() {
        let mut dwell = TooltipDwell::default();
        let t0 = Instant::now();
        dwell.advance(t0, Some((0, 0)), true);
        let t1 = t0 + TOOLTIP_DWELL - Duration::from_millis(1);
        assert!(!dwell.advance(t1, Some((0, 0)), true));

        // A moment before the first cell's dwell would have lapsed, the
        // pointer moves: the new cell's clock starts from the move, so the
        // old cell's almost-mature rest buys nothing.
        assert!(!dwell.advance(t1, Some((0, 1)), true));
        assert!(dwell.needs_frame());
        assert!(!dwell.advance(
            t1 + TOOLTIP_DWELL - Duration::from_millis(1),
            Some((0, 1)),
            true
        ));
        assert!(dwell.advance(t1 + TOOLTIP_DWELL, Some((0, 1)), true));
        assert_eq!(dwell.visible_key(), Some((0, 1)));
    }

    #[test]
    fn leaving_the_strip_drops_the_chip_the_same_frame() {
        let mut dwell = TooltipDwell::default();
        let t0 = Instant::now();
        dwell.advance(t0, Some((0, 1)), true);
        assert!(dwell.advance(t0 + TOOLTIP_DWELL, Some((0, 1)), true));

        assert!(!dwell.advance(t0 + TOOLTIP_DWELL * 2, None, false));
        assert_eq!(dwell.visible_key(), None);
        // Nothing is pending either: hovering nothing must never spin the
        // renderer.
        assert!(!dwell.needs_frame());

        // Coming back to the same cell is a new rest, not a resumption.
        assert!(!dwell.advance(t0 + TOOLTIP_DWELL * 3, Some((0, 1)), true));
        assert!(dwell.needs_frame());
    }

    #[test]
    fn a_cell_whose_title_fits_never_raises_a_chip() {
        let mut dwell = TooltipDwell::default();
        let t0 = Instant::now();

        // However long the pointer rests on an untruncated cell, no chip —
        // and no frame pumping, since none is ever due.
        assert!(!dwell.advance(t0, Some((0, 0)), false));
        assert!(!dwell.needs_frame());
        assert!(!dwell.advance(t0 + TOOLTIP_DWELL * 10, Some((0, 0)), false));
        assert_eq!(dwell.visible_key(), None);
        assert!(!dwell.needs_frame());

        // A title refresh can make a resting cell stop being truncated; the
        // chip drops the same frame rather than outliving its reason.
        let mut dwell = TooltipDwell::default();
        assert!(!dwell.advance(t0, Some((0, 0)), true));
        assert!(dwell.advance(t0 + TOOLTIP_DWELL, Some((0, 0)), true));
        assert!(!dwell.advance(t0 + TOOLTIP_DWELL * 2, Some((0, 0)), false));
        assert_eq!(dwell.visible_key(), None);
    }

    /// The chip centres on its cell but never leaves the screen sideways.
    #[test]
    fn the_chip_centres_on_its_cell_and_clamps_to_the_screen() {
        // A strip well below the top edge: the chip floats above it.
        let bar: Rect = [40.0, 100.0, 900.0, 28.0];
        let (chip_w, chip_h) = (200.0, 30.0);
        let (screen_w, screen_h) = (1920.0, 1080.0);

        // Middle cell: centred.
        let cell = cell_rect(bar, 3, 1).expect("cell in range");
        let [x, y, w, h] =
            tooltip_rect(bar, cell, chip_w, chip_h, screen_w, screen_h).expect("drawable chip");
        let centre = cell[0] + cell[2] * 0.5;
        assert!((x + w * 0.5 - centre).abs() < 1e-3);
        assert_eq!(y, bar[1] - TOOLTIP_GAP - chip_h);
        assert_eq!([w, h], [chip_w, chip_h]);
        // Floating means off the band: the gap sits between chip and cell.
        assert!(y + h <= cell[1]);

        // Leftmost cell of a bar at the screen's left edge: centred would
        // clip, so the chip pins to the edge instead.
        let edge_bar: Rect = [0.0, 100.0, 60.0, 28.0];
        let cell = cell_rect(edge_bar, 1, 0).expect("cell in range");
        let [x, ..] =
            tooltip_rect(edge_bar, cell, chip_w, chip_h, screen_w, screen_h).expect("chip");
        assert_eq!(x, 0.0);

        // Same at the right edge.
        let edge_bar: Rect = [screen_w - 60.0, 100.0, 60.0, 28.0];
        let cell = cell_rect(edge_bar, 1, 0).expect("cell in range");
        let [x, _, w, _] =
            tooltip_rect(edge_bar, cell, chip_w, chip_h, screen_w, screen_h).expect("chip");
        assert_eq!(x + w, screen_w);
    }

    /// A strip on the screen's top edge has no room overhead; the chip hangs
    /// below the band rather than clipping, and still clears the cell.
    #[test]
    fn a_strip_on_the_top_edge_flips_the_chip_below_it() {
        let bar: Rect = [0.0, 0.0, 900.0, 28.0];
        let (chip_w, chip_h) = (200.0, 30.0);
        let cell = cell_rect(bar, 3, 1).expect("cell in range");
        let [x, y, w, h] =
            tooltip_rect(bar, cell, chip_w, chip_h, 1920.0, 1080.0).expect("drawable chip");
        assert_eq!(y, bar[1] + bar[3] + TOOLTIP_GAP);
        assert!(y >= cell[1] + cell[3], "the chip must not cover its cell");
        assert!(y + h <= 1080.0 && x >= 0.0 && x + w <= 1920.0);

        // A strip one pixel below the edge fits a one-pixel-shorter gap only
        // if the chip is shorter than the room: not here, so this flips too.
        let bar: Rect = [0.0, 1.0, 900.0, 28.0];
        let [_, y, ..] = tooltip_rect(bar, cell, chip_w, chip_h, 1920.0, 1080.0).expect("chip");
        assert_eq!(y, bar[1] + bar[3] + TOOLTIP_GAP);

        // A screen shorter than the chip keeps it on the screen as a last
        // resort instead of returning an unpaintable rectangle.
        let [_, y, _, h] = tooltip_rect(bar, cell, chip_w, chip_h, 1920.0, 20.0).expect("chip");
        assert_eq!([y, h], [0.0, chip_h]);
    }

    #[test]
    fn degenerate_chips_and_screens_have_no_placement() {
        let bar: Rect = [40.0, 100.0, 900.0, 28.0];
        let cell = cell_rect(bar, 3, 1).expect("cell in range");
        for (chip_w, chip_h, screen_w, screen_h) in [
            (0.0, 30.0, 1920.0, 1080.0),
            (200.0, -1.0, 1920.0, 1080.0),
            (f32::NAN, 30.0, 1920.0, 1080.0),
            (200.0, f32::INFINITY, 1920.0, 1080.0),
            (200.0, 30.0, 0.0, 1080.0),
            (200.0, 30.0, 1920.0, f32::NAN),
        ] {
            assert_eq!(
                tooltip_rect(bar, cell, chip_w, chip_h, screen_w, screen_h),
                None,
                "chip {chip_w}x{chip_h} on {screen_w}x{screen_h}"
            );
        }
        assert_eq!(
            tooltip_rect(
                [f32::NAN, 0.0, 900.0, 28.0],
                cell,
                200.0,
                30.0,
                1920.0,
                1080.0
            ),
            None
        );
    }
}
