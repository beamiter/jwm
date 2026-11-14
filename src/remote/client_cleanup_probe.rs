//! Owned-Xvfb cleanup-order regression. Hooks only schedule test threads; they
//! do not release input, change transport errors, or alter session policy.
use super::*;
use std::cell::RefCell;

thread_local! {
    static ACTIVE: RefCell<Option<Arc<Probe>>> = const { RefCell::new(None) };
}

pub(super) struct Probe {
    finished: mpsc::Sender<Instant>,
    join: mpsc::Sender<Instant>,
    gate: Mutex<mpsc::Receiver<()>>,
}

pub(super) fn current() -> Option<Arc<Probe>> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

impl Probe {
    pub(super) fn receiver_finished(&self) {
        self.finished
            .send(Instant::now())
            .expect("observer remains alive");
        self.gate
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("observer releases receiver gate before deadline");
    }
    pub(super) fn join_started(&self) {
        self.join
            .send(Instant::now())
            .expect("observer remains alive");
    }
}

fn scoped<T>(probe: Arc<Probe>, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    ACTIVE.with(|slot| assert!(slot.borrow_mut().replace(probe).is_none()));
    let _reset = Reset;
    run()
}

struct ReleaseGate(Option<mpsc::Sender<()>>);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[test]
#[ignore = "requires a dedicated owned JWM_TEST_X11_DISPLAY Xvfb server"]
fn transport_cleanup_returns_local_input_before_waiting_for_receiver() {
    use crate::remote::protocol::server_handshake;
    use crate::remote::source_session::{SourceId, SourceVersion};
    use crate::remote::source_wire::{SourceEntry, encode_frame_payload};
    use crate::remote::tile::{TileEncodeRequest, TileEncoder};
    use crate::remote::x11_capture::CapturedFrame;
    use image::{Rgb, RgbImage};
    use std::net::TcpListener;
    use x11rb::CURRENT_TIME;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    assert_eq!(
        std::env::var("JWM_REMOTE_TEST_VIEWER_OWNED").as_deref(),
        Ok("1")
    );
    let display = std::env::var("JWM_TEST_X11_DISPLAY").unwrap();
    assert!(display.starts_with(':'));
    let (driver, screen) = x11rb::connect(Some(&display)).unwrap();
    let root = driver.setup().roots[screen].root;
    driver
        .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 700, 500)
        .unwrap()
        .check()
        .unwrap();
    let name_atom = driver
        .intern_atom(false, b"_NET_WM_NAME")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let label = format!("cleanup-grab-probe-{:016x}", rand::random::<u64>());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (disconnect_tx, disconnect_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let (join_tx, join_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel();
    let probe = Arc::new(Probe {
        finished: finished_tx,
        join: join_tx,
        gate: Mutex::new(gate_rx),
    });
    const KEY: [u8; 32] = [0x71; 32];

    thread::scope(|scope| {
        let host = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("mock host accept: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let control = stream.try_clone().unwrap();
            let keys = server_handshake(&mut stream, &KEY, [0x62; 32]).unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(stream.try_clone().unwrap(), receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            let requested = ClientHello::decode(&payload).unwrap();
            writer
                .write_message(
                    MessageKind::HelloAck,
                    &ServerHello {
                        stream: super::super::messages::StreamCapabilities {
                            frame_limits: requested.stream.frame_limits,
                            ..Default::default()
                        },
                        pointer_enabled: true,
                        keyboard_enabled: true,
                        clipboard_enabled: false,
                        clipboard_files_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            writer
                .write_message(
                    MessageKind::SourceCatalog,
                    &SourceCatalog {
                        current: SourceVersion {
                            source: SourceId(1),
                            epoch: 1,
                        },
                        entries: vec![SourceEntry {
                            id: SourceId(1),
                            label: "test source".into(),
                        }],
                    }
                    .encode()
                    .unwrap(),
                )
                .unwrap();
            let frame = CapturedFrame {
                geometry_epoch: 1,
                source_width: 320,
                source_height: 240,
                image: RgbImage::from_pixel(320, 240, Rgb([20, 40, 60])),
            };
            let mut encoder = TileEncoder::new();
            encoder.plan(&frame, TileEncodeRequest::Keyframe).unwrap();
            let mut payload = Vec::new();
            encoder.encode_into(&mut payload, 1, &frame, 80).unwrap();
            writer
                .write_message(
                    MessageKind::Frame,
                    &encode_frame_payload(1, FramePurpose::Video, &payload).unwrap(),
                )
                .unwrap();
            writer.flush().unwrap();
            // Either explicit observer signal or its RAII sender drop closes the stream.
            let _ = disconnect_rx.recv_timeout(Duration::from_secs(10));
            control.shutdown(Shutdown::Both).unwrap();
        });
        let options = ClientOptions {
            control_duration: Default::default(),
            viewer_backend: ViewerBackend::X11,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_pointer: false,
            wayland_control: false,
            resumable_control: false,
            wayland_seamless_input: false,
            wayland_text: None,
            static_lossless: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            wayland_popup_pointer: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            address: address.to_string(),
            peer_label: label.clone(),
            key: KeySource::Bytes(KEY.to_vec()),
            display: Some(display.clone()),
            view_only: false,
            grab_input: true,
            seamless_input: true,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            frame_limits: Default::default(),
            escape_key: "F12".into(),
            scaling: Default::default(),
        };
        let client = scope.spawn(move || scoped(probe, || run_client(options)));
        // These guards drop before scope joins, including on an assertion panic.
        let disconnect = ReleaseGate(Some(disconnect_tx));
        let gate = ReleaseGate(Some(gate_tx));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut armed = false;
        let mut window = None;
        let mut motion = 0;
        while Instant::now() < deadline {
            for candidate in driver.query_tree(root).unwrap().reply().unwrap().children {
                let reply = driver
                    .get_property(false, candidate, name_atom, xproto::AtomEnum::ANY, 0, 4096)
                    .unwrap()
                    .reply()
                    .unwrap();
                let title = String::from_utf8_lossy(&reply.value);
                if title.starts_with(&label) {
                    window = Some(candidate);
                    if title.contains("seamless: leave image to release") {
                        armed = true;
                        break;
                    }
                    motion ^= 1;
                    driver
                        .warp_pointer(x11rb::NONE, candidate, 0, 0, 0, 0, 30 + motion, 30)
                        .unwrap()
                        .check()
                        .unwrap();
                }
            }
            if armed {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            armed,
            "real client must present and acquire seamless input first; window={window:?}"
        );
        drop(disconnect);
        let receiver_gated_at = finished_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("receiver published disconnect and reached gate");
        let join_started_at = join_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("real cleanup entered receiver join");
        let keyboard = driver
            .grab_keyboard(
                false,
                root,
                CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status;
        let pointer = driver
            .grab_pointer(
                false,
                root,
                xproto::EventMask::POINTER_MOTION,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                CURRENT_TIME,
            )
            .unwrap()
            .reply()
            .unwrap()
            .status;
        if keyboard == xproto::GrabStatus::SUCCESS {
            driver
                .ungrab_keyboard(CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
        }
        if pointer == xproto::GrabStatus::SUCCESS {
            driver
                .ungrab_pointer(CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
        }
        // A slow observer must not falsely pass after production cleanup or the
        // bounded worker gate has already timed out and dropped the viewer.
        let observed_during_wait = join_started_at.elapsed() < RECEIVER_JOIN_TIMEOUT
            && receiver_gated_at.elapsed() < Duration::from_secs(5);
        drop(gate);
        let result = client.join().unwrap();
        host.join().unwrap();
        let error = result.expect_err("the mock host deliberately disconnected");
        assert!(
            !classify_failure(error.as_ref()).reconnect_certified,
            "a session holding input at cleanup entry must require explicit reconnect"
        );
        assert!(
            observed_during_wait,
            "observer missed the bounded cleanup/gate interval"
        );
        eprintln!(
            "input while actual receiver cleanup is gated: keyboard={keyboard:?}, pointer={pointer:?}"
        );
        assert_eq!(
            keyboard,
            xproto::GrabStatus::SUCCESS,
            "network cleanup must not retain the local keyboard"
        );
        assert_eq!(
            pointer,
            xproto::GrabStatus::SUCCESS,
            "network cleanup must not retain the local pointer"
        );
    });
}
