use super::{JwmClientState, JwmWaylandState};
use crate::backend::api::{BackendEvent, Geometry, NetWmAction, NetWmState, WindowType};
use crate::backend::common_define::WindowId;
use smithay::reexports::calloop::{EventLoop, channel};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationToken, XdgActivationTokenData,
};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
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

fn wire_message(sender: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let size = 8 + payload.len();
    assert_eq!(size % 4, 0);
    let mut message = Vec::with_capacity(size);
    message.extend_from_slice(&sender.to_ne_bytes());
    message.extend_from_slice(&(((size as u32) << 16) | u32::from(opcode)).to_ne_bytes());
    message.extend_from_slice(payload);
    message
}

fn wire_u32(value: u32) -> [u8; 4] {
    value.to_ne_bytes()
}

fn wire_string(value: &str) -> Vec<u8> {
    let len = value.len() + 1;
    let mut encoded = Vec::with_capacity(4 + (len + 3) / 4 * 4);
    encoded.extend_from_slice(&(len as u32).to_ne_bytes());
    encoded.extend_from_slice(value.as_bytes());
    encoded.push(0);
    encoded.resize((encoded.len() + 3) & !3, 0);
    encoded
}

fn wire_bind(registry_name: u32, object_id: u32, interface: &str, version: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&registry_name.to_ne_bytes());
    payload.extend_from_slice(&wire_string(interface));
    payload.extend_from_slice(&version.to_ne_bytes());
    payload.extend_from_slice(&object_id.to_ne_bytes());
    wire_message(2, 0, &payload)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("wire u32"))
}

fn discover_globals(peer: &mut UnixStream) -> Vec<(u32, String, u32)> {
    let mut bytes = Vec::new();
    peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("set wire read timeout");
    loop {
        let mut chunk = [0u8; 8192];
        let read = peer.read(&mut chunk).expect("read registry events");
        assert!(read > 0, "Wayland server closed during registry discovery");
        bytes.extend_from_slice(&chunk[..read]);

        let mut offset = 0;
        let mut done = false;
        while bytes.len().saturating_sub(offset) >= 8 {
            let sender = read_u32(&bytes, offset);
            let header = read_u32(&bytes, offset + 4);
            let size = (header >> 16) as usize;
            let opcode = header as u16;
            if size < 8 || bytes.len() - offset < size {
                break;
            }
            if sender == 3 && opcode == 0 {
                done = true;
            }
            offset += size;
        }
        if done {
            break;
        }
    }

    let mut globals = Vec::new();
    let mut offset = 0;
    while bytes.len().saturating_sub(offset) >= 8 {
        let sender = read_u32(&bytes, offset);
        let header = read_u32(&bytes, offset + 4);
        let size = (header >> 16) as usize;
        let opcode = header as u16;
        assert!(size >= 8 && offset + size <= bytes.len());
        if sender == 2 && opcode == 0 {
            let payload = offset + 8;
            let name = read_u32(&bytes, payload);
            let string_len = read_u32(&bytes, payload + 4) as usize;
            assert!(string_len > 0);
            let string_start = payload + 8;
            let interface = std::str::from_utf8(
                &bytes[string_start..string_start + string_len.saturating_sub(1)],
            )
            .expect("registry interface is UTF-8")
            .to_owned();
            let padded_len = (string_len + 3) & !3;
            let version = read_u32(&bytes, string_start + padded_len);
            globals.push((name, interface, version));
        }
        offset += size;
    }
    globals
}

