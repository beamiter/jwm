use super::{JwmClientState, JwmWaylandState};
use crate::backend::api::{
    BackendEvent, Geometry, MaximizeAxes, NetWmAction, NetWmState, WindowType,
};
use crate::backend::common_define::WindowId;
use smithay::reexports::calloop::{EventLoop, channel};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux, ConnectionExt,
    CreateWindowAux, EventMask, WindowClass,
};
use smithay::reexports::x11rb::rust_connection::RustConnection;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationToken, XdgActivationTokenData,
};
use smithay::xwayland::X11Wm;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn intern(conn: &RustConnection, name: &[u8]) -> Atom {
    conn.intern_atom(false, name)
        .expect("intern atom request")
        .reply()
        .expect("intern atom reply")
        .atom
}

fn pump_xwm(
    event_loop: &mut EventLoop<'static, JwmWaylandState>,
    state: &mut JwmWaylandState,
    until: impl Fn(&JwmWaylandState) -> bool,
) {
    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    while !until(state) && Instant::now() < deadline {
        event_loop
            .dispatch(std::time::Duration::from_millis(10), state)
            .expect("dispatch XWM events");
    }
    assert!(until(state), "timed out waiting for XWM state");
}

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

/// Wire events up to and including `wl_callback.done` of `callback_id`, as
/// (sender, opcode, payload). The caller has already sent
/// `wl_display.sync(callback_id)`, dispatched it and flushed the clients, so
/// the marker is on the socket and a read never waits on the server.
fn read_frames_until_callback(peer: &mut UnixStream, callback_id: u32) -> Vec<(u32, u16, Vec<u8>)> {
    peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .expect("set wire read timeout");
    let mut bytes = Vec::new();
    let mut frames = Vec::new();
    let mut offset = 0;
    loop {
        let mut chunk = [0u8; 8192];
        let read = peer.read(&mut chunk).expect("read Wayland events");
        assert!(read > 0, "Wayland server closed before the sync callback");
        bytes.extend_from_slice(&chunk[..read]);

        while bytes.len().saturating_sub(offset) >= 8 {
            let sender = read_u32(&bytes, offset);
            let header = read_u32(&bytes, offset + 4);
            let size = (header >> 16) as usize;
            let opcode = header as u16;
            assert!(size >= 8, "malformed Wayland event header");
            if bytes.len() - offset < size {
                break;
            }
            frames.push((sender, opcode, bytes[offset + 8..offset + size].to_vec()));
            offset += size;
            if sender == callback_id && opcode == 0 {
                return frames;
            }
        }
    }
}

/// Decoded `xdg_toplevel.configure` events (opcode 0) sent to `toplevel_id`:
/// width, height and the states array.
fn toplevel_configures(
    frames: &[(u32, u16, Vec<u8>)],
    toplevel_id: u32,
) -> Vec<(i32, i32, Vec<u32>)> {
    frames
        .iter()
        .filter(|(sender, opcode, _)| *sender == toplevel_id && *opcode == 0)
        .map(|(_, _, payload)| {
            let width = read_u32(payload, 0) as i32;
            let height = read_u32(payload, 4) as i32;
            let states_len = read_u32(payload, 8) as usize;
            let states = payload[12..12 + states_len]
                .chunks_exact(4)
                .map(|state| u32::from_ne_bytes(state.try_into().expect("wire state u32")))
                .collect();
            (width, height, states)
        })
        .collect()
}

/// `xdg_toplevel.state.maximized` on the wire.
const XDG_STATE_MAXIMIZED: u32 = 1;
/// Object id the xdg wire fixture gives its `xdg_toplevel`.
const WIRE_TOPLEVEL: u32 = 8;

/// A raw Wayland client with one xdg_toplevel (object 8) that has not yet
/// received its initial configure. Fields drop in the order the inline
/// fixture of `xdg_toplevel_wire_requests_reach_shared_window_policy` drops
/// its locals: the client first, the event loop last.
struct XdgWireFixture {
    peer: UnixStream,
    state: JwmWaylandState,
    pending_events: Arc<Mutex<VecDeque<BackendEvent>>>,
    display: Display<JwmWaylandState>,
    // Keeps the per-toplevel initial-configure timer armed but never
    // dispatched, so no fallback configure lands in a count.
    _event_loop: EventLoop<'static, JwmWaylandState>,
    window: WindowId,
    next_callback: u32,
}

