//! Owned native control viewer -> authenticated TCP -> one original surface.
//! Both Wayland displays and every input resource belong to this fixture.
//! Presentation feedback is emitted only after production GL pixel readback;
//! this is not a physical input, DRM page-flip or external desktop assertion.
use super::*;
use crate::backend::wayland_udev::{
    image_copy_capture::wire_test_client::Client, remote_control, remote_keyboard, remote_pointer,
};
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{ViewerBackend, with_wayland_control_observer, with_wayland_test_stream},
    wayland_capture::WaylandCapture,
    wayland_viewer::{with_wayland_control_frame_observer, with_wayland_control_scroll_observer},
    window_size::WindowSize,
};
use smithay::reexports::{
    wayland_protocols::xdg::shell::server::xdg_toplevel::State as TopState,
    wayland_server::protocol::{
        wl_keyboard::{KeyState, WlKeyboard},
        wl_pointer::{Axis, AxisSource, ButtonState, WlPointer},
    },
};
use smithay::{backend::input::InputTime, input::pointer::MotionEvent, utils::SERIAL_COUNTER};
use std::{
    fs::File,
    io::Write,
    os::fd::AsRawFd,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
const KEY: [u8; 32] = [0x79; 32];
const SW: u32 = 200;
const SH: u32 = 150;
const VW: u32 = 400;
const VH: u32 = 300;
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
        allow_wayland_pointer: false,
        allow_wayland_scroll: scroll,
        allow_wayland_horizontal_scroll: horizontal_scroll,
        allow_wayland_control: true,
        allow_wayland_text: false,
        allow_static_lossless: false,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}
