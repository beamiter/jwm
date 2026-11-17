//! Explicit popup actions through authenticated TCP and the production viewer.
//! Owned compositor feedback follows real GL readback; no GTK or DRM claim.
//! This is a child of the existing pointer viewer fixture for low-level helpers.
use super::*;
use crate::backend::wayland_udev::{image_copy_capture::wire_test_client::Event, remote_popup};

const ROOT_WIDTH: u32 = 32;
const ROOT_HEIGHT: u32 = 24;

fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let done = client.new_id();
    client.request(1, 0, &[done]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        if let Some(error) = events.iter().find(|e| e.sender == 1 && e.opcode == 0) {
            panic!(
                "unexpected protocol error: object={:?} code={:?}",
                error.args.first(),
                error.args.get(1)
            );
        }
        let complete = events.iter().any(|e| e.sender == done && e.opcode == 0);
        all.extend(events);
        if complete {
            return all;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}
fn direct_popup_resources(
    server: &mut Server,
    app: &mut App,
    seat: u32,
    serial: u32,
    main_pointer: u32,
    main_keyboard: u32,
    private_pointer: u32,
) -> (u32, u32, u32) {
    let c = &mut app.client;
    let compositor = c.bind("wl_compositor", 6);
    let shell = c.bind("xdg_wm_base", 6);
    let pos = c.new_id();
    c.request(shell, 1, &[pos]);
    c.request(pos, 1, &[12, 10]);
    c.request(pos, 2, &[0, 0, 1, 1]);
    c.request(pos, 3, &[5]);
    c.request(pos, 4, &[8]);
    c.request(pos, 6, &[4, 3]);
    let surface = c.new_id();
    c.request(compositor, 0, &[surface]);
    let xdg = c.new_id();
    c.request(shell, 2, &[xdg, surface]);
    let role = c.new_id();
    c.request(xdg, 2, &[role, app.xdg, pos]);
    c.request(role, 1, &[seat, serial]);
    c.request(surface, 6, &[]);
    let events = fence(server, c);
    for e in &events {
        assert_ne!(e.sender, main_pointer, "popup mapping touched main seat");
        assert_ne!(
            e.sender, main_keyboard,
            "popup mapping touched main keyboard"
        );
        if e.sender == private_pointer {
            assert!(
                !matches!(e.opcode, 2 | 3 | 4),
                "mapping alone delivered pointer input"
            );
        }
    }
    assert!(
        events
            .iter()
            .any(|e| e.sender == role && e.opcode == 0 && e.args == [4, 3, 12, 10])
    );
    let serial = events
        .iter()
        .find(|e| e.sender == xdg && e.opcode == 0)
        .unwrap()
        .args[0];
    c.request(xdg, 4, &[serial]);
    let (pixels, _file) = shm(server, c, [0, 255, 0, 255], 12, 10);
    c.request(xdg, 3, &[0, 0, 12, 10]);
    c.request(surface, 1, &[pixels, 0, 0]);
    c.request(surface, 6, &[]);
    for e in fence(server, c) {
        assert_ne!(e.sender, main_pointer, "popup commit touched main seat");
        assert_ne!(
            e.sender, main_keyboard,
            "popup commit touched main keyboard"
        );
        if e.sender == private_pointer {
            assert!(
                !matches!(e.opcode, 2 | 3 | 4),
                "popup commit alone delivered input"
            );
        }
    }
    (surface, xdg, role)
}

fn direct_popup(
    server: &mut Server,
    app: &mut App,
    seat: u32,
    serial: u32,
    main_pointer: u32,
    main_keyboard: u32,
    private_pointer: u32,
) -> u32 {
    direct_popup_resources(
        server,
        app,
        seat,
        serial,
        main_pointer,
        main_keyboard,
        private_pointer,
    )
    .0
}

/// JPEG evidence is deliberately tested away from popup edges. A popup image
/// must contain both its green interior and the unchanged red root exterior.
fn assert_family_pixels(pixels: &[u8], popup: bool, selected: bool) {
    assert_eq!(pixels.len(), (ROOT_WIDTH * ROOT_HEIGHT * 4) as usize);
    let at = |x: u32, y: u32| &pixels[((y * ROOT_WIDTH + x) * 4) as usize..][..4];
    let expected = if selected { 0 } else { 2 };
    for (x, y) in [(25, 5), (25, 19), (5, 20)] {
        let p = at(x, y);
        assert!(p[expected] >= 230 && p[3] == 255);
        assert!(p[(expected + 1) % 3] <= 25 && p[(expected + 2) % 3] <= 25);
    }
    if popup {
        for (x, y) in [(8, 7), (10, 8)] {
            let p = at(x, y);
            assert!(
                p[1] >= 230 && p[0] <= 25 && p[2] <= 25 && p[3] == 255,
                "actual viewer must contain the selected direct popup interior"
            );
        }
    }
}

fn bind_main(server: &mut Server, app: &mut App) -> (u32, u32) {
    let seat = app.client.bind("wl_seat", 7);
    let pointer = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    fence(server, &mut app.client);
    (pointer, keyboard)
}
struct PopupHarness {
    native: Server,
    local: Server,
    target: App,
    other: App,
    main_pointer: u32,
    main_keyboard: u32,
    other_pointer: u32,
    other_keyboard: u32,
    private_seat: Option<u32>,
    private_pointer: Option<u32>,
    input: Option<WlPointer>,
    feedback: Vec<PresentationFeedbackCallback>,
    readback: Option<Readback>,
    frames: Arc<Mutex<(Option<u64>, Option<u64>)>>,
    ready: Arc<Mutex<Option<u64>>>,
    dispatched: Arc<Mutex<(u64, u64)>>,
    private_events: Vec<(u16, Vec<u32>)>,
    last_presented: Option<u64>,
    last_visual: Option<(bool, bool)>,
    shutdown: Arc<AtomicBool>,
    raw_ready: Arc<AtomicBool>,
    popup_client_lifetime: Arc<Mutex<crate::remote::client::PopupLifetimeObservation>>,
    popup_host_lifetime: Arc<Mutex<Option<host::popup::PopupHostLifetimeSnapshot>>>,
    _stop: Stop,
    client_done: mpsc::Receiver<crate::remote::RemoteResult<()>>,
    host_done: mpsc::Receiver<crate::remote::RemoteResult<()>>,
    client: Option<thread::JoinHandle<()>>,
    host: Option<thread::JoinHandle<()>>,
}
impl PopupHarness {
    fn new(gl: &mut HeadlessSmithayGles, host_permission: bool, client_permission: bool) -> Self {
        Self::new_peer(gl, host_permission, client_permission, None)
    }
    fn new_peer(
        gl: &mut HeadlessSmithayGles,
        host_permission: bool,
        client_permission: bool,
        raw: Option<RawCase>,
    ) -> Self {
        Self::configured(gl, host_permission, client_permission, raw, None)
    }
    fn configured(
        gl: &mut HeadlessSmithayGles,
        host_permission: bool,
        client_permission: bool,
        raw: Option<RawCase>,
        prepared_change: Option<PreparedChange>,
    ) -> Self {
        Self::configured_with_schedule(
            gl,
            host_permission,
            client_permission,
            raw,
            prepared_change,
            None,
        )
    }
    fn configured_with_schedule(
        gl: &mut HeadlessSmithayGles,
        host_permission: bool,
        client_permission: bool,
        raw: Option<RawCase>,
        prepared_change: Option<PreparedChange>,
        schedule: Option<Arc<dyn Fn(host::popup::PopupHostTestEvent) + Send + Sync>>,
    ) -> Self {
        Self::configured_with_delivery_pause(
            gl,
            host_permission,
            client_permission,
            raw,
            prepared_change,
            schedule,
            None,
        )
    }
    fn configured_with_delivery_pause(
        gl: &mut HeadlessSmithayGles,
        host_permission: bool,
        client_permission: bool,
        raw: Option<RawCase>,
        prepared_change: Option<PreparedChange>,
        schedule: Option<Arc<dyn Fn(host::popup::PopupHostTestEvent) + Send + Sync>>,
        delivery_pause: Option<
            Arc<crate::remote::wayland_capture::interactive_popup::PopupDeliveryPause>,
        >,
    ) -> Self {
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        let mut target =
            App::with_size_color(&mut native, [0, 0, 255, 255], ROOT_WIDTH, ROOT_HEIGHT);
        let (main_pointer, main_keyboard) = bind_main(&mut native, &mut target);
        focus(&mut native, &target);
        fence(&mut native, &mut target.client);
        let (server_stream, capture_stream) = UnixStream::pair().unwrap();
        native
            .display
            .handle()
            .insert_client(server_stream, Arc::new(JwmClientState::default()))
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stopping = shutdown.clone();
        let (selected_tx, selected) = mpsc::channel();
        let (host_tx, host_done) = mpsc::channel();
        let popup_host_lifetime = Arc::new(Mutex::new(None));
        let host_lifetime = popup_host_lifetime.clone();
        let host = thread::spawn(move || {
            let result = (|| -> crate::remote::RemoteResult<()> {
                let capture = WaylandCapture::connect_test_stream_popup_pointer(capture_stream, 1)?;
                selected_tx.send(())?;
                let deadline = Instant::now() + Duration::from_secs(10);
                let stream = loop {
                    assert!(!stopping.load(Ordering::Acquire) && Instant::now() < deadline);
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => thread::yield_now(),
                        Err(e) => return Err(e.into()),
                    }
                };
                let peer = host::authenticate_peer(stream, &KEY)?;
                let mut opts = options(false, false);
                opts.allow_wayland_pointer = false;
                opts.allow_wayland_popup_pointer = host_permission;
                let serve = || {
                    host::popup::with_popup_lifetime_observer(host_lifetime, || {
                        host::serve_authenticated(
                            peer,
                            &opts,
                            &stopping,
                            &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
                        )
                    })
                };
                let scheduled = || {
                    if let Some(schedule) = schedule {
                        host::popup::with_popup_schedule_observer(schedule, serve)
                    } else {
                        serve()
                    }
                };
                if let Some(pause) = delivery_pause {
                    crate::remote::wayland_capture::interactive_popup::with_popup_delivery_pause(
                        pause, scheduled,
                    )
                } else {
                    scheduled()
                }
            })();
            let _ = host_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            native.roundtrip();
            fulfill_test_toplevel_capture(
                &mut gl.renderer,
                &mut None,
                &native.state,
                native.state.image_capture_pending.as_ref().unwrap(),
            );
            match selected.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("popup host selection failed: {:?}", host_done.try_recv())
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "popup source selection deadline");
        }
        // `selected` is sent only after the production prepared constructor
        // returns. This test must not add a source-creation synchronization fix.
        if let Some(change) = prepared_change {
            assert!(!remote_popup::active(&native.state));
            match change {
                PreparedChange::Unmap => {
                    target.unmap(&mut native);
                    target.remap(&mut native);
                }
                PreparedChange::Lock => {
                    native.state.session_lock_supported = true;
                    let mut locker = native.connect();
                    let manager = locker.bind("ext_session_lock_manager_v1", 1);
                    let lock = locker.new_id();
                    locker.request(manager, 1, &[lock]);
                    assert!(
                        fence(&mut native, &mut locker)
                            .iter()
                            .any(|e| e.sender == lock && e.opcode == 0)
                    );
                    // ext_session_lock_v1: get_lock_surface is opcode 1;
                    // the real unlock_and_destroy request is opcode 2.
                    locker.request(lock, 2, &[]);
                    fence(&mut native, &mut locker);
                }
            }
            focus(&mut native, &target);
            fence(&mut native, &mut target.client);
            assert!(!remote_popup::active(&native.state));
        }
        // The unrelated root is created only after exact source selection.
        let mut other =
            App::with_size_color(&mut native, [255, 0, 0, 255], ROOT_WIDTH, ROOT_HEIGHT);
        let (other_pointer, other_keyboard) = bind_main(&mut native, &mut other);
        focus(&mut native, &target);
        fence(&mut native, &mut target.client);
        fence(&mut native, &mut other.client);
        assert_eq!(other.client.global_count("wl_seat"), 1);
        let (server_stream, stream) = UnixStream::pair().unwrap();
        local
            .display
            .handle()
            .insert_client(server_stream, Arc::new(JwmClientState::default()))
            .unwrap();
        let frames = Arc::new(Mutex::new((None, None)));
        let ready = Arc::new(Mutex::new(None));
        let dispatched = Arc::new(Mutex::new((0, 0)));
        let observed_dispatch = dispatched.clone();
        let observed_frames = frames.clone();
        let observed_ready = ready.clone();
        let (client_tx, client_done) = mpsc::channel();
        let raw_ready = Arc::new(AtomicBool::new(false));
        let observed_raw_ready = raw_ready.clone();
        let popup_client_lifetime = Arc::new(Mutex::new(
            crate::remote::client::PopupLifetimeObservation::default(),
        ));
        let client_lifetime = popup_client_lifetime.clone();
        let client = thread::spawn(move || {
            if let Some(case) = raw {
                let _ = client_tx.send(raw_peer(address, case, observed_raw_ready));
                return;
            }
            let opts = ClientOptions {
                control_duration: Default::default(),
                viewer_backend: ViewerBackend::Wayland,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: false,
                wayland_popup_pointer: client_permission,
                wayland_control: false,
                resumable_control: false,
                wayland_seamless_input: false,
                wayland_text: None,
                static_lossless: false,
                wayland_scroll: false,
                wayland_horizontal_scroll: false,
                resize_source: None,
                follow_source_size: false,
                window_size: Some(WindowSize {
                    width: ROOT_WIDTH as u16,
                    height: ROOT_HEIGHT as u16,
                }),
                window_position: None,
                frame_limits: Default::default(),
                address: address.to_string(),
                peer_label: "owned popup action".into(),
                key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
                display: None,
                view_only: !client_permission,
                grab_input: false,
                seamless_input: false,
                clipboard: false,
                clipboard_mode: Default::default(),
                clipboard_direction: Default::default(),
                clipboard_file_limit: 0,
                escape_key: "Escape".into(),
                scaling: Default::default(),
            };
            let result = crate::remote::client::with_popup_lifetime_observer(
                client_lifetime,
                || {
                    with_wayland_pointer_observer(observed_ready, || {
                        crate::remote::wayland_viewer::with_wayland_control_frame_observer(
                            observed_frames,
                            || {
                                crate::remote::wayland_viewer::with_pointer_button_dispatch_observer(
                                    observed_dispatch,
                                    || with_wayland_test_stream(stream, || run_client(opts)),
                                )
                            },
                        )
                    })
                },
            );
            let _ = client_tx.send(result);
        });
        Self {
            native,
            local,
            target,
            other,
            main_pointer,
            main_keyboard,
            other_pointer,
            other_keyboard,
            private_seat: None,
            private_pointer: None,
            input: None,
            feedback: Vec::new(),
            readback: None,
            frames,
            ready,
            dispatched,
            private_events: Vec::new(),
            last_presented: None,
            last_visual: None,
            _stop: Stop(shutdown.clone()),
            shutdown,
            raw_ready,
            popup_client_lifetime,
            popup_host_lifetime,
            client_done,
            host_done,
            client: Some(client),
            host: Some(host),
        }
    }
}
impl PopupHarness {
    fn tick(&mut self, gl: &mut HeadlessSmithayGles, display: Option<(bool, bool)>) {
        self.native.dispatch_timers(Duration::from_millis(1));
        // Bind the real dynamic application-filtered seat after negotiated grant.
        // No receiver is fabricated or attached to the compositor's main seat.
        for e in self.target.client.events() {
            assert!(!(e.sender == 1 && e.opcode == 0), "target protocol error");
            assert_ne!(
                e.sender, self.main_keyboard,
                "popup mode touched main keyboard receiver"
            );
            assert_ne!(
                e.sender, self.main_pointer,
                "remote popup action touched main-seat receiver"
            );
            if e.sender == 2 && e.opcode == 0 {
                let len = e.args[1] as usize;
                let bytes: Vec<_> = e.args[2..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                if &bytes[..len - 1] == b"wl_seat" {
                    assert!(
                        self.private_seat.is_none(),
                        "private seat must not be regranted"
                    );
                    let seat = self.target.client.new_id();
                    self.target.client.request(
                        2,
                        0,
                        &[
                            e.args[0],
                            8,
                            u32::from_ne_bytes(*b"wl_s"),
                            u32::from_ne_bytes(*b"eat\0"),
                            7,
                            seat,
                        ],
                    );
                    let pointer = self.target.client.new_id();
                    self.target.client.request(seat, 0, &[pointer]);
                    self.private_seat = Some(seat);
                    self.private_pointer = Some(pointer);
                }
            }
            if Some(e.sender) == self.private_pointer {
                self.private_events.push((e.opcode, e.args));
            }
        }
        for e in self.other.client.events() {
            assert_ne!(
                e.sender, self.other_keyboard,
                "popup mode touched other keyboard receiver"
            );
            assert_ne!(
                e.sender, self.other_pointer,
                "unselected root received input"
            );
            if e.sender == 2 && e.opcode == 0 {
                let len = e.args[1] as usize;
                let bytes: Vec<_> = e.args[2..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                assert_ne!(
                    &bytes[..len - 1],
                    b"wl_seat",
                    "private seat leaked to other client"
                );
            }
        }
        self.native.roundtrip();
        if self.private_pointer.is_some() {
            self.raw_ready.store(true, Ordering::Release);
        }
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.native.state,
            self.native.state.image_capture_pending.as_ref().unwrap(),
        );
        self.native.roundtrip();
        let frames = self.frames.clone();
        let snapshot = frames.lock().unwrap();
        self.local.roundtrip();
        for (&window, top) in &self.local.state.toplevels {
            if !top.is_initial_configure_sent() {
                let geo = self.local.state.window_geometry.get_mut(&window).unwrap();
                geo.w = ROOT_WIDTH;
                geo.h = ROOT_HEIGHT;
                top.with_pending_state(|s| {
                    s.size = Some((ROOT_WIDTH as i32, ROOT_HEIGHT as i32).into());
                    s.states.set(TopState::Activated);
                });
                top.send_configure();
            }
        }
        self.local.roundtrip();
        if let Some(top) = self.local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            if self.input.is_none() {
                let pointers: Vec<_> = self
                    .local
                    .state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .client_pointers(&surface.client().unwrap())
                    .collect();
                if let Some(p) = pointers.first() {
                    assert_eq!(pointers.len(), 1);
                    p.enter(1, &surface, 8.0, 8.0);
                    p.frame();
                    self.input = Some(p.clone());
                    self.local.roundtrip();
                }
            }
            self.feedback.extend(take_feedback(&surface));
        }
        if display.is_some() && !self.feedback.is_empty() {
            let seq = snapshot.0.expect("real committed viewer sequence");
            assert_eq!(
                self.feedback.len(),
                1,
                "one retained native proof/commit at a time"
            );
            let reader = self
                .readback
                .get_or_insert_with(|| Readback::new(&mut self.local, (ROOT_WIDTH, ROOT_HEIGHT)));
            let pixels = reader.pixels(&mut self.local, gl);
            assert_eq!(snapshot.0, Some(seq));
            let popup = pixels[((7 * ROOT_WIDTH + 8) * 4 + 1) as usize] >= 230;
            let selected = pixels[((19 * ROOT_WIDTH + 25) * 4) as usize] >= 230;
            assert_family_pixels(&pixels, popup, selected);
            self.last_visual = Some((popup, selected));
            present(&mut self.local, std::mem::take(&mut self.feedback));
            self.last_presented = Some(seq);
        }
    }
    fn wait(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        label: &str,
        display: Option<(bool, bool)>,
        mut done: impl FnMut(&Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.tick(gl, display);
            if let Ok(result) = self.client_done.try_recv() {
                panic!("{label}: client ended: {result:?}");
            }
            if let Ok(result) = self.host_done.try_recv() {
                panic!("{label}: host ended: {result:?}");
            }
            assert!(Instant::now() < deadline, "{label}: deadline");
            if done(self) {
                break;
            }
            thread::yield_now();
        }
    }
    fn finish(self, gl: &mut HeadlessSmithayGles) {
        self.finish_with_ack_barrier(gl, None);
    }
    fn finish_with_ack_barrier(
        mut self,
        gl: &mut HeadlessSmithayGles,
        barrier: Option<&PopupCloseBarrier>,
    ) {
        let binding = self.popup_host_lifetime.lock().unwrap().unwrap().binding;
        let mut released = false;
        self.local
            .state
            .toplevels
            .values()
            .next()
            .unwrap()
            .send_close();
        self.local.roundtrip();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.tick(gl, None);
            if let Some(barrier) = barrier {
                if !released {
                    let confirmed = barrier.events.lock().unwrap().iter().find_map(|e| match e {
                        host::popup::PopupHostTestEvent::NativeCleanupConfirmed {
                            binding,
                            at,
                            deadline,
                        } => Some((*binding, *at, *deadline)),
                        _ => None,
                    });
                    if let Some((observed, at, native_deadline)) = confirmed {
                        assert_eq!(observed, binding);
                        assert!(at < native_deadline && Instant::now() < native_deadline);
                        assert!(
                            !remote_popup::active(&self.native.state),
                            "native destroy sync must precede ACK"
                        );
                        assert!(
                            matches!(self.client_done.try_recv(), Err(mpsc::TryRecvError::Empty)),
                            "client cannot complete close before real RevokeAck"
                        );
                        assert!(!barrier.events.lock().unwrap().iter().any(|e| matches!(
                            e,
                            host::popup::PopupHostTestEvent::AckWritten { .. }
                        )));
                        barrier.release.send(()).unwrap();
                        released = true;
                    }
                }
            }
            match self.client_done.try_recv() {
                Ok(result) => {
                    assert!(
                        barrier.is_none() || released,
                        "ACK barrier was not exercised"
                    );
                    result.expect("normal viewer close");
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("popup client worker lost result"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "client close deadline");
        }
        // Let the host consume the client's queued Revoke and retire native
        // authority. Setting the emergency stop here would race that dequeue.
        let mut host_result = None;
        loop {
            self.tick(gl, None);
            if host_result.is_none() {
                host_result = self.host_done.try_recv().ok();
            }
            if host_result.is_some() && !remote_popup::active(&self.native.state) {
                break;
            }
            assert!(Instant::now() < deadline, "native popup cleanup deadline");
        }
        host_result
            .unwrap()
            .expect("normal authenticated host teardown");
        if let Some(barrier) = barrier {
            let events = barrier.events.lock().unwrap();
            let confirmations: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    host::popup::PopupHostTestEvent::NativeCleanupConfirmed {
                        binding,
                        at,
                        deadline,
                    } => Some((*binding, *at, *deadline)),
                    _ => None,
                })
                .collect();
            let acknowledgements: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    host::popup::PopupHostTestEvent::AckWritten { binding, at } => {
                        Some((*binding, *at))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(confirmations.len(), 1);
            assert_eq!(acknowledgements.len(), 1);
            assert_eq!(acknowledgements[0].0, binding);
            assert!(acknowledgements[0].1 >= confirmations[0].1);
            assert!(acknowledgements[0].1 < confirmations[0].2);
        }
        self.shutdown.store(true, Ordering::Release);
        self.client.take().unwrap().join().unwrap();
        self.host.take().unwrap().join().unwrap();
    }
    fn click(&mut self, x: f64, y: f64) {
        let p = self
            .input
            .as_ref()
            .expect("actual viewer wl_pointer binding");
        p.motion(10, x, y);
        p.frame();
        p.button(11, 11, 0x110, ButtonState::Pressed);
        p.frame();
        p.button(12, 12, 0x110, ButtonState::Released);
        p.frame();
        self.local.roundtrip();
    }
}
impl Drop for PopupHarness {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Threads own their network/Wayland transports; dropping the owned
        // servers disconnects them. Runtime success paths explicitly join.
        let _ = self.client.take();
        let _ = self.host.take();
    }
}
#[test]
fn authenticated_popup_click_requires_arm_new_proof_and_actual_family_presentation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let barrier = PopupCloseBarrier::new();
    let mut h = PopupHarness::configured_with_schedule(
        &mut gl,
        true,
        true,
        None,
        None,
        Some(barrier.hook.clone()),
    );
    h.wait(&mut gl, "initial real commit withheld", None, |s| {
        s.private_pointer.is_some() && s.input.is_some() && !s.feedback.is_empty()
    });
    assert!(remote_popup::active(&h.native.state));
    assert!(
        h.private_events.is_empty(),
        "unpresented first frame must not Arm or deliver input"
    );
    assert!(h.ready.lock().unwrap().is_none());
    // A frame callback is not presentation. No synthetic ACK or native grant.
    let surface = h
        .local
        .state
        .toplevels
        .values()
        .next()
        .unwrap()
        .wl_surface()
        .clone();
    frame_callback_only(&surface);
    h.tick(&mut gl, None);
    assert!(h.private_events.is_empty());
    let initial = h.frames.lock().unwrap().0.unwrap();
    h.tick(&mut gl, Some((false, false)));
    h.wait(
        &mut gl,
        "Arm completed but next proof not presented",
        None,
        |s| {
            s.private_events.iter().any(|e| e.0 == 0)
                && !s.feedback.is_empty()
                && s.frames.lock().unwrap().0.is_some_and(|n| n > initial)
        },
    );
    assert!(h.ready.lock().unwrap().is_none());
    h.click(8.0, 8.0);
    h.wait(
        &mut gl,
        "Waiting click actually dispatched without authorization",
        None,
        |s| *s.dispatched.lock().unwrap() == (1, 1),
    );
    assert!(h.private_events.iter().all(|e| matches!(e.0, 0 | 2 | 5)));
    h.wait(
        &mut gl,
        "real Arm and distinct actually presented Ready",
        Some((false, false)),
        |s| s.ready.lock().unwrap().is_some() && s.private_events.iter().any(|e| e.0 == 0),
    );
    let before_root = h.ready.lock().unwrap().unwrap();
    assert!(
        before_root > initial,
        "Arm consumes the first presented proof"
    );
    assert_eq!(
        *h.frames.lock().unwrap(),
        (Some(before_root), Some(before_root))
    );
    assert!(
        h.private_events.iter().all(|e| matches!(e.0, 0 | 2 | 5)),
        "Arm has no button/axis"
    );
    let before_actions = h.private_events.len();
    h.click(8.0, 8.0);
    h.wait(
        &mut gl,
        "root atomic click actual receiver",
        Some((false, false)),
        |s| {
            s.private_events[before_actions..]
                .iter()
                .filter(|e| e.0 == 3)
                .count()
                == 2
        },
    );
    let buttons: Vec<_> = h.private_events[before_actions..]
        .iter()
        .filter(|e| e.0 == 3)
        .collect();
    assert_eq!(&buttons[0].1[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].1[2..], &[0x110, 0]);
    let serial = buttons[0].1[0];
    let popup_surface = direct_popup(
        &mut h.native,
        &mut h.target,
        h.private_seat.unwrap(),
        serial,
        h.main_pointer,
        h.main_keyboard,
        h.private_pointer.unwrap(),
    );
    h.wait(
        &mut gl,
        "direct popup pixels and new exact presented Ready",
        Some((true, false)),
        |s| {
            s.last_visual == Some((true, false))
                && s.ready.lock().unwrap().is_some_and(|n| {
                    n > before_root
                        && s.last_presented == Some(n)
                        && s.frames.lock().unwrap().1 == Some(n)
                })
        },
    );
    let before_popup = h.private_events.len();
    h.click(7.0, 8.0);
    h.wait(
        &mut gl,
        "popup-local atomic click actual receiver",
        Some((true, false)),
        |s| {
            s.private_events[before_popup..]
                .iter()
                .filter(|e| e.0 == 3)
                .count()
                == 2
        },
    );
    let delivered = &h.private_events[before_popup..];
    assert!(
        delivered
            .iter()
            .any(|e| e.0 == 2 && e.1[1..] == [3 * 256, 5 * 256])
    );
    let buttons: Vec<_> = delivered.iter().filter(|e| e.0 == 3).collect();
    assert_eq!(&buttons[0].1[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].1[2..], &[0x110, 0]);
    // Application callback occurs only after its exact real popup receiver got
    // the balanced click, then produces new pixels through the same TCP path.
    let (blue, _file) = shm(
        &mut h.native,
        &mut h.target.client,
        [255, 0, 0, 255],
        ROOT_WIDTH,
        ROOT_HEIGHT,
    );
    h.target.client.request(h.target.surface, 1, &[blue, 0, 0]);
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, ROOT_WIDTH, ROOT_HEIGHT]);
    h.target.client.request(h.target.surface, 6, &[]);
    assert_ne!(popup_surface, h.target.surface);
    h.native.roundtrip();
    h.wait(
        &mut gl,
        "application selection repaint captured and presented",
        Some((true, true)),
        |s| s.last_visual == Some((true, true)) && s.frames.lock().unwrap().1 == s.last_presented,
    );
    h.finish_with_ack_barrier(&mut gl, Some(&barrier));
}

