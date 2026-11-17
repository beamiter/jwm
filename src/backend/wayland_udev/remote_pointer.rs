//! Trusted-local, exact native surface pointer lease. Never a virtual pointer.
use super::image_copy_capture::{
    CaptureAuthority, CaptureFrameData, CaptureSessionData, CaptureSource, ImageCaptureSourceData,
    authority_is_current,
};
use crate::backend::wayland::state::JwmWaylandState;
use crate::remote_pointer_protocol::server::{
    jwm_remote_pointer_lease_v1::{self, JwmRemotePointerLeaseV1},
    jwm_remote_pointer_manager_v1::{self, JwmRemotePointerManagerV1},
};
use smithay::backend::{input::InputTime, renderer::utils::with_renderer_surface_state};
use smithay::input::{keyboard::AuthorizedKeyboardSnapshot, pointer::AuthorizedPointerSnapshot};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::{wl_output::Transform, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::SERIAL_COUNTER;
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use smithay::reexports::wayland_server::Weak;
use smithay::wayland::{
    compositor::{RectangleKind, SurfaceAttributes, get_children, with_states},
    shell::xdg::SurfaceCachedState,
    viewporter::ViewportCachedState,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
const TTL: Duration = Duration::from_secs(30);
const FRAME_AGE: Duration = Duration::from_secs(1);
pub(crate) const DENIED: u32 = 1;
pub(crate) const SOURCE: u32 = 2;
pub(crate) const SEAT: u32 = 3;
const EXPIRED: u32 = 5;
const INVALID: u32 = 6;
#[derive(Default)]
pub(crate) struct RemotePointerState {
    next_epoch: u32,
    active: Option<Lease>,
}
struct Lease {
    source: Weak<ExtImageCaptureSourceV1>,
    resource: JwmRemotePointerLeaseV1,
    epoch: u32,
    scroll_axes: u32,
    authority: Arc<CaptureAuthority>,
    pointer: AuthorizedPointerSnapshot,
    keyboard: AuthorizedKeyboardSnapshot,
    mapping: Mapping,
    sequence: u32,
    last_ready: u64,
    last_action: Option<Instant>,
    created: Instant,
    tokens: f64,
    refill: Instant,
}
pub struct ManagerData;
pub struct LeaseData {
    epoch: u32,
}
#[derive(Clone, PartialEq)]
pub(super) struct Mapping {
    geometry: Option<(i32, i32, i32, i32)>,
    region: Option<Vec<(bool, i32, i32, i32, i32)>>,
}
// Only a 1:1 uncropped root buffer is supported. Reject rather than guess a
// coordinate transform or route a root pixel into a subsurface or decoration.
struct NonzeroPointerOffset;
pub(super) fn mapping(authority: &CaptureAuthority) -> Option<Mapping> {
    let surface = &authority.surface;
    if !get_children(surface).is_empty() {
        return None;
    }
    let size = (authority.width as i32, authority.height as i32);
    let valid_buffer = with_renderer_surface_state(surface, |state| {
        state.buffer_size().is_some_and(|s| (s.w, s.h) == size)
            && state.view().is_some_and(|v| {
                v.offset.x == 0
                    && v.offset.y == 0
                    && v.src.loc.x == 0.0
                    && v.src.loc.y == 0.0
                    && (v.src.size.w, v.src.size.h) == (f64::from(size.0), f64::from(size.1))
                    && (v.dst.w, v.dst.h) == size
            })
    })
    .unwrap_or(false);
    if !valid_buffer {
        return None;
    }
    with_states(surface, |states| {
        if states.data_map.get::<NonzeroPointerOffset>().is_some() {
            return None;
        }
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        let a = attrs.current();
        if a.buffer_scale != 1
            || a.buffer_transform != Transform::Normal
            || a.buffer_delta.is_some_and(|d| d.x != 0 || d.y != 0)
        {
            return None;
        }
        let region = match &a.input_region {
            Some(r) if r.rects.len() > 256 => return None,
            Some(r) => Some(
                r.rects
                    .iter()
                    .map(|(k, r)| {
                        (
                            matches!(k, RectangleKind::Add),
                            r.loc.x,
                            r.loc.y,
                            r.size.w,
                            r.size.h,
                        )
                    })
                    .collect(),
            ),
            None => None,
        };
        let mut viewport = states.cached_state.get::<ViewportCachedState>();
        let v = viewport.current();
        if v.src.is_some() || v.dst.is_some() {
            return None;
        }
        let mut xdg = states.cached_state.get::<SurfaceCachedState>();
        let geometry = xdg
            .current()
            .geometry
            .map(|g| (g.loc.x, g.loc.y, g.size.w, g.size.h));
        if geometry.is_some_and(|g| g != (0, 0, size.0, size.1)) {
            return None;
        }
        Some(Mapping { geometry, region })
    })
}
fn action_allowed(kind: u32, axes: u32) -> bool {
    axes <= 3
        && (matches!(kind, 0 | 1)
            || (axes & 1 != 0 && matches!(kind, 2 | 3))
            || (axes & 2 != 0 && matches!(kind, 4 | 5)))
}
pub(super) fn point_allowed(mapping: &Mapping, x: u32, y: u32) -> bool {
    let Some(region) = &mapping.region else {
        return true;
    };
    let mut inside = false;
    for &(add, rx, ry, rw, rh) in region {
        if i64::from(x) >= i64::from(rx)
            && i64::from(y) >= i64::from(ry)
            && i64::from(x) < i64::from(rx) + i64::from(rw)
            && i64::from(y) < i64::from(ry) + i64::from(rh)
        {
            inside = add;
        }
    }
    inside
}
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, JwmRemotePointerManagerV1, _>(3, ManagerData);
}
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state.remote_pointer.active.is_some()
}
/// Read-only observation of the native lease clock, not the host repair/proof
/// deadlines. The test must retain this snapshot before the lease is removed.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn lease_timing_for_test(state: &JwmWaylandState) -> Option<(u32, Instant, Instant)> {
    state
        .remote_pointer
        .active
        .as_ref()
        .map(|lease| (lease.epoch, lease.created, lease.created + TTL))
}
pub(crate) fn revoke(state: &mut JwmWaylandState, reason: u32) {
    super::remote_text::revoke(state, reason);
    super::remote_control::revoke(state, reason);
    if let Some(lease) = state.remote_pointer.active.take() {
        lease.resource.revoked(reason);
        state.request_client_flush();
    }
}
pub(crate) fn revoke_for_window(
    state: &mut JwmWaylandState,
    window: crate::backend::common_define::WindowId,
) {
    super::remote_text::revoke_for_window(state, window);
    super::remote_control::revoke_for_window(state, window);
    if state
        .remote_pointer
        .active
        .as_ref()
        .is_some_and(|l| l.authority.window == window)
    {
        revoke(state, SOURCE);
    }
}
pub(crate) fn surface_committed(state: &mut JwmWaylandState, surface: &WlSurface) {
    // wl_surface offset is a delta, consumed by rendering. Do not lose a prior
    // nonzero origin just because a later commit omits/restores the offset.
    // The marker lives exactly as long as the surface, with no global registry.
    with_states(surface, |states| {
        if states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer_delta
            .is_some_and(|d| d.x != 0 || d.y != 0)
        {
            states.data_map.insert_if_missing(|| NonzeroPointerOffset);
        }
    });
    super::remote_control::surface_committed(state, surface);
    if state.remote_pointer.active.as_ref().is_some_and(|l| {
        l.authority.surface == *surface && mapping(&l.authority).as_ref() != Some(&l.mapping)
    }) {
        revoke(state, SOURCE);
    }
}
pub(crate) fn new_subsurface(state: &mut JwmWaylandState, parent: &WlSurface) {
    super::remote_control::new_subsurface(state, parent);
    if state
        .remote_pointer
        .active
        .as_ref()
        .is_some_and(|l| l.authority.surface == *parent)
    {
        revoke(state, SOURCE);
    }
}
pub(super) fn unconstrained(state: &JwmWaylandState, surface: &WlSurface) -> bool {
    state.seat.get_pointer().is_some_and(|pointer| {
        smithay::wayland::pointer_constraints::with_pointer_constraint(
            surface,
            &pointer,
            |constraint| constraint.is_none(),
        )
    })
}
fn current(state: &JwmWaylandState, l: &Lease) -> bool {
    l.resource.is_alive()
        && l.source.upgrade().is_ok_and(|source| source.is_alive())
        && l.created.elapsed() < TTL
        && authority_is_current(state, &l.authority)
        && unconstrained(state, &l.authority.surface)
        && state.remote_keyboard_seat_eligible()
        && state.remote_keyboard_target_eligible(&l.authority)
        && mapping(&l.authority).as_ref() == Some(&l.mapping)
        && state
            .seat
            .get_keyboard()
            .is_some_and(|k| k.authorized_keyboard_snapshot_is_current(&l.keyboard))
        && state
            .seat
            .get_pointer()
            .is_some_and(|p| p.authorized_pointer_snapshot_is_current(&l.pointer))
}
fn watchdog(state: &mut JwmWaylandState, epoch: u32) -> bool {
    let Some(l) = state.remote_pointer.active.as_ref() else {
        return false;
    };
    if l.epoch != epoch {
        return false;
    }
    if !current(state, l) {
        revoke(state, EXPIRED);
        return false;
    }
    true
}
impl GlobalDispatch<JwmRemotePointerManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<JwmRemotePointerManagerV1>,
        _data: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ManagerData);
    }
    fn can_view(client: Client, _data: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemotePointerManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &JwmRemotePointerManagerV1,
        request: jwm_remote_pointer_manager_v1::Request,
        _data: &ManagerData,
        _dh: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        // A v2 bind is not permission. The original request remains strictly
        // motion/click-only, even when issued on a version-two manager.
        let (id, source, scroll_axes, axes_valid) = match request {
            jwm_remote_pointer_manager_v1::Request::CreateLease { id, source } => {
                (id, source, 0, true)
            }
            jwm_remote_pointer_manager_v1::Request::CreateLeaseWithScroll { id, source } => {
                (id, source, 1, true)
            }
            jwm_remote_pointer_manager_v1::Request::CreateLeaseWithScrollAxes {
                id,
                source,
                axes,
            } => (id, source, axes, matches!(axes, 1..=3)),
            _ => return,
        };
        {
            let epoch = state.remote_pointer.next_epoch.checked_add(1).unwrap_or(0);
            let resource = init.init(id, LeaseData { epoch });
            if epoch == 0 {
                resource.revoked(DENIED);
                return;
            }
            state.remote_pointer.next_epoch = epoch;
            let Some(CaptureSource::Toplevel(authority)) = source
                .data::<ImageCaptureSourceData>()
                .and_then(|d| d.source.as_ref())
            else {
                resource.revoked(DENIED);
                return;
            };
            let authority = authority.clone();
            if source.client().map(|c| c.id()) != Some(client.id())
                || !authority.consume_pointer_attempt()
                || !axes_valid
                || active(state)
                || super::remote_keyboard::active(state)
                || super::remote_text::active(state)
                || super::remote_popup::active(state)
                || super::remote_control::active(state)
                || !authority_is_current(state, &authority)
                || !unconstrained(state, &authority.surface)
                || !state.remote_keyboard_seat_eligible()
                || !state.remote_keyboard_target_eligible(&authority)
            {
                resource.revoked(DENIED);
                return;
            }
            let Some(mapping) = mapping(&authority) else {
                resource.revoked(SOURCE);
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
            let Some(keyboard) = state
                .seat
                .get_keyboard()
                .and_then(|k| k.snapshot_authorized_keyboard(&authority.surface))
            else {
                resource.revoked(SEAT);
                return;
            };
            let now = Instant::now();
            state.remote_pointer.active = Some(Lease {
                source: source.downgrade(),
                resource: resource.clone(),
                epoch,
                scroll_axes,
                authority,
                pointer,
                keyboard,
                mapping,
                sequence: 0,
                last_ready: 0,
                last_action: None,
                created: now,
                tokens: 8.0,
                refill: now,
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
            resource.granted(epoch, TTL.as_millis() as u32);
            state.request_client_flush();
        }
    }
}
impl Dispatch<JwmRemotePointerLeaseV1, LeaseData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &JwmRemotePointerLeaseV1,
        request: jwm_remote_pointer_lease_v1::Request,
        data: &LeaseData,
        _dh: &DisplayHandle,
        _init: &mut DataInit<'_, Self>,
    ) {
        let Some(mut lease) = state.remote_pointer.active.take() else {
            return;
        };
        if lease.resource != *resource || lease.epoch != data.epoch {
            state.remote_pointer.active = Some(lease);
            return;
        }
        if matches!(request, jwm_remote_pointer_lease_v1::Request::Destroy) {
            return;
        }
        let reject = |state: &mut Self, reason| {
            resource.revoked(reason);
            state.request_client_flush();
        };
        if !current(state, &lease) {
            reject(state, SEAT);
            return;
        }
        if let jwm_remote_pointer_lease_v1::Request::Action {
            epoch,
            sequence,
            frame,
            kind,
            x,
            y,
        } = request
        {
            let now = Instant::now();
            if epoch != lease.epoch
                || sequence != lease.sequence.saturating_add(1)
                || sequence > 60000
                || !action_allowed(kind, lease.scroll_axes)
                || x >= lease.authority.width
                || y >= lease.authority.height
                || !point_allowed(&lease.mapping, x, y)
            {
                reject(state, INVALID);
                return;
            }
            let stamp=frame.data::<CaptureFrameData>().and_then(|d| {
                let valid=frame.is_alive()&&frame.client().map(|c|c.id())==resource.client().map(|c|c.id())&&d.ready.load(Ordering::Acquire)&&matches!(d.source.as_ref(),Some(CaptureSource::Toplevel(a)) if Arc::ptr_eq(a,&lease.authority))&&d.session.upgrade().is_ok_and(|s|s.is_alive()&&s.data::<CaptureSessionData>().is_some_and(|d|!d.stopped.load(Ordering::Acquire)));
                valid.then(||*d.ready_stamp.lock().unwrap()).flatten()
            });
            let Some((serial, ready_at)) = stamp else {
                reject(state, SOURCE);
                return;
            };
            if serial <= lease.last_ready
                || now
                    .checked_duration_since(ready_at)
                    .is_none_or(|age| age > FRAME_AGE)
                || lease.last_action.is_some_and(|at| ready_at <= at)
            {
                reject(state, SOURCE);
                return;
            }
            lease.tokens =
                (lease.tokens + now.duration_since(lease.refill).as_secs_f64() * 30.0).min(8.0);
            lease.refill = now;
            if lease.tokens < 1.0 {
                reject(state, INVALID);
                return;
            }
            lease.tokens -= 1.0;
            // No external dispatch can interleave this validation and the
            // vendor's exact-receiver atomic event group. Serial is never
            // installed as a compositor implicit grab or popup key.
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
                reject(state, SEAT);
                return;
            }
            lease.sequence = sequence;
            lease.last_ready = serial;
            lease.last_action = Some(Instant::now());
            resource.accepted(epoch, sequence);
            state.remote_pointer.active = Some(lease);
            state.request_client_flush();
        }
    }
    fn destroyed(
        state: &mut Self,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemotePointerLeaseV1,
        _data: &LeaseData,
    ) {
        if state
            .remote_pointer
            .active
            .as_ref()
            .is_some_and(|l| l.resource == *resource)
        {
            revoke(state, DENIED);
        }
    }
}
#[cfg(test)]
#[path = "remote_pointer_tests.rs"]
mod tests;
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn expire_for_test(state: &mut JwmWaylandState) {
    if let Some(lease) = state.remote_pointer.active.as_mut() {
        lease.created = Instant::now() - TTL;
    }
}

