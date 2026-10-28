//! Precision scrolling for compositor-owned controls. Application scrolling
//! stays in the backend; only our own lists receive local kinetic motion.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::backend::api::{Backend, HitTarget, SystemUiHitTarget};
use crate::backend::common_define::Mods;
use crate::backend::scroll::{PointerScroll, ScrollPhase, ScrollSource};
use crate::core::controller::WMController;
use crate::core::models::MonitorKey;
use crate::jwm::Jwm;
use crate::jwm::features::ControlKind;
use xbar_core::{ScrollAccumulator, ScrollDelta, ScrollMode, ScrollStep};

const KINETIC_FRAME: Duration = Duration::from_millis(16);
const VELOCITY_WINDOW_MS: u32 = 80;
const KINETIC_DECAY_MS: f64 = 160.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    epoch: u64,
    panel: &'static str,
    monitor: Option<MonitorKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    List,
    Slider { row: usize, kind: ControlKind },
    Navigation,
    Capture,
}

#[derive(Debug, Default)]
struct Velocity {
    samples: VecDeque<(u32, f64)>,
    position: f64,
}

impl Velocity {
    fn add(&mut self, time: u32, delta: f64) {
        if delta == 0.0 || !delta.is_finite() {
            return;
        }
        self.position += delta;
        if self.samples.back().is_some_and(|(last, _)| *last == time) {
            self.samples.back_mut().unwrap().1 = self.position;
        } else {
            self.samples.push_back((time, self.position));
        }
        while self
            .samples
            .front()
            .is_some_and(|(first, _)| time.wrapping_sub(*first) > VELOCITY_WINDOW_MS)
        {
            self.samples.pop_front();
        }
        // Input timestamps have millisecond resolution. The time window
        // therefore bounds memory even for a very high polling-rate device.
    }

    fn at_release(&self, time: u32) -> Option<f64> {
        let &(first_time, first_position) = self.samples.front()?;
        let &(last_time, last_position) = self.samples.back()?;
        if time.wrapping_sub(last_time) > VELOCITY_WINDOW_MS {
            return None;
        }
        let elapsed = last_time.wrapping_sub(first_time);
        if elapsed == 0 || elapsed > VELOCITY_WINDOW_MS {
            return None;
        }
        let velocity = ((last_position - first_position) / elapsed as f64).clamp(-4.0, 4.0);
        (velocity.abs() >= 0.08).then_some(velocity)
    }
}

#[derive(Debug)]
struct Stream {
    owner: Owner,
    control: Option<Control>,
    source: ScrollSource,
    pointer: (f64, f64),
    blocked: bool,
    horizontal: Option<bool>,
    axis_intent: (f64, f64),
    row_height: Option<f64>,
    steps: ScrollAccumulator<Control>,
    velocity: Velocity,
}

impl Stream {
    fn new(
        owner: Owner,
        control: Option<Control>,
        source: ScrollSource,
        pointer: (f64, f64),
    ) -> Self {
        Self {
            owner,
            control,
            source,
            pointer,
            blocked: control.is_none(),
            horizontal: None,
            axis_intent: (0.0, 0.0),
            row_height: None,
            steps: ScrollAccumulator::default(),
            velocity: Velocity::default(),
        }
    }

    fn list_displacement(&mut self, delta: (f64, f64)) -> f64 {
        if self.horizontal.is_none() {
            self.axis_intent.0 += delta.0;
            self.axis_intent.1 += delta.1;
            // A small slop prevents a lone subpixel packet from choosing the
            // axis. Once chosen, a horizontal gesture cannot spend vertical
            // jitter on a list later in the same physical sequence.
            if self.axis_intent.0.abs().max(self.axis_intent.1.abs()) < 2.0 {
                return 0.0;
            }
            self.horizontal = Some(self.axis_intent.0.abs() > self.axis_intent.1.abs());
            return if self.horizontal == Some(false) {
                self.axis_intent.1
            } else {
                0.0
            };
        }
        if self.horizontal == Some(false) && delta.1.abs() >= delta.0.abs() {
            delta.1
        } else {
            0.0
        }
    }
}

#[derive(Debug)]
struct Kinetic {
    owner: Owner,
    device: String,
    velocity: f64,
    row_height: f64,
    pointer: (f64, f64),
    started: Instant,
    last_tick: Instant,
}

