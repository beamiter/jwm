//! Explicit single-surface control bundle: one deadline, sequence and revocation domain.
//! Scroll requires an additional explicit v2 request. No drag, focus changes,
//! global input or modifier-click.
use super::image_copy_capture::{
    CaptureAuthority, CaptureFrameData, CaptureSource, ImageCaptureSourceData, authority_is_current,
};
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote_control_protocol::server::{
    jwm_remote_control_lease_v1::{self, JwmRemoteControlLeaseV1},
    jwm_remote_control_manager_v1::{self, JwmRemoteControlManagerV1},
};
use smithay::backend::input::{InputTime, KeyState};
use smithay::input::keyboard::{AuthorizedKeyboardSnapshot, KeymapFile, ModifiersState};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::SERIAL_COUNTER;
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use xkbcommon::xkb;
use smithay::input::pointer::AuthorizedPointerSnapshot;
use smithay::reexports::wayland_server::{Weak, protocol::wl_surface::WlSurface};
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use super::remote_pointer::{Mapping, mapping, point_allowed, unconstrained};
const TTL: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(50);
pub(crate) const DENIED: u32 = 1;
pub(crate) const SOURCE: u32 = 2;
pub(crate) const SEAT: u32 = 3;

const EXPIRED: u32 = 5;
const SEQUENCE: u32 = 6;
const QUOTA: u32 = 7;
#[derive(Default)]
pub(crate) struct RemoteControlState {
    next_epoch: u32,
    active: Option<Lease>,
}
struct Lease {
    resource: JwmRemoteControlLeaseV1,
    epoch: u32,
    scroll_axes: u32,
    authority: Arc<CaptureAuthority>,
    snapshot: AuthorizedKeyboardSnapshot,
    pointer: AuthorizedPointerSnapshot,
    mapping: Mapping,
    source: Weak<ExtImageCaptureSourceV1>,
    last_ready: u64,
    last_action: Option<Instant>,
    pointer_tokens: f64,
    pointer_refill: Instant,
    xkb: xkb::State,
    held: BTreeSet<u32>,
    armed: bool,
    sequence: u32,
    created: Instant,
    rate_start: Instant,
    rate: u32,
    tokens: f64,
    refill: Instant,
}
pub struct ManagerData;
pub struct LeaseData {
    epoch: u32,
}
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, JwmRemoteControlManagerV1, _>(3, ManagerData);
}
fn current(state: &JwmWaylandState, lease: &Lease) -> bool {
    lease.resource.is_alive()
        && lease.source.upgrade().is_ok_and(|s| s.is_alive())
        && unconstrained(state, &lease.authority.surface)
        && mapping(&lease.authority).as_ref() == Some(&lease.mapping)
        && state
            .seat
            .get_pointer()
            .is_some_and(|p| p.authorized_pointer_snapshot_is_current(&lease.pointer))
        && lease.created.elapsed() < TTL
        && authority_is_current(state, &lease.authority)
        && state.remote_keyboard_seat_eligible()
        && state.remote_keyboard_target_eligible(&lease.authority)
        && state.seat.get_keyboard().is_some_and(|keyboard| {
            keyboard.authorized_keyboard_snapshot_is_current(&lease.snapshot)
        })
}
fn finish(state: &mut JwmWaylandState, mut lease: Lease, reason: u32, cleanup: bool) {
    if cleanup && let Some(keyboard) = state.seat.get_keyboard() {
        let keys: Vec<_> = lease.held.iter().map(|code| (*code + 8).into()).collect();
        keyboard.cleanup_authorized_keyboard(
            &lease.snapshot,
            &keys,
            SERIAL_COUNTER.next_serial(),
            InputTime::now(),
        );
    }
    lease.held.clear();
    if lease.resource.is_alive() {
        lease.resource.revoked(reason);
    }
    state.request_client_flush();
}
/// Read-only native lease timing; does not refresh or alter authority.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn lease_timing_for_test(state: &JwmWaylandState) -> Option<(u32, Instant, Instant)> {
    state
        .remote_control
        .active
        .as_ref()
        .map(|lease| (lease.epoch, lease.created, lease.created + TTL))
}
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state.remote_control.active.is_some()
}
pub(crate) fn revoke(state: &mut JwmWaylandState, reason: u32) {
    if let Some(lease) = state.remote_control.active.take() {
        finish(state, lease, reason, true);
    }
}
/// SeatHandler::focus_changed is invoked under Smithay's keyboard lock. It
/// must not reenter that lock. wl_keyboard.leave ends the old focus's keys.
pub(crate) fn focus_changed(state: &mut JwmWaylandState) {
    if let Some(lease) = state.remote_control.active.take() {
        finish(state, lease, SEAT, false);
    }
}
pub(crate) fn revoke_for_window(
    state: &mut JwmWaylandState,
    window: crate::backend::common_define::WindowId,
) {
    if state
        .remote_control
        .active
        .as_ref()
        .is_some_and(|lease| lease.authority.window == window)
    {
        revoke(state, SOURCE);
    }
}
fn watchdog(state: &mut JwmWaylandState, epoch: u32) -> bool {
    let Some(lease) = state.remote_control.active.as_ref() else {
        return false;
    };
    if lease.epoch != epoch {
        return false;
    }
    if !current(state, lease) {
        revoke(state, EXPIRED);
        return false;
    }
    true
}
impl GlobalDispatch<JwmRemoteControlManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<JwmRemoteControlManagerV1>,
        _data: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ManagerData);
    }
    fn can_view(client: Client, _data: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemoteControlManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &JwmRemoteControlManagerV1,
        request: jwm_remote_control_manager_v1::Request,
        _data: &ManagerData,
        _dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        // Interface version is not consent: only the additional request grants wheel.
        let (id, source, scroll_axes, axes_valid) = match request {
            jwm_remote_control_manager_v1::Request::CreateLease { id, source } => {
                (id, source, 0, true)
            }
            jwm_remote_control_manager_v1::Request::CreateLeaseWithScroll { id, source } => {
                (id, source, 1, true)
            }
            jwm_remote_control_manager_v1::Request::CreateLeaseWithScrollAxes {
                id,
                source,
                axes,
            } => (id, source, axes, matches!(axes, 1..=3)),
            _ => return,
        };
        {
            let epoch = state.remote_control.next_epoch.checked_add(1).unwrap_or(0);
            let resource = init.init(id, LeaseData { epoch });
            if epoch == 0 {
                resource.revoked(DENIED);
                return;
            }
            state.remote_control.next_epoch = epoch;
            let authority = match source
                .data::<ImageCaptureSourceData>()
                .and_then(|data| data.source.as_ref())
            {
                Some(CaptureSource::Toplevel(authority)) => authority.clone(),
                _ => {
                    resource.revoked(DENIED);
                    return;
                }
            };
            if source.client().as_ref().map(Client::id) != Some(client.id())
                || !authority.consume_control_attempt()
                || !axes_valid
                || state.remote_control.active.is_some()
                || super::remote_pointer::active(state)
                || super::remote_keyboard::active(state)
                || super::remote_text::active(state)
                || !authority_is_current(state, &authority)
                || !state.remote_keyboard_seat_eligible()
                || !state.remote_keyboard_target_eligible(&authority)
            {
                resource.revoked(DENIED);
                return;
            }
            let Some(keyboard) = state.seat.get_keyboard() else {
                resource.revoked(DENIED);
                return;
            };
            let Some(snapshot) = keyboard.snapshot_authorized_keyboard(&authority.surface) else {
                resource.revoked(SEAT);
                return;
            };
            let Some(pointer) = state
                .seat
                .get_pointer()
                .and_then(|p| p.snapshot_authorized_pointer(&authority.surface))
            else {
                resource.revoked(SEAT);
                return;
            };
            let Some(mapping) = mapping(&authority) else {
                resource.revoked(SOURCE);
                return;
            };
            if !unconstrained(state, &authority.surface)
                || !keyboard.authorized_keyboard_snapshot_is_current(&snapshot)
            {
                resource.revoked(SEAT);
                return;
            }
            if snapshot.keymap_text().len() > 1024 * 1024 {
                resource.revoked(DENIED);
                return;
            }
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let Some(map) = xkb::Keymap::new_from_string(
                &context,
                snapshot.keymap_text().to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) else {
                resource.revoked(DENIED);
                return;
            };
            let keymap = KeymapFile::new(&map);
            if keymap
                .with_fd(true, |fd, size| resource.keymap(1, fd, size as u32))
                .is_err()
            {
                resource.revoked(DENIED);
                return;
            }
            let now = Instant::now();
            state.remote_control.active = Some(Lease {
                resource: resource.clone(),
                epoch,
                scroll_axes,
                authority,
                snapshot,
                pointer,
                mapping,
                source: source.downgrade(),
                last_ready: 0,
                last_action: None,
                pointer_tokens: 8.0,
                pointer_refill: now,
                xkb: xkb::State::new(&map),
                held: BTreeSet::new(),
                armed: false,
                sequence: 0,
                created: now,
                rate_start: now,
                rate: 0,
                tokens: 32.0,
                refill: now,
            });
            let timer = Timer::from_duration(TICK);
            if state
                .loop_handle
                .insert_source(timer, move |_, _, state| {
                    if watchdog(state, epoch) {
                        TimeoutAction::ToDuration(TICK)
                    } else {
                        TimeoutAction::Drop
                    }
                })
                .is_err()
            {
                revoke(state, DENIED);
                return;
            }
            resource.granted(epoch, epoch, TTL.as_millis() as u32);
            state.request_client_flush();
        }
    }
}
impl Dispatch<JwmRemoteControlLeaseV1, LeaseData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &JwmRemoteControlLeaseV1,
        request: jwm_remote_control_lease_v1::Request,
        data: &LeaseData,
        _dh: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        let Some(mut lease) = state.remote_control.active.take() else {
            return;
        };
        if lease.resource != *resource || lease.epoch != data.epoch {
            state.remote_control.active = Some(lease);
            return;
        }
        if matches!(request, jwm_remote_control_lease_v1::Request::Destroy) {
            finish(state, lease, DENIED, true);
            return;
        }
        if !current(state, &lease) {
            finish(state, lease, SEAT, true);
            return;
        }
        match request {
            jwm_remote_control_lease_v1::Request::Arm { epoch, frame } => {
                let valid=epoch==lease.epoch && !lease.armed && frame.is_alive() && frame.client().map(|client|client.id())==resource.client().map(|client|client.id()) && frame.data::<CaptureFrameData>().is_some_and(|data|data.ready.load(Ordering::Acquire) && matches!(data.source.as_ref(),Some(CaptureSource::Toplevel(authority)) if Arc::ptr_eq(authority,&lease.authority)) && data.session.upgrade().is_ok_and(|session|session.is_alive() && session.data::<super::image_copy_capture::CaptureSessionData>().is_some_and(|data|!data.stopped.load(Ordering::Acquire))));
                if !valid {
                    finish(state, lease, SOURCE, true);
                    return;
                }
                lease.armed = true;
                resource.armed(epoch);
            }
            jwm_remote_control_lease_v1::Request::Key {
                epoch,
                sequence,
                key,
                state: key_state,
            } => {
                if !lease.armed
                    || epoch != lease.epoch
                    || !valid_sequence(lease.sequence, sequence)
                    || !crate::remote_keyboard_protocol::keyboard_key_allowed(key)
                    || key_state > 1
                    || (key_state == 1) == lease.held.contains(&key)
                {
                    finish(state, lease, SEQUENCE, true);
                    return;
                }
                if lease.rate_start.elapsed() >= Duration::from_secs(1) {
                    lease.rate_start = Instant::now();
                    lease.rate = 0;
                }
                lease.rate += 1;
                let now = Instant::now();
                lease.tokens = (lease.tokens
                    + now.duration_since(lease.refill).as_secs_f64() * 120.0)
                    .min(32.0);
                lease.refill = now;
                if lease.tokens < 1.0 {
                    finish(state, lease, QUOTA, true);
                    return;
                }
                lease.tokens -= 1.0;
                if lease.rate > 120 || (key_state == 1 && lease.held.len() >= 32) {
                    finish(state, lease, QUOTA, true);
                    return;
                }
                let pressed = key_state == 1;
                let code = (key + 8).into();
                lease.xkb.update_key(
                    code,
                    if pressed {
                        xkb::KeyDirection::Down
                    } else {
                        xkb::KeyDirection::Up
                    },
                );
                let mut modifiers = ModifiersState::default();
                modifiers.update_with(&lease.xkb);
                if modifiers.serialized.latched != 0
                    || modifiers.serialized.locked != 0
                    || modifiers.serialized.layout_effective != 0
                {
                    finish(state, lease, SEQUENCE, true);
                    return;
                }
                let keyboard = state.seat.get_keyboard().expect("validated keyboard");
                if !keyboard.deliver_authorized_keyboard(
                    &lease.snapshot,
                    modifiers,
                    (
                        code,
                        if pressed {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        },
                        SERIAL_COUNTER.next_serial(),
                        InputTime::now(),
                    ),
                ) {
                    finish(state, lease, SEAT, true);
                    return;
                }
                if pressed {
                    lease.held.insert(key);
                } else {
                    lease.held.remove(&key);
                }
                lease.last_action = Some(Instant::now());
                lease.sequence = sequence;
                resource.accepted(epoch, sequence);
            }
            jwm_remote_control_lease_v1::Request::Action {
                epoch,
                sequence,
                frame,
                kind,
                x,
                y,
            } => {
                if !lease.armed
                    || !lease.held.is_empty()
                    || epoch != lease.epoch
                    || !valid_sequence(lease.sequence, sequence)
                    || !action_allowed(kind, lease.scroll_axes)
                    || x >= lease.authority.width
                    || y >= lease.authority.height
                    || !point_allowed(&lease.mapping, x, y)
                {
                    finish(state, lease, SEQUENCE, true);
                    return;
                }
                let stamp = frame.data::<CaptureFrameData>().and_then(|data| {
                    let valid = frame.is_alive()
                        && frame.client().map(|c| c.id()) == resource.client().map(|c| c.id())
                        && data.ready.load(Ordering::Acquire)
                        && matches!(data.source.as_ref(), Some(CaptureSource::Toplevel(a)) if Arc::ptr_eq(a, &lease.authority))
                        && data.session.upgrade().is_ok_and(|s| s.is_alive() && s.data::<super::image_copy_capture::CaptureSessionData>().is_some_and(|d| !d.stopped.load(Ordering::Acquire)));
                    valid.then(|| *data.ready_stamp.lock().unwrap()).flatten()
                });
                let Some((serial, ready_at)) = stamp else {
                    finish(state, lease, SOURCE, true);
                    return;
                };
                let now = Instant::now();
                if serial <= lease.last_ready
                    || now
                        .checked_duration_since(ready_at)
                        .is_none_or(|age| age > Duration::from_secs(1))
                    || lease.last_action.is_some_and(|at| ready_at <= at)
                {
                    finish(state, lease, SOURCE, true);
                    return;
                }
                lease.pointer_tokens = (lease.pointer_tokens
                    + now.duration_since(lease.pointer_refill).as_secs_f64() * 30.0)
                    .min(8.0);
                lease.pointer_refill = now;
                if lease.pointer_tokens < 1.0 {
                    finish(state, lease, QUOTA, true);
                    return;
                }
                lease.pointer_tokens -= 1.0;
                if !state.seat.get_pointer().is_some_and(|p| {
                    if kind >= 2 {
                        p.send_authorized_scroll_axis(
                            &lease.pointer,
                            f64::from(x),
                            f64::from(y),
                            if kind >= 4 {
                                smithay::backend::input::Axis::Horizontal
                            } else {
                                smithay::backend::input::Axis::Vertical
                            },
                            if matches!(kind, 2 | 4) { -1 } else { 1 },
                            InputTime::now(),
                        )
                    } else {
                        p.send_authorized_pointer(
                            &lease.pointer,
                            f64::from(x),
                            f64::from(y),
                            kind == 1,
                            SERIAL_COUNTER.next_serial(),
                            InputTime::now(),
                        )
                    }
                }) {
                    finish(state, lease, SEAT, true);
                    return;
                }
                lease.sequence = sequence;
                lease.last_ready = serial;
                lease.last_action = Some(Instant::now());
                resource.accepted(epoch, sequence);
            }
            _ => {}
        }
        state.remote_control.active = Some(lease);
        state.request_client_flush();
    }
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemoteControlLeaseV1,
        _data: &LeaseData,
    ) {
        if state
            .remote_control
            .active
            .as_ref()
            .is_some_and(|lease| lease.resource == *resource)
        {
            revoke(state, DENIED);
        }
    }
}