#[test]
fn authenticated_popup_requires_both_explicit_permissions_without_private_seat() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for (host_permission, client_permission) in [(false, true), (true, false)] {
        let mut h = PopupHarness::new(&mut gl, host_permission, client_permission);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client_result = None;
        let mut host_result = None;
        loop {
            h.tick(&mut gl, None);
            if client_result.is_none() {
                client_result = h.client_done.try_recv().ok();
            }
            if host_result.is_none() {
                host_result = h.host_done.try_recv().ok();
            }
            assert!(h.private_events.is_empty());
            assert!(h.private_pointer.is_none());
            assert!(!remote_popup::active(&h.native.state));
            assert!(
                Instant::now() < deadline,
                "explicit permission refusal deadline"
            );
            if client_result.is_some() && host_result.is_some() {
                break;
            }
        }
        let error = host_result
            .unwrap()
            .expect_err("one-sided popup permission must reject");
        let io = error
            .downcast_ref::<std::io::Error>()
            .expect("typed permission refusal");
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            io.to_string(),
            "popup pointer permission was not mutually negotiated exclusively"
        );
        let error = client_result
            .unwrap()
            .expect_err("viewer must receive negotiated refusal");
        let diagnostic = crate::remote::client::classify_failure(error.as_ref());
        assert_eq!(
            diagnostic.peer_close_code,
            Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
        );
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
    }
}

