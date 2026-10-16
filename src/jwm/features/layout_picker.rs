//! State for the interactive layout picker.
//!
//! The picker shows every layout as one cell of a film strip and commits the
//! highlighted one. Browsing is live: stepping the strip applies the layout to
//! the real desktop behind the panel, so what the user confirms is what they
//! already see. Escape puts back the layout that was current when the picker
//! opened.
//!
//! Three things confirm, and they are deliberately redundant: Enter, a mouse
//! click, or simply stopping — a tap of the cycle key that is not followed by
//! another one commits on its own after [`AUTO_CONFIRM`].

use crate::core::layout::{
    LayoutClient, LayoutEnum, LayoutParams, MAX_PREVIEW_CLIENTS, contextual_preview,
    preview_frames, preview_window_count,
};
use std::time::{Duration, Instant};

/// How long the picker waits, after the last interaction, before committing
/// the highlighted layout on its own.
pub const AUTO_CONFIRM: Duration = Duration::from_millis(2600);

/// Bounded client data plus the actual total; all dimensions are logical pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewContext {
    pub count: usize,
    pub width: i32,
    pub height: i32,
    pub n_master: u32,
    pub m_fact: f32,
    pub gap: i32,
    pub clients: Vec<(f32, i32)>,
}

#[derive(Debug, Clone)]
pub struct LayoutPickerState {
    /// One entry per layout, in cycle order.
    pub layouts: Vec<&'static LayoutEnum>,
    /// Thumbnail geometry per layout, parallel to `layouts`.
    pub previews: Vec<Vec<[f32; 4]>>,
    /// Optional live snapshot; legacy construction still supports catalog previews.
    context: Option<PreviewContext>,
    estimates: Vec<Option<(i32, i32)>>,
    pub selected: usize,
    /// The layout that was current when the picker opened, restored on cancel.
    pub origin: usize,
    /// The monitor and per-tag slot that own this live preview. Pure picker
    /// construction leaves it unset; the JWM opener supplies the context.
    pub(crate) owner: Option<(crate::core::models::MonitorKey, usize)>,
    /// Previous-layout history at open and after the last live preview.
    /// Each pair holds the live monitor value and the owner's saved tag value.
    /// Comparing both avoids overwriting a newer independent history change.
    pub(crate) origin_previous: Option<(LayoutEnum, LayoutEnum)>,
    pub(crate) preview_previous: Option<(LayoutEnum, LayoutEnum)>,
    /// Bar preference before previewing, and the last value left by a preview.
    /// Comparing the latter preserves a newer independent toggle on cancel.
    pub(crate) origin_show_bar: Option<bool>,
    pub(crate) preview_show_bar: Option<bool>,
    /// When the auto-confirm delay started running.
    pub touched: Instant,
}

impl LayoutPickerState {
    /// Open the picker on `current`.
    pub fn new(current: &LayoutEnum) -> Self {
        let layouts: Vec<&'static LayoutEnum> = LayoutEnum::all().iter().collect();
        let previews = layouts
            .iter()
            .map(|layout| preview_frames(layout, preview_window_count(layout)))
            .collect();
        let selected = current.cycle_index();
        Self {
            layouts,
            previews,
            context: None,
            estimates: Vec::new(),
            selected,
            origin: selected,
            owner: None,
            origin_previous: None,
            preview_previous: None,
            origin_show_bar: None,
            preview_show_bar: None,
            touched: Instant::now(),
        }
    }

    /// Refresh only when geometry inputs change, never from selection/countdown.
    /// Does not touch origin, selected layout or the auto-confirm timer.
    pub fn update_context(&mut self, mut context: PreviewContext) {
        context.clients.truncate(MAX_PREVIEW_CLIENTS);
        if self.context.as_ref() == Some(&context) {
            return;
        }
        let params = LayoutParams {
            screen_area: crate::core::types::Rect::new(0, 0, context.width, context.height),
            n_master: context.n_master,
            m_fact: context.m_fact,
            gap: context.gap,
        };
        let clients: Vec<_> = context
            .clients
            .iter()
            .take(MAX_PREVIEW_CLIENTS)
            .enumerate()
            .map(|(key, &(factor, border_w))| LayoutClient {
                key,
                factor,
                border_w,
            })
            .collect();
        self.previews.clear();
        self.estimates.clear();
        for layout in &self.layouts {
            let preview = contextual_preview(layout, &params, &clients);
            self.previews.push(preview.frames);
            self.estimates.push(preview.smallest);
        }
        self.context = Some(context);
    }