impl XdgWireFixture {
    fn new(seat_name: &str) -> Self {
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
            seat_name.to_owned(),
            false,
            false,
        )
        .expect("initialize wire-test Wayland state");
        assert!(socket_name.is_none());

        let (server, mut peer) = UnixStream::pair().expect("create Wayland socket pair");
        display_handle
            .insert_client(server, Arc::new(JwmClientState::default()))
            .expect("insert raw Wayland client");

        // wl_display.get_registry(new_id=2), then sync(new_id=3) as the
        // discovery end marker.
        let mut discovery = wire_message(1, 1, &wire_u32(2));
        discovery.extend_from_slice(&wire_message(1, 0, &wire_u32(3)));
        peer.write_all(&discovery).expect("send registry request");
        display
            .dispatch_clients(&mut state)
            .expect("dispatch registry request");
        display.flush_clients().expect("flush registry events");
        let globals = discover_globals(&mut peer);
        let global = |wanted: &str| {
            globals
                .iter()
                .find_map(|(name, interface, version)| {
                    (interface == wanted).then_some((*name, *version))
                })
                .unwrap_or_else(|| panic!("{wanted} global"))
        };
        let (compositor_name, compositor_version) = global("wl_compositor");
        let (xdg_name, xdg_version) = global("xdg_wm_base");

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
        requests.extend_from_slice(&wire_message(7, 1, &wire_u32(WIRE_TOPLEVEL)));
        peer.write_all(&requests)
            .expect("send xdg toplevel requests");
        display
            .dispatch_clients(&mut state)
            .expect("dispatch xdg toplevel requests");

        let window = pending_events
            .lock()
            .expect("pending event lock")
            .drain(..)
            .find_map(|event| match event {
                BackendEvent::WindowCreated(window) => Some(window),
                _ => None,
            })
            .expect("xdg_toplevel creation reaches the backend");

        Self {
            peer,
            state,
            pending_events,
            display,
            _event_loop: event_loop,
            window,
            next_callback: 9,
        }
    }

    /// Send raw xdg_toplevel requests and dispatch them.
    fn send_toplevel_requests(&mut self, opcodes: &[u16]) {
        let mut requests = Vec::new();
        for opcode in opcodes {
            requests.extend_from_slice(&wire_message(WIRE_TOPLEVEL, *opcode, &[]));
        }
        self.peer
            .write_all(&requests)
            .expect("send xdg toplevel requests");
        self.display
            .dispatch_clients(&mut self.state)
            .expect("dispatch xdg toplevel requests");
    }

    /// Every `xdg_toplevel.configure` the client received since the last
    /// call, delimited by a fresh `wl_display.sync` round trip.
    fn configures_since_last_sync(&mut self) -> Vec<(i32, i32, Vec<u32>)> {
        let callback = self.next_callback;
        self.next_callback += 1;
        self.peer
            .write_all(&wire_message(1, 0, &wire_u32(callback)))
            .expect("send wl_display.sync");
        self.display
            .dispatch_clients(&mut self.state)
            .expect("dispatch wl_display.sync");
        self.display.flush_clients().expect("flush Wayland events");
        let frames = read_frames_until_callback(&mut self.peer, callback);
        toplevel_configures(&frames, WIRE_TOPLEVEL)
    }

    fn stage_size(&self, width: i32, height: i32) {
        self.state
            .toplevels
            .get(&self.window)
            .expect("wire toplevel stays live")
            .with_pending_state(|pending| pending.size = Some((width, height).into()));
    }
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

fn toplevel_pending_is_maximized(state: &JwmWaylandState, window: WindowId) -> bool {
    state
        .toplevels
        .get(&window)
        .expect("wire toplevel stays live")
        .with_pending_state(|pending| pending.states.contains(xdg_toplevel::State::Maximized))
}

