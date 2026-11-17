//! Owned real Wayland keyboard protocol tests. No ambient display or X connection.
//! The compositor emits actual wl_keyboard events; these tests never mutate
//! viewer keyboard state or substitute buffer release for presentation.
use super::*;
use smithay::reexports::wayland_server::protocol::wl_keyboard::{KeyState, WlKeyboard};

#[derive(Debug)]
enum KeyboardReply {
    Connected,
    Enabled([u8; 32]),
    Events(Vec<ViewerEvent>),
    Checked,
    Failed(String),
    Stopped,
}
enum KeyboardCommand {
    Enable,
    Check,
    Poll,
    Stop,
}
struct KeyboardPeer {
    commands: mpsc::Sender<KeyboardCommand>,
    replies: mpsc::Receiver<KeyboardReply>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for KeyboardPeer {
    fn drop(&mut self) {
        let _ = self.commands.send(KeyboardCommand::Stop);
    }
}
impl KeyboardPeer {
    fn start(stream: UnixStream) -> Self {
        let (commands, input) = mpsc::channel();
        let (output, replies) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> crate::remote::RemoteResult<()> {
                    let mut viewer = WaylandViewer::connect_test_stream(
                        stream,
                        800,
                        600,
                        None,
                        "owned keyboard viewer",
                    )?;
                    output.send(KeyboardReply::Connected)?;
                    loop {
                        match input.recv_timeout(Duration::from_secs(12))? {
                            KeyboardCommand::Enable => {
                                output.send(KeyboardReply::Enabled(
                                    viewer.enable_wayland_keyboard(None)?,
                                ))?;
                            }
                            KeyboardCommand::Check => {
                                viewer.check_wayland_keyboard(true)?;
                                output.send(KeyboardReply::Checked)?;
                            }
                            KeyboardCommand::Poll => {
                                output.send(KeyboardReply::Events(viewer.poll_events()?))?;
                            }
                            KeyboardCommand::Stop => {
                                output.send(KeyboardReply::Stopped)?;
                                return Ok(());
                            }
                        }
                    }
                },
            ));
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some("keyboard viewer worker panicked".into()),
            };
            if let Some(error) = failure {
                let _ = output.send(KeyboardReply::Failed(error));
            }
        });
        Self {
            commands,
            replies,
            worker: Some(worker),
        }
    }
    fn receive(&self, server: &mut Server, phase: &str) -> KeyboardReply {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            server.roundtrip();
            for top in server.state.toplevels.values() {
                if !top.is_initial_configure_sent() {
                    top.with_pending_state(|state| state.size = Some((800, 600).into()));
                    top.send_configure();
                }
            }
            server.roundtrip();
            match self.replies.try_recv() {
                Ok(reply) => return reply,
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("{phase}: keyboard worker disconnected")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < until,
                "{phase}: keyboard wire phase timed out"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn stop(mut self, server: &mut Server) {
        self.commands.send(KeyboardCommand::Stop).unwrap();
        assert!(matches!(
            self.receive(server, "stop"),
            KeyboardReply::Stopped
        ));
        self.worker.take().unwrap().join().unwrap();
    }
    fn events(&self, server: &mut Server) -> Vec<ViewerEvent> {
        // Queue the real protocol stimulus before waking the nonblocking
        // viewer poll. A server flush is not a client dispatch barrier: callers
        // expecting a terminal event must still await that explicit evidence.
        server.roundtrip();
        self.commands.send(KeyboardCommand::Poll).unwrap();
        match self.receive(server, "poll") {
            KeyboardReply::Events(events) => events,
            other => panic!("expected events: {other:?}"),
        }
    }
    fn revoked_events(&self, server: &mut Server, phase: &str) -> Vec<ViewerEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = self.events(server);
            // Even an earlier partial batch must never leak a key. In
            // particular SCM_RIGHTS delivery need not finish in one poll.
            assert_no_keys(&events);
            if events.contains(&ViewerEvent::ReleaseAll) {
                return events;
            }
            assert!(
                Instant::now() < deadline,
                "{phase}: no explicit ReleaseAll before deadline; last events: {events:?}"
            );
        }
    }
}
fn keyboard_fixture() -> (Server, KeyboardPeer) {
    keyboard_fixture_setup(|_| {})
}
fn keyboard_fixture_setup(setup: impl FnOnce(&mut Server)) -> (Server, KeyboardPeer) {
    let mut server = Server::new();
    register_test_presentation(&mut server);
    setup(&mut server);
    let (native, stream) = UnixStream::pair().unwrap();
    server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let peer = KeyboardPeer::start(stream);
    assert!(matches!(
        peer.receive(&mut server, "connect"),
        KeyboardReply::Connected
    ));
    (server, peer)
}
fn enable(server: &mut Server, peer: &KeyboardPeer) -> WlKeyboard {
    peer.commands.send(KeyboardCommand::Enable).unwrap();
    let digest = match peer.receive(server, "enable") {
        KeyboardReply::Enabled(digest) => digest,
        other => panic!("enable: {other:?}"),
    };
    assert_ne!(digest, [0; 32]);
    let surface = viewer_surface(server);
    let keyboard = server.state.seat.get_keyboard().unwrap();
    let resources: Vec<_> = keyboard
        .client_keyboards(&surface.client().unwrap())
        .collect();
    assert_eq!(resources.len(), 1);
    resources[0].clone()
}
fn enter_clean(server: &mut Server, peer: &KeyboardPeer, keyboard: &WlKeyboard) {
    let surface = viewer_surface(server);
    keyboard.enter(1, &surface, Vec::new());
    keyboard.modifiers(2, 0, 0, 0, 0);
    peer.commands.send(KeyboardCommand::Check).unwrap();
    assert!(matches!(
        peer.receive(server, "clean focus"),
        KeyboardReply::Checked
    ));
}
fn assert_no_keys(events: &[ViewerEvent]) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ViewerEvent::Key { .. })),
        "unexpected forwarded key: {events:?}"
    );
}