fn bind_inputs(server: &mut Server, app: &mut App, version: u32) -> (u32, u32) {
    let seat = app.client.bind("wl_seat", version);
    let pointer = app.client.new_id();
    app.client.request(seat, 0, &[pointer]);
    let keyboard = app.client.new_id();
    app.client.request(seat, 1, &[keyboard]);
    server.roundtrip();
    server.roundtrip();
    app.client.events();
    (pointer, keyboard)
}
fn focus(server: &mut Server, app: &App) {
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
    server.roundtrip();
    server.roundtrip();
}
/// Use the same complete canonical map on both owned compositors, not whatever
/// happens to be installed/configured on a developer's current desktop.
fn install_keymaps(native: &mut Server, local: &mut Server) -> u32 {
    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let mask = 1u32
        .checked_shl(keymap.mod_get_index(xkbcommon::xkb::MOD_NAME_SHIFT))
        .unwrap();
    let map = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);
    for server in [native, local] {
        server
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .set_keymap_from_string(&mut server.state, map.clone())
            .unwrap();
    }
    mask
}
#[derive(Default, Debug)]
struct Recipient {
    pointer_actions: Vec<(u16, Vec<u32>)>,
    pointer_wire: Vec<(u16, Vec<u32>)>,
    keys: Vec<(u32, u32)>,
    keyboard_wire: Vec<(u16, Vec<u32>)>,
}
impl Recipient {
    fn collect(&mut self, app: &mut App, pointer: u32, keyboard: u32) {
        for event in app.client.events() {
            assert!(
                !(event.sender == 1 && event.opcode == 0),
                "owned wl_display error"
            );
            if event.sender == pointer {
                if matches!(event.opcode, 2..=4) {
                    self.pointer_actions
                        .push((event.opcode, event.args.clone()));
                }
                self.pointer_wire.push((event.opcode, event.args));
            } else if event.sender == keyboard && matches!(event.opcode, 1..=4) {
                if event.opcode == 3 {
                    assert_eq!(event.args.len(), 4);
                    self.keys.push((event.args[2], event.args[3]));
                }
                self.keyboard_wire.push((event.opcode, event.args));
            }
        }
    }
    fn assert_no_input(&self) {
        assert!(
            self.pointer_wire.is_empty(),
            "unselected pointer received {self:?}"
        );
        assert!(
            self.keyboard_wire.is_empty(),
            "unselected keyboard received {self:?}"
        );
    }
}
fn shm(s: &mut Server, c: &mut Client, color: [u8; 4], w: u32, h: u32) -> (u32, File) {
    let shm = c.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"owned-control-pixels",
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
    fn pixels(&mut self, s: &mut Server, gl: &mut HeadlessSmithayGles) -> Vec<u8> {
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
        self.client.request(frame, 0, &[]);
        s.roundtrip();
        pixels
    }
}
struct Harness {
    native: Server,
    local: Server,
    target: App,
    other: App,
    target_pointer: u32,
    target_keyboard: u32,
    other_pointer: u32,
    other_keyboard: u32,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    held_on_enter: bool,
    shift_mask: u32,
    feedback: Vec<PresentationFeedbackCallback>,
    readback: Option<Readback>,
    received: Recipient,
    unexpected: Recipient,
    blue_presented: bool,
    presentations: u64,
    presented_sequence: Option<u64>,
    frames: Arc<Mutex<(Option<u64>, Option<u64>)>>,
    ready: Arc<Mutex<Option<u64>>>,
    scroll_frames: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
    _stop: Stop,
    client_done: mpsc::Receiver<Result<(), String>>,
    host_done: mpsc::Receiver<Result<(), String>>,
    client: Option<thread::JoinHandle<()>>,
    host: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new(gl: &mut HeadlessSmithayGles, held_on_enter: bool) -> Self {
        Self::new_with_scroll(gl, held_on_enter, false, false, 7)
    }
    fn new_with_scroll(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        host_scroll: bool,
        viewer_scroll: bool,
        recipient_version: u32,
    ) -> Self {
        Self::new_with_scroll_permissions(
            gl,
            held_on_enter,
            (host_scroll, false),
            (viewer_scroll, false),
            recipient_version,
        )
    }
    fn new_with_input_wake(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        enabled: bool,
        probe: Arc<host::InputWakeProbe>,
    ) -> Self {
        Self::new_with_wake_config(
            gl,
            held_on_enter,
            (false, false),
            (false, false),
            7,
            Some((enabled, probe)),
            None,
        )
    }
    fn new_with_scroll_permissions(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        host_scroll: (bool, bool),
        viewer_scroll: (bool, bool),
        recipient_version: u32,
    ) -> Self {
        Self::new_with_wake_config(
            gl,
            held_on_enter,
            host_scroll,
            viewer_scroll,
            recipient_version,
            None,
            None,
        )
    }
    fn new_with_proof_keyframes(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        legacy: bool,
        probe: Arc<host::ProofDeltaProbe>,
    ) -> Self {
        // Preserve the production ACK wake policy for all existing callers.
        Self::new_with_proof_policy(gl, held_on_enter, legacy, true, probe)
    }
    fn new_with_proof_policy(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        legacy_keyframe: bool,
        ack_wake: bool,
        probe: Arc<host::ProofDeltaProbe>,
    ) -> Self {
        Self::new_with_proof_schedule(gl, held_on_enter, legacy_keyframe, ack_wake, false, probe)
    }
    fn new_with_proof_schedule(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        legacy_keyframe: bool,
        ack_wake: bool,
        legacy_retained_slots: bool,
        probe: Arc<host::ProofDeltaProbe>,
    ) -> Self {
        Self::new_with_wake_config(
            gl,
            held_on_enter,
            (false, false),
            (false, false),
            7,
            None,
            Some((legacy_keyframe, ack_wake, legacy_retained_slots, probe)),
        )
    }
    fn new_with_wake_config(
        gl: &mut HeadlessSmithayGles,
        held_on_enter: bool,
        host_scroll: (bool, bool),
        viewer_scroll: (bool, bool),
        recipient_version: u32,
        wake_config: Option<(bool, Arc<host::InputWakeProbe>)>,
        proof_config: Option<(bool, bool, bool, Arc<host::ProofDeltaProbe>)>,
    ) -> Self {
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        let shift_mask = install_keymaps(&mut native, &mut local);
        let mut target = App::with_size_color(&mut native, [0, 0, 255, 255], SW, SH);
        let (target_pointer, target_keyboard) =
            bind_inputs(&mut native, &mut target, recipient_version);
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
        let (selected_tx, selected) = mpsc::channel();
        let (host_tx, host_done) = mpsc::channel();
        let host = thread::spawn(move || {
            let run = || -> crate::remote::RemoteResult<()> {
                let capture = WaylandCapture::connect_test_stream(capture_stream, 1, 0)?;
                selected_tx.send(())?;
                let deadline = Instant::now() + Duration::from_secs(10);
                let stream = loop {
                    assert!(
                        !stopping.load(Ordering::Acquire) && Instant::now() < deadline,
                        "owned accept deadline"
                    );
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
                    &options(host_scroll.0, host_scroll.1),
                    &stopping,
                    &Mutex::new(Some(host::GrantedCapture::Wayland(Box::new(capture)))),
                )
            };
            let run_with_wake = || match wake_config {
                Some((enabled, probe)) => host::with_input_wake(enabled, probe, run),
                None => run(),
            };
            let result = match proof_config {
                Some((legacy, ack_wake, legacy_slots, probe)) => {
                    host::with_legacy_retained_proof_slots(legacy_slots, || {
                        host::with_legacy_disable_ack_wake(!ack_wake, || {
                            host::with_proof_delta(legacy, probe, run_with_wake)
                        })
                    })
                }
                None => run_with_wake(),
            }
            .map_err(|e| e.to_string());
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
            native.roundtrip();
            match selected.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Disconnected) => panic!("capture selection worker ended"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "owned target selection deadline");
            thread::sleep(Duration::from_millis(1));
        }
        // Add a different, green app only after the real capture selected red.
        // Neither red nor the click-triggered blue phase can be this bystander.
        let mut other = App::with_size_color(&mut native, [0, 255, 0, 255], SW, SH);
        let (other_pointer, other_keyboard) =
            bind_inputs(&mut native, &mut other, recipient_version);
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
        let observer = ready.clone();
        let frames = Arc::new(Mutex::new((None, None)));
        let frame_observer = frames.clone();
        let scroll_frames = Arc::new(AtomicU64::new(0));
        let scroll_observer = scroll_frames.clone();
        let (client_tx, client_done) = mpsc::channel();
        let client = thread::spawn(move || {
            let opts = ClientOptions {
                viewer_backend: ViewerBackend::Wayland,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: false,
                wayland_scroll: viewer_scroll.0,
                wayland_horizontal_scroll: viewer_scroll.1,
                wayland_control: true,
                wayland_text: None,
                static_lossless: false,
                resize_source: None,
                follow_source_size: false,
                window_size: Some(WindowSize {
                    width: VW as u16,
                    height: VH as u16,
                }),
                window_position: None,
                frame_limits: Default::default(),
                address: address.to_string(),
                peer_label: "owned native control".into(),
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
            let result = with_wayland_control_observer(observer, || {
                with_wayland_control_frame_observer(frame_observer, || {
                    with_wayland_control_scroll_observer(scroll_observer, || {
                        with_wayland_test_stream(stream, || run_client(opts))
                    })
                })
            })
            .map_err(|e| e.to_string());
            let _ = client_tx.send(result);
        });
        Self {
            native,
            local,
            target,
            other,
            target_pointer,
            target_keyboard,
            other_pointer,
            other_keyboard,
            pointer: None,
            keyboard: None,
            held_on_enter,
            shift_mask,
            feedback: Vec::new(),
            readback: None,
            received: Recipient::default(),
            unexpected: Recipient::default(),
            blue_presented: false,
            presentations: 0,
            presented_sequence: None,
            frames,
            ready,
            scroll_frames,
            shutdown,
            _stop: stop,
            client_done,
            host_done,
            client: Some(client),
            host: Some(host),
        }
    }
    fn tick(&mut self, gl: &mut HeadlessSmithayGles, drawn: bool) {
        self.tick_with_inspector(gl, drawn, |pixels, _sequence| {
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
            blue
        });
    }
    /// Reuse the actual production GL readback and exact commit/presentation
    /// association. The inspector may observe pixels; it cannot manufacture
    /// feedback or alter its frame sequence. Its return value preserves the
    /// original fixture's red/blue observation for existing tests.
    fn tick_with_inspector(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        drawn: bool,
        mut inspect: impl FnMut(&[u8], u64) -> bool,
    ) {
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
            if self.pointer.is_none() || self.keyboard.is_none() {
                let owner = surface.client().unwrap();
                let pointers: Vec<_> = self
                    .local
                    .state
                    .seat
                    .get_pointer()
                    .unwrap()
                    .client_pointers(&owner)
                    .collect();
                let keyboards: Vec<_> = self
                    .local
                    .state
                    .seat
                    .get_keyboard()
                    .unwrap()
                    .client_keyboards(&owner)
                    .collect();
                // Deliver the two initial focuses together, never using an
                // arbitrary resource belonging to another native client.
                if pointers.len() == 1 && keyboards.len() == 1 {
                    assert!(self.pointer.is_none() && self.keyboard.is_none());
                    let p = pointers[0].clone();
                    let k = keyboards[0].clone();
                    let held = if self.held_on_enter {
                        42u32.to_ne_bytes().to_vec()
                    } else {
                        Vec::new()
                    };
                    k.enter(1, &surface, held);
                    k.modifiers(
                        2,
                        if self.held_on_enter {
                            self.shift_mask
                        } else {
                            0
                        },
                        0,
                        0,
                        0,
                    );
                    p.enter(3, &surface, 160.0, 120.0);
                    p.frame();
                    self.pointer = Some(p);
                    self.keyboard = Some(k);
                    self.local.roundtrip();
                } else {
                    assert!(
                        pointers.len() <= 1 && keyboards.len() <= 1,
                        "ambiguous native viewer input resources"
                    );
                }
            }
            self.feedback.extend(take_feedback(&surface));
        }
        // A feedback request may arrive just before draw returns and publishes
        // its read-only sequence observation. Wait for that exact new commit;
        // never label the current pixels with an older Ready or guessed seq.
        let committed = self.frames.lock().unwrap().0;
        if drawn
            && !self.feedback.is_empty()
            && let Some(sequence) =
                committed.filter(|n| self.presented_sequence.is_none_or(|old| *n > old))
        {
            assert_eq!(self.feedback.len(), 1, "one actual pending viewer frame");
            if self.readback.is_none() {
                self.readback = Some(Readback::new(&mut self.local));
            }
            let pixels = self.readback.as_mut().unwrap().pixels(&mut self.local, gl);
            self.blue_presented = inspect(&pixels, sequence);
            assert_eq!(
                self.frames.lock().unwrap().0,
                Some(sequence),
                "committed pixels cannot change before their controlled feedback"
            );
            let feedback = std::mem::take(&mut self.feedback);
            self.presentations += feedback.len() as u64;
            self.presented_sequence = Some(sequence);
            present(&mut self.local, feedback);
        }
        self.received
            .collect(&mut self.target, self.target_pointer, self.target_keyboard);
        self.unexpected
            .collect(&mut self.other, self.other_pointer, self.other_keyboard);
        self.unexpected.assert_no_input();
        assert!(
            !remote_keyboard::active(&self.native.state),
            "bundle must not acquire a separate keyboard lease"
        );
        assert!(
            !remote_pointer::active(&self.native.state),
            "bundle must not acquire a separate pointer lease"
        );
    }
    fn wait(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        phase: &str,
        drawn: bool,
        done: impl Fn(&Self) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !done(self) {
            self.tick(gl, drawn);
            match self.client_done.try_recv() {
                Ok(r) => panic!("{phase}: viewer ended prematurely: {r:?}"),
                Err(mpsc::TryRecvError::Disconnected) => panic!("{phase}: viewer worker panicked"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "{phase}: timed out; recipient={:?}, ready={:?}",
                self.received,
                self.ready.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn activate(&mut self, gl: &mut HeadlessSmithayGles) -> u64 {
        self.wait(gl, "initial commit, no presentation feedback", false, |s| {
            s.pointer.is_some() && s.keyboard.is_some() && !s.feedback.is_empty()
        });
        let until = Instant::now() + Duration::from_millis(150);
        while Instant::now() < until {
            self.tick(gl, false);
            self.received.assert_no_input();
            assert!(!remote_control::active(&self.native.state));
            assert_eq!(
                remote_control::attempts_for_test(&self.native.state),
                0,
                "no grant request before actual presentation"
            );
            assert!(self.ready.lock().unwrap().is_none());
            thread::sleep(Duration::from_millis(1));
        }
        self.wait(gl, "single presented control grant", true, |s| {
            remote_control::active(&s.native.state) && s.ready.lock().unwrap().is_some()
        });
        assert!(!self.blue_presented, "initial selected app is red");
        assert_eq!(remote_control::attempts_for_test(&self.native.state), 1);
        self.ready.lock().unwrap().unwrap()
    }
    fn click(&mut self) {
        let pointer = self.pointer.as_ref().unwrap();
        pointer.button(10, 10, 0x110, ButtonState::Pressed);
        pointer.frame();
        pointer.button(11, 11, 0x110, ButtonState::Released);
        pointer.frame();
        self.local.roundtrip();
    }
    fn key(&mut self, serial: u32, code: u32, down: bool) {
        let k = self.keyboard.as_ref().unwrap();
        k.key(
            serial,
            serial,
            code,
            if down {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
        );
        if code == 42 {
            k.modifiers(serial + 1, if down { self.shift_mask } else { 0 }, 0, 0, 0);
        }
    }
    fn repaint(&mut self, blue: bool) {
        let (buffer, _file) = shm(
            &mut self.native,
            &mut self.target.client,
            if blue {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            },
            SW,
            SH,
        );
        self.target
            .client
            .request(self.target.surface, 1, &[buffer, 0, 0]);
        self.target
            .client
            .request(self.target.surface, 2, &[0, 0, SW, SH]);
        self.target.client.request(self.target.surface, 6, &[]);
        self.native.roundtrip();
        self.native.roundtrip();
    }
    fn click_repaint_type(&mut self, gl: &mut HeadlessSmithayGles) {
        let first = self.activate(gl);
        self.click();
        self.wait(
            gl,
            "atomic click delivered to selected native app",
            true,
            |s| s.received.pointer_actions.len() >= 3,
        );
        let events = &self.received.pointer_actions;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].0, 2);
        assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
        assert_eq!(events[1].0, 3);
        assert_eq!(&events[1].1[2..], &[0x110, 1]);
        assert_eq!(events[2].0, 3);
        assert_eq!(&events[2].1[2..], &[0x110, 0]);
        assert!(
            self.received.keys.is_empty(),
            "no type before click feedback"
        );
        let presentations = self.presentations;
        // The owned app changes its actual SHM pixels only after the real click.
        self.repaint(true);
        self.wait(
            gl,
            "new blue pixels returned through TCP and real GL before type",
            true,
            |s| {
                assert!(s.received.keys.is_empty());
                s.blue_presented && s.presentations > presentations
            },
        );
        let blue_sequence = self.presented_sequence.unwrap();
        assert!(blue_sequence > first);
        // Withhold further feedback while checking the exact blue completion.
        // A late Ready belonging to a prior red frame cannot satisfy this gate.
        self.wait(
            gl,
            "exact blue frame consumed and acknowledged Ready",
            false,
            |s| {
                assert!(s.received.keys.is_empty());
                s.frames.lock().unwrap().1 == Some(blue_sequence)
                    && *s.ready.lock().unwrap() == Some(blue_sequence)
            },
        );
        self.key(20, 30, true);
        self.key(22, 30, false);
        self.wait(
            gl,
            "balanced A reaches same native target after blue feedback",
            true,
            |s| s.received.keys.len() >= 2,
        );
        assert_eq!(self.received.keys, [(30, 1), (30, 0)]);
        assert_eq!(self.received.pointer_actions.len(), 3);
        assert!(remote_control::active(&self.native.state));
        assert_eq!(
            remote_control::attempts_for_test(&self.native.state),
            1,
            "keyboard and pointer share one lease"
        );
        eprintln!(
            "owned control TCP: click -> actual blue GL/presented -> balanced A; bystander input zero"
        );
    }
    fn wheel(&mut self, step: i32) {
        self.wheel_axis(step, Axis::VerticalScroll);
    }
    fn wheel_axis(&mut self, step: i32, axis: Axis) {
        assert!(matches!(step, -1 | 1));
        let pointer = self.pointer.as_ref().unwrap();
        assert_eq!(
            pointer.version(),
            7,
            "viewer uses explicit discrete v7 wheel"
        );
        // Enter already supplied the surface-local point. Do not precede this
        // with motion: held-key negatives must reject the axis itself.
        pointer.axis_source(AxisSource::Wheel);
        pointer.axis_discrete(axis, step);
        pointer.axis(100, axis, f64::from(step) * 10.0);
        pointer.frame();
        self.local.roundtrip();
    }
    fn waiting_wheels_processed(&mut self, gl: &mut HeadlessSmithayGles, count: u64) {
        let before = self.received.pointer_wire.clone();
        let keys = self.received.keys.clone();
        self.wait(
            gl,
            "actual Waiting wheel groups consumed without input",
            false,
            |s| {
                assert_eq!(s.received.pointer_wire, before);
                assert_eq!(s.received.keys, keys);
                assert!(s.ready.lock().unwrap().is_none());
                s.scroll_frames.load(Ordering::Acquire) >= count
            },
        );
        assert_eq!(self.scroll_frames.load(Ordering::Acquire), count);
    }
    fn repaint_and_ready(&mut self, gl: &mut HeadlessSmithayGles, blue: bool) -> u64 {
        let before = self.received.pointer_wire.clone();
        let keys = self.received.keys.clone();
        let presentations = self.presentations;
        let previous_sequence = self.presented_sequence.unwrap();
        assert_ne!(
            self.blue_presented, blue,
            "actual new pixel phase, not unchanged capture"
        );
        self.repaint(blue);
        self.wait(
            gl,
            "post-action pixels traverse capture TCP and actual viewer GL",
            true,
            |s| {
                assert_eq!(
                    s.received.pointer_wire, before,
                    "Waiting wheel was replayed during repaint"
                );
                assert_eq!(s.received.keys, keys);
                s.blue_presented == blue && s.presentations > presentations
            },
        );
        let sequence = self.presented_sequence.unwrap();
        assert!(sequence > previous_sequence);
        self.wait(
            gl,
            "same exact new pixels presented and Ready",
            false,
            |s| {
                assert_eq!(
                    s.received.pointer_wire, before,
                    "Waiting wheel was replayed at Ready"
                );
                assert_eq!(s.received.keys, keys);
                s.frames.lock().unwrap().1 == Some(sequence)
                    && *s.ready.lock().unwrap() == Some(sequence)
            },
        );
        assert!(remote_control::active(&self.native.state));
        assert_eq!(remote_control::attempts_for_test(&self.native.state), 1);
        sequence
    }
    fn denied_scroll(self, gl: &mut HeadlessSmithayGles) {
        self.denied_scroll_with_error(gl, "Wayland scroll permission was not mutually negotiated");
    }
    fn denied_scroll_with_error(mut self, gl: &mut HeadlessSmithayGles, expected: &str) {
        // Negotiation can correctly fail before a viewer surface exists. This
        // is separate from rejected(), whose local-input cases require both
        // real owned device resources and a specific local failure cause.
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            self.tick(gl, false);
            self.received.assert_no_input();
            assert!(!remote_control::active(&self.native.state));
            assert_eq!(remote_control::attempts_for_test(&self.native.state), 0);
            match self.client_done.try_recv() {
                Ok(result) => {
                    let error =
                        result.expect_err("missing host scroll consent must fail negotiation");
                    assert!(
                        error.contains(expected),
                        "expected exact permission failure, not timeout/transport: {error}"
                    );
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("scroll negotiation worker panicked")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "scroll permission denial did not complete"
            );
            thread::sleep(Duration::from_millis(1));
        }
        self.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.tick(gl, false);
            self.received.assert_no_input();
            assert!(!remote_control::active(&self.native.state));
            assert_eq!(remote_control::attempts_for_test(&self.native.state), 0);
            match self.host_done.try_recv() {
                Ok(result) => {
                    eprintln!("owned denied-scroll host cleanup: {result:?}");
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("denied-scroll host worker panicked")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "denied-scroll host cleanup deadline"
            );
            thread::sleep(Duration::from_millis(1));
        }
        self.client.take().unwrap().join().unwrap();
        self.host.take().unwrap().join().unwrap();
    }
    /// Wait for the production adapter's explicit local rejection, not any
    /// successful close, missing channel, panic, test timeout or input fallback.
    fn rejected(
        mut self,
        gl: &mut HeadlessSmithayGles,
        expected_keys: &[(u32, u32)],
        expected_pointer: &[(u16, Vec<u32>)],
        attempts: u32,
        expected_reason: &str,
    ) {
        let expected_pointer_wire = self.received.pointer_wire.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.tick(gl, false);
            assert_eq!(
                self.received.pointer_wire, expected_pointer_wire,
                "rejected input must not leak even partial pointer protocol events"
            );
            assert_eq!(self.received.pointer_actions, expected_pointer);
            assert!(expected_keys.starts_with(&self.received.keys));
            match self.client_done.try_recv() {
                Ok(result) => {
                    let error =
                        result.expect_err("unsafe native input must fail, not report success");
                    assert!(
                        self.pointer.is_some() && self.keyboard.is_some(),
                        "negative case must reach the owned dual input resources"
                    );
                    assert!(
                        error.contains(expected_reason),
                        "expected local input rejection {expected_reason:?}, not an unrelated failure: {error}"
                    );
                    eprintln!("owned control adapter rejected: {error}");
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("native control viewer worker panicked")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(
                Instant::now() < deadline,
                "native control did not reject unsafe local input"
            );
            thread::sleep(Duration::from_millis(1));
        }
        self.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut host_finished = false;
        loop {
            self.tick(gl, false);
            assert_eq!(
                self.received.pointer_wire, expected_pointer_wire,
                "rejected input must not leak even partial pointer protocol events"
            );
            assert_eq!(self.received.pointer_actions, expected_pointer);
            assert!(expected_keys.starts_with(&self.received.keys));
            if !host_finished {
                match self.host_done.try_recv() {
                    Ok(result) => {
                        eprintln!(
                            "owned control host after explicit adapter rejection: {result:?}"
                        );
                        host_finished = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("native control host worker panicked")
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            assert_eq!(
                remote_control::attempts_for_test(&self.native.state),
                attempts,
                "terminal bundle cannot regrant after renewed local focus"
            );
            if host_finished
                && !remote_control::active(&self.native.state)
                && self.received.keys == expected_keys
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native bundle cleanup, balanced keys and host exit must all finish"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(self.received.keys, expected_keys);
        self.unexpected.assert_no_input();
        self.client.take().unwrap().join().unwrap();
        self.host.take().unwrap().join().unwrap();
    }
}
#[test]
fn actual_wayland_control_viewer_tcp_click_repaint_type_and_either_focus_loss_revoke_together() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for keyboard_leave in [true, false] {
        let mut h = Harness::new(&mut gl, false);
        h.click_repaint_type(&mut gl);
        let before = h.received.pointer_actions.clone();
        let surface = viewer_surface(&h.local);
        if keyboard_leave {
            let keyboard = h.keyboard.as_ref().unwrap();
            keyboard.leave(40, &surface);
            keyboard.enter(41, &surface, Vec::new());
            keyboard.modifiers(42, 0, 0, 0, 0);
        } else {
            let pointer = h.pointer.as_ref().unwrap();
            pointer.leave(40, &surface);
            pointer.enter(41, &surface, 160.0, 120.0);
            pointer.frame();
        }
        // Same-dispatch clean-looking focus and both kinds of input cannot
        // reacquire either half of a revoked bundle.
        h.key(50, 30, true);
        h.key(52, 30, false);
        h.click();
        h.rejected(
            &mut gl,
            &[(30, 1), (30, 0)],
            &before,
            1,
            if keyboard_leave {
                "Wayland keyboard focus left"
            } else {
                "Wayland pointer left own surface"
            },
        );
    }
}
#[test]
fn actual_wayland_control_viewer_tcp_held_enter_never_requests_bundle() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let h = Harness::new(&mut gl, true);
    h.rejected(
        &mut gl,
        &[],
        &[],
        0,
        "Wayland keyboard enter is not a fresh clean own surface",
    );
}
#[test]
fn actual_wayland_control_viewer_tcp_shift_survives_new_pixels_then_types_and_releases() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(&mut gl, false);
    h.click_repaint_type(&mut gl);
    let before = h.received.pointer_actions.clone();
    h.key(60, 42, true);
    h.wait(
        &mut gl,
        "Shift reaches native target before redraw",
        true,
        |s| s.received.keys.len() >= 3,
    );
    assert_eq!(h.received.keys, [(30, 1), (30, 0), (42, 1)]);
    let presentations = h.presentations;
    h.repaint(false);
    h.wait(
        &mut gl,
        "held Shift survives actual new red viewer pixels and feedback",
        true,
        |s| {
            assert!(remote_control::active(&s.native.state));
            assert_eq!(s.received.keys, [(30, 1), (30, 0), (42, 1)]);
            assert_eq!(s.received.pointer_actions, before);
            s.presentations > presentations && !s.blue_presented
        },
    );
    let red_sequence = h.presented_sequence.unwrap();
    h.wait(
        &mut gl,
        "held-key client consumes this exact new red presentation",
        false,
        |s| {
            assert!(remote_control::active(&s.native.state));
            assert_eq!(s.received.keys, [(30, 1), (30, 0), (42, 1)]);
            s.frames.lock().unwrap().1 == Some(red_sequence)
        },
    );
    h.key(70, 30, true);
    h.key(72, 30, false);
    h.key(74, 42, false);
    let expected = [(30, 1), (30, 0), (42, 1), (30, 1), (30, 0), (42, 0)];
    h.wait(
        &mut gl,
        "Shift+A and release after a real intermediate frame",
        true,
        |s| s.received.keys.len() >= expected.len(),
    );
    assert_eq!(h.received.keys, expected);
    assert_eq!(h.received.pointer_actions, before);
    assert!(remote_control::active(&h.native.state));
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    // Close with the same explicit local revoke used in the other positive
    // cases, preserving exact recipient assertions throughout host cleanup.
    let surface = viewer_surface(&h.local);
    h.keyboard.as_ref().unwrap().leave(80, &surface);
    h.local.roundtrip();
    h.rejected(
        &mut gl,
        &expected,
        &before,
        1,
        "Wayland keyboard focus left",
    );
}
#[test]
fn actual_wayland_control_viewer_tcp_held_key_pointer_rejection_cleans_original_recipient() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(&mut gl, false);
    h.click_repaint_type(&mut gl);
    let before = h.received.pointer_actions.clone();
    h.key(60, 42, true);
    h.wait(
        &mut gl,
        "Shift positive control delivered before pointer conflict",
        true,
        |s| s.received.keys.len() >= 3,
    );
    assert_eq!(h.received.keys, [(30, 1), (30, 0), (42, 1)]);
    assert!(remote_control::active(&h.native.state));
    let pointer = h.pointer.as_ref().unwrap();
    pointer.motion(62, 170.0, 125.0);
    pointer.frame();
    h.local.roundtrip();
    // A native cleanup release must target the original key recipient only.
    // Do not send a local Shift-up that could hide absent bundle cleanup.
    h.rejected(
        &mut gl,
        &[(30, 1), (30, 0), (42, 1), (42, 0)],
        &before,
        1,
        "Wayland control pointer overlaps held keys",
    );
}

fn assert_control_wheel_120(events: &[(u16, Vec<u32>)], step: i32, axis: Axis) {
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        [2, 6, 9, 4, 5],
        "one exact v9 recipient wheel group, no duplicate axis or button"
    );
    assert_eq!(&events[0].1[1..], &[80 * 256, 60 * 256]);
    assert_eq!(events[1].1, [0]); // wheel source
    assert_eq!(events[2].1, [axis as u32, (step * 120) as u32]);
    assert_eq!(&events[3].1[1..], &[axis as u32, (step * 10 * 256) as u32]);
    assert!(events[4].1.is_empty());
}

#[test]
fn actual_wayland_control_scroll_tcp_click_type_both_directions_and_no_waiting_replay() {
    exercise_control_scroll_tcp(false);
}

#[test]
fn actual_wayland_control_horizontal_scroll_tcp_click_type_both_directions_no_waiting_replay() {
    exercise_control_scroll_tcp(true);
}

fn exercise_control_scroll_tcp(horizontal: bool) {
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
        false,
        (!horizontal, horizontal),
        (!horizontal, horizontal),
        9,
    );
    h.wait(
        &mut gl,
        "owned scroll viewer before initial presentation",
        false,
        |s| s.pointer.is_some() && s.keyboard.is_some() && !s.feedback.is_empty(),
    );
    h.wheel_axis(-1, axis);
    h.wheel_axis(1, axis);
    h.waiting_wheels_processed(&mut gl, 2);
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 0);
    h.click_repaint_type(&mut gl);
    assert_eq!(
        h.received.pointer_actions.len(),
        3,
        "initial Waiting axes not replayed at grant"
    );
    // Keyboard invalidated its old pointer ticket. Repaint before the first
    // scroll instead of borrowing the pre-key blue Ready.
    h.repaint_and_ready(&mut gl, false);
    let mut processed = 2;
    for step in [-1, 1] {
        let before = h.received.pointer_wire.len();
        h.wheel_axis(step, axis);
        processed += 1;
        h.wait(
            &mut gl,
            "native selected recipient receives real +/-120 axis",
            false,
            |s| s.received.pointer_wire.len() >= before + 5,
        );
        assert_control_wheel_120(&h.received.pointer_wire[before..], step, axis);
        h.wait(&mut gl, "axis consumes shared Ready", false, |s| {
            s.ready.lock().unwrap().is_none()
                && s.scroll_frames.load(Ordering::Acquire) == processed
        });
        let after_axis = h.received.pointer_wire.clone();
        h.wheel_axis(1, axis);
        h.wheel_axis(-1, axis);
        h.wheel_axis(1, axis);
        processed += 3;
        h.waiting_wheels_processed(&mut gl, processed);
        assert_eq!(h.received.pointer_wire, after_axis);
        let next = h.repaint_and_ready(&mut gl, true);
        assert_eq!(
            h.received.pointer_wire, after_axis,
            "Waiting axes cannot turn into late input after a new exact frame"
        );
        eprintln!(
            "owned control scroll {step}: exact new blue presented Ready {next}, bystander zero"
        );
        if step == -1 {
            // Same authenticated session and same native bundle: scroll then
            // type B on the new screen, then another scroll after new evidence.
            h.key(120, 48, true);
            h.key(122, 48, false);
            h.wait(
                &mut gl,
                "post-scroll keyboard uses same original recipient",
                true,
                |s| s.received.keys.len() >= 4,
            );
            assert_eq!(h.received.keys, [(30, 1), (30, 0), (48, 1), (48, 0)]);
            assert_eq!(h.received.pointer_wire, after_axis);
            h.repaint_and_ready(&mut gl, false);
        }
    }
    assert_eq!(h.received.keys, [(30, 1), (30, 0), (48, 1), (48, 0)]);
    if horizontal {
        // The new exact presentation is an input barrier, not scroll-only:
        // keyboard B above and this click still use the original bundle.
        let before = h.received.pointer_actions.len();
        h.click();
        h.wait(
            &mut gl,
            "post-horizontal exact Ready admits same-bundle click",
            false,
            |s| s.received.pointer_actions.len() == before + 3,
        );
        let click = &h.received.pointer_actions[before..];
        assert_eq!(click.iter().map(|e| e.0).collect::<Vec<_>>(), [2, 3, 3]);
        assert_eq!(&click[1].1[2..], &[0x110, 1]);
        assert_eq!(&click[2].1[2..], &[0x110, 0]);
    }
    assert_eq!(remote_control::attempts_for_test(&h.native.state), 1);
    let before = h.received.pointer_actions.clone();
    let surface = viewer_surface(&h.local);
    h.keyboard.as_ref().unwrap().leave(140, &surface);
    h.local.roundtrip();
    h.rejected(
        &mut gl,
        &[(30, 1), (30, 0), (48, 1), (48, 0)],
        &before,
        1,
        "Wayland keyboard focus left",
    );
}

#[test]
fn actual_wayland_control_scroll_tcp_requires_both_explicit_permissions() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    Harness::new_with_scroll(&mut gl, false, false, true, 9).denied_scroll(&mut gl);
    let mut h = Harness::new_with_scroll(&mut gl, false, true, false, 9);
    h.activate(&mut gl);
    h.wheel(1); // host permission alone cannot supply the viewer's omitted intent
    h.rejected(
        &mut gl,
        &[],
        &[],
        1,
        "Wayland scroll lacks permission or conflicts with held button",
    );
}

