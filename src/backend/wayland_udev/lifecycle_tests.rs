use super::JwmWaylandState;
use crate::backend::api::{BackendEvent, Geometry, NetWmAction, NetWmState, WindowType};
use crate::backend::common_define::WindowId;
use smithay::reexports::calloop::{EventLoop, channel};
use smithay::reexports::wayland_server::Display;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn headless_state() -> (JwmWaylandState, Arc<Mutex<VecDeque<BackendEvent>>>) {
    let event_loop: EventLoop<'static, JwmWaylandState> =
        EventLoop::try_new().expect("create test event loop");
    let display = Display::<JwmWaylandState>::new().expect("create test display");
    let pending_events = Arc::new(Mutex::new(VecDeque::new()));
    let (flush_tx, _flush_rx) = channel::channel();
    let (state, socket_name) = JwmWaylandState::init(
        &display.handle(),
        event_loop.handle(),
        pending_events.clone(),
        flush_tx,
        Arc::new(AtomicBool::new(false)),
        "test-seat".to_owned(),
        false,
        false,
    )
    .expect("initialize headless Wayland state");
    assert!(socket_name.is_none());
    (state, pending_events)
}

#[test]
fn remove_wayland_window_closes_foreign_handle_and_clears_owned_state() {
    let (mut state, pending_events) = headless_state();
    let removed = WindowId::from_raw(41);
    let surviving = WindowId::from_raw(42);

    let removed_handle = state
        .foreign_toplevel_list_state
        .new_toplevel_with_identifier::<JwmWaylandState>("removed", "test", "jwm-test-41");
    let surviving_handle = state
        .foreign_toplevel_list_state
        .new_toplevel_with_identifier::<JwmWaylandState>("surviving", "test", "jwm-test-42");
    state
        .foreign_toplevel_handles
        .insert(removed, removed_handle.clone());
    state
        .foreign_toplevel_handles
        .insert(surviving, surviving_handle.clone());

    for win in [removed, surviving] {
        state.pending_initial_configure.insert(win);
        state
            .pending_size_reconfigure
            .insert(win, ((640, 480), Instant::now()));
        state.window_geometry.insert(
            win,
            Geometry {
                x: 1,
                y: 2,
                w: 640,
                h: 480,
                border: 3,
            },
        );
        state.window_stack.push(win);
        state.mapped_windows.insert(win);
        state.window_title.insert(win, format!("window-{win:?}"));
        state.window_app_id.insert(win, "test.app".to_owned());
        state
            .window_activation_app_id
            .insert(win, "activation.app".to_owned());
        state.window_is_fullscreen.insert(win, true);
        state
            .window_type_overrides
            .insert(win, vec![WindowType::Dialog]);
        state.window_border_color.insert(win, [1.0, 0.5, 0.0, 1.0]);
    }

    state.remove_wayland_window(removed);

    assert!(removed_handle.is_closed());
    assert!(!surviving_handle.is_closed());
    assert!(!state.foreign_toplevel_handles.contains_key(&removed));
    assert!(state.foreign_toplevel_handles.contains_key(&surviving));
    assert!(!state.pending_initial_configure.contains(&removed));
    assert!(!state.pending_size_reconfigure.contains_key(&removed));
    assert!(!state.window_geometry.contains_key(&removed));
    assert!(!state.window_stack.contains(&removed));
    assert!(!state.mapped_windows.contains(&removed));
    assert!(!state.window_title.contains_key(&removed));
    assert!(!state.window_app_id.contains_key(&removed));
    assert!(!state.window_activation_app_id.contains_key(&removed));
    assert!(!state.window_is_fullscreen.contains_key(&removed));
    assert!(!state.window_type_overrides.contains_key(&removed));
    assert!(!state.window_border_color.contains_key(&removed));

    assert!(state.pending_initial_configure.contains(&surviving));
    assert!(state.pending_size_reconfigure.contains_key(&surviving));
    assert!(state.window_geometry.contains_key(&surviving));
    assert!(state.window_stack.contains(&surviving));
    assert!(state.mapped_windows.contains(&surviving));
    assert!(state.window_title.contains_key(&surviving));
    assert!(state.window_app_id.contains_key(&surviving));
    assert!(state.window_activation_app_id.contains_key(&surviving));
    assert!(state.window_is_fullscreen.contains_key(&surviving));
    assert!(state.window_type_overrides.contains_key(&surviving));
    assert!(state.window_border_color.contains_key(&surviving));

    assert_eq!(state.compositor_dead_windows, vec![removed.raw()]);
    let events = pending_events.lock().expect("pending event lock");
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events.front(),
        Some(BackendEvent::WindowDestroyed(win)) if *win == removed
    ));
}

#[test]
fn x11_minimize_requests_reuse_the_shared_hidden_state_event() {
    let (mut state, pending_events) = headless_state();
    let win = WindowId::from_raw(51);
    state.x11_surface_to_window.insert(0x1234, win);

    state.request_x11_minimized(0x1234, true);
    state.request_x11_minimized(0x1234, false);
    state.request_x11_minimized(0x9999, true);

    let events = pending_events.lock().expect("pending event lock");
    assert_eq!(events.len(), 2, "an unknown X11 id must not emit an event");
    assert!(matches!(
        events.front(),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Add,
            state: NetWmState::Hidden,
        }) if *window == win
    ));
    assert!(matches!(
        events.back(),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Remove,
            state: NetWmState::Hidden,
        }) if *window == win
    ));
}

#[test]
fn manager_hidden_window_stays_gated_until_explicit_restore() {
    let (mut state, _pending_events) = headless_state();
    let win = WindowId::from_raw(61);

    state.set_manager_window_mapped(win, false);
    assert!(!state.mapped_windows.contains(&win));
    assert!(!state.manager_allows_surface_map(win));
    assert!(
        state.take_window_mapping(win),
        "a real unmap must still acknowledge a manager-hidden window"
    );

    state.set_manager_window_mapped(win, false);
    state.set_manager_window_mapped(win, true);
    assert!(state.mapped_windows.contains(&win));
    assert!(state.manager_allows_surface_map(win));
}
