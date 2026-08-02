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

use crate::core::layout::{LayoutEnum, preview_frames, preview_window_count};
use std::time::{Duration, Instant};

/// How long the picker waits, after the last interaction, before committing
/// the highlighted layout on its own.
pub const AUTO_CONFIRM: Duration = Duration::from_millis(2600);

#[derive(Debug, Clone)]
pub struct LayoutPickerState {
    /// One entry per layout, in cycle order.
    pub layouts: Vec<&'static LayoutEnum>,
    /// Thumbnail geometry per layout, parallel to `layouts`.
    pub previews: Vec<Vec<[f32; 4]>>,
    pub selected: usize,
    /// The layout that was current when the picker opened, restored on cancel.
    pub origin: usize,
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
            selected,
            origin: selected,
            touched: Instant::now(),
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
        let len = self.layouts.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
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
    fn hovering_the_selected_cell_only_restarts_the_delay() {
        let mut picker = LayoutPickerState::new(&LayoutEnum::TILE);
        picker.touched = Instant::now() - AUTO_CONFIRM;
        assert_eq!(picker.select(picker.selected), None);
        assert!(!picker.expired(Instant::now()));
        assert_eq!(picker.select(usize::MAX), None);
    }
}