#[test]
fn authenticated_popup_unpresented_proof_expires_without_native_arm() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = PopupHarness::new(&mut gl, true, true);
    h.wait(
        &mut gl,
        "initial committed proof without feedback",
        None,
        |s| s.private_pointer.is_some() && !s.feedback.is_empty(),
    );
    let (_, _, native_deadline) = remote_popup::lease_timing_for_test(&h.native.state).unwrap();
    // Read back the committed viewer image but deliberately do not send actual
    // presentation feedback. The host must expire this proof, not wait for TTL.
    let frames = h.frames.clone();
    {
        let commit = frames.lock().unwrap();
        assert!(commit.0.is_some() && commit.1.is_none());
        let reader = h
            .readback
            .get_or_insert_with(|| Readback::new(&mut h.local, (ROOT_WIDTH, ROOT_HEIGHT)));
        let pixels = reader.pixels(&mut h.local, &mut gl);
        assert_family_pixels(&pixels, false, false);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client_result = None;
    let mut host_result = None;
    loop {
        h.tick(&mut gl, None);
        if client_result.is_none() {
            client_result = h.client_done.try_recv().ok();
        }
        if host_result.is_none() {
            host_result = h.host_done.try_recv().ok();
        }
        assert!(
            h.private_events.is_empty(),
            "unpresented expired proof must not Arm"
        );
        assert!(Instant::now() < deadline && Instant::now() < native_deadline);
        if client_result.is_some()
            && host_result.is_some()
            && !remote_popup::active(&h.native.state)
        {
            break;
        }
    }
    let error = host_result
        .unwrap()
        .expect_err("unpresented proof must expire before the lease");
    assert_eq!(
        error.downcast_ref::<crate::remote::wayland_popup_wire::Failure>(),
        Some(&crate::remote::wayland_popup_wire::Failure::Stale),
        "unexpected host termination: {error:?}; client result: {client_result:?}"
    );
    let error = client_result
        .unwrap()
        .expect_err("failed session must terminate viewer");
    let diagnostic = crate::remote::client::classify_failure(error.as_ref());
    assert_eq!(
        diagnostic.peer_close_code,
        Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
    );
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

#[derive(Clone, Copy, Debug)]
enum RawCase {
    FutureAck,
    OldAck,
}
fn raw_peer(
    address: std::net::SocketAddr,
    case: RawCase,
    bound: Arc<AtomicBool>,
) -> crate::remote::RemoteResult<()> {
    use crate::remote::{
        messages::{ClientHello, StreamCapabilities, encode_source_frame_ack},
        protocol::{MessageKind, SessionReader, SessionWriter, client_handshake},
        source_wire::{FramePurpose, decode_frame_payload},
        wayland_popup_wire::{METADATA_LEN, Metadata},
        x11_keymap::KeymapTable,
    };
    let mut socket = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    let (receive, send) = client_handshake(&mut socket, &KEY, [0x43; 32])?.into_client();
    let mut reader = SessionReader::new(socket.try_clone()?, receive);
    let mut writer = SessionWriter::new(socket, send);
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            stream: StreamCapabilities {
                wayland_popup_pointer: true,
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
    let mut initial = None;
    let mut stimulus = false;
    let mut decoder = crate::remote::tile::TileDecoder::new();
    loop {
        let (kind, body) = reader.read_message()?;
        match kind {
            MessageKind::SessionClose => {
                assert!(stimulus, "typed close before tested stimulus");
                let close = crate::remote::close_wire::SessionClose::decode(&body)?;
                assert_eq!(
                    close.code,
                    crate::remote::close_wire::SessionCloseCode::CaptureUnavailable
                );
                return Ok(());
            }
            MessageKind::HelloAck
            | MessageKind::SourceCatalog
            | MessageKind::WaylandPopupStatus => {}
            MessageKind::Frame => {
                let (source, purpose, inner) = decode_frame_payload(&body)?;
                assert_eq!(purpose, FramePurpose::PopupFamilyProof);
                let meta = Metadata::decode(&inner[..METADATA_LEN])?;
                let frame = decoder.decode_into(
                    &inner[METADATA_LEN..],
                    crate::remote::frame::new_decode_buffer_pool(),
                )?;
                assert_eq!(frame.sequence(), meta.key.wire_sequence);
                assert_eq!(frame.dimensions(), (ROOT_WIDTH, ROOT_HEIGHT));
                let seq = meta.key.wire_sequence;
                let bound_deadline = Instant::now() + Duration::from_secs(1);
                while !bound.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < bound_deadline,
                        "real private pointer must be bound before raw claim"
                    );
                    thread::yield_now();
                }
                match case {
                    RawCase::FutureAck => {
                        assert!(initial.is_none());
                        writer.write_message(
                            MessageKind::FrameAck,
                            &encode_source_frame_ack(source, seq.checked_add(1).unwrap())?,
                        )?;
                        stimulus = true;
                    }
                    RawCase::OldAck => {
                        if let Some(first) = initial {
                            assert!(seq > first);
                            writer.write_message(
                                MessageKind::FrameAck,
                                &encode_source_frame_ack(source, first)?,
                            )?;
                            stimulus = true;
                        } else {
                            // This raw peer is explicitly an adversarial display
                            // claimant, not evidence of actual viewer presentation.
                            writer.write_message(
                                MessageKind::FrameAck,
                                &encode_source_frame_ack(source, seq)?,
                            )?;
                        }
                    }
                }
                initial = Some(seq);
                writer.flush()?;
            }
            other => panic!("unexpected authenticated raw popup message: {other:?}"),
        }
    }
}