#[test]
fn xdg_maximize_wire_requests_enter_shared_policy_and_owe_exactly_one_reply() {
    let mut fixture = XdgWireFixture::new("maximize-wire-seat");
    let window = fixture.window;

    // xdg_toplevel.set_maximized (9) and unset_maximized (10).
    fixture.send_toplevel_requests(&[9, 10]);

    let events = fixture
        .pending_events
        .lock()
        .expect("pending event lock")
        .drain(..)
        .collect::<Vec<_>>();
    let maximize_requests = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::WindowMaximizeRequest {
                window: event_window,
                action,
                axes,
            } if *event_window == window => Some((*action, *axes)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        maximize_requests,
        [
            (NetWmAction::Add, MaximizeAxes::BOTH),
            (NetWmAction::Remove, MaximizeAxes::BOTH),
        ]
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            BackendEvent::WindowStateRequest {
                state: NetWmState::MaximizedVert | NetWmState::MaximizedHorz,
                ..
            }
        )),
        "xdg maximize must not arrive as per-axis state requests"
    );
    assert!(
        !fixture.state.window_maximized.contains_key(&window),
        "protocol callbacks must leave confirmation to shared policy"
    );
    assert!(!toplevel_pending_is_maximized(&fixture.state, window));
    assert!(fixture.state.xdg_state_reply_owed.contains(&window));

    // Policy dropped both requests: the post-drain backstop pays the reply
    // exactly once.
    assert!(fixture.state.flush_owed_xdg_state_replies());
    assert!(!fixture.state.flush_owed_xdg_state_replies());
    let configures = fixture.configures_since_last_sync();
    assert_eq!(
        configures.len(),
        1,
        "one reply for the owed requests: {configures:?}"
    );
    assert!(!configures[0].2.contains(&XDG_STATE_MAXIMIZED));
}

#[test]
fn xdg_maximized_state_rides_the_policy_configure_and_refusals_still_reply() {
    let mut fixture = XdgWireFixture::new("maximize-reply-seat");
    let window = fixture.window;

    // 1. Manage: the initial configure.
    fixture.stage_size(800, 600);
    assert!(fixture.state.send_toplevel_configure(window, false));
    let configures = fixture.configures_since_last_sync();
    assert_eq!(configures.len(), 1, "{configures:?}");
    assert_eq!((configures[0].0, configures[0].1), (800, 600));
    assert!(!configures[0].2.contains(&XDG_STATE_MAXIMIZED));

    // 2. set_maximized is owed, not answered by the callback.
    fixture.send_toplevel_requests(&[9]);
    assert!(fixture.state.xdg_state_reply_owed.contains(&window));
    assert!(
        fixture.configures_since_last_sync().is_empty(),
        "the callback must not announce state before policy decides"
    );

    // 3. Policy accepts: state and size ride one configure.
    fixture
        .state
        .set_window_maximized(window, MaximizeAxes::BOTH)
        .expect("publish accepted maximize");
    fixture.stage_size(1280, 690);
    assert!(fixture.state.send_toplevel_configure(window, false));
    let configures = fixture.configures_since_last_sync();
    assert_eq!(configures.len(), 1, "{configures:?}");
    assert_eq!((configures[0].0, configures[0].1), (1280, 690));
    assert!(configures[0].2.contains(&XDG_STATE_MAXIMIZED));
    assert!(fixture.state.xdg_state_reply_owed.is_empty());
    assert_eq!(
        fixture.state.window_maximized.get(&window),
        Some(&MaximizeAxes::BOTH)
    );

    // 4. unset_maximized accepted: one configure with the restore size.
    fixture.send_toplevel_requests(&[10]);
    fixture
        .state
        .set_window_maximized(window, MaximizeAxes::NONE)
        .expect("publish accepted unmaximize");
    fixture.stage_size(800, 600);
    assert!(fixture.state.send_toplevel_configure(window, false));
    let configures = fixture.configures_since_last_sync();
    assert_eq!(configures.len(), 1, "{configures:?}");
    assert_eq!((configures[0].0, configures[0].1), (800, 600));
    assert!(!configures[0].2.contains(&XDG_STATE_MAXIMIZED));
    assert!(!fixture.state.window_maximized.contains_key(&window));

    // 5. set_maximized refused: the repair republish stages nothing new, yet
    //    the owed reply still repeats the current state once.
    fixture.send_toplevel_requests(&[9]);
    fixture
        .state
        .set_window_maximized(window, MaximizeAxes::NONE)
        .expect("republish current state");
    assert!(fixture.state.send_toplevel_configure(window, false));
    let configures = fixture.configures_since_last_sync();
    assert_eq!(configures.len(), 1, "{configures:?}");
    assert_eq!((configures[0].0, configures[0].1), (800, 600));
    assert!(!configures[0].2.contains(&XDG_STATE_MAXIMIZED));
    assert!(fixture.state.xdg_state_reply_owed.is_empty());

    // 6. Nothing owed and nothing staged: udev sends nothing.
    assert!(!fixture.state.send_toplevel_configure(window, false));
    assert!(fixture.configures_since_last_sync().is_empty());
}

