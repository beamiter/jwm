//! Actual owned Wayland dispatch and GL readback. Test-controlled feedback is
//! not evidence of physical display presentation or production KMS completion.
use super::*;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::{
    wl_keyboard::{KeyState, WlKeyboard},
    wl_pointer::{Axis, AxisSource, ButtonState, WlPointer},
};
#[derive(Debug)]
enum Answer {
    Connected,
    Enabled([u8; 32]),
    Done,
    Events(Vec<ViewerEvent>, Vec<u64>),
    Failed(String),
}
enum Request {
    Enable(bool),
    Draw(u64),
    Confirm(u64),
    Ready(Option<u64>),
    Check,
    Poll,
    Stop,
}
struct ControlPeer {
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<Answer>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for ControlPeer {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Stop);
    }
}
impl ControlPeer {
    fn start(stream: UnixStream) -> Self {
        let (tx, input) = mpsc::channel();
        let (output, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> crate::remote::RemoteResult<()> {
                    let mut viewer = WaylandViewer::connect_test_stream(
                        stream,
                        800,
                        600,
                        None,
                        "owned control viewer",
                    )?;
                    let mut presented = Vec::new();
                    output.send(Answer::Connected)?;
                    loop {
                        match input.recv_timeout(Duration::from_secs(12))? {
                            Request::Enable(scroll) => {
                                output.send(Answer::Enabled(
                                    viewer.enable_wayland_control(scroll)?,
                                ))?;
                            }
                            Request::Draw(seq) => {
                                let frame = RecyclableDecodedFrame::new(
                                    seq,
                                    800,
                                    600,
                                    RgbImage::from_pixel(800, 600, Rgb([255, 0, 0])),
                                    new_decode_buffer_pool(),
                                );
                                assert!(viewer.draw_recyclable(frame)?);
                                output.send(Answer::Done)?;
                            }
                            Request::Confirm(seq) => {
                                assert!(
                                    presented.contains(&seq),
                                    "confirmation requires actual protocol presented"
                                );
                                viewer.wayland_pointer_presented(seq);
                                output.send(Answer::Done)?;
                            }
                            Request::Ready(sequence) => {
                                if let Some(seq) = sequence {
                                    assert!(presented.contains(&seq));
                                }
                                viewer.wayland_pointer_ready(sequence)?;
                                output.send(Answer::Done)?;
                            }
                            Request::Check => {
                                viewer.check_wayland_control(true)?;
                                output.send(Answer::Done)?;
                            }
                            Request::Poll => {
                                let events = viewer.poll_events()?;
                                let shown = viewer.take_presented_sequences()?;
                                presented.extend(&shown);
                                output.send(Answer::Events(events, shown))?;
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
                    let _ = output.send(Answer::Failed("control worker panicked".into()));
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
                Ok(a) => return a,
                Err(mpsc::TryRecvError::Empty) => {}
                Err(e) => panic!("control peer disconnected: {e}"),
            }
            assert!(Instant::now() < deadline, "control response deadline");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn done(&self, s: &mut Server, command: Request) {
        self.tx.send(command).unwrap();
        assert!(matches!(self.receive(s), Answer::Done));
    }
    fn poll(&self, s: &mut Server) -> (Vec<ViewerEvent>, Vec<u64>) {
        self.tx.send(Request::Poll).unwrap();
        match self.receive(s) {
            Answer::Events(e, p) => (e, p),
            a => panic!("expected events, got {a:?}"),
        }
    }
    fn failed(mut self, s: &mut Server, command: Request, reason: &str) {
        self.tx.send(command).unwrap();
        match self.receive(s) {
            Answer::Failed(error) => assert!(error.contains(reason), "wrong failure: {error}"),
            a => panic!("expected explicit failure, got {a:?}"),
        }
        self.worker.take().unwrap().join().unwrap();
    }
    fn stop(mut self, s: &mut Server) {
        self.done(s, Request::Stop);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn fixture() -> (Server, ControlPeer, WlKeyboard, WlPointer) {
    fixture_scroll(false)
}
fn fixture_scroll(scroll: bool) -> (Server, ControlPeer, WlKeyboard, WlPointer) {
    let mut s = Server::new();
    register_test_presentation(&mut s);
    s.state.image_capture_pending = Some(init_image_copy_capture(&s.display.handle()));
    let (native, stream) = UnixStream::pair().unwrap();
    s.display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let p = ControlPeer::start(stream);
    assert!(matches!(p.receive(&mut s), Answer::Connected));
    p.tx.send(Request::Enable(scroll)).unwrap();
    match p.receive(&mut s) {
        Answer::Enabled(digest) => assert_ne!(digest, [0; 32]),
        a => panic!("enable: {a:?}"),
    }
    let surface = viewer_surface(&s);
    let client = surface.client().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let (keyboard, pointer) = loop {
        let (events, shown) = p.poll(&mut s);
        assert!(events.is_empty());
        assert!(shown.is_empty());
        let keys: Vec<_> = s
            .state
            .seat
            .get_keyboard()
            .unwrap()
            .client_keyboards(&client)
            .collect();
        let pointers: Vec<_> = s
            .state
            .seat
            .get_pointer()
            .unwrap()
            .client_pointers(&client)
            .collect();
        if keys.len() == 1 && pointers.len() == 1 {
            break (keys[0].clone(), pointers[0].clone());
        }
        assert!(
            Instant::now() < deadline,
            "both devices on the owned seat must bind"
        );
    };
    keyboard.enter(10, &surface, Vec::new());
    keyboard.modifiers(11, 0, 0, 0, 0);
    pointer.enter(12, &surface, 100.0, 120.0);
    pointer.frame();
    s.roundtrip();
    assert!(p.poll(&mut s).0.is_empty());
    (s, p, keyboard, pointer)
}
fn mapped(s: &mut Server, p: &ControlPeer, gl: &mut HeadlessSmithayGles, seq: u64) {
    p.done(s, Request::Draw(seq));
    let surface = viewer_surface(s);
    let feedback = wait_feedback(s, &surface);
    let mut capture = Capture::new(s);
    verify_actual_gl_pixels(s, gl, &mut capture);
    assert!(p.poll(s).1.is_empty(), "GL alone cannot activate control");
    present(s, feedback);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, shown) = p.poll(s);
        if !shown.is_empty() {
            assert_eq!(shown, [seq]);
            break;
        }
        assert!(Instant::now() < end);
    }
    p.done(s, Request::Confirm(seq));
    p.done(s, Request::Check);
}
fn key(k: &WlKeyboard, pressed: bool) {
    k.key(
        20,
        20,
        30,
        if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        },
    );
}
fn motion(p: &WlPointer) {
    p.motion(21, 100.0, 120.0);
    p.frame();
}
#[test]
fn actual_wayland_control_requires_real_initial_presentation() {
    let (mut s, p, _, _) = fixture();
    p.failed(
        &mut s,
        Request::Check,
        "clean dual focus and actual presentation",
    );
}
#[test]
fn actual_wayland_control_preserves_keyboard_pointer_dispatch_order() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p, k, ptr) = fixture();
    mapped(&mut s, &p, &mut gl, 1);
    key(&k, true);
    key(&k, false);
    motion(&ptr);
    key(&k, true);
    key(&k, false);
    s.roundtrip();
    let (e, _) = p.poll(&mut s);
    assert!(
        matches!(
            e.as_slice(),
            [
                ViewerEvent::Key { pressed: true, .. },
                ViewerEvent::Key { pressed: false, .. },
                ViewerEvent::WaylandPointer { .. },
                ViewerEvent::Key { pressed: true, .. },
                ViewerEvent::Key { pressed: false, .. }
            ]
        ),
        "cross-device order: {e:?}"
    );
    p.stop(&mut s);
}
#[test]
fn actual_wayland_control_cross_device_held_edges_revoke_whole_batch() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for key_first in [true, false] {
        let (mut s, p, k, ptr) = fixture();
        mapped(&mut s, &p, &mut gl, 1);
        if key_first {
            key(&k, true);
            motion(&ptr);
        } else {
            ptr.button(30, 30, 0x110, ButtonState::Pressed);
            ptr.frame();
            key(&k, true);
        }
        s.roundtrip();
        p.failed(
            &mut s,
            Request::Poll,
            if key_first {
                "pointer overlaps held keys"
            } else {
                "key overlaps pointer action"
            },
        );
    }
}
#[test]
fn actual_wayland_control_focus_layout_and_seat_loss_clear_queued_actions() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for loss in 0..4 {
        let (mut s, p, k, ptr) = fixture();
        mapped(&mut s, &p, &mut gl, 1);
        key(&k, true);
        key(&k, false);
        motion(&ptr);
        let surface = viewer_surface(&s);
        let reason = match loss {
            0 => {
                k.leave(40, &surface);
                "keyboard focus left"
            }
            1 => {
                ptr.leave(40, &surface);
                "pointer left own surface"
            }
            2 => {
                k.modifiers(40, 0, 0, 0, 1);
                "modifier state"
            }
            _ => {
                s.state.seat.remove_pointer();
                "capabilities changed"
            }
        };
        s.roundtrip();
        p.failed(&mut s, Request::Poll, reason);
    }
}
#[test]
fn actual_wayland_control_new_mapping_cancels_old_pointer_not_keyboard() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p, k, ptr) = fixture();
    mapped(&mut s, &p, &mut gl, 1);
    // Draw pumps real dispatch before replacing the source mapping.
    key(&k, true);
    key(&k, false);
    motion(&ptr);
    s.roundtrip();
    committed_buffer(&viewer_surface(&s)).release();
    s.roundtrip();
    p.done(&mut s, Request::Draw(2));
    let (e, _) = p.poll(&mut s);
    assert_eq!(
        e.iter()
            .filter(|e| matches!(e, ViewerEvent::Key { .. }))
            .count(),
        2
    );
    assert!(
        !e.iter()
            .any(|e| matches!(e, ViewerEvent::WaylandPointer { .. })),
        "old pointer survived new mapping: {e:?}"
    );
    p.stop(&mut s);
}