fn action_allowed(kind: u32, axes: u32) -> bool {
    axes <= 3
        && (matches!(kind, 0 | 1)
            || (axes & 1 != 0 && matches!(kind, 2 | 3))
            || (axes & 2 != 0 && matches!(kind, 4 | 5)))
}
fn valid_sequence(previous: u32, next: u32) -> bool {
    previous.checked_add(1) == Some(next) && next <= 60000
}
pub(crate) fn surface_committed(state: &mut JwmWaylandState, surface: &WlSurface) {
    if state.remote_control.active.as_ref().is_some_and(|l| {
        l.authority.surface == *surface && mapping(&l.authority).as_ref() != Some(&l.mapping)
    }) {
        revoke(state, SOURCE);
    }
}
pub(crate) fn new_subsurface(state: &mut JwmWaylandState, surface: &WlSurface) {
    if state
        .remote_control
        .active
        .as_ref()
        .is_some_and(|l| l.authority.surface == *surface)
    {
        revoke(state, SOURCE);
    }
}
pub(crate) fn source_destroyed(state: &mut JwmWaylandState, source: &ExtImageCaptureSourceV1) {
    if state
        .remote_control
        .active
        .as_ref()
        .is_some_and(|l| l.source.id() == source.id())
    {
        revoke(state, SOURCE);
    }
}
#[cfg(test)]
mod primitive_tests {
    use super::*;
    #[test]
    fn wheel_requires_explicit_grant_and_unknown_actions_always_fail() {
        for axes in 0..=3 {
            assert!(action_allowed(0, axes));
            assert!(action_allowed(1, axes));
            for kind in [2, 3] {
                assert_eq!(action_allowed(kind, axes), axes & 1 != 0);
            }
            for kind in [4, 5] {
                assert_eq!(action_allowed(kind, axes), axes & 2 != 0);
            }
            for kind in [6, 120, u32::MAX] {
                assert!(!action_allowed(kind, axes));
            }
        }
        for axes in [4, 7, u32::MAX] {
            for kind in 0..6 {
                assert!(!action_allowed(kind, axes));
            }
        }
    }
    #[test]
    fn shared_sequence_rejects_replay_gap_and_exhaustion() {
        assert!(valid_sequence(0, 1));
        assert!(valid_sequence(59999, 60000));
        assert!(!valid_sequence(1, 1));
        assert!(!valid_sequence(1, 3));
        assert!(!valid_sequence(60000, 60001));
        assert!(!valid_sequence(u32::MAX, 0));
    }
}
#[cfg(all(test, feature = "backend-wayland-udev", feature = "remote-wayland"))]
pub(crate) fn attempts_for_test(state: &JwmWaylandState) -> u32 {
    state.remote_control.next_epoch
}
#[cfg(test)]
#[path = "remote_control_tests.rs"]
mod tests;