#[test]
fn xwm_above_below_requests_write_real_properties_and_raise_real_windows() {
    let xvfb = crate::backend::clipboard_offer::IsolatedXvfb::acquire();
    let display_number = xvfb.name().trim_start_matches(':');
    let xwm_socket = UnixStream::connect(format!("/tmp/.X11-unix/X{display_number}"))
        .expect("connect XWM to isolated Xvfb");

    let mut event_loop: EventLoop<'static, JwmWaylandState> =
        EventLoop::try_new().expect("create XWM test event loop");
    let display = Display::<JwmWaylandState>::new().expect("create XWM test Wayland display");
    let mut display_handle = display.handle();
    let pending_events = Arc::new(Mutex::new(VecDeque::new()));
    let (flush_tx, _flush_rx) = channel::channel();
    let (mut state, socket_name) = JwmWaylandState::init(
        &display_handle,
        event_loop.handle(),
        pending_events.clone(),
        flush_tx,
        Arc::new(AtomicBool::new(false)),
        "xwm-test-seat".to_owned(),
        false,
        false,
    )
    .expect("initialize XWM test state");
    assert!(socket_name.is_none());

    let (wl_server, _wl_peer) = UnixStream::pair().expect("create dummy Wayland client");
    let xwayland_client = display_handle
        .insert_client(wl_server, Arc::new(JwmClientState::default()))
        .expect("insert dummy XWayland client");
    state.x11_wm = Some(
        X11Wm::start_wm(
            event_loop.handle(),
            &display_handle,
            xwm_socket,
            xwayland_client,
        )
        .expect("start XWM on isolated Xvfb"),
    );

    let (conn, screen_num) =
        smithay::reexports::x11rb::connect(Some(xvfb.name())).expect("connect X11 test client");
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let net_wm_state = intern(&conn, b"_NET_WM_STATE");
    let above = intern(&conn, b"_NET_WM_STATE_ABOVE");
    let below = intern(&conn, b"_NET_WM_STATE_BELOW");

    let first = conn.generate_id().expect("allocate first window");
    let second = conn.generate_id().expect("allocate second window");
    for (window, x) in [(first, 10), (second, 80)] {
        conn.create_window(
            screen.root_depth,
            window,
            root,
            x,
            10,
            60,
            40,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .expect("create X11 test window");
        conn.map_window(window).expect("map X11 test window");
    }
    conn.flush().expect("flush X11 window creation");
    pump_xwm(&mut event_loop, &mut state, |state| {
        state.x11_surface_to_window.contains_key(&first)
            && state.x11_surface_to_window.contains_key(&second)
    });
    let first_win = state.x11_surface_to_window[&first];
    let second_win = state.x11_surface_to_window[&second];
    pending_events.lock().expect("pending event lock").clear();

    for (action, atom, expected_action, expected_state) in [
        (1, above, NetWmAction::Add, NetWmState::Above),
        (0, above, NetWmAction::Remove, NetWmState::Above),
        (1, below, NetWmAction::Add, NetWmState::Below),
        (0, below, NetWmAction::Remove, NetWmState::Below),
    ] {
        let event = ClientMessageEvent::new(
            32,
            first,
            net_wm_state,
            ClientMessageData::from([action, atom, 0, 1, 0]),
        );
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .expect("send _NET_WM_STATE request");
        conn.flush().expect("flush _NET_WM_STATE request");
        pump_xwm(&mut event_loop, &mut state, |_| {
            !pending_events
                .lock()
                .expect("pending event lock")
                .is_empty()
        });
        let actual = pending_events
            .lock()
            .expect("pending event lock")
            .pop_front()
            .expect("state request event");
        assert!(matches!(
            actual,
            BackendEvent::WindowStateRequest { window, action, state: flag }
                if window == first_win && action == expected_action && flag == expected_state
        ));
    }

    for (flag, atom) in [(NetWmState::Above, above), (NetWmState::Below, below)] {
        state
            .set_x11_net_state(first_win, flag, true)
            .expect("write X11 state property");
        conn.flush().expect("flush X11 property read connection");
        assert!(state.has_x11_net_state(first_win, flag));
        let atoms = conn
            .get_property(false, first, net_wm_state, AtomEnum::ATOM, 0, u32::MAX)
            .expect("query _NET_WM_STATE")
            .reply()
            .expect("read _NET_WM_STATE")
            .value32()
            .expect("32-bit state atoms")
            .collect::<Vec<_>>();
        assert!(atoms.contains(&atom));

        state
            .set_x11_net_state(first_win, flag, false)
            .expect("remove X11 state property");
        assert!(!state.has_x11_net_state(first_win, flag));
    }

    let first_frame = state.x11_surfaces[&first_win]
        .mapped_window_id()
        .unwrap_or(first);
    let second_frame = state.x11_surfaces[&second_win]
        .mapped_window_id()
        .unwrap_or(second);
    state
        .raise_window(first_win)
        .expect("raise first X11 window");
    conn.flush().expect("flush before querying X11 stack");
    let children = conn
        .query_tree(root)
        .expect("query root tree")
        .reply()
        .expect("read root tree")
        .children;
    let first_pos = children
        .iter()
        .position(|window| *window == first_frame)
        .expect("first frame in root tree");
    let second_pos = children
        .iter()
        .position(|window| *window == second_frame)
        .expect("second frame in root tree");
    assert!(
        first_pos > second_pos,
        "raised window must be above its peer"
    );
    assert_eq!(state.window_stack.last(), Some(&first_win));

    // Maximize round trip: the paired atoms become one shared request, the
    // accepted state is written back as real atoms, and a maximized window's
    // own resize request is answered with JWM's geometry.
    let maximized_horz = intern(&conn, b"_NET_WM_STATE_MAXIMIZED_HORZ");
    let maximized_vert = intern(&conn, b"_NET_WM_STATE_MAXIMIZED_VERT");
    let send_maximize_message = |action: u32| {
        let event = ClientMessageEvent::new(
            32,
            first,
            net_wm_state,
            ClientMessageData::from([action, maximized_horz, maximized_vert, 1, 0]),
        );
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .expect("send paired maximize request");
        conn.flush().expect("flush paired maximize request");
    };
    pending_events.lock().expect("pending event lock").clear();
    send_maximize_message(1);
    pump_xwm(&mut event_loop, &mut state, |_| {
        !pending_events
            .lock()
            .expect("pending event lock")
            .is_empty()
    });
    let actual = pending_events
        .lock()
        .expect("pending event lock")
        .pop_front()
        .expect("maximize request event");
    assert!(matches!(
        actual,
        BackendEvent::WindowMaximizeRequest { window, action: NetWmAction::Add, axes }
            if window == first_win && axes == MaximizeAxes::BOTH
    ));

    state
        .set_window_maximized(first_win, MaximizeAxes::BOTH)
        .expect("publish XWayland maximize");
    conn.flush().expect("flush X11 property read connection");
    assert!(state.x11_surfaces[&first_win].is_maximized());
    let atoms = conn
        .get_property(false, first, net_wm_state, AtomEnum::ATOM, 0, u32::MAX)
        .expect("query _NET_WM_STATE")
        .reply()
        .expect("read _NET_WM_STATE")
        .value32()
        .expect("32-bit state atoms")
        .collect::<Vec<_>>();
    assert!(atoms.contains(&maximized_horz) && atoms.contains(&maximized_vert));

    // `Geometry` has no `PartialEq`; compare its fields.
    let geometry_of = |state: &JwmWaylandState| {
        let g = state.window_geometry[&first_win];
        (g.x, g.y, g.w, g.h, g.border)
    };
    let geometry_before = geometry_of(&state);
    conn.configure_window(first, &ConfigureWindowAux::new().width(200).height(150))
        .expect("send client resize request");
    // The unmaximize message doubles as an ordering marker: the XWM sees the
    // ConfigureRequest before it.
    send_maximize_message(0);
    pump_xwm(&mut event_loop, &mut state, |_| {
        !pending_events
            .lock()
            .expect("pending event lock")
            .is_empty()
    });
    let events = pending_events
        .lock()
        .expect("pending event lock")
        .drain(..)
        .collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BackendEvent::WindowConfigured { .. })),
        "a maximized window's resize request must not reach policy: {events:?}"
    );
    assert_eq!(geometry_of(&state), geometry_before);
    assert!(matches!(
        events.as_slice(),
        [BackendEvent::WindowMaximizeRequest { window, action: NetWmAction::Remove, axes }]
            if *window == first_win && *axes == MaximizeAxes::BOTH
    ));
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
        state.window_maximized.insert(win, MaximizeAxes::BOTH);
        state.xdg_state_reply_owed.insert(win);
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
    assert!(!state.window_maximized.contains_key(&removed));
    assert!(!state.xdg_state_reply_owed.contains(&removed));
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
    assert!(state.window_maximized.contains_key(&surviving));
    assert!(state.xdg_state_reply_owed.contains(&surviving));
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

