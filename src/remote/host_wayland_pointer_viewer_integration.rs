//! Actual native pointer viewer, authenticated TCP and original wl_pointer.
//! Both servers are owned. Feedback follows production GL pixel capture, not
//! DRM presentation. Small source/viewer dimensions bound debug rendering cost.
use super::*;
use crate::backend::wayland_udev::{image_copy_capture::wire_test_client::Client, remote_pointer};
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{
        ViewerBackend, with_wayland_pointer_candidate_observer, with_wayland_pointer_observer,
        with_wayland_test_stream,
    },
    wayland_capture::WaylandCapture,
    window_size::WindowSize,
};
use smithay::reexports::{
    wayland_protocols::xdg::shell::server::xdg_toplevel::State as TopState,
    wayland_server::protocol::wl_pointer::{Axis, AxisSource, ButtonState, WlPointer},
};
use smithay::{backend::input::InputTime, input::pointer::MotionEvent, utils::SERIAL_COUNTER};
use std::{
    fs::File,
    io::Write,
    os::fd::AsRawFd,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
const KEY: [u8; 32] = [0x78; 32];
const SW: u32 = 200;
const SH: u32 = 150;
const VW: u32 = 400;
const VH: u32 = 300;
#[derive(Clone, Copy, Debug)]
enum Case {
    Positive,
    NewFrame,
    Leave,
    Deactivate,
}
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn options(scroll: bool, horizontal_scroll: bool) -> host::HostOptions {
    host::HostOptions {
        frame_limits: Default::default(),
        listen: "127.0.0.1:0".into(),
        key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
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
        capture_source: Default::default(),
        capture_area: crate::remote::x11_capture::CaptureArea::Root,
        shared_sources: Vec::new(),
        scaling: Default::default(),
        allow_lan: false,
        allow_input: false,
        allow_window_resize: false,
        allow_wayland_keyboard: false,
        allow_wayland_pointer: true,
        allow_wayland_control: false,
        allow_wayland_text: false,
        allow_static_lossless: false,
        allow_wayland_scroll: scroll,
        allow_wayland_horizontal_scroll: horizontal_scroll,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}
fn shm(s: &mut Server, c: &mut Client, color: [u8; 4], w: u32, h: u32) -> (u32, File) {
    let shm = c.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"owned-pointer-pixels",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    nix::unistd::ftruncate(&fd, i64::from(w * h * 4)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&color.repeat((w * h) as usize)).unwrap();
    let pool = c.new_id();
    c.request_with_fd_args(shm, 0, &[pool, w * h * 4], file.as_raw_fd());
    let buffer = c.new_id();
    c.request(pool, 0, &[buffer, 0, w, h, w * 4, 0]);
    s.roundtrip();
    s.roundtrip();
    (buffer, file)
}
fn bind(s: &mut Server, a: &mut App, version: u32) -> u32 {
    let seat = a.client.bind("wl_seat", version);
    let pointer = a.client.new_id();
    a.client.request(seat, 0, &[pointer]);
    let keyboard = a.client.new_id();
    a.client.request(seat, 1, &[keyboard]);
    s.roundtrip();
    s.roundtrip();
    a.client.events();
    pointer
}
fn focus(s: &mut Server, a: &App) {
    let surface = s.state.surface_for_window(a.window).unwrap();
    s.state
        .set_keyboard_focus(Some(surface.clone()), SERIAL_COUNTER.next_serial());
    let pointer = s.state.seat.get_pointer().unwrap();
    pointer.motion(
        &mut s.state,
        Some((surface, (0.0, 0.0).into())),
        &MotionEvent {
            location: (5.0, 5.0).into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: InputTime::now(),
        },
    );
    pointer.frame(&mut s.state);
    s.roundtrip();
    s.roundtrip();
}
fn collect(a: &mut App, id: u32, out: &mut Vec<(u16, Vec<u32>)>, wire: &mut Vec<(u16, Vec<u32>)>) {
    for e in a.client.events() {
        assert!(
            !(e.sender == 1 && e.opcode == 0),
            "owned app protocol error"
        );
        if e.sender == id {
            if matches!(e.opcode, 2 | 3 | 4) {
                out.push((e.opcode, e.args.clone()));
            }
            wire.push((e.opcode, e.args));
        }
    }
}
struct Readback {
    size: (u32, u32),
    client: Client,
    session: u32,
}
impl Readback {
    fn new(s: &mut Server, size: (u32, u32)) -> Self {
        let mut c = s.connect();
        let list = c.bind("ext_foreign_toplevel_list_v1", 1);
        let sources = c.bind("ext_foreign_toplevel_image_capture_source_manager_v1", 1);
        let manager = c.bind("ext_image_copy_capture_manager_v1", 1);
        s.roundtrip();
        s.roundtrip();
        let handle = c
            .events()
            .into_iter()
            .find(|e| e.sender == list && e.opcode == 0)
            .unwrap()
            .args[0];
        let source = c.new_id();
        c.request(sources, 0, &[source, handle]);
        let session = c.new_id();
        c.request(manager, 0, &[session, source, 0]);
        s.roundtrip();
        s.roundtrip();
        assert!(
            c.events()
                .iter()
                .any(|e| e.sender == session && e.opcode == 0 && e.args == [size.0, size.1])
        );
        Self {
            client: c,
            session,
            size,
        }
    }
    fn pixels(&mut self, s: &mut Server, gl: &mut HeadlessSmithayGles) -> Vec<u8> {
        let (buffer, mut file) = shm(s, &mut self.client, [0x5a; 4], self.size.0, self.size.1);
        let frame = self.client.new_id();
        self.client.request(self.session, 0, &[frame]);
        self.client.request(frame, 1, &[buffer]);
        self.client.request(frame, 3, &[]);
        s.roundtrip();
        s.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &s.state,
            s.state.image_capture_pending.as_ref().unwrap(),
        );
        s.roundtrip();
        s.roundtrip();
        assert!(
            self.client
                .events()
                .iter()
                .any(|e| e.sender == frame && e.opcode == 3),
            "production GL ready"
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut pixels = Vec::new();
        file.read_to_end(&mut pixels).unwrap();
        assert_eq!(pixels.len(), (self.size.0 * self.size.1 * 4) as usize);
        self.client.request(frame, 0, &[]);
        s.roundtrip();
        pixels
    }
}
struct Harness {
    source_size: (u32, u32),
    viewer_size: (u32, u32),
    native: Server,
    local: Server,
    target: App,
    other: App,
    target_pointer: u32,
    other_pointer: u32,
    pointer: Option<WlPointer>,
    feedback: Vec<PresentationFeedbackCallback>,
    readback: Option<Readback>,
    events: Vec<(u16, Vec<u32>)>,
    unexpected: Vec<(u16, Vec<u32>)>,
    wire_events: Vec<(u16, Vec<u32>)>,
    unexpected_wire: Vec<(u16, Vec<u32>)>,
    blue_presented: bool,
    pointer_lifetime: Arc<Mutex<Option<(Instant, Instant)>>>,
    button_dispatch: Arc<Mutex<(u64, u64)>>,
    frames: Arc<Mutex<(Option<u64>, Option<u64>)>>,
    purposes: Arc<Mutex<Vec<(u64, crate::remote::source_wire::FramePurpose)>>>,
    presentation_count: u64,
    latest_ready: Option<u64>,
    presented_ready_floor: Option<u64>,
    ready: Arc<Mutex<Option<u64>>>,
    candidate: Arc<Mutex<(Option<u64>, bool, u64)>>,
    refresh: Arc<Mutex<Option<Instant>>>,
    shutdown: Arc<AtomicBool>,
    _stop: Stop,
    client_done: mpsc::Receiver<Result<(), String>>,
    host_done: mpsc::Receiver<crate::remote::RemoteResult<()>>,
    client: Option<thread::JoinHandle<()>>,
    host: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new(gl: &mut HeadlessSmithayGles) -> Self {
        Self::new_with_scroll(gl, false, false)
    }
    fn new_with_scroll(
        gl: &mut HeadlessSmithayGles,
        host_scroll: bool,
        viewer_scroll: bool,
    ) -> Self {
        Self::new_with_scroll_permissions(gl, (host_scroll, false), (viewer_scroll, false))
    }
    fn new_with_scroll_permissions(
        gl: &mut HeadlessSmithayGles,
        host_scroll: (bool, bool),
        viewer_scroll: (bool, bool),
    ) -> Self {
        Self::new_configured(gl, host_scroll, viewer_scroll, false, false)
    }
    fn new_with_lossless(
        gl: &mut HeadlessSmithayGles,
        host_lossless: bool,
        viewer_lossless: bool,
    ) -> Self {
        Self::new_configured(
            gl,
            (false, false),
            (false, false),
            host_lossless,
            viewer_lossless,
        )
    }
    fn new_configured(
        gl: &mut HeadlessSmithayGles,
        host_scroll: (bool, bool),
        viewer_scroll: (bool, bool),
        host_lossless: bool,
        viewer_lossless: bool,
    ) -> Self {
        Self::new_sized(
            gl,
            host_scroll,
            viewer_scroll,
            host_lossless,
            viewer_lossless,
            (SW, SH),
            (VW, VH),
        )
    }
    fn new_sized(
        gl: &mut HeadlessSmithayGles,
        host_scroll: (bool, bool),
        viewer_scroll: (bool, bool),
        host_lossless: bool,
        viewer_lossless: bool,
        source_size: (u32, u32),
        viewer_size: (u32, u32),
    ) -> Self {
        // Keep old vertical v7 coverage. Horizontal tests independently verify
        // the real selected recipient's v9 value120 group, not viewer metadata.
        let recipient_version = if host_scroll.1 || viewer_scroll.1 {
            9
        } else {
            7
        };
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        let mut target =
            App::with_size_color(&mut native, [0, 0, 255, 255], source_size.0, source_size.1);
        let target_pointer = bind(&mut native, &mut target, recipient_version);
        focus(&mut native, &target);
        target.client.events();
        let (ns, capture_stream) = UnixStream::pair().unwrap();
        native
            .display
            .handle()
            .insert_client(ns, Arc::new(JwmClientState::default()))
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Stop(shutdown.clone());
        let stopping = shutdown.clone();
        let refresh = Arc::new(Mutex::new(None));
        let host_refresh = refresh.clone();
        let (selected_tx, selected) = mpsc::channel();
        let (host_tx, host_done) = mpsc::channel();
        let host = thread::spawn(move || {
            let result = host::host_viewer_integration::with_forced_refresh(host_refresh, || {
                (|| -> crate::remote::RemoteResult<()> {
                    let capture = WaylandCapture::connect_test_stream(capture_stream, 1, 0)?;
                    selected_tx.send(())?;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let stream = loop {
                        assert!(!stopping.load(Ordering::Acquire) && Instant::now() < deadline);
                        match listener.accept() {
                            Ok((s, _)) => break s,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(1))
                            }
                            Err(e) => return Err(e.into()),
                        }
                    };
                    let peer = host::authenticate_peer(stream, &KEY)?;
                    let mut host_options = options(host_scroll.0, host_scroll.1);
                    host_options.allow_static_lossless = host_lossless;
                    host::serve_authenticated(
                        peer,
                        &host_options,
                        &stopping,
                        &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
                    )
                })()
            });
            host_tx.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while selected.try_recv().is_err() {
            native.roundtrip();
            fulfill_test_toplevel_capture(
                &mut gl.renderer,
                &mut None,
                &native.state,
                native.state.image_capture_pending.as_ref().unwrap(),
            );
            native.roundtrip();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        // Distinct from both target phases (red, then blue): a fallback to the
        // unrelated surface cannot masquerade as the click-triggered repaint.
        let mut other =
            App::with_size_color(&mut native, [0, 255, 0, 255], source_size.0, source_size.1);
        let other_pointer = bind(&mut native, &mut other, recipient_version);
        focus(&mut native, &target);
        target.client.events();
        other.client.events();
        let (ls, stream) = UnixStream::pair().unwrap();
        local
            .display
            .handle()
            .insert_client(ls, Arc::new(JwmClientState::default()))
            .unwrap();
        let ready = Arc::new(Mutex::new(None));
        let candidate = Arc::new(Mutex::new((None, false, 0)));
        let pointer_lifetime = Arc::new(Mutex::new(None));
        let observed_lifetime = pointer_lifetime.clone();
        let button_dispatch = Arc::new(Mutex::new((0, 0)));
        let observed_buttons = button_dispatch.clone();
        let frames = Arc::new(Mutex::new((None, None)));
        let purposes = Arc::new(Mutex::new(Vec::new()));
        let observed_frames = frames.clone();
        let observed_purposes = purposes.clone();
        let observed = ready.clone();
        let observed_candidate = candidate.clone();
        let (client_tx, client_done) = mpsc::channel();
        let client = thread::spawn(move || {
            let opts = ClientOptions {
                viewer_backend: ViewerBackend::Wayland,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: true,
                wayland_control: false,
                wayland_text: None,
                static_lossless: viewer_lossless,
                wayland_scroll: viewer_scroll.0,
                wayland_horizontal_scroll: viewer_scroll.1,
                resize_source: None,
                follow_source_size: false,
                window_size: Some(WindowSize {
                    width: viewer_size.0 as u16,
                    height: viewer_size.1 as u16,
                }),
                window_position: None,
                frame_limits: Default::default(),
                address: address.to_string(),
                peer_label: "owned native pointer".into(),
                key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
                display: None,
                view_only: false,
                grab_input: false,
                clipboard: false,
                clipboard_mode: Default::default(),
                clipboard_direction: Default::default(),
                clipboard_file_limit: 0,
                escape_key: "Escape".into(),
                scaling: Default::default(),
            };
            let result = with_wayland_pointer_observer(observed, || {
                with_wayland_pointer_candidate_observer(observed_candidate, || {
                    crate::remote::client::with_frame_purpose_observer(observed_purposes, || {
                        crate::remote::wayland_viewer::with_wayland_control_frame_observer(
                            observed_frames,
                            || crate::remote::wayland_viewer::with_pointer_button_dispatch_observer(
                                observed_buttons, || crate::remote::client::with_pointer_lifetime_observer(observed_lifetime, || with_wayland_test_stream(stream, || run_client(opts)))),
                        )
                    })
                })
            })
            .map_err(|e| e.to_string());
            client_tx.send(result).unwrap();
        });
        Self {
            source_size,
            viewer_size,
            native,
            local,
            target,
            other,
            target_pointer,
            other_pointer,
            pointer: None,
            feedback: Vec::new(),
            readback: None,
            events: Vec::new(),
            unexpected: Vec::new(),
            wire_events: Vec::new(),
            unexpected_wire: Vec::new(),
            blue_presented: false,
            pointer_lifetime,
            button_dispatch,
            frames,
            purposes,
            presentation_count: 0,
            latest_ready: None,
            presented_ready_floor: None,
            ready,
            candidate,
            refresh,
            shutdown,
            _stop: stop,
            client_done,
            host_done,
            client: Some(client),
            host: Some(host),
        }
    }
    fn tick(&mut self, gl: &mut HeadlessSmithayGles, drawn: bool) {
        self.tick_with_inspector(gl, drawn, None);
    }
    fn tick_with_inspector(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        drawn: bool,
        mut inspector: Option<&mut dyn FnMut(&[u8], u64) -> bool>,
    ) {
        if let Some(sequence) = *self.ready.lock().unwrap() {
            self.latest_ready = Some(self.latest_ready.map_or(sequence, |old| old.max(sequence)));
        }
        self.native.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.native.state,
            self.native.state.image_capture_pending.as_ref().unwrap(),
        );
        self.native.roundtrip();
        self.local.roundtrip();
        for (&window, top) in &self.local.state.toplevels {
            if !top.is_initial_configure_sent() {
                let geo = self.local.state.window_geometry.get_mut(&window).unwrap();
                geo.w = self.viewer_size.0;
                geo.h = self.viewer_size.1;
                top.with_pending_state(|s| {
                    s.size = Some((self.viewer_size.0 as i32, self.viewer_size.1 as i32).into());
                    s.states.set(TopState::Activated);
                });
                top.send_configure();
            }
        }
        self.local.roundtrip();
        if let Some(top) = self.local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            if self.pointer.is_none() {
                let resources: Vec<_> = self
                    .local
                    .state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .client_pointers(&surface.client().unwrap())
                    .collect();
                if let Some(p) = resources.first() {
                    assert_eq!(resources.len(), 1);
                    p.enter(1, &surface, 160.0, 120.0);
                    p.frame();
                    self.pointer = Some(p.clone());
                    self.local.roundtrip();
                }
            }
            self.feedback.extend(take_feedback(&surface));
        }
        if drawn && !self.feedback.is_empty() {
            if self.readback.is_none() {
                self.readback = Some(Readback::new(&mut self.local, self.viewer_size));
            }
            let sequence = self
                .frames
                .lock()
                .unwrap()
                .0
                .expect("actual committed viewer sequence");
            let pixels = self.readback.as_mut().unwrap().pixels(&mut self.local, gl);
            assert_eq!(
                self.frames.lock().unwrap().0,
                Some(sequence),
                "readback must describe the same commit"
            );
            let allow_feedback = if let Some(inspect) = inspector.as_mut() {
                inspect(&pixels, sequence)
            } else {
                let blue = pixels
                    .chunks_exact(4)
                    .all(|p| p[0] >= 250 && p[1] <= 3 && p[2] <= 3 && p[3] == 255);
                let red = pixels
                    .chunks_exact(4)
                    .all(|p| p[2] >= 250 && p[1] <= 3 && p[0] <= 3 && p[3] == 255);
                assert!(
                    blue || red,
                    "actual viewer pixels must equal selected red/blue source"
                );
                self.blue_presented = blue;
                true
            };
            if allow_feedback {
                let feedback = std::mem::take(&mut self.feedback);
                self.presentation_count += feedback.len() as u64;
                self.presented_ready_floor = self.latest_ready;
                present(&mut self.local, feedback);
                eprintln!(
                    "owned viewer presentation ordinal={} actual_GL_color={}",
                    self.presentation_count,
                    if self.blue_presented { "blue" } else { "red" }
                );
            }
        }
        collect(
            &mut self.target,
            self.target_pointer,
            &mut self.events,
            &mut self.wire_events,
        );
        collect(
            &mut self.other,
            self.other_pointer,
            &mut self.unexpected,
            &mut self.unexpected_wire,
        );
        assert!(
            self.unexpected_wire.is_empty(),
            "unselected native pointer received protocol events"
        );
        assert!(
            self.unexpected.is_empty(),
            "unselected native pointer receiver got events"
        );
    }
    fn wait(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        phase: &str,
        drawn: bool,
        mut done: impl FnMut(&Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !done(self) {
            self.tick(gl, drawn);
            if let Ok(result) = self.client_done.try_recv() {
                panic!("{phase}: viewer ended {result:?}");
            }
            assert!(
                Instant::now() < deadline,
                "{phase}: timed out; events={:?} candidate={:?}",
                self.events,
                self.candidate.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn ready_after(&mut self, gl: &mut HeadlessSmithayGles, old: Option<u64>) -> u64 {
        self.wait(gl, "fresh actually presented Ready", true, |s| {
            s.ready
                .lock()
                .unwrap()
                .is_some_and(|n| old.is_none_or(|o| n > o))
        });
        self.ready.lock().unwrap().unwrap()
    }
    fn wheel_axis(&mut self, step: i32, axis: Axis) {
        assert!(matches!(step, -1 | 1));
        let p = self.pointer.as_ref().unwrap();
        assert_eq!(
            p.version(),
            7,
            "native viewer intentionally uses v7 discrete axis"
        );
        p.motion(20, 160.0, 120.0);
        p.axis_source(AxisSource::Wheel);
        p.axis_discrete(axis, step);
        p.axis(20, axis, f64::from(step) * 10.0);
        p.frame();
        self.local.roundtrip();
    }
    fn repaint(&mut self, blue: bool) {
        let color = if blue {
            [255, 0, 0, 255]
        } else {
            [0, 0, 255, 255]
        };
        let (buffer, _file) = shm(
            &mut self.native,
            &mut self.target.client,
            color,
            self.source_size.0,
            self.source_size.1,
        );
        self.target
            .client
            .request(self.target.surface, 1, &[buffer, 0, 0]);
        self.target.client.request(
            self.target.surface,
            2,
            &[0, 0, self.source_size.0, self.source_size.1],
        );
        self.target.client.request(self.target.surface, 6, &[]);
        self.native.roundtrip();
        self.native.roundtrip();
    }
    fn button(&mut self, down: bool) {
        let p = self.pointer.as_ref().unwrap();
        p.button(
            10,
            10,
            0x110,
            if down {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        );
        p.frame();
        self.local.roundtrip();
    }
    fn close(mut self, gl: &mut HeadlessSmithayGles) {
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
            self.tick(gl, false);
            if let Ok(r) = self.client_done.try_recv() {
                r.unwrap();
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        self.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut finished = false;
        loop {
            self.tick(gl, false);
            if let Ok(r) = self.host_done.try_recv() {
                eprintln!("owned pointer host cleanup {r:?}");
                finished = true;
            }
            if finished && !remote_pointer::active(&self.native.state) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "host exit and native cleanup must both complete"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!remote_pointer::active(&self.native.state));
        self.client.take().unwrap().join().unwrap();
        self.host.take().unwrap().join().unwrap();
    }
}
#[test]
fn actual_wayland_pointer_viewer_tcp_presented_actions_and_revocation() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in [
        Case::Positive,
        Case::NewFrame,
        Case::Leave,
        Case::Deactivate,
    ] {
        eprintln!("owned native pointer TCP case {case:?}");
        let mut h = Harness::new(&mut gl);
        h.wait(
            &mut gl,
            "actual initial commit without feedback",
            false,
            |s| s.pointer.is_some() && !s.feedback.is_empty(),
        );
        let withheld_until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < withheld_until {
            h.tick(&mut gl, false);
            assert!(h.events.is_empty());
            assert!(!remote_pointer::active(&h.native.state));
            assert!(h.ready.lock().unwrap().is_none());
            thread::sleep(Duration::from_millis(1));
        }
        let first = h.ready_after(&mut gl, None);
        let p = h.pointer.as_ref().unwrap();
        p.motion(2, 160.0, 120.0);
        p.frame();
        h.local.roundtrip();
        h.wait(&mut gl, "precise source-local motion", true, |s| {
            s.events.len() == 1
        });
        assert_eq!(h.events[0].0, 2);
        assert_eq!(&h.events[0].1[1..], &[80 * 256, 60 * 256]);
        let second = h.ready_after(&mut gl, Some(first));
        if matches!(case, Case::NewFrame) {
            h.button(true);
            h.wait(&mut gl, "real down candidate", true, |s| {
                let c = s.candidate.lock().unwrap();
                c.0 == Some(second) && c.1 && c.2 == 1
            });
            *h.refresh.lock().unwrap() = Some(Instant::now());
            let next = h.ready_after(&mut gl, Some(second));
            h.wait(
                &mut gl,
                "new presented frame cancels held candidate",
                true,
                |s| {
                    let c = s.candidate.lock().unwrap();
                    c.0.is_none() && c.1 && c.2 == 1
                },
            );
            eprintln!("real native pointer candidate frame {second} replaced by presented {next}");
            h.button(false);
            h.wait(&mut gl, "cancelled up cannot click", true, |s| {
                let c = s.candidate.lock().unwrap();
                !c.1 && c.0.is_none() && c.2 == 1
            });
            assert_eq!(h.events.len(), 1);
        }
        h.button(true);
        h.button(false);
        h.wait(&mut gl, "atomic native click", true, |s| {
            s.events.len() >= 4
        });
        assert_eq!(h.events.len(), 4);
        assert_eq!(h.events[1].0, 2);
        assert_eq!(&h.events[1].1[1..], &[80 * 256, 60 * 256]);
        assert_eq!(h.events[2].0, 3);
        assert_eq!(&h.events[2].1[2..], &[0x110, 1]);
        assert_eq!(h.events[3].0, 3);
        assert_eq!(&h.events[3].1[2..], &[0x110, 0]);
        // The owned application repaints only after observing the real click.
        let (buffer, _file) = shm(
            &mut h.native,
            &mut h.target.client,
            [255, 0, 0, 255],
            SW,
            SH,
        );
        h.target
            .client
            .request(h.target.surface, 1, &[buffer, 0, 0]);
        h.target
            .client
            .request(h.target.surface, 2, &[0, 0, SW, SH]);
        h.target.client.request(h.target.surface, 6, &[]);
        h.native.roundtrip();
        h.native.roundtrip();
        // A host-retained frame may still be red; only real blue viewer capture
        // followed by feedback proves that the changed application came through.
        h.wait(
            &mut gl,
            "click repaint through capture TCP and actual viewer GL",
            true,
            |s| s.blue_presented,
        );
        if matches!(case, Case::Leave | Case::Deactivate) {
            let surface = viewer_surface(&h.local);
            if matches!(case, Case::Leave) {
                let p = h.pointer.as_ref().unwrap();
                p.leave(40, &surface);
                p.enter(41, &surface, 160.0, 120.0);
                p.frame();
            } else {
                let top = h.local.state.toplevels.values().next().unwrap();
                top.with_pending_state(|s| s.states.unset(TopState::Activated));
                top.send_configure();
            }
            h.local.roundtrip();
            h.wait(&mut gl, "permanent local lifecycle revocation", true, |s| {
                !remote_pointer::active(&s.native.state) && s.ready.lock().unwrap().is_none()
            });
            let before = h.events.clone();
            let before_presentation = h.presentation_count;
            assert!(h.blue_presented, "pre-revocation positive control was blue");
            // Repaint only AFTER revocation. The new red pixels must traverse
            // the production capture/TCP/viewer pipeline, then be read by GL
            // before the owned compositor sends new presentation feedback.
            let (buffer, _red_file) = shm(
                &mut h.native,
                &mut h.target.client,
                [0, 0, 255, 255],
                SW,
                SH,
            );
            h.target
                .client
                .request(h.target.surface, 1, &[buffer, 0, 0]);
            h.target
                .client
                .request(h.target.surface, 2, &[0, 0, SW, SH]);
            h.target.client.request(h.target.surface, 6, &[]);
            h.native.roundtrip();
            h.native.roundtrip();
            h.wait(
                &mut gl,
                "revoked lease still streams new red pixels",
                true,
                |s| {
                    assert_eq!(s.events, before);
                    assert!(!remote_pointer::active(&s.native.state));
                    assert!(s.ready.lock().unwrap().is_none());
                    s.presentation_count > before_presentation && !s.blue_presented
                },
            );
            eprintln!(
                "read-only continuation after revoke: presentation ordinal {} -> {}, blue -> red",
                before_presentation, h.presentation_count
            );
            let top = h.local.state.toplevels.values().next().unwrap();
            top.with_pending_state(|s| s.states.set(TopState::Activated));
            top.send_configure();
            h.button(true);
            h.button(false);
            let until = Instant::now() + Duration::from_millis(150);
            while Instant::now() < until {
                h.tick(&mut gl, true);
                assert_eq!(h.events, before);
                assert!(!remote_pointer::active(&h.native.state));
                assert!(h.ready.lock().unwrap().is_none());
                thread::sleep(Duration::from_millis(1));
            }
        }
        h.close(&mut gl);
    }
}

fn assert_one_wheel(events: &[(u16, Vec<u32>)], step: i32) {
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        [2, 6, 8, 4, 5],
        "one exact target-only v7 wheel group, no extra motion/button action"
    );
    assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
    assert_eq!(events[1].1, [0]); // wl_pointer.axis_source wheel
    assert_eq!(events[2].1, [0, step as u32]); // vertical, signed detent
    assert_eq!(&events[3].1[1..], &[0, (step * 10 * 256) as u32]);
    assert!(events[4].1.is_empty());
}

fn assert_one_horizontal_wheel_120(events: &[(u16, Vec<u32>)], step: i32) {
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        [2, 6, 9, 4, 5]
    );
    assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
    assert_eq!(events[1].1, [0]);
    assert_eq!(events[2].1, [1, (step * 120) as u32]);
    assert_eq!(&events[3].1[1..], &[1, (step * 10 * 256) as u32]);
    assert!(events[4].1.is_empty());
}

#[test]
fn actual_wayland_scroll_viewer_tcp_both_directions_presented_ready_no_waiting_replay() {
    exercise_scroll_viewer_tcp(false);
}

#[test]
fn actual_wayland_horizontal_scroll_viewer_tcp_both_directions_presented_ready_no_waiting_replay() {
    exercise_scroll_viewer_tcp(true);
}

fn exercise_scroll_viewer_tcp(horizontal: bool) {
    let axis = if horizontal {
        Axis::HorizontalScroll
    } else {
        Axis::VerticalScroll
    };
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_scroll_permissions(
        &mut gl,
        (!horizontal, horizontal),
        (!horizontal, horizontal),
    );
    h.wait(&mut gl, "scroll initial commit withheld", false, |s| {
        s.pointer.is_some() && !s.feedback.is_empty()
    });
    // These complete, real wheel frames precede presentation feedback on the
    // same ordered Wayland connection. They cannot become a later Ready action.
    h.wheel_axis(-1, axis);
    h.wheel_axis(1, axis);
    let withheld_until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < withheld_until {
        h.tick(&mut gl, false);
        assert!(h.wire_events.is_empty());
        assert!(!remote_pointer::active(&h.native.state));
        thread::sleep(Duration::from_millis(1));
    }
    let mut ready = h.ready_after(&mut gl, None);
    assert!(
        h.wire_events.is_empty(),
        "pre-presentation wheel was not replayed"
    );
    for (index, step) in [-1, 1].into_iter().enumerate() {
        let before = h.wire_events.len();
        h.wheel_axis(step, axis);
        h.wait(&mut gl, "real native one-notch axis", false, |s| {
            s.wire_events.len() >= before + 5
        });
        if horizontal {
            assert_one_horizontal_wheel_120(&h.wire_events[before..], step);
        } else {
            assert_one_wheel(&h.wire_events[before..], step);
        }
        h.wait(&mut gl, "scroll consumes Ready", false, |s| {
            s.ready.lock().unwrap().is_none() && s.candidate.lock().unwrap().2 == index as u64 + 1
        });
        let after_action = h.wire_events.clone();
        // While no replacement is presented, do not accumulate either direction.
        h.wheel_axis(1, axis);
        h.wheel_axis(-1, axis);
        h.wheel_axis(1, axis);
        let blue = index == 0;
        let before_presentation = h.presentation_count;
        h.repaint(blue); // the owned app reacts only to the observed axis above
        h.wait(
            &mut gl,
            "scroll app repaint traverses real capture TCP GL feedback",
            true,
            |s| {
                assert_eq!(
                    s.wire_events, after_action,
                    "Waiting burst generated native input"
                );
                s.presentation_count > before_presentation && s.blue_presented == blue
            },
        );
        let floor = h.presented_ready_floor.map_or(ready, |old| old.max(ready));
        let next = h.ready_after(&mut gl, Some(floor));
        assert!(next > ready);
        assert_eq!(
            h.wire_events, after_action,
            "Waiting wheel not replayed at new Ready"
        );
        // The real client action counter independently remains one per notch.
        assert_eq!(h.candidate.lock().unwrap().2, index as u64 + 1);
        eprintln!(
            "native wheel {step}: frame {ready} -> new presented Ready {next}, actual blue={blue}"
        );
        ready = next;
    }
    if horizontal {
        let before = h.events.len();
        h.button(true);
        h.button(false);
        h.wait(
            &mut gl,
            "post-horizontal exact Ready admits real click",
            false,
            |s| s.events.len() == before + 3,
        );
        assert_eq!(
            h.events[before..].iter().map(|e| e.0).collect::<Vec<_>>(),
            [2, 3, 3]
        );
        assert_eq!(&h.events[before + 1].1[2..], &[0x110, 1]);
        assert_eq!(&h.events[before + 2].1[2..], &[0x110, 0]);
    }
    h.close(&mut gl);
}

#[test]
fn actual_wayland_scroll_viewer_tcp_requires_both_permissions_and_no_held_left() {
    exercise_scroll_permissions_tcp(false);
}

#[test]
fn actual_wayland_horizontal_scroll_viewer_tcp_requires_each_permission_and_no_held_left() {
    exercise_scroll_permissions_tcp(true);
}

fn exercise_scroll_permissions_tcp(horizontal: bool) {
    let axis = if horizontal {
        Axis::HorizontalScroll
    } else {
        Axis::VerticalScroll
    };
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    // A viewer request cannot manufacture the host's separate scroll consent.
    let mut denied =
        Harness::new_with_scroll_permissions(&mut gl, (horizontal, false), (true, horizontal));
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        denied.tick(&mut gl, false);
        if let Ok(result) = denied.client_done.try_recv() {
            let error = result.expect_err("host denied scroll must fail negotiation");
            assert!(
                error.contains(if horizontal {
                    "Wayland horizontal scroll permission was not mutually negotiated"
                } else {
                    "Wayland scroll permission was not mutually negotiated"
                }),
                "expected concrete authorization failure: {error}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "denied scroll negotiation did not finish"
        );
        thread::sleep(Duration::from_millis(1));
    }
    denied.shutdown.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        denied.tick(&mut gl, false);
        if denied.host_done.try_recv().is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "denied host cleanup");
        thread::sleep(Duration::from_millis(1));
    }
    assert!(denied.wire_events.is_empty());
    assert!(!remote_pointer::active(&denied.native.state));
    denied.client.take().unwrap().join().unwrap();
    denied.host.take().unwrap().join().unwrap();
    drop(denied);
    for viewer_scroll in [false, true] {
        let mut h = Harness::new_with_scroll_permissions(
            &mut gl,
            (true, horizontal),
            (
                if horizontal { true } else { viewer_scroll },
                horizontal && viewer_scroll,
            ),
        );
        h.wait(&mut gl, "negative axis initial commit", false, |s| {
            s.pointer.is_some() && !s.feedback.is_empty()
        });
        let ready = h.ready_after(&mut gl, None);
        if viewer_scroll {
            h.button(true);
            h.wait(&mut gl, "left held before wheel", true, |s| {
                let candidate = s.candidate.lock().unwrap();
                candidate.0 == Some(ready) && candidate.1 && candidate.2 == 0
            });
        }
        h.wheel_axis(1, axis); // either unrequested locally, or forbidden while left held
        h.wait(
            &mut gl,
            "unsupported wheel revokes without native action",
            true,
            |s| !remote_pointer::active(&s.native.state) && s.ready.lock().unwrap().is_none(),
        );
        assert!(
            h.wire_events.is_empty(),
            "no host axis or click from denied local wheel"
        );
        if viewer_scroll {
            h.button(false);
        }
        h.wheel_axis(-1, axis);
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            h.tick(&mut gl, true);
            assert!(h.wire_events.is_empty());
            assert!(!remote_pointer::active(&h.native.state));
            thread::sleep(Duration::from_millis(1));
        }
        h.close(&mut gl);
    }
}

#[test]
fn actual_wayland_horizontal_scroll_viewer_tcp_mixed_axes_revoke_without_input() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_scroll_permissions(&mut gl, (true, true), (true, true));
    h.wait(&mut gl, "mixed axis viewer commit", false, |s| {
        s.pointer.is_some() && !s.feedback.is_empty()
    });
    h.ready_after(&mut gl, None);
    let pointer = h.pointer.as_ref().unwrap();
    pointer.axis_source(AxisSource::Wheel);
    pointer.axis_discrete(Axis::VerticalScroll, 1);
    pointer.axis(30, Axis::VerticalScroll, 10.0);
    pointer.axis_discrete(Axis::HorizontalScroll, -1);
    pointer.axis(30, Axis::HorizontalScroll, -10.0);
    pointer.frame();
    h.local.roundtrip();
    h.wait(&mut gl, "mixed axes revoke real native lease", false, |s| {
        !remote_pointer::active(&s.native.state) && s.ready.lock().unwrap().is_none()
    });
    assert!(h.wire_events.is_empty());
    h.wheel_axis(1, Axis::HorizontalScroll);
    let until = Instant::now() + Duration::from_millis(100);
    while Instant::now() < until {
        h.tick(&mut gl, true);
        assert!(h.wire_events.is_empty());
        assert!(!remote_pointer::active(&h.native.state));
        thread::sleep(Duration::from_millis(1));
    }
    h.close(&mut gl);
}

#[path = "host_wayland_pointer_lossless_integration.rs"]
mod pointer_lossless_integration;
