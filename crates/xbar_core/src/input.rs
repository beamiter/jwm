//! Framework-neutral wheel and precision-scroll normalization.
//!
//! A wheel notch, a fractional X11 scroll line and a touchpad pixel packet are
//! different amounts of input. Accumulating them here keeps all native bars
//! consistent without letting a tiny packet switch an entire workspace.

use std::time::{Duration, Instant};

use crate::BarEdge;
use crate::presentation::{NodeId, PointerAction};

/// Backends such as X11 do not report gesture boundaries. An idle interval is
/// the boundary for those streams; explicit start/end events take precedence.
pub const SCROLL_GESTURE_IDLE: Duration = Duration::from_millis(250);
/// Logical distance needed to navigate to another workspace, layout or page.
pub const NAVIGATION_SCROLL_PIXELS: f64 = 40.0;
/// Logical distance needed for one existing volume/brightness adjustment.
pub const ADJUSTMENT_SCROLL_PIXELS: f64 = 16.0;

/// Shared sensitivity policy. Frontends can expose these values without
/// changing wheel-notch semantics or inventing another gesture recognizer.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ScrollPolicy {
    pub navigation_pixels: f64,
    pub adjustment_pixels: f64,
    pub idle: Duration,
}

impl Default for ScrollPolicy {
    fn default() -> Self {
        Self {
            navigation_pixels: NAVIGATION_SCROLL_PIXELS,
            adjustment_pixels: ADJUSTMENT_SCROLL_PIXELS,
            idle: SCROLL_GESTURE_IDLE,
        }
    }
}

impl ScrollPolicy {
    pub(crate) fn valid(self) -> Self {
        let defaults = Self::default();
        Self {
            navigation_pixels: if self.navigation_pixels.is_finite() && self.navigation_pixels > 0.0
            {
                self.navigation_pixels
            } else {
                defaults.navigation_pixels
            },
            adjustment_pixels: if self.adjustment_pixels.is_finite() && self.adjustment_pixels > 0.0
            {
                self.adjustment_pixels
            } else {
                defaults.adjustment_pixels
            },
            idle: if self.idle.is_zero() {
                defaults.idle
            } else {
                self.idle
            },
        }
    }
}

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
    policy: ScrollPolicy,
    target: Option<T>,
    mode: Option<ScrollMode>,
    last_update: Option<Instant>,
    pixel_units: Option<bool>,
    axis: Option<Axis>,
    residual: f64,
    precise: bool,
    navigation_fired: bool,
    finished: bool,
    explicit_active: bool,
}

impl<T> Default for ScrollAccumulator<T> {
    fn default() -> Self {
        Self {
            policy: ScrollPolicy::default(),
            target: None,
            mode: None,
            last_update: None,
            pixel_units: None,
            axis: None,
            residual: 0.0,
            precise: false,
            navigation_fired: false,
            finished: false,
            explicit_active: false,
        }
    }
}

impl<T: Copy + Eq> ScrollAccumulator<T> {
    #[must_use]
    pub fn with_policy(policy: ScrollPolicy) -> Self {
        Self {
            policy: policy.valid(),
            ..Self::default()
        }
    }

    pub fn set_policy(&mut self, policy: ScrollPolicy) {
        let policy = policy.valid();
        if self.policy != policy {
            self.policy = policy;
            self.reset();
        }
    }