#[test]
fn x11_maximize_callbacks_reuse_the_shared_maximize_request() {
    let (mut state, pending_events) = headless_state();
    let win = WindowId::from_raw(71);
    state.x11_surface_to_window.insert(0x1234, win);

    state.request_x11_maximize(0x1234, true);
    state.request_x11_maximize(0x1234, false);
    state.request_x11_maximize(0x9999, true);

    let events = pending_events
        .lock()
        .expect("pending event lock")
        .drain(..)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2, "an unknown X11 id must not emit an event");
    assert!(matches!(
        events[0],
        BackendEvent::WindowMaximizeRequest { window, action: NetWmAction::Add, axes }
            if window == win && axes == MaximizeAxes::BOTH
    ));
    assert!(matches!(
        events[1],
        BackendEvent::WindowMaximizeRequest { window, action: NetWmAction::Remove, axes }
            if window == win && axes == MaximizeAxes::BOTH
    ));
    assert!(
        state.xdg_state_reply_owed.is_empty(),
        "XWayland has no xdg configure to owe"
    );

    state
        .set_window_maximized(win, MaximizeAxes::BOTH)
        .expect("publishing without surfaces only updates the cache");
    assert_eq!(state.window_maximized.get(&win), Some(&MaximizeAxes::BOTH));
    assert!(state.has_window_net_state(win, NetWmState::MaximizedVert));
    assert!(state.has_window_net_state(win, NetWmState::MaximizedHorz));
    assert!(!state.has_x11_net_state(win, NetWmState::MaximizedVert));
}