pub(crate) fn source_destroyed(state: &mut JwmWaylandState, source: &ExtImageCaptureSourceV1) {
    if state
        .remote_pointer
        .active
        .as_ref()
        .is_some_and(|lease| lease.source.id() == source.id())
    {
        revoke(state, SOURCE);
    }
}

#[cfg(test)]
mod coordinate_tests {
    use super::*;
    #[test]
    fn scroll_axes_require_independent_explicit_grants() {
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
    fn region_membership_uses_half_open_bounds_and_ordered_subtraction() {
        let mut mapping = Mapping {
            geometry: None,
            region: Some(vec![(true, 0, 0, 10, 10), (false, 2, 2, 3, 3)]),
        };
        assert!(point_allowed(&mapping, 0, 0));
        assert!(point_allowed(&mapping, 9, 9));
        assert!(!point_allowed(&mapping, 10, 0));
        assert!(!point_allowed(&mapping, 2, 2));
        assert!(point_allowed(&mapping, 5, 2));
        mapping.region.as_mut().unwrap().push((true, 3, 3, 1, 1));
        assert!(point_allowed(&mapping, 3, 3));
        assert!(!point_allowed(&mapping, u32::MAX, u32::MAX));
        mapping.region = Some(vec![(true, i32::MAX, i32::MAX, i32::MAX, i32::MAX)]);
        assert!(!point_allowed(&mapping, u32::MAX, u32::MAX));
    }
}
