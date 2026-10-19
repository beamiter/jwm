//! Framework-neutral wheel and precision-scroll normalization.
//!
//! A wheel notch, a fractional X11 scroll line and a touchpad pixel packet are
//! different amounts of input. Accumulating them here keeps all native bars
//! consistent without letting a tiny packet switch an entire workspace.

use std::time::{Duration, Instant};

/// Backends such as X11 do not report gesture boundaries. An idle interval is
/// the boundary for those streams; explicit start/end events take precedence.
pub const SCROLL_GESTURE_IDLE: Duration = Duration::from_millis(250);
/// Logical distance needed to navigate to another workspace, layout or page.
pub const NAVIGATION_SCROLL_PIXELS: f64 = 40.0;
/// Logical distance needed for one existing volume/brightness adjustment.
pub const ADJUSTMENT_SCROLL_PIXELS: f64 = 16.0;

/// Positive deltas follow the native convention: up/left, toward the previous
/// item. Pixel values are logical pixels, independent of the output's DPI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDelta {
    Lines { x: f64, y: f64 },
    Pixels { x: f64, y: f64 },
}

impl ScrollDelta {
    /// Convert a native physical-pixel packet once, at the frontend boundary.
    /// Invalid scales yield an invalid packet, which the accumulator ignores.
    #[must_use]
    pub fn physical_pixels(x: f64, y: f64, scale_factor: f64) -> Self {
        if !scale_factor.is_finite() || scale_factor <= 0.0 {
            return Self::Pixels {
                x: f64::NAN,
                y: f64::NAN,
            };
        }
        Self::Pixels {
            x: x / scale_factor,
            y: y / scale_factor,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScrollPhase {
    Started,
    #[default]
    Moved,
    Ended,
    Cancelled,
}

/// Navigation commits once per precision gesture. Adjustments can repeat as
/// the user keeps moving, with at most one dispatch per native event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMode {
    Navigation,
    Adjustment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollStep {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// State for one pointer's active control. `T` should be a stable control ID;
/// including its output/placement identity also isolates live bar relocation.
#[derive(Debug, Clone)]
pub struct ScrollAccumulator<T> {
    target: Option<T>,
    mode: Option<ScrollMode>,
    last_update: Option<Instant>,
    pixel_units: Option<bool>,
    axis: Option<Axis>,
    residual: f64,
    precise: bool,
    navigation_fired: bool,
    finished: bool,
}

impl<T> Default for ScrollAccumulator<T> {
    fn default() -> Self {
        Self {
            target: None,
            mode: None,
            last_update: None,
            pixel_units: None,
            axis: None,
            residual: 0.0,
            precise: false,
            navigation_fired: false,
            finished: false,
        }
    }
}

impl<T: Copy + Eq> ScrollAccumulator<T> {
    /// Call on pointer leave, target changes, DPI changes or a fresh press.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Moving between cells in one navigation group must discard unfinished
    /// distance without rearming a gesture that already selected a new item.
    pub fn discard_partial(&mut self) {
        self.residual = 0.0;
        self.axis = None;
    }

    /// Fence an interrupted gesture while keeping its identity. A focus/grab
    /// transition can be caused by the scroll action opening a popup; clearing
    /// the whole state there would let its momentum immediately open it again.
    pub fn cancel(&mut self, now: Instant) {
        self.residual = 0.0;
        self.finished = true;
        self.last_update = Some(now);
    }

    /// Consume one packet. Vertical wheel input works on every bar edge.
    /// Horizontal bars also accept predominantly horizontal gestures; side
    /// bars ignore predominantly cross-axis input, including its small jitter.
    ///
    /// Ordinary whole-line wheel notches remain immediate. Fractional lines
    /// and pixels must reach a threshold; navigation then ignores the rest of
    /// that gesture, including momentum. No action backlog is retained.
    pub fn step(
        &mut self,
        target: T,
        delta: ScrollDelta,
        phase: ScrollPhase,
        mode: ScrollMode,
        allow_horizontal: bool,
        now: Instant,
    ) -> Option<ScrollStep> {
        let (x, y, pixels) = match delta {
            ScrollDelta::Lines { x, y } => (x, y, false),
            ScrollDelta::Pixels { x, y } => (x, y, true),
        };
        let expired = self.last_update.is_none_or(|last| {
            now.checked_duration_since(last)
                .is_none_or(|idle| idle >= SCROLL_GESTURE_IDLE)
        });
        if phase == ScrollPhase::Started
            || expired
            || self.target != Some(target)
            || self.mode != Some(mode)
            || self.pixel_units != Some(pixels)
        {
            self.reset();
        }
        self.target = Some(target);
        self.mode = Some(mode);
        self.pixel_units = Some(pixels);
        self.last_update = Some(now);
        if phase == ScrollPhase::Cancelled {
            self.cancel(now);
            return None;
        }
        let step = self.accumulate(x, y, pixels, mode, allow_horizontal);
        if phase == ScrollPhase::Ended {
            // Wayland can include the last actual displacement in the stop
            // frame. Consume it once, then fence any late momentum packets.
            self.cancel(now);
        }
        step
    }

    fn accumulate(
        &mut self,
        x: f64,
        y: f64,
        pixels: bool,
        mode: ScrollMode,
        allow_horizontal: bool,
    ) -> Option<ScrollStep> {
        if !x.is_finite() || !y.is_finite() {
            self.reset();
            return None;
        }
        if self.finished || (x == 0.0 && y == 0.0) {
            return None;
        }
        self.precise |= pixels || x.fract().abs() > 1e-6 || y.fract().abs() > 1e-6;
        if mode == ScrollMode::Navigation && self.precise && self.navigation_fired {
            return None;
        }

        // Use exactly one axis. Changing the dominant axis never carries its
        // unfinished distance into the other axis or synthesizes two actions.
        let axis = if x.abs() > y.abs() {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        if self.axis != Some(axis) {
            self.residual = 0.0;
            self.axis = Some(axis);
        }
        if axis == Axis::Horizontal && !allow_horizontal {
            self.residual = 0.0;
            return None;
        }
        let amount = match axis {
            Axis::Horizontal => x,
            Axis::Vertical => y,
        } / if pixels {
            match mode {
                ScrollMode::Navigation => NAVIGATION_SCROLL_PIXELS,
                ScrollMode::Adjustment => ADJUSTMENT_SCROLL_PIXELS,
            }
        } else {
            1.0
        };
        if self.residual.signum() != amount.signum() {
            self.residual = 0.0;
        }
        self.residual += amount;
        if self.residual.abs() + 1e-9 < 1.0 {
            return None;
        }
        let step = if self.residual > 0.0 {
            ScrollStep::Up
        } else {
            ScrollStep::Down
        };
        // Huge/coalesced deltas do not queue dozens of commands for a scene
        // whose destination IDs may already be stale before the next frame.
        self.residual = if self.residual.abs() >= 1.0 {
            self.residual.fract()
        } else {
            0.0
        };
        self.navigation_fired = true;
        Some(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(
        scroll: &mut ScrollAccumulator<u8>,
        delta: ScrollDelta,
        phase: ScrollPhase,
        now: Instant,
    ) -> Option<ScrollStep> {
        scroll.step(1, delta, phase, ScrollMode::Navigation, true, now)
    }

    #[test]
    fn real_wheel_notches_stay_immediate_and_do_not_queue_overshoot() {
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        for y in [1.0, 1.0, 12.0, 1.0] {
            assert_eq!(
                nav(
                    &mut scroll,
                    ScrollDelta::Lines { x: 0.0, y },
                    ScrollPhase::Moved,
                    now
                ),
                Some(ScrollStep::Up)
            );
        }
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Lines { x: 0.0, y: 0.01 },
                ScrollPhase::Moved,
                now
            ),
            None
        );
    }

    #[test]
    fn fractional_x11_lines_commit_once_until_the_stream_is_idle() {
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        let delta = ScrollDelta::Lines { x: 0.0, y: -0.1 };
        for _ in 0..9 {
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        }
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Moved, now),
            Some(ScrollStep::Down)
        );
        for _ in 0..50 {
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        }
        assert_eq!(
            nav(
                &mut scroll,
                delta,
                ScrollPhase::Moved,
                now + SCROLL_GESTURE_IDLE
            ),
            None
        );
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Lines { x: 0.0, y: -0.9 },
                ScrollPhase::Moved,
                now + SCROLL_GESTURE_IDLE
            ),
            Some(ScrollStep::Down)
        );
    }

    #[test]
    fn physical_pixels_have_the_same_threshold_on_mixed_dpi_outputs() {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            let mut scroll = ScrollAccumulator::default();
            let now = Instant::now();
            let delta = ScrollDelta::physical_pixels(0.0, 10.0 * scale, scale);
            for _ in 0..3 {
                assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
            }
            assert_eq!(
                nav(&mut scroll, delta, ScrollPhase::Moved, now),
                Some(ScrollStep::Up)
            );
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        }
    }