#[test]
fn per_axis_net_state_writes_merge_through_the_maximize_cache() {
    let (mut state, _pending_events) = headless_state();
    let win = WindowId::from_raw(81);

    for (flag, on, expected) in [
        (NetWmState::MaximizedVert, true, Some(MaximizeAxes::VERT)),
        (NetWmState::MaximizedHorz, true, Some(MaximizeAxes::BOTH)),
        (NetWmState::MaximizedVert, false, Some(MaximizeAxes::HORZ)),
        (NetWmState::MaximizedHorz, false, None),
    ] {
        state
            .set_window_net_state(win, flag, on)
            .expect("per-axis maximize write");
        assert_eq!(
            state.window_maximized.get(&win).copied(),
            expected,
            "after {flag:?} = {on}"
        );
        assert_eq!(state.has_window_net_state(win, flag), on);
    }

    state.window_maximized.insert(win, MaximizeAxes::VERT);
    state
        .set_window_net_state(win, NetWmState::Above, true)
        .expect("non-maximize write");
    assert_eq!(
        state.window_maximized.get(&win),
        Some(&MaximizeAxes::VERT),
        "other atoms must leave the maximize cache alone"
    );
}

#[test]
fn every_wayland_run_loop_flushes_owed_xdg_state_replies() {
    let flush_call = format!("{}(", "flush_owed_xdg_state_replies");
    let drain_call = format!("{}(self, ev)?", "handler.handle_event");
    let configure_send = format!("{}(", "send_toplevel_configure");
    for (name, source) in [
        ("wayland_udev", include_str!("backend.rs")),
        ("wayland_x11", include_str!("../wayland_x11/backend.rs")),
        ("wayland_winit", include_str!("../wayland_winit/backend.rs")),
    ] {
        let run = source
            .split_once("fn run(&mut self, handler: &mut dyn EventHandler)")
            .unwrap_or_else(|| panic!("{name}: Backend::run"))
            .1;
        let flush_at = run
            .find(&flush_call)
            .unwrap_or_else(|| panic!("{name}: the run loop must pay owed xdg replies"));
        let drain_at = run
            .find(&drain_call)
            .unwrap_or_else(|| panic!("{name}: the run loop drains pending events"));
        assert!(
            drain_at < flush_at,
            "{name}: owed replies are paid only after policy saw the queued requests"
        );

        let window_ops = source
            .split_once("impl WindowOps for WaylandWindowOps")
            .unwrap_or_else(|| panic!("{name}: WindowOps impl"))
            .1;
        let configure = window_ops
            .split_once("fn configure(")
            .unwrap_or_else(|| panic!("{name}: WindowOps::configure"))
            .1;
        let configure_body = configure
            .split_once("\n    fn ")
            .map_or(configure, |(body, _)| body);
        assert!(
            configure_body.contains(&configure_send),
            "{name}: WindowOps::configure must send through send_toplevel_configure"
        );
    }
}

