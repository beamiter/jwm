//! Real X11 viewer presentation/input -> authenticated TCP -> native Wayland recipient.
//! All X input is injected only into a runner-owned Xvfb. Production offscreen
//! rendering is exercised, not two-machine latency or DRM presentation.
use super::*;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::{App, shm_buffer_file},
    init_image_copy_capture,
    wire_test_client::Server,
};
use crate::remote::wayland_capture::WaylandCapture;
use crate::remote::{
    client::{ClientOptions, run_client},
    viewer_test_driver::ViewerTestDriver,
    window_size::WindowSize,
};
use smithay::utils::SERIAL_COUNTER;
use smithay::{backend::input::InputTime, input::pointer::MotionEvent};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

// A per-host-thread scheduling request, never an authorization or clock override.
// Only the actual viewer candidate checkpoint sets this flag. Consuming it
// shortens the existing retained frame's lifetime and uses the ordinary
// discard -> capture -> TCP -> presentation -> ACK path for the replacement.
thread_local! {
    static FORCED_REFRESH: std::cell::RefCell<Option<Arc<Mutex<Option<Instant>>>>> = const {
        std::cell::RefCell::new(None)
    };
}
// Scoped scheduling-only reuse by the owned native viewer regression.
pub(super) fn with_forced_refresh<T>(
    request: Arc<Mutex<Option<Instant>>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCED_REFRESH.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    FORCED_REFRESH.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(request);
    });
    let _reset = Reset;
    run()
}
pub(super) fn take_forced_refresh() -> bool {
    FORCED_REFRESH.with(|slot| {
        slot.borrow().as_ref().is_some_and(|request| {
            let requested = request.lock().unwrap().take();
            if let Some(at) = requested {
                eprintln!("forced pointer refresh consumed after {:?}", at.elapsed());
            }
            requested.is_some()
        })
    })
}