    /// Call on pointer leave, target changes, DPI changes or a fresh press.
    pub fn reset(&mut self) {
        *self = Self::with_policy(self.policy);
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
        self.explicit_active = false;
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
        let expired = !self.explicit_active
            && self.last_update.is_none_or(|last| {
                now.checked_duration_since(last)
                    .is_none_or(|idle| idle >= self.policy.idle)
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
        self.explicit_active |= phase == ScrollPhase::Started;
        if phase == ScrollPhase::Cancelled {
            self.cancel(now);
            return None;
        }
        // An explicitly begun gesture is precision input even if its
        // toolkit coalesced or rounded the current line delta to an integer.
        self.precise |= self.explicit_active;
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
                ScrollMode::Navigation => self.policy.navigation_pixels,
                ScrollMode::Adjustment => self.policy.adjustment_pixels,
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

/// One shared scroll owner for a toolkit bar, rather than one accumulator per
/// widget. Rebuilding/replacing the active workspace must not rearm momentum.
#[derive(Debug, Clone, Default)]
pub struct ControlScroll {
    accumulator: ScrollAccumulator<(NodeId, BarEdge)>,
    owner: Option<(NodeId, BarEdge)>,
    hovered: Option<(NodeId, BarEdge)>,
    last_update: Option<Instant>,
    precise: bool,
    pixel_units: Option<bool>,
    explicit_active: bool,
}

impl ControlScroll {
    #[must_use]
    pub fn with_policy(policy: ScrollPolicy) -> Self {
        Self {
            accumulator: ScrollAccumulator::with_policy(policy),
            ..Self::default()
        }
    }

    pub fn reset(&mut self) {
        self.accumulator.reset();
        self.owner = None;
        self.hovered = None;
        self.last_update = None;
        self.precise = false;
        self.pixel_units = None;
        self.explicit_active = false;
    }

    /// Fence UI actions without ending a physical gesture. Hover, focus and
    /// popup changes are not End/Cancelled events, even after a long pause.
    pub fn interrupt(&mut self, now: Instant) {
        self.accumulator.cancel(now);
        self.last_update = Some(now);
    }

    /// A real End/Cancelled event releases the explicit physical lifetime.
    pub fn cancel(&mut self, now: Instant) {
        self.accumulator.cancel(now);
        self.explicit_active = false;
        self.last_update = Some(now);
    }

    pub fn set_policy(&mut self, policy: ScrollPolicy) {
        if self.accumulator.policy != policy.valid() {
            self.accumulator.set_policy(policy);
            self.reset();
        }
    }

    /// Call for actual pointer movement, not for layout changes underneath it.
    pub fn hover(&mut self, id: Option<NodeId>, edge: BarEdge, now: Instant) {
        let next = id.map(|id| (id, edge));
        if self.hovered != next || next.is_none() {
            self.accumulator.discard_partial();
            if self.precise && next.map(|(id, edge)| (scroll_group(id), edge)) != self.owner {
                self.interrupt(now);
            }
            self.hovered = next;
        }
    }

    /// Keep tracking lifecycle packets delivered over a gap or outside the bar.
    pub fn outside(&mut self, edge: BarEdge, phase: ScrollPhase, now: Instant) {
        if phase == ScrollPhase::Started {
            self.reset();
            self.explicit_active = true;
            self.precise = true;
        }
        self.hover(None, edge, now);
        if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
            self.cancel(now);
        }
    }

    pub fn step(
        &mut self,
        id: NodeId,
        edge: BarEdge,
        delta: ScrollDelta,
        phase: ScrollPhase,
        now: Instant,
    ) -> Option<PointerAction> {
        let target = (scroll_group(id), edge);
        let (x, y, pixels) = match delta {
            ScrollDelta::Pixels { x, y } => (x, y, true),
            ScrollDelta::Lines { x, y } => (x, y, false),
        };
        if !x.is_finite() || !y.is_finite() {
            self.interrupt(now);
            if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
                self.cancel(now);
            }
            return None;
        }
        // Toolkits may merge mouse-wheel and touchpad packets into one seat
        // stream. A unit change must not steal a still-active touchpad owner.
        if self.explicit_active && self.pixel_units != Some(pixels) && phase != ScrollPhase::Started
        {
            if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
                self.cancel(now);
            }
            return None;
        }
        let expired = !self.explicit_active
            && self.last_update.is_none_or(|last| {
                now.checked_duration_since(last)
                    .is_none_or(|idle| idle >= self.accumulator.policy.idle)
            });
        if phase == ScrollPhase::Started || expired || self.pixel_units != Some(pixels) {
            self.reset();
        }
        self.pixel_units = Some(pixels);
        self.explicit_active |= phase == ScrollPhase::Started;
        self.last_update = Some(now);
        self.precise |= self.explicit_active
            || match delta {
                ScrollDelta::Pixels { .. } => true,
                ScrollDelta::Lines { x, y } => x.fract().abs() > 1e-6 || y.fract().abs() > 1e-6,
            };
        if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
            self.explicit_active = false;
        }
        // The wrapper owns gesture lifetime. Once fenced, do not let the
        // inner accumulator's idle fallback rearm a paused explicit stream.
        if self.accumulator.finished {
            return None;
        }
        if self.precise && self.owner.is_some_and(|owner| owner != target) {
            // A precision gesture belongs to its initial control even if a
            // popup, reflow or pointer movement puts another control below it.
            self.accumulator.cancel(now);
            return None;
        }
        self.owner = Some(target);
        self.accumulator
            .step(
                target,
                delta,
                phase,
                match id {
                    NodeId::Audio | NodeId::Brightness => ScrollMode::Adjustment,
                    _ => ScrollMode::Navigation,
                },
                !edge.is_vertical(),
                now,
            )
            .map(|step| match step {
                ScrollStep::Up => PointerAction::ScrollUp,
                ScrollStep::Down => PointerAction::ScrollDown,
            })
    }
}

