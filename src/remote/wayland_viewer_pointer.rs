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
pub(super) struct Pointer {
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
    pub(super) fn revoke(&mut self, reason: &'static str) {
        if !self.requested {
            return;
        }
        self.revoked.get_or_insert(reason);
        self.mapping = None;
        self.confirmed = None;
        self.observed_down = None;
        self.frame_events.clear();
        self.completed.clear();
        self.frame_started = None;
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
        self.observed_down = None;
        // Cancel any old-frame local candidate without inventing a release.
        self.frame_events.clear();
        self.completed.clear();
        self.frame_started = None;
        if self.requested && self.revoked.is_none() {
            self.completed
                .push(ViewerEvent::WaylandPointerButtonCancel {
                    pressed: self.left_down,
                });
        }
    }
    pub(super) fn presented(&mut self, mapping: Mapping) {
        if self.requested && self.revoked.is_none() {
            self.mapping = Some(mapping);
            self.confirmed = None;
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
        if self.frame_events.len() + self.completed.len() >= MAX_EVENTS {
            self.revoke("Wayland pointer queue overflow");
            return;
        }
        self.frame_started.get_or_insert_with(Instant::now);
        self.frame_events.push(event);
    }
    fn button(&mut self, pressed: bool) {
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
            wl_pointer::Event::Frame => {
                pointer.frame_started = None;
                pointer.completed.append(&mut pointer.frame_events);
            }
            _ => pointer.revoke("unsupported Wayland pointer button or axis"),
        }
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
        assert_eq!(
            pointer.frame_events.pop(),
            Some(ViewerEvent::WaylandPointerButtonCancel { pressed: false })
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
}