#[test]
fn authenticated_popup_future_and_consumed_cumulative_ack_are_not_new_authority() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in [RawCase::FutureAck, RawCase::OldAck] {
        let mut h = PopupHarness::new_peer(&mut gl, true, true, Some(case));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client_result = None;
        let mut host_result = None;
        loop {
            h.tick(&mut gl, None);
            if client_result.is_none() {
                client_result = h.client_done.try_recv().ok();
            }
            if host_result.is_none() {
                host_result = h.host_done.try_recv().ok();
            }
            assert!(
                !h.private_events
                    .iter()
                    .any(|e| matches!(e.0, 3 | 4 | 6 | 7 | 8 | 9)),
                "invalid ACK cannot deliver button/axis"
            );
            if matches!(case, RawCase::FutureAck) {
                assert!(h.private_events.is_empty());
            }
            assert!(
                Instant::now() < deadline,
                "raw bad ACK rejection deadline {case:?}"
            );
            if client_result.is_some()
                && host_result.is_some()
                && !remote_popup::active(&h.native.state)
            {
                break;
            }
        }
        let e = host_result
            .unwrap()
            .expect_err("non-exact ACK must be terminal");
        assert_eq!(
            e.downcast_ref::<crate::remote::wayland_popup_wire::Failure>(),
            Some(&crate::remote::wayland_popup_wire::Failure::Identity)
        );
        client_result.unwrap().unwrap();
        if matches!(case, RawCase::OldAck) {
            assert_eq!(
                h.private_events.iter().filter(|e| e.0 == 0).count(),
                1,
                "only the valid first proof Arm may enter"
            );
        }
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
    }
}