impl Kinetic {
    fn advance(&mut self, now: Instant) -> Option<f64> {
        let elapsed = now.checked_duration_since(self.last_tick)?;
        if elapsed > Duration::from_millis(250)
            || now.duration_since(self.started) > Duration::from_secs(1)
            || self.velocity.abs() < 0.025
        {
            return None;
        }
        let dt = elapsed.as_secs_f64() * 1000.0;
        let next_velocity = self.velocity * (-dt / KINETIC_DECAY_MS).exp();
        let displacement = (self.velocity - next_velocity) * KINETIC_DECAY_MS;
        self.velocity = next_velocity;
        self.last_tick = now;
        Some(displacement / self.row_height)
    }
}

#[derive(Debug, Default)]
pub(crate) struct WmScrollState {
    streams: HashMap<String, Stream>,
    kinetic: Option<Kinetic>,
}

impl WmScrollState {
    fn reset(&mut self) {
        self.streams.clear();
        self.kinetic = None;
    }

    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.kinetic
            .as_ref()
            .map(|kinetic| (kinetic.last_tick + KINETIC_FRAME).saturating_duration_since(now))
    }
}

impl Jwm {
    pub(super) fn pointer_scroll_is_owned(&self) -> bool {
        self.features.system_ui.is_active()
            || !self.features.pointer_scroll.streams.is_empty()
            || self.features.pointer_scroll.kinetic.is_some()
    }

    /// A change of modal owner invalidates packets already in the WM queue,
    /// terminates the previous client, and fences the physical gesture tail.
    pub(crate) fn cancel_pointer_scroll(&mut self, backend: &mut dyn Backend) {
        self.features.pointer_scroll.reset();
        backend.compositor_cancel_scroll();
    }

    fn pointer_scroll_owner(&self, epoch: u64) -> Option<Owner> {
        if self.features.system_ui.is_locked() {
            return None;
        }
        let panel = if let Some(kind) = self.features.system_ui.panel_kind() {
            kind
        } else if self.features.screenshot.active {
            "screenshot"
        } else if self.features.recording.selecting_region {
            "recording_region"
        } else if self.features.expose_active {
            "expose"
        } else if self.features.annotation_active {
            "annotation"
        } else {
            return None;
        };
        Some(Owner {
            epoch,
            panel,
            monitor: self.state.sel_mon,
        })
    }

    fn pointer_scroll_control(&self, backend: &dyn Backend, x: f64, y: f64) -> Option<Control> {
        if !self.features.system_ui.is_active() {
            return Some(Control::Capture);
        }
        if self.features.system_ui.is_locked() || self.features.system_ui.is_prompting() {
            return None;
        }
        // These two overlays have their own shared geometry rather than a
        // list hit map. Their existing wheel actions retain their semantics.
        if self.features.system_ui.is_layout_picker() || self.features.system_ui.is_tags_overview()
        {
            return Some(Control::Navigation);
        }
        let hit = backend.compositor_system_ui_hit_test(x, y);
        if matches!(
            hit,
            SystemUiHitTarget::Outside | SystemUiHitTarget::Unavailable
        ) {
            return None;
        }
        if let SystemUiHitTarget::Item(row, _) = hit
            && let Some((kind, _)) = self.features.system_ui.wheel_slider_step(row, 1)
        {
            return Some(Control::Slider { row, kind });
        }
        if self.features.system_ui.list_viewport().is_some() {
            Some(Control::List)
        } else {
            Some(Control::Navigation)
        }
    }

