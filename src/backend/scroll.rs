//! Scroll packets and sequence ownership, shared by native and nested input.
//!
//! Pointer buttons are discrete; a finger scroll belongs to its original
//! recipient until both axes stop. A modal hand-off terminates that recipient
//! and fences the physical tail instead of delivering it to a new surface.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollSource {
    Wheel,
    #[default]
    Finger,
    Continuous,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollPhase {
    Started,
    #[default]
    Moved,
    Ended,
    Cancelled,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PointerScroll {
    pub device: String,
    /// Positive values move down/right, as in Wayland/libinput.
    pub delta: (f64, f64),
    /// Wheel units, 120 per detent; never fabricated for finger scrolling.
    pub v120: Option<(f64, f64)>,
    pub source: ScrollSource,
    pub stop: (bool, bool),
    pub phase: ScrollPhase,
    pub time: u32,
    /// Captured at routing, checked again when a queued WM event is dispatched.
    pub owner_epoch: u64,
}

impl PointerScroll {
    pub fn is_valid(&self) -> bool {
        self.delta.0.is_finite()
            && self.delta.1.is_finite()
            && self
                .v120
                .is_none_or(|(x, y)| x.is_finite() && y.is_finite())
    }

    pub fn has_motion(&self) -> bool {
        self.delta != (0.0, 0.0) || self.v120.is_some_and(|v| v != (0.0, 0.0))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollMode {
    #[default]
    Client,
    SystemUi,
    Capture,
    Locked(u64),
    Suspended,
}

impl ScrollMode {
    pub fn belongs_to_wm(self) -> bool {
        matches!(self, Self::SystemUi | Self::Capture)
    }
}

#[derive(Clone, Debug)]
struct Sequence<T> {
    focus: Option<T>,
    source: ScrollSource,
    axes: (bool, bool),
    suppressed: bool,
    time: u32,
    wm_owned: bool,
}

#[derive(Clone, Debug)]
pub struct ScrollTermination<T> {
    pub device: String,
    pub focus: Option<T>,
    pub source: ScrollSource,
    pub axes: (bool, bool),
    pub time: u32,
    pub wm_owned: bool,
}

/// Only nested streams without real stops use the idle boundary. Native
/// finger streams may pause in place for any duration without starting over.
#[derive(Debug)]
pub struct ScrollRoutes<T> {
    sequences: HashMap<String, Sequence<T>>,
    mode: ScrollMode,
    epoch: u64,
    pending_terminations: Vec<ScrollTermination<T>>,
}

impl<T> Default for ScrollRoutes<T> {
    fn default() -> Self {
        Self {
            sequences: HashMap::new(),
            mode: ScrollMode::Client,
            epoch: 0,
            pending_terminations: Vec::new(),
        }
    }
}

impl<T: Clone + PartialEq> ScrollRoutes<T> {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn belongs_to_wm(&self) -> bool {
        self.mode.belongs_to_wm()
    }

    pub fn set_mode(&mut self, mode: ScrollMode) -> Vec<ScrollTermination<T>> {
        if self.mode == mode {
            return Vec::new();
        }
        self.mode = mode;
        self.invalidate()
    }

    /// Changing the panel inside the same mode is still a new input owner.
    pub fn invalidate(&mut self) -> Vec<ScrollTermination<T>> {
        self.epoch = self.epoch.wrapping_add(1);
        self.interrupt()
    }

    /// Terminate the recipient, but retain a tombstone until physical lift.
    pub fn interrupt(&mut self) -> Vec<ScrollTermination<T>> {
        let mut ended = Vec::new();
        for (device, sequence) in &mut self.sequences {
            if !sequence.suppressed {
                ended.push(ScrollTermination {
                    device: device.clone(),
                    focus: sequence.focus.take(),
                    source: sequence.source,
                    axes: sequence.axes,
                    time: sequence.time,
                    wm_owned: sequence.wm_owned,
                });
                sequence.suppressed = true;
            }
        }
        ended
    }

    pub fn remove(&mut self, device: &str) -> Option<ScrollTermination<T>> {
        self.sequences
            .remove(device)
            .filter(|s| !s.suppressed)
            .map(|s| ScrollTermination {
                device: device.to_owned(),
                focus: s.focus,
                source: s.source,
                axes: s.axes,
                time: s.time,
                wm_owned: s.wm_owned,
            })
    }

    /// Send stops before wl_pointer.leave, including surface changes inside
    /// one client. A later axis event cannot safely terminate the old focus.
    pub fn change_focus(&mut self, focus: Option<&T>) -> Vec<ScrollTermination<T>> {
        if self.mode.belongs_to_wm() {
            return Vec::new();
        }
        let mut ended = Vec::new();
        for (device, sequence) in &mut self.sequences {
            if !sequence.suppressed && sequence.focus.as_ref() != focus {
                ended.push(ScrollTermination {
                    device: device.clone(),
                    focus: sequence.focus.take(),
                    source: sequence.source,
                    axes: sequence.axes,
                    time: sequence.time,
                    wm_owned: sequence.wm_owned,
                });
                sequence.suppressed = true;
            }
        }
        ended
    }

    pub fn take_terminations(&mut self) -> Vec<ScrollTermination<T>> {
        std::mem::take(&mut self.pending_terminations)
    }

    /// Returns whether the packet can reach its original recipient. A focus
    /// change also fences the tail: it cannot scroll a new window accidentally.
    pub fn route(
        &mut self,
        packet: &mut PointerScroll,
        focus: Option<T>,
        implicit_end: bool,
    ) -> bool {
        packet.owner_epoch = self.epoch;
        if !packet.is_valid() {
            return false;
        }
        if self.mode == ScrollMode::Suspended {
            return false;
        }
        if packet.source == ScrollSource::Wheel {
            if let Some(end) = self.remove(&packet.device) {
                self.pending_terminations.push(end);
            }
            return true;
        }
        if implicit_end
            && self
                .sequences
                .get(&packet.device)
                .is_some_and(|s| packet.time.wrapping_sub(s.time) >= 250)
        {
            if let Some(end) = self.remove(&packet.device) {
                self.pending_terminations.push(end);
            }
        }
        let fresh = !self.sequences.contains_key(&packet.device);
        if fresh && !packet.has_motion() {
            return false;
        }
        let sequence = self
            .sequences
            .entry(packet.device.clone())
            .or_insert_with(|| Sequence {
                focus: focus.clone(),
                source: packet.source,
                axes: (false, false),
                suppressed: false,
                time: packet.time,
                wm_owned: self.mode.belongs_to_wm(),
            });
        if sequence.focus != focus || sequence.source != packet.source {
            if !sequence.suppressed {
                self.pending_terminations.push(ScrollTermination {
                    device: packet.device.clone(),
                    focus: sequence.focus.take(),
                    source: sequence.source,
                    axes: sequence.axes,
                    time: sequence.time,
                    wm_owned: sequence.wm_owned,
                });
            }
            sequence.suppressed = true;
        }
        sequence.time = packet.time;
        if packet.delta.0 != 0.0 {
            sequence.axes.0 = true;
        }
        if packet.delta.1 != 0.0 {
            sequence.axes.1 = true;
        }
        if packet.stop.0 {
            sequence.axes.0 = false;
        }
        if packet.stop.1 {
            sequence.axes.1 = false;
        }
        let ended = sequence.axes == (false, false);
        let deliver = !sequence.suppressed;
        packet.phase = if ended {
            ScrollPhase::Ended
        } else if fresh {
            ScrollPhase::Started
        } else {
            ScrollPhase::Moved
        };
        if ended {
            self.sequences.remove(&packet.device);
        }
        deliver
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn finger(device: &str, x: f64, y: f64, time: u32) -> PointerScroll {
        PointerScroll {
            device: device.into(),
            delta: (x, y),
            time,
            ..Default::default()
        }
    }

    #[test]
    fn modal_handoff_ends_old_client_and_never_delivers_the_tail() {
        let mut routes = ScrollRoutes::default();
        let mut start = finger("pad", 0.5, 1.25, 10);
        assert!(routes.route(&mut start, Some(7), false));
        assert_eq!(start.phase, ScrollPhase::Started);
        let ends = routes.set_mode(ScrollMode::SystemUi);
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].focus, Some(7));
        assert_eq!(ends[0].axes, (true, true));
        assert!(!routes.route(&mut finger("pad", 0.0, 4.0, 20), None, false));
        let mut stop = finger("pad", 0.0, 0.0, 21);
        stop.stop = (true, true);
        assert!(!routes.route(&mut stop, None, false));
        let mut next = finger("pad", 0.0, 0.25, 22);
        assert!(routes.route(&mut next, None, false));
        assert_eq!(next.phase, ScrollPhase::Started);
        assert_ne!(start.owner_epoch, next.owner_epoch);
    }

    #[test]
    fn each_device_and_axis_has_an_independent_lifetime() {
        let mut routes = ScrollRoutes::default();
        assert!(routes.route(&mut finger("a", 1.0, 2.0, 1), Some(7), false));
        assert!(routes.route(&mut finger("b", 0.0, 1.0, 1), Some(7), false));
        let mut x_stop = finger("a", 0.0, 2.0, 2);
        x_stop.stop.0 = true;
        assert!(routes.route(&mut x_stop, Some(7), false));
        assert_eq!(x_stop.phase, ScrollPhase::Moved);
        assert_eq!(routes.remove("a").unwrap().axes, (false, true));
        assert!(routes.route(&mut finger("b", 0.0, 1.0, 3), Some(7), false));
    }

    #[test]
    fn native_pause_does_not_rearm_but_unphased_nested_input_expires() {
        for implicit in [false, true] {
            let mut routes = ScrollRoutes::default();
            routes.route(
                &mut finger("pad", 0.0, 1.0, u32::MAX - 10),
                Some(7),
                implicit,
            );
            routes.set_mode(ScrollMode::SystemUi);
            assert_eq!(
                routes.route(&mut finger("pad", 0.0, 2.0, 300), None, implicit),
                implicit
            );
        }
    }

    #[test]
    fn closing_a_panel_does_not_scroll_the_window_under_it() {
        let mut routes = ScrollRoutes::default();
        routes.set_mode(ScrollMode::SystemUi);
        routes.route(&mut finger("pad", 0.0, 10.0, 1), None::<u32>, false);
        routes.set_mode(ScrollMode::Client);
        assert!(!routes.route(&mut finger("pad", 0.0, 10.0, 2), Some(7), false));
    }

    #[test]
    fn same_mode_replacement_invalidates_queued_packets_and_late_tails() {
        let mut routes = ScrollRoutes::<u32>::default();
        routes.set_mode(ScrollMode::SystemUi);
        let mut queued = finger("pad", 0.0, 1.0, 1);
        assert!(routes.route(&mut queued, None, false));
        let ends = routes.invalidate();
        assert_eq!(ends.len(), 1);
        assert!(ends[0].wm_owned);
        assert_eq!(ends[0].device, "pad");
        assert_ne!(queued.owner_epoch, routes.epoch());
        assert!(!routes.route(&mut finger("pad", 0.0, 1.0, 2), None, false));
        assert!(
            routes.invalidate().is_empty(),
            "terminate each recipient once"
        );
    }

    #[test]
    fn focus_changes_end_the_old_axes_before_fencing_the_tail() {
        let mut routes = ScrollRoutes::default();
        routes.route(&mut finger("pad", 0.5, 2.0, 1), Some(7), false);
        let ends = routes.change_focus(Some(&8));
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0].focus, Some(7));
        assert_eq!(ends[0].axes, (true, true));
        assert!(!routes.route(&mut finger("pad", 0.0, 3.0, 2), Some(8), false));
        assert!(routes.change_focus(Some(&9)).is_empty());
        let mut stop = finger("pad", 0.0, 0.0, 3);
        stop.stop = (true, true);
        assert!(!routes.route(&mut stop, Some(9), false));
        assert!(routes.route(&mut finger("pad", 0.0, 1.0, 4), Some(9), false));
    }

    #[test]
    fn nested_idle_boundary_terminates_old_client_before_new_sequence() {
        let mut routes = ScrollRoutes::default();
        routes.route(&mut finger("nested", 0.0, 2.0, 1), Some(7), true);
        let mut next = finger("nested", 0.0, 0.5, 300);
        assert!(routes.route(&mut next, Some(8), true));
        assert_eq!(next.phase, ScrollPhase::Started);
        let ended = routes.take_terminations();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].focus, Some(7));
        assert_eq!(ended[0].axes, (false, true));
    }
}
