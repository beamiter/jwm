//! Real X11 keyboard viewer -> authenticated TCP -> original native wl_keyboard recipient.
//! The fixture explicitly installs the owned Xvfb complete map in the native test seat;
//! this is not a claim that default desktop maps match, and production proof is unchanged.
//! All X input is injected only into a runner-owned Xvfb. Production offscreen
//! rendering is exercised, not two-machine latency or DRM presentation.
use super::*;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::App, init_image_copy_capture, wire_test_client::Server,
};
use crate::remote::keyboard_keymap::canonical_digest;
use crate::remote::wayland_capture::WaylandCapture;
use crate::remote::{
    client::{ClientOptions, run_client},
    viewer_test_driver::ViewerTestDriver,
    window_size::WindowSize,
};
use smithay::utils::SERIAL_COUNTER;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use xkbcommon::xkb;

const TEST_KEY: [u8; 32] = [0x47; 32];
#[derive(Clone, Copy, Debug)]
enum Case {
    Positive,
    Focus,
    MapMismatch,
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
        allow_wayland_pointer: false,
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
    assert!(
        events
            .iter()
            .all(|event| !(event.sender == 1 && event.opcode == 0))
    );
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
fn pump_until(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    cache: &mut Option<(i32, i32, smithay::backend::renderer::gles::GlesRenderbuffer)>,
    probe: &Probe,
    phase: &str,
    mut done: impl FnMut() -> bool,
) {
    eprintln!("actual viewer keyboard case={:?} phase={phase}", probe.case);
    let deadline = Instant::now() + Duration::from_secs(20);
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
            "case={:?} phase={phase}: real actual viewer keyboard test timed out; trace={:?}",
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

fn client_options(
    address: std::net::SocketAddr,
    display: String,
    label: String,
    _case: Case,
) -> ClientOptions {
    ClientOptions {
        wayland_keyboard: true,
        wayland_pointer: false,
        resize_source: None,
        window_size: Some(WindowSize {
            width: 1000,
            height: 600,
        }),
        window_position: None,
        frame_limits: Default::default(),
        address: address.to_string(),
        peer_label: label,
        key: KeySource::Bytes(TEST_KEY.to_vec()),
        display: Some(display),
        view_only: false,
        grab_input: false,
        clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        escape_key: "Escape".into(),
    }
}
fn drive(label: &str, case: Case, steps: mpsc::Receiver<()>, trace: &Trace) -> RemoteResult<()> {
    let driver = ViewerTestDriver::connect()?;
    let window = driver.wait_window(label)?;
    driver.focus(window)?;
    driver.wait_red(window)?;
    if matches!(case, Case::MapMismatch) {
        driver.wait_keyboard_revoked(window)?;
        note(trace, "full-map mismatch actually denied");
        driver.key_press(30)?;
        driver.key_release(30)?;
    } else {
        driver.wait_keyboard_armed(window)?;
        driver.key_press(30)?;
        steps.recv_timeout(Duration::from_secs(5))?;
        note(
            trace,
            "native original recipient received actual viewer key press",
        );
        if matches!(case, Case::Focus) {
            driver.unfocus()?;
            driver.wait_keyboard_revoked(window)?;
            steps.recv_timeout(Duration::from_secs(5))?;
            driver.key_release(30)?;
            driver.focus(window)?;
            driver.key_press(48)?;
            driver.key_release(48)?;
            driver.wait_keyboard_revoked(window)?;
        } else {
            driver.key_release(30)?;
            steps.recv_timeout(Duration::from_secs(5))?;
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    driver.close(window)?;
    Ok(())
}

#[test]
#[ignore = "requires runner-owned Xvfb and production offscreen GL; run via test-remote-viewer-e2e.py"]
fn actual_viewer_tcp_keyboard_presented_scope_and_revocation() {
    if std::env::var_os("JWM_REMOTE_TEST_VIEWER_DISPLAY").is_none() {
        assert!(
            std::env::var_os("JWM_REMOTE_TEST_REQUIRE_VIEWER").is_none(),
            "owned Xvfb viewer display required"
        );
        return;
    }
    assert_eq!(
        std::env::var("JWM_REMOTE_TEST_VIEWER_OWNED").as_deref(),
        Ok("1")
    );
    let display = std::env::var("JWM_REMOTE_TEST_VIEWER_DISPLAY").unwrap();
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        panic!("actual viewer test requires production offscreen GL");
    };
    let owned_map = ViewerTestDriver::connect()
        .unwrap()
        .snapshot_keymap()
        .unwrap();
    let owned_digest = canonical_digest(owned_map.as_bytes()).unwrap();
    for case in [Case::Positive, Case::Focus, Case::MapMismatch] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let map = if matches!(case, Case::MapMismatch) {
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let map = xkb::Keymap::new_from_names(
                &context,
                "",
                "",
                "de",
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap()
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            assert_ne!(
                canonical_digest(map.as_bytes()).unwrap(),
                owned_digest,
                "negative fixture must genuinely mismatch the full map"
            );
            map
        } else {
            owned_map.clone()
        };
        let keyboard = server.state.seat.get_keyboard().unwrap();
        keyboard
            .set_keymap_from_string(&mut server.state, map.clone())
            .unwrap();
        let mut app = App::with_color(&mut server, [0, 0, 255, 255]);
        let app_keyboard = bind_keyboard(&mut server, &mut app);
        let surface = server.state.surface_for_window(app.window).unwrap();
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        sync(&mut server);
        let native_map = keyboard
            .snapshot_authorized_keyboard(&surface)
            .unwrap()
            .keymap_text()
            .to_owned();
        assert_eq!(
            canonical_digest(native_map.as_bytes()).unwrap(),
            canonical_digest(map.as_bytes()).unwrap()
        );
        app.client.events();
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
        let other_keyboard = bind_keyboard(&mut server, &mut other);
        server
            .state
            .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
        sync(&mut server);
        app.client.events();
        other.client.events();

        // Establish a real, deterministic owned-server focus before mapping the
        // viewer. PointerRoot -> explicit focus can itself generate FocusOut;
        // do not weaken the production historical revocation guard to hide it.
        ViewerTestDriver::connect().unwrap().unfocus().unwrap();
        note(
            &trace,
            "owned X focus initialized to explicit root before client map",
        );
        let label = format!("owned-viewer-{}-{case:?}", std::process::id());
        let opts = client_options(address, display.clone(), label.clone(), case);
        let client_trace = Arc::clone(&trace);
        let client_failed = failed_tx.clone();
        let client = std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_client(opts)));
            let result = match result {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("actual viewer client panicked".into()),
            };
            note(&client_trace, format!("client finished {result:?}"));
            if let Err(error) = &result {
                let _ = client_failed.send(error.clone());
            }
            result
        });
        let (steps, step_rx) = mpsc::channel();
        let driver_trace = Arc::clone(&trace);
        let driver = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drive(&label, case, step_rx, &driver_trace)
            }));
            let result = match result {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(_) => Err("owned viewer driver panicked".into()),
            };
            note(&driver_trace, format!("driver finished {result:?}"));
            if let Err(error) = &result {
                let _ = failed_tx.send(error.clone());
            }
            result
        });
        let mut events = Vec::new();
        let mut unexpected = Vec::new();
        let mut notified = 0;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "real viewer actions",
            || {
                collect_keys(&mut app, app_keyboard, &mut events);
                collect_keys(&mut other, other_keyboard, &mut unexpected);
                if notified == 0 && events == [(30, 1)] {
                    steps.send(()).unwrap();
                    notified = 1;
                }
                if notified == 1 && events == [(30, 1), (30, 0)] {
                    steps.send(()).unwrap();
                    notified = 2;
                }
                driver.is_finished()
            },
        );
        driver
            .join()
            .expect("driver thread panicked")
            .expect("real viewer driver failed");
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "actual client teardown",
            || client.is_finished(),
        );
        client
            .join()
            .expect("client thread panicked")
            .expect("actual client failed");
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
                        Some("host worker panicked")
                    );
                    ended = true;
                }
                ended
            },
        );
        host.join().expect("host thread panicked");
        sync(&mut server);
        collect_keys(&mut app, app_keyboard, &mut events);
        collect_keys(&mut other, other_keyboard, &mut unexpected);
        let expected = if matches!(case, Case::MapMismatch) {
            vec![]
        } else {
            vec![(30, 1), (30, 0)]
        };
        assert_eq!(events, expected, "case={case:?} trace={trace:?}");
        assert!(
            unexpected.is_empty(),
            "cleanup or remote key reached another native receiver"
        );
    }
}