#[derive(Clone, Copy, Debug)]
enum PreparedChange {
    Unmap,
    Lock,
}
#[test]
fn authenticated_popup_prepared_source_cannot_revive_across_prehello_lifecycle_aba() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for change in [PreparedChange::Unmap, PreparedChange::Lock] {
        let mut h = PopupHarness::configured(&mut gl, true, true, None, Some(change));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client_result = None;
        let mut host_result = None;
        loop {
            h.tick(&mut gl, None);
            if client_result.is_none() {
                client_result = h.client_done.try_recv().ok();
            }
            if host_result.is_none() {
                host_result = h.host_done.try_recv().ok();
            }
            assert!(
                h.private_pointer.is_none(),
                "old prepared source must not produce a new seat"
            );
            assert!(h.private_events.is_empty());
            assert!(!remote_popup::active(&h.native.state));
            assert!(
                Instant::now() < deadline,
                "pre-Hello ABA rejection {change:?}"
            );
            if client_result.is_some() && host_result.is_some() {
                break;
            }
        }
        let e = host_result
            .unwrap()
            .expect_err("revoked original source must not be reselected");
        assert_eq!(e.to_string(), "interactive popup lease revoked");
        let error = client_result
            .unwrap()
            .expect_err("refused original source must terminate negotiation");
        let diagnostic = crate::remote::client::classify_failure(error.as_ref());
        assert_eq!(
            diagnostic.peer_close_code,
            Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
        );
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
    }
}

#[test]
fn authenticated_popup_lock_or_root_unmap_ends_current_lease_without_new_input() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock_case in [true, false] {
        let mut h = PopupHarness::new(&mut gl, true, true);
        h.wait(
            &mut gl,
            "actual initial and post-arm presentations",
            Some((false, false)),
            |s| s.ready.lock().unwrap().is_some(),
        );
        assert!(!h.private_events.iter().any(|e| e.0 == 3));
        let mut locker = None;
        if lock_case {
            h.native.state.session_lock_supported = true;
            let mut c = h.native.connect();
            let manager = c.bind("ext_session_lock_manager_v1", 1);
            let lock = c.new_id();
            c.request(manager, 1, &[lock]);
            assert!(
                fence(&mut h.native, &mut c)
                    .iter()
                    .any(|e| e.sender == lock && e.opcode == 0)
            );
            locker = Some(c);
        } else {
            h.target.client.request(h.target.surface, 1, &[0, 0, 0]);
            h.target.client.request(h.target.surface, 6, &[]);
        }
        let lifecycle = fence(&mut h.native, &mut h.target.client);
        let main: Vec<_> = lifecycle
            .iter()
            .filter(|e| e.sender == h.main_pointer)
            .collect();
        if lock_case {
            // The trusted local lock entry restores then clears its own main
            // pointer focus. This exact synchronous sequence is not remote input.
            assert_eq!(
                main.iter().map(|e| e.opcode).collect::<Vec<_>>(),
                vec![2, 1, 5]
            );
            assert_eq!(main[0].args, vec![0, 5 * 256, 5 * 256]);
            assert_eq!(main[1].args.get(1), Some(&h.target.surface));
        } else {
            assert!(
                main.is_empty()
                    || (main.len() == 2
                        && main[0].opcode == 1
                        && main[0].args.get(1) == Some(&h.target.surface)
                        && main[1].opcode == 5),
                "unmapping may only clear the old main focus"
            );
            assert!(!h.native.state.mapped_windows.contains(&h.target.window));
        }
        let keyboard: Vec<_> = lifecycle
            .iter()
            .filter(|e| e.sender == h.main_keyboard)
            .collect();
        assert!(
            keyboard.len() <= 1,
            "lifecycle cannot duplicate keyboard cleanup"
        );
        if lock_case {
            assert_eq!(
                keyboard.len(),
                1,
                "local lock clears the focused main keyboard"
            );
        }
        for e in keyboard {
            assert_eq!(
                e.opcode, 2,
                "lifecycle may only leave the old keyboard focus, never send key/modifiers/keymap/repeat/enter"
            );
            assert_eq!(e.args.len(), 2);
            assert_eq!(
                e.args[1], h.target.surface,
                "keyboard leave must name the original root"
            );
        }
        for e in lifecycle {
            if Some(e.sender) == h.private_pointer {
                assert!(
                    matches!(e.opcode, 1 | 5),
                    "lifecycle cleanup cannot inject private input"
                );
                h.private_events.push((e.opcode, e.args));
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client_result = None;
        let mut host_result = None;
        loop {
            h.tick(&mut gl, None);
            if client_result.is_none() {
                client_result = h.client_done.try_recv().ok();
            }
            if host_result.is_none() {
                host_result = h.host_done.try_recv().ok();
            }
            assert!(
                !h.private_events
                    .iter()
                    .any(|e| matches!(e.0, 3 | 4 | 6 | 7 | 8 | 9))
            );
            assert!(Instant::now() < deadline, "lifecycle closure deadline");
            if client_result.is_some()
                && host_result.is_some()
                && !remote_popup::active(&h.native.state)
            {
                break;
            }
        }
        let e = host_result
            .unwrap()
            .expect_err("live source loss must close popup session");
        assert!(
            matches!(
                e.to_string().as_str(),
                "interactive popup lease revoked" | "Wayland image capture session stopped"
            ),
            "wrong lifecycle failure: {e}"
        );
        let error = client_result
            .unwrap()
            .expect_err("host lifecycle close must end viewer");
        let diagnostic = crate::remote::client::classify_failure(error.as_ref());
        assert_eq!(
            diagnostic.peer_close_code,
            Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
        );
        h.client.take().unwrap().join().unwrap();
        h.host.take().unwrap().join().unwrap();
        drop(locker);
    }
}

#[test]
fn authenticated_popup_periodic_real_presentation_does_not_renew_original_lease() {
    use crate::remote::client::PopupLifetimeFailureReason;
    use crate::remote::protocol::ProtocolError;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = PopupHarness::new(&mut gl, true, true);
    h.wait(
        &mut gl,
        "initial and post-arm real presentation",
        Some((false, false)),
        |s| s.ready.lock().unwrap().is_some(),
    );
    let native_original = remote_popup::lease_timing_for_test(&h.native.state)
        .expect("actual native lease must already exist");
    let host_original = h
        .popup_host_lifetime
        .lock()
        .unwrap()
        .expect("actual host lease clock");
    let (requested, fixed_deadline) = {
        let observed = h.popup_client_lifetime.lock().unwrap();
        assert!(observed.ttl_fixed);
        assert!(observed.first_failure.is_none());
        (observed.requested_at.unwrap(), observed.deadline.unwrap())
    };
    assert_eq!(
        host_original.deadline.duration_since(host_original.created),
        Duration::from_secs(30)
    );
    assert_eq!(
        native_original.2.duration_since(native_original.1),
        Duration::from_secs(30)
    );
    assert_eq!(host_original.binding.lease_epoch, native_original.0);
    assert!(requested <= host_original.created && host_original.created <= native_original.1);
    assert!(fixed_deadline > requested && fixed_deadline <= requested + Duration::from_secs(30));
    // The first server TTL is conservatively anchored to the original client
    // request, not its later receipt. This case deliberately verifies that
    // actual earlier client limit; it does not pretend the native timer fired.
    assert!(fixed_deadline < host_original.deadline && host_original.deadline <= native_original.2);
    let midpoint = requested + fixed_deadline.duration_since(requested) / 2;
    let overall_deadline = fixed_deadline + Duration::from_secs(5);
    let mut ready_observations: Vec<(Instant, u64)> = Vec::new();
    let mut last_ready = None;
    let mut terminal_tail: Option<(Instant, u64, Instant)> = None;
    let mut client_result = None;
    let mut host_result = None;
    loop {
        // Once an exact acknowledged, actually displayed proof covers the
        // fixed expiry, stop expensive GL sampling during the final half-second.
        // Event pumping continues; no clock/deadline/lease is changed. This also
        // avoids reading a viewer surface concurrently with its natural teardown.
        h.tick(
            &mut gl,
            if terminal_tail.is_none() {
                Some((false, false))
            } else {
                None
            },
        );
        if client_result.is_none() {
            client_result = h.client_done.try_recv().ok();
        }
        if host_result.is_none() {
            host_result = h.host_done.try_recv().ok();
        }
        let now = Instant::now();
        assert!(
            now < overall_deadline,
            "original deadline plus five seconds cleanup only"
        );
        let host = h
            .popup_host_lifetime
            .lock()
            .unwrap()
            .expect("host observation persists");
        assert_eq!(
            (host.created, host.deadline, host.binding),
            (
                host_original.created,
                host_original.deadline,
                host_original.binding
            ),
            "host must never renew identity or lifetime"
        );
        let (failure, observed_deadline) = {
            let observed = h.popup_client_lifetime.lock().unwrap();
            assert_eq!(observed.requested_at, Some(requested));
            assert!(observed.ttl_fixed);
            (observed.first_failure, observed.deadline)
        };
        assert_eq!(
            observed_deadline,
            Some(fixed_deadline),
            "refresh statuses must not renew the client deadline"
        );
        if let Some(native) = remote_popup::lease_timing_for_test(&h.native.state) {
            assert_eq!(
                native, native_original,
                "same real native lease throughout refreshes"
            );
        } else {
            let failure = failure.unwrap_or_else(|| panic!(
                "native ended before actual client deadline check: client={client_result:?}; host={host_result:?}; host observation={host:?}"
            ));
            assert!(failure.at >= fixed_deadline);
        }
        if let Some(sequence) = *h.ready.lock().unwrap() {
            let frames = *h.frames.lock().unwrap();
            if frames.1 == Some(sequence)
                && h.last_presented == Some(sequence)
                && last_ready.is_none_or(|old| sequence > old)
            {
                assert_eq!(h.last_visual, Some((false, false)));
                ready_observations.push((now, sequence));
                last_ready = Some(sequence);
                if terminal_tail.is_none()
                    && now < fixed_deadline
                    && fixed_deadline.duration_since(now) <= Duration::from_millis(500)
                    && host.current_acked
                    && host.current_sequence == Some(sequence)
                {
                    let captured_at = host
                        .current_captured_at
                        .expect("ACK must name a real retained proof");
                    if captured_at + Duration::from_secs(1) > fixed_deadline {
                        terminal_tail = Some((now, sequence, captured_at));
                    }
                }
            }
        }
        assert!(
            !h.private_events
                .iter()
                .any(|e| matches!(e.0, 3 | 4 | 6 | 7 | 8 | 9)),
            "refresh and expiry must not inject buttons or axes"
        );
        if client_result.is_some() || host_result.is_some() {
            let failure = failure.unwrap_or_else(|| panic!(
                "session ended without actual client deadline check: client={client_result:?}; host={host_result:?}; host observation={host:?}"
            ));
            assert_eq!(failure.reason, PopupLifetimeFailureReason::LeaseExpired);
            assert!(!failure.ended);
            assert_eq!(
                failure.pending_deadline, None,
                "no outstanding action can masquerade as TTL"
            );
            assert_eq!(failure.deadline, Some(fixed_deadline));
            assert!(failure.at >= fixed_deadline);
            assert!(
                terminal_tail.is_some(),
                "must have fresh actual Ready evidence near the original deadline"
            );
        }
        // tick drains recipient events before dispatching native teardown.
        // A just-revoked lease can therefore be inactive one iteration before
        // its wl_pointer.leave is observed. Wait for actual recipient cleanup,
        // within the existing overall deadline, not merely native inactivity.
        if client_result.is_some()
            && host_result.is_some()
            && !remote_popup::active(&h.native.state)
            && h.private_events.iter().any(|event| event.0 == 1)
        {
            break;
        }
        thread::yield_now();
    }
    assert!(
        ready_observations.len() >= 3,
        "must actually refresh, not wait on one frame"
    );
    assert!(
        ready_observations
            .windows(2)
            .all(|pair| pair[1].1 > pair[0].1)
    );
    assert!(ready_observations.iter().any(|(at, _)| *at >= midpoint));
    let (tail_at, tail_sequence, tail_capture) = terminal_tail.unwrap();
    assert!(tail_at < fixed_deadline && tail_capture + Duration::from_secs(1) > fixed_deadline);
    assert!(
        tail_at.duration_since(ready_observations[0].0) >= Duration::from_secs(1),
        "separate periodic refreshes, not one startup batch"
    );
    let failed = h
        .popup_client_lifetime
        .lock()
        .unwrap()
        .first_failure
        .unwrap();
    let error = client_result
        .unwrap()
        .expect_err("fixed original client lifetime is terminal");
    let diagnostic = crate::remote::client::classify_failure(error.as_ref());
    assert_ne!(
        diagnostic.cleanup, "incomplete",
        "expiry must not conceal a cleanup failure"
    );
    let expiry = error
        .source()
        .and_then(|cause| cause.downcast_ref::<std::io::Error>())
        .expect("ClientFailure must directly wrap the original expiry, not a cleanup wrapper");
    assert_eq!(expiry.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(expiry.to_string(), "popup pointer session ended or expired");
    let final_host = h.popup_host_lifetime.lock().unwrap().unwrap();
    assert!(final_host.exited_at.expect("actual host exit") >= failed.at);
    if let Some(captured) = final_host.current_captured_at {
        assert!(
            final_host
                .current_sequence
                .is_some_and(|sequence| sequence >= tail_sequence),
            "host exit proof must not precede the actual displayed tail"
        );
        assert!(
            captured >= tail_capture && captured + Duration::from_secs(1) > fixed_deadline,
            "a prior expired proof cannot be counted as natural TTL"
        );
    }
    if final_host.current_captured_at.is_none() {
        assert!(final_host.current_sequence.is_none());
    }
    // None here means a retired slot between captures, never proof of freshness.
    // The positive freshness evidence is the Some+ACK+actual Ready tail above.
    let error = host_result
        .unwrap()
        .expect_err("client deadline closes its authenticated transport");
    // Client expiry closes both halves of TCP. The reader may publish EOF
    // first, or a concurrent frame/status write may observe the same close.
    // All positive TTL/fresh-presentation/cleanup evidence above is required
    // before classifying this host-side transport result.
    let transport_closed = |error: &(dyn std::error::Error + 'static)| {
        if let Some(ProtocolError::Io(io)) = error.downcast_ref::<ProtocolError>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            );
        }
        error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            io.kind() == std::io::ErrorKind::InvalidData
                && matches!(
                    io.to_string().as_str(),
                    "popup transport closed" | "popup reader ended"
                )
        })
    };
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
    ] {
        assert!(transport_closed(&ProtocolError::Io(std::io::Error::from(
            kind
        ))));
    }
    for kind in [
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::InvalidData,
    ] {
        assert!(!transport_closed(&ProtocolError::Io(std::io::Error::from(
            kind
        ))));
    }
    assert!(!transport_closed(&ProtocolError::AuthenticationFailed));
    assert!(!transport_closed(&std::io::Error::other(
        "popup reader ended"
    )));
    assert!(!transport_closed(&std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "Stale"
    )));
    assert!(
        transport_closed(error.as_ref()),
        "unexpected host termination (especially Stale is not TTL): {error}"
    );
    assert_eq!(
        h.private_events.iter().filter(|e| e.0 == 0).count(),
        1,
        "one native Arm only"
    );
    assert_eq!(
        h.private_events.iter().filter(|e| e.0 == 1).count(),
        1,
        "exact private focus cleanup"
    );
    assert_eq!(
        h.private_events.iter().filter(|e| e.0 == 2).count(),
        1,
        "only Arm's initial motion"
    );
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