// Exact v7 discrete wheel group; source is permitted after axis metadata too.
fn wheel(ptr: &WlPointer, step: i32, late_source: bool) {
    assert!(matches!(step, -1 | 1));
    ptr.motion(50, 100.0, 120.0);
    if !late_source {
        ptr.axis_source(AxisSource::Wheel);
    }
    ptr.axis_discrete(Axis::VerticalScroll, step);
    ptr.axis(50, Axis::VerticalScroll, f64::from(step) * 10.0);
    if late_source {
        ptr.axis_source(AxisSource::Wheel);
    }
    ptr.frame();
}
#[test]
fn actual_wayland_control_scroll_two_directions_single_action_after_fresh_frame() {
    use crate::remote::x11_viewer::WaylandPointerEventKind;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p, _, ptr) = fixture_scroll(true);
    for (seq, step) in [(1, -1), (2, 1)] {
        mapped(&mut s, &p, &mut gl, seq);
        p.done(&mut s, Request::Ready(Some(seq)));
        wheel(&ptr, step, step == 1);
        s.roundtrip();
        let (events, _) = p.poll(&mut s);
        let expected = if step == -1 {
            WaylandPointerEventKind::ScrollUp
        } else {
            WaylandPointerEventKind::ScrollDown
        };
        assert!(
            matches!(events.as_slice(), [ViewerEvent::WaylandPointer { kind, .. }] if *kind == expected),
            "one wheel action, no duplicate motion: {events:?}"
        );
        p.done(&mut s, Request::Ready(None));
        committed_buffer(&viewer_surface(&s)).release();
        s.roundtrip();
    }
    p.stop(&mut s);
}
#[test]
fn actual_wayland_control_scroll_waiting_groups_and_partial_group_never_promote() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut s, p, _, ptr) = fixture_scroll(true);
    mapped(&mut s, &p, &mut gl, 1);
    // Leave complete old groups queued, then raise Ready through the production
    // display.sync fence. They must be consumed with its admission gate closed.
    wheel(&ptr, -1, false);
    wheel(&ptr, 1, true);
    s.roundtrip();
    p.done(&mut s, Request::Ready(Some(1)));
    assert!(
        !p.poll(&mut s)
            .0
            .iter()
            .any(|e| matches!(e, ViewerEvent::WaylandPointer { .. }))
    );
    p.done(&mut s, Request::Ready(None));
    ptr.axis_source(AxisSource::Wheel);
    ptr.axis_discrete(Axis::VerticalScroll, -1);
    s.roundtrip();
    p.done(&mut s, Request::Ready(Some(1)));
    ptr.axis(60, Axis::VerticalScroll, -10.0);
    ptr.frame();
    s.roundtrip();
    assert!(
        !p.poll(&mut s)
            .0
            .iter()
            .any(|e| matches!(e, ViewerEvent::WaylandPointer { .. }))
    );
    wheel(&ptr, 1, false);
    s.roundtrip();
    let (events, _) = p.poll(&mut s);
    assert!(
        matches!(
            events.as_slice(),
            [ViewerEvent::WaylandPointer {
                kind: crate::remote::x11_viewer::WaylandPointerEventKind::ScrollDown,
                ..
            }]
        ),
        "fresh notch must still work: {events:?}"
    );
    p.stop(&mut s);
}
#[test]
fn actual_wayland_control_scroll_consent_held_inputs_and_invalid_axis_fail_closed() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    for case in 0..4 {
        let (mut s, p, k, ptr) = fixture_scroll(case != 0);
        mapped(&mut s, &p, &mut gl, 1);
        p.done(&mut s, Request::Ready(Some(1)));
        if case == 1 {
            key(&k, true);
        }
        if case == 2 {
            ptr.button(60, 60, 0x110, ButtonState::Pressed);
            ptr.frame();
        }
        if case == 3 {
            ptr.axis_source(AxisSource::Wheel);
            ptr.axis_discrete(Axis::HorizontalScroll, 1);
            ptr.axis(60, Axis::HorizontalScroll, 10.0);
            ptr.frame();
        } else {
            wheel(&ptr, 1, false);
        }
        s.roundtrip();
        p.failed(
            &mut s,
            Request::Poll,
            match case {
                0 | 2 => "lacks permission or conflicts with held button",
                1 => "overlaps held keys",
                _ => "exactly one vertical notch",
            },
        );
    }
}
