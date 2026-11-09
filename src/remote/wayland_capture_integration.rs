//! Real production Wayland adapter ↔ compositor wire/SHM ↔ production offscreen
//! renderer. This is not an output/KMS/DRM presentation or hardware acceptance.
use super::wayland_capture::WaylandCapture;
use super::x11_capture::CaptureOutcome;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::App, init_image_copy_capture, wire_test_client::Server,
};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

enum Reply {
    Connected,
    Pixels { source: (u16, u16), bytes: Vec<u8> },
    Failed(String),
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
            Err(mpsc::TryRecvError::Disconnected) => panic!("adapter exited before result"),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        assert!(
            Instant::now() < deadline,
            "production adapter handshake/frame exceeded deadline"
        );
        server.roundtrip();
        let queue = server.state.image_capture_pending.as_ref().unwrap();
        fulfill_test_toplevel_capture(&mut gl.renderer, cache, &server.state, queue);
        server.roundtrip();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn real_wayland_adapter_isolates_selected_pixels_and_never_revives_revoked_source() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        // CI requires JWM_REQUIRE_HEADLESS_GL=1, so no silent skip there.
        return;
    };
    for revocation in ["lock", "unmap", "close"] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = Some(App::with_color(&mut server, [0, 0, 255, 255]));
        let (server_end, peer) = UnixStream::pair().unwrap();
        server
            .display
            .handle()
            .insert_client(server_end, Arc::new(JwmClientState::default()))
            .unwrap();
        let (commands, requests) = mpsc::channel::<()>();
        let (results, replies) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut adapter = match WaylandCapture::connect_test_stream(peer, 1, 0) {
                Ok(adapter) => adapter,
                Err(error) => {
                    let _ = results.send(Reply::Failed(error.to_string()));
                    return;
                }
            };
            if results.send(Reply::Connected).is_err() {
                return;
            }
            while requests.recv().is_ok() {
                let reply = match adapter.frame() {
                    Ok(CaptureOutcome::Frame(frame)) => Reply::Pixels {
                        source: (frame.source_width, frame.source_height),
                        bytes: frame.image.into_raw(),
                    },
                    Ok(CaptureOutcome::NoChange) => {
                        Reply::Failed("unexpected NoChange for explicit wire capture".into())
                    }
                    Err(error) => Reply::Failed(error.to_string()),
                };
                if results.send(reply).is_err() {
                    break;
                }
            }
        });
        let mut cache = None;
        match receive(&mut server, &mut gl, &mut cache, &replies) {
            Reply::Connected => {}
            Reply::Failed(error) => panic!("real adapter failed to initialize: {error}"),
            Reply::Pixels { .. } => panic!("frame before initialization"),
        }
        // This second client's surface overlaps the target but was never selected.
        let _blue = App::with_color(&mut server, [255, 0, 0, 255]);
        commands.send(()).unwrap();
        match receive(&mut server, &mut gl, &mut cache, &replies) {
            Reply::Pixels { source, bytes } => {
                assert_eq!(source, (800, 600));
                assert_eq!(bytes.len(), 800 * 600 * 3);
                assert!(
                    bytes.chunks_exact(3).all(|rgb| rgb == [255, 0, 0]),
                    "adapter captured nonselected surface/desktop pixels"
                );
            }
            Reply::Failed(error) => panic!("actual production capture failed: {error}"),
            Reply::Connected => panic!("unexpected reconnect"),
        }
        let mut locker = None;
        if revocation == "lock" {
            server.state.session_lock_supported = true;
            let mut client = server.connect();
            let manager = client.bind("ext_session_lock_manager_v1", 1);
            let lock = client.new_id();
            client.request(manager, 1, &[lock]);
            server.roundtrip();
            server.roundtrip();
            assert!(server.state.session_locked);
            assert!(
                client
                    .events()
                    .iter()
                    .any(|event| event.sender == lock && event.opcode == 0)
            );
            client.request(lock, 2, &[]);
            server.roundtrip();
            assert!(!server.state.session_locked);
            locker = Some(client);
        } else if revocation == "unmap" {
            app.as_mut().unwrap().unmap(&mut server);
            app.as_mut().unwrap().remap(&mut server);
        } else {
            drop(app.take());
            server.roundtrip();
            server.roundtrip();
        }
        for _ in 0..2 {
            commands.send(()).unwrap();
            match receive(&mut server, &mut gl, &mut cache, &replies) {
                Reply::Failed(error) => assert!(!error.is_empty()),
                _ => panic!("{revocation} authorized a new frame or fallback"),
            }
        }
        drop(commands);
        worker.join().expect("production adapter worker panicked");
        drop(locker);
        server.roundtrip();
    }
}
