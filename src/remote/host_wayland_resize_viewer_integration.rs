//! Owned native source -> authenticated TCP -> actual Wayland viewer pixels.
//! Controlled feedback follows production GL readback, not DRM presentation.
use super::*;
use crate::backend::wayland_udev::image_copy_capture::{self, wire_test_client::Client};
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{ViewerBackend, with_wayland_test_stream},
    wayland_capture::{WaylandCapture, with_wayland_capture_frame_observer},
    wayland_viewer::with_wayland_control_frame_observer,
    window_size::WindowSize,
};
use std::{
    fs::File,
    io::Write,
    os::fd::AsRawFd,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const KEY: [u8; 32] = [0x6d; 32];
const VW: u32 = 800;
const VH: u32 = 600;
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
        wayland_resize_follow: true,
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
        allow_wayland_scroll: false,
        allow_wayland_control: false,
        scroll_copy: false,
        allow_clipboard: false,
        clipboard_mode: Default::default(),
        clipboard_direction: Default::default(),
        clipboard_file_limit: 0,
        once: true,
        stats_json: false,
    }
}
fn pixels(s: &mut Server, c: &mut Client, w: u32, h: u32, color: [u8; 4]) -> (u32, File) {
    let shm = c.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"owned-resize-pixels",
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
struct Harness {
    native: Server,
    local: Server,
    target: App,
    _other: App,
    frames: Arc<Mutex<(Option<u64>, Option<u64>)>>,
    captures: Arc<Mutex<Vec<(u32, u32, u64)>>>,
    readback: Option<Capture>,
    presented: Vec<(u64, bool)>,
    stop: Arc<AtomicBool>,
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
        let target = App::with_size_color(&mut native, [0, 0, 255, 255], 320, 240);
        let (server, stream) = UnixStream::pair().unwrap();
        native
            .display
            .handle()
            .insert_client(server, Arc::new(JwmClientState::default()))
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_guard = Stop(stop.clone());
        let stopping = stop.clone();
        let (selected_tx, selected) = mpsc::channel();
        let (host_tx, host_done) = mpsc::channel();
        let captures = Arc::new(Mutex::new(Vec::new()));
        let capture_observer = captures.clone();
        let host = thread::spawn(move || {
            let result = with_wayland_capture_frame_observer(
                capture_observer,
                || -> crate::remote::RemoteResult<()> {
                    let capture = WaylandCapture::connect_test_stream_follow(stream, 1, 0)?;
                    selected_tx.send(())?;
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let stream = loop {
                        assert!(!stopping.load(Ordering::Acquire) && Instant::now() < deadline);
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
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
                },
            )
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
                Err(mpsc::TryRecvError::Disconnected) => panic!("selection ended prematurely"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        // The bystander appears only after selection and must never supply pixels.
        let other = App::with_size_color(&mut native, [0, 255, 0, 255], 640, 360);
        let (server, stream) = UnixStream::pair().unwrap();
        local
            .display
            .handle()
            .insert_client(server, Arc::new(JwmClientState::default()))
            .unwrap();
        let frames = Arc::new(Mutex::new((None, None)));
        let observer = frames.clone();
        let (client_tx, client_done) = mpsc::channel();
        let client = thread::spawn(move || {
            let opts = ClientOptions {
                viewer_backend: ViewerBackend::Wayland,
                viewer_wayland_display: None,
                wayland_keyboard: false,
                wayland_pointer: false,
                wayland_scroll: false,
                wayland_control: false,
                resize_source: None,
                follow_source_size: false,
                window_size: Some(WindowSize {
                    width: VW as u16,
                    height: VH as u16,
                }),
                window_position: None,
                frame_limits: Default::default(),
                address: address.to_string(),
                peer_label: "owned readonly resize".into(),
                key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
                display: None,
                view_only: true,
                grab_input: false,
                clipboard: false,
                clipboard_mode: Default::default(),
                clipboard_direction: Default::default(),
                clipboard_file_limit: 0,
                escape_key: "Escape".into(),
            };
            let result = with_wayland_control_frame_observer(observer, || {
                with_wayland_test_stream(stream, || run_client(opts))
            })
            .map_err(|e| e.to_string());
            let _ = client_tx.send(result);
        });
        Self {
            native,
            local,
            target,
            _other: other,
            frames,
            captures,
            readback: None,
            presented: Vec::new(),
            stop,
            _stop: stop_guard,
            client_done,
            host_done,
            client: Some(client),
            host: Some(host),
        }
    }
    /// Return actual color only after the same production toplevel GL renderer
    /// has read the viewer's committed SHM tree. No test observer supplies pixels.
    fn read_pixels(&mut self, gl: &mut HeadlessSmithayGles) -> bool {
        if self.readback.is_none() {
            self.readback = Some(Capture::new(&mut self.local));
        }
        let c = self.readback.as_mut().unwrap();
        let (buffer, mut file) = shm_buffer_file(&mut self.local, &mut c.client, [0x5a; 4]);
        let frame = c.client.new_id();
        c.client.request(c.session, 0, &[frame]);
        c.client.request(frame, 1, &[buffer]);
        c.client.request(frame, 3, &[]);
        self.local.roundtrip();
        self.local.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.local.state,
            self.local.state.image_capture_pending.as_ref().unwrap(),
        );
        self.local.roundtrip();
        self.local.roundtrip();
        assert!(
            c.client
                .events()
                .iter()
                .any(|e| e.sender == frame && e.opcode == 3)
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len(), (VW * VH * 4) as usize);
        let center = ((VH / 2 * VW + VW / 2) * 4) as usize;
        let p = &bytes[center..center + 4];
        let blue = p[0] >= 250 && p[1] <= 3 && p[2] <= 3 && p[3] == 255;
        let red = p[2] >= 250 && p[1] <= 3 && p[0] <= 3 && p[3] == 255;
        assert!(
            red || blue,
            "selected red/blue source, never green bystander: {p:?}"
        );
        // Check the entire content interior, not a single favorable pixel.
        // Stay two pixels from the resampling boundary to avoid edge rounding.
        for y in if blue { 77..523 } else { 2..598 } {
            for x in 2..798 {
                let i = ((y * VW + x) * 4) as usize;
                let pixel = &bytes[i..i + 4];
                assert!(pixel[1] <= 3 && pixel[3] == 255);
                if blue {
                    assert!(pixel[0] >= 250 && pixel[2] <= 3);
                } else {
                    assert!(pixel[2] >= 250 && pixel[0] <= 3);
                }
            }
        }
        for y in [10, 590] {
            let i = ((y * VW + VW / 2) * 4) as usize;
            if blue {
                assert_eq!(&bytes[i..i + 3], &[0, 0, 0], "640x360 letterbox");
            } else {
                assert!(
                    bytes[i + 2] >= 250 && bytes[i] <= 3,
                    "320x240 fills viewport"
                );
            }
        }
        c.client.request(frame, 0, &[]);
        self.local.roundtrip();
        blue
    }
    fn tick(&mut self, gl: &mut HeadlessSmithayGles) {
        self.native.roundtrip();
        fulfill_test_toplevel_capture(
            &mut gl.renderer,
            &mut None,
            &self.native.state,
            self.native.state.image_capture_pending.as_ref().unwrap(),
        );
        self.native.roundtrip();
        self.local.roundtrip();
        for (&win, top) in &self.local.state.toplevels {
            if !top.is_initial_configure_sent() {
                let geo = self.local.state.window_geometry.get_mut(&win).unwrap();
                geo.w = VW;
                geo.h = VH;
                top.with_pending_state(|s| s.size = Some((VW as i32, VH as i32).into()));
                top.send_configure();
            }
        }
        self.local.roundtrip();
        let sequence = self.frames.lock().unwrap().0;
        if let Some(top) = self.local.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            if sequence.is_some_and(|n| self.presented.last().is_none_or(|old| n > old.0)) {
                let feedback = take_feedback(&surface);
                if !feedback.is_empty() {
                    assert_eq!(feedback.len(), 1);
                    let blue = self.read_pixels(gl);
                    assert_eq!(self.frames.lock().unwrap().0, sequence);
                    self.presented.push((sequence.unwrap(), blue));
                    present(&mut self.local, feedback);
                }
            }
        }
    }
    fn wait_color(&mut self, gl: &mut HeadlessSmithayGles, blue: bool, after: Option<u64>) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.tick(gl);
            if let Some(&(sequence, color)) = self.presented.last() {
                if after.is_none_or(|old| sequence > old)
                    && color == blue
                    && self.frames.lock().unwrap().1 == Some(sequence)
                {
                    return sequence;
                }
            }
            if let Ok(result) = self.client_done.try_recv() {
                panic!("viewer ended before expected pixels: {result:?}");
            }
            if let Ok(result) = self.host_done.try_recv() {
                panic!("host ended before expected pixels: {result:?}");
            }
            assert!(
                Instant::now() < deadline,
                "expected new presented color {blue}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn resize(
        &mut self,
        width: u32,
        height: u32,
        blue: bool,
        previous: Option<&image_copy_capture::PendingImageCapture>,
    ) {
        // This test compositor has no WM tick. Exercise the same geometry
        // invalidation edge before its real configure/ACK/buffer commit.
        image_copy_capture::toplevel_capture_geometry_changing(
            &mut self.native.state,
            self.target.window,
            width,
            height,
        );
        if let Some(previous) = previous {
            assert!(
                !previous.authorized(&self.native.state),
                "old guard must fail before desired size mutation or replacement commit"
            );
        }
        let geo = self
            .native
            .state
            .window_geometry
            .get_mut(&self.target.window)
            .unwrap();
        geo.w = width;
        geo.h = height;
        let top = &self.native.state.toplevels[&self.target.window];
        top.with_pending_state(|s| s.size = Some((width as i32, height as i32).into()));
        top.send_configure();
        self.native.roundtrip();
        self.native.roundtrip();
        let serial = self
            .target
            .client
            .events()
            .into_iter()
            .filter(|e| e.sender == self.target.xdg && e.opcode == 0)
            .next_back()
            .unwrap()
            .args[0];
        self.target.client.request(self.target.xdg, 4, &[serial]);
        self.target
            .client
            .request(self.target.xdg, 3, &[0, 0, width, height]);
        let (buffer, _file) = pixels(
            &mut self.native,
            &mut self.target.client,
            width,
            height,
            if blue {
                [255, 0, 0, 255]
            } else {
                [0, 0, 255, 255]
            },
        );
        self.target
            .client
            .request(self.target.surface, 1, &[buffer, 0, 0]);
        self.target
            .client
            .request(self.target.surface, 9, &[0, 0, width, height]);
        self.target.client.request(self.target.surface, 6, &[]);
        self.native.roundtrip();
        self.native.roundtrip();
    }
    fn finish(mut self, gl: &mut HeadlessSmithayGles, lock: bool) {
        if lock {
            self.native.state.session_lock_supported = true;
            let mut locker = self.native.connect();
            let manager = locker.bind("ext_session_lock_manager_v1", 1);
            let lease = locker.new_id();
            locker.request(manager, 1, &[lease]);
            self.native.roundtrip();
            self.native.roundtrip();
            assert!(self.native.state.session_locked);
            locker.request(lease, 2, &[]);
            self.native.roundtrip();
            self.native.roundtrip();
            assert!(!self.native.state.session_locked);
        } else {
            self.target.unmap(&mut self.native);
            self.target.remap(&mut self.native);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut host_done, mut client_done) = (false, false);
        loop {
            self.tick(gl);
            if !host_done {
                match self.host_done.try_recv() {
                    Ok(result) => {
                        assert!(result.is_err(), "revoked capture cannot continue");
                        host_done = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => panic!("host panicked"),
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if !client_done {
                match self.client_done.try_recv() {
                    Ok(_) => client_done = true,
                    Err(mpsc::TryRecvError::Disconnected) => panic!("viewer panicked"),
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if host_done && client_done {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "revoked source did not terminate original connection"
            );
            thread::sleep(Duration::from_millis(1));
        }
        self.stop.store(true, Ordering::Release);
        self.host.take().unwrap().join().unwrap();
        self.client.take().unwrap().join().unwrap();
    }
}
#[test]
fn actual_wayland_readonly_resize_tcp_new_pixels_and_returned_size_do_not_revive_old_layout() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for lock in [false, true] {
        let mut h = Harness::new(&mut gl);
        let first = h.wait_color(&mut gl, false, None);
        let first_capture = *h.captures.lock().unwrap().last().unwrap();
        assert_eq!((first_capture.0, first_capture.1), (320, 240));
        // Hold the actual host's queued native capture across geometry change.
        let deadline = Instant::now() + Duration::from_secs(5);
        let old = loop {
            h.native.roundtrip();
            let queue = h.native.state.image_capture_pending.as_ref().unwrap();
            if let Some(frame) = queue.lock().unwrap().pop() {
                break frame;
            }
            h.local.roundtrip();
            assert!(Instant::now() < deadline, "host must queue native frame");
            thread::sleep(Duration::from_millis(1));
        };
        assert!(old.authorized(&h.native.state));
        let old_authority = match &old.source {
            image_copy_capture::CaptureSource::Toplevel(authority) => authority.clone(),
            _ => panic!("host must capture selected toplevel, never output"),
        };
        h.resize(640, 360, true, Some(&old));
        assert!(
            !old.authorized(&h.native.state),
            "old layout invalid before any renderer write"
        );
        h.native
            .state
            .image_capture_pending
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .push(old);
        let second = h.wait_color(&mut gl, true, Some(first));
        let second_capture = *h.captures.lock().unwrap().last().unwrap();
        assert_eq!((second_capture.0, second_capture.1), (640, 360));
        assert!(second_capture.2 > first_capture.2);
        h.resize(320, 240, false, None);
        let third = h.wait_color(&mut gl, false, Some(second));
        assert!(first < second && second < third);
        let third_capture = *h.captures.lock().unwrap().last().unwrap();
        assert_eq!((third_capture.0, third_capture.1), (320, 240));
        assert!(third_capture.2 > second_capture.2);
        assert!(
            !image_copy_capture::authority_is_current(&h.native.state, &old_authority),
            "returning to the original dimensions cannot revive the old guard"
        );
        assert!(
            h.captures
                .lock()
                .unwrap()
                .windows(2)
                .all(|pair| pair[1].2 >= pair[0].2)
        );
        h.finish(&mut gl, lock);
    }
}
