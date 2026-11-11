//! Owned native capture + owned viewer GL/readback + real wp_presentation.
//! No GTK compatibility, physical display, DRM or network-presentation claim.
use super::{
    committed_buffer, frame_callback_only, present, register_test_presentation, take_feedback,
    viewer_surface,
};
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::{App, Capture},
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
};
use crate::remote::wayland_capture::interactive_popup::PopupPresentationCandidate;
use crate::remote::wayland_viewer::popup_presentation::PopupPresentationReceipt;
use crate::remote::{wayland_capture::WaylandCapture, wayland_viewer::WaylandViewer};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
#[derive(Default)]
struct Exchange {
    candidate: Option<PopupPresentationCandidate>,
    receipt: Option<PopupPresentationReceipt>,
}
const W: u32 = 32;
const H: u32 = 24;
type PopupRect = (u32, u32, u32, u32);
type ProofSummary = (u64, u64, Option<PopupRect>);
fn fence(server: &mut Server, client: &mut Client) -> Vec<Event> {
    let done = client.new_id();
    client.request(1, 0, &[done]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    loop {
        server.roundtrip();
        let events = client.events();
        assert!(
            !events.iter().any(|e| e.sender == 1 && e.opcode == 0),
            "unexpected protocol error"
        );
        let complete = events.iter().any(|e| e.sender == done && e.opcode == 0);
        all.extend(events);
        if complete {
            return all;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}
fn buffer(server: &mut Server, client: &mut Client, w: u32, h: u32, color: [u8; 4]) -> (u32, File) {
    let shm = client.bind("wl_shm", 1);
    let fd = nix::sys::memfd::memfd_create(
        c"popup-action-pixels",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .unwrap();
    let bytes = w * h * 4;
    nix::unistd::ftruncate(&fd, i64::from(bytes)).unwrap();
    let mut file = File::from(fd);
    file.write_all(&color.repeat((w * h) as usize)).unwrap();
    let pool = client.new_id();
    client.request_with_fd_args(shm, 0, &[pool, bytes], file.as_raw_fd());
    let id = client.new_id();
    client.request(pool, 0, &[id, 0, w, h, w * 4, 0]);
    fence(server, client);
    (id, file)
}
fn viewer_pixels(server: &mut Server, gl: &mut HeadlessSmithayGles, popup: bool) {
    let mut capture = Capture::new_with_size(server, (W, H));
    let (buffer, mut file) = buffer(server, &mut capture.client, W, H, [0x5a; 4]);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    fence(server, &mut capture.client);
    let queue = server.state.image_capture_pending.as_ref().unwrap();
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, queue);
    let events = fence(server, &mut capture.client);
    assert!(
        events.iter().any(|e| e.sender == frame && e.opcode == 3),
        "actual viewer GL readback ready"
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes.len(), (W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let expected = if popup && (4..16).contains(&x) && (3..13).contains(&y) {
                [0, 255, 0, 255]
            } else {
                [0, 0, 255, 255]
            };
            assert_eq!(
                &bytes[((y * W + x) * 4) as usize..][..4],
                &expected,
                "actual displayed family pixels"
            );
        }
    }
}
fn direct_popup(server: &mut Server, app: &mut App, seat: u32, serial: u32) {
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
    let (pixels, _file) = buffer(server, c, 12, 10, [0, 255, 0, 255]);
    c.request(xdg, 3, &[0, 0, 12, 10]);
    c.request(surface, 1, &[pixels, 0, 0]);
    c.request(surface, 6, &[]);
    fence(server, c);
}
#[derive(Clone, Copy)]
enum Command {
    Draw,
    PollReceipt,
    Arm,
    Click(u32, u32),
    Discard,
    DestroySource,
    WaitViewerChange(bool),
    OrdinaryDraw,
    ArmWithoutRefresh,
    ExportCandidate,
    DrawImported,
    ExportReceipt,
    ArmImported,
    ClickImported,
}
#[derive(Debug)]
enum Reply {
    Connected,
    Drawn(u64, u64, Option<PopupRect>),
    Done,
    Receipt(bool),
    NoReceipt,
    Failed(String),
}
struct Harness {
    native: Server,
    viewer: Server,
    app: App,
    other: Option<App>,
    gl: HeadlessSmithayGles,
    seat: u32,
    pointer: u32,
    other_pointer: u32,
    released: bool,
    proof: Option<ProofSummary>,
    exchange: Arc<Mutex<Exchange>>,
    tx: Option<mpsc::Sender<Command>>,
    rx: mpsc::Receiver<Reply>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Harness {
    fn new(gl: HeadlessSmithayGles) -> Self {
        let mut native = Server::new();
        native.state.image_capture_pending =
            Some(init_image_copy_capture(&native.display.handle()));
        let mut app = App::with_size_color(&mut native, [0, 0, 255, 255], W, H);
        fence(&mut native, &mut app.client);
        let mut viewer = Server::new();
        register_test_presentation(&mut viewer);
        viewer.state.image_capture_pending =
            Some(init_image_copy_capture(&viewer.display.handle()));
        let (n, np) = UnixStream::pair().unwrap();
        native
            .display
            .handle()
            .insert_client(n, Arc::new(JwmClientState::default()))
            .unwrap();
        let (v, vp) = UnixStream::pair().unwrap();
        viewer
            .display
            .handle()
            .insert_client(v, Arc::new(JwmClientState::default()))
            .unwrap();
        let (tx, requests) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let exchange = Arc::new(Mutex::new(Exchange::default()));
        let worker_exchange = exchange.clone();
        let worker = thread::spawn(move || {
            let run = || -> crate::remote::RemoteResult<()> {
                let mut capture = WaylandCapture::connect_test_stream_interactive_popup(np, 1)?;
                let mut display = WaylandViewer::connect_test_stream(
                    vp,
                    W as u16,
                    H as u16,
                    None,
                    "owned popup action",
                )?;
                let mut receipt = None;
                results.send(Reply::Connected)?;
                while let Ok(command) = requests.recv() {
                    let result = (|| -> crate::remote::RemoteResult<Reply> {
                        Ok(match command {
                            Command::Draw => {
                                assert!(receipt.is_none());
                                let candidate = capture.capture_popup_presentation_candidate()?;
                                let proof = candidate.proof().clone();
                                assert!(display.draw_popup_capture(candidate)?);
                                Reply::Drawn(
                                    proof.native_ready_serial,
                                    proof.generation,
                                    proof.popup_rect,
                                )
                            }
                            Command::ExportCandidate => {
                                let candidate = capture.capture_popup_presentation_candidate()?;
                                let proof = candidate.proof().clone();
                                let mut slot = worker_exchange.lock().unwrap();
                                assert!(slot.candidate.is_none());
                                slot.candidate = Some(candidate);
                                Reply::Drawn(
                                    proof.native_ready_serial,
                                    proof.generation,
                                    proof.popup_rect,
                                )
                            }
                            Command::DrawImported => {
                                let candidate = worker_exchange
                                    .lock()
                                    .unwrap()
                                    .candidate
                                    .take()
                                    .expect("real transferred candidate");
                                let proof = candidate.proof().clone();
                                assert!(display.draw_popup_capture(candidate)?);
                                Reply::Drawn(
                                    proof.native_ready_serial,
                                    proof.generation,
                                    proof.popup_rect,
                                )
                            }
                            Command::ExportReceipt => {
                                let mut slot = worker_exchange.lock().unwrap();
                                assert!(slot.receipt.is_none());
                                slot.receipt =
                                    Some(receipt.take().expect("actual presented receipt"));
                                Reply::Done
                            }
                            Command::ArmImported | Command::ClickImported => {
                                let r = worker_exchange
                                    .lock()
                                    .unwrap()
                                    .receipt
                                    .take()
                                    .expect("real transferred receipt");
                                // Deliberately exercise the adapter's identity defense directly.
                                if matches!(command, Command::ArmImported) {
                                    capture.arm_popup_presented(r)?;
                                } else {
                                    capture.click_popup_presented(r, 8, 8)?;
                                }
                                Reply::Done
                            }
                            Command::ArmWithoutRefresh => match receipt.take() {
                                Some(r) => {
                                    capture.arm_popup_presented(r)?;
                                    Reply::Done
                                }
                                None => Reply::NoReceipt,
                            },
                            Command::PollReceipt => {
                                let received = display.take_presented_popup_frame()?;
                                let any = received.is_some();
                                if any {
                                    assert!(receipt.is_none());
                                    receipt = received;
                                }
                                Reply::Receipt(any)
                            }
                            Command::Arm => match receipt.take() {
                                Some(r) => {
                                    let r = display.refresh_popup_presentation(r)?;
                                    capture.arm_popup_presented(r)?;
                                    Reply::Done
                                }
                                None => Reply::NoReceipt,
                            },
                            Command::Click(x, y) => match receipt.take() {
                                Some(r) => {
                                    let r = display.refresh_popup_presentation(r)?;
                                    capture.click_popup_presented(r, x, y)?;
                                    Reply::Done
                                }
                                None => Reply::NoReceipt,
                            },
                            Command::Discard => {
                                capture.discard_popup_frame();
                                Reply::Done
                            }
                            Command::OrdinaryDraw => {
                                let frame = crate::remote::frame::RecyclableDecodedFrame::new(
                                    999,
                                    W as u16,
                                    H as u16,
                                    image::RgbImage::from_pixel(W, H, image::Rgb([255, 0, 0])),
                                    crate::remote::frame::new_decode_buffer_pool(),
                                );
                                assert!(display.draw_recyclable(frame)?);
                                Reply::Done
                            }
                            Command::WaitViewerChange(close) => {
                                let deadline = Instant::now() + Duration::from_secs(5);
                                loop {
                                    display.poll_events()?;
                                    if (close && display.user_close_requested())
                                        || (!close
                                            && display.viewport() == ((W + 1) as u16, H as u16))
                                    {
                                        break;
                                    }
                                    assert!(
                                        Instant::now() < deadline,
                                        "actual viewer lifecycle event not dispatched"
                                    );
                                    thread::yield_now();
                                }
                                Reply::Done
                            }
                            Command::DestroySource => {
                                capture.destroy_original_popup_source_for_test();
                                Reply::Done
                            }
                        })
                    })();
                    results.send(match result {
                        Ok(r) => r,
                        Err(e) => Reply::Failed(e.to_string()),
                    })?;
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = results.send(Reply::Failed(e.to_string()));
            }
        });
        // The second app is created only after the adapter has selected its sole
        // original root. No numerical toplevel guess can select the bystander.
        let mut pending = Self {
            native,
            viewer,
            app,
            other: None,
            gl,
            seat: 0,
            pointer: 0,
            other_pointer: 0,
            released: false,
            proof: None,
            exchange,
            tx: Some(tx),
            rx,
            worker: Some(worker),
        };
        assert!(matches!(pending.receive(), Reply::Connected));
        let events = fence(&mut pending.native, &mut pending.app.client);
        let globals: Vec<_> = events
            .iter()
            .filter(|e| e.sender == 2 && e.opcode == 0)
            .filter_map(|e| {
                let len = e.args[1] as usize;
                let bytes: Vec<_> = e.args[2..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                (&bytes[..len - 1] == b"wl_seat").then_some(e.args[0])
            })
            .collect();
        assert_eq!(globals.len(), 1);
        pending.seat = pending.app.client.new_id();
        pending.app.client.request(
            2,
            0,
            &[
                globals[0],
                8,
                u32::from_ne_bytes(*b"wl_s"),
                u32::from_ne_bytes(*b"eat\0"),
                7,
                pending.seat,
            ],
        );
        pending.pointer = pending.app.client.new_id();
        pending
            .app
            .client
            .request(pending.seat, 0, &[pending.pointer]);
        fence(&mut pending.native, &mut pending.app.client);
        let mut other = App::with_size_color(&mut pending.native, [255, 0, 0, 255], W, H);
        assert_eq!(other.client.global_count("wl_seat"), 1);
        let seat = other.client.bind("wl_seat", 7);
        pending.other_pointer = other.client.new_id();
        other.client.request(seat, 0, &[pending.other_pointer]);
        fence(&mut pending.native, &mut other.client);
        pending.other = Some(other);
        pending
    }
    fn receive(&mut self) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            self.native.dispatch_timers(Duration::from_millis(1));
            self.viewer.roundtrip();
            for (id, top) in &self.viewer.state.toplevels {
                if !top.is_initial_configure_sent() {
                    let g = self.viewer.state.window_geometry.get_mut(id).unwrap();
                    g.w = W;
                    g.h = H;
                    top.with_pending_state(|s| s.size = Some((W as i32, H as i32).into()));
                    top.send_configure();
                }
            }
            self.viewer.roundtrip();
            let queue = self.native.state.image_capture_pending.as_ref().unwrap();
            fulfill_test_toplevel_capture(
                &mut self.gl.renderer,
                &mut None,
                &self.native.state,
                queue,
            );
            self.native.roundtrip();
            match self.rx.try_recv() {
                Ok(r) => return r,
                Err(mpsc::TryRecvError::Disconnected) => panic!("action worker disconnected"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "owned action operation deadline");
        }
    }
    fn command(&mut self, c: Command) -> Reply {
        self.tx.as_ref().unwrap().send(c).unwrap();
        self.receive()
    }
    fn done(&mut self, c: Command) {
        let r = self.command(c);
        if matches!(
            c,
            Command::Draw | Command::DrawImported | Command::ExportCandidate
        ) {
            let Reply::Drawn(serial, generation, rect) = r else {
                panic!("actual draw failed: {r:?}");
            };
            self.proof = Some((serial, generation, rect));
            self.released = false;
        } else {
            assert!(
                matches!(r, Reply::Done),
                "expected operation success: {r:?}"
            );
        }
    }
    fn events(&mut self) -> Vec<Event> {
        let other = self.other.as_mut().unwrap();
        assert!(
            !fence(&mut self.native, &mut other.client)
                .iter()
                .any(|e| e.sender == self.other_pointer),
            "bystander received input"
        );
        fence(&mut self.native, &mut self.app.client)
            .into_iter()
            .filter(|e| e.sender == self.pointer)
            .collect()
    }
    fn presented(&mut self, popup: bool) {
        let surface = viewer_surface(&self.viewer);
        let feedback = take_feedback(&surface);
        assert_eq!(feedback.len(), 1);
        viewer_pixels(&mut self.viewer, &mut self.gl, popup);
        present(&mut self.viewer, feedback);
        if !self.released {
            committed_buffer(&surface).release();
            self.released = true;
        }
        frame_callback_only(&surface);
        self.viewer.roundtrip();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.command(Command::PollReceipt) {
                Reply::Receipt(true) => break,
                Reply::Receipt(false) => {}
                r => panic!("presentation receipt failed: {r:?}"),
            };
            assert!(Instant::now() < deadline);
        }
        assert!(
            matches!(self.command(Command::PollReceipt), Reply::Receipt(false)),
            "receipt must be delivered once"
        );
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(w) = self.worker.take() {
            w.join().expect("action worker panic");
        }
    }
}
#[test]
fn actual_popup_action_requires_new_exact_viewer_presentation_after_arm() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    h.done(Command::Draw);
    assert!(matches!(
        h.command(Command::PollReceipt),
        Reply::Receipt(false)
    ));
    assert!(matches!(h.command(Command::Arm), Reply::NoReceipt));
    assert!(h.events().is_empty());
    let surface = viewer_surface(&h.viewer);
    committed_buffer(&surface).release();
    h.released = true;
    frame_callback_only(&surface);
    h.viewer.roundtrip();
    assert!(
        matches!(h.command(Command::PollReceipt), Reply::Receipt(false)),
        "release/frame callback is not presentation"
    );
    let arm_proof = h.proof.unwrap();
    assert_eq!(arm_proof.2, None);
    h.presented(false);
    h.done(Command::Arm);
    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| e.opcode == 0 && e.args[1] == h.app.surface)
    );
    assert!(!events.iter().any(|e| e.opcode == 3));
    assert!(matches!(h.command(Command::Click(8, 8)), Reply::NoReceipt));
    assert!(h.events().is_empty());
    h.done(Command::Draw);
    let click_proof = h.proof.unwrap();
    assert!(click_proof.0 > arm_proof.0);
    h.presented(false);
    h.done(Command::Click(8, 8));
    let events = h.events();
    let buttons: Vec<_> = events.iter().filter(|e| e.opcode == 3).collect();
    assert_eq!(buttons.len(), 2);
    assert_eq!(&buttons[0].args[2..], &[0x110, 1]);
    assert_eq!(&buttons[1].args[2..], &[0x110, 0]);
    let serial = buttons[0].args[0];
    direct_popup(&mut h.native, &mut h.app, h.seat, serial);
    h.done(Command::Draw);
    let popup_proof = h.proof.unwrap();
    assert!(popup_proof.0 > click_proof.0 && popup_proof.1 > click_proof.1);
    assert_eq!(popup_proof.2, Some((4, 3, 12, 10)));
    h.presented(true);
    h.done(Command::Click(7, 8));
    let events = h.events();
    assert!(
        events
            .iter()
            .any(|e| e.opcode == 2 && e.args[1..] == [3 * 256, 5 * 256])
    );
    assert_eq!(events.iter().filter(|e| e.opcode == 3).count(), 2);
}
#[test]
fn actual_popup_presented_receipt_is_rejected_after_capture_discard() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    h.done(Command::Draw);
    h.presented(false);
    h.done(Command::Discard);
    match h.command(Command::Arm) {
        Reply::Failed(e) => assert_eq!(e, "invalid popup presentation binding"),
        r => panic!("discarded proof accepted: {r:?}"),
    };
    assert!(h.events().is_empty());
}
#[test]
fn actual_popup_presented_receipt_cannot_survive_lock_source_loss_or_freshness_expiry() {
    for cause in [0, 1, 2] {
        let Some(gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let mut h = Harness::new(gl);
        h.done(Command::Draw);
        h.presented(false);
        let mut locker = None;
        if cause == 0 {
            h.native.state.session_lock_supported = true;
            let mut c = h.native.connect();
            let manager = c.bind("ext_session_lock_manager_v1", 1);
            let lock = c.new_id();
            c.request(manager, 1, &[lock]);
            let events = fence(&mut h.native, &mut c);
            assert!(events.iter().any(|e| e.sender == lock && e.opcode == 0));
            locker = Some(c);
        } else if cause == 1 {
            h.done(Command::DestroySource);
        } else {
            let end = Instant::now() + Duration::from_millis(1001);
            while Instant::now() < end {
                h.native.dispatch_timers(Duration::from_millis(1));
            }
        }
        match h.command(Command::Arm) {
            Reply::Failed(e) => assert!(
                if cause == 2 {
                    e == "interactive popup protocol deadline expired"
                } else {
                    matches!(
                        e.as_str(),
                        "interactive popup lease revoked" | "Wayland image capture session stopped"
                    )
                },
                "unexpected lifecycle failure: {e}"
            ),
            r => panic!("stale presentation accepted: {r:?}"),
        };
        assert!(h.events().is_empty());
        drop(locker);
    }
}

#[test]
fn actual_popup_claimed_receipt_cannot_survive_viewer_resize_or_close() {
    for (change, direct) in [
        (0, false),
        (1, false),
        (2, false),
        (0, true),
        (1, true),
        (2, true),
    ] {
        let close = change == 1;
        let Some(gl) = HeadlessSmithayGles::new() else {
            return;
        };
        let mut h = Harness::new(gl);
        h.done(Command::Draw);
        h.presented(false);
        if change == 2 {
            h.done(Command::OrdinaryDraw);
        } else {
            let top = h.viewer.state.toplevels.values().next().unwrap();
            if close {
                top.send_close();
            } else {
                top.with_pending_state(|s| s.size = Some(((W + 1) as i32, H as i32).into()));
                top.send_configure();
            }
            h.viewer.roundtrip();
            h.done(Command::WaitViewerChange(close));
        }
        match h.command(if direct {
            Command::ArmWithoutRefresh
        } else {
            Command::Arm
        }) {
            Reply::Failed(e) => assert_eq!(
                e,
                if close && !direct {
                    "popup receipt belongs to an unavailable viewer"
                } else {
                    "popup presentation invalidated after receipt"
                }
            ),
            r => panic!("viewer-invalidated receipt authorized input: {r:?}"),
        }
        assert!(matches!(h.command(Command::Arm), Reply::NoReceipt));
        assert!(h.events().is_empty());
    }
}

fn transfer_candidate(from: &Harness, to: &Harness) {
    let value = from
        .exchange
        .lock()
        .unwrap()
        .candidate
        .take()
        .expect("real candidate");
    let mut dest = to.exchange.lock().unwrap();
    assert!(dest.candidate.is_none());
    dest.candidate = Some(value);
}
fn transfer_receipt(from: &Harness, to: &Harness) {
    let value = from
        .exchange
        .lock()
        .unwrap()
        .receipt
        .take()
        .expect("real receipt");
    let mut dest = to.exchange.lock().unwrap();
    assert!(dest.receipt.is_none());
    dest.receipt = Some(value);
}
#[test]
fn actual_popup_receipt_cannot_cross_capture_or_bound_viewer_connections() {
    for cross_viewer in [false, true] {
        let Some(gl_a) = HeadlessSmithayGles::new() else {
            return;
        };
        let Some(gl_b) = HeadlessSmithayGles::new() else {
            return;
        };
        let mut a = Harness::new(gl_a);
        let mut b = Harness::new(gl_b);
        a.done(Command::Draw);
        a.presented(false);
        if cross_viewer {
            a.done(Command::Arm);
            a.events();
            a.done(Command::ExportCandidate);
            transfer_candidate(&a, &b);
            b.done(Command::DrawImported);
            b.presented(false);
            b.done(Command::ExportReceipt);
            transfer_receipt(&b, &a);
            match a.command(Command::ClickImported) {
                Reply::Failed(e) => assert_eq!(e, "invalid popup presentation binding"),
                r => panic!("foreign viewer authorized click: {r:?}"),
            };
        } else {
            a.done(Command::ExportReceipt);
            b.done(Command::ExportCandidate);
            assert_eq!(
                a.proof, b.proof,
                "two genuine initial proofs have colliding serial/generation/layout values"
            );
            transfer_receipt(&a, &b);
            match b.command(Command::ArmImported) {
                Reply::Failed(e) => assert_eq!(e, "invalid popup presentation binding"),
                r => panic!("foreign capture authorized arm: {r:?}"),
            };
        }
        let events = a.events();
        if cross_viewer {
            // Failed input closes the already-armed private focus. Exactly one
            // leave/frame cleanup is permitted, never new enter/motion/button.
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].opcode, 1);
            assert_eq!(events[0].args.get(1), Some(&a.app.surface));
            assert_eq!(events[1].opcode, 5);
        } else {
            assert!(events.is_empty());
        }
        assert!(b.events().is_empty());
    }
}