    pub fn context_title(&self) -> String {
        self.context.as_ref().map_or_else(
            || "LAYOUT".into(),
            |c| {
                let sampled = if c.count > MAX_PREVIEW_CLIENTS {
                    " (64 sampled)"
                } else {
                    ""
                };
                format!(
                    "LAYOUT · {} tiled{} · {}×{}",
                    c.count, sampled, c.width, c.height
                )
            },
        )
    }

    pub fn selected_caption(&self) -> String {
        let layout = self.selected_layout();
        let name = format!("{}   {}", layout.symbol(), layout.label());
        let Some(context) = &self.context else {
            return name;
        };
        if context.count == 0 {
            return format!("{name} · No tiled windows");
        }
        if layout.0 == "float" {
            return format!("{name} · Illustrative cascade");
        }
        if layout.0 == "scrolling" {
            return format!("{name} · One window/column estimate; scroll for more");
        }
        if layout.0 == "deck" && context.count > context.n_master as usize {
            return format!("{name} · Overlapping stack; fewer visible tiles");
        }
        match self.estimates.get(self.selected).copied().flatten() {
            Some((w, h)) if w < 240 || h < 160 => {
                format!("{name} · ~{w}×{h} min · Try Scrolling / Deck")
            }
            Some((w, h)) => format!("{name} · ~{w}×{h} min tile"),
            None => name,
        }
    }