#[test]
fn actual_wayland_control_scroll_tcp_held_key_or_left_button_revokes_whole_bundle() {
    exercise_control_scroll_held(false);
}

#[test]
fn actual_wayland_control_horizontal_scroll_tcp_held_key_or_left_button_revokes_whole_bundle() {
    exercise_control_scroll_held(true);
}

fn exercise_control_scroll_held(horizontal: bool) {
    let axis = if horizontal {
        Axis::HorizontalScroll
    } else {
        Axis::VerticalScroll
    };
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for held_key in [true, false] {
        let mut h = Harness::new_with_scroll_permissions(
            &mut gl,
            false,
            (!horizontal, horizontal),
            (!horizontal, horizontal),
            9,
        );
        h.activate(&mut gl);
        if held_key {
            h.key(160, 42, true);
            h.wait(
                &mut gl,
                "real original Shift-down before axis conflict",
                false,
                |s| !s.received.keys.is_empty(),
            );
            assert_eq!(h.received.keys, [(42, 1)]);
            assert!(remote_control::active(&h.native.state));
        } else {
            let pointer = h.pointer.as_ref().unwrap();
            pointer.button(161, 161, 0x110, ButtonState::Pressed);
            pointer.frame();
            // Ordered on the same real Wayland connection. No fake host key
            // state, synthetic grant or local left-up conceals cancellation.
        }
        h.wheel_axis(-1, axis);
        h.rejected(
            &mut gl,
            if held_key { &[(42, 1), (42, 0)] } else { &[] },
            &[],
            1,
            if held_key {
                "Wayland control pointer overlaps held keys"
            } else {
                "Wayland scroll lacks permission or conflicts with held button"
            },
        );
    }
}

