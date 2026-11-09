//! Owned protocol input, real SHM rendering and explicit presentation feedback.
//! Remote action/recipient assertions live in the separate authenticated TCP test.
use super::*;
use crate::remote::x11_viewer::WaylandPointerEventKind as Kind;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_pointer::{ButtonState, WlPointer};

#[derive(Debug)]
enum Answer {
    Connected,
    Done,
    Poll(Vec<ViewerEvent>, Vec<u64>),
    Failed(String),
}
enum Request {
    Enable,
    Draw(u64, u16, u16),
    Confirm(u64),
    Poll,
    FencePoll(mpsc::Sender<()>, mpsc::Receiver<()>),
    Stop,
}
struct PointerPeer {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Answer>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for PointerPeer {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
    }
}
impl PointerPeer {
    fn start(stream: UnixStream) -> Self {
        let (tx, input) = mpsc::channel();
        let (output, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> crate::remote::RemoteResult<()> {
                    let mut v = WaylandViewer::connect_test_stream(
                        stream,
                        800,
                        600,
                        None,
                        "owned pointer viewer",
                    )?;
                    output.send(Answer::Connected)?;
                    loop {
                        match input.recv_timeout(Duration::from_secs(12))? {
                            Request::Enable => {
                                v.enable_wayland_pointer()?;
                                output.send(Answer::Done)?;
                            }
                            Request::Draw(seq, w, h) => {
                                let f = RecyclableDecodedFrame::new(
                                    seq,
                                    w,
                                    h,
                                    RgbImage::from_pixel(w.into(), h.into(), Rgb([255, 0, 0])),
                                    new_decode_buffer_pool(),
                                );
                                assert!(v.draw_recyclable(f)?, "owned draw must submit");
                                output.send(Answer::Done)?;
                            }
                            Request::Confirm(seq) => {
                                v.wayland_pointer_presented(seq);
                                output.send(Answer::Done)?;
                            }
                            Request::Poll => {
                                let e = v.poll_events()?;
                                let p = v.take_presented_sequences()?;
                                output.send(Answer::Poll(e, p))?;
                            }
                            Request::FencePoll(reached, release) => {
                                let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let hook_observed = observed.clone();
                                let events =
                                    crate::remote::wayland_viewer::with_pointer_fence_hook(
                                        move || {
                                            hook_observed
                                                .store(true, std::sync::atomic::Ordering::Release);
                                            reached.send(()).unwrap();
                                            release.recv_timeout(Duration::from_secs(5)).expect(
                                                "test must release real completed-frame pause",
                                            );
                                        },
                                        || -> crate::remote::RemoteResult<Vec<ViewerEvent>> {
                                            let end = Instant::now() + Duration::from_secs(5);
                                            loop {
                                                let events = v.poll_events()?;
                                                if observed
                                                    .load(std::sync::atomic::Ordering::Acquire)
                                                {
                                                    return Ok(events);
                                                }
                                                assert!(
                                                    events.is_empty(),
                                                    "input escaped before real frame fence"
                                                );
                                                assert!(
                                                    Instant::now() < end,
                                                    "real completed pointer frame never reached fence"
                                                );
                                                thread::yield_now();
                                            }
                                        },
                                    )?;
                                output.send(Answer::Poll(events, Vec::new()))?;
                            }
                            Request::Stop => {
                                output.send(Answer::Done)?;
                                return Ok(());
                            }
                        }
                    }
                },
            ));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = output.send(Answer::Failed(e.to_string()));
                }
                Err(_) => {
                    let _ = output.send(Answer::Failed("pointer worker panicked".into()));
                }
            }
        });
        Self {
            tx,
            rx,
            worker: Some(worker),
        }
    }
    fn receive(&self, s: &mut Server) -> Answer {
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            s.roundtrip();
            for top in s.state.toplevels.values() {
                if !top.is_initial_configure_sent() {
                    top.with_pending_state(|v| {
                        v.size = Some((800, 600).into());
                        v.states.set(xdg_toplevel::State::Activated);
                    });
                    top.send_configure();
                }
            }
            s.roundtrip();
            match self.rx.try_recv() {
                Ok(Answer::Failed(error)) => panic!("pointer worker failed: {error}"),
                Ok(a) => return a,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(e) => panic!("pointer worker disconnected: {e}"),
            }
            assert!(Instant::now() < end, "owned pointer response deadline");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn command(&self, s: &mut Server, r: Request) {
        s.roundtrip();
        self.tx.send(r).unwrap();
        assert!(matches!(self.receive(s), Answer::Done));
    }
    fn poll(&self, s: &mut Server) -> (Vec<ViewerEvent>, Vec<u64>) {
        s.roundtrip();
        self.tx.send(Request::Poll).unwrap();
        match self.receive(s) {
            Answer::Poll(e, p) => (e, p),
            a => panic!("expected pointer poll: {a:?}"),
        }
    }
    fn wait_events(&self, s: &mut Server) -> Vec<ViewerEvent> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let (e, _) = self.poll(s);
            if !e.is_empty() {
                return e;
            }
            assert!(Instant::now() < end, "explicit pointer event deadline");
        }
    }
    fn revoked(&self, s: &mut Server) {
        let e = self.wait_events(s);
        assert!(
            e.contains(&ViewerEvent::WaylandPointerRevoke),
            "explicit revoke required: {e:?}"
        );
        assert!(
            !e.iter()
                .any(|e| matches!(e, ViewerEvent::WaylandPointer { .. })),
            "revoked batch leaked input: {e:?}"
        );
    }
    fn stop(mut self, s: &mut Server) {
        self.command(s, Request::Stop);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn fixture() -> (Server, PointerPeer) {
    let mut s = Server::new();
    register_test_presentation(&mut s);
    s.state.image_capture_pending = Some(init_image_copy_capture(&s.display.handle()));
    let (native, stream) = UnixStream::pair().unwrap();
    s.display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let p = PointerPeer::start(stream);
    assert!(matches!(p.receive(&mut s), Answer::Connected));
    (s, p)
}
fn enable(s: &mut Server, p: &PointerPeer) -> WlPointer {
    p.command(s, Request::Enable);
    let surface = viewer_surface(s);
    let client = surface.client().unwrap();
    assert_eq!(
        s.state
            .seat
            .get_keyboard()
            .unwrap()
            .client_keyboards(&client)
            .count(),
        0,
        "pointer optin must not bind keyboard"
    );
    let pointers: Vec<_> = s
        .state
        .seat
        .get_pointer()
        .unwrap()
        .client_pointers(&client)
        .collect();
    assert_eq!(pointers.len(), 1);
    pointers[0].clone()
}
fn mapped(s: &mut Server, p: &PointerPeer, gl: &mut HeadlessSmithayGles, seq: u64, w: u16, h: u16) {
    p.command(s, Request::Draw(seq, w, h));
    let surface = viewer_surface(s);
    let feedback = wait_feedback(s, &surface);
    let mut capture = Capture::new(s);
    // Production capture of the actual viewer surface must succeed. For the
    // letterbox case check its center and the black final column explicitly.
    if (w, h) == (800, 600) {
        verify_actual_gl_pixels(s, gl, &mut capture);
    } else {
        let (buffer, mut file) = shm_buffer_file(s, &mut capture.client, [0x5a; 4]);
        let f = capture.client.new_id();
        capture.client.request(capture.session, 0, &[f]);
        capture.client.request(f, 1, &[buffer]);
        capture.client.request(f, 3, &[]);
        s.roundtrip();
        let q = s.state.image_capture_pending.as_ref().unwrap().clone();
        fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &s.state, &q);
        s.roundtrip();
        assert!(
            capture
                .client
                .events()
                .iter()
                .any(|e| e.sender == f && e.opcode == 3)
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut pixels = Vec::new();
        file.read_to_end(&mut pixels).unwrap();
        assert_eq!(pixels.len(), 800 * 600 * 4);
        assert_eq!(
            &pixels[(300 * 800 + 400) * 4..(300 * 800 + 400) * 4 + 3],
            &[0, 0, 255]
        );
        assert_eq!(
            &pixels[(300 * 800 + 799) * 4..(300 * 800 + 799) * 4 + 3],
            &[0, 0, 0]
        );
        capture.client.request(f, 0, &[]);
        s.roundtrip();
    }
    assert!(p.poll(s).1.is_empty(), "GL copy alone is not presented");
    present(s, feedback);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, shown) = p.poll(s);
        if !shown.is_empty() {
            assert_eq!(shown, [seq]);
            break;
        }
        assert!(Instant::now() < end, "real presentation deadline");
    }
    p.command(s, Request::Confirm(seq));
}
fn enter(s: &mut Server, p: &PointerPeer, ptr: &WlPointer) {
    ptr.enter(1, &viewer_surface(s), 400.0, 300.0);
    ptr.frame();
    let _ = p.poll(s);
}
fn motion(ptr: &WlPointer, x: f64, y: f64) {
    ptr.motion(2, x, y);
    ptr.frame();
}