// These barriers only pause an already selected production scheduling point.
// The input, queue, refresh, rejection, capture and presentation remain real.
struct PopupSchedule {
    events: Arc<Mutex<Vec<host::popup::PopupHostTestEvent>>>,
    release: mpsc::SyncSender<()>,
    hook: Arc<dyn Fn(host::popup::PopupHostTestEvent) + Send + Sync>,
}
impl PopupSchedule {
    fn new(refresh_first: bool) -> Self {
        use host::popup::PopupHostTestEvent as E;
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let paused = AtomicBool::new(false);
        let (release, resume) = mpsc::sync_channel(1);
        let resume = Mutex::new(resume);
        let hook = Arc::new(move |event| {
            let stop = if refresh_first {
                matches!(event, E::BeforeIdleRefresh(_))
            } else {
                matches!(event, E::ReadyPublished(_))
            } && !paused.swap(true, Ordering::AcqRel);
            {
                let mut events = recorded.lock().unwrap();
                assert!(events.len() < 512, "bounded scheduling observations");
                events.push(event);
            }
            if stop || (refresh_first && matches!(event, E::RejectedStale { .. })) {
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test must release real host scheduling barrier");
            }
        });
        Self {
            events,
            release,
            hook,
        }
    }
    fn snapshot(&self) -> Vec<host::popup::PopupHostTestEvent> {
        self.events.lock().unwrap().clone()
    }
}