/// The interfaces a fresh client is advertised by a state initialized the
/// way a backend with (`true`) or without (`false`) the DRM/KMS output
/// pipeline initializes it.
fn advertised_interfaces(drm_output_pipeline: bool) -> Vec<String> {
    let event_loop: EventLoop<'static, JwmWaylandState> =
        EventLoop::try_new().expect("create test event loop");
    let mut display = Display::<JwmWaylandState>::new().expect("create test display");
    let (flush_tx, _flush_rx) = channel::channel();
    let (mut state, _) = JwmWaylandState::init(
        &display.handle(),
        event_loop.handle(),
        Arc::new(Mutex::new(VecDeque::new())),
        flush_tx,
        Arc::new(AtomicBool::new(false)),
        "globals-test-seat".to_owned(),
        false,
        drm_output_pipeline,
    )
    .expect("initialize headless Wayland state");
    let (server, mut peer) = UnixStream::pair().expect("create Wayland socket pair");
    display
        .handle()
        .insert_client(server, Arc::new(JwmClientState::default()))
        .expect("insert raw Wayland client");
    // wl_display.get_registry(new_id=2), then sync(new_id=3) as the end marker.
    let mut discovery = wire_message(1, 1, &wire_u32(2));
    discovery.extend_from_slice(&wire_message(1, 0, &wire_u32(3)));
    peer.write_all(&discovery).expect("send registry request");
    display
        .dispatch_clients(&mut state)
        .expect("dispatch registry request");
    display.flush_clients().expect("flush registry events");
    discover_globals(&mut peer)
        .into_iter()
        .map(|(_, interface, _)| interface)
        .collect()
}

#[test]
fn output_management_is_advertised_only_where_output_configure_is_serviced() {
    // A nested backend never pops the ack an Apply queues: wlr-randr and
    // kanshi blocked forever there.
    assert!(
        !advertised_interfaces(false)
            .iter()
            .any(|interface| interface == "zwlr_output_manager_v1"),
        "nested backends must not advertise wlr-output-management"
    );
    if crate::config::CONFIG
        .load()
        .behavior()
        .wayland_enable_output_management
    {
        assert!(
            advertised_interfaces(true)
                .iter()
                .any(|interface| interface == "zwlr_output_manager_v1"),
            "the DRM/KMS backend keeps wlr-output-management"
        );
    }
}

/// `source` with everything from its first `#[cfg(test)]` on removed, so a
/// pin cannot be satisfied by a test's own needle.
fn production(source: &str) -> &str {
    source
        .split_once("#[cfg(test)]")
        .map_or(source, |(code, _)| code)
}

#[test]
fn every_wayland_run_loop_syncs_foreign_toplevel_outputs() {
    let sync_call = format!("{}()", "sync_foreign_toplevel_outputs");
    for (name, source) in [
        ("wayland_udev", include_str!("backend.rs")),
        ("wayland_x11", include_str!("../wayland_x11/backend.rs")),
        ("wayland_winit", include_str!("../wayland_winit/backend.rs")),
    ] {
        let run = production(source)
            .split_once("fn run(&mut self, handler: &mut dyn EventHandler)")
            .unwrap_or_else(|| panic!("{name}: Backend::run"))
            .1;
        assert!(
            run.contains(&sync_call),
            "{name}: taskbar handles must follow windows to their outputs"
        );
    }
}