#[test]
fn xdg_toplevel_wire_requests_reach_shared_window_policy() {
    let event_loop: EventLoop<'static, JwmWaylandState> =
        EventLoop::try_new().expect("create test event loop");
    let mut display = Display::<JwmWaylandState>::new().expect("create test display");
    let mut display_handle = display.handle();
    let pending_events = Arc::new(Mutex::new(VecDeque::new()));
    let (flush_tx, _flush_rx) = channel::channel();
    let (mut state, socket_name) = JwmWaylandState::init(
        &display_handle,
        event_loop.handle(),
        pending_events.clone(),
        flush_tx,
        Arc::new(AtomicBool::new(false)),
        "wire-test-seat".to_owned(),
        false,
        false,
    )
    .expect("initialize wire-test Wayland state");
    assert!(socket_name.is_none());

    let (server, mut peer) = UnixStream::pair().expect("create Wayland socket pair");
    display_handle
        .insert_client(server, Arc::new(JwmClientState::default()))
        .expect("insert raw Wayland client");

    // wl_display.get_registry(new_id=2), followed by sync(new_id=3) so the
    // callback.done event provides a deterministic end marker for discovery.
    let mut discovery = wire_message(1, 1, &wire_u32(2));
    discovery.extend_from_slice(&wire_message(1, 0, &wire_u32(3)));
    peer.write_all(&discovery).expect("send registry request");
    display
        .dispatch_clients(&mut state)
        .expect("dispatch registry request");
    display.flush_clients().expect("flush registry events");
    let globals = discover_globals(&mut peer);

    let (compositor_name, compositor_version) = globals
        .iter()
        .find_map(|(name, interface, version)| {
            (interface == "wl_compositor").then_some((*name, *version))
        })
        .expect("wl_compositor global");
    let (xdg_name, xdg_version) = globals
        .iter()
        .find_map(|(name, interface, version)| {
            (interface == "xdg_wm_base").then_some((*name, *version))
        })
        .expect("xdg_wm_base global");

    let mut requests = wire_bind(
        compositor_name,
        4,
        "wl_compositor",
        compositor_version.min(6),
    );
    requests.extend_from_slice(&wire_bind(xdg_name, 5, "xdg_wm_base", xdg_version.min(6)));
    // wl_compositor.create_surface(6)
    requests.extend_from_slice(&wire_message(4, 0, &wire_u32(6)));
    // xdg_wm_base.get_xdg_surface(new_id=7, wl_surface=6)
    let mut get_xdg_surface = Vec::new();
    get_xdg_surface.extend_from_slice(&wire_u32(7));
    get_xdg_surface.extend_from_slice(&wire_u32(6));
    requests.extend_from_slice(&wire_message(5, 2, &get_xdg_surface));
    // xdg_surface.get_toplevel(new_id=8)
    requests.extend_from_slice(&wire_message(7, 1, &wire_u32(8)));
    // xdg_toplevel.set_fullscreen(output=null), unset_fullscreen, set_minimized.
    requests.extend_from_slice(&wire_message(8, 11, &wire_u32(0)));
    requests.extend_from_slice(&wire_message(8, 12, &[]));
    requests.extend_from_slice(&wire_message(8, 13, &[]));

    peer.write_all(&requests)
        .expect("send xdg toplevel requests");
    display
        .dispatch_clients(&mut state)
        .expect("dispatch xdg toplevel requests");

    let events = pending_events.lock().expect("pending event lock");
    let window = events
        .iter()
        .find_map(|event| match event {
            BackendEvent::WindowCreated(window) => Some(*window),
            _ => None,
        })
        .expect("xdg_toplevel creation reaches the backend");
    let requests = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::WindowStateRequest {
                window: event_window,
                action,
                state,
            } if *event_window == window => Some((*action, *state)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests,
        [
            (NetWmAction::Add, NetWmState::Fullscreen),
            (NetWmAction::Remove, NetWmState::Fullscreen),
            (NetWmAction::Add, NetWmState::Hidden),
        ]
    );
    assert_eq!(
        state.window_is_fullscreen.get(&window),
        Some(&false),
        "protocol callbacks must leave confirmation to shared policy"
    );
    drop(events);

    let surface = state
        .toplevels
        .get(&window)
        .expect("wire-created toplevel remains live")
        .wl_surface()
        .clone();
    let previously_active = WindowId::from_raw(900);
    state.active_toplevel = Some(previously_active);
    XdgActivationHandler::request_activation(
        &mut state,
        XdgActivationToken::from("fresh-token".to_owned()),
        XdgActivationTokenData {
            app_id: Some("fresh.app".to_owned()),
            ..XdgActivationTokenData::default()
        },
        surface.clone(),
    );
    assert_eq!(state.active_toplevel, Some(previously_active));
    assert_eq!(
        state
            .window_activation_app_id
            .get(&window)
            .map(String::as_str),
        Some("fresh.app")
    );
    let events = pending_events.lock().expect("pending event lock");
    assert!(matches!(
        events.back(),
        Some(BackendEvent::ActiveWindowMessage { window: active }) if *active == window
    ));
    let event_count = events.len();
    drop(events);

    XdgActivationHandler::request_activation(
        &mut state,
        XdgActivationToken::from("expired-token".to_owned()),
        XdgActivationTokenData {
            app_id: Some("expired.app".to_owned()),
            timestamp: Instant::now() - std::time::Duration::from_secs(11),
            ..XdgActivationTokenData::default()
        },
        surface,
    );
    assert_eq!(state.active_toplevel, Some(previously_active));
    assert_eq!(
        state
            .window_activation_app_id
            .get(&window)
            .map(String::as_str),
        Some("fresh.app"),
        "an expired token must not replace activation metadata"
    );
    assert_eq!(
        pending_events.lock().expect("pending event lock").len(),
        event_count,
        "an expired token must not request activation"
    );
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
fn x11_mode_and_activation_requests_reuse_shared_policy_events() {
    let (mut state, pending_events) = headless_state();
    let win = WindowId::from_raw(51);
    state.x11_surface_to_window.insert(0x1234, win);

    state.request_x11_minimized(0x1234, true);
    state.request_x11_minimized(0x1234, false);
    state.request_x11_state(0x1234, NetWmState::Fullscreen, true);
    state.request_x11_state(0x1234, NetWmState::Fullscreen, false);
    state.request_x11_activation(0x1234);
    state.request_x11_minimized(0x9999, true);
    state.request_x11_state(0x9999, NetWmState::Fullscreen, true);
    state.request_x11_activation(0x9999);

    let events = pending_events.lock().expect("pending event lock");
    assert_eq!(events.len(), 5, "an unknown X11 id must not emit an event");
    assert!(matches!(
        events.get(0),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Add,
            state: NetWmState::Hidden,
        }) if *window == win
    ));
    assert!(matches!(
        events.get(1),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Remove,
            state: NetWmState::Hidden,
        }) if *window == win
    ));
    assert!(matches!(
        events.get(2),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Add,
            state: NetWmState::Fullscreen,
        }) if *window == win
    ));
    assert!(matches!(
        events.get(3),
        Some(BackendEvent::WindowStateRequest {
            window,
            action: NetWmAction::Remove,
            state: NetWmState::Fullscreen,
        }) if *window == win
    ));
    assert!(matches!(
        events.get(4),
        Some(BackendEvent::ActiveWindowMessage { window }) if *window == win
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