fn popup_refresh_click_order(refresh_first: bool) {
    use host::popup::PopupHostTestEvent as E;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let schedule = PopupSchedule::new(refresh_first);
    let mut h = PopupHarness::configured_with_schedule(
        &mut gl,
        true,
        true,
        None,
        None,
        Some(schedule.hook.clone()),
    );
    h.wait(
        &mut gl,
        "real Ready and chosen host scheduling barrier",
        Some((false, false)),
        |s| {
            s.ready.lock().unwrap().is_some_and(|sequence| {
                s.last_presented == Some(sequence)
                    && s.frames.lock().unwrap().1 == Some(sequence)
                    && schedule.snapshot().iter().any(|event| match event {
                        E::BeforeIdleRefresh(key) if refresh_first => key.wire_sequence == sequence,
                        E::ReadyPublished(key) if !refresh_first => key.wire_sequence == sequence,
                        _ => false,
                    })
            })
        },
    );
    let old_sequence = h.ready.lock().unwrap().unwrap();
    let native_lifetime = remote_popup::lease_timing_for_test(&h.native.state).unwrap();
    let client_deadline = h.popup_client_lifetime.lock().unwrap().deadline.unwrap();
    let host_lifetime = h.popup_host_lifetime.lock().unwrap().unwrap();
    assert!(h.private_events.iter().all(|e| matches!(e.0, 0 | 2 | 5)));
    let before = h.private_events.len();
    h.click(8.0, 8.0);
    h.wait(
        &mut gl,
        "actual old Ready click queued while host paused",
        None,
        |_| {
            schedule.snapshot().iter().any(|e| {
                matches!(e,
            E::ClickQueued { key, sequence: 1 } if key.wire_sequence == old_sequence)
            })
        },
    );
    assert_eq!(*h.dispatched.lock().unwrap(), (1, 1));
    assert_eq!(h.private_events.len(), before);
    schedule.release.send(()).unwrap();

    if refresh_first {
        h.wait(
            &mut gl,
            "exact stale rejection and unpresented replacement",
            None,
            |s| {
                s.popup_client_lifetime
                    .lock()
                    .unwrap()
                    .last_rejected
                    .is_some_and(|(key, sequence)| {
                        key.wire_sequence == old_sequence && sequence == 1
                    })
                    && !s.feedback.is_empty()
                    && s.frames.lock().unwrap().0.is_some_and(|n| n > old_sequence)
            },
        );
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(
            h.private_events.len(),
            before,
            "rejected click never reaches native"
        );
        let events = schedule.snapshot();
        let (key, dequeued) = events
            .iter()
            .find_map(|event| match event {
                E::ClickDequeued {
                    key,
                    sequence: 1,
                    at,
                } => Some((*key, *at)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            h.popup_client_lifetime.lock().unwrap().last_rejected,
            Some((key, 1))
        );
        let rejected = events
            .iter()
            .find_map(|event| match event {
                E::RejectedStale {
                    key: rejected,
                    sequence: 1,
                    at,
                } if *rejected == key => Some(*at),
                _ => None,
            })
            .unwrap();
        assert!(rejected >= dequeued);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, E::Accepted { .. }))
        );
        // The host already sent the intermediate refresh frame before it
        // dequeued this click. Present that real frame while the rejection
        // hook holds the next capture back; its ACK may drain credit only.
        let intermediate = h.frames.lock().unwrap().0.unwrap();
        h.tick(&mut gl, Some((false, false)));
        h.wait(
            &mut gl,
            "discarded intermediate frame actually presented",
            None,
            |s| s.frames.lock().unwrap().1 == Some(intermediate),
        );
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.private_events.len(), before);
        schedule.release.send(()).unwrap();
        h.wait(
            &mut gl,
            "post-rejection capture committed but not presented",
            None,
            |s| {
                !s.feedback.is_empty()
                    && s.frames.lock().unwrap().0.is_some_and(|n| n > intermediate)
            },
        );
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.private_events.len(), before);
        // A presentation observation alone does not prove transmission:
        // Sender can coalesce unsent FrameAck records. Keep N+1 unpresented
        // until the host actually consumes N through its discarded-ACK slot.
        h.wait(
            &mut gl,
            "exact intermediate ACK consumed without authorization",
            None,
            |_| {
                schedule.snapshot().iter().any(|event| {
                    matches!(event,
                E::DiscardedAck { sequence, .. } if *sequence == intermediate)
                })
            },
        );
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.private_events.len(), before);
        assert_eq!(
            remote_popup::lease_timing_for_test(&h.native.state),
            Some(native_lifetime)
        );
        assert_eq!(
            h.popup_client_lifetime.lock().unwrap().deadline,
            Some(client_deadline)
        );
        let ack_snapshot = h.popup_host_lifetime.lock().unwrap().unwrap();
        assert_eq!(
            (
                ack_snapshot.created,
                ack_snapshot.deadline,
                ack_snapshot.binding
            ),
            (
                host_lifetime.created,
                host_lifetime.deadline,
                host_lifetime.binding
            )
        );
        assert!(
            !ack_snapshot.current_acked,
            "discarded ACK cannot ACK replacement"
        );
        let events = schedule.snapshot();
        let discarded: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                E::DiscardedAck { sequence, at } if *sequence == intermediate => Some(*at),
                _ => None,
            })
            .collect();
        assert_eq!(discarded.len(), 1, "exactly one discarded ACK consumed");
        assert!(discarded[0] >= rejected);
        let committed = h.frames.lock().unwrap().0.unwrap();
        assert_eq!(ack_snapshot.current_sequence, Some(committed));
        let captured = events
            .iter()
            .find_map(|event| match event {
                E::FrameCaptured { key, at } if key.wire_sequence == committed => Some(*at),
                _ => None,
            })
            .unwrap();
        assert!(
            captured > dequeued,
            "replacement capture must start after rejected dequeue"
        );
        h.wait(
            &mut gl,
            "new actual GL presentation and exact ACK restore Ready",
            Some((false, false)),
            |s| {
                s.ready.lock().unwrap().is_some_and(|n| {
                    n > old_sequence
                        && s.last_presented == Some(n)
                        && s.frames.lock().unwrap().1 == Some(n)
                })
            },
        );
        assert_eq!(
            h.private_events.len(),
            before,
            "new Ready does not replay rejected input"
        );
        h.click(8.0, 8.0);
    }
    let accepted_sequence = if refresh_first { 2 } else { 1 };
    h.wait(
        &mut gl,
        "one fresh atomic click reaches exact target",
        Some((false, false)),
        |s| {
            s.private_events[before..]
                .iter()
                .filter(|e| e.0 == 3)
                .count()
                == 2
                && schedule.snapshot().iter().any(|event| {
                    matches!(event,
                E::Accepted { sequence, .. } if *sequence == accepted_sequence)
                })
        },
    );
    let buttons: Vec<_> = h.private_events[before..]
        .iter()
        .filter(|e| e.0 == 3)
        .collect();
    assert_eq!(&buttons[0].1[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].1[2..], &[0x110, 0]);
    assert!(
        h.private_events[before..]
            .iter()
            .all(|e| matches!(e.0, 2 | 3 | 5))
    );
    let events = schedule.snapshot();
    let accepted: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            E::Accepted { key, sequence, at } => Some((*key, *sequence, *at)),
            _ => None,
        })
        .collect();
    assert_eq!(accepted.len(), 1, "no duplicate native acceptance");
    assert_eq!(accepted[0].1, accepted_sequence);
    let dequeued = events
        .iter()
        .find_map(|event| match event {
            E::ClickDequeued { key, sequence, at }
                if *key == accepted[0].0 && *sequence == accepted_sequence =>
            {
                Some(*at)
            }
            _ => None,
        })
        .unwrap();
    assert!(accepted[0].2 >= dequeued);
    if !refresh_first {
        assert_eq!(accepted[0].0.wire_sequence, old_sequence);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, E::RejectedStale { .. }))
        );
    }
    assert_eq!(
        remote_popup::lease_timing_for_test(&h.native.state),
        Some(native_lifetime)
    );
    assert_eq!(
        h.popup_client_lifetime.lock().unwrap().deadline,
        Some(client_deadline)
    );
    let after = h.popup_host_lifetime.lock().unwrap().unwrap();
    assert_eq!(
        (after.created, after.deadline, after.binding),
        (
            host_lifetime.created,
            host_lifetime.deadline,
            host_lifetime.binding
        )
    );
    h.finish(&mut gl);
}

#[test]
fn authenticated_popup_refresh_before_click_rejects_without_replay_then_new_presented_click_succeeds()
 {
    popup_refresh_click_order(true);
}

#[test]
fn authenticated_popup_click_before_refresh_is_accepted_exactly_once() {
    popup_refresh_click_order(false);
}

struct PopupCloseBarrier {
    events: Arc<Mutex<Vec<host::popup::PopupHostTestEvent>>>,
    release: mpsc::SyncSender<()>,
    hook: Arc<dyn Fn(host::popup::PopupHostTestEvent) + Send + Sync>,
}
impl PopupCloseBarrier {
    fn new() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let (release, resume) = mpsc::sync_channel(1);
        let resume = Mutex::new(resume);
        let hook = Arc::new(move |event| {
            {
                let mut events = recorded.lock().unwrap();
                assert!(events.len() < 512);
                events.push(event);
            }
            if let host::popup::PopupHostTestEvent::NativeCleanupConfirmed { deadline, .. } = event
            {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .expect("original native close deadline");
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(remaining)
                    .expect("release before original close deadline");
            }
        });
        Self {
            events,
            release,
            hook,
        }
    }
}

// This independent, read-only compositor capture observes the application
// callback. It is not the revoked popup family's video or an input grant.
fn terminal_root_readback(h: &mut PopupHarness) -> Readback {
    let mut c = h.native.connect();
    let list = c.bind("ext_foreign_toplevel_list_v1", 1);
    let sources = c.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
    let manager = c.bind("ext_image_copy_capture_manager_v1", 1);
    let events = fence(&mut h.native, &mut c);
    let identifier =
        crate::backend::wayland_udev::state::JwmWaylandState::foreign_toplevel_identifier(
            h.target.window,
        );
    let handles: Vec<_> = events
        .iter()
        .filter(|e| e.sender == list && e.opcode == 0)
        .map(|e| e.args[0])
        .collect();
    let selected: Vec<_> = events
        .iter()
        .filter(|e| {
            if e.opcode != 4 || !handles.contains(&e.sender) || e.args.is_empty() {
                return false;
            }
            let len = e.args[0] as usize;
            let bytes: Vec<_> = e.args[1..]
                .iter()
                .flat_map(|word| word.to_ne_bytes())
                .collect();
            len > 0
                && len <= bytes.len()
                && bytes[len - 1] == 0
                && &bytes[..len - 1] == identifier.as_bytes()
        })
        .map(|e| e.sender)
        .collect();
    assert_eq!(
        selected.len(),
        1,
        "exact original application root identifier"
    );
    let source = c.new_id();
    c.request(sources, 0, &[source, selected[0]]);
    let session = c.new_id();
    c.request(manager, 0, &[session, source, 0]);
    let events = fence(&mut h.native, &mut c);
    assert!(
        events
            .iter()
            .any(|e| e.sender == session && e.opcode == 0 && e.args == [ROOT_WIDTH, ROOT_HEIGHT])
    );
    Readback {
        size: (ROOT_WIDTH, ROOT_HEIGHT),
        client: c,
        session,
    }
}