#[test]
fn actual_wayland_viewer_keyboard_default_is_unbound_and_optin_uses_real_keymap_fd() {
    let (mut server, peer) = keyboard_fixture();
    let surface = viewer_surface(&server);
    assert!(
        server
            .state
            .seat
            .client_seats(&surface.client().unwrap())
            .is_empty()
    );
    let keyboard = enable(&mut server, &peer);
    assert_eq!(
        server
            .state
            .seat
            .client_seats(&surface.client().unwrap())
            .len(),
        1
    );
    enter_clean(&mut server, &peer, &keyboard);
    keyboard.key(3, 1, 30, KeyState::Pressed);
    keyboard.key(4, 2, 30, KeyState::Released);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut events = Vec::new();
    while events.len() < 2 {
        events.extend(peer.events(&mut server));
        assert!(Instant::now() < deadline, "balanced key delivery deadline");
    }
    let keys: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ViewerEvent::Key { keycode, pressed } => Some((*keycode, *pressed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [(38, true), (38, false)],
        "Wayland evdev must receive exactly one +8 adapter translation"
    );
    keyboard.repeat_info(200, 1);
    peer.revoked_events(&mut server, "repeat-info replacement");
    assert_no_keys(&peer.events(&mut server));
    peer.stop(&mut server);
}

#[test]
fn actual_wayland_viewer_keyboard_initial_held_and_modifiers_fail_closed() {
    for case in 0..4 {
        let (mut server, mut peer) = keyboard_fixture();
        let keyboard = enable(&mut server, &peer);
        let surface = viewer_surface(&server);
        keyboard.enter(
            1,
            &surface,
            if case == 0 {
                30_u32.to_ne_bytes().to_vec()
            } else {
                Vec::new()
            },
        );
        keyboard.modifiers(
            2,
            0,
            u32::from(case == 1),
            u32::from(case == 2),
            u32::from(case == 3),
        );
        peer.commands.send(KeyboardCommand::Check).unwrap();
        match peer.receive(&mut server, "dirty initial state") {
            KeyboardReply::Failed(error) => assert!(!error.contains("panicked"), "{error}"),
            other => panic!("case {case}: expected explicit rejection, got {other:?}"),
        }
        peer.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn actual_wayland_viewer_keyboard_leave_same_batch_key_and_restore_never_forward() {
    let (mut server, peer) = keyboard_fixture();
    let keyboard = enable(&mut server, &peer);
    enter_clean(&mut server, &peer, &keyboard);
    let surface = viewer_surface(&server);
    keyboard.key(3, 1, 30, KeyState::Pressed);
    keyboard.leave(4, &surface);
    keyboard.enter(5, &surface, Vec::new());
    keyboard.modifiers(6, 0, 0, 0, 0);
    let events = peer.revoked_events(&mut server, "leave and restore");
    assert_no_keys(&events);
    assert!(events.contains(&ViewerEvent::ReleaseAll));
    keyboard.key(7, 2, 31, KeyState::Pressed);
    assert_no_keys(&peer.events(&mut server));
    peer.stop(&mut server);
}

#[test]
fn actual_wayland_viewer_keyboard_capability_and_global_history_are_terminal() {
    use crate::backend::wayland::state::JwmWaylandState;
    use smithay::reexports::wayland_server::protocol::wl_seat::Capability;
    for case in 0..3 {
        let (mut server, peer) = keyboard_fixture();
        let keyboard = enable(&mut server, &peer);
        enter_clean(&mut server, &peer, &keyboard);
        let surface = viewer_surface(&server);
        match case {
            0 => {
                let seat = server
                    .state
                    .seat
                    .client_seats(&surface.client().unwrap())
                    .remove(0);
                seat.capabilities(Capability::empty());
                seat.capabilities(Capability::Keyboard);
            }
            1 => {
                let global = server.state.seat.global().unwrap();
                server
                    .display
                    .handle()
                    .remove_global::<JwmWaylandState>(global);
            }
            _ => {
                let _other = server
                    .state
                    .seat_state
                    .new_wl_seat(&server.display.handle(), "ambiguous-second-seat");
            }
        }
        keyboard.key(9, 9, 30, KeyState::Pressed);
        let events = peer.revoked_events(&mut server, &format!("terminal case {case}"));
        assert_no_keys(&events);
        assert!(
            events.contains(&ViewerEvent::ReleaseAll),
            "case={case}: {events:?}"
        );
        keyboard.key(10, 10, 30, KeyState::Released);
        assert_no_keys(&peer.events(&mut server));
        peer.stop(&mut server);
    }
}

#[test]
fn actual_wayland_viewer_keyboard_bad_edges_and_queue_overflow_clear_whole_batch() {
    for case in 0..5 {
        let (mut server, peer) = keyboard_fixture();
        let keyboard = enable(&mut server, &peer);
        enter_clean(&mut server, &peer, &keyboard);
        match case {
            0 => keyboard.key(3, 1, 30, KeyState::Released),
            1 => {
                keyboard.key(3, 1, 30, KeyState::Pressed);
                keyboard.key(4, 2, 30, KeyState::Pressed);
            }
            2 => keyboard.key(3, 1, u32::MAX, KeyState::Pressed),
            3 => {
                for code in 1..=33 {
                    keyboard.key(code, code, code, KeyState::Pressed);
                }
            }
            _ => {
                for sequence in 1..=129 {
                    keyboard.key(sequence * 2, sequence, 30, KeyState::Pressed);
                    keyboard.key(sequence * 2 + 1, sequence, 30, KeyState::Released);
                }
            }
        }
        let events = peer.revoked_events(&mut server, &format!("terminal case {case}"));
        assert_no_keys(&events);
        assert!(
            events.contains(&ViewerEvent::ReleaseAll),
            "case={case}: {events:?}"
        );
        peer.stop(&mut server);
    }
}

#[test]
fn actual_wayland_viewer_keyboard_map_replacement_fd_never_restores_authority() {
    use smithay::reexports::wayland_server::protocol::wl_keyboard::KeymapFormat;
    use std::os::fd::AsFd;
    for case in 0..3 {
        let (mut server, peer) = keyboard_fixture();
        let keyboard = enable(&mut server, &peer);
        enter_clean(&mut server, &peer, &keyboard);
        let fd = nix::sys::memfd::memfd_create(
            c"owned-keymap-negative",
            nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
        )
        .unwrap();
        let file = std::fs::File::from(fd);
        // Real SCM_RIGHTS transfer; malformed replacement cannot revive an
        // existing accepted map. Initial-map bounds have separate pure tests.
        let size = match case {
            0 => 0,
            1 => 1024 * 1024 + 1,
            _ => 32,
        };
        keyboard.keymap(KeymapFormat::XkbV1, file.as_fd(), size);
        keyboard.key(8, 8, 30, KeyState::Pressed);
        let events = peer.revoked_events(&mut server, &format!("terminal case {case}"));
        assert_no_keys(&events);
        assert!(events.contains(&ViewerEvent::ReleaseAll));
        keyboard.key(9, 9, 30, KeyState::Released);
        assert_no_keys(&peer.events(&mut server));
        peer.stop(&mut server);
    }
}

#[test]
fn actual_wayland_viewer_keyboard_requires_one_existing_seat() {
    use crate::backend::wayland::state::JwmWaylandState;
    for multiple in [false, true] {
        let (mut server, mut peer) = keyboard_fixture_setup(|server| {
            if multiple {
                let _other = server
                    .state
                    .seat_state
                    .new_wl_seat(&server.display.handle(), "second-initial-seat");
            } else {
                server
                    .display
                    .handle()
                    .remove_global::<JwmWaylandState>(server.state.seat.global().unwrap());
            }
        });
        peer.commands.send(KeyboardCommand::Enable).unwrap();
        match peer.receive(&mut server, "ambiguous/missing seat") {
            KeyboardReply::Failed(error) => assert!(!error.contains("panicked"), "{error}"),
            other => panic!("must reject unavailable seat topology: {other:?}"),
        }
        peer.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn actual_wayland_viewer_keyboard_balanced_input_before_activation_is_not_forgotten() {
    let (mut server, mut peer) = keyboard_fixture();
    let keyboard = enable(&mut server, &peer);
    let surface = viewer_surface(&server);
    keyboard.enter(1, &surface, Vec::new());
    keyboard.modifiers(2, 0, 0, 0, 0);
    keyboard.key(3, 1, 30, KeyState::Pressed);
    keyboard.key(4, 2, 30, KeyState::Released);
    peer.commands.send(KeyboardCommand::Check).unwrap();
    match peer.receive(&mut server, "balanced preactivation input") {
        KeyboardReply::Failed(error) => {
            assert!(error.contains("preceded lease activation"), "{error}")
        }
        other => {
            panic!("must reject preactivation input even though held set is now empty: {other:?}")
        }
    }
    peer.worker.take().unwrap().join().unwrap();
}
