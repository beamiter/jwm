//! Real WaylandCapture keyboard adapter → production protocol → wl_keyboard.
//! Uses software offscreen rendering, not a TCP/X11 viewer or DRM acceptance.
use super::keyboard_keymap::canonical_digest;
use super::wayland_capture::WaylandCapture;
use super::wayland_keyboard_wire::Key;
use super::x11_capture::CaptureOutcome;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::App, init_image_copy_capture, wire_test_client::Server,
};
use crate::backend::wayland_udev::remote_keyboard::{LOCAL, revoke};
use smithay::utils::SERIAL_COUNTER;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

enum Command {
    Frame,
    Arm([u8; 32]),
    Key(u64, u16, bool),
}
#[derive(Debug)]
enum Reply {
    Connected,
    Frame,
    Armed,
    Accepted,
    Failed(String),
}
fn roundtrip(server: &mut Server) {
    server.roundtrip();
    server.roundtrip();
}
fn keyboard(server: &mut Server, app: &mut App) -> u32 {
    let seat = app.client.bind("wl_seat", 7);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    roundtrip(server);
    app.client.events();
    keyboard
}
fn keys(app: &mut App, keyboard: u32) -> Vec<(u32, u32)> {
    let events = app.client.events();
    assert!(events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)));
    events
        .into_iter()
        .filter(|e| e.sender == keyboard && e.opcode == 3)
        .map(|e| (e.args[2], e.args[3]))
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
            Ok(reply) => return reply,
            Err(mpsc::TryRecvError::Disconnected) => panic!("keyboard adapter worker ended"),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        assert!(
            Instant::now() < deadline,
            "keyboard adapter protocol timeout"
        );
        roundtrip(server);
        let queue = server.state.image_capture_pending.as_ref().unwrap();
        fulfill_test_toplevel_capture(&mut gl.renderer, cache, &server.state, queue);
        roundtrip(server);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn require_failed(reply: Reply) {
    match reply {
        Reply::Failed(message) => assert!(!message.is_empty()),
        other => panic!("unexpected keyboard authorization: {other:?}"),
    }
}

#[test]
fn actual_wayland_keyboard_adapter_arms_ready_frame_and_revokes_recipient_delivery() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for wrong_digest in [false, true] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_color(&mut server, [0, 0, 255, 255]);
        let app_keyboard = keyboard(&mut server, &mut app);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        roundtrip(&mut server);
        app.client.events();
        let snapshot = server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .snapshot_authorized_keyboard(&surface)
            .unwrap();
        let digest = canonical_digest(snapshot.keymap_text().as_bytes()).unwrap();
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
                match WaylandCapture::connect_test_stream(peer, 1, 0).and_then(|mut adapter| {
                    adapter.enable_keyboard()?;
                    Ok(adapter)
                }) {
                    Ok(adapter) => adapter,
                    Err(error) => {
                        let _ = results.send(Reply::Failed(error.to_string()));
                        return;
                    }
                };
            if results.send(Reply::Connected).is_err() {
                return;
            }
            let mut epoch = 0;
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
                                    .all(|rgb| rgb == [255, 0, 0])
                            );
                            Reply::Frame
                        }
                        Ok(CaptureOutcome::NoChange) => {
                            Reply::Failed("expected captured frame".into())
                        }
                        Err(error) => Reply::Failed(error.to_string()),
                    },
                    Command::Arm(digest) => match adapter.arm_keyboard(digest) {
                        Ok(granted) => {
                            epoch = granted;
                            Reply::Armed
                        }
                        Err(error) => Reply::Failed(error.to_string()),
                    },
                    Command::Key(sequence, keycode, pressed) => match adapter.keyboard_key(&Key {
                        lease_epoch: epoch,
                        sequence,
                        source_epoch: 1,
                        geometry_epoch: adapter.geometry_epoch(),
                        frame_sequence: 1,
                        keycode,
                        pressed,
                    }) {
                        Ok(()) => Reply::Accepted,
                        Err(error) => Reply::Failed(error.to_string()),
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
        // Add a nonselected overlapping app and a real competing keyboard recipient.
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let other_keyboard = keyboard(&mut server, &mut other);
        tx.send(Command::Frame).unwrap();
        assert!(matches!(
            receive(&mut server, &mut gl, &mut cache, &rx),
            Reply::Frame
        ));
        let mut proposed = digest;
        if wrong_digest {
            proposed[0] ^= 1;
        }
        tx.send(Command::Arm(proposed)).unwrap();
        let reply = receive(&mut server, &mut gl, &mut cache, &rx);
        if wrong_digest {
            require_failed(reply);
            tx.send(Command::Arm(digest)).unwrap();
            require_failed(receive(&mut server, &mut gl, &mut cache, &rx));
            assert!(keys(&mut app, app_keyboard).is_empty());
            assert!(keys(&mut other, other_keyboard).is_empty());
        } else {
            assert!(matches!(reply, Reply::Armed));
            // Successful arm must release the retained capture frame and allow
            // subsequent normal capture, not permanently freeze the source.
            tx.send(Command::Frame).unwrap();
            assert!(matches!(
                receive(&mut server, &mut gl, &mut cache, &rx),
                Reply::Frame
            ));
            for (sequence, key, pressed) in [(1, 30, true), (2, 30, false), (3, 31, true)] {
                tx.send(Command::Key(sequence, key, pressed)).unwrap();
                assert!(matches!(
                    receive(&mut server, &mut gl, &mut cache, &rx),
                    Reply::Accepted
                ));
                assert_eq!(
                    keys(&mut app, app_keyboard),
                    [(u32::from(key), u32::from(pressed))]
                );
                assert!(keys(&mut other, other_keyboard).is_empty());
            }
            revoke(&mut server.state, LOCAL);
            roundtrip(&mut server);
            assert_eq!(keys(&mut app, app_keyboard), [(31, 0)]);
            for _ in 0..2 {
                tx.send(Command::Key(4, 31, false)).unwrap();
                require_failed(receive(&mut server, &mut gl, &mut cache, &rx));
                assert!(keys(&mut app, app_keyboard).is_empty());
                assert!(keys(&mut other, other_keyboard).is_empty());
            }
        }
        drop(tx);
        worker.join().expect("keyboard adapter thread panicked");
        roundtrip(&mut server);
    }
}