fn scroll_group(id: NodeId) -> NodeId {
    match id {
        NodeId::Tag(_) | NodeId::WorkspaceOverflow => NodeId::WorkspaceOverflow,
        id => id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_owner_survives_hover_leave_pause_and_return_until_terminal_event() {
        let now = Instant::now();
        let paused = now + Duration::from_secs(1);
        let delta = ScrollDelta::Pixels { x: 0.0, y: 16.0 };
        for terminal in [ScrollPhase::Ended, ScrollPhase::Cancelled] {
            for edge in [BarEdge::Top, BarEdge::Bottom, BarEdge::Left, BarEdge::Right] {
                let mut scroll = ControlScroll::default();
                scroll.hover(Some(NodeId::Audio), edge, now);
                assert_eq!(
                    scroll.step(NodeId::Audio, edge, delta, ScrollPhase::Started, now),
                    Some(PointerAction::ScrollUp)
                );
                scroll.hover(Some(NodeId::Brightness), edge, now);
                assert!(scroll.explicit_active);
                assert_eq!(
                    scroll.step(NodeId::Brightness, edge, delta, ScrollPhase::Moved, paused),
                    None
                );
                scroll.hover(None, edge, paused);
                scroll.hover(Some(NodeId::Audio), edge, paused);
                assert_eq!(
                    scroll.step(NodeId::Audio, edge, delta, ScrollPhase::Moved, paused),
                    None
                );
                scroll.outside(edge, terminal, paused);
                assert!(!scroll.explicit_active);
                assert_eq!(
                    scroll.step(NodeId::Brightness, edge, delta, ScrollPhase::Moved, paused),
                    None
                );
                assert_eq!(
                    scroll.step(
                        NodeId::Brightness,
                        edge,
                        delta,
                        ScrollPhase::Started,
                        paused
                    ),
                    Some(PointerAction::ScrollUp)
                );
            }
        }
    }

    #[test]
    fn interleaved_wheel_packets_cannot_release_a_paused_touchpad_owner() {
        let now = Instant::now();
        let paused = now + Duration::from_secs(1);
        let touchpad = ScrollDelta::Pixels { x: 0.0, y: 16.0 };
        let wheel = ScrollDelta::Lines { x: 0.0, y: 1.0 };
        let mut scroll = ControlScroll::default();
        assert!(
            scroll
                .step(
                    NodeId::Audio,
                    BarEdge::Top,
                    touchpad,
                    ScrollPhase::Started,
                    now
                )
                .is_some()
        );
        scroll.hover(Some(NodeId::Brightness), BarEdge::Top, now);
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                wheel,
                ScrollPhase::Moved,
                paused
            ),
            None
        );
        assert!(scroll.explicit_active);
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                touchpad,
                ScrollPhase::Moved,
                paused
            ),
            None
        );
        scroll.cancel(paused);
        assert!(
            scroll
                .step(
                    NodeId::Brightness,
                    BarEdge::Top,
                    wheel,
                    ScrollPhase::Moved,
                    paused
                )
                .is_some()
        );
        // Device/seat teardown explicitly resets all ownership and residuals.
        scroll.reset();
        assert!(
            scroll
                .step(
                    NodeId::Audio,
                    BarEdge::Top,
                    touchpad,
                    ScrollPhase::Started,
                    paused
                )
                .is_some()
        );
    }

    #[test]
    fn invalid_packets_and_empty_space_do_not_end_an_explicit_gesture() {
        let now = Instant::now();
        let delta = ScrollDelta::Pixels { x: 0.0, y: 16.0 };
        let mut scroll = ControlScroll::default();
        scroll.outside(BarEdge::Top, ScrollPhase::Started, now);
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Top,
                delta,
                ScrollPhase::Moved,
                now + Duration::from_secs(1)
            ),
            None
        );
        scroll.step(
            NodeId::Audio,
            BarEdge::Top,
            delta,
            ScrollPhase::Started,
            now,
        );
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Top,
                ScrollDelta::Pixels {
                    x: f64::NAN,
                    y: 0.0
                },
                ScrollPhase::Moved,
                now
            ),
            None
        );
        assert!(scroll.explicit_active);
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                delta,
                ScrollPhase::Moved,
                now + Duration::from_secs(1)
            ),
            None
        );
        scroll.outside(BarEdge::Top, ScrollPhase::Cancelled, now);
        assert!(!scroll.explicit_active);
    }

    #[test]
    fn toolkit_navigation_survives_reflow_without_momentum_changing_another_control() {
        let now = Instant::now();
        let mut scroll = ControlScroll::default();
        let tag = |index| NodeId::Tag(crate::TagId::new(index).unwrap());
        let half = ScrollDelta::Pixels { x: 0.0, y: 20.0 };
        assert_eq!(
            scroll.step(tag(0), BarEdge::Top, half, ScrollPhase::Started, now),
            None
        );
        assert_eq!(
            scroll.step(tag(0), BarEdge::Top, half, ScrollPhase::Moved, now),
            Some(PointerAction::ScrollUp)
        );
        // The WM snapshot replaces the tag under a stationary cursor.
        assert_eq!(
            scroll.step(tag(1), BarEdge::Top, half, ScrollPhase::Moved, now),
            None
        );
        // A layout change exposes brightness; remaining momentum cannot dim it.
        let adjustment = ScrollDelta::Pixels { x: 0.0, y: -16.0 };
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                adjustment,
                ScrollPhase::Moved,
                now
            ),
            None
        );
        let paused = now + Duration::from_secs(2);
        assert_eq!(
            scroll.step(tag(0), BarEdge::Top, half, ScrollPhase::Moved, paused),
            None
        );
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                adjustment,
                ScrollPhase::Started,
                paused
            ),
            Some(PointerAction::ScrollDown)
        );
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Top,
                adjustment,
                ScrollPhase::Moved,
                paused
            ),
            Some(PointerAction::ScrollDown)
        );
    }

    #[test]
    fn toolkit_zero_cancel_and_pointer_leave_do_not_adjust_settings() {
        let now = Instant::now();
        let mut scroll = ControlScroll::default();
        let partial = ScrollDelta::Pixels { x: 0.0, y: 12.0 };
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                partial,
                ScrollPhase::Moved,
                now
            ),
            None
        );
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                ScrollDelta::Pixels { x: 0.0, y: 0.0 },
                ScrollPhase::Moved,
                now
            ),
            None
        );
        scroll.hover(None, BarEdge::Right, now);
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                partial,
                ScrollPhase::Moved,
                now
            ),
            None
        );
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                partial,
                ScrollPhase::Started,
                now
            ),
            None
        );
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                partial,
                ScrollPhase::Cancelled,
                now
            ),
            None
        );
        assert_eq!(
            scroll.step(
                NodeId::Audio,
                BarEdge::Right,
                partial,
                ScrollPhase::Moved,
                now
            ),
            None
        );
        // Switching back to a real mouse wheel starts its own unit stream.
        assert_eq!(
            scroll.step(
                NodeId::Brightness,
                BarEdge::Right,
                ScrollDelta::Lines { x: 0.0, y: 1.0 },
                ScrollPhase::Moved,
                now
            ),
            Some(PointerAction::ScrollUp)
        );
    }

    #[test]
    fn custom_sensitivity_survives_resets_and_rearms_at_its_own_idle_boundary() {
        let policy = ScrollPolicy {
            navigation_pixels: 60.0,
            adjustment_pixels: 24.0,
            idle: Duration::from_millis(400),
        };
        let mut scroll = ScrollAccumulator::with_policy(policy);
        let now = Instant::now();
        let half = ScrollDelta::Pixels { x: 0.0, y: 30.0 };
        assert_eq!(nav(&mut scroll, half, ScrollPhase::Moved, now), None);
        assert_eq!(
            nav(&mut scroll, half, ScrollPhase::Moved, now),
            Some(ScrollStep::Up)
        );
        assert_eq!(
            nav(
                &mut scroll,
                half,
                ScrollPhase::Moved,
                now + Duration::from_millis(300)
            ),
            None
        );
        assert_eq!(
            nav(
                &mut scroll,
                half,
                ScrollPhase::Moved,
                now + Duration::from_millis(700)
            ),
            None
        );
        assert_eq!(
            nav(
                &mut scroll,
                half,
                ScrollPhase::Moved,
                now + Duration::from_millis(700)
            ),
            Some(ScrollStep::Up)
        );
        scroll.reset();
        assert_eq!(nav(&mut scroll, half, ScrollPhase::Moved, now), None);
        scroll.set_policy(ScrollPolicy {
            navigation_pixels: f64::NAN,
            adjustment_pixels: 0.0,
            idle: Duration::ZERO,
        });
        assert_eq!(
            nav(
                &mut scroll,
                ScrollDelta::Pixels { x: 0.0, y: 40.0 },
                ScrollPhase::Moved,
                now
            ),
            Some(ScrollStep::Up)
        );
    }

    #[test]
    fn an_explicit_gesture_can_pause_without_navigating_twice() {
        let now = Instant::now();
        let paused = now + Duration::from_secs(2);
        let delta = ScrollDelta::Pixels { x: 0.0, y: 40.0 };
        for end in [ScrollPhase::Ended, ScrollPhase::Cancelled] {
            let mut scroll = ScrollAccumulator::default();
            assert_eq!(
                nav(&mut scroll, delta, ScrollPhase::Started, now),
                Some(ScrollStep::Up)
            );
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, paused), None);
            assert_eq!(nav(&mut scroll, delta, end, paused), None);
            assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, paused), None);
            assert_eq!(
                nav(
                    &mut scroll,
                    delta,
                    ScrollPhase::Moved,
                    paused + SCROLL_GESTURE_IDLE
                ),
                Some(ScrollStep::Up)
            );

            let mut controls = ControlScroll::default();
            let id = NodeId::WorkspaceOverflow;
            assert_eq!(
                controls.step(id, BarEdge::Top, delta, ScrollPhase::Started, now),
                Some(PointerAction::ScrollUp)
            );
            assert_eq!(
                controls.step(id, BarEdge::Top, delta, ScrollPhase::Moved, paused),
                None
            );
            assert_eq!(controls.step(id, BarEdge::Top, delta, end, paused), None);
            assert_eq!(
                controls.step(
                    id,
                    BarEdge::Top,
                    delta,
                    ScrollPhase::Moved,
                    paused + SCROLL_GESTURE_IDLE
                ),
                Some(PointerAction::ScrollUp)
            );
        }
    }

    #[test]
    fn a_terminal_packet_after_reflow_releases_the_explicit_owner() {
        let now = Instant::now();
        let delta = ScrollDelta::Pixels { x: 0.0, y: 40.0 };
        for end in [ScrollPhase::Ended, ScrollPhase::Cancelled] {
            let mut controls = ControlScroll::default();
            assert_eq!(
                controls.step(
                    NodeId::WorkspaceOverflow,
                    BarEdge::Top,
                    delta,
                    ScrollPhase::Started,
                    now,
                ),
                Some(PointerAction::ScrollUp)
            );
            // Reflow delivers the stop to a different control, which must
            // neither inherit the gesture nor retain its explicit phase lock.
            assert_eq!(
                controls.step(NodeId::Audio, BarEdge::Top, delta, end, now),
                None
            );
            assert_eq!(
                controls.step(
                    NodeId::Audio,
                    BarEdge::Top,
                    delta,
                    ScrollPhase::Moved,
                    now + SCROLL_GESTURE_IDLE,
                ),
                Some(PointerAction::ScrollUp)
            );
        }
    }

    #[test]
    fn explicitly_begun_line_gestures_do_not_become_repeated_wheel_notches() {
        let now = Instant::now();
        let mut scroll = ScrollAccumulator::default();
        let delta = ScrollDelta::Lines { x: 0.0, y: 1.0 };
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Started, now),
            Some(ScrollStep::Up)
        );
        assert_eq!(nav(&mut scroll, delta, ScrollPhase::Moved, now), None);
        assert_eq!(
            nav(&mut scroll, delta, ScrollPhase::Started, now),
            Some(ScrollStep::Up)
        );
    }

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