#[test]
fn every_wayland_backend_confirms_session_locks_from_presented_frames() {
    let note = format!("{}(", "note_locked_frame_presented");
    // The DRM path confirms from the page flip of a locked frame.
    let kms = production(include_str!("../udev_kms.rs"));
    let notifier = kms
        .split_once("DrmEvent::VBlank(crtc) =>")
        .expect("DRM notifier callback")
        .1;
    let notifier = notifier
        .split_once("DrmEvent::Error")
        .map_or(notifier, |(arm, _)| arm);
    assert!(
        notifier.contains("take_presented_locked_frames()") && notifier.contains(&note),
        "the vblank handler must report presented locked frames"
    );
    assert!(
        kms.contains("frame_pending_lock_epoch.take()")
            && kms.contains("state.session_locked.then_some(state.session_lock_epoch)"),
        "a queued frame must remember whether it was rendered locked"
    );
    // The nested paths confirm once the host took the frame.
    for (name, source, submit) in [
        (
            "wayland_x11",
            include_str!("../wayland_x11/backend.rs"),
            "self.x11_surface.submit()",
        ),
        (
            "wayland_winit",
            include_str!("../wayland_winit/backend.rs"),
            "self.winit_backend.submit(",
        ),
    ] {
        let render = production(source)
            .split_once("fn render_if_needed(&mut self)")
            .unwrap_or_else(|| panic!("{name}: render_if_needed"))
            .1;
        let render = render
            .split_once("\n    pub fn new(")
            .map_or(render, |(body, _)| body);
        let submit_at = render
            .find(submit)
            .unwrap_or_else(|| panic!("{name}: the frame is submitted"));
        let note_at = render
            .find(&note)
            .unwrap_or_else(|| panic!("{name}: a presented locked frame confirms the lock"));
        assert!(
            submit_at < note_at,
            "{name}: only a submitted frame can confirm the lock"
        );
        assert!(
            render.contains("self.lock_shield_id.clone()"),
            "{name}: a locked frame draws the opaque shield"
        );
    }
}

#[test]
fn managed_xwayland_windows_reach_foreign_toplevel_managers() {
    let source = production(include_str!("state.rs"));
    let xwm = source
        .split_once("impl XwmHandler for JwmWaylandState")
        .expect("XwmHandler impl")
        .1;
    let body = |name: &str| {
        let start = xwm
            .split_once(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("XwmHandler::{name}"))
            .1;
        start
            .split_once("\n    fn ")
            .map_or(start, |(body, _)| body)
    };
    assert!(
        body("map_window_request").contains("announce_new_toplevel("),
        "a managed X11 window must be announced to taskbars"
    );
    assert!(
        !body("mapped_override_redirect_window").contains("announce_new_toplevel("),
        "override-redirect menus and tooltips are not toplevels"
    );
    for name in ["unmapped_window", "destroyed_window"] {
        assert!(
            body(name).contains("ftm.remove_window(win_id)"),
            "{name} must send `closed` for the X11 window"
        );
    }
    let property = body("property_notify");
    assert!(property.contains("ftm.update_title(") && property.contains("ftm.update_app_id("));
}

#[test]
fn every_wayland_backend_publishes_monitors_to_workspace_managers() {
    let sync_call = format!("{}(monitors)", "self.state.sync_workspace_monitors");
    for (name, source) in [
        ("wayland_udev", include_str!("backend.rs")),
        ("wayland_x11", include_str!("../wayland_x11/backend.rs")),
        ("wayland_winit", include_str!("../wayland_winit/backend.rs")),
    ] {
        let set_monitors = production(source)
            .split_once("fn compositor_set_monitors(")
            .unwrap_or_else(|| panic!("{name}: compositor_set_monitors"))
            .1;
        let body = set_monitors
            .split_once("\n    fn ")
            .map_or(set_monitors, |(body, _)| body);
        assert!(
            body.contains(&sync_call),
            "{name}: taskbars must follow monitor and tag changes"
        );
    }
}
