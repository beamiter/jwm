//! Authenticated TCP host -> real Wayland capture/lease -> native recipient.
//! Protocol peer reports a decoded frame as drawn; this is not X11 UI, two
//! machines, or DRM presentation acceptance. All sockets and apps are owned.
use super::*;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::App, init_image_copy_capture, wire_test_client::Server,
};
use crate::remote::keyboard_keymap::canonical_digest;
use crate::remote::protocol::client_handshake;
use crate::remote::source_wire::decode_source_payload;
use crate::remote::tile::TileDecoder;
use crate::remote::wayland_capture::WaylandCapture;
use crate::remote::wayland_keyboard_wire::{Key, LeaseRequest, LeaseStatusKind};
use crate::remote::x11_keymap::KeymapTable;
use smithay::utils::SERIAL_COUNTER;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

const TEST_KEY: [u8; 32] = [0x47; 32];
#[derive(Clone, Copy, Debug)]
enum Case {
    BadDrawnScope,
    OldEpoch,
    Viewport,
    Disconnect,
}
#[derive(Debug)]
enum PeerEvent {
    Granted,
    Denied,
    Finished,
}
#[derive(Clone, Copy)]
enum PeerCommand {
    Press,
    End,
}
struct ShutdownGuard(Arc<AtomicBool>);
impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn options() -> HostOptions {
    HostOptions {
        frame_limits: Default::default(),
        listen: "127.0.0.1:0".into(),
        key: KeySource::File("/unused-private-test-key".into()),
        display: None,
        wayland_select: true,
        wayland_display: None,
        fps: 30,
        jpeg_quality: 90,
        jpeg_quality_floor: 90,
        fixed_jpeg_quality: true,
        max_width: 0,
        capture_source: CaptureSource::Auto,
        capture_area: CaptureArea::Root,
        shared_sources: Vec::new(),
        scaling: ScalingPolicy::Auto,
        allow_lan: false,
        allow_input: false,
        allow_window_resize: false,
        allow_wayland_keyboard: true,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}
fn bind_keyboard(server: &mut Server, app: &mut App) -> u32 {
    let seat = app.client.bind("wl_seat", 7);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    server.roundtrip();
    server.roundtrip();
    app.client.events();
    keyboard
}
fn collect_keys(app: &mut App, keyboard: u32, keys: &mut Vec<(u32, u32)>) {
    let events = app.client.events();
    assert!(events
        .iter()
        .all(|event| !(event.sender == 1 && event.opcode == 0)));
    keys.extend(
        events
            .into_iter()
            .filter(|event| event.sender == keyboard && event.opcode == 3)
            .map(|event| (event.args[2], event.args[3])),
    );
}
type Trace = Arc<Mutex<Vec<String>>>;
struct Probe {
    case: Case,
    trace: Trace,
    failures: mpsc::Receiver<String>,
}
fn note(trace: &Trace, message: impl Into<String>) {
    trace.lock().unwrap().push(message.into());
}
fn read_before(
    reader: &mut SessionReader<TcpStream>,
    deadline: Instant,
) -> Result<(MessageKind, Vec<u8>), ProtocolError> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| {
            ProtocolError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "absolute peer phase deadline",
            ))
        })?;
    reader
        .get_ref()
        .set_read_timeout(Some(left.min(Duration::from_secs(3))))?;
    reader.read_message()
}
fn pump_until(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    cache: &mut Option<(i32, i32, smithay::backend::renderer::gles::GlesRenderbuffer)>,
    probe: &Probe,
    phase: &str,
    mut done: impl FnMut() -> bool,
) {
    eprintln!("TCP keyboard case={:?} phase={phase}", probe.case);
    let deadline = Instant::now() + Duration::from_secs(12);
    while !done() {
        if let Ok(error) = probe.failures.try_recv() {
            panic!(
                "case={:?} phase={phase}: {error}; trace={:?}",
                probe.case,
                probe.trace.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "case={:?} phase={phase}: real TCP keyboard test timed out; trace={:?}",
            probe.case,
            probe.trace.lock().unwrap()
        );
        server.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            cache,
            &server.state,
            server.state.image_capture_pending.as_ref().unwrap(),
        );
        server.roundtrip();
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn peer(
    address: std::net::SocketAddr,
    digest: [u8; 32],
    case: Case,
    commands: mpsc::Receiver<PeerCommand>,
    events: mpsc::Sender<PeerEvent>,
    trace: &Trace,
) -> RemoteResult<()> {
    note(trace, "peer handshake");
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (receive, send) = client_handshake(&mut socket, &TEST_KEY, [0x25; 32])?.into_client();
    let mut reader = SessionReader::new(socket.try_clone()?, receive);
    let mut writer = SessionWriter::new(socket, send);
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            stream: super::super::messages::StreamCapabilities {
                wayland_keyboard: true,
                ..Default::default()
            },
            initial_viewport: None,
            request_input: false,
            request_clipboard: false,
            request_clipboard_files: false,
            keymap: KeymapTable::empty(),
        }
        .encode(),
    )?;
    writer.flush()?;
    note(trace, "peer waiting hello ack");
    let (kind, payload) = read_before(&mut reader, Instant::now() + Duration::from_secs(5))?;
    assert_eq!(kind, MessageKind::HelloAck);
    let hello = ServerHello::decode(&payload)?;
    assert!(hello.stream.wayland_keyboard);
    assert!(!hello.pointer_enabled && !hello.keyboard_enabled && !hello.clipboard_enabled);
    let mut decoder = TileDecoder::with_frame_limits(hello.stream.frame_limits)?;
    note(trace, "peer waiting first frame");
    let frame_deadline = Instant::now() + Duration::from_secs(5);
    let scope = loop {
        let (kind, payload) = read_before(&mut reader, frame_deadline)?;
        if kind == MessageKind::Frame {
            let (source_epoch, inner) = decode_source_payload(&payload)?;
            let frame =
                decoder.decode_into(inner, super::super::frame::new_decode_buffer_pool())?;
            assert_eq!(frame.dimensions(), (800, 600));
            let pixel = frame.image().get_pixel(400, 300).0;
            assert!(
                pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16,
                "wrong selected source pixels: {pixel:?}"
            );
            break DrawnScope {
                source_epoch,
                geometry_epoch: frame.geometry_epoch(),
                frame_sequence: frame.sequence(),
            };
        }
        assert_eq!(kind, MessageKind::SourceCatalog);
    };
    writer.write_message(
        MessageKind::FrameAck,
        &super::super::messages::encode_source_frame_ack(scope.source_epoch, scope.frame_sequence)?,
    )?;
    let request = LeaseRequest {
        keymap_digest: digest,
        source_epoch: scope.source_epoch,
        geometry_epoch: scope.geometry_epoch + u64::from(matches!(case, Case::BadDrawnScope)),
        frame_sequence: scope.frame_sequence,
    };
    writer.write_message(MessageKind::WaylandKeyboardRequest, &request.encode()?)?;
    writer.flush()?;
    note(trace, format!("peer sent lease request case={case:?}"));
    let lease_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match read_before(&mut reader, lease_deadline) {
            Ok((MessageKind::WaylandKeyboardStatus, payload)) => {
                break LeaseStatus::decode(&payload)?;
            }
            Ok((MessageKind::Frame, _)) => continue,
            Ok((MessageKind::SessionClose | MessageKind::Close, _))
                if matches!(case, Case::BadDrawnScope) =>
            {
                let _ = events.send(PeerEvent::Denied);
                return Ok(());
            }
            Err(ProtocolError::Io(error))
                if matches!(case, Case::BadDrawnScope)
                    && matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                    ) =>
            {
                events.send(PeerEvent::Denied)?;
                return Ok(());
            }
            other => {
                return Err(io::Error::other(format!("unexpected lease reply: {other:?}")).into());
            }
        }
    };
    if matches!(case, Case::BadDrawnScope) {
        assert_ne!(status.status, LeaseStatusKind::Granted);
        let _ = events.send(PeerEvent::Denied);
        return Ok(());
    }
    assert_eq!(status.status, LeaseStatusKind::Granted);
    assert_eq!(
        (
            status.source_epoch,
            status.geometry_epoch,
            status.frame_sequence
        ),
        (
            scope.source_epoch,
            scope.geometry_epoch,
            scope.frame_sequence
        )
    );
    note(trace, "peer granted");
    events.send(PeerEvent::Granted)?;
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(5))?,
        PeerCommand::Press
    ));
    let key = Key {
        lease_epoch: status.lease_epoch,
        sequence: 1,
        source_epoch: scope.source_epoch,
        geometry_epoch: scope.geometry_epoch,
        frame_sequence: scope.frame_sequence,
        keycode: 30,
        pressed: true,
    };
    note(trace, "peer sent press");
    writer.write_message(MessageKind::WaylandKeyboardKey, &key.encode()?)?;
    writer.flush()?;
    assert!(matches!(
        commands.recv_timeout(Duration::from_secs(5))?,
        PeerCommand::End
    ));
    note(trace, format!("peer ending case={case:?}"));
    match case {
        Case::OldEpoch => writer.write_message(
            MessageKind::WaylandKeyboardKey,
            &Key {
                lease_epoch: status.lease_epoch + 1,
                sequence: 2,
                keycode: 31,
                ..key
            }
            .encode()?,
        )?,
        Case::Viewport => writer.write_message(
            MessageKind::Viewport,
            &super::super::messages::encode_viewport(640, 480),
        )?,
        Case::Disconnect => {
            events.send(PeerEvent::Finished)?;
            return Ok(());
        }
        Case::BadDrawnScope => unreachable!(),
    }
    writer.flush()?;
    let revoke_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (kind, payload) = read_before(&mut reader, revoke_deadline)?;
        if kind == MessageKind::WaylandKeyboardStatus {
            assert_eq!(
                LeaseStatus::decode(&payload)?.status,
                LeaseStatusKind::Revoked
            );
            break;
        }
        assert_eq!(kind, MessageKind::Frame);
    }
    events.send(PeerEvent::Finished)?;
    Ok(())
}