#[test]
fn actual_wayland_control_horizontal_scroll_tcp_requires_each_permission() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    Harness::new_with_scroll_permissions(&mut gl, false, (true, false), (true, true), 9)
        .denied_scroll_with_error(
            &mut gl,
            "Wayland horizontal scroll permission was not mutually negotiated",
        );
    let mut h =
        Harness::new_with_scroll_permissions(&mut gl, false, (true, true), (true, false), 9);
    h.activate(&mut gl);
    h.wheel_axis(1, Axis::HorizontalScroll);
    h.rejected(&mut gl, &[], &[], 1, "unapproved Wayland wheel notch axis");
}

#[test]
fn actual_wayland_control_horizontal_scroll_tcp_mixed_axis_revokes_without_input() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new_with_scroll_permissions(&mut gl, false, (true, true), (true, true), 9);
    h.activate(&mut gl);
    let pointer = h.pointer.as_ref().unwrap();
    pointer.axis_source(AxisSource::Wheel);
    pointer.axis_discrete(Axis::VerticalScroll, 1);
    pointer.axis(190, Axis::VerticalScroll, 10.0);
    pointer.axis_discrete(Axis::HorizontalScroll, -1);
    pointer.axis(190, Axis::HorizontalScroll, -10.0);
    pointer.frame();
    h.local.roundtrip();
    h.rejected(
        &mut gl,
        &[],
        &[],
        1,
        "duplicate or mixed Wayland wheel notch",
    );
}

