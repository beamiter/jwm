//! Actual WaylandCapture pointer adapter -> native lease -> wl_pointer.
//! Production offscreen GL and private sockets, not TCP/viewer or DRM acceptance.
use super::wayland_capture::WaylandCapture;
use super::wayland_pointer_wire::{DrawnScope, PointerAction, PointerActionKind};
use super::x11_capture::CaptureOutcome;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{authority_tests::App, init_image_copy_capture, wire_test_client::Server},
    remote_keyboard::LOCAL,
    remote_pointer::revoke,
};
use smithay::{backend::input::InputTime, input::pointer::MotionEvent, utils::SERIAL_COUNTER};
use std::{
    os::unix::net::UnixStream,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
enum Command {
    Frame,
    Grant,
    Action(u64, PointerActionKind),
}
#[derive(Debug)]
enum Reply {
    Connected,
    Frame,
    Granted,
    Accepted,
    Failed(String),
}
fn sync(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn bind(server: &mut Server, app: &mut App) -> u32 {
    let seat = app.client.bind("wl_seat", 7);
    let pointer = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    sync(server);
    app.client.events();
    pointer
}
fn focused(server: &mut Server, app: &App) {
    let surface = server.state.surface_for_window(app.window).unwrap();
    server
        .state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    let pointer = server.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut server.state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: (5.0, 5.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut server.state);
    sync(server);
}
fn received(app: &mut App, pointer: u32) -> Vec<(u16, Vec<u32>)> {
    let ev = app.client.events();
    assert!(ev.iter().all(|e| !(e.sender == 1 && e.opcode == 0)));
    ev.into_iter()
        .filter(|e| e.sender == pointer)
        .map(|e| (e.opcode, e.args))
        .collect()
}
fn receive(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    cache: &mut Option<(i32, i32, smithay::backend::renderer::gles::GlesRenderbuffer)>,
    rx: &mpsc::Receiver<Reply>,
) -> Reply {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Disconnected) => panic!("pointer adapter worker disconnected"),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        assert!(
            Instant::now() < deadline,
            "pointer adapter protocol deadline"
        );
        sync(server);
        let queue = server.state.image_capture_pending.as_ref().unwrap();
        fulfill_test_toplevel_capture(&mut gl.renderer, cache, &server.state, queue);
        sync(server);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn failed(reply: Reply) {
    match reply {
        Reply::Failed(message) => {
            assert!(!message.is_empty());
            assert!(
                !message.contains("timed out"),
                "timeout is not proof of denial: {message}"
            );
        }
        other => panic!("unexpected pointer authorization: {other:?}"),
    }
}
#[test]
fn actual_pointer_adapter_ready_motion_click_and_permanent_revocation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for stale_frame in [false, true] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_color(&mut server, [0, 0, 255, 255]);
        let pointer = bind(&mut server, &mut app);
        focused(&mut server, &app);
        app.client.events();
        let (server_end, peer) = UnixStream::pair().unwrap();
        server
            .display
            .handle()
            .insert_client(server_end, Arc::new(JwmClientState::default()))
            .unwrap();
        let (tx, requests) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut adapter =
                match WaylandCapture::connect_test_stream(peer, 1, 0).and_then(|mut a| {
                    a.enable_pointer()?;
                    Ok(a)
                }) {
                    Ok(a) => a,
                    Err(e) => {
                        results.send(Reply::Failed(e.to_string())).unwrap();
                        return;
                    }
                };
            results.send(Reply::Connected).unwrap();
            let mut epoch = 0;
            let mut serial = 0;
            while let Ok(command) = requests.recv() {
                let reply = match command {
                    Command::Frame => match adapter.frame() {
                        Ok(CaptureOutcome::Frame(frame)) => {
                            assert_eq!((frame.source_width, frame.source_height), (800, 600));
                            assert!(
                                frame
                                    .image
                                    .as_raw()
                                    .chunks_exact(3)
                                    .all(|p| p == [255, 0, 0])
                            );
                            serial = adapter
                                .pointer_frame()
                                .expect("retained native ready proof")
                                .0;
                            Reply::Frame
                        }
                        Ok(CaptureOutcome::NoChange | CaptureOutcome::RetainedProofBlocked) => {
                            Reply::Failed("expected frame".into())
                        }
                        Err(e) => Reply::Failed(e.to_string()),
                    },
                    Command::Grant => match adapter.grant_pointer() {
                        Ok(e) => {
                            epoch = e;
                            Reply::Granted
                        }
                        Err(e) => Reply::Failed(e.to_string()),
                    },
                    Command::Action(sequence, kind) => match adapter.pointer_action(
                        &PointerAction {
                            lease_epoch: epoch,
                            sequence,
                            scope: DrawnScope {
                                source_epoch: 1,
                                geometry_epoch: adapter.geometry_epoch(),
                                frame_sequence: serial,
                            },
                            kind,
                            x: 40,
                            y: 30,
                        },
                        serial,
                    ) {
                        Ok(()) => {
                            assert!(
                                adapter.pointer_frame().is_none(),
                                "accepted action consumes retained frame"
                            );
                            Reply::Accepted
                        }
                        Err(e) => Reply::Failed(e.to_string()),
                    },
                };
                if results.send(reply).is_err() {
                    break;
                }
            }
        });
        let mut cache = None;
        assert!(matches!(
            receive(&mut server, &mut gl, &mut cache, &rx),
            Reply::Connected
        ));
        // Complete receiver registration before native grant: changes revoke later.
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let other_pointer = bind(&mut server, &mut other);
        focused(&mut server, &app);
        app.client.events();
        other.client.events();
        tx.send(Command::Frame).unwrap();
        assert!(matches!(
            receive(&mut server, &mut gl, &mut cache, &rx),
            Reply::Frame
        ));
        tx.send(Command::Grant).unwrap();
        assert!(matches!(
            receive(&mut server, &mut gl, &mut cache, &rx),
            Reply::Granted
        ));
        tx.send(Command::Action(1, PointerActionKind::Motion))
            .unwrap();
        assert!(matches!(
            receive(&mut server, &mut gl, &mut cache, &rx),
            Reply::Accepted
        ));
        let ev = received(&mut app, pointer);
        assert!(
            ev.iter()
                .any(|(op, a)| *op == 2 && a[1..] == [40 * 256, 30 * 256])
        );
        assert!(ev.iter().all(|(op, _)| matches!(op, 2 | 5)));
        assert!(received(&mut other, other_pointer).is_empty());
        if stale_frame {
            tx.send(Command::Action(2, PointerActionKind::ClickLeft))
                .unwrap();
            failed(receive(&mut server, &mut gl, &mut cache, &rx));
        } else {
            tx.send(Command::Frame).unwrap();
            assert!(matches!(
                receive(&mut server, &mut gl, &mut cache, &rx),
                Reply::Frame
            ));
            tx.send(Command::Action(2, PointerActionKind::ClickLeft))
                .unwrap();
            assert!(matches!(
                receive(&mut server, &mut gl, &mut cache, &rx),
                Reply::Accepted
            ));
            let ev = received(&mut app, pointer);
            assert_eq!(
                ev.iter()
                    .filter(|(op, _)| *op == 3)
                    .map(|(_, a)| (a[2], a[3]))
                    .collect::<Vec<_>>(),
                [(0x110, 1), (0x110, 0)]
            );
            assert!(received(&mut other, other_pointer).is_empty());
            tx.send(Command::Frame).unwrap();
            assert!(matches!(
                receive(&mut server, &mut gl, &mut cache, &rx),
                Reply::Frame
            ));
            revoke(&mut server.state, LOCAL);
            sync(&mut server);
            tx.send(Command::Action(3, PointerActionKind::ClickLeft))
                .unwrap();
            failed(receive(&mut server, &mut gl, &mut cache, &rx));
        }
        for _ in 0..2 {
            tx.send(Command::Action(4, PointerActionKind::ClickLeft))
                .unwrap();
            failed(receive(&mut server, &mut gl, &mut cache, &rx));
            assert!(received(&mut app, pointer).is_empty());
            assert!(received(&mut other, other_pointer).is_empty());
        }
        drop(tx);
        worker.join().expect("pointer adapter worker panicked");
        sync(&mut server);
    }
}
