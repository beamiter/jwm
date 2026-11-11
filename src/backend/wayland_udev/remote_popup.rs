//! Explicit root-family pointer authority on an isolated, application-only seat.
use super::{
    capture_popup_authority::{FamilyAuthority, FamilyProof},
    image_copy_capture::{
        CaptureFrameData, CaptureSessionData, CaptureSource, ImageCaptureSourceData,
        create_interactive_family_source, export_interactive_family_frame,
    },
};
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote_popup_protocol::server::{
    jwm_remote_popup_lease_v1::{self, JwmRemotePopupLeaseV1},
    jwm_remote_popup_manager_v1::{self, JwmRemotePopupManagerV1},
};
use smithay::reexports::{
    calloop::timer::{TimeoutAction, Timer},
    wayland_protocols::ext::{
        image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
    },
    wayland_server::{
        Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
        backend::ClientId,
        protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
    },
};
use smithay::{
    backend::input::InputTime,
    input::{Seat, pointer::PrivatePointerSnapshot},
    utils::{SERIAL_COUNTER, Serial},
    wayland::shell::xdg::PopupSurface,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
const TTL: Duration = Duration::from_secs(30);
const FRESH: Duration = Duration::from_secs(1);
pub(crate) const DENIED: u32 = 1;
pub(crate) const SOURCE: u32 = 2;
pub(crate) const SEAT: u32 = 3;
const EXPIRED: u32 = 5;
#[derive(Default)]
pub(crate) struct RemotePopupState {
    next_epoch: u32,
    active: Option<Lease>,
    prepared: Option<PreparedSeat>,
    prepared_owners: Vec<ClientId>,
}
const PREPARE_TTL: Duration = Duration::from_secs(10);
struct PreparedSeat {
    owner: ClientId,
    seat: Seat<JwmWaylandState>,
    created: Instant,
}
/// Trusted controlled-launch experiment only. Call after exact client insertion
/// and before dispatching its first registry request. This does not grant input.
#[cfg(test)]
pub(crate) fn prepare_dormant(
    state: &mut JwmWaylandState,
    owner: ClientId,
) -> Option<Seat<JwmWaylandState>> {
    if state.remote_popup.active.is_some()
        || state.remote_popup.prepared.is_some()
        || state.remote_popup.prepared_owners.len() >= 64
        || state.remote_popup.prepared_owners.contains(&owner)
        || state
            .display_handle
            .backend_handle()
            .get_client_data(owner.clone())
            .is_err()
    {
        return None;
    }
    // Record before allocation: failure/expiry/consumption never permits revival.
    state.remote_popup.prepared_owners.push(owner.clone());
    let mut seat = state.seat_state.new_client_wl_seat(
        &state.display_handle,
        "jwm-remote-popup",
        owner.clone(),
    );
    seat.add_pointer();
    state.remote_popup.prepared = Some(PreparedSeat {
        owner: owner.clone(),
        seat: seat.clone(),
        created: Instant::now(),
    });
    if state
        .loop_handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(50)),
            move |_, _, state| {
                let Some(prepared) = state.remote_popup.prepared.as_ref() else {
                    return TimeoutAction::Drop;
                };
                if prepared.owner != owner {
                    return TimeoutAction::Drop;
                }
                if prepared.created.elapsed() >= PREPARE_TTL
                    || state
                        .display_handle
                        .backend_handle()
                        .get_client_data(owner.clone())
                        .is_err()
                {
                    let prepared = state.remote_popup.prepared.take().unwrap();
                    state
                        .seat_state
                        .retire_wl_seat(&state.display_handle, &prepared.seat);
                    state.request_client_flush();
                    TimeoutAction::Drop
                } else {
                    TimeoutAction::ToDuration(Duration::from_millis(50))
                }
            },
        )
        .is_err()
    {
        let prepared = state.remote_popup.prepared.take().unwrap();
        state
            .seat_state
            .retire_wl_seat(&state.display_handle, &prepared.seat);
        return None;
    }
    Some(seat)
}
struct Lease {
    resource: JwmRemotePopupLeaseV1,
    epoch: u32,
    family: Arc<FamilyAuthority>,
    original: Weak<ExtImageCaptureSourceV1>,
    source: Option<Weak<ExtImageCaptureSourceV1>>,
    unexported: Option<CaptureSource>,
    seat: Seat<JwmWaylandState>,
    receivers: Option<PrivatePointerSnapshot>,
    sequence: u32,
    last_ready: u64,
    exports: ExportLedger,
    last_action: Option<Instant>,
    popup: Option<WlSurface>,
    grabbed: bool,
    popup_mapped: bool,
    click: Option<(Serial, Instant)>,
    tokens: f64,
    refill: Instant,
}
#[derive(Default)]
struct ExportLedger {
    count: u32,
    last_serial: u64,
}
impl ExportLedger {
    fn accept(&mut self, serial: u64) -> bool {
        if self.count >= 1024 || serial <= self.last_serial {
            return false;
        }
        let Some(next) = self.count.checked_add(1) else {
            return false;
        };
        self.count = next;
        self.last_serial = serial;
        true
    }
}
fn words(value: u64) -> (u32, u32) {
    ((value >> 32) as u32, value as u32)
}
fn next_epoch(previous: &mut u32) -> Option<u32> {
    let next = previous.checked_add(1)?;
    *previous = next;
    Some(next)
}
fn click_serial_valid(click: Option<(Serial, Instant)>, serial: Serial, now: Instant) -> bool {
    click.is_some_and(|(expected, at)| {
        expected == serial
            && now
                .checked_duration_since(at)
                .is_some_and(|age| age <= FRESH)
    })
}
pub struct ManagerData;
pub struct LeaseData {
    epoch: u32,
}
fn clean_local(state: &JwmWaylandState) -> bool {
    state.remote_keyboard_seat_eligible()
        && state
            .seat
            .get_pointer()
            .is_some_and(|p| !p.is_grabbed() && !p.has_pressed_buttons())
        && state.seat.get_keyboard().is_some_and(|k| {
            let m = k.modifier_state().serialized;
            !k.is_grabbed()
                && k.pressed_keys().is_empty()
                && m.depressed == 0
                && m.latched == 0
                && m.locked == 0
                && m.layout_effective == 0
        })
}
fn current(state: &JwmWaylandState, l: &Lease) -> bool {
    l.resource.is_alive()
        && l.family.current(state)
        && clean_local(state)
        && l.original.upgrade().is_ok_and(|s| s.is_alive())
        && l.source
            .as_ref()
            .is_none_or(|s| s.upgrade().is_ok_and(|s| s.is_alive()))
        && l.seat.get_pointer().is_some_and(|p| {
            l.receivers
                .as_ref()
                .is_none_or(|r| p.private_pointer_is_current(r))
        })
}
fn finish(state: &mut JwmWaylandState, mut l: Lease, reason: u32) {
    #[cfg(test)]
    eprintln!(
        "JWM_POPUP_GATE=finish reason={} armed={} child={} grabbed={} mapped={}",
        reason,
        l.receivers.is_some(),
        l.popup.is_some(),
        l.grabbed,
        l.popup_mapped
    );
    l.family.revoke();
    // End only this lease's exact child; never touch the compositor's local
    // popup-grab state or another application's popup resources.
    if let Some(surface) = &l.popup {
        if surface.is_alive() {
            if let Some(popup) = state
                .popups
                .get(&surface.id())
                .filter(|p| p.wl_surface() == surface)
            {
                popup.send_popup_done();
            }
        }
    }
    if let (Some(pointer), Some(receivers)) = (l.seat.get_pointer(), l.receivers.as_mut()) {
        pointer.cleanup_private_pointer(receivers, SERIAL_COUNTER.next_serial());
    }
    state
        .seat_state
        .retire_wl_seat(&state.display_handle, &l.seat);
    if l.resource.is_alive() {
        l.resource.revoked(reason);
    }
    state.request_client_flush();
}
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state.remote_popup.active.is_some()
}
pub(crate) fn revoke(state: &mut JwmWaylandState, reason: u32) {
    if let Some(l) = state.remote_popup.active.take() {
        finish(state, l, reason);
    }
}
pub(crate) fn revoke_for_window(
    state: &mut JwmWaylandState,
    window: crate::backend::common_define::WindowId,
) {
    if state
        .remote_popup
        .active
        .as_ref()
        .is_some_and(|l| l.family.root.window == window)
    {
        revoke(state, SOURCE);
    }
}
pub(crate) fn source_destroyed(state: &mut JwmWaylandState, source: &ExtImageCaptureSourceV1) {
    if state.remote_popup.active.as_ref().is_some_and(|l| {
        l.original.id() == source.id() || l.source.as_ref().is_some_and(|s| s.id() == source.id())
    }) {
        revoke(state, SOURCE);
    }
}
pub(crate) fn surface_committed(state: &mut JwmWaylandState, surface: &WlSurface) {
    let Some(l) = state.remote_popup.active.as_mut() else {
        return;
    };
    if l.family.root.surface == *surface {
        l.family.invalidate();
    }
    if l.popup.as_ref() == Some(surface) {
        let mapped = smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| {
            s.view().is_some()
        })
        .unwrap_or(false);
        if (mapped && !l.grabbed) || (!mapped && l.popup_mapped) {
            #[cfg(test)]
            eprintln!(
                "JWM_POPUP_GATE=surface_lifecycle mapped={} grabbed={} previously_mapped={}",
                mapped, l.grabbed, l.popup_mapped
            );
            revoke(state, SOURCE);
            return;
        }
        l.popup_mapped |= mapped;
        l.family.invalidate();
    }
}
pub(crate) fn new_subsurface(state: &mut JwmWaylandState, parent: &WlSurface) {
    if state
        .remote_popup
        .active
        .as_ref()
        .is_some_and(|l| l.family.root.surface == *parent || l.popup.as_ref() == Some(parent))
    {
        revoke(state, SOURCE);
    }
}
pub(crate) fn popup_destroyed(state: &mut JwmWaylandState, surface: &WlSurface) {
    if state
        .remote_popup
        .active
        .as_ref()
        .is_some_and(|l| l.popup.as_ref() == Some(surface))
    {
        revoke(state, SOURCE);
    }
}
/// Only the exact, pending root click may create this one direct child.
pub(crate) fn popup_created(state: &mut JwmWaylandState, popup: &PopupSurface) -> bool {
    let Some(l) = state.remote_popup.active.as_ref() else {
        return false;
    };
    let allowed = current(state, l)
        && l.popup.is_none()
        && l.receivers.is_some()
        && popup.get_parent_surface().as_ref() == Some(&l.family.root.surface)
        && popup.wl_surface().client().is_some_and(|c| {
            l.family
                .root
                .surface
                .client()
                .is_some_and(|root| root.id() == c.id())
        })
        && l.click.is_some_and(|(_, at)| at.elapsed() <= FRESH);
    #[cfg(test)]
    eprintln!(
        "JWM_POPUP_GATE=create allowed={} current={} first_child={} armed={} direct_parent={} same_owner={} fresh_click={}",
        allowed,
        current(state, l),
        l.popup.is_none(),
        l.receivers.is_some(),
        popup.get_parent_surface().as_ref() == Some(&l.family.root.surface),
        popup.wl_surface().client().is_some_and(|c| l
            .family
            .root
            .surface
            .client()
            .is_some_and(|r| c.id() == r.id())),
        l.click.is_some_and(|(_, at)| at.elapsed() <= FRESH)
    );
    if !allowed {
        revoke(state, SOURCE);
        return false;
    }
    let l = state.remote_popup.active.as_mut().unwrap();
    l.popup = Some(popup.wl_surface().clone());
    l.family.invalidate();
    true
}
/// Handle only our exact private seat. No ordinary grab/focus/selection state is modified.
pub(crate) fn popup_grab(
    state: &mut JwmWaylandState,
    popup: &PopupSurface,
    seat: &WlSeat,
    serial: Serial,
) -> bool {
    let Some(l) = state.remote_popup.active.as_ref() else {
        return false;
    };
    if Seat::<JwmWaylandState>::from_resource(seat).as_ref() != Some(&l.seat) {
        #[cfg(test)]
        eprintln!(
            "JWM_POPUP_GATE=grab_wrong_seat exact_child={}",
            l.popup.as_ref() == Some(popup.wl_surface())
        );
        if l.popup.as_ref() == Some(popup.wl_surface()) {
            revoke(state, DENIED);
            return true;
        }
        return false;
    }
    let valid = current(state, l)
        && l.seat.owns(seat)
        && seat.id().same_client_as(&popup.wl_surface().id())
        && l.popup.as_ref() == Some(popup.wl_surface())
        && !l.grabbed
        && popup.get_parent_surface().as_ref() == Some(&l.family.root.surface)
        && click_serial_valid(l.click, serial, Instant::now());
    #[cfg(test)]
    eprintln!(
        "JWM_POPUP_GATE=grab allowed={} current={} owns_seat={} same_owner={} exact_child={} unused={} direct_parent={} exact_serial={} fresh_click={}",
        valid,
        current(state, l),
        l.seat.owns(seat),
        seat.id().same_client_as(&popup.wl_surface().id()),
        l.popup.as_ref() == Some(popup.wl_surface()),
        !l.grabbed,
        popup.get_parent_surface().as_ref() == Some(&l.family.root.surface),
        l.click.is_some_and(|(expected, _)| expected == serial),
        l.click.is_some_and(|(_, at)| at.elapsed() <= FRESH)
    );
    if valid {
        let l = state.remote_popup.active.as_mut().unwrap();
        l.click = None;
        l.grabbed = true;
        l.family.invalidate();
    } else {
        revoke(state, DENIED);
    }
    true
}
fn proof(
    state: &JwmWaylandState,
    l: &Lease,
    frame: &ExtImageCopyCaptureFrameV1,
) -> Option<(u64, Instant, Arc<FamilyProof>)> {
    if !frame.is_alive() || frame.client()?.id() != l.resource.client()?.id() {
        return None;
    }
    let d = frame.data::<CaptureFrameData>()?;
    if !d.ready.load(Ordering::Acquire)
        || !d.session.upgrade().is_ok_and(|s| {
            s.is_alive()
                && s.data::<CaptureSessionData>()
                    .is_some_and(|d| !d.stopped.load(Ordering::Acquire))
        })
    {
        return None;
    }
    let CaptureSource::Toplevel(a) = d.source.as_ref()? else {
        return None;
    };
    if a.family.as_ref().is_none_or(|f| !Arc::ptr_eq(f, &l.family)) {
        return None;
    }
    let proof = d.family_proof.lock().unwrap().clone()?;
    if !Arc::ptr_eq(&proof.authority, &l.family) || !proof.current(state) {
        return None;
    }
    match (l.popup.as_ref(), proof.layout.popup.as_ref()) {
        (None, None) => {}
        (Some(expected), Some(popup))
            if l.grabbed && l.popup_mapped && expected == &popup.surface => {}
        _ => return None,
    }
    let (serial, at) = (*d.ready_stamp.lock().unwrap())?;
    if serial <= l.last_ready
        || at.elapsed() > FRESH
        || l.last_action.is_some_and(|last| at <= last)
    {
        return None;
    }
    Some((serial, at, proof))
}
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, JwmRemotePopupManagerV1, _>(2, ManagerData);
}
impl GlobalDispatch<JwmRemotePopupManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        id: New<JwmRemotePopupManagerV1>,
        _: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(id, ManagerData);
    }
    fn can_view(client: Client, _: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemotePopupManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &JwmRemotePopupManagerV1,
        request: jwm_remote_popup_manager_v1::Request,
        _: &ManagerData,
        dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let jwm_remote_popup_manager_v1::Request::CreateLease { id, source } = request else {
            return;
        };
        let epoch = next_epoch(&mut state.remote_popup.next_epoch).unwrap_or(0);
        let resource = init.init(id, LeaseData { epoch });
        let family = (|| {
            if epoch == 0
                || state.remote_popup.active.is_some()
                || super::remote_keyboard::active(state)
                || super::remote_pointer::active(state)
                || super::remote_control::active(state)
                || super::remote_text::active(state)
                || !clean_local(state)
                || source.client()?.id() != client.id()
            {
                return None;
            }
            let CaptureSource::Toplevel(root) =
                source.data::<ImageCaptureSourceData>()?.source.as_ref()?
            else {
                return None;
            };
            if !root.owned_by(&client.id())
                || !root.is_original_root()
                || !root.consume_control_attempt()
            {
                return None;
            }
            FamilyAuthority::new(state, root)
        })();
        let derived = family
            .as_ref()
            .and_then(|f| create_interactive_family_source(state, &f.root, f.clone()));
        let Some(family) = family.filter(|_| derived.is_some()) else {
            resource.revoked(DENIED);
            return;
        };
        let Some(owner) = family.root.surface.client() else {
            resource.revoked(SOURCE);
            return;
        };
        let seat = if state.remote_popup.prepared_owners.contains(&owner.id()) {
            let valid = state
                .remote_popup
                .prepared
                .as_ref()
                .is_some_and(|p| p.owner == owner.id() && p.created.elapsed() < PREPARE_TTL);
            if !valid {
                resource.revoked(DENIED);
                return;
            }
            state.remote_popup.prepared.take().unwrap().seat
        } else {
            // Never consume or retire a different application's preparation.
            if state.remote_popup.prepared.is_some() {
                resource.revoked(DENIED);
                return;
            }
            let mut seat = state
                .seat_state
                .new_client_wl_seat(dh, "jwm-remote-popup", owner.id());
            seat.add_pointer();
            seat
        };
        state.remote_popup.active = Some(Lease {
            resource: resource.clone(),
            epoch,
            family,
            original: source.downgrade(),
            source: None,
            unexported: derived,
            seat,
            receivers: None,
            sequence: 0,
            last_ready: 0,
            exports: ExportLedger::default(),
            last_action: None,
            popup: None,
            grabbed: false,
            popup_mapped: false,
            click: None,
            tokens: 8.0,
            refill: Instant::now(),
        });
        if state
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(50)),
                move |_, _, state| {
                    let Some(l) = state.remote_popup.active.as_ref() else {
                        return TimeoutAction::Drop;
                    };
                    if l.epoch != epoch {
                        return TimeoutAction::Drop;
                    }
                    if !current(state, l) {
                        let reason = if l.family.created.elapsed() >= TTL {
                            EXPIRED
                        } else {
                            SOURCE
                        };
                        revoke(state, reason);
                        TimeoutAction::Drop
                    } else {
                        TimeoutAction::ToDuration(Duration::from_millis(50))
                    }
                },
            )
            .is_err()
        {
            revoke(state, DENIED);
            return;
        }
        resource.granted(epoch, TTL.as_millis() as u32);
        state.request_client_flush();
    }
}
impl Dispatch<JwmRemotePopupLeaseV1, LeaseData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &JwmRemotePopupLeaseV1,
        request: jwm_remote_popup_lease_v1::Request,
        data: &LeaseData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let jwm_remote_popup_lease_v1::Request::GetCaptureSource { capture_source } = request {
            let Some(mut l) = state.remote_popup.active.take() else {
                init.init(capture_source, ImageCaptureSourceData { source: None });
                return;
            };
            if l.epoch != data.epoch || l.resource != *resource {
                init.init(capture_source, ImageCaptureSourceData { source: None });
                state.remote_popup.active = Some(l);
                return;
            }
            let valid = current(state, &l) && l.source.is_none() && l.unexported.is_some();
            let exported = init.init(
                capture_source,
                ImageCaptureSourceData {
                    source: if valid { l.unexported.take() } else { None },
                },
            );
            if valid {
                l.source = Some(exported.downgrade());
                state.remote_popup.active = Some(l);
            } else {
                finish(state, l, DENIED);
            }
            return;
        }
        let Some(mut l) = state.remote_popup.active.take() else {
            return;
        };
        if l.epoch != data.epoch || l.resource != *resource {
            state.remote_popup.active = Some(l);
            return;
        }
        if matches!(request, jwm_remote_popup_lease_v1::Request::Destroy) {
            finish(state, l, DENIED);
            return;
        }
        if !current(state, &l) {
            finish(state, l, SOURCE);
            return;
        }
        let accepted = match request {
            jwm_remote_popup_lease_v1::Request::ExportFrameProof {
                epoch,
                frame,
                cookie,
            } => {
                let exported = (|| {
                    if epoch != l.epoch {
                        return None;
                    }
                    let (serial, at, frozen) = proof(state, &l, &frame)?;
                    let source = l.source.as_ref()?.upgrade().ok()?;
                    let owner = resource.client()?.id();
                    let (checked_serial, checked_at, checked_proof) =
                        export_interactive_family_frame(state, &l.family, &source, &frame, &owner)?;
                    if checked_serial != serial
                        || checked_at != at
                        || !Arc::ptr_eq(&frozen, &checked_proof)
                    {
                        return None;
                    }
                    Some((serial, frozen.export_layout()?))
                })();
                if let Some((serial, layout)) =
                    exported.filter(|(serial, _)| l.exports.accept(*serial))
                {
                    let (ready_hi, ready_lo) = words(serial);
                    let (generation_hi, generation_lo) = words(layout.generation);
                    let present = u32::from(layout.popup.is_some());
                    let (x, y, width, height) = layout.popup.unwrap_or((0, 0, 0, 0));
                    resource.frame_proof(
                        epoch,
                        &frame,
                        cookie,
                        ready_hi,
                        ready_lo,
                        generation_hi,
                        generation_lo,
                        layout.width,
                        layout.height,
                        present,
                        x,
                        y,
                        width,
                        height,
                    );
                    true
                } else {
                    false
                }
            }
            jwm_remote_popup_lease_v1::Request::Arm { epoch, frame } => {
                if epoch != l.epoch || l.receivers.is_some() {
                    false
                } else if let Some((serial, _, proof)) = proof(state, &l, &frame) {
                    if proof.layout.popup.is_some() {
                        false
                    } else {
                        let p = l.seat.get_pointer().unwrap();
                        let owner = l.family.root.surface.client().unwrap().id();
                        if let Some(mut receivers) = p.snapshot_private_pointer(owner) {
                            if p.send_private_pointer(
                                &mut receivers,
                                &l.family.root.surface,
                                (0.0, 0.0),
                                false,
                                SERIAL_COUNTER.next_serial(),
                                InputTime::now(),
                            ) {
                                l.receivers = Some(receivers);
                                l.last_ready = serial;
                                l.last_action = Some(Instant::now());
                                resource.armed(epoch);
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    }
                } else {
                    false
                }
            }
            jwm_remote_popup_lease_v1::Request::Action {
                epoch,
                sequence,
                frame,
                kind,
                x,
                y,
            } => {
                if epoch != l.epoch
                    || sequence != l.sequence.saturating_add(1)
                    || sequence > 60000
                    || kind > 1
                    || l.receivers.is_none()
                {
                    false
                } else if let Some((ready, _, proof)) = proof(state, &l, &frame) {
                    if let Some((target, localx, localy)) =
                        proof.layout.target_at(x, y).filter(|(target, _, _)| {
                            l.popup.as_ref().is_none_or(|popup| popup == target)
                        })
                    {
                        let now = Instant::now();
                        l.tokens =
                            (l.tokens + now.duration_since(l.refill).as_secs_f64() * 30.0).min(8.0);
                        l.refill = now;
                        if l.tokens < 1.0 {
                            false
                        } else {
                            let serial = SERIAL_COUNTER.next_serial();
                            let p = l.seat.get_pointer().unwrap();
                            if p.send_private_pointer(
                                l.receivers.as_mut().unwrap(),
                                &target,
                                (localx, localy),
                                kind == 1,
                                serial,
                                InputTime::now(),
                            ) {
                                l.tokens -= 1.0;
                                l.sequence = sequence;
                                l.last_ready = ready;
                                l.last_action = Some(now);
                                l.click = if kind == 1
                                    && target == l.family.root.surface
                                    && l.popup.is_none()
                                {
                                    Some((serial, now))
                                } else {
                                    None
                                };
                                resource.accepted(epoch, sequence);
                                true
                            } else {
                                false
                            }
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
            _ => false,
        };
        if accepted {
            state.remote_popup.active = Some(l);
            state.request_client_flush();
        } else {
            finish(state, l, DENIED);
        }
    }
    fn destroyed(
        state: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemotePopupLeaseV1,
        _: &LeaseData,
    ) {
        if state
            .remote_popup
            .active
            .as_ref()
            .is_some_and(|l| l.resource == *resource)
        {
            revoke(state, DENIED);
        }
    }
}
#[cfg(test)]
#[path = "remote_popup_tests.rs"]
mod tests;

#[cfg(test)]
mod primitive_tests {
    use super::*;
    #[test]
    fn export_ledger_is_bounded_monotonic_and_does_not_truncate_serials() {
        let mut ledger = ExportLedger::default();
        assert!(!ledger.accept(0));
        let first = (1u64 << 40) + 3;
        assert!(ledger.accept(first));
        assert!(!ledger.accept(first));
        assert!(!ledger.accept(first - 1));
        assert_eq!(ledger.count, 1);
        for offset in 1..1024 {
            assert!(ledger.accept(first + offset));
        }
        assert!(!ledger.accept(first + 1024));
        assert_eq!(ledger.last_serial, first + 1023);
        let (hi, lo) = words(u64::MAX - 1);
        assert_eq!((u64::from(hi) << 32) | u64::from(lo), u64::MAX - 1);
    }

    #[test]
    fn private_click_serial_is_exact_bounded_and_not_future() {
        let now = Instant::now();
        let a = Serial::from(17u32);
        let b = Serial::from(18u32);
        assert!(click_serial_valid(Some((a, now)), a, now));
        assert!(!click_serial_valid(Some((a, now)), b, now));
        assert!(!click_serial_valid(None, a, now));
        assert!(!click_serial_valid(
            Some((a, now)),
            a,
            now + FRESH + Duration::from_nanos(1)
        ));
        assert!(!click_serial_valid(
            Some((a, now + Duration::from_nanos(1))),
            a,
            now
        ));
    }
    #[test]
    fn exhausted_private_epoch_never_restarts() {
        let mut epoch = u32::MAX - 1;
        assert_eq!(next_epoch(&mut epoch), Some(u32::MAX));
        assert_eq!(next_epoch(&mut epoch), None);
        assert_eq!(next_epoch(&mut epoch), None);
        assert_eq!(epoch, u32::MAX);
    }
}

/// Read-only original lease clock for real-time protocol tests.
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn lease_timing_for_test(state: &JwmWaylandState) -> Option<(u32, Instant, Instant)> {
    state
        .remote_popup
        .active
        .as_ref()
        .map(|l| (l.epoch, l.family.created, l.family.created + TTL))
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
#[path = "remote_popup_backend_tests.rs"]
mod backend_tests;

#[cfg(all(test, feature = "backend-wayland-udev"))]
#[path = "remote_popup_gtk_tests.rs"]
mod gtk_tests;