    pub(crate) fn on_pointer_scroll(
        &mut self,
        backend: &mut dyn Backend,
        target: HitTarget,
        modifiers: u16,
        x: f64,
        y: f64,
        packet: PointerScroll,
    ) {
        if packet.owner_epoch != backend.compositor_scroll_epoch() || !packet.is_valid() {
            return;
        }
        if packet.phase == ScrollPhase::Cancelled {
            self.features.pointer_scroll.streams.remove(&packet.device);
            if self
                .features
                .pointer_scroll
                .kinetic
                .as_ref()
                .is_some_and(|k| k.device == packet.device)
            {
                self.features.pointer_scroll.kinetic = None;
            }
            return;
        }
        let Some(owner) = self.pointer_scroll_owner(packet.owner_epoch) else {
            return;
        };
        let mut control = self.pointer_scroll_control(backend, x, y);
        let precise = packet.source != ScrollSource::Wheel;
        let now = Instant::now();
        if packet.has_motion() || packet.phase == ScrollPhase::Started {
            self.features.pointer_scroll.kinetic = None;
        }
        let mut stream = self
            .features
            .pointer_scroll
            .streams
            .remove(&packet.device)
            .filter(|stream| stream.owner == owner && stream.source == packet.source)
            .filter(|_| packet.phase != ScrollPhase::Started)
            .unwrap_or_else(|| Stream::new(owner, control, packet.source, (x, y)));
        // Crossing a whole row rebuilds its text texture and invalidates the
        // hit map until the next painted frame. More packets can already be
        // queued at the same pointer position. Keep their captured control;
        // a moved pointer still requires a new, valid hit test.
        if precise
            && control.is_none()
            && stream.pointer == (x, y)
            && !self.features.system_ui.is_prompting()
            && backend.compositor_system_ui_hit_test(x, y) == SystemUiHitTarget::Unavailable
        {
            control = stream.control;
        }
        // A precision gesture stays on its initial control. Crossing from a
        // volume row into brightness must not spend its residual distance on
        // the new control, even if the pointer returns before finger lift.
        if precise && stream.control != control {
            stream.blocked = true;
        }
        if !precise && stream.control != control {
            stream = Stream::new(owner, control, packet.source, (x, y));
        }
        if control == stream.control
            && control.is_some()
            && backend.compositor_system_ui_hit_test(x, y) != SystemUiHitTarget::Unavailable
        {
            stream.pointer = (x, y);
        }
        if let Some(height) = backend
            .compositor_system_ui_row_height()
            .filter(|height| height.is_finite() && *height > 0.0)
        {
            stream.row_height = Some(height);
        }
        if let Some(Control::Slider { row, kind }) = stream.control
            && self
                .features
                .system_ui
                .wheel_slider_step(row, 1)
                .map(|(current, _)| current)
                != Some(kind)
        {
            // A background controls refresh can insert rows before the
            // original slider while its new hit map is still being painted.
            stream.blocked = true;
        }
        if !stream.blocked
            && let Some(control) = stream.control
        {
            if control == Control::List && precise {
                // Lists are one-dimensional. Ignore a horizontal gesture's
                // vertical jitter, without quantizing real vertical motion.
                let displacement = stream.list_displacement(packet.delta);
                if displacement != 0.0 {
                    let row_height = stream.row_height.unwrap_or(24.0);
                    let changed = self
                        .features
                        .system_ui
                        .scroll_list(displacement / row_height);
                    if changed == Some(true) {
                        stream.velocity.add(packet.time, displacement);
                        backend.compositor_set_system_ui_hover(None);
                        self.sync_system_ui(backend);
                    } else {
                        stream.velocity = Velocity::default();
                    }
                }
                if packet.phase == ScrollPhase::Ended
                    && packet.source == ScrollSource::Finger
                    && crate::config::CONFIG.load().motion_enabled()
                    && let Some(velocity) = stream.velocity.at_release(packet.time)
                {
                    let row_height = stream.row_height.unwrap_or(24.0);
                    self.features.pointer_scroll.kinetic = Some(Kinetic {
                        owner,
                        device: packet.device.clone(),
                        velocity,
                        row_height,
                        pointer: (x, y),
                        started: now,
                        last_tick: now,
                    });
                }
            } else {
                let (dx, dy) = if packet.source == ScrollSource::Wheel {
                    packet
                        .v120
                        .map(|(x, y)| (x / 120.0, y / 120.0))
                        .unwrap_or((packet.delta.0 / 15.0, packet.delta.1 / 15.0))
                } else {
                    packet.delta
                };
                let allow_horizontal = matches!(control, Control::Navigation);
                if dx != 0.0 || dy != 0.0 {
                    stream.horizontal = Some(allow_horizontal && dx.abs() > dy.abs());
                }
                let delta = if packet.source == ScrollSource::Wheel {
                    ScrollDelta::Lines { x: -dx, y: -dy }
                } else {
                    ScrollDelta::Pixels { x: -dx, y: -dy }
                };
                let phase = match packet.phase {
                    ScrollPhase::Started => xbar_core::ScrollPhase::Started,
                    ScrollPhase::Moved => xbar_core::ScrollPhase::Moved,
                    ScrollPhase::Ended => xbar_core::ScrollPhase::Ended,
                    ScrollPhase::Cancelled => xbar_core::ScrollPhase::Cancelled,
                };
                let mode = match control {
                    Control::Slider { .. } | Control::Capture | Control::List => {
                        ScrollMode::Adjustment
                    }
                    Control::Navigation => ScrollMode::Navigation,
                };
                if let Some(step) =
                    stream
                        .steps
                        .step(control, delta, phase, mode, allow_horizontal, now)
                {
                    let direction = if step == ScrollStep::Up { -1 } else { 1 };
                    if let Control::Slider { row, .. } = control {
                        let shift = backend
                            .key_ops()
                            .clean_mods(modifiers)
                            .contains(Mods::SHIFT);
                        self.scroll_system_ui_from_pointer(backend, direction, Some(row), shift);
                    } else if control == Control::List {
                        if self.features.system_ui.scroll_list(direction as f64 * 3.0) == Some(true)
                        {
                            backend.compositor_set_system_ui_hover(None);
                            self.sync_system_ui(backend);
                        }
                    } else {
                        let detail = match (stream.horizontal.unwrap_or(false), direction < 0) {
                            (false, true) => 4,
                            (false, false) => 5,
                            (true, true) => 6,
                            (true, false) => 7,
                        };
                        self.on_button_press(backend, target, modifiers, detail, packet.time);
                        // Capture's wheel press intentionally arms a swallowed
                        // release. Pair it here just like a physical wheel.
                        self.on_button_release(backend, target, detail, packet.time);
                    }
                }
            }
        }
        if packet.phase != ScrollPhase::Ended
            && self.pointer_scroll_owner(backend.compositor_scroll_epoch()) == Some(owner)
        {
            self.features
                .pointer_scroll
                .streams
                .insert(packet.device, stream);
        }
    }

