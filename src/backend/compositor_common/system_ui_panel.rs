//! Geometry for the modal system-UI card.
//!
//! The launcher, the Shell Hub, the pickers, the calendar and the lock card
//! are all the same surface: a title, an optional search field, a list, and a
//! footer hint. Both compositors used to compute that layout inline, in two
//! ~290-line functions that had to be kept in step by hand — so every change
//! to the panel's look was two edits and a diff review.
//!
//! Everything here is pure arithmetic on the rasterized section sizes, exactly
//! like [`super::layout_strip`] is for the film strip. That is what lets the
//! X11 and Wayland renderers draw the same card, and what makes the layout
//! testable without a GL context.
//!
//! Two rules the arithmetic encodes, which are easy to lose in a draw loop:
//!
//! * the card's width only ever grows while a panel is on screen. The list is
//!   re-measured on every keystroke, so a width that tracked its content would
//!   breathe in and out under the user's typing.
//! * the selection pill's height is the *rasterizer's* line height, recovered
//!   as `(items_h - 2 * TEXT_PAD) / rows`. The list is one texture, so this is
//!   the only handle the renderer has on a row.

/// A rectangle in screen pixels: `[x, y, w, h]`.
pub(crate) type Rect = [f32; 4];

/// A rasterized section's `(width, height)` in pixels. `(0.0, 0.0)` means the
/// section is absent.
pub(crate) type Size = (f32, f32);

/// Padding between the card edge and its contents.
const PAD: f32 = 30.0;
/// Vertical breathing room between bands.
const GAP: f32 = 16.0;
/// Inset of the query text inside its field.
const QUERY_PAD: f32 = 12.0;
/// Height the query field carries over its text.
const QUERY_LEAD: f32 = 16.0;
/// Baseline offset of the query text inside its field.
const QUERY_TEXT_LEAD: f32 = 8.0;
/// Corner radius of the query field.
pub(crate) const QUERY_RADIUS: f32 = 10.0;
/// Corner radius of the selection pill.
pub(crate) const SELECTION_RADIUS: f32 = 8.0;
/// How far the selection pill bleeds into the padding on each side, so the
/// highlight reads as a row of the card rather than a box around the text.
const SELECTION_BLEED: f32 = 8.0;
/// Padding the text rasterizer adds around every texture
/// (`compositor_font::TEXT_PAD`), which has to come back out of the item block
/// before it can be divided into rows.
const TEXT_PAD: f32 = 2.0;
/// Narrowest content column. Below this a one-word panel would be a stub.
const MIN_CONTENT_W: f32 = 360.0;
/// Closest the card may come to the screen edges.
const SCREEN_MARGIN: f32 = 64.0;
/// The content column is rounded up to this step, which divides
/// [`MIN_CONTENT_W`] exactly. Between the step and the width floor the caller
/// carries, a launcher list re-measured on every keystroke stops resizing the
/// card underneath it.
const WIDTH_STEP: f32 = 40.0;
/// Thickness of the hairline between the list and the footer hint.
pub(crate) const DIVIDER_H: f32 = 1.0;
/// Width of the scroll indicator.
const SCROLLBAR_W: f32 = 3.0;
/// Shortest the scroll thumb may be drawn, so a long list still shows one.
const SCROLLBAR_MIN_THUMB: f32 = 20.0;
/// Corner radius of the scroll track and thumb: a capsule.
pub(crate) const SCROLLBAR_RADIUS: f32 = SCROLLBAR_W * 0.5;

/// Widest a card may be on this screen.
///
/// The ordinary margin is a total inset (half on either side). On a very
/// narrow nested output it wins over the usual content-width floor: keeping a
/// 360 px minimum on a 320 px output made the close edge and the query caret
/// unreachable.
#[must_use]
pub(crate) fn max_panel_width(screen_w: f32) -> f32 {
    (screen_w.max(1.0) - SCREEN_MARGIN).max(1.0)
}

