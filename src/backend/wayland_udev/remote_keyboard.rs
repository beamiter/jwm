//! Explicit trusted-local, single-surface, keyboard-only capability.
//! No global virtual-keyboard authority, focus changes, pointer or clipboard.
use super::image_copy_capture::{
    CaptureAuthority, CaptureFrameData, CaptureSource, ImageCaptureSourceData, authority_is_current,
};
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote_keyboard_protocol::server::{
    jwm_remote_keyboard_lease_v1::{self, JwmRemoteKeyboardLeaseV1},
    jwm_remote_keyboard_manager_v1::{self, JwmRemoteKeyboardManagerV1},
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
const TTL: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(50);
pub(crate) const DENIED: u32 = 1;
pub(crate) const SOURCE: u32 = 2;
pub(crate) const SEAT: u32 = 3;
pub(crate) const LOCAL: u32 = 4;
const EXPIRED: u32 = 5;
const SEQUENCE: u32 = 6;
const QUOTA: u32 = 7;
#[derive(Default)]
pub(crate) struct RemoteKeyboardState {
    next_epoch: u32,
    active: Option<Lease>,
}
struct Lease {
    resource: JwmRemoteKeyboardLeaseV1,
    epoch: u32,
    authority: Arc<CaptureAuthority>,
    snapshot: AuthorizedKeyboardSnapshot,
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
    dh.create_global::<JwmWaylandState, JwmRemoteKeyboardManagerV1, _>(1, ManagerData);
}
fn current(state: &JwmWaylandState, lease: &Lease) -> bool {
    lease.resource.is_alive()
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
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state.remote_keyboard.active.is_some()
}
pub(crate) fn revoke(state: &mut JwmWaylandState, reason: u32) {
    super::remote_pointer::revoke(state, reason);
    if let Some(lease) = state.remote_keyboard.active.take() {
        finish(state, lease, reason, true);
    }
}
/// SeatHandler::focus_changed is invoked under Smithay's keyboard lock. It
/// must not reenter that lock. wl_keyboard.leave ends the old focus's keys.
pub(crate) fn focus_changed(state: &mut JwmWaylandState) {
    super::remote_control::focus_changed(state);
    super::remote_pointer::revoke(state, SEAT);
    if let Some(lease) = state.remote_keyboard.active.take() {
        finish(state, lease, SEAT, false);
    }
}
pub(crate) fn revoke_for_window(
    state: &mut JwmWaylandState,
    window: crate::backend::common_define::WindowId,
) {
    super::remote_pointer::revoke_for_window(state, window);
    if state
        .remote_keyboard
        .active
        .as_ref()
        .is_some_and(|lease| lease.authority.window == window)
    {
        revoke(state, SOURCE);
    }
}
fn watchdog(state: &mut JwmWaylandState, epoch: u32) -> bool {
    let Some(lease) = state.remote_keyboard.active.as_ref() else {
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
impl GlobalDispatch<JwmRemoteKeyboardManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<JwmRemoteKeyboardManagerV1>,
        _data: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ManagerData);
    }
    fn can_view(client: Client, _data: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemoteKeyboardManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &JwmRemoteKeyboardManagerV1,
        request: jwm_remote_keyboard_manager_v1::Request,
        _data: &ManagerData,
        _dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let jwm_remote_keyboard_manager_v1::Request::CreateLease { id, source } = request {
            let epoch = state.remote_keyboard.next_epoch.checked_add(1).unwrap_or(0);
            let resource = init.init(id, LeaseData { epoch });
            if epoch == 0 {
                resource.revoked(DENIED);
                return;
            }
            state.remote_keyboard.next_epoch = epoch;
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
                || !authority.consume_keyboard_attempt()
                || state.remote_keyboard.active.is_some()
                || super::remote_pointer::active(state)
                || super::remote_control::active(state)
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
            state.remote_keyboard.active = Some(Lease {
                resource: resource.clone(),
                epoch,
                authority,
                snapshot,
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
impl Dispatch<JwmRemoteKeyboardLeaseV1, LeaseData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &JwmRemoteKeyboardLeaseV1,
        request: jwm_remote_keyboard_lease_v1::Request,
        data: &LeaseData,
        _dh: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        let Some(mut lease) = state.remote_keyboard.active.take() else {
            return;
        };
        if lease.resource != *resource || lease.epoch != data.epoch {
            state.remote_keyboard.active = Some(lease);
            return;
        }
        if matches!(request, jwm_remote_keyboard_lease_v1::Request::Destroy) {
            finish(state, lease, DENIED, true);
            return;
        }
        if !current(state, &lease) {
            finish(state, lease, SEAT, true);
            return;
        }
        match request {
            jwm_remote_keyboard_lease_v1::Request::Arm { epoch, frame } => {
                let valid=epoch==lease.epoch && !lease.armed && frame.is_alive() && frame.client().map(|client|client.id())==resource.client().map(|client|client.id()) && frame.data::<CaptureFrameData>().is_some_and(|data|data.ready.load(Ordering::Acquire) && matches!(data.source.as_ref(),Some(CaptureSource::Toplevel(authority)) if Arc::ptr_eq(authority,&lease.authority)) && data.session.upgrade().is_ok_and(|session|session.is_alive() && session.data::<super::image_copy_capture::CaptureSessionData>().is_some_and(|data|!data.stopped.load(Ordering::Acquire))));
                if !valid {
                    finish(state, lease, SOURCE, true);
                    return;
                }
                lease.armed = true;
                resource.armed(epoch);
            }
            jwm_remote_keyboard_lease_v1::Request::Key {
                epoch,
                sequence,
                key,
                state: key_state,
            } => {
                if !lease.armed
                    || epoch != lease.epoch
                    || sequence != lease.sequence.saturating_add(1)
                    || sequence > 60000
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
                lease.sequence = sequence;
                resource.accepted(epoch, sequence);
            }
            _ => {}
        }
        state.remote_keyboard.active = Some(lease);
        state.request_client_flush();
    }
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemoteKeyboardLeaseV1,
        _data: &LeaseData,
    ) {
        if state
            .remote_keyboard
            .active
            .as_ref()
            .is_some_and(|lease| lease.resource == *resource)
        {
            revoke(state, DENIED);
        }
    }
}
#[cfg(test)]
#[path = "remote_keyboard_tests.rs"]
mod tests;

/// Read-only evidence of create-lease requests, including rejected attempts.
/// The production handler increments next_epoch before checking source/admission.
#[cfg(all(test, feature = "backend-wayland-udev", feature = "remote-wayland"))]
pub(crate) fn attempts_for_test(state: &JwmWaylandState) -> u32 {
    state.remote_keyboard.next_epoch
}
