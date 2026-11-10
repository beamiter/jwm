//! Opt-in readonly popup family -> authenticated TCP -> actual Wayland viewer pixels.
//! Controlled feedback follows production GL readback, not DRM presentation.
use super::*;
use crate::backend::wayland_udev::image_copy_capture::popup_tests::Popup;
use crate::remote::{
    client::{ClientOptions, run_client},
    host,
    viewer_backend::{ViewerBackend, with_wayland_test_stream},
    wayland_capture::{WaylandCapture, with_wayland_capture_frame_observer},
    wayland_viewer::with_wayland_control_frame_observer,
    window_size::WindowSize,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

const KEY: [u8; 32] = [0x7f; 32];
const VW: u32 = 800;
const VH: u32 = 600;
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn options(popups: bool) -> host::HostOptions {
    host::HostOptions {
        frame_limits: Default::default(),
        listen: "127.0.0.1:0".into(),
        key: crate::remote::key::KeySource::Bytes(KEY.to_vec()),
        display: None,
        wayland_select: true,
        wayland_display: None,
        wayland_resize_follow: false,
        wayland_popups: popups,
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
        allow_wayland_horizontal_scroll: false,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pixels {
    RootOnly,
    PopupInitial,
    PopupClipped,
}
struct Harness {
    native: Server,
    local: Server,
    target: App,
    popup: Option<Popup>,
    popups: bool,
    _other: App,
    frames: Arc<Mutex<(Option<u64>, Option<u64>)>>,
    captures: Arc<Mutex<Vec<(u32, u32, u64)>>>,
    readback: Option<Capture>,
    presented: Vec<(u64, Pixels)>,
    stop: Arc<AtomicBool>,
    _stop: Stop,
    client_done: mpsc::Receiver<Result<(), String>>,
    host_done: mpsc::Receiver<Result<(), String>>,
    client: Option<thread::JoinHandle<()>>,
    host: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new(gl: &mut HeadlessSmithayGles, popups: bool) -> Self {
        let mut native = Server::new();
        let mut local = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        local.state.image_capture_pending = Some(init_image_copy_capture(&local.display.handle()));
        register_test_presentation(&mut local);
        let mut target = App::with_size_color(&mut native, [0, 0, 255, 255], 320, 240);
        // Explicit root geometry fixes the 320x240 authority canvas. Popup
        // inclusion must not grow it to the surrounding desktop or bystander.
        target.client.request(target.xdg, 3, &[0, 0, 320, 240]);
        target.client.request(target.surface, 6, &[]);
        native.roundtrip();
        native.roundtrip();
        assert!(
            target
                .client
                .events()
                .iter()
                .all(|e| !(e.sender == 1 && e.opcode == 0))
        );
        let surface = native.state.surface_for_window(target.window).unwrap();
        let committed = native.state.surface_window_geometry_rect(&surface).unwrap();
        assert_eq!((committed.size.w, committed.size.h), (320, 240));
        smithay::wayland::shm::with_buffer_contents(&committed_buffer(&surface), |_, _, data| {
            assert_eq!((data.width, data.height), (320, 240));
        })
        .unwrap();
        let geometry = native.state.window_geometry[&target.window];
        assert_eq!((geometry.w, geometry.h), (320, 240));
        let root_xdg = target.xdg;
        let popup = Popup::new(
            &mut native,
            &mut target.client,
            root_xdg,
            (40, 40, 80, 60),
            [255, 0, 0, 255],
        );
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
                    let capture = if popups {
                        WaylandCapture::connect_test_stream_popups(stream, 1, 0)?
                    } else {
                        WaylandCapture::connect_test_stream(stream, 1, 0)?
                    };
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
                        &options(popups),
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
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Selection can fail before its success signal. Preserve the
                    // adapter's actual error instead of hiding it behind mpsc.
                    let failure = host_done.recv_timeout(Duration::from_secs(1));
                    panic!("native capture selection failed: {failure:?}");
                }
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
                wayland_horizontal_scroll: false,
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
                peer_label: "owned readonly popup family".into(),
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
            popup: Some(popup),
            popups,
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
    fn read_pixels(&mut self, gl: &mut HeadlessSmithayGles) -> Pixels {
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
        let pixel = |x: u32, y: u32| {
            let i = ((y * VW + x) * 4) as usize;
            &bytes[i..i + 4]
        };
        let is_blue = |p: &[u8]| p[0] >= 220 && p[1] <= 35 && p[2] <= 35 && p[3] == 255;
        let initial = is_blue(pixel(200, 175));
        let clipped = is_blue(pixel(725, 525));
        let actual = match (initial, clipped) {
            (true, false) => Pixels::PopupInitial,
            (false, true) => Pixels::PopupClipped,
            (false, false) => Pixels::RootOnly,
            (true, true) => panic!("popup movement left its old blue pixels behind"),
        };
        // Root320x240 fills viewer800x600. Check the whole content interior,
        // excluding narrow JPEG/resampling boundary bands, not only probes.
        let rectangle = match actual {
            Pixels::RootOnly => None,
            Pixels::PopupInitial => Some((100i32, 100i32, 300i32, 250i32)),
            // Popup80x60 at260,180 crosses the root right edge by20 pixels.
            Pixels::PopupClipped => Some((650, 450, 800, 600)),
        };
        for y in 2i32..598 {
            for x in 2i32..798 {
                let p = pixel(x as u32, y as u32);
                assert!(
                    p[1] <= 35 && p[3] == 255,
                    "green bystander/invalid alpha at{x},{y}: {p:?}"
                );
                if rectangle.is_some_and(|(left, top, right, bottom)| {
                    ((x - left).abs() <= 8 || (x - right).abs() <= 8)
                        && y >= top - 8
                        && y <= bottom + 8
                        || ((y - top).abs() <= 8 || (y - bottom).abs() <= 8)
                            && x >= left - 8
                            && x <= right + 8
                }) {
                    continue;
                }
                let blue = rectangle.is_some_and(|(left, top, right, bottom)| {
                    x > left && x < right && y > top && y < bottom
                });
                if blue {
                    assert!(is_blue(p), "popup pixel missing at{x},{y}: {p:?}");
                } else {
                    assert!(
                        p[2] >= 220 && p[0] <= 35,
                        "root pixel/residual popup at{x},{y}: {p:?}"
                    );
                }
            }
        }
        if !self.popups {
            assert_eq!(
                actual,
                Pixels::RootOnly,
                "default capture must never include popup pixels"
            );
        }
        c.client.request(frame, 0, &[]);
        self.local.roundtrip();
        actual
    }
    fn tick(&mut self, gl: &mut HeadlessSmithayGles) {
        use crate::backend::wayland_udev::{remote_control, remote_keyboard, remote_pointer};
        assert!(!remote_control::active(&self.native.state));
        assert!(!remote_keyboard::active(&self.native.state));
        assert!(!remote_pointer::active(&self.native.state));
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
                    let pixels = self.read_pixels(gl);
                    assert_eq!(self.frames.lock().unwrap().0, sequence);
                    self.presented.push((sequence.unwrap(), pixels));
                    present(&mut self.local, feedback);
                }
            }
        }
    }
    fn wait_pixels(
        &mut self,
        gl: &mut HeadlessSmithayGles,
        expected: Pixels,
        after: Option<u64>,
    ) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.tick(gl);
            if let Some(&(sequence, color)) = self.presented.last() {
                if after.is_none_or(|old| sequence > old)
                    && color == expected
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
                "expected new exact presented popup pixels {expected:?}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn move_popup(&mut self) {
        self.popup.as_mut().unwrap().reposition(
            &mut self.native,
            &mut self.target.client,
            (260, 180),
        );
    }
    fn destroy_popup(&mut self) {
        self.popup
            .take()
            .unwrap()
            .destroy(&mut self.native, &mut self.target.client);
    }
    fn assert_fixed_canvas(&self) {
        let captures = self.captures.lock().unwrap();
        assert!(!captures.is_empty());
        assert!(
            captures
                .iter()
                .all(|frame| (frame.0, frame.1) == (320, 240)),
            "popup must not expand the root canvas: {captures:?}"
        );
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
fn actual_wayland_readonly_popup_tcp_selected_family_pixels_move_clip_destroy_no_residue() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(&mut gl, true);
    let first = h.wait_pixels(&mut gl, Pixels::PopupInitial, None);
    h.move_popup();
    let moved = h.wait_pixels(&mut gl, Pixels::PopupClipped, Some(first));
    h.destroy_popup();
    let destroyed = h.wait_pixels(&mut gl, Pixels::RootOnly, Some(moved));
    assert!(first < moved && moved < destroyed);
    h.assert_fixed_canvas();
    h.finish(&mut gl, false);
}

#[test]
fn actual_wayland_readonly_popup_tcp_default_excludes_popup_pixels_and_root_unmap_revokes() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(&mut gl, false);
    let first = h.wait_pixels(&mut gl, Pixels::RootOnly, None);
    h.move_popup();
    // The normal source must remain root-only, not inherit popup opt-in.
    // A forced root repaint supplies new evidence even when popup-only damage
    // is correctly irrelevant to this capture contract.
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, 320, 240]);
    h.target.client.request(h.target.surface, 6, &[]);
    h.native.roundtrip();
    let moved = h.wait_pixels(&mut gl, Pixels::RootOnly, Some(first));
    h.destroy_popup();
    h.target
        .client
        .request(h.target.surface, 2, &[0, 0, 320, 240]);
    h.target.client.request(h.target.surface, 6, &[]);
    h.native.roundtrip();
    h.wait_pixels(&mut gl, Pixels::RootOnly, Some(moved));
    h.assert_fixed_canvas();
    h.finish(&mut gl, false);
}

#[test]
fn actual_wayland_readonly_popup_tcp_root_lock_unlock_does_not_revive_source() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(&mut gl, true);
    h.wait_pixels(&mut gl, Pixels::PopupInitial, None);
    h.assert_fixed_canvas();
    h.finish(&mut gl, true);
}
