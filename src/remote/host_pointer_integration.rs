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
use crate::remote::protocol::client_handshake;
use crate::remote::source_wire::decode_source_payload;
use crate::remote::tile::TileDecoder;
use crate::remote::wayland_capture::WaylandCapture;
use crate::remote::wayland_pointer_wire::*;
use crate::remote::x11_keymap::KeymapTable;
use smithay::utils::SERIAL_COUNTER;
use smithay::{backend::input::InputTime, input::pointer::MotionEvent};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

const TEST_KEY: [u8; 32] = [0x47; 32];
#[derive(Clone, Copy, Debug)]
enum Case {
    BadAck,
    StaleScope,
    StaticTwice,
    Viewport,
    Disconnect,
    UnnegotiatedScroll,
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
        allow_wayland_keyboard: false,
        allow_wayland_pointer: true,
        allow_wayland_scroll: false,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
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
    eprintln!("TCP pointer case={:?} phase={phase}", probe.case);
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
            "case={:?} phase={phase}: real TCP pointer test timed out; trace={:?}",
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

fn peer(address: std::net::SocketAddr, case: Case, trace: &Trace) -> RemoteResult<()> {
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    let (receive, send) = client_handshake(&mut socket, &TEST_KEY, [0x38; 32])?.into_client();
    let mut reader = SessionReader::new(socket.try_clone()?, receive);
    let mut writer = SessionWriter::new(socket, send);
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            stream: super::super::messages::StreamCapabilities {
                wayland_pointer: true,
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
    let (kind, payload) = read_before(&mut reader, Instant::now() + Duration::from_secs(5))?;
    assert_eq!(kind, MessageKind::HelloAck);
    let hello = ServerHello::decode(&payload)?;
    assert!(hello.stream.wayland_pointer);
    assert!(
        !hello.stream.wayland_scroll,
        "this peer negotiates pointer only"
    );
    assert!(!hello.pointer_enabled && !hello.keyboard_enabled);
    let mut decoder = TileDecoder::with_frame_limits(hello.stream.frame_limits)?;
    let mut first: Option<DrawnScope> = None;
    let mut requested = false;
    let mut negative_sent = false;
    let mut actions = 0u64;
    let mut expected_scope = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let record = read_before(&mut reader, deadline);
        let (kind, payload) = match record {
            Ok(record) => record,
            Err(ProtocolError::Io(error))
                if matches!(
                    case,
                    Case::BadAck | Case::StaleScope | Case::Viewport | Case::UnnegotiatedScroll
                ) && matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                assert!(
                    negative_sent,
                    "connection ended before negative stimulus: {case:?}"
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        match kind {
            MessageKind::SourceCatalog => {}
            MessageKind::Frame => {
                let (source_epoch, inner) = decode_source_payload(&payload)?;
                let frame =
                    decoder.decode_into(inner, super::super::frame::new_decode_buffer_pool())?;
                assert_eq!(frame.dimensions(), (800, 600));
                let pixel = frame.image().get_pixel(400, 300).0;
                assert!(
                    pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16,
                    "wrong source pixels {pixel:?}"
                );
                let scope = DrawnScope {
                    source_epoch,
                    geometry_epoch: frame.geometry_epoch(),
                    frame_sequence: frame.sequence(),
                };
                if let Some(old) = first {
                    if actions > 0 {
                        assert!(
                            scope.frame_sequence > old.frame_sequence,
                            "static scene needs a new actual wire frame"
                        );
                    }
                } else {
                    first = Some(scope);
                }
                expected_scope = Some(scope);
                writer.write_message(
                    MessageKind::FrameAck,
                    &super::super::messages::encode_source_frame_ack(
                        scope.source_epoch,
                        scope.frame_sequence + u64::from(matches!(case, Case::BadAck)),
                    )?,
                )?;
                if !requested {
                    writer.write_message(
                        MessageKind::WaylandPointerRequest,
                        &PointerRequest { scope }.encode()?,
                    )?;
                    requested = true;
                }
                writer.flush()?;
                if matches!(case, Case::BadAck) {
                    negative_sent = true;
                }
                note(trace, format!("ACK scope={scope:?} actions={actions}"));
            }
            MessageKind::WaylandPointerStatus => {
                let status = PointerStatus::decode(&payload)?;
                match status.status {
                    PointerStatusKind::Ready => {
                        assert!(!matches!(case, Case::BadAck), "wrong ACK granted pointer");
                        assert_eq!(Some(status.scope), expected_scope);
                        if actions == 1 && matches!(case, Case::Viewport) {
                            writer.write_message(
                                MessageKind::Viewport,
                                &super::super::messages::encode_viewport(640, 480),
                            )?;
                            writer.flush()?;
                            negative_sent = true;
                            continue;
                        }
                        if actions == 1 && matches!(case, Case::Disconnect) {
                            return Ok(());
                        }
                        if actions == 2 && matches!(case, Case::StaticTwice) {
                            return Ok(());
                        }
                        let scope = if actions == 1 && matches!(case, Case::StaleScope) {
                            first.unwrap()
                        } else {
                            status.scope
                        };
                        actions += 1;
                        let action = PointerAction {
                            lease_epoch: status.lease_epoch,
                            sequence: actions,
                            scope,
                            kind: if actions == 2 && matches!(case, Case::UnnegotiatedScroll) {
                                // A successful click and a second actual Ready
                                // establish the live lease, next sequence, fresh
                                // drawn scope and valid target coordinates. Only
                                // this independent capability is unauthorized.
                                assert_eq!(scope, status.scope);
                                assert_eq!(Some(scope), expected_scope);
                                assert!(scope.frame_sequence > first.unwrap().frame_sequence);
                                PointerActionKind::ScrollUp
                            } else {
                                PointerActionKind::ClickLeft
                            },
                            x: 12,
                            y: 14,
                        };
                        writer
                            .write_message(MessageKind::WaylandPointerAction, &action.encode()?)?;
                        writer.flush()?;
                        if actions == 2
                            && matches!(case, Case::StaleScope | Case::UnnegotiatedScroll)
                        {
                            negative_sent = true;
                        }
                        note(trace, format!("action={actions} scope={scope:?}"));
                    }
                    PointerStatusKind::Waiting => {}
                    PointerStatusKind::Revoked | PointerStatusKind::Denied => {
                        assert!(
                            matches!(
                                case,
                                Case::BadAck
                                    | Case::StaleScope
                                    | Case::Viewport
                                    | Case::UnnegotiatedScroll
                            ),
                            "unexpected denial {status:?}"
                        );
                        assert!(
                            negative_sent,
                            "lease denied before negative stimulus: {case:?}"
                        );
                        return Ok(());
                    }
                }
            }
            MessageKind::SessionClose | MessageKind::Close => {
                assert!(
                    matches!(
                        case,
                        Case::BadAck | Case::StaleScope | Case::Viewport | Case::UnnegotiatedScroll
                    ),
                    "unexpected close"
                );
                assert!(
                    negative_sent,
                    "session closed before negative stimulus: {case:?}"
                );
                return Ok(());
            }
            other => {
                return Err(
                    io::Error::other(format!("unexpected pointer record {other:?}")).into(),
                );
            }
        }
    }
}

#[test]
fn actual_tcp_pointer_host_requires_new_drawn_frame_for_every_action() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        assert!(
            std::env::var_os("JWM_REMOTE_TEST_REQUIRE_GL").is_none(),
            "required offscreen GL unavailable"
        );
        return;
    };
    for case in [
        Case::BadAck,
        Case::StaticTwice,
        Case::StaleScope,
        Case::Viewport,
        Case::Disconnect,
        Case::UnnegotiatedScroll,
    ] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_color(&mut server, [0, 0, 255, 255]);
        let app_pointer = bind(&mut server, &mut app);
        focused(&mut server, &app);
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

        let mut other = App::with_color(&mut server, [255, 0, 0, 255]);
        let other_pointer = bind(&mut server, &mut other);
        focused(&mut server, &app);
        app.client.events();
        other.client.events();
        let peer_trace = Arc::clone(&trace);
        let client = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                peer(address, case, &peer_trace)
            }));
            let result = match result {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(_) => Err("pointer protocol peer panicked".into()),
            };
            note(&peer_trace, format!("peer finished: {result:?}"));
            if let Err(error) = &result {
                let _ = failed_tx.send(error.clone());
            }
            result
        });
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "pointer protocol",
            || client.is_finished(),
        );
        client
            .join()
            .expect("peer thread panicked")
            .expect("pointer peer failed");
        let mut ended = false;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "host teardown",
            || {
                if let Ok(result) = host_rx.try_recv() {
                    assert_ne!(
                        result.as_ref().err().map(String::as_str),
                        Some("host worker panicked"),
                        "{trace:?}"
                    );
                    ended = true;
                }
                ended
            },
        );
        host.join().expect("host thread panicked");
        sync(&mut server);
        let ev = received(&mut app, app_pointer);
        let buttons: Vec<_> = ev
            .iter()
            .filter(|(op, _)| *op == 3)
            .map(|(_, args)| (args[2], args[3]))
            .collect();
        let clicks = if matches!(case, Case::BadAck) {
            0
        } else if matches!(case, Case::StaticTwice) {
            2
        } else {
            1
        };
        assert_eq!(
            buttons,
            [(0x110, 1), (0x110, 0)].repeat(clicks),
            "case={case:?} {trace:?}"
        );
        assert!(
            ev.iter()
                .all(|(op, _)| !matches!(*op, 4 | 6 | 7 | 8 | 9 | 10)),
            "pointer-only session delivered native axis metadata/events: case={case:?} {ev:?}"
        );
        for (_, args) in ev.iter().filter(|(op, _)| *op == 2) {
            assert_eq!((args[1], args[2]), (12 * 256, 14 * 256));
        }
        assert!(
            received(&mut other, other_pointer).is_empty(),
            "non-target recipient received remote pointer"
        );
    }
}