#[test]
fn actual_control_zero_dirty_proof_requires_new_exact_presentation() {
    exercise_zero_dirty_proof_after_hold(Duration::ZERO);
}
#[test]
fn actual_control_retained_hold_click_requires_fresh_proof_and_presentation() {
    exercise_zero_dirty_proof_after_hold(Duration::from_millis(250));
}
fn exercise_zero_dirty_proof_after_hold(hold: Duration) {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let probe = Arc::new(host::ProofDeltaProbe::default());
    let mut h = Harness::new_with_proof_keyframes(&mut gl, false, false, probe.clone());
    let first = h.activate(&mut gl);
    // Hold across multiple 30 FPS slots without consuming or releasing the
    // current proof. This stays below the native one-second proof lifetime.
    let retained_evidence_count = probe.snapshot().len();
    let held_until = Instant::now() + hold;
    while Instant::now() < held_until {
        h.tick(&mut gl, false);
        assert_eq!(*h.ready.lock().unwrap(), Some(first));
        assert_eq!(h.frames.lock().unwrap().0, Some(first));
        h.received.assert_no_input();
        assert_eq!(
            probe.snapshot().len(),
            retained_evidence_count,
            "retained wait must not duplicate a proof"
        );
        thread::sleep(Duration::from_millis(1));
    }
    h.click();
    // Consume the old ticket without changing the application's red pixels.
    // Keep the next real viewer commit's feedback pending.
    h.wait(
        &mut gl,
        "new unchanged proof committed without presentation",
        false,
        |s| {
            s.received.pointer_actions.len() == 3
                && s.frames.lock().unwrap().0.is_some_and(|seq| {
                    seq > first && probe.snapshot().iter().any(|frame| frame.sequence == seq)
                })
                && !s.feedback.is_empty()
                && s.ready.lock().unwrap().is_none()
        },
    );
    let next = h.frames.lock().unwrap().0.unwrap();
    let sent = probe.snapshot();
    let old = sent
        .iter()
        .find(|frame| frame.sequence == first)
        .expect("first proof wire evidence");
    let new = sent
        .iter()
        .find(|frame| frame.sequence == next)
        .expect("unchanged proof wire evidence");
    assert!(new.native_serial > old.native_serial);
    assert_eq!(
        (new.source_epoch, new.geometry_epoch),
        (old.source_epoch, old.geometry_epoch)
    );
    assert!(!new.keyframe);
    assert_eq!(
        new.dirty_tiles, 0,
        "test must exercise an actual zero-dirty record"
    );
    assert!(new.total_tiles > 0);
    // The previous, legitimately acknowledged frame must not authorize this
    // newer native proof. No replayed ACK is manufactured by the fixture.
    let until = Instant::now() + Duration::from_millis(120);
    while Instant::now() < until {
        h.tick(&mut gl, false);
        assert_eq!(h.frames.lock().unwrap().1, Some(first));
        assert!(h.ready.lock().unwrap().is_none());
        assert_eq!(h.received.pointer_actions.len(), 3);
        assert!(h.received.keys.is_empty());
        thread::sleep(Duration::from_millis(1));
    }
    h.wait(
        &mut gl,
        "unchanged pixels actually GL verified and exactly presented",
        true,
        |s| s.presented_sequence == Some(next),
    );
    h.wait(
        &mut gl,
        "new exact unchanged presentation earns a new ticket",
        false,
        |s| s.frames.lock().unwrap().1 == Some(next) && *s.ready.lock().unwrap() == Some(next),
    );
    assert!(!h.blue_presented);
    h.click();
    h.wait(
        &mut gl,
        "second authorized click after exact new feedback",
        false,
        |s| s.received.pointer_actions.len() == 6,
    );
    let before = h.received.pointer_actions.clone();
    let surface = viewer_surface(&h.local);
    h.pointer.as_ref().unwrap().leave(80, &surface);
    h.pointer.as_ref().unwrap().frame();
    h.local.roundtrip();
    h.rejected(&mut gl, &[], &before, 1, "Wayland pointer left own surface");
}

#[path = "host_wayland_control_latency_benchmark.rs"]
mod latency_benchmark;
