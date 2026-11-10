//! One-shot trusted-local text delivery to an existing exact text-input context.
//! This is not a compositor IME, clipboard operation, or keyboard emulation.
use super::image_copy_capture::{CaptureAuthority, CaptureFrameData, CaptureSessionData, CaptureSource, ImageCaptureSourceData, authority_is_current};
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote_text_protocol::server::{jwm_remote_text_lease_v1::{self, JwmRemoteTextLeaseV1}, jwm_remote_text_manager_v1::{self, JwmRemoteTextManagerV1}};
use smithay::input::keyboard::AuthorizedKeyboardSnapshot;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak};
use smithay::wayland::text_input::{AuthorizedTextTarget, TextInputSeat};
use std::{sync::{Arc, atomic::Ordering}, time::{Duration, Instant}};
const TTL: Duration = Duration::from_secs(30);
const FRAME_AGE: Duration = Duration::from_secs(1);
pub(crate) const DENIED: u32 = 1;
pub(crate) const SOURCE: u32 = 2;
pub(crate) const CONTEXT: u32 = 3;
const EXPIRED: u32 = 5;
const INVALID: u32 = 6;
#[derive(Default)]
pub(crate) struct RemoteTextState {
    next_epoch: u32,
    active: Option<Lease>,
}
struct Lease {
    source: Weak<ExtImageCaptureSourceV1>,
    resource: JwmRemoteTextLeaseV1,
    epoch: u32,
    authority: Arc<CaptureAuthority>,
    target: AuthorizedTextTarget,
    keyboard: AuthorizedKeyboardSnapshot,
    created: Instant,
}
pub struct ManagerData;
pub struct LeaseData {
    epoch: u32,
}
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, JwmRemoteTextManagerV1, _>(1, ManagerData);
}
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state.remote_text.active.is_some()
}
// Safe even from focus_changed while the keyboard lock is held: no seat calls.
pub(crate) fn revoke(state: &mut JwmWaylandState, reason: u32) {
    if let Some(lease) = state.remote_text.active.take() {
        if lease.resource.is_alive() {
            lease.resource.revoked(reason);
        }
        state.request_client_flush();
    }
}
pub(crate) fn revoke_for_window(
    state: &mut JwmWaylandState,
    window: crate::backend::common_define::WindowId,
) {
    if state
        .remote_text
        .active
        .as_ref()
        .is_some_and(|l| l.authority.window == window)
    {
        revoke(state, SOURCE);
    }
}
pub(crate) fn source_destroyed(state: &mut JwmWaylandState, source: &ExtImageCaptureSourceV1) {
    if state
        .remote_text
        .active
        .as_ref()
        .is_some_and(|l| l.source.id() == source.id())
    {
        revoke(state, SOURCE);
    }
}
fn current(state: &JwmWaylandState, lease: &Lease) -> bool {
    lease.resource.is_alive()
        && lease.source.upgrade().is_ok_and(|s| s.is_alive())
        && lease.created.elapsed() < TTL
        && authority_is_current(state, &lease.authority)
        && state.remote_keyboard_seat_eligible()
        && state.remote_keyboard_target_eligible(&lease.authority)
        && !super::remote_keyboard::active(state)
        && !super::remote_pointer::active(state)
        && !super::remote_control::active(state)
        && state
            .seat
            .get_keyboard()
            .is_some_and(|k| k.authorized_keyboard_snapshot_is_current(&lease.keyboard))
        && state
            .seat
            .text_input()
            .authorized_text_is_current(&lease.target)
}
fn watchdog(state: &mut JwmWaylandState, epoch: u32) -> bool {
    let Some(lease) = state.remote_text.active.as_ref() else {
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
fn valid_text(bytes: &[u8]) -> Option<&str> {
    if bytes.is_empty() || bytes.len() > 4000 || bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}
impl GlobalDispatch<JwmRemoteTextManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<JwmRemoteTextManagerV1>,
        _data: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ManagerData);
    }
    fn can_view(client: Client, _data: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemoteTextManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &JwmRemoteTextManagerV1,
        request: jwm_remote_text_manager_v1::Request,
        _data: &ManagerData,
        _dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let jwm_remote_text_manager_v1::Request::CreateLease { id, source } = request else {
            return;
        };
        let epoch = state.remote_text.next_epoch.checked_add(1).unwrap_or(0);
        let resource = init.init(id, LeaseData { epoch });
        if epoch == 0 {
            resource.revoked(DENIED);
            return;
        }
        state.remote_text.next_epoch = epoch;
        let Some(CaptureSource::Toplevel(authority)) = source
            .data::<ImageCaptureSourceData>()
            .and_then(|d| d.source.as_ref())
        else {
            resource.revoked(DENIED);
            return;
        };
        let authority = Arc::clone(authority);
        if source.client().map(|c| c.id()) != Some(client.id())
            || !authority.consume_text_attempt()
            || active(state)
            || super::remote_keyboard::active(state)
            || super::remote_pointer::active(state)
            || super::remote_control::active(state)
            || !authority_is_current(state, &authority)
            || !state.remote_keyboard_seat_eligible()
            || !state.remote_keyboard_target_eligible(&authority)
        {
            resource.revoked(DENIED);
            return;
        }
        let Some(keyboard) = state
            .seat
            .get_keyboard()
            .and_then(|k| k.snapshot_authorized_keyboard(&authority.surface))
        else {
            resource.revoked(CONTEXT);
            return;
        };
        let Some(target) = state
            .seat
            .text_input()
            .snapshot_authorized_text(&authority.surface)
        else {
            resource.revoked(CONTEXT);
            return;
        };
        state.remote_text.active = Some(Lease {
            source: source.downgrade(),
            resource: resource.clone(),
            epoch,
            authority,
            target,
            keyboard,
            created: Instant::now(),
        });
        if state
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(50)),
                move |_, _, state| {
                    if watchdog(state, epoch) {
                        TimeoutAction::ToDuration(Duration::from_millis(50))
                    } else {
                        TimeoutAction::Drop
                    }
                },
            )
            .is_err()
        {
            revoke(state, DENIED);
            return;
        }
        resource.granted(epoch, epoch, TTL.as_millis() as u32);
        state.request_client_flush();
    }
}
impl Dispatch<JwmRemoteTextLeaseV1, LeaseData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &JwmRemoteTextLeaseV1,
        request: jwm_remote_text_lease_v1::Request,
        data: &LeaseData,
        _dh: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        let Some(lease) = state.remote_text.active.take() else {
            return;
        };
        if lease.resource != *resource || lease.epoch != data.epoch {
            state.remote_text.active = Some(lease);
            return;
        }
        if matches!(request, jwm_remote_text_lease_v1::Request::Destroy) {
            return;
        }
        // Taking the lease consumes the sole attempt before any validation or delivery.
        let result = (|| {
            if !current(state, &lease) {
                return Err(CONTEXT);
            }
            let jwm_remote_text_lease_v1::Request::Commit { epoch, frame, text } = request else {
                return Err(INVALID);
            };
            let text = valid_text(&text).ok_or(INVALID)?;
            if epoch != lease.epoch {
                return Err(INVALID);
            }
            let ready_at = frame.data::<CaptureFrameData>().and_then(|d| {
                let valid = frame.is_alive()
                    && frame.client().map(|c| c.id()) == resource.client().map(|c| c.id())
                    && d.ready.load(Ordering::Acquire)
                    && matches!(d.source.as_ref(), Some(CaptureSource::Toplevel(a)) if Arc::ptr_eq(a, &lease.authority))
                    && d.session.upgrade().is_ok_and(|s| s.is_alive() && s.data::<CaptureSessionData>().is_some_and(|d| !d.stopped.load(Ordering::Acquire)));
                valid.then(|| *d.ready_stamp.lock().unwrap()).flatten().map(|(_, at)| at)
            }).ok_or(SOURCE)?;
            if Instant::now()
                .checked_duration_since(ready_at)
                .is_none_or(|age| age > FRAME_AGE)
            {
                return Err(SOURCE);
            }
            if !state
                .seat
                .text_input()
                .commit_authorized_text(&lease.target, text)
            {
                return Err(CONTEXT);
            }
            Ok(())
        })();
        match result {
            Ok(()) => resource.accepted(lease.epoch),
            Err(reason) => resource.revoked(reason),
        }
        state.request_client_flush();
    }
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemoteTextLeaseV1,
        _data: &LeaseData,
    ) {
        if state
            .remote_text
            .active
            .as_ref()
            .is_some_and(|l| l.resource == *resource)
        {
            revoke(state, DENIED);
        }
    }
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
#[path = "remote_text_tests.rs"]
pub(crate) mod tests;
#[cfg(test)]
mod bounds_tests {
    use super::*;
    #[test]
    fn text_bytes_are_bounded_exact_utf8_without_nul() {
        assert_eq!(valid_text("  中文\n".as_bytes()), Some("  中文\n"));
        assert!(valid_text(&vec![b'a'; 4000]).is_some());
        for bytes in [&b""[..], &b"a\0b"[..], &b"\xff"[..], &vec![b'a'; 4001][..]] {
            assert!(valid_text(bytes).is_none());
        }
    }
}