/// Pixel budget available to each rasterized text line inside a card.
///
/// Renderers fit before allocating CPU and GL textures; merely clipping at
/// draw time would still allow an untrusted title or a long query to allocate
/// a texture thousands of pixels wide.
#[must_use]
pub(crate) fn max_content_width(screen_w: f32) -> u32 {
    (max_panel_width(screen_w) - 2.0 * PAD).max(0.0).floor() as u32
}

/// The query text sits inside the content column's own inset field.
#[must_use]
pub(crate) fn max_query_text_width(screen_w: f32) -> u32 {
    max_content_width(screen_w).saturating_sub((2.0 * QUERY_PAD) as u32)
}

/// Where a windowed list currently sits, for the scroll indicator.
///
/// The window manager does the windowing — it decides how many rows fit and
/// which slice to send — so the renderer has no way to know a list is scrolled
/// unless it is told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scroll {
    /// Index of the first row being shown.
    pub(crate) first: usize,
    /// How many rows are being shown.
    pub(crate) visible: usize,
    /// How many rows there are in total.
    pub(crate) total: usize,
}

impl Scroll {
    /// Whether there is anything off-screen worth drawing an indicator for.
    #[must_use]
    pub(crate) fn overflows(&self) -> bool {
        self.total > self.visible && self.visible > 0
    }
}

/// The rasterized size of each of the card's four text sections.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SectionSizes {
    pub(crate) title: Size,
    /// `(0.0, 0.0)` when the panel has no search field.
    pub(crate) query: Size,
    pub(crate) items: Size,
    pub(crate) hint: Size,
}

impl SectionSizes {
    fn query_field_h(&self) -> f32 {
        if self.query.1 > 0.0 {
            self.query.1 + QUERY_LEAD
        } else {
            0.0
        }
    }
}

/// Everything inside the card, in screen pixels.
///
/// Positions are text origins (top-left); rects are fills. Every field is
/// `None` when its section is absent, so a renderer is a straight walk down
/// this struct with no layout arithmetic of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PanelContents {
    pub(crate) title: [f32; 2],
    pub(crate) query_field: Option<Rect>,
    pub(crate) query_text: Option<[f32; 2]>,
    pub(crate) items: Option<[f32; 2]>,
    /// Height of one list row, i.e. the rasterizer's line height.
    pub(crate) row_height: f32,
    pub(crate) selection: Option<Rect>,
    /// Hairline between the list and the footer.
    pub(crate) divider: Option<Rect>,
    pub(crate) hint: Option<[f32; 2]>,
    /// Full travel of the scroll indicator, and the thumb inside it.
    pub(crate) scroll_track: Option<Rect>,
    pub(crate) scroll_thumb: Option<Rect>,
}

/// The size the card wants for `sizes`, before the open/morph spring.
///
/// `width_floor` is the widest this panel has been since it opened; the result
/// never goes under it. Pass `0.0` for a panel that should hug its content —
/// the lock card, which is centred on its own backdrop and has nothing to jitter
/// against.
#[must_use]
pub(crate) fn target_size(sizes: &SectionSizes, screen_w: f32, width_floor: f32) -> Size {
    let content = sizes
        .title
        .0
        .max(sizes.query.0 + 2.0 * QUERY_PAD)
        .max(sizes.items.0)
        .max(sizes.hint.0)
        .max(MIN_CONTENT_W);
    // Round up to the step before the floor is applied, so a panel that grows
    // by a few pixels a keystroke crosses a step at most once.
    let content = (content / WIDTH_STEP).ceil() * WIDTH_STEP;
    let width = (content + 2.0 * PAD)
        .max(width_floor)
        .min(max_panel_width(screen_w));

    let mut height = 2.0 * PAD + sizes.title.1;
    let query_field_h = sizes.query_field_h();
    if query_field_h > 0.0 {
        height += GAP + query_field_h;
    }
    if sizes.items.1 > 0.0 {
        height += GAP + sizes.items.1;
    }
    if sizes.hint.1 > 0.0 {
        height += GAP + sizes.hint.1;
    }
    (width, height)
}