const TEST_KEY: [u8; 32] = [0x47; 32];
#[derive(Clone, Copy, Debug)]
enum Case {
    Positive,
    NewFrameCancel,
    BlackBar,
    Viewport,
    ResizeRestore,
    Focus,
    ReadOnly,
    Scroll,
    ScrollNotRequested,
    Horizontal,
    HorizontalNotRequested,
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
        wayland_popups: false,
        wayland_resize_follow: false,
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
        allow_wayland_control: false,
        allow_wayland_scroll: false,
        allow_wayland_horizontal_scroll: false,
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
    capture_hold: Arc<AtomicBool>,
}
fn note(trace: &Trace, message: impl Into<String>) {
    let message = message.into();
    eprintln!("actual viewer phase: {message}");
    trace.lock().unwrap().push(message);
}
fn pump_until(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    cache: &mut Option<(i32, i32, smithay::backend::renderer::gles::GlesRenderbuffer)>,
    probe: &Probe,
    phase: &str,
    mut done: impl FnMut(&mut Server) -> bool,
) {
    eprintln!("actual viewer pointer case={:?} phase={phase}", probe.case);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(server) {
        if let Ok(error) = probe.failures.try_recv() {
            panic!(
                "case={:?} phase={phase}: {error}; trace={:?}",
                probe.case,
                probe.trace.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < deadline,
            "case={:?} phase={phase}: real actual viewer pointer test timed out; trace={:?}",
            probe.case,
            probe.trace.lock().unwrap()
        );
        server.roundtrip();
        if !probe.capture_hold.load(Ordering::Acquire) {
            fulfill_test_toplevel_capture(
                &mut gl.renderer,
                cache,
                &server.state,
                server.state.image_capture_pending.as_ref().unwrap(),
            );
        }
        server.roundtrip();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn client_options(
    address: std::net::SocketAddr,
    display: String,
    label: String,
    case: Case,
) -> ClientOptions {
    ClientOptions {
        viewer_backend: crate::remote::viewer_backend::ViewerBackend::X11,
        viewer_wayland_display: None,
        wayland_keyboard: false,
        wayland_pointer: !matches!(case, Case::ReadOnly),
        wayland_control: false,
        wayland_scroll: matches!(case, Case::Scroll | Case::HorizontalNotRequested),
        wayland_horizontal_scroll: matches!(case, Case::Horizontal),
        resize_source: None,
        follow_source_size: false,
        // A small real viewport keeps the controlled short-hold regression
        // inexpensive in debug software rendering. This case also uses a real
        // 200x150 native source; the 250 ms assertion is never relaxed.
        window_size: Some(if matches!(case, Case::NewFrameCancel) {
            WindowSize {
                width: 400,
                height: 300,
            }
        } else {
            WindowSize {
                width: 1000,
                height: 600,
            }
        }),
        window_position: None,
        frame_limits: Default::default(),
        address: address.to_string(),
        peer_label: label,
        key: KeySource::Bytes(TEST_KEY.to_vec()),
        display: Some(display),
        view_only: matches!(case, Case::ReadOnly),
        grab_input: false,
        clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        escape_key: "Escape".into(),
        scaling: Default::default(),
    }
}
fn drive(
    label: &str,
    case: Case,
    steps: mpsc::Receiver<()>,
    trace: &Trace,
    refresh: Arc<Mutex<Option<Instant>>>,
    cancel_checkpoint: Arc<AtomicBool>,
    capture_hold: Arc<AtomicBool>,
) -> RemoteResult<()> {
    note(trace, "driver connecting owned Xvfb");
    let driver = ViewerTestDriver::connect()?;
    note(trace, format!("driver waiting unique window label={label}"));
    let window = driver.wait_window(label)?;
    note(
        trace,
        format!("driver found window={window}, requesting focus"),
    );
    driver.focus(window)?;
    note(
        trace,
        format!(
            "driver waiting actual red framebuffer, title={:?}",
            driver.title(window)?
        ),
    );
    driver.wait_red(window)?;
    note(trace, "real viewer framebuffer red");
    if matches!(case, Case::ReadOnly) {
        assert!(!driver.title(window)?.contains("Pointer: Ready"));
        driver.motion(window, 500, 300)?;
        driver.click_at_current()?;
        std::thread::sleep(Duration::from_millis(100));
        driver.close(window)?;
        return Ok(());
    }
    note(trace, "driver waiting initial true Ready property");
    let ready = driver.wait_ready_after(window, None)?;
    note(trace, format!("driver initial Ready seq={ready}"));
    if matches!(case, Case::BlackBar) {
        driver.motion(window, 10, 300)?;
        driver.click_at_current()?;
        note(trace, "black bar click actually injected");
    }
    let (center_x, center_y) = if matches!(case, Case::NewFrameCancel) {
        (200, 150)
    } else {
        (500, 300)
    };
    driver.motion(window, center_x, center_y)?;
    note(trace, "driver waiting native accepted motion");
    steps.recv_timeout(Duration::from_secs(5))?; // native accepted real viewer motion
    note(
        trace,
        format!("driver waiting post-motion Ready after={ready}"),
    );
    let ready = driver.wait_ready_after(window, Some(ready))?;
    note(trace, format!("driver post-motion Ready seq={ready}"));
    let wheel = |negative, count| {
        if matches!(case, Case::Horizontal | Case::HorizontalNotRequested) {
            driver.horizontal_notches(negative, count)
        } else {
            driver.wheel_notches(negative, count)
        }
    };
    if matches!(
        case,
        Case::ScrollNotRequested | Case::HorizontalNotRequested
    ) {
        wheel(true, 3)?;
        driver.wait_scroll_observed(window, 3)?;
        let observed = driver
            .pointer_observation(window)?
            .expect("pointer observation");
        assert_eq!(
            observed.action_sequence, 1,
            "unrequested scroll must not send an action"
        );
        driver.close(window)?;
        return Ok(());
    }
    if matches!(case, Case::Scroll | Case::Horizontal) {
        // Hold only new capture fulfillment, never forge Ready or relax native
        // freshness. This makes the Waiting burst a deterministic negative case.
        capture_hold.store(true, Ordering::Release);
        wheel(true, 1)?;
        steps.recv_timeout(Duration::from_secs(5))?;
        driver.wait_title(window, "Pointer: Waiting")?;
        wheel(false, 3)?;
        driver.wait_scroll_observed(window, 4)?;
        let observed = driver
            .pointer_observation(window)?
            .expect("pointer observation");
        assert_eq!(
            observed.action_sequence, 2,
            "Waiting notches cannot queue/batch actions"
        );
        capture_hold.store(false, Ordering::Release);
        driver.wait_green(window)?;
        let green_ready = driver.wait_ready_after(window, Some(ready))?;
        assert_eq!(
            driver.pointer_observation(window)?.unwrap().action_sequence,
            2,
            "Waiting wheel burst must not replay when Ready returns"
        );
        // A new physical notch, not a queued old one, consumes the new ticket.
        wheel(false, 1)?;
        steps.recv_timeout(Duration::from_secs(5))?;
        driver.wait_red(window)?;
        driver.wait_ready_after(window, Some(green_ready))?;
        assert_eq!(
            driver.pointer_observation(window)?.unwrap().action_sequence,
            3
        );
        note(
            trace,
            "actual scroll recipient repainted green then red through capture/TCP/draw/ACK",
        );
        driver.close(window)?;
        return Ok(());
    }
    driver.click_at_current()?;
    steps.recv_timeout(Duration::from_secs(5))?; // balanced native click, not a UI claim
    note(
        trace,
        "first balanced click received by original native target",
    );
    match case {
        Case::Positive => {
            let _ = driver.wait_ready_after(window, Some(ready))?;
            driver.click_at_current()?;
            steps.recv_timeout(Duration::from_secs(5))?;
        }
        Case::NewFrameCancel => {
            let before = driver.wait_ready_after(window, Some(ready))?;
            let started = Instant::now();
            driver.button_down()?;
            let candidate = driver.wait_pointer_candidate(window, before)?;
            assert!(candidate.left_down);
            assert_eq!(candidate.candidate_frame, Some(before));
            assert_eq!(
                candidate.action_sequence, 2,
                "only motion and first click sent"
            );
            // This is the sole trigger. The host only discards the old frame;
            // the replacement must still traverse real capture and presentation.
            note(
                trace,
                format!(
                    "candidate observed after {:?}; forcing real refresh",
                    started.elapsed()
                ),
            );
            *refresh.lock().unwrap() = Some(Instant::now());
            let newer = driver.wait_ready_after(window, Some(before))?;
            note(
                trace,
                format!(
                    "replacement Ready {before}->{newer} after {:?}",
                    started.elapsed()
                ),
            );
            let observed = driver
                .pointer_observation(window)?
                .expect("candidate observation");
            assert_eq!(
                observed.candidate_frame, None,
                "new presented frame cancels candidate"
            );
            assert!(
                observed.left_down,
                "still physically held across frame boundary"
            );
            assert_eq!(observed.action_sequence, 2);
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "new presentation exceeded short-hold budget: {:?}",
                started.elapsed()
            );
            driver.button_up()?;
            note(
                trace,
                format!("button up flushed after {:?}", started.elapsed()),
            );
            let released = driver.wait_pointer_released(window, 2)?;
            assert!(!released.left_down);
            assert_eq!(released.candidate_frame, None);
            assert_eq!(
                released.action_sequence, 2,
                "cancelled release must not emit an action"
            );
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "release exceeded click duration; not cancellation evidence: {:?}",
                started.elapsed()
            );
            note(
                trace,
                format!(
                    "short hold cancelled by actual frame {before}->{newer}, elapsed={:?}",
                    started.elapsed()
                ),
            );
            cancel_checkpoint.store(true, Ordering::Release);
            steps.recv_timeout(Duration::from_secs(5))?; // native edges still exactly first click
            // A subsequent real click must succeed on this SAME lease. This
            // excludes native rejection/disconnect as a false cancellation proof.
            let _ = driver.wait_ready_after(window, Some(before))?;
            driver.click_at_current()?;
            steps.recv_timeout(Duration::from_secs(5))?;
        }
        Case::Focus => {
            driver.unfocus()?;
            driver.wait_title(window, "Pointer: Revoked")?;
            driver.focus(window)?;
            driver.motion(window, 500, 300)?;
            driver.click_at_current()?;
            driver.wait_title(window, "Pointer: Revoked")?;
        }
        Case::Viewport => {
            driver.resize(window, 900, 600)?;
            driver.wait_title(window, "Pointer: Revoked")?;
            driver.resize(window, 1000, 600)?;
            driver.motion(window, 500, 300)?;
            driver.click_at_current()?;
            driver.wait_title(window, "Pointer: Revoked")?;
        }
        Case::ResizeRestore => {
            driver.resize_restore(window, 900, 600, 1000, 600)?;
            driver.wait_title(window, "Pointer: Revoked")?;
            driver.motion(window, 500, 300)?;
            driver.click_at_current()?;
            driver.wait_title(window, "Pointer: Revoked")?;
        }
        Case::BlackBar => {}
        Case::ReadOnly
        | Case::Scroll
        | Case::ScrollNotRequested
        | Case::Horizontal
        | Case::HorizontalNotRequested => unreachable!(),
    }
    std::thread::sleep(Duration::from_millis(100));
    driver.close(window)?;
    Ok(())
}

#[test]
#[ignore = "requires runner-owned Xvfb and production offscreen GL; run via test-remote-viewer-e2e.py"]
fn actual_viewer_tcp_pointer_presented_scope_and_revocation() {
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
    for case in [
        Case::Positive,
        Case::NewFrameCancel,
        Case::BlackBar,
        Case::Viewport,
        Case::ResizeRestore,
        Case::Focus,
        Case::ReadOnly,
        Case::Scroll,
        Case::ScrollNotRequested,
        Case::Horizontal,
        Case::HorizontalNotRequested,
    ] {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = if matches!(case, Case::NewFrameCancel) {
            App::with_size_color(&mut server, [0, 0, 255, 255], 200, 150)
        } else {
            App::with_color(&mut server, [0, 0, 255, 255])
        };
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
        let capture_hold = Arc::new(AtomicBool::new(false));
        let probe = Probe {
            case,
            trace: Arc::clone(&trace),
            failures: failed_rx,
            capture_hold: Arc::clone(&capture_hold),
        };
        let refresh = Arc::new(Mutex::new(None));
        let cancel_checkpoint = Arc::new(AtomicBool::new(false));
        let host_refresh = Arc::clone(&refresh);
        let host_trace = Arc::clone(&trace);
        let host_failed = failed_tx.clone();
        let host = std::thread::spawn(move || {
            if matches!(case, Case::NewFrameCancel) {
                FORCED_REFRESH.with(|slot| *slot.borrow_mut() = Some(host_refresh));
            }
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
                    let mut opts = options();
                    opts.allow_wayland_scroll = matches!(
                        case,
                        Case::Scroll | Case::ScrollNotRequested | Case::HorizontalNotRequested
                    );
                    opts.allow_wayland_horizontal_scroll =
                        matches!(case, Case::Horizontal | Case::HorizontalNotRequested);
                    serve_authenticated(
                        peer,
                        &opts,
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
            |_| match ready_rx.try_recv() {
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

        // Establish a real, deterministic owned-server focus before mapping the
        // viewer. PointerRoot -> explicit focus can itself generate FocusOut;
        // do not weaken the production historical revocation guard to hide it.
        // Keep this connection alive until every session worker has joined.
        // Otherwise Xvfb's last-client reset can silently undo this setup.
        let startup_driver = ViewerTestDriver::connect().unwrap();
        startup_driver.initialize_outside_focus().unwrap();
        note(
            &trace,
            "owned X pointer outside viewer and focus explicit root before client map",
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
        let driver_checkpoint = Arc::clone(&cancel_checkpoint);
        let driver = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drive(
                    &label,
                    case,
                    step_rx,
                    &driver_trace,
                    refresh,
                    driver_checkpoint,
                    capture_hold,
                )
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
        let mut cancellation_checked = false;
        let mut scroll_notified = 0;
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "real viewer actions",
            |server| {
                events.extend(received(&mut app, app_pointer));
                unexpected.extend(received(&mut other, other_pointer));
                let motion = events.iter().any(|(op, _)| *op == 2);
                let edges = events.iter().filter(|(op, _)| *op == 3).count();
                if notified == 0 && motion {
                    steps.send(()).unwrap();
                    notified = 1;
                }
                let scrolls = events.iter().filter(|(op, _)| *op == 4).count();
                if matches!(case, Case::Scroll | Case::Horizontal) && scrolls > scroll_notified {
                    assert_eq!(scrolls, scroll_notified + 1, "wheel notches must not batch");
                    assert!(
                        scrolls <= 2,
                        "Waiting notches must not reach native recipient"
                    );
                    // This owned app responds only to a genuinely received axis
                    // event. New buffer/damage/commit then traverses real capture.
                    let color = if scrolls == 1 {
                        [0, 255, 0, 255]
                    } else {
                        [0, 0, 255, 255]
                    };
                    let (buffer, _file) = shm_buffer_file(server, &mut app.client, color);
                    app.client.request(app.surface, 1, &[buffer, 0, 0]);
                    app.client.request(app.surface, 2, &[0, 0, 800, 600]);
                    app.client.request(app.surface, 6, &[]);
                    sync(server);
                    scroll_notified = scrolls;
                    steps.send(()).unwrap();
                }
                if notified == 1 && edges >= 2 {
                    steps.send(()).unwrap();
                    notified = 2;
                }
                if matches!(case, Case::NewFrameCancel)
                    && !cancellation_checked
                    && cancel_checkpoint.load(Ordering::Acquire)
                {
                    assert_eq!(
                        edges, 2,
                        "cancelled candidate must not reach native recipient"
                    );
                    assert!(unexpected.is_empty());
                    cancellation_checked = true;
                    steps.send(()).unwrap();
                }
                if notified == 2 && edges >= 4 {
                    steps.send(()).unwrap();
                    notified = 3;
                }
                driver.is_finished()
            },
        );
        let driver_result = driver.join().expect("driver thread panicked");
        if let Err(error) = driver_result {
            panic!(
                "real viewer driver failed: {error}; case={case:?}; trace={:?}",
                trace.lock().unwrap()
            );
        }
        pump_until(
            &mut server,
            &mut gl,
            &mut cache,
            &probe,
            "actual client teardown",
            |_| client.is_finished(),
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
            |_| {
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
        drop(startup_driver);
        sync(&mut server);
        events.extend(received(&mut app, app_pointer));
        unexpected.extend(received(&mut other, other_pointer));
        let buttons: Vec<_> = events
            .iter()
            .filter(|(op, _)| *op == 3)
            .map(|(_, args)| (args[2], args[3]))
            .collect();
        let clicks = if matches!(
            case,
            Case::ReadOnly
                | Case::Scroll
                | Case::ScrollNotRequested
                | Case::Horizontal
                | Case::HorizontalNotRequested
        ) {
            0
        } else if matches!(case, Case::Positive | Case::NewFrameCancel) {
            2
        } else {
            1
        };
        let axes: Vec<_> = events
            .iter()
            .filter(|(op, _)| *op == 4)
            .map(|(_, args)| (args[1], args[2] as i32))
            .collect();
        let discrete: Vec<_> = events
            .iter()
            .filter(|(op, _)| *op == 8)
            .map(|(_, args)| (args[0], args[1] as i32))
            .collect();
        if matches!(case, Case::Scroll | Case::Horizontal) {
            assert_eq!(axes.len(), 2);
            let axis = u32::from(matches!(case, Case::Horizontal));
            assert_eq!(axes, [(axis, -10 * 256), (axis, 10 * 256)]);
            assert_eq!(discrete, [(axis, -1), (axis, 1)]);
            assert_eq!(scroll_notified, 2);
        } else {
            assert!(axes.is_empty(), "no scroll grant/action in case={case:?}");
            assert!(discrete.is_empty());
        }
        if matches!(case, Case::NewFrameCancel) {
            assert!(cancellation_checked);
        }
        assert_eq!(
            buttons,
            [(0x110, 1), (0x110, 0)].repeat(clicks),
            "case={case:?} trace={trace:?}"
        );
        assert!(
            unexpected.is_empty(),
            "non-target native recipient received actual viewer action"
        );
        if matches!(case, Case::ReadOnly) {
            assert!(
                events.is_empty(),
                "read-only viewer emitted native pointer events"
            );
        }
        let (native_x, native_y) = if matches!(case, Case::NewFrameCancel) {
            (100, 75)
        } else {
            (400, 300)
        };
        for (_, args) in events.iter().filter(|(op, _)| *op == 2) {
            assert!(
                args[1] >= (native_x - 2) * 256
                    && args[1] <= (native_x + 2) * 256
                    && args[2] >= (native_y - 2) * 256
                    && args[2] <= (native_y + 2) * 256,
                "unexpected mapped coordinate {args:?}"
            );
        }
    }
}