#[test]
fn actual_tcp_keyboard_host_requires_drawn_scope_and_cleans_only_original_recipient() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in [
        Case::BadDrawnScope,
        Case::OldEpoch,
        Case::Viewport,
        Case::Disconnect,
    ] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_color(&mut server, [0, 0, 255, 255]);
        let app_keyboard = bind_keyboard(&mut server, &mut app);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        server.roundtrip();
        server.roundtrip();
        app.client.events();
        let digest = canonical_digest(
            server
                .state
                .seat
                .get_keyboard()
                .unwrap()
                .snapshot_authorized_keyboard(&surface)
                .unwrap()
                .keymap_text()
                .as_bytes(),
        )
        .unwrap();
        let (native, peer_socket) = UnixStream::pair().unwrap();
        server
            .display
            .handle()
            .insert_client(native, Arc::new(JwmClientState::default()))
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let _shutdown_guard = ShutdownGuard(Arc::clone(&shutdown));
        let (host_done, host_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let (failed_tx, failed_rx) = mpsc::channel();
        let probe = Probe {
            case,
            trace: Arc::clone(&trace),
            failures: failed_rx,
        };
        let host_trace = Arc::clone(&trace);
        let host_failed = failed_tx.clone();
        let host = std::thread::spawn(move || {
            note(&host_trace, "host connecting native capture");
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> RemoteResult<()> {
                    let capture = WaylandCapture::connect_test_stream(peer_socket, 1, 0)?;
                    note(&host_trace, "host native capture selected");
                    ready_tx.send(())?;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let stream = loop {
                        if shutdown.load(Ordering::Acquire) || Instant::now() >= deadline {
                            return Err(io::Error::other(
                                "private host accept canceled or timed out",
                            )
                            .into());
                        }
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(1))
                            }
                            Err(error) => return Err(error.into()),
                        }
                    };
                    let peer = authenticate_peer(stream, &TEST_KEY)?;
                    note(&host_trace, "host authenticated");
                    serve_authenticated(
                        peer,
                        &options(),
                        &shutdown,
                        &Mutex::new(Some(GrantedCapture::Wayland(Box::new(capture)))),
                    )
                }));
            let result = match result {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => {
                    let _ = host_failed.send("host worker panicked".into());
                    Err("host worker panicked".into())
                }
            };
            note(&host_trace, format!("host finished: {result:?}"));
            let _ = host_done.send(result);
        });
        let mut cache = None;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "native selection",
            || match ready_rx.try_recv() {
                Ok(()) => true,
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("native selection ended: {:?}", trace.lock().unwrap())
                }
            },
        );
        // Selection already holds the only live handle present at that point.
        // Finish *all* receiver registration before the native keyboard snapshot:
        // new_kbd correctly invalidates seat-wide authority, even for other apps.
        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let other_keyboard = bind_keyboard(&mut server, &mut other);
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        server.roundtrip();
        server.roundtrip();
        app.client.events();
        other.client.events();
        let (commands, command_rx) = mpsc::channel();
        let (events, event_rx) = mpsc::channel();
        let peer_trace = Arc::clone(&trace);
        let client = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                peer(address, digest, case, command_rx, events, &peer_trace)
            }));
            let result = match result {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err("protocol peer panicked".into()),
            };
            note(&peer_trace, format!("peer finished: {result:?}"));
            if let Err(error) = &result {
                let _ = failed_tx.send(error.clone());
            }
            result
        });
        let mut first = None;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "lease decision",
            || {
                if let Ok(event) = event_rx.try_recv() {
                    first = Some(event);
                    true
                } else {
                    false
                }
            },
        );
        if matches!(case, Case::BadDrawnScope) {
            assert!(matches!(first, Some(PeerEvent::Denied)));
            let mut keys = Vec::new();
            collect_keys(&mut app, app_keyboard, &mut keys);
            assert!(keys.is_empty());
            collect_keys(&mut other, other_keyboard, &mut keys);
            assert!(keys.is_empty());
        } else {
            assert!(matches!(first, Some(PeerEvent::Granted)));
            let mut received = Vec::new();
            let mut unexpected = Vec::new();
            commands.send(PeerCommand::Press).unwrap();
            pump_until(
                &mut server,
                &mut gl,
                &mut cache,
                &probe,
                "target press",
                || {
                    collect_keys(&mut app, app_keyboard, &mut received);
                    collect_keys(&mut other, other_keyboard, &mut unexpected);
                    received == [(30, 1)]
                },
            );
            assert!(unexpected.is_empty());
            commands.send(PeerCommand::End).unwrap();
            pump_until(
                &mut server,
                &mut gl,
                &mut cache,
                &probe,
                "held cleanup",
                || {
                    collect_keys(&mut app, app_keyboard, &mut received);
                    collect_keys(&mut other, other_keyboard, &mut unexpected);
                    received == [(30, 1), (30, 0)]
                },
            );
            assert!(unexpected.is_empty(), "cleanup must not send to other apps");
            let mut finished = false;
            pump_until(
                &mut server,
                &mut gl,
                &mut cache,
                &probe,
                "peer terminal response",
                || {
                    if let Ok(PeerEvent::Finished) = event_rx.try_recv() {
                        finished = true;
                    }
                    finished
                },
            );
        }
        client
            .join()
            .expect("private protocol peer panicked")
            .expect("private protocol peer failed");
        let mut ended = false;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "host teardown",
            || {
                if let Ok(result) = host_rx.try_recv() {
                    // Protocol rejection/disconnect may end the host with an error,
                    // but catch_unwind must never turn a worker panic into success.
                    assert_ne!(
                        result.as_ref().err().map(String::as_str),
                        Some("host worker panicked"),
                        "host worker panicked: {:?}",
                        trace.lock().unwrap()
                    );
                    ended = true;
                }
                ended
            },
        );
        host.join().expect("production host thread panicked");
    }
}