/// Lay the card's contents out inside `panel`.
///
/// `panel` is the rect actually being drawn, which mid-animation is smaller
/// than [`target_size`] asked for — the contents follow the card as it opens
/// rather than being clipped by it.
#[must_use]
pub(crate) fn contents(
    panel: Rect,
    sizes: &SectionSizes,
    rows: usize,
    selected: Option<usize>,
    scroll: Option<Scroll>,
) -> PanelContents {
    let [x, y, panel_w, _] = panel;
    let inner_w = (panel_w - 2.0 * PAD).max(0.0);
    let mut out = PanelContents {
        title: [x + PAD, y + PAD],
        ..PanelContents::default()
    };

    let mut cy = y + PAD + sizes.title.1;

    let query_field_h = sizes.query_field_h();
    if query_field_h > 0.0 {
        cy += GAP;
        out.query_field = Some([x + PAD, cy, inner_w, query_field_h]);
        out.query_text = Some([x + PAD + QUERY_PAD, cy + QUERY_TEXT_LEAD]);
        cy += query_field_h;
    }

    let items_h = sizes.items.1;
    if items_h > 0.0 {
        cy += GAP;
        let items_y = cy;
        out.items = Some([x + PAD, items_y]);
        if rows > 0 {
            // The list is one texture, so a row's height is only recoverable
            // from the block: the rasterizer padded it once, top and bottom.
            let row_h = (items_h - 2.0 * TEXT_PAD) / rows as f32;
            out.row_height = row_h;
            if let Some(sel) = selected.filter(|sel| *sel < rows) {
                out.selection = Some([
                    x + PAD - SELECTION_BLEED,
                    items_y + sel as f32 * row_h,
                    inner_w + 2.0 * SELECTION_BLEED,
                    row_h + 2.0 * TEXT_PAD,
                ]);
            }
        }
        if let Some(scroll) = scroll.filter(Scroll::overflows) {
            // Centred in the right-hand padding, clear of the selection pill's
            // bleed, so the list itself keeps its full width.
            let track_x = x + panel_w - PAD * 0.5 - SCROLLBAR_W * 0.5;
            let track = [track_x, items_y, SCROLLBAR_W, items_h];
            let span = (scroll.visible as f32 / scroll.total as f32) * items_h;
            let thumb_h = span.clamp(SCROLLBAR_MIN_THUMB.min(items_h), items_h);
            let last = scroll.total.saturating_sub(scroll.visible).max(1) as f32;
            let progress = (scroll.first as f32 / last).clamp(0.0, 1.0);
            out.scroll_track = Some(track);
            out.scroll_thumb = Some([
                track_x,
                items_y + progress * (items_h - thumb_h),
                SCROLLBAR_W,
                thumb_h,
            ]);
        }
        cy += items_h;
    }

    if sizes.hint.1 > 0.0 {
        cy += GAP;
        out.hint = Some([x + PAD, cy]);
        // A rule only where there is a list to separate the footer *from*.
        // On a card that is title-and-hint alone it would just be a line.
        if items_h > 0.0 {
            out.divider = Some([x + PAD, cy - GAP * 0.5, inner_w, DIVIDER_H]);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(title: Size, query: Size, items: Size, hint: Size) -> SectionSizes {
        SectionSizes {
            title,
            query,
            items,
            hint,
        }
    }

    const SCREEN_W: f32 = 2560.0;

    #[test]
    fn a_narrow_panel_still_gets_a_usable_column() {
        let (w, _) = target_size(
            &sizes((40.0, 24.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0)),
            SCREEN_W,
            0.0,
        );
        assert_eq!(w, MIN_CONTENT_W + 2.0 * PAD);
    }

    #[test]
    fn the_card_never_narrows_while_a_panel_is_open() {
        // The launcher re-measures its list on every keystroke. Tracking that
        // width would resize the card under the user's typing, so the caller's
        // floor wins and the step absorbs what is left.
        let wide = sizes((40.0, 24.0), (200.0, 22.0), (980.0, 300.0), (300.0, 20.0));
        let (opened, _) = target_size(&wide, SCREEN_W, 0.0);

        let narrowed = sizes((40.0, 24.0), (200.0, 22.0), (120.0, 26.0), (300.0, 20.0));
        let (after, _) = target_size(&narrowed, SCREEN_W, opened);
        assert_eq!(after, opened, "the card shrank under the typing");
    }

    #[test]
    fn the_content_column_moves_in_steps_rather_than_pixels() {
        // Quantisation cannot stop a width change, only make it rare: the
        // column is a whole number of steps, so a step's worth of growth
        // crosses at most one boundary. The no-shrink floor is what keeps a
        // live launcher still; this is what keeps its first frames still.
        let widths: Vec<f32> = (0..=WIDTH_STEP as usize)
            .map(|i| {
                let s = sizes(
                    (40.0, 24.0),
                    (0.0, 0.0),
                    (700.0 + i as f32, 300.0),
                    (0.0, 0.0),
                );
                target_size(&s, SCREEN_W, 0.0).0
            })
            .collect();
        for w in &widths {
            assert_eq!((w - 2.0 * PAD) % WIDTH_STEP, 0.0, "{w} is not a whole step");
        }
        let changes = widths.windows(2).filter(|pair| pair[0] != pair[1]).count();
        assert!(
            changes <= 1,
            "a step of growth resized the card {changes} times"
        );
    }

    #[test]
    fn the_card_stays_off_the_screen_edges() {
        let huge = sizes((4000.0, 24.0), (0.0, 0.0), (4000.0, 300.0), (0.0, 0.0));
        let (w, _) = target_size(&huge, 1280.0, 0.0);
        assert_eq!(w, 1280.0 - SCREEN_MARGIN);

        // ... and a floor from a wider panel cannot push it back over them.
        let (w, _) = target_size(&huge, 1280.0, 5000.0);
        assert_eq!(w, 1280.0 - SCREEN_MARGIN);
    }

    #[test]
    fn a_narrow_output_wins_over_the_desktop_width_floor() {
        let huge = sizes(
            (4000.0, 24.0),
            (4000.0, 22.0),
            (4000.0, 300.0),
            (4000.0, 20.0),
        );
        for screen_w in [240.0, 320.0, 800.0] {
            let (w, _) = target_size(&huge, screen_w, 5000.0);
            assert_eq!(w, max_panel_width(screen_w));
            assert!(w <= screen_w, "{w} overflowed a {screen_w} px output");
            assert_eq!(max_content_width(screen_w), (w - 2.0 * PAD).max(0.0) as u32);
        }
    }

    #[test]
    fn the_query_budget_accounts_for_both_field_insets() {
        for screen_w in [240.0, 320.0, 800.0, SCREEN_W] {
            assert_eq!(
                max_query_text_width(screen_w),
                max_content_width(screen_w).saturating_sub(24)
            );
        }
    }

    #[test]
    fn absent_sections_cost_no_height() {
        let full = sizes((40.0, 24.0), (200.0, 22.0), (300.0, 260.0), (300.0, 20.0));
        let (_, tall) = target_size(&full, SCREEN_W, 0.0);
        let bare = sizes((40.0, 24.0), (0.0, 0.0), (0.0, 0.0), (0.0, 0.0));
        let (_, short) = target_size(&bare, SCREEN_W, 0.0);

        assert_eq!(short, 2.0 * PAD + 24.0);
        assert_eq!(
            tall,
            short + GAP + (22.0 + QUERY_LEAD) + GAP + 260.0 + GAP + 20.0
        );
    }

    #[test]
    fn the_bands_stack_in_order_and_end_inside_the_card() {
        let s = sizes((40.0, 24.0), (200.0, 22.0), (300.0, 260.0), (300.0, 20.0));
        let (w, h) = target_size(&s, SCREEN_W, 0.0);
        let panel = [100.0, 50.0, w, h];
        let c = contents(panel, &s, 10, Some(3), None);

        assert_eq!(c.title, [130.0, 80.0]);
        let field = c.query_field.unwrap();
        let items = c.items.unwrap();
        let hint = c.hint.unwrap();
        assert!(field[1] > c.title[1] + s.title.1);
        assert!(items[1] > field[1] + field[3]);
        assert!(hint[1] > items[1] + s.items.1);
        // The footer's own text must still sit inside the card, with the
        // bottom padding under it.
        assert!(hint[1] + s.hint.1 <= 50.0 + h);

        // Everything is inset by the same padding.
        for left in [c.title[0], field[0], items[0], hint[0]] {
            assert_eq!(left, 130.0);
        }
        assert_eq!(field[2], w - 2.0 * PAD);
    }

    #[test]
    fn the_selection_pill_covers_exactly_one_row() {
        let s = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 204.0), (0.0, 0.0));
        let panel = [0.0, 0.0, 600.0, 400.0];
        let rows = 10;
        let items_y = contents(panel, &s, rows, None, None).items.unwrap()[1];
        let row_h = (204.0 - 2.0 * TEXT_PAD) / rows as f32;

        for sel in 0..rows {
            let pill = contents(panel, &s, rows, Some(sel), None)
                .selection
                .unwrap();
            assert!(
                (pill[1] - (items_y + sel as f32 * row_h)).abs() < 0.001,
                "row {sel}"
            );
            assert!((pill[3] - (row_h + 2.0 * TEXT_PAD)).abs() < 0.001);
            // Wider than the text block on both sides, so the highlight reads
            // as a row of the card.
            assert_eq!(pill[0], PAD - SELECTION_BLEED);
            assert_eq!(pill[2], 600.0 - 2.0 * PAD + 2.0 * SELECTION_BLEED);
        }
    }

    #[test]
    fn a_selection_past_the_end_draws_no_pill() {
        let s = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 204.0), (0.0, 0.0));
        let panel = [0.0, 0.0, 600.0, 400.0];
        assert!(contents(panel, &s, 4, Some(4), None).selection.is_none());
        assert!(contents(panel, &s, 0, Some(0), None).selection.is_none());
        assert!(contents(panel, &s, 4, None, None).selection.is_none());
    }

    #[test]
    fn the_footer_rule_only_appears_between_a_list_and_a_hint() {
        let panel = [0.0, 0.0, 600.0, 400.0];
        let both = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 204.0), (300.0, 20.0));
        let rule = contents(panel, &both, 8, None, None).divider.unwrap();
        let hint = contents(panel, &both, 8, None, None).hint.unwrap();
        assert!(rule[1] < hint[1] && rule[1] > hint[1] - GAP);
        assert_eq!(rule[2], 600.0 - 2.0 * PAD);
        assert_eq!(rule[3], DIVIDER_H);

        let hint_only = sizes((40.0, 24.0), (0.0, 0.0), (0.0, 0.0), (300.0, 20.0));
        assert!(contents(panel, &hint_only, 0, None, None).divider.is_none());
        let list_only = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 204.0), (0.0, 0.0));
        assert!(contents(panel, &list_only, 8, None, None).divider.is_none());
    }

    #[test]
    fn a_list_that_fits_shows_no_scroll_indicator() {
        let s = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 204.0), (0.0, 0.0));
        let panel = [0.0, 0.0, 600.0, 400.0];
        for scroll in [
            Scroll {
                first: 0,
                visible: 12,
                total: 12,
            },
            Scroll {
                first: 0,
                visible: 12,
                total: 3,
            },
            Scroll {
                first: 0,
                visible: 0,
                total: 40,
            },
        ] {
            let c = contents(panel, &s, 12, None, Some(scroll));
            assert!(c.scroll_track.is_none(), "{scroll:?}");
            assert!(c.scroll_thumb.is_none());
        }
    }

    #[test]
    fn the_scroll_thumb_travels_the_track_and_stays_on_it() {
        let s = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 240.0), (0.0, 0.0));
        let panel = [0.0, 0.0, 600.0, 400.0];
        let at = |first| {
            contents(
                panel,
                &s,
                12,
                None,
                Some(Scroll {
                    first,
                    visible: 12,
                    total: 60,
                }),
            )
        };

        let track = at(0).scroll_track.unwrap();
        assert_eq!(track[2], SCROLLBAR_W);
        // In the right-hand padding, clear of the pill that bleeds into it.
        assert!(track[0] > 600.0 - PAD + SELECTION_BLEED);
        assert!(track[0] + SCROLLBAR_W < 600.0);

        let top = at(0).scroll_thumb.unwrap();
        let bottom = at(48).scroll_thumb.unwrap();
        assert_eq!(top[1], track[1], "thumb does not start at the top");
        assert!(
            (bottom[1] + bottom[3] - (track[1] + track[3])).abs() < 0.001,
            "thumb does not reach the bottom"
        );
        assert!(top[3] < track[3] && top[3] >= SCROLLBAR_MIN_THUMB);

        // Monotonic, and never off the end of the track.
        let mut previous = f32::MIN;
        for first in 0..=48 {
            let thumb = at(first).scroll_thumb.unwrap();
            assert!(thumb[1] >= previous - 0.001, "went backwards at {first}");
            assert!(thumb[1] >= track[1] - 0.001);
            assert!(thumb[1] + thumb[3] <= track[1] + track[3] + 0.001);
            previous = thumb[1];
        }
    }

    #[test]
    fn a_very_long_list_keeps_a_grabbable_thumb() {
        let s = sizes((40.0, 24.0), (0.0, 0.0), (300.0, 240.0), (0.0, 0.0));
        let panel = [0.0, 0.0, 600.0, 400.0];
        let thumb = contents(
            panel,
            &s,
            12,
            None,
            Some(Scroll {
                first: 0,
                visible: 2,
                total: 4000,
            }),
        )
        .scroll_thumb
        .unwrap();
        assert_eq!(thumb[3], SCROLLBAR_MIN_THUMB);
    }

    #[test]
    fn a_card_still_springing_open_lays_its_contents_out_inside_itself() {
        // Mid-open the card is narrower than its target. The contents follow
        // it, so nothing is drawn where the card is not yet.
        let s = sizes((40.0, 24.0), (200.0, 22.0), (300.0, 204.0), (300.0, 20.0));
        let (w, h) = target_size(&s, SCREEN_W, 0.0);
        let seed = [0.0, 0.0, w * 0.3, h * 0.3];
        let c = contents(seed, &s, 8, Some(0), None);
        let field = c.query_field.unwrap();
        assert!(field[0] + field[2] <= seed[2]);
        assert_eq!(field[2], (seed[2] - 2.0 * PAD).max(0.0));
    }

    #[test]
    fn a_card_narrower_than_its_own_padding_produces_no_negative_widths() {
        let s = sizes((40.0, 24.0), (200.0, 22.0), (300.0, 204.0), (300.0, 20.0));
        let c = contents(
            [0.0, 0.0, 4.0, 4.0],
            &s,
            8,
            Some(1),
            Some(Scroll {
                first: 1,
                visible: 4,
                total: 40,
            }),
        );
        assert_eq!(c.query_field.unwrap()[2], 0.0);
        assert!(c.selection.unwrap()[2] >= 0.0);
        assert!(c.divider.unwrap()[2] >= 0.0);
    }
}