#[test]
fn actual_wayland_pointer_viewer_optin_presented_mapping_and_unknown_release() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p) = fixture();
    let surface = viewer_surface(&s);
    assert!(
        s.state
            .seat
            .client_seats(&surface.client().unwrap())
            .is_empty()
    );
    let ptr = enable(&mut s, &p);
    mapped(&mut s, &p, &mut gl, 1, 800, 600);
    enter(&mut s, &p, &ptr);
    motion(&ptr, 400.0, 300.0);
    let e = p.wait_events(&mut s);
    assert!(
        e.iter().any(|e| matches!(
            e,
            ViewerEvent::WaylandPointer {
                frame_sequence: 1,
                x: 400,
                y: 300,
                kind: Kind::Motion,
                ..
            }
        )),
        "{e:?}"
    );
    ptr.button(3, 3, 0x110, ButtonState::Released);
    ptr.frame();
    let e = p.wait_events(&mut s);
    assert!(
        e.contains(&ViewerEvent::WaylandPointerButtonCancel { pressed: false }),
        "{e:?}"
    );
    ptr.button(4, 4, 0x110, ButtonState::Pressed);
    ptr.button(5, 5, 0x110, ButtonState::Released);
    ptr.frame();
    let e = p.wait_events(&mut s);
    let kinds: Vec<_> = e
        .iter()
        .filter_map(|e| match e {
            ViewerEvent::WaylandPointer { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, [Kind::LeftDown, Kind::LeftUp]);
    p.stop(&mut s);
}
#[test]
fn actual_wayland_pointer_viewer_letterbox_and_fractional_edges_cancel() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p) = fixture();
    let ptr = enable(&mut s, &p);
    mapped(&mut s, &p, &mut gl, 1, 401, 301);
    enter(&mut s, &p, &ptr);
    for (x, y) in [
        (-0.25, 300.0),
        (799.0, 300.0),
        (400.0, -0.25),
        (400.0, 600.0),
        (32768.0, 300.0),
    ] {
        motion(&ptr, x, y);
        let e = p.wait_events(&mut s);
        assert!(
            e.contains(&ViewerEvent::WaylandPointerCancel),
            "({x},{y}): {e:?}"
        );
        assert!(
            !e.iter()
                .any(|e| matches!(e, ViewerEvent::WaylandPointer { .. }))
        );
    }
    motion(&ptr, 400.0, 300.0);
    let e = p.wait_events(&mut s);
    assert!(e.iter().any(
        |e| matches!(e,ViewerEvent::WaylandPointer{x,y,kind:Kind::Motion,..} if *x<401 && *y<301)
    ));
    p.stop(&mut s);
}
#[test]
fn actual_wayland_pointer_viewer_nonleft_and_axis_are_terminal() {
    use smithay::reexports::wayland_server::protocol::wl_pointer::Axis;
    for axis in [false, true] {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let (mut s, p) = fixture();
        let ptr = enable(&mut s, &p);
        mapped(&mut s, &p, &mut gl, 1, 800, 600);
        enter(&mut s, &p, &ptr);
        if axis {
            ptr.axis(3, Axis::VerticalScroll, 10.0);
        } else {
            ptr.button(3, 3, 0x111, ButtonState::Pressed);
        }
        ptr.motion(4, 401.0, 301.0);
        ptr.frame();
        p.revoked(&mut s);
        p.stop(&mut s);
    }
}
#[test]
fn actual_wayland_pointer_viewer_lifecycle_aba_clears_whole_batch() {
    use crate::backend::wayland::state::JwmWaylandState;
    use smithay::reexports::wayland_server::protocol::wl_seat::Capability;
    for case in 0..5 {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let (mut s, p) = fixture();
        let ptr = enable(&mut s, &p);
        mapped(&mut s, &p, &mut gl, 1, 800, 600);
        enter(&mut s, &p, &ptr);
        let surface = viewer_surface(&s);
        ptr.motion(3, 401.0, 301.0);
        match case {
            0 => {
                ptr.leave(4, &surface);
                ptr.enter(5, &surface, 400.0, 300.0);
            }
            1 => {
                let top = s.state.toplevels.values().next().unwrap();
                top.with_pending_state(|v| v.states.unset(xdg_toplevel::State::Activated));
                top.send_configure();
                top.with_pending_state(|v| v.states.set(xdg_toplevel::State::Activated));
                top.send_configure();
            }
            2 => {
                let top = s.state.toplevels.values().next().unwrap();
                top.with_pending_state(|v| v.size = Some((801, 600).into()));
                top.send_configure();
                top.with_pending_state(|v| v.size = Some((800, 600).into()));
                top.send_configure();
            }
            3 => {
                let seat = s
                    .state
                    .seat
                    .client_seats(&surface.client().unwrap())
                    .remove(0);
                seat.capabilities(Capability::empty());
                seat.capabilities(Capability::Pointer);
            }
            _ => {
                let global = s.state.seat.global().unwrap();
                s.display.handle().remove_global::<JwmWaylandState>(global);
            }
        }
        ptr.frame();
        p.revoked(&mut s);
        p.stop(&mut s);
    }
}

