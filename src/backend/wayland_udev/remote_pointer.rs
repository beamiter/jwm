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
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::server::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use smithay::reexports::wayland_server::Weak;
use smithay::wayland::{
    compositor::{
        RectangleKind, SubsurfaceCachedState, SurfaceAttributes, TraversalAction, get_children, with_states,
        with_surface_tree_downward,
    },
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
    root_size: (i32, i32),
    region: Option<Vec<(bool, i32, i32, i32, i32)>>,
    paint_children: Vec<ObjectId>,
}
impl Mapping {
    pub(super) fn is_paint_child(&self, surface: &WlSurface) -> bool {
        self.paint_children.contains(&surface.id())
    }
}
struct NonzeroPointerOffset;
pub(super) fn mapping(authority: &CaptureAuthority) -> Option<Mapping> {
    root_mapping(authority, false, false)
}
pub(super) fn gesture_mapping(authority: &CaptureAuthority) -> Option<Mapping> {
    root_mapping(authority, true, false)
}
pub(super) fn window_gesture_mapping(authority: &CaptureAuthority) -> Option<Mapping> {
    root_mapping(authority, true, true)
}
pub(super) fn has_empty_input_region(surface: &WlSurface) -> bool {
    surface.is_alive()
        && with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .input_region
                .as_ref()
                .is_some_and(|region| region.rects.is_empty())
        })
}
fn root_mapping(
    authority: &CaptureAuthority,
    allow_window_geometry: bool,
    dynamic_window_tree: bool,
) -> Option<Mapping> {
    let surface = &authority.surface;
    let mut children = if dynamic_window_tree {
        Vec::new()
    } else {
        get_children(surface)
    };
    if !allow_window_geometry && !children.is_empty() {
        return None;
    }
    // A native compositor client can paint its window in subsurfaces while
    // the root still receives all input. Only an explicit empty input region
    // on every descendant preserves the original root-only input authority.
    const MAX_PAINT_CHILDREN: usize = 64;
    let mut index = 0;
    while let Some(child) = children.get(index) {
        if children.len() > MAX_PAINT_CHILDREN || !has_empty_input_region(child) {
            return None;
        }
        let descendants = get_children(child);
        children.extend(descendants);
        index += 1;
    }
    let paint_children = children.iter().map(Resource::id).collect();
    let size = (authority.width as i32, authority.height as i32);
    // Capture draws at scale 1 in logical surface pixels, including when the
    // root attaches an integer HiDPI buffer. Viewport transforms need a
    // different mapping and are rejected below.
    let (root_size, rendered_scale) = with_renderer_surface_state(surface, |state| {
        let root = state.buffer_size()?;
        let view = state.view()?;
        (view.offset.x == 0
            && view.offset.y == 0
            && view.src.loc.x == 0.0
            && view.src.loc.y == 0.0
            && (view.src.size.w, view.src.size.h) == (f64::from(root.w), f64::from(root.h))
            && view.dst == root)
            .then_some(((root.w, root.h), state.buffer_scale()))
    })
    .flatten()?;
    if !allow_window_geometry && root_size != size {
        return None;
    }
    with_states(surface, |states| {
        if states.data_map.get::<NonzeroPointerOffset>().is_some() {
            return None;
        }
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        let a = attrs.current();
        if a.buffer_scale != rendered_scale
            || (!allow_window_geometry && a.buffer_scale != 1)
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
        let (x, y, width, height) = geometry.unwrap_or((0, 0, root_size.0, root_size.1));
        // The same xdg geometry origin used by the capture renderer is the
        // offset from capture coordinates back into the root's input region.
        if (width, height) != size
            || (!allow_window_geometry && (x, y) != (0, 0))
            || x < 0
            || y < 0
            || x.checked_add(width).is_none_or(|right| right > root_size.0)
            || y.checked_add(height)
                .is_none_or(|bottom| bottom > root_size.1)
        {
            return None;
        }
        Some(Mapping {
            geometry,
            root_size,
            region,
            paint_children,
        })
    })
}

type InputRegion = Vec<(bool, Rectangle<i32, Logical>)>;

#[derive(PartialEq)]
struct HitSurface {
    surface: WlSurface,
    bounds: Rectangle<i32, Logical>,
    region: Option<InputRegion>,
}

/// Same-client targets in the exact order painted into the shared root.
/// The frame stores this geometry before sending ready. Live hit testing is
/// permitted only when the comparable tree still describes the same targets.
#[derive(PartialEq)]
pub(crate) struct WindowInputLayout(Vec<HitSurface>);

