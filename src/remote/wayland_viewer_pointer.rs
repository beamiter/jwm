//! Explicit pointer-only observation, not a new remote grant. No initial button
//! snapshot exists in wl_pointer: a click requires our own observed down edge.
use super::super::x11_viewer::WaylandPointerEventKind as Kind;
use super::*;
use wayland_client::protocol::{wl_pointer, wl_seat};

const MAX_EVENTS: usize = 32;
pub(super) struct PointerSeat;
pub(super) struct PointerFence(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mapping {
    pub(super) sequence: u64,
    pub(super) source_epoch: u64,
    pub(super) geometry_epoch: u64,
    pub(super) generation: u64,
    pub(super) viewport: (u16, u16),
    pub(super) encoded: (u16, u16),
    pub(super) source: (u16, u16),
}
/// Exactly the integer nearest-neighbour fit used by upload_xrgb, not X11's
/// renderer transform. A half-open black bar is never clamped onto content.
fn map_point(map: Mapping, x: f64, y: f64) -> Option<(u16, u16, i16, i16)> {
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= f64::from(map.viewport.0)
        || y >= f64::from(map.viewport.1)
        || map.encoded.0 == 0
        || map.encoded.1 == 0
        || map.source.0 == 0
        || map.source.1 == 0
    {
        return None;
    }
    let (w, h) = (u64::from(map.viewport.0), u64::from(map.viewport.1));
    let (iw, ih) = (u64::from(map.encoded.0), u64::from(map.encoded.1));
    let (dw, dh) = if w * ih <= h * iw {
        (w, (w * ih / iw).max(1))
    } else {
        ((h * iw / ih).max(1), h)
    };
    let (ox, oy) = ((w - dw) / 2, (h - dh) / 2);
    let (px, py) = (x.floor() as u64, y.floor() as u64);
    if px < ox || py < oy || px >= ox + dw || py >= oy + dh {
        return None;
    }
    let sx = ((px - ox) * iw / dw) * u64::from(map.source.0) / iw;
    let sy = ((py - oy) * ih / dh) * u64::from(map.source.1) / ih;
    Some((
        u16::try_from(sx).ok()?,
        u16::try_from(sy).ok()?,
        i16::try_from(px).ok()?,
        i16::try_from(py).ok()?,
    ))
}
#[derive(Default)]
struct ScrollFrame {
    active: bool,
    source: Option<wl_pointer::AxisSource>,
    axis: Option<f64>,
    axis_kind: Option<wl_pointer::Axis>,
    discrete_kind: Option<wl_pointer::Axis>,
    discrete: Option<i32>,
    mapping: Option<Mapping>,
    confirmed: Option<u64>,
    ready: Option<u64>,
}
#[derive(Default)]
pub(super) struct Pointer {
    pub(super) scroll_enabled: bool,
    pub(super) horizontal_enabled: bool,
    ready: Option<u64>,
    frame_axis: bool,
    #[cfg(test)]
    pub(super) scroll_frames_observed: u64,
    scroll: ScrollFrame,
    frame_count: usize,
    frame_button: bool,
    discard_frame: bool,
    pub(super) fence: u64,
    pub(super) fence_done: u64,
    pub(super) requested: bool,
    pub(super) seat: Option<wl_seat::WlSeat>,
    pub(super) device: Option<wl_pointer::WlPointer>,
    pub(super) surface: Option<wl_surface::WlSurface>,
    pub(super) revoked: Option<&'static str>,
    reported: bool,
    capabilities_seen: bool,
    entered: bool,
    focused: bool,
    activated: bool,
    activation_seen: bool,
    position: Option<(f64, f64)>,
    left_down: bool,
    observed_down: Option<Mapping>,
    mapping: Option<Mapping>,
    confirmed: Option<u64>,
    frame_events: Vec<ViewerEvent>,
    completed: Vec<ViewerEvent>,
    frame_started: Option<Instant>,
}
impl Pointer {
    /// Local evidence only: an unfinished Wayland frame is not idle even when
    /// it has not produced a client-visible event. Never synthesize a release.
    pub(super) fn repair_busy(&self) -> bool {
        self.requested
            && (self.revoked.is_some()
                || self.left_down
                || self.frame_started.is_some()
                || !self.frame_events.is_empty()
                || !self.completed.is_empty()
                || self.frame_button
                || self.frame_axis)
    }
    pub(super) fn control_busy(&self) -> bool {
        self.left_down || !self.frame_events.is_empty() || self.frame_button || self.frame_axis
    }
    pub(super) fn revoke(&mut self, reason: &'static str) {
        if !self.requested {
            return;
        }
        self.revoked.get_or_insert(reason);
        self.mapping = None;
        self.confirmed = None;
        self.ready = None;
        self.observed_down = None;
        self.frame_events.clear();
        self.completed.clear();
        self.frame_started = None;
        self.scroll = ScrollFrame::default();
        self.frame_count = 0;
        self.frame_axis = false;
        self.frame_button = false;
        self.discard_frame = false;
    }
    pub(super) fn ready(&self) -> Option<u64> {
        self.ready
    }
    pub(super) fn mapping(&self) -> Option<Mapping> {
        self.mapping
    }
    pub(super) fn set_ready(&mut self, ready: Option<u64>) {
        if self.ready == ready {
            return;
        }
        if self.frame_started.is_some() {
            self.discard_frame = true;
            self.scroll = ScrollFrame::default();
        }
        self.completed.retain(|event| {
            !matches!(
                event,
                ViewerEvent::WaylandPointer {
                    kind: Kind::ScrollUp | Kind::ScrollDown | Kind::ScrollLeft | Kind::ScrollRight,
                    ..
                }
            )
        });
        self.ready = ready;
    }
    pub(super) fn locally_ready(&self) -> bool {
        self.requested
            && self.revoked.is_none()
            && self.focused
            && self.activated
            && self
                .mapping
                .is_some_and(|map| self.confirmed == Some(map.sequence))
    }
    pub(super) fn check(&mut self) {
        if self.frame_started.is_some_and(|at| at.elapsed() >= TIMEOUT) {
            self.revoke("Wayland pointer frame deadline");
        }
    }
    pub(super) fn needs_fence(&self) -> bool {
        self.requested && self.revoked.is_none() && !self.completed.is_empty()
    }
    pub(super) fn begin_fence(&mut self) -> Option<u64> {
        let Some(next) = self.fence.checked_add(1) else {
            self.revoke("Wayland pointer fence exhausted");
            return None;
        };
        self.fence = next;
        Some(next)
    }
    fn finish_fence(&mut self, id: u64) {
        if id == self.fence && id > self.fence_done {
            self.fence_done = id;
        } else {
            self.revoke("stale Wayland pointer fence");
        }
    }
    pub(super) fn has_events(&self) -> bool {
        !self.completed.is_empty() || (self.revoked.is_some() && !self.reported)
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.frame_started.map(|at| at + TIMEOUT)
    }
    pub(super) fn invalidate_mapping(&mut self) {
        self.mapping = None;
        self.confirmed = None;
        self.ready = None;
        self.observed_down = None;
        // Cancel any old-frame local candidate without inventing a release.
        self.frame_events.clear();
        self.completed.clear();
        // Preserve the open group boundary and deadline: later axis fragments
        // must never attach to a new frame's mapping or fresh Ready ticket.
        if self.frame_started.is_some() {
            self.discard_frame = true;
        }
        self.scroll = ScrollFrame::default();
        if self.requested && self.revoked.is_none() {
            self.completed
                .push(ViewerEvent::WaylandPointerButtonCancel {
                    pressed: self.left_down,
                });
        }
    }
    pub(super) fn presented(&mut self, mapping: Mapping) {
        if self.requested && self.revoked.is_none() {
            if self.frame_started.is_some() {
                self.discard_frame = true;
                self.frame_events.clear();
                self.scroll = ScrollFrame::default();
            }
            self.mapping = Some(mapping);
            self.confirmed = None;
            self.ready = None;
        }
    }
    pub(super) fn confirm(&mut self, sequence: u64) {
        if !self.requested || self.revoked.is_some() {
            return;
        }
        if self.mapping.is_some_and(|map| map.sequence == sequence) {
            self.confirmed = Some(sequence);
        } else {
            self.revoke("Wayland pointer stale client presentation confirmation");
        }
    }
    pub(super) fn activation(&mut self, states: &[u8]) {
        if states.len() % 4 != 0 || states.len() > 128 {
            self.revoke("invalid Wayland activation states");
            return;
        }
        let active = states.chunks_exact(4).any(|bytes| {
            u32::from_ne_bytes(bytes.try_into().unwrap())
                == u32::from(xdg_toplevel::State::Activated)
        });
        if self.activation_seen && !active {
            self.revoke("Wayland pointer window deactivated");
        }
        self.activated = active;
        self.activation_seen |= active;
    }
    fn scoped(&self, kind: Kind) -> ViewerEvent {
        let cancel = match kind {
            Kind::LeftDown => ViewerEvent::WaylandPointerButtonCancel { pressed: true },
            Kind::LeftUp => ViewerEvent::WaylandPointerButtonCancel { pressed: false },
            _ => ViewerEvent::WaylandPointerCancel,
        };
        let Some(map) = self.mapping else {
            return cancel;
        };
        if !self.focused || !self.activated || self.confirmed != Some(map.sequence) {
            return cancel;
        }
        let Some((x, y)) = self.position else {
            return cancel;
        };
        let Some((sx, sy, lx, ly)) = map_point(map, x, y) else {
            return cancel;
        };
        ViewerEvent::WaylandPointer {
            frame_sequence: map.sequence,
            geometry_epoch: map.geometry_epoch,
            x: sx,
            y: sy,
            local_x: lx,
            local_y: ly,
            kind,
        }
    }
    fn queue(&mut self, event: ViewerEvent) {
        if self.revoked.is_some() {
            return;
        }
        if self.frame_count + self.completed.len() >= MAX_EVENTS {
            self.revoke("Wayland pointer queue overflow");
            return;
        }
        self.frame_started.get_or_insert_with(Instant::now);
        self.frame_count += 1;
        if !self.discard_frame {
            self.frame_events.push(event);
        }
    }
    fn button(&mut self, pressed: bool) {
        self.frame_button = true;
        if self.scroll.active {
            self.revoke("Wayland scroll cannot share a button frame");
            return;
        }
        if pressed {
            if self.left_down {
                self.observed_down = None;
                self.queue(ViewerEvent::WaylandPointerButtonCancel { pressed: true });
                return;
            }
            self.left_down = true;
            let event = self.scoped(Kind::LeftDown);
            self.observed_down = if matches!(event, ViewerEvent::WaylandPointer { .. }) {
                self.mapping
            } else {
                None
            };
            self.queue(event);
        } else {
            let matched = self.left_down
                && self.observed_down.is_some()
                && self.observed_down == self.mapping;
            self.left_down = false;
            self.observed_down = None;
            self.queue(if matched {
                self.scoped(Kind::LeftUp)
            } else {
                ViewerEvent::WaylandPointerButtonCancel { pressed: false }
            });
        }
    }
    fn begin_axis(&mut self) -> bool {
        if !(self.scroll_enabled || self.horizontal_enabled) || self.left_down || self.frame_button
        {
            self.revoke("Wayland scroll lacks permission or conflicts with held button");
            return false;
        }
        if self.frame_count + self.completed.len() >= MAX_EVENTS {
            self.revoke("Wayland pointer axis queue overflow");
            return false;
        }
        self.frame_count += 1;
        self.frame_axis = true;
        self.frame_started.get_or_insert_with(Instant::now);
        if self.discard_frame {
            return false;
        }
        if !self.scroll.active {
            self.scroll.active = true;
            self.scroll.mapping = self.mapping;
            self.scroll.confirmed = self.confirmed;
            self.scroll.ready = self.ready;
        }
        true
    }
    fn axis_source(&mut self, source: WEnum<wl_pointer::AxisSource>) {
        if source != WEnum::Value(wl_pointer::AxisSource::Wheel) {
            self.revoke("Wayland scroll requires discrete wheel source");
            return;
        }
        if !self.begin_axis() {
            return;
        }
        if self
            .scroll
            .source
            .replace(wl_pointer::AxisSource::Wheel)
            .is_some()
        {
            self.revoke("duplicate Wayland wheel source");
        }
    }
    fn permitted_axis(&self, axis: WEnum<wl_pointer::Axis>) -> Option<wl_pointer::Axis> {
        match axis {
            WEnum::Value(wl_pointer::Axis::VerticalScroll) if self.scroll_enabled => {
                Some(wl_pointer::Axis::VerticalScroll)
            }
            WEnum::Value(wl_pointer::Axis::HorizontalScroll) if self.horizontal_enabled => {
                Some(wl_pointer::Axis::HorizontalScroll)
            }
            _ => None,
        }
    }
    fn axis_value(&mut self, axis: WEnum<wl_pointer::Axis>, value: f64) {
        let Some(axis) = self.permitted_axis(axis) else {
            self.revoke("unapproved Wayland wheel axis");
            return;
        };
        if !value.is_finite() || value == 0.0 {
            self.revoke("invalid Wayland wheel axis");
            return;
        }
        if !self.begin_axis() {
            return;
        }
        if self.scroll.axis.replace(value).is_some()
            || self.scroll.discrete_kind.is_some_and(|other| other != axis)
        {
            self.revoke("duplicate or mixed Wayland wheel axis");
            return;
        }
        self.scroll.axis_kind = Some(axis);
    }
    fn axis_discrete(&mut self, axis: WEnum<wl_pointer::Axis>, discrete: i32) {
        let Some(axis) = self.permitted_axis(axis) else {
            self.revoke("unapproved Wayland wheel notch axis");
            return;
        };
        if !matches!(discrete, -1 | 1) {
            self.revoke("Wayland wheel must contain exactly one notch");
            return;
        }
        if !self.begin_axis() {
            return;
        }
        if self.scroll.discrete.replace(discrete).is_some()
            || self.scroll.axis_kind.is_some_and(|other| other != axis)
        {
            self.revoke("duplicate or mixed Wayland wheel notch");
            return;
        }
        self.scroll.discrete_kind = Some(axis);
    }
    fn finish_frame(&mut self) {
        self.check();
        if self.revoked.is_some() {
            return;
        }
        #[cfg(test)]
        if self.frame_axis {
            self.scroll_frames_observed += 1;
        }
        if self.discard_frame {
            self.frame_events.clear();
            if self.completed.len() >= MAX_EVENTS {
                self.revoke("Wayland pointer queue overflow");
                return;
            }
            self.completed
                .push(ViewerEvent::WaylandPointerButtonCancel {
                    pressed: self.left_down,
                });
        } else if self.scroll.active {
            let scroll = std::mem::take(&mut self.scroll);
            let valid = scroll.source == Some(wl_pointer::AxisSource::Wheel)
                && scroll.axis_kind.is_some()
                && scroll.axis_kind == scroll.discrete_kind
                && matches!((scroll.axis, scroll.discrete), (Some(axis),Some(notch)) if axis.is_sign_positive() == (notch > 0));
            if !valid || self.frame_button || self.left_down {
                self.revoke("incomplete or inconsistent Wayland wheel frame");
                return;
            }
            // Motion belongs to this same logical input frame. It must not
            // consume the sole Ready gate before its wheel action does.
            self.frame_events.clear();
            let event = if scroll.mapping == self.mapping
                && scroll.confirmed == self.confirmed
                && scroll.ready.is_some()
                && scroll.ready == self.ready
                && self
                    .mapping
                    .is_some_and(|map| scroll.ready == Some(map.sequence))
                && self.locally_ready()
            {
                self.scoped(match (scroll.axis_kind, scroll.discrete) {
                    (Some(wl_pointer::Axis::HorizontalScroll), Some(-1)) => Kind::ScrollLeft,
                    (Some(wl_pointer::Axis::HorizontalScroll), _) => Kind::ScrollRight,
                    (_, Some(-1)) => Kind::ScrollUp,
                    _ => Kind::ScrollDown,
                })
            } else {
                ViewerEvent::WaylandPointerCancel
            };
            if self.completed.len() >= MAX_EVENTS {
                self.revoke("Wayland pointer queue overflow");
                return;
            }
            self.completed.push(event);
        } else {
            self.completed.append(&mut self.frame_events);
        }
        self.scroll = ScrollFrame::default();
        self.frame_count = 0;
        self.frame_axis = false;
        self.frame_button = false;
        self.discard_frame = false;
        self.frame_started = None;
    }
    pub(super) fn take_events(&mut self) -> Vec<ViewerEvent> {
        self.check();
        if self.revoked.is_some() {
            self.completed.clear();
            self.frame_events.clear();
            if !self.reported {
                self.reported = true;
                return vec![ViewerEvent::WaylandPointerRevoke];
            }
            return Vec::new();
        }
        std::mem::take(&mut self.completed)
    }
}
impl Dispatch<wl_callback::WlCallback, PointerFence> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        fence: &PointerFence,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.pointer.finish_fence(fence.0);
        }
    }
}
impl Dispatch<wl_seat::WlSeat, PointerSeat> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &PointerSeat,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if state.pointer.revoked.is_some() {
            return;
        }
        match event {
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(capabilities),
            } => {
                if state.pointer.capabilities_seen
                    || !capabilities.contains(wl_seat::Capability::Pointer)
                {
                    state
                        .pointer
                        .revoke("Wayland pointer capability changed or absent");
                    return;
                }
                state.pointer.capabilities_seen = true;
                state.pointer.device = Some(seat.get_pointer(qh, ()));
            }
            wl_seat::Event::Name { .. } => {}
            _ => state.pointer.revoke("unsupported pointer seat event"),
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        #[cfg(all(test, feature = "backend-wayland-udev"))]
        if let wl_pointer::Event::Button {
            button: 0x110,
            state: WEnum::Value(edge),
            ..
        } = &event
        {
            POINTER_BUTTON_DISPATCH_OBSERVER.with(|slot| {
                if let Some(observer) = slot.borrow().as_ref() {
                    let mut counts = observer.lock().unwrap();
                    match edge {
                        wl_pointer::ButtonState::Pressed => counts.0 += 1,
                        wl_pointer::ButtonState::Released => counts.1 += 1,
                        _ => {}
                    }
                }
            });
        }
        if state.control_mode
            && state.keyboard.held_keys()
            && matches!(
                &event,
                wl_pointer::Event::Motion { .. }
                    | wl_pointer::Event::Button { .. }
                    | wl_pointer::Event::Axis { .. }
                    | wl_pointer::Event::AxisSource { .. }
                    | wl_pointer::Event::AxisDiscrete { .. }
            )
        {
            state
                .pointer
                .revoke("Wayland control pointer overlaps held keys");
            state.control_collect();
            return;
        }
        let pointer = &mut state.pointer;
        if pointer.revoked.is_some() {
            return;
        }
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                if pointer.entered
                    || pointer.surface.as_ref() != Some(&surface)
                    || !surface_x.is_finite()
                    || !surface_y.is_finite()
                {
                    pointer.revoke("Wayland pointer enter history mismatch");
                    return;
                }
                pointer.entered = true;
                pointer.focused = true;
                pointer.position = Some((surface_x, surface_y));
                // Enter supplies no button snapshot and is not a synthetic motion.
                pointer.frame_started.get_or_insert_with(Instant::now);
            }
            wl_pointer::Event::Leave { .. } => {
                pointer.focused = false;
                pointer.revoke("Wayland pointer left own surface");
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if !pointer.focused || !surface_x.is_finite() || !surface_y.is_finite() {
                    pointer.revoke("invalid Wayland pointer motion");
                    return;
                }
                pointer.position = Some((surface_x, surface_y));
                pointer.queue(pointer.scoped(Kind::Motion));
            }
            wl_pointer::Event::Button {
                button: 0x110,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } => pointer.button(true),
            wl_pointer::Event::Button {
                button: 0x110,
                state: WEnum::Value(wl_pointer::ButtonState::Released),
                ..
            } => pointer.button(false),
            wl_pointer::Event::Axis { axis, value, .. } => pointer.axis_value(axis, value),
            wl_pointer::Event::AxisSource { axis_source } => pointer.axis_source(axis_source),
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                pointer.axis_discrete(axis, discrete)
            }
            wl_pointer::Event::Frame => pointer.finish_frame(),
            _ => pointer.revoke("unsupported Wayland pointer button or axis"),
        }
        state.control_collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mapping() -> Mapping {
        Mapping {
            sequence: 1,
            source_epoch: 1,
            geometry_epoch: 1,
            generation: 1,
            viewport: (101, 81),
            encoded: (80, 40),
            source: (160, 80),
        }
    }
    fn pointer() -> Pointer {
        let mut pointer = Pointer {
            requested: true,
            focused: true,
            activated: true,
            position: Some((50.0, 40.0)),
            ..Default::default()
        };
        pointer.presented(mapping());
        pointer.confirm(1);
        pointer
    }
    fn scroll_pointer() -> Pointer {
        let mut p = pointer();
        p.scroll_enabled = true;
        p.set_ready(Some(1));
        p
    }
    fn wheel(p: &mut Pointer, notch: i32) {
        p.axis_value(
            WEnum::Value(wl_pointer::Axis::VerticalScroll),
            f64::from(notch) * 10.0,
        );
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), notch);
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
    }
    #[test]
    fn repair_busy_tracks_partial_axis_without_changing_its_deadline() {
        let mut p = scroll_pointer();
        assert!(!p.repair_busy());
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        let started = p.frame_started;
        let count = p.frame_count;
        assert!(p.repair_busy());
        assert!(p.take_events().is_empty());
        assert!(p.repair_busy());
        p.set_ready(None);
        p.invalidate_mapping();
        p.presented(mapping());
        p.confirm(1);
        p.set_ready(Some(1));
        assert_eq!(p.frame_started, started);
        assert_eq!(p.frame_count, count);
        assert!(p.repair_busy());
        p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), 10.0);
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 1);
        p.finish_frame();
        assert!(p.repair_busy()); // Cancellation still awaits normal delivery.
        assert!(
            p.take_events()
                .iter()
                .all(|e| !matches!(e, ViewerEvent::WaylandPointer { .. }))
        );
        assert!(!p.repair_busy());
    }

    #[test]
    fn repair_busy_preserves_physical_hold_and_revocation() {
        assert!(!Pointer::default().repair_busy());
        let mut p = pointer();
        p.left_down = true;
        p.invalidate_mapping();
        p.take_events();
        assert!(p.repair_busy());
        assert!(p.left_down);
        p.left_down = false;
        assert!(!p.repair_busy());
        p.revoke("test local revocation");
        p.take_events();
        assert!(p.repair_busy());
    }

    #[test]
    fn scroll_late_source_and_motion_emit_one_action_only_at_frame() {
        for notch in [-1, 1] {
            let mut p = scroll_pointer();
            p.queue(p.scoped(Kind::Motion));
            wheel(&mut p, notch);
            p.position = Some((60.0, 40.0));
            p.queue(p.scoped(Kind::Motion));
            assert!(p.take_events().is_empty());
            p.finish_frame();
            let events = p.take_events();
            assert_eq!(events.len(), 1);
            assert!(
                matches!(events[0],ViewerEvent::WaylandPointer { kind,local_x:60,.. } if kind == if notch<0 {Kind::ScrollUp} else {Kind::ScrollDown})
            );
        }
    }
    #[test]
    fn scroll_strict_permission_shape_direction_and_button_fail_closed() {
        for case in 0..16 {
            let mut p = scroll_pointer();
            match case {
                0 => {
                    p.scroll_enabled = false;
                    wheel(&mut p, 1);
                }
                1 => {
                    p.axis_source(WEnum::Value(wl_pointer::AxisSource::Finger));
                }
                2 => {
                    p.axis_value(WEnum::Value(wl_pointer::Axis::HorizontalScroll), 10.0);
                }
                3 => {
                    p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 2);
                }
                4 => {
                    p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), f64::NAN);
                }
                5 => {
                    p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), 0.0);
                }
                6 => {
                    wheel(&mut p, 1);
                    p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
                }
                7 => {
                    p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), -1.0);
                    p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 1);
                    p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
                }
                8 => {
                    p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
                }
                9 => {
                    p.button(true);
                    p.button(false);
                    wheel(&mut p, 1);
                }
                10 => {
                    wheel(&mut p, 1);
                    p.button(true);
                }
                11 => p.axis_source(WEnum::Unknown(99)),
                12 => p.axis_value(WEnum::Unknown(99), 1.0),
                13 => p.axis_value(
                    WEnum::Value(wl_pointer::Axis::VerticalScroll),
                    f64::INFINITY,
                ),
                14 => {
                    wheel(&mut p, 1);
                    p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 1);
                }
                _ => {
                    wheel(&mut p, 1);
                    p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), 10.0);
                }
            }
            p.finish_frame();
            assert!(p.revoked.is_some(), "case {case}");
            assert_eq!(p.take_events(), vec![ViewerEvent::WaylandPointerRevoke]);
        }
    }
    #[test]
    fn scroll_old_fragments_never_bind_new_mapping_or_cross_frames() {
        let mut p = scroll_pointer();
        p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), 10.0);
        p.invalidate_mapping();
        p.presented(Mapping {
            sequence: 2,
            ..mapping()
        });
        p.confirm(2);
        p.set_ready(Some(2));
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 1);
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        p.finish_frame();
        assert!(
            p.take_events()
                .iter()
                .all(|e| !matches!(e, ViewerEvent::WaylandPointer { .. }))
        );
        assert!(p.revoked.is_none());
        wheel(&mut p, -1);
        p.finish_frame();
        assert!(matches!(
            p.take_events().as_slice(),
            [ViewerEvent::WaylandPointer {
                frame_sequence: 2,
                kind: Kind::ScrollUp,
                ..
            }]
        ));
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        p.finish_frame();
        assert!(p.revoked.is_some());
    }
    #[test]
    fn scroll_deadline_and_unready_input_do_not_accumulate() {
        let mut p = scroll_pointer();
        p.confirmed = None;
        wheel(&mut p, 1);
        p.finish_frame();
        assert_eq!(p.take_events(), vec![ViewerEvent::WaylandPointerCancel]);
        p.confirm(1);
        p.finish_frame();
        assert!(p.take_events().is_empty());
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        p.frame_started = Some(Instant::now() - TIMEOUT);
        p.finish_frame();
        assert!(p.revoked.is_some());
    }
    #[test]
    fn scroll_ready_transition_discards_old_fragments_and_preserves_deadline() {
        let mut p = scroll_pointer();
        p.set_ready(None);
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        let start = p.frame_started;
        p.set_ready(Some(1));
        p.set_ready(None);
        p.set_ready(Some(1));
        assert_eq!(p.frame_started, start);
        p.axis_value(WEnum::Value(wl_pointer::Axis::VerticalScroll), 10.0);
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::VerticalScroll), 1);
        p.finish_frame();
        assert!(
            p.take_events()
                .iter()
                .all(|e| !matches!(e, ViewerEvent::WaylandPointer { .. }))
        );
        wheel(&mut p, 1);
        p.finish_frame();
        assert!(matches!(
            p.take_events().as_slice(),
            [ViewerEvent::WaylandPointer {
                kind: Kind::ScrollDown,
                ..
            }]
        ));
    }
    #[test]
    fn pointer_fence_rejects_stale_duplicate_and_exhaustion() {
        let mut pointer = pointer();
        let id = pointer.begin_fence().unwrap();
        pointer.finish_fence(id);
        assert_eq!(pointer.fence_done, id);
        assert!(pointer.revoked.is_none());
        pointer.finish_fence(id);
        assert!(pointer.revoked.is_some());
        let mut pointer = self::pointer();
        pointer.begin_fence();
        pointer.finish_fence(0);
        assert!(pointer.revoked.is_some());
        let mut pointer = self::pointer();
        pointer.fence = u64::MAX;
        assert!(pointer.begin_fence().is_none());
        assert!(pointer.revoked.is_some());
    }
    #[test]
    fn mapping_uses_upload_integer_fit_and_rejects_blackbars_or_nonfinite() {
        let map = mapping(); // draw 101x50 at y=15
        assert_eq!(map_point(map, 0.0, 15.0), Some((0, 0, 0, 15)));
        assert_eq!(map_point(map, 100.9, 64.9), Some((158, 78, 100, 64)));
        for (x, y) in [
            (0.0, 14.99),
            (0.0, 65.0),
            (101.0, 20.0),
            (-0.1, 20.0),
            (f64::NAN, 20.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(map_point(map, x, y).is_none());
        }
    }
    #[test]
    fn unknown_release_and_new_frame_never_create_click_release() {
        let mut pointer = pointer();
        pointer.button(false);
        assert_eq!(
            pointer.frame_events.pop(),
            Some(ViewerEvent::WaylandPointerButtonCancel { pressed: false })
        );
        pointer.button(true);
        assert!(matches!(
            pointer.frame_events.pop(),
            Some(ViewerEvent::WaylandPointer {
                kind: Kind::LeftDown,
                ..
            })
        ));
        pointer.invalidate_mapping();
        pointer.presented(Mapping {
            sequence: 2,
            ..mapping()
        });
        pointer.confirm(2);
        pointer.button(false);
        // A generation change discards the entire unfinished input group.
        // Its Frame still delivers the actual release state, never a click.
        assert!(pointer.frame_events.is_empty());
        pointer.finish_frame();
        let events = pointer.take_events();
        assert_eq!(
            events.last(),
            Some(&ViewerEvent::WaylandPointerButtonCancel { pressed: false })
        );
        assert!(
            events
                .iter()
                .all(|event| matches!(event, ViewerEvent::WaylandPointerButtonCancel { .. }))
        );
        assert!(!pointer.left_down);
    }
    #[test]
    fn invalidation_preserves_unpolled_release_as_cancel_state() {
        let mut pointer = pointer();
        pointer.button(true);
        pointer.frame_events.clear(); // Down was delivered to ClientPointer.
        pointer.button(false); // Actual Up observed but not yet polled.
        assert!(!pointer.left_down);
        pointer.invalidate_mapping();
        assert_eq!(
            pointer.take_events(),
            vec![ViewerEvent::WaylandPointerButtonCancel { pressed: false }]
        );
        assert!(pointer.observed_down.is_none());
        let mut pointer = self::pointer();
        pointer.button(true);
        pointer.invalidate_mapping();
        assert_eq!(
            pointer.take_events(),
            vec![ViewerEvent::WaylandPointerButtonCancel { pressed: true }]
        );
    }
    #[test]
    fn activation_aba_and_queue_overflow_permanently_revoke() {
        let mut pointer = pointer();
        pointer.activation(&u32::from(xdg_toplevel::State::Activated).to_ne_bytes());
        pointer.button(true);
        pointer.button(false);
        pointer.activation(&[]);
        pointer.activation(&u32::from(xdg_toplevel::State::Activated).to_ne_bytes());
        assert_eq!(
            pointer.take_events(),
            vec![ViewerEvent::WaylandPointerRevoke]
        );
        assert!(!pointer.locally_ready());
        let mut pointer = self::pointer();
        for _ in 0..=MAX_EVENTS {
            pointer.queue(ViewerEvent::WaylandPointerCancel);
        }
        assert!(pointer.revoked.is_some());
        assert!(pointer.frame_events.is_empty());
    }
    fn horizontal_wheel(p: &mut Pointer, notch: i32) {
        p.axis_value(
            WEnum::Value(wl_pointer::Axis::HorizontalScroll),
            f64::from(notch) * 10.0,
        );
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::HorizontalScroll), notch);
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
    }
    #[test]
    fn horizontal_scroll_independent_permission_and_one_action() {
        for notch in [-1, 1] {
            let mut p = scroll_pointer();
            horizontal_wheel(&mut p, notch);
            assert!(p.revoked.is_some(), "vertical does not grant horizontal");
            let mut p = pointer();
            p.horizontal_enabled = true;
            p.set_ready(Some(1));
            p.queue(p.scoped(Kind::Motion));
            horizontal_wheel(&mut p, notch);
            p.finish_frame();
            let events = p.take_events();
            assert_eq!(events.len(), 1);
            assert!(
                matches!(events[0],ViewerEvent::WaylandPointer{kind,..} if kind==if notch<0 {Kind::ScrollLeft}else{Kind::ScrollRight})
            );
            wheel(&mut p, notch);
            assert!(p.revoked.is_some(), "horizontal does not grant vertical");
        }
    }
    #[test]
    fn horizontal_scroll_mixed_axis_and_held_rejected() {
        for reverse in [false, true] {
            let mut p = scroll_pointer();
            p.horizontal_enabled = true;
            let (a, b) = if reverse {
                (
                    wl_pointer::Axis::VerticalScroll,
                    wl_pointer::Axis::HorizontalScroll,
                )
            } else {
                (
                    wl_pointer::Axis::HorizontalScroll,
                    wl_pointer::Axis::VerticalScroll,
                )
            };
            p.axis_value(WEnum::Value(a), 10.0);
            p.axis_discrete(WEnum::Value(b), 1);
            p.finish_frame();
            assert!(p.revoked.is_some());
        }
        let mut p = scroll_pointer();
        p.horizontal_enabled = true;
        p.left_down = true;
        horizontal_wheel(&mut p, 1);
        p.finish_frame();
        assert!(p.revoked.is_some());
    }
    #[test]
    fn horizontal_scroll_ready_drops_complete_and_partial_old_groups() {
        let mut p = scroll_pointer();
        p.horizontal_enabled = true;
        horizontal_wheel(&mut p, 1);
        p.finish_frame();
        p.set_ready(None);
        p.set_ready(Some(1));
        assert!(p.take_events().is_empty());
        p.axis_value(WEnum::Value(wl_pointer::Axis::HorizontalScroll), 10.0);
        p.set_ready(None);
        p.set_ready(Some(1));
        p.axis_discrete(WEnum::Value(wl_pointer::Axis::HorizontalScroll), 1);
        p.axis_source(WEnum::Value(wl_pointer::AxisSource::Wheel));
        p.finish_frame();
        assert!(!p.take_events().iter().any(|e| matches!(
            e,
            ViewerEvent::WaylandPointer {
                kind: Kind::ScrollLeft | Kind::ScrollRight,
                ..
            }
        )));
        assert!(p.revoked.is_none());
        horizontal_wheel(&mut p, -1);
        p.finish_frame();
        assert!(p.take_events().iter().any(|e| matches!(
            e,
            ViewerEvent::WaylandPointer {
                kind: Kind::ScrollLeft,
                ..
            }
        )));
    }
}