    pub fn selected_layout(&self) -> &'static LayoutEnum {
        self.layouts[self.selected.min(self.layouts.len() - 1)]
    }

    pub fn origin_layout(&self) -> &'static LayoutEnum {
        self.layouts[self.origin.min(self.layouts.len() - 1)]
    }

    /// Step the selection by `delta`, wrapping in both directions. Returns the
    /// layout now highlighted.
    pub fn step(&mut self, delta: i32) -> &'static LayoutEnum {
        let len = self.layouts.len() as i64;
        let next = (self.selected as i64 + i64::from(delta)).rem_euclid(len);
        self.selected = next as usize;
        self.touch();
        self.selected_layout()
    }

    /// Highlight a cell the pointer landed on. Returns the layout if the
    /// selection actually moved.
    pub fn select(&mut self, index: usize) -> Option<&'static LayoutEnum> {
        if index >= self.layouts.len() {
            return None;
        }
        self.touch();
        if index == self.selected {
            return None;
        }
        self.selected = index;
        Some(self.selected_layout())
    }

    /// Jump to the first or last cell without wrapping. Returns the layout
    /// when the highlight moved.
    pub fn jump_edge(&mut self, to_end: bool) -> Option<&'static LayoutEnum> {
        if self.layouts.is_empty() {
            return None;
        }
        let next = if to_end { self.layouts.len() - 1 } else { 0 };
        self.select(next)
    }

    /// Page the strip by a quarter of its length (at least 1), clamping at
    /// the ends — the twin of switcher / Info Page Up/Down.
    pub fn page(&mut self, direction: i32) -> Option<&'static LayoutEnum> {
        if self.layouts.is_empty() || direction == 0 {
            return None;
        }
        let step = (self.layouts.len() / 4).max(1) as i32;
        let next = if direction < 0 {
            self.selected.saturating_sub(step as usize)
        } else {
            (self.selected + step as usize).min(self.layouts.len() - 1)
        };
        self.select(next)
    }

    /// Restart the auto-confirm delay. Every interaction does this: someone
    /// still driving the picker has not finished choosing.
    pub fn touch(&mut self) {
        self.touched = Instant::now();
    }

    /// Fraction of the auto-confirm delay elapsed, `0.0..=1.0`.
    pub fn countdown(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.touched).as_secs_f32();
        (elapsed / AUTO_CONFIRM.as_secs_f32()).clamp(0.0, 1.0)
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.touched) >= AUTO_CONFIRM
    }

    /// Time left before auto-confirm, for the event loop's next wakeup.
    pub fn remaining(&self, now: Instant) -> Duration {
        AUTO_CONFIRM.saturating_sub(now.saturating_duration_since(self.touched))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(count: usize, width: i32, height: i32) -> PreviewContext {
        PreviewContext {
            count,
            width,
            height,
            n_master: 1,
            m_fact: 0.55,
            gap: 5,
            clients: vec![(1.0, 1); count.min(MAX_PREVIEW_CLIENTS)],
        }
    }

    #[test]
    fn contextual_empty_single_and_twenty_windows_are_bounded() {
        for count in [0, 1, 20, 64, 100_000] {
            for (width, height) in [(1920, 1080), (720, 1280), (1, 1)] {
                let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
                picker.update_context(context(count, width, height));
                for preview in &picker.previews {
                    assert!(preview.len() <= count.min(MAX_PREVIEW_CLIENTS));
                    if count == 0 {
                        assert!(preview.is_empty());
                    }
                    for &[x, y, w, h] in preview {
                        assert!(x.is_finite() && y.is_finite());
                        assert!(x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0);
                        assert!(x + w <= 1.001 && y + h <= 1.001);
                    }
                }
                assert_eq!(picker.selected_layout(), &LayoutEnum::TILE);
            }
        }
    }

    #[test]
    fn preview_uses_portrait_workarea_factors_and_master_settings() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::GRID);
        picker.update_context(context(20, 1920, 1080));
        let landscape = picker.previews.clone();
        picker.update_context(context(20, 720, 1280));
        assert_ne!(
            landscape[LayoutEnum::GRID.cycle_index()],
            picker.previews[LayoutEnum::GRID.cycle_index()]
        );
        let mut inputs = context(4, 1920, 1080);
        picker.update_context(inputs.clone());
        let equal = picker.previews[LayoutEnum::TILE.cycle_index()].clone();
        inputs.clients[1].0 = 3.0;
        picker.update_context(inputs.clone());
        assert_ne!(equal, picker.previews[LayoutEnum::TILE.cycle_index()]);
        let weighted = picker.previews.clone();
        inputs.n_master = 2;
        inputs.m_fact = 0.7;
        inputs.gap = 20;
        picker.update_context(inputs);
        assert_ne!(weighted, picker.previews);
    }

    #[test]
    fn refresh_preserves_cancel_origin_selection_and_deadline() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::GRID);
        picker.step(1);
        let selected = picker.selected;
        let touched = picker.touched;
        picker.update_context(context(20, 720, 1280));
        assert_eq!(picker.origin_layout(), &LayoutEnum::GRID);
        assert_eq!(picker.selected, selected);
        assert_eq!(picker.touched, touched);
        let allocation = picker.previews.as_ptr();
        picker.update_context(context(20, 720, 1280));
        assert_eq!(picker.previews.as_ptr(), allocation);
        assert_eq!(picker.touched, touched);
    }

    #[test]
    fn density_advice_is_explicit_and_never_selects_a_layout() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        picker.update_context(context(20, 1280, 720));
        assert!(picker.selected_caption().contains("Try Scrolling / Deck"));
        assert_eq!(picker.selected_layout(), &LayoutEnum::TILE);
        picker.select(LayoutEnum::SCROLLING.cycle_index());
        assert!(
            picker
                .selected_caption()
                .contains("estimate; scroll for more")
        );
        picker.select(LayoutEnum::DECK.cycle_index());
        assert!(picker.selected_caption().contains("Overlapping stack"));
        picker.update_context(context(100_000, 1280, 720));
        assert!(picker.context_title().contains("100000 tiled (64 sampled)"));
        picker.update_context(context(0, 1280, 720));
        assert!(picker.selected_caption().contains("No tiled windows"));
    }

    #[test]
    fn layout_picker_accepts_the_complete_ipc_delta_range() {
        for delta in [i32::MIN, i32::MAX, -1, 0, 1] {
            let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
            picker.select(1).unwrap();
            let expected = (1_i64 + i64::from(delta)).rem_euclid(picker.layouts.len() as i64);
            picker.step(delta);
            assert_eq!(picker.selected, expected as usize);
        }
    }

    #[test]
    fn opens_on_the_current_layout_and_remembers_it() {
        let picker = LayoutPickerState::new(&LayoutEnum::GRID);
        assert_eq!(picker.selected_layout(), &LayoutEnum::GRID);
        assert_eq!(picker.origin_layout(), &LayoutEnum::GRID);
    }

    #[test]
    fn stepping_matches_the_plain_layout_cycle() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        assert_eq!(picker.step(1), LayoutEnum::TILE.cycle_next());
        assert_eq!(picker.step(-1), &LayoutEnum::TILE);
        assert_eq!(picker.step(-1), LayoutEnum::TILE.cycle_prev());
    }

    #[test]
    fn stepping_wraps_both_ways() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        let count = picker.layouts.len();
        for _ in 0..count {
            picker.step(1);
        }
        assert_eq!(picker.selected_layout(), &LayoutEnum::TILE);
        picker.step(-1);
        assert_eq!(picker.selected, count - 1);
    }

    #[test]
    fn every_layout_gets_a_thumbnail_with_something_in_it() {
        let picker = LayoutPickerState::new(&LayoutEnum::TILE);
        assert_eq!(picker.previews.len(), picker.layouts.len());
        for (layout, preview) in picker.layouts.iter().zip(&picker.previews) {
            assert!(
                !preview.is_empty(),
                "{} has an empty thumbnail",
                layout.label()
            );
            for window in preview {
                let [x, y, w, h] = *window;
                assert!(
                    x >= 0.0 && y >= 0.0 && x + w <= 1.001 && y + h <= 1.001,
                    "{} draws outside its frame: {window:?}",
                    layout.label()
                );
                assert!(w > 0.0 && h > 0.0);
            }
        }
    }

    #[test]
    fn thumbnails_tell_the_layouts_apart() {
        let picker = LayoutPickerState::new(&LayoutEnum::TILE);
        for (i, a) in picker.previews.iter().enumerate() {
            for (j, b) in picker.previews.iter().enumerate().skip(i + 1) {
                // Monocle and Fullscreen genuinely put one window over the
                // whole screen; the status bar rule is what separates them.
                let both_full = picker.layouts[i].is_monocle() && picker.layouts[j].is_monocle();
                assert!(
                    both_full || a != b,
                    "{} and {} draw the same thumbnail",
                    picker.layouts[i].label(),
                    picker.layouts[j].label()
                );
            }
        }
    }

    #[test]
    fn interaction_restarts_the_auto_confirm_delay() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        let now = Instant::now();
        picker.touched = now - AUTO_CONFIRM;
        assert!(picker.expired(now));
        assert_eq!(picker.countdown(now), 1.0);

        picker.step(1);
        assert!(!picker.expired(Instant::now()));
        assert!(picker.countdown(Instant::now()) < 0.5);
    }

    #[test]
    fn the_hit_test_follows_the_selected_cells_lift() {
        // The picker's `selected` is what both renderers lift and what the
        // WM hands `layout_strip::cell_at`; this pins the pair together, so
        // a press on the drawn card of the highlighted layout stays on that
        // layout instead of landing in the gap beside it.
        use crate::backend::compositor_common::layout_strip;

        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        let count = picker.layouts.len();
        assert!(count >= 3, "the strip needs an interior cell to lift");
        let index = count / 2;
        picker.select(index);
        assert_eq!(picker.selected, index);

        let geometry = layout_strip::strip_geometry([0.0, 0.0, 1920.0, 1080.0], count);
        let cell = &geometry.cells[index];
        let (film, _) = layout_strip::presented_cell(cell, true);
        let y = layout_strip::center(cell.cell)[1];

        // A press one pixel into the gap left of the cell is on the lifted
        // card the user sees, and resolves to the cell they are looking at.
        let x = cell.cell[0] - 1.0;
        assert!(film[0] < x, "the lift must overhang its cell");
        assert_eq!(
            layout_strip::cell_at(&geometry, Some(picker.selected), x, y),
            Some(index)
        );
        // Re-selecting what is already highlighted is the hover no-op, so
        // the press commits the layout the strip is already showing.
        assert_eq!(picker.select(index), None);
        assert_eq!(picker.selected_layout(), picker.layouts[index]);
    }

    #[test]
    fn home_end_and_page_jump_without_wrapping() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        let last = picker.layouts.len() - 1;
        assert!(picker.jump_edge(true).is_some());
        assert_eq!(picker.selected, last);
        assert!(picker.jump_edge(true).is_none());
        assert!(picker.jump_edge(false).is_some());
        assert_eq!(picker.selected, 0);
        assert!(picker.jump_edge(false).is_none());
        assert!(picker.page(1).is_some());
        assert!(picker.selected > 0);
        let mid = picker.selected;
        assert!(picker.page(-1).is_some());
        assert!(picker.selected < mid);
    }

    #[test]
    fn hovering_the_selected_cell_only_restarts_the_delay() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        picker.touched = Instant::now() - AUTO_CONFIRM;
        assert_eq!(picker.select(picker.selected), None);
        assert!(!picker.expired(Instant::now()));
        assert_eq!(picker.select(usize::MAX), None);
    }
}