    pub(crate) fn tick_pointer_scroll(&mut self, backend: &mut dyn Backend) {
        let Some(mut kinetic) = self.features.pointer_scroll.kinetic.take() else {
            return;
        };
        let still_on_list =
            self.pointer_scroll_control(backend, self.last_mouse_root.0, self.last_mouse_root.1)
                == Some(Control::List)
                || (kinetic.pointer == self.last_mouse_root
                    && !self.features.system_ui.is_prompting()
                    && backend.compositor_system_ui_hit_test(
                        self.last_mouse_root.0,
                        self.last_mouse_root.1,
                    ) == SystemUiHitTarget::Unavailable);
        if !crate::config::CONFIG.load().motion_enabled()
            || self.pointer_scroll_owner(backend.compositor_scroll_epoch()) != Some(kinetic.owner)
            || !still_on_list
        {
            return;
        }
        let now = Instant::now();
        if now.saturating_duration_since(kinetic.last_tick) < KINETIC_FRAME {
            self.features.pointer_scroll.kinetic = Some(kinetic);
            return;
        }
        let Some(delta_rows) = kinetic.advance(now) else {
            return;
        };
        if self.features.system_ui.scroll_list(delta_rows) == Some(true) {
            self.sync_system_ui(backend);
            self.features.pointer_scroll.kinetic = Some(kinetic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_before_lift_does_not_launch_inertia() {
        let mut velocity = Velocity::default();
        velocity.add(0, 5.0);
        velocity.add(10, 5.0);
        velocity.add(20, 5.0);
        assert_eq!(velocity.at_release(21), Some(0.5));
        assert_eq!(velocity.at_release(101), None);
    }

    #[test]
    fn velocity_handles_timestamp_wrap_and_coalesced_packets() {
        let mut velocity = Velocity::default();
        velocity.add(u32::MAX - 9, 5.0);
        velocity.add(0, 2.0);
        velocity.add(0, 3.0);
        assert_eq!(velocity.at_release(1), Some(0.5));
        assert_eq!(velocity.samples.len(), 2);
    }

    #[test]
    fn kinetic_distance_is_frame_rate_independent_and_drops_stalled_frames() {
        fn make(now: Instant) -> Kinetic {
            Kinetic {
                owner: Owner {
                    epoch: 1,
                    panel: "launcher",
                    monitor: None,
                },
                device: "pad".into(),
                velocity: 1.0,
                row_height: 20.0,
                pointer: (0.0, 0.0),
                started: now,
                last_tick: now,
            }
        }
        let now = Instant::now();
        let mut slow = make(now);
        let coarse = slow.advance(now + Duration::from_millis(80)).unwrap();
        let mut fast = make(now);
        let fine: f64 = (1..=5)
            .map(|i| fast.advance(now + Duration::from_millis(i * 16)).unwrap())
            .sum();
        assert!((coarse - fine).abs() < 1e-10);
        assert!(fast.advance(now + Duration::from_secs(2)).is_none());
    }

    #[test]
    fn idle_scrolling_has_no_periodic_wakeup() {
        assert_eq!(WmScrollState::default().next_wakeup(Instant::now()), None);
    }
}