fn terminal_open_popup(h: &mut PopupHarness, gl: &mut HeadlessSmithayGles) -> (u32, u32, u32) {
    h.wait(
        gl,
        "terminal scenario initial actual Ready",
        Some((false, false)),
        |s| {
            s.ready
                .lock()
                .unwrap()
                .is_some_and(|n| s.last_presented == Some(n))
        },
    );
    h.click(8.0, 8.0);
    h.wait(
        gl,
        "terminal scenario real root click",
        Some((false, false)),
        |s| s.private_events.iter().filter(|e| e.0 == 3).count() == 2,
    );
    let serial = h
        .private_events
        .iter()
        .find(|e| e.0 == 3 && e.1[3] == 1)
        .unwrap()
        .1[0];
    let popup = direct_popup_resources(
        &mut h.native,
        &mut h.target,
        h.private_seat.unwrap(),
        serial,
        h.main_pointer,
        h.main_keyboard,
        h.private_pointer.unwrap(),
    );
    h.wait(
        gl,
        "terminal scenario real popup pixels and proof",
        Some((true, false)),
        |s| {
            s.last_visual == Some((true, false))
                && s.ready.lock().unwrap().is_some_and(|n| {
                    s.last_presented == Some(n) && s.frames.lock().unwrap().1 == Some(n)
                })
        },
    );
    popup
}

fn terminal_application_callback(h: &mut PopupHarness, popup: (u32, u32, u32)) {
    // The test app callback runs only after its own real popup receiver has
    // observed the balanced click, never in response to a host status alone.
    let buttons: Vec<_> = h.private_events.iter().filter(|e| e.0 == 3).collect();
    assert_eq!(buttons.len(), 4, "one root click plus one popup click");
    assert_eq!(&buttons[2].1[2..], &[0x110, 1]);
    assert_eq!(&buttons[3].1[2..], &[0x110, 0]);
    let (blue, _file) = shm(
        &mut h.native,
        &mut h.target.client,
        [255, 0, 0, 255],
        ROOT_WIDTH,
        ROOT_HEIGHT,
    );
    h.target.client.request(popup.2, 0, &[]); // xdg_popup.destroy
    h.target.client.request(popup.1, 0, &[]); // xdg_surface.destroy
    h.target.client.request(popup.0, 0, &[]); // wl_surface.destroy
    h.target.client.request(h.target.surface, 1, &[blue, 0, 0]);
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, ROOT_WIDTH, ROOT_HEIGHT]);
    h.target.client.request(h.target.surface, 6, &[]);
    for event in fence(&mut h.native, &mut h.target.client) {
        assert_ne!(
            event.sender, h.main_pointer,
            "callback touched main pointer"
        );
        assert_ne!(
            event.sender, h.main_keyboard,
            "callback touched main keyboard"
        );
        if Some(event.sender) == h.private_pointer {
            assert!(
                matches!(event.opcode, 1 | 5),
                "callback may only clean private focus"
            );
            h.private_events.push((event.opcode, event.args));
        }
    }
    assert!(
        !remote_popup::active(&h.native.state),
        "popup destroy permanently revokes family"
    );
}

fn popup_destroy_after_delivery(before_tcp_accepted: bool) {
    use crate::remote::wayland_capture::interactive_popup::PopupDeliveryPause;
    use host::popup::PopupHostTestEvent as E;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let pause = Arc::new(PopupDeliveryPause::new(2));
    struct ReleaseOnDrop(Arc<PopupDeliveryPause>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(pause.clone());
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorded = log.clone();
    let (release, resume) = mpsc::sync_channel(1);
    let resume = Mutex::new(resume);
    let hook = Arc::new(move |event| {
        {
            let mut events = recorded.lock().unwrap();
            assert!(events.len() < 512);
            events.push(event);
        }
        if !before_tcp_accepted && matches!(event, E::Accepted { sequence: 2, .. }) {
            resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(1))
                .expect("application callback releases host completion barrier");
        }
    });
    let mut h = PopupHarness::configured_with_delivery_pause(
        &mut gl,
        true,
        true,
        None,
        None,
        Some(hook),
        before_tcp_accepted.then(|| pause.clone()),
    );
    let popup = terminal_open_popup(&mut h, &mut gl);
    let original_deadline = remote_popup::lease_timing_for_test(&h.native.state).unwrap();
    let mut root = terminal_root_readback(&mut h);
    let pixels = root.pixels(&mut h.native, &mut gl);
    assert!(pixels.chunks_exact(4).all(|p| p == [0, 0, 255, 255]));
    let ready = h.ready.lock().unwrap().unwrap();
    h.click(7.0, 8.0);
    h.wait(
        &mut gl,
        "actual popup click before application destroy",
        None,
        |s| {
            s.private_events.iter().filter(|e| e.0 == 3).count() == 4
                && if before_tcp_accepted {
                    pause.observed().is_some()
                } else {
                    log.lock()
                        .unwrap()
                        .iter()
                        .any(|e| matches!(e, E::Accepted { sequence: 2, .. }))
                }
        },
    );
    assert!(
        h.private_events
            .iter()
            .any(|e| e.0 == 2 && e.1[1..] == [3 * 256, 5 * 256])
    );
    if before_tcp_accepted {
        let observed = pause.observed().unwrap();
        assert_eq!(observed.native_sequence, 2);
        assert_eq!(observed.metadata.key.wire_sequence, ready);
        assert!(observed.accepted_at < observed.deadline);
        assert!(Instant::now() < observed.deadline);
        assert!(observed.deadline <= original_deadline.2);
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, E::Accepted { sequence: 2, .. }))
        );
    }
    terminal_application_callback(&mut h, popup);
    if before_tcp_accepted {
        pause.release();
    } else {
        release.send(()).unwrap();
    }
    // Observe the application's own committed result independently of the
    // now-revoked family. No post-revocation remote video success is assumed.
    let pixels = root.pixels(&mut h.native, &mut gl);
    assert!(pixels.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = None;
    let mut host = None;
    loop {
        h.tick(&mut gl, None);
        if client.is_none() {
            client = h.client_done.try_recv().ok();
        }
        if host.is_none() {
            host = h.host_done.try_recv().ok();
        }
        assert!(!remote_popup::active(&h.native.state));
        assert_eq!(
            h.private_events.iter().filter(|e| e.0 == 3).count(),
            4,
            "terminal result cannot dispatch another click"
        );
        assert!(
            Instant::now() < deadline,
            "terminal result and native cleanup deadline"
        );
        if client.is_some() && host.is_some() {
            break;
        }
    }
    let client_error = client
        .unwrap()
        .expect_err("terminal lease never resumes input");
    if before_tcp_accepted {
        assert!(
            crate::remote::client::popup_delivered_terminal(client_error.as_ref()),
            "exact DeliveredTerminal, not EOF or generic close: {client_error:?}"
        );
        host.unwrap()
            .expect("flushed terminal delivery exits host normally");
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, E::Accepted { sequence: 2, .. }))
        );
    } else {
        assert_eq!(
            crate::remote::client::classify_failure(client_error.as_ref()).peer_close_code,
            Some(crate::remote::close_wire::SessionCloseCode::CaptureUnavailable)
        );
        let error = host
            .unwrap()
            .expect_err("destroy after Accepted permanently closes family");
        assert_eq!(error.to_string(), "interactive popup lease revoked");
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, E::Accepted { sequence: 2, .. }))
                .count(),
            1
        );
    }
    let events = log.lock().unwrap();
    let terminal_action = events
        .iter()
        .position(|e| matches!(e, E::ClickDequeued { sequence: 2, .. }))
        .unwrap();
    assert!(
        !events[terminal_action + 1..]
            .iter()
            .any(|e| matches!(e, E::FrameCaptured { .. } | E::ReadyPublished(_))),
        "destroyed popup must not acquire replacement proof or Ready"
    );
    drop(events);
    assert!(
        h.private_events
            .iter()
            .all(|e| matches!(e.0, 0 | 1 | 2 | 3 | 5))
    );
    assert!(remote_popup::lease_timing_for_test(&h.native.state).is_none());
    h.client.take().unwrap().join().unwrap();
    h.host.take().unwrap().join().unwrap();
}

#[test]
fn authenticated_popup_destroy_after_native_delivery_reports_terminal_without_replay() {
    popup_destroy_after_delivery(true);
}

#[test]
fn authenticated_popup_destroy_after_tcp_acceptance_permanently_ends_authority() {
    popup_destroy_after_delivery(false);
}