impl WindowInputLayout {
    #[cfg(feature = "backend-wayland-udev")]
    pub(super) fn surface_count(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn snapshot(root: &WlSurface) -> Option<Self> {
        use smithay::backend::renderer::utils::RendererSurfaceStateUserData;

        let mut budget = 0;
        let mapped = super::capture_popup_snapshot::validate_tree(root, (0, 0), &mut budget)?;
        let mut entries = Some(Vec::with_capacity(mapped));
        with_surface_tree_downward(
            root,
            Point::<i32, Logical>::from((0, 0)),
            |_, states, point| {
                let view = states
                    .data_map
                    .get::<RendererSurfaceStateUserData>()
                    .and_then(|renderer| renderer.lock().unwrap().view());
                view.map_or(TraversalAction::SkipChildren, |view| {
                    TraversalAction::DoChildren(*point + view.offset)
                })
            },
            |surface, states, point| {
                let Some(view) = states
                    .data_map
                    .get::<RendererSurfaceStateUserData>()
                    .and_then(|renderer| renderer.lock().unwrap().view())
                else {
                    return;
                };
                if surface != root
                    && states
                        .cached_state
                        .get::<SubsurfaceCachedState>()
                        .current()
                        .location
                        != view.offset
                {
                    entries = None;
                    return;
                }
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                let region = attrs.current().input_region.as_ref();
                if region.is_some_and(|region| region.rects.len() > 256) {
                    entries = None;
                    return;
                }
                if let Some(entries) = entries.as_mut() {
                    entries.push(HitSurface {
                        surface: surface.clone(),
                        bounds: Rectangle::new(*point + view.offset, view.dst),
                        region: region.map(|region| {
                            region
                                .rects
                                .iter()
                                .map(|(kind, bounds)| (matches!(kind, RectangleKind::Add), *bounds))
                                .collect()
                        }),
                    });
                }
            },
            |_, _, _| true,
        );
        entries
            .filter(|entries| entries.len() == mapped && mapped > 0)
            .map(Self)
    }

    pub(super) fn hit(&self, point: (f64, f64)) -> Option<(&WlSurface, (f64, f64))> {
        let p = Point::<f64, Logical>::from(point);
        self.0.iter().find_map(|target| {
            if !target.bounds.to_f64().contains(p) {
                return None;
            }
            let offset = p - target.bounds.loc.to_f64();
            let local = offset.to_i32_floor::<i32>();
            let accepts = target.region.as_ref().is_none_or(|rects| {
                rects
                    .iter()
                    .rev()
                    .find(|(_, bounds)| {
                        let x = i64::from(local.x) - i64::from(bounds.loc.x);
                        let y = i64::from(local.y) - i64::from(bounds.loc.y);
                        x >= 0
                            && x < i64::from(bounds.size.w)
                            && y >= 0
                            && y < i64::from(bounds.size.h)
                    })
                    .is_some_and(|(add, _)| *add)
            });
            accepts.then_some((&target.surface, (offset.x, offset.y)))
        })
    }
}
fn action_allowed(kind: u32, axes: u32) -> bool {
    axes <= 3
        && (matches!(kind, 0 | 1)
            || (axes & 1 != 0 && matches!(kind, 2 | 3))
            || (axes & 2 != 0 && matches!(kind, 4 | 5)))
}
pub(super) fn point_allowed(mapping: &Mapping, x: u32, y: u32) -> bool {
    mapped_point(mapping, x, y).is_some()
}
pub(super) fn mapped_point(mapping: &Mapping, x: u32, y: u32) -> Option<(f64, f64)> {
    let (ox, oy, width, height) =
        mapping
            .geometry
            .unwrap_or((0, 0, mapping.root_size.0, mapping.root_size.1));
    if i64::from(x) >= i64::from(width) || i64::from(y) >= i64::from(height) {
        return None;
    }
    let x = i64::from(x) + i64::from(ox);
    let y = i64::from(y) + i64::from(oy);
    let Some(region) = &mapping.region else {
        return Some((x as f64, y as f64));
    };
    let mut inside = false;
    for &(add, rx, ry, rw, rh) in region {
        if x >= i64::from(rx)
            && y >= i64::from(ry)
            && x < i64::from(rx) + i64::from(rw)
            && y < i64::from(ry) + i64::from(rh)
        {
            inside = add;
        }
    }
    inside.then_some((x as f64, y as f64))
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
            paint_children: Vec::new(),
            geometry: None,
            root_size: (64, 48),
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
