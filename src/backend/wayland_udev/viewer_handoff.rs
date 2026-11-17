//! Explicit, nonrenewable JWM udev viewer focus transaction. Never remote authority.
use crate::backend::wayland::state::JwmWaylandState;
use crate::viewer_handoff_protocol::server::{
    jwm_remote_viewer_handoff_manager_v1::{self as manager, JwmRemoteViewerHandoffManagerV1},
    jwm_remote_viewer_handoff_session_v1::{self as session, JwmRemoteViewerHandoffSessionV1},
};
use smithay::input::Seat;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::ObjectId, protocol::wl_surface::WlSurface,
};
use smithay::utils::{SERIAL_COUNTER, Serial};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(crate) struct ManagerData;
pub(crate) struct SessionData;
#[derive(Default)]
pub(crate) struct HandoffState {
    session: Option<Session>,
    attempted: HashMap<ObjectId, Weak<WlSurface>>,
    changing_focus: bool,
    quarantine: bool,
    restore: Option<WlSurface>,
}
struct Session {
    resource: JwmRemoteViewerHandoffSessionV1,
    surface: WlSurface,
    deadline: Instant,
    active: bool,
    sequence: u32,
    last_enter: Option<Serial>,
    prior: Option<WlSurface>,
    outside_since: Option<Instant>,
    geometry: Option<(i32, i32, u32, u32)>,
    seen_mapped: bool,
}
// Called only by actual udev setup, not shared/nested state initialization.
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn init(dh: &DisplayHandle) {
    dh.create_global::<JwmWaylandState, JwmRemoteViewerHandoffManagerV1, _>(3, ManagerData);
}
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn active(state: &JwmWaylandState) -> bool {
    state
        .viewer_handoff
        .session
        .as_ref()
        .is_some_and(|s| s.active)
}
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn inhibits(state: &JwmWaylandState) -> bool {
    active(state)
}
pub(crate) fn quarantined(state: &JwmWaylandState) -> bool {
    state.viewer_handoff.quarantine
}
#[cfg(all(test, feature = "backend-wayland-udev"))]
pub(crate) fn registered_for_test(state: &JwmWaylandState) -> bool {
    state.viewer_handoff.session.is_some()
}
fn eligible(state: &JwmWaylandState, surface: &WlSurface) -> bool {
    surface.is_alive()
        && !state.session_locked
        && state.remote_keyboard_seat_eligible()
        && state
            .seat
            .get_keyboard()
            .is_some_and(|keyboard| !keyboard.is_grabbed())
        && super::remote_pointer::unconstrained(state, surface)
        && !state.layer_shell_state.layer_surfaces().any(|layer| {
            state
                .surface_to_window
                .get(&layer.wl_surface().id())
                .is_some_and(|win| state.mapped_windows.contains(win))
                && layer.with_cached_state(|data| {
                    data.keyboard_interactivity
                        == smithay::wayland::shell::wlr_layer::KeyboardInteractivity::Exclusive
                })
        })
        && smithay::desktop::PopupManager::popups_for_surface(surface)
            .next()
            .is_none()
        && state
            .surface_to_window
            .get(&surface.id())
            .is_some_and(|window| {
                state
                    .toplevels
                    .get(window)
                    .is_some_and(|top| top.wl_surface() == surface)
            })
}
fn geometry(state: &JwmWaylandState, surface: &WlSurface) -> Option<(i32, i32, u32, u32)> {
    let window = state.surface_to_window.get(&surface.id())?;
    if !state.mapped_windows.contains(window) {
        return None;
    }
    state
        .window_geometry
        .get(window)
        .map(|g| (g.x, g.y, g.w, g.h))
}
fn pointer_hits_surface(state: &JwmWaylandState, surface: &WlSurface) -> bool {
    state
        .surface_under(state.pointer_location)
        .is_some_and(|(_, hit, _)| &hit == surface)
}
fn change_focus(state: &mut JwmWaylandState, focus: Option<WlSurface>) {
    state.viewer_handoff.changing_focus = true;
    state.set_keyboard_focus(focus, SERIAL_COUNTER.next_serial());
    // A grab may ignore or redirect a focus request. Project only its actual
    // result, never deactivate a still-focused viewer based on requested None.
    let win = state
        .seat
        .get_keyboard()
        .and_then(|keyboard| keyboard.current_focus())
        .and_then(|surface| state.surface_to_window.get(&surface.id()).copied())
        .filter(|window| state.toplevels.contains_key(window));
    state.set_active_toplevel(win);
    state.viewer_handoff.changing_focus = false;
}
fn release_focus(state: &mut JwmWaylandState, s: &Session) {
    // Never overwrite a newer owner, including a lock/exclusive surface.
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    if keyboard.current_focus().as_ref() != Some(&s.surface) {
        return;
    }
    let prior = s
        .prior
        .clone()
        .filter(|surface| eligible(state, surface) && geometry(state, surface).is_some());
    let held = keyboard.pressed_keys();
    if !held.is_empty() {
        state.viewer_handoff.quarantine = true;
        state.viewer_handoff.restore = prior;
        // Retire delivery history before any focus change: a grab may ignore
        // None or redirect focus. Never synthesize releases into its recipient.
        // Physical/XKB held state remains until real releases in quarantine.
        keyboard.retire_forwarded_keys(&held.into_iter().collect::<Vec<_>>());
        change_focus(state, None);
    } else {
        change_focus(state, prior);
    }
}
fn terminate(state: &mut JwmWaylandState, reason: u32) {
    if let Some(mut s) = state.viewer_handoff.session.take() {
        let was_active = s.active;
        s.active = false;
        if s.resource.is_alive() {
            s.resource.revoked(reason);
        }
        if was_active {
            release_focus(state, &s);
        }
        state.request_client_flush();
    }
}
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn escape(state: &mut JwmWaylandState) {
    terminate(state, 4);
}
/// Called under Smithay's keyboard lock; never query or change that lock here.
pub(crate) fn focus_changed(state: &mut JwmWaylandState, focus: Option<&WlSurface>) {
    if !state.viewer_handoff.changing_focus
        && state
            .viewer_handoff
            .session
            .as_ref()
            .is_some_and(|s| s.active && focus != Some(&s.surface))
    {
        if let Some(s) = state.viewer_handoff.session.take() {
            if s.resource.is_alive() {
                s.resource.revoked(3);
            }
            state.request_client_flush();
        }
    }
}
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn quarantine_key_finished(state: &mut JwmWaylandState) {
    if !quarantined(state)
        || !state
            .seat
            .get_keyboard()
            .is_some_and(|k| k.pressed_keys().is_empty())
    {
        return;
    }
    state.viewer_handoff.quarantine = false;
    let restore = state
        .viewer_handoff
        .restore
        .take()
        .filter(|s| eligible(state, s) && geometry(state, s).is_some());
    if state
        .seat
        .get_keyboard()
        .is_some_and(|k| k.current_focus().is_none())
        && !state.session_locked
    {
        change_focus(state, restore);
    }
}
/// Revalidate at the physical-input authorization boundary, not just on the
/// periodic timer. This runs outside Smithay's keyboard lock.
#[cfg(feature = "backend-wayland-udev")]
pub(crate) fn refresh_before_input(state: &mut JwmWaylandState) {
    watchdog(state);
}
fn watchdog(state: &mut JwmWaylandState) -> bool {
    let Some(mut s) = state.viewer_handoff.session.take() else {
        return false;
    };
    let now = Instant::now();
    let mapped = geometry(state, &s.surface);
    let valid = s.resource.is_alive()
        && eligible(state, &s.surface)
        && (!s.active
            || state
                .seat
                .get_keyboard()
                .is_some_and(|keyboard| keyboard.current_focus().as_ref() == Some(&s.surface)))
        && now < s.deadline
        && (!s.seen_mapped || mapped.is_some())
        && s.geometry.is_none_or(|g| mapped == Some(g))
        // A real pointer leave gets its bounded exit transaction. A stationary
        // pointer whose stale focus no longer matches scene hit-testing does not.
        && (!s.active || state.seat.get_pointer().is_none_or(|pointer| {
            pointer.current_focus().as_ref() != Some(&s.surface)
                || pointer_hits_surface(state, &s.surface)
        }));
    s.seen_mapped |= mapped.is_some();
    if s.active {
        if state
            .seat
            .get_pointer()
            .is_some_and(|p| p.current_focus().as_ref() == Some(&s.surface))
        {
            s.outside_since = None;
        } else {
            s.outside_since.get_or_insert(now);
        }
    }
    let overdue = s
        .outside_since
        .is_some_and(|at| now.duration_since(at) >= Duration::from_millis(500));
    state.viewer_handoff.session = Some(s);
    if !valid || overdue {
        terminate(state, 2);
        false
    } else {
        true
    }
}
impl GlobalDispatch<JwmRemoteViewerHandoffManagerV1, ManagerData> for JwmWaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<JwmRemoteViewerHandoffManagerV1>,
        _: &ManagerData,
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ManagerData);
    }
    fn can_view(client: Client, _: &ManagerData) -> bool {
        !crate::backend::wayland::state::client_is_sandboxed(&client)
    }
}
impl Dispatch<JwmRemoteViewerHandoffManagerV1, ManagerData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _: &JwmRemoteViewerHandoffManagerV1,
        request: manager::Request,
        _: &ManagerData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let (id, surface, seat, ttl_ms, max_ttl_ms) = match request {
            manager::Request::Create {
                id,
                surface,
                seat,
                ttl_ms,
            } => (id, surface, seat, ttl_ms, 30_000),
            manager::Request::CreateTimed {
                id,
                surface,
                seat,
                ttl_ms,
            } => (id, surface, seat, ttl_ms, 300_000),
            _ => return,
        };
        let resource = init.init(id, SessionData);
        state
            .viewer_handoff
            .attempted
            .retain(|_, surface| surface.upgrade().is_ok_and(|s| s.is_alive()));
        let owns = surface.client().as_ref().map(Client::id) == Some(client.id())
            && seat.client().as_ref().map(Client::id) == Some(client.id());
        if !owns
            || !eligible(state, &surface)
            || !(1..=max_ttl_ms).contains(&ttl_ms)
            || Seat::<Self>::from_resource(&seat).as_ref() != Some(&state.seat)
            || state.viewer_handoff.session.is_some()
            || quarantined(state)
            || state.viewer_handoff.attempted.len() >= 64
            || state.viewer_handoff.attempted.contains_key(&surface.id())
        {
            resource.revoked(1);
            state.request_client_flush();
            return;
        }
        state
            .viewer_handoff
            .attempted
            .insert(surface.id(), surface.downgrade());
        state.viewer_handoff.session = Some(Session {
            resource: resource.clone(),
            surface,
            deadline: Instant::now() + Duration::from_millis(u64::from(ttl_ms)),
            active: false,
            sequence: 0,
            last_enter: None,
            prior: None,
            outside_since: None,
            geometry: None,
            seen_mapped: false,
        });
        let owner = resource.id();
        if state
            .loop_handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(10)),
                move |_, _, state| {
                    if !state
                        .viewer_handoff
                        .session
                        .as_ref()
                        .is_some_and(|s| s.resource.id() == owner)
                    {
                        return TimeoutAction::Drop;
                    }
                    if watchdog(state) {
                        TimeoutAction::ToDuration(Duration::from_millis(10))
                    } else {
                        TimeoutAction::Drop
                    }
                },
            )
            .is_err()
        {
            terminate(state, 1);
            return;
        }
        resource.granted(ttl_ms);
        state.request_client_flush();
    }
}
impl Dispatch<JwmRemoteViewerHandoffSessionV1, SessionData> for JwmWaylandState {
    fn request(
        state: &mut Self,
        _: &Client,
        resource: &JwmRemoteViewerHandoffSessionV1,
        request: session::Request,
        _: &SessionData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if !state
            .viewer_handoff
            .session
            .as_ref()
            .is_some_and(|s| s.resource == *resource)
        {
            return;
        }
        if matches!(request, session::Request::Destroy) {
            terminate(state, 1);
            return;
        }
        if !watchdog(state) {
            return;
        }
        let mut s = state.viewer_handoff.session.take().unwrap();
        let sequence = match request {
            session::Request::Enter { sequence, .. } | session::Request::Exit { sequence } => {
                sequence
            }
            _ => 0,
        };
        if sequence != s.sequence + 1 || sequence > 60_000 {
            state.viewer_handoff.session = Some(s);
            terminate(state, 5);
            return;
        }
        match request {
            session::Request::Enter { serial, .. } => {
                let serial = Serial::from(serial);
                let pointer_clean = state.seat.get_pointer().is_some_and(|p| {
                    !p.is_grabbed()
                        && !p.has_pressed_buttons()
                        && p.current_focus().as_ref() == Some(&s.surface)
                        && p.last_enter() == Some(serial)
                        && pointer_hits_surface(state, &s.surface)
                });
                let keyboard_clean = state.seat.get_keyboard().is_some_and(|k| {
                    let m = k.modifier_state().serialized;
                    !k.is_grabbed()
                        && k.current_focus().is_none_or(|focus| {
                            state
                                .surface_to_window
                                .get(&focus.id())
                                .is_some_and(|window| {
                                    state
                                        .toplevels
                                        .get(window)
                                        .is_some_and(|top| top.wl_surface() == &focus)
                                })
                        })
                        && k.pressed_keys().is_empty()
                        && m.depressed == 0
                        && m.latched == 0
                        && m.locked == 0
                        && m.layout_effective == 0
                });
                if s.active
                    || s.last_enter == Some(serial)
                    || quarantined(state)
                    || !pointer_clean
                    || !keyboard_clean
                    || geometry(state, &s.surface).is_none()
                {
                    state.viewer_handoff.session = Some(s);
                    terminate(state, 5);
                    return;
                }
                s.resource.transition(sequence, 1);
                s.prior = state
                    .seat
                    .get_keyboard()
                    .and_then(|k| k.current_focus())
                    .filter(|f| f != &s.surface);
                s.geometry = geometry(state, &s.surface);
                s.seen_mapped = true;
                s.last_enter = Some(serial);
                s.sequence = sequence;
                s.active = true;
                let surface = s.surface.clone();
                state.viewer_handoff.session = Some(s);
                change_focus(state, Some(surface));
                if let Some(s) = state.viewer_handoff.session.as_ref() {
                    s.resource.active(sequence);
                }
            }
            session::Request::Exit { .. } => {
                s.resource.transition(sequence, 0);
                s.sequence = sequence;
                let was_active = s.active;
                s.active = false;
                s.outside_since = None;
                if was_active {
                    release_focus(state, &s);
                }
                s.resource.inactive(sequence);
                state.viewer_handoff.session = Some(s);
            }
            _ => {
                state.viewer_handoff.session = Some(s);
                terminate(state, 5);
            }
        }
        state.request_client_flush();
    }
    fn destroyed(
        state: &mut Self,
        _: smithay::reexports::wayland_server::backend::ClientId,
        resource: &JwmRemoteViewerHandoffSessionV1,
        _: &SessionData,
    ) {
        if state
            .viewer_handoff
            .session
            .as_ref()
            .is_some_and(|s| s.resource == *resource)
        {
            terminate(state, 1);
        }
    }
}

#[cfg(all(test, feature = "backend-wayland-udev"))]
#[path = "viewer_handoff_tests.rs"]
mod tests;