    #[test]
    fn crossing_navigation_cells_discards_partial_distance_without_rearming() {
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        let delta = ScrollDelta::Pixels { x: 0.0, y: 20.0 };
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        scroll.discard_partial();
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Moved, now),
            Some(ScrollStep::Up)
        );
        scroll.discard_partial();
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
    }

    #[test]
    fn end_and_cancel_fence_late_momentum_until_a_new_gesture() {
        for phase in [ScrollPhase::Ended, ScrollPhase::Cancelled] {
            let mut scroll = ScrollAccumulator::default();
            let now = Instant::now();
            let delta = ScrollDelta::Pixels { x: 0.0, y: 40.0 };
            assert_eq!(
                nav(&mut scroll, delta, ScrollPhase::Started, now),
                Some(ScrollStep::Up)
            );
            assert_eq!(nav(&mut scroll, delta, phase, now), None);
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
            assert_eq!(
                nav(&mut scroll, delta, ScrollPhase::Started, now),
                Some(ScrollStep::Up)
            );
        }
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        let delta = ScrollDelta::Pixels { x: 0.0, y: 40.0 };
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Started, now),
            Some(ScrollStep::Up)
        );
        scroll.cancel(now);
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        scroll.reset();
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Moved, now),
            Some(ScrollStep::Up)
        );
    }

    #[test]
    fn final_displacement_is_consumed_before_fencing_a_wayland_stop_frame() {
        let now = Instant::now();
        for (mode, half_step) in [
            (ScrollMode::Navigation, 20.0),
            (ScrollMode::Adjustment, 8.0),
        ] {
            let mut scroll = ScrollAccumulator::default();
            let delta = ScrollDelta::Pixels {
                x: 0.0,
                y: half_step,
            };
            assert_eq!(
                scroll.step(1, delta, ScrollPhase::Started, mode, true, now),
                None
            );
            assert_eq!(
                scroll.step(1, delta, ScrollPhase::Ended, mode, true, now),
                Some(ScrollStep::Up)
            );
            assert_eq!(
                scroll.step(1, delta, ScrollPhase::Moved, mode, true, now),
                None
            );
            assert_eq!(
                scroll.step(1, delta, ScrollPhase::Moved, mode, true, now),
                None
            );
        }
    }

    #[test]
    fn adjustments_repeat_but_never_inherit_distance_across_targets_or_directions() {
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        let mut adjust = |target, y| {
            scroll.step(
                target,
                ScrollDelta::Pixels { x: 0.0, y },
                ScrollPhase::Moved,
                ScrollMode::Adjustment,
                true,
                now,
            )
        };
        assert_eq!(adjust(1, 12.0), None);
        assert_eq!(adjust(2, 4.0), None);
        assert_eq!(adjust(2, -12.0), None);
        assert_eq!(adjust(2, -4.0), Some(ScrollStep::Down));
        assert_eq!(adjust(2, -16.0), Some(ScrollStep::Down));
        assert_eq!(adjust(2, -1600.0), Some(ScrollStep::Down));
        assert_eq!(adjust(2, -0.5), None);
    }

    #[test]
    fn horizontal_gestures_work_on_horizontal_bars_without_diagonal_double_dispatch() {
        let now = Instant::now();
        let delta = ScrollDelta::Pixels { x: -40.0, y: -4.0 };
        for horizontal in [true, false] {
            let mut scroll = ScrollAccumulator::default();
            assert_eq!(
                scroll.step(
                    1,
                    delta,
                    ScrollPhase::Moved,
                    ScrollMode::Navigation,
                    horizontal,
                    now
                ),
                horizontal.then_some(ScrollStep::Down)
            );
        }
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Pixels { x: 25.0, y: 25.0 },
                ScrollPhase::Moved,
                now
            ),
            None
        );
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Pixels { x: 15.0, y: 15.0 },
                ScrollPhase::Moved,
                now
            ),
            Some(ScrollStep::Up)
        );
    }

    #[test]
    fn units_resets_and_invalid_packets_cannot_leak_partial_gestures() {
        let mut scroll = ScrollAccumulator::default();
        let now = Instant::now();
        let partial = ScrollDelta::Pixels { x: 0.0, y: 30.0 };
        assert_eq!(nav(&mut scroll, partial, ScrollPhase::Moved, now), None);
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Lines { x: 0.0, y: 0.3 },
                ScrollPhase::Moved,
                now
            ),
            None
        );
        scroll.reset();
        assert_eq!(nav(&mut scroll, partial, ScrollPhase::Moved, now), None);
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                nav(
                    &mut scroll,
                    ScrollDelta::physical_pixels(0.0, 100.0, scale),
                    ScrollPhase::Moved,
                    now
                ),
                None
            );
        }
        assert_eq!(nav(&mut scroll, partial, ScrollPhase::Moved, now), None);
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Pixels {
                    x: f64::NAN,
                    y: 100.0
                },
                ScrollPhase::Moved,
                now
            ),
            None
        );
        assert_eq!(nav(&mut scroll, partial, ScrollPhase::Moved, now), None);
    }
}