#[test]
fn actual_wayland_pointer_viewer_new_presented_frame_invalidates_observed_down() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p) = fixture();
    let ptr = enable(&mut s, &p);
    mapped(&mut s, &p, &mut gl, 1, 800, 600);
    enter(&mut s, &p, &ptr);
    ptr.button(3, 3, 0x110, ButtonState::Pressed);
    ptr.frame();
    let e = p.wait_events(&mut s);
    assert!(
        e.iter().any(|e| matches!(
            e,
            ViewerEvent::WaylandPointer {
                frame_sequence: 1,
                kind: Kind::LeftDown,
                ..
            }
        )),
        "{e:?}"
    );
    mapped(&mut s, &p, &mut gl, 2, 800, 600);
    ptr.button(4, 4, 0x110, ButtonState::Released);
    ptr.frame();
    let e = p.wait_events(&mut s);
    assert!(
        e.contains(&ViewerEvent::WaylandPointerButtonCancel { pressed: false }),
        "new frame must cancel old down: {e:?}"
    );
    assert!(
        !e.iter().any(|e| matches!(
            e,
            ViewerEvent::WaylandPointer {
                kind: Kind::LeftUp,
                ..
            }
        )),
        "must not upgrade old click to new presentation"
    );
    ptr.button(5, 5, 0x110, ButtonState::Pressed);
    ptr.button(6, 6, 0x110, ButtonState::Released);
    ptr.frame();
    let e = p.wait_events(&mut s);
    let edges: Vec<_> = e
        .iter()
        .filter_map(|e| match e {
            ViewerEvent::WaylandPointer {
                frame_sequence,
                kind,
                ..
            } => Some((*frame_sequence, *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(edges, [(2, Kind::LeftDown), (2, Kind::LeftUp)]);
    p.stop(&mut s);
}

#[test]
fn actual_wayland_pointer_viewer_completed_frame_fences_later_lifecycle_batch() {
    use crate::backend::wayland::state::JwmWaylandState;
    for case in 0..3 {
        let Some(mut gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let (mut s, p) = fixture();
        let ptr = enable(&mut s, &p);
        mapped(&mut s, &p, &mut gl, 1, 800, 600);
        enter(&mut s, &p, &ptr);
        // One complete real input frame, delivered independently of the later
        // lifecycle message. No callback, event or internal state is fabricated.
        ptr.motion(3, 401.0, 301.0);
        ptr.button(4, 4, 0x110, ButtonState::Pressed);
        ptr.button(5, 5, 0x110, ButtonState::Released);
        ptr.frame();
        s.roundtrip();
        let (reached, at_fence) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        p.tx.send(Request::FencePoll(reached, resume)).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            s.roundtrip();
            match at_fence.try_recv() {
                Ok(()) => break,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(e) => panic!("fence observer disconnected: {e}"),
            }
            if let Ok(reply) = p.rx.try_recv() {
                panic!("pointer returned before fence observation: {reply:?}");
            }
            assert!(
                Instant::now() < end,
                "real completed frame fence was never observed"
            );
            thread::yield_now();
        }
        // The observer can only run after production needs_fence sees completed
        // events. Queue a separate protocol batch before letting real sync run.
        match case {
            0 => ptr.leave(6, &viewer_surface(&s)),
            1 => {
                let top = s.state.toplevels.values().next().unwrap();
                top.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Activated));
                top.send_configure();
            }
            _ => {
                let global = s.state.seat.global().unwrap();
                s.display.handle().remove_global::<JwmWaylandState>(global);
            }
        }
        s.roundtrip();
        release.send(()).unwrap();
        let events = match p.receive(&mut s) {
            Answer::Poll(events, _) => events,
            other => panic!("expected fenced events: {other:?}"),
        };
        assert!(
            events.contains(&ViewerEvent::WaylandPointerRevoke),
            "case {case}: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ViewerEvent::WaylandPointer { .. })),
            "old completed input escaped cross-protocol fence: {events:?}"
        );
        p.stop(&mut s);
    }
}
