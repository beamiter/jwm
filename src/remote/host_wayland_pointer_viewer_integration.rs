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
    wayland_server::protocol::wl_pointer::{ButtonState, WlPointer},
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
fn options() -> host::HostOptions {
    host::HostOptions {
        frame_limits: Default::default(),
        listen: "127.0.0.1:0".into(),
        key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
        display: None,
        wayland_select: true,
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
fn bind(s: &mut Server, a: &mut App) -> u32 {
    let seat = a.client.bind("wl_seat", 7);
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
fn collect(a: &mut App, id: u32, out: &mut Vec<(u16, Vec<u32>)>) {
    for e in a.client.events() {
        assert!(
            !(e.sender == 1 && e.opcode == 0),
            "owned app protocol error"
        );
        if e.sender == id && matches!(e.opcode, 2 | 3 | 4) {
            out.push((e.opcode, e.args));
        }
    }
}
struct Readback {
    client: Client,
    session: u32,
}
impl Readback {
    fn new(s: &mut Server) -> Self {
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
                .any(|e| e.sender == session && e.opcode == 0 && e.args == [VW, VH])
        );
        Self { client: c, session }
    }
    fn blue(&mut self, s: &mut Server, gl: &mut HeadlessSmithayGles) -> bool {
        let (buffer, mut file) = shm(s, &mut self.client, [0x5a; 4], VW, VH);
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
        assert_eq!(pixels.len(), (VW * VH * 4) as usize);
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
        self.client.request(frame, 0, &[]);
        s.roundtrip();
        blue
    }
}
struct Harness {
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
    blue_presented: bool,
    presentation_count: u64,
    ready: Arc<Mutex<Option<u64>>>,
    candidate: Arc<Mutex<(Option<u64>, bool, u64)>>,
    refresh: Arc<Mutex<Option<Instant>>>,
    shutdown: Arc<AtomicBool>,
    _stop: Stop,
    client_done: mpsc::Receiver<Result<(), String>>,
    host_done: mpsc::Receiver<Result<(), String>>,
    client: Option<thread::JoinHandle<()>>,
    host: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new(gl: &mut HeadlessSmithayGles) -> Self {
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        let mut target = App::with_size_color(&mut native, [0, 0, 255, 255], SW, SH);
        let target_pointer = bind(&mut native, &mut target);
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
                    host::serve_authenticated(
                        peer,
                        &options(),
                        &stopping,
                        &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
                    )
                })()
            })
            .map_err(|e| e.to_string());
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
        let mut other = App::with_size_color(&mut native, [0, 255, 0, 255], SW, SH);
        let other_pointer = bind(&mut native, &mut other);
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
        let observed = ready.clone();
        let observed_candidate = candidate.clone();
        let (client_tx, client_done) = mpsc::channel();
        let client = thread::spawn(move || {
            let opts = ClientOptions {
                viewer_backend: ViewerBackend::Wayland,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: true,
                wayland_scroll: false,
                resize_source: None,
                follow_source_size: false,
                window_size: Some(WindowSize {
                    width: VW as u16,
                    height: VH as u16,
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
            };
            let result = with_wayland_pointer_observer(observed, || {
                with_wayland_pointer_candidate_observer(observed_candidate, || {
                    with_wayland_test_stream(stream, || run_client(opts))
                })
            })
            .map_err(|e| e.to_string());
            client_tx.send(result).unwrap();
        });
        Self {
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
            blue_presented: false,
            presentation_count: 0,
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
                geo.w = VW;
                geo.h = VH;
                top.with_pending_state(|s| {
                    s.size = Some((VW as i32, VH as i32).into());
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
                self.readback = Some(Readback::new(&mut self.local));
            }
            self.blue_presented = self.readback.as_mut().unwrap().blue(&mut self.local, gl);
            let feedback = std::mem::take(&mut self.feedback);
            self.presentation_count += feedback.len() as u64;
            present(&mut self.local, feedback);
            eprintln!(
                "owned viewer presentation ordinal={} actual_GL_color={}",
                self.presentation_count,
                if self.blue_presented { "blue" } else { "red" }
            );
        }
        collect(&mut self.target, self.target_pointer, &mut self.events);
        collect(&mut self.other, self.other_pointer, &mut self.unexpected);
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
