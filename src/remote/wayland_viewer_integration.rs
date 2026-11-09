//! Real viewer -> owned Wayland protocol server, and authenticated TCP tests.
//! Feedback timing here is controlled by the test compositor, only after an
//! actual GL copy of committed SHM pixels. This verifies the viewer contract,
//! NOT the production KMS pageflip batching or physical display presentation.
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::backend::fulfill_test_toplevel_capture;
use crate::backend::wayland_udev::compositor::headless_render::HeadlessSmithayGles;
use crate::backend::wayland_udev::image_copy_capture::{
    authority_tests::{App, Capture, shm_buffer_file}, init_image_copy_capture,
    wire_test_client::Server,
};
use crate::remote::frame::{RecyclableDecodedFrame,new_decode_buffer_pool};
use crate::remote::wayland_viewer::WaylandViewer;
use crate::remote::x11_viewer::ViewerEvent;
use image::{Rgb,RgbImage};
use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
use smithay::reexports::wayland_server::{Resource,protocol::{wl_surface::WlSurface,wl_buffer::WlBuffer}};
use smithay::wayland::{compositor::{with_states,SurfaceAttributes},presentation::{PresentationFeedbackCachedState,PresentationFeedbackCallback,Refresh}};
use std::{io::{Read,Seek,SeekFrom},os::unix::net::UnixStream,sync::{Arc,mpsc},thread,time::{Duration,Instant}};

#[derive(Debug)]
struct Snapshot {
    presented: Vec<u64>,
    can_submit: bool,
    closed: bool,
}
enum Command {
    Draw(u64),
    Poll,
    InvalidateSource,
    Stop,
}
#[derive(Debug)]
enum Reply {
    Connected,
    Drawn(bool),
    Snapshot(Snapshot),
    Failed(String),
    Stopped,
}
struct Peer {
    command: mpsc::Sender<Command>,
    reply: mpsc::Receiver<Reply>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.command.send(Command::Stop);
    }
}
impl Peer {
    fn start(stream: UnixStream) -> Self {
        let (commands, rx) = mpsc::channel();
        let (tx, replies) = mpsc::channel();
        let thread = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> crate::remote::RemoteResult<()> {
                    let mut viewer = WaylandViewer::connect_test_stream(
                        stream,
                        800,
                        600,
                        None,
                        "private Wayland viewer test",
                    )?;
                    tx.send(Reply::Connected)?;
                    loop {
                        match rx.recv_timeout(Duration::from_secs(12))? {
                            Command::Draw(sequence) => {
                                let frame = RecyclableDecodedFrame::new(
                                    sequence,
                                    800,
                                    600,
                                    RgbImage::from_pixel(
                                        800,
                                        600,
                                        Rgb(if sequence % 2 == 0 {
                                            [0, 0, 255]
                                        } else {
                                            [255, 0, 0]
                                        }),
                                    ),
                                    new_decode_buffer_pool(),
                                );
                                tx.send(Reply::Drawn(viewer.draw_recyclable(frame)?))?;
                            }
                            Command::Poll => {
                                let events = viewer.poll_events()?;
                                assert!(
                                    events.iter().all(|e| matches!(e, ViewerEvent::Close)),
                                    "readonly viewer emits input"
                                );
                                let closed = events.contains(&ViewerEvent::Close);
                                tx.send(Reply::Snapshot(Snapshot {
                                    presented: viewer.take_presented_sequences()?,
                                    can_submit: viewer.can_submit_frame(),
                                    closed,
                                }))?;
                            }
                            Command::InvalidateSource => {
                                viewer.invalidate_source("replacement source".into())?;
                                return Err("source switch unexpectedly succeeded".into());
                            }
                            Command::Stop => {
                                tx.send(Reply::Stopped)?;
                                return Ok(());
                            }
                        }
                    }
                },
            ));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = tx.send(Reply::Failed(e.to_string()));
                }
                Err(_) => {
                    let _ = tx.send(Reply::Failed("viewer worker panicked".into()));
                }
            }
        });
        Self {
            command: commands,
            reply: replies,
            thread: Some(thread),
        }
    }
    fn recv(&self, server: &mut Server) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            server.roundtrip();
            // This owned fixture has no WM layout tick. Issue the real initial
            // configure once, rather than relying on an undriven calloop timer.
            for top in server.state.toplevels.values() {
                if !top.is_initial_configure_sent() {
                    top.with_pending_state(|state| state.size = Some((800, 600).into()));
                    top.send_configure();
                }
            }
            server.roundtrip();
            match self.reply.try_recv() {
                Ok(reply) => return reply,
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("viewer reply channel disconnected")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "viewer protocol phase timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn poll(&self, server: &mut Server) -> Snapshot {
        self.command.send(Command::Poll).unwrap();
        match self.recv(server) {
            Reply::Snapshot(snapshot) => snapshot,
            other => panic!("expected snapshot, got {other:?}"),
        }
    }
    fn join_failed(mut self) {
        self.thread.take().unwrap().join().unwrap();
    }
    fn stop(mut self, server: &mut Server) {
        self.command.send(Command::Stop).unwrap();
        assert!(matches!(self.recv(server), Reply::Stopped));
        self.thread.take().unwrap().join().unwrap();
    }
}
// Production nested state intentionally exposes no presentation global. The
// owned compositor explicitly takes responsibility for controlled GL feedback.
fn register_test_presentation(server: &mut Server) {
    server.state.presentation_state = smithay::wayland::presentation::PresentationState::new::<
        crate::backend::wayland::state::JwmWaylandState,
    >(&server.display.handle(), libc::CLOCK_MONOTONIC as u32);
}
fn server_and_peer(missing_presentation: bool) -> (Server, Peer) {
    let (server, peer, _probe) = server_and_peer_with_probe(missing_presentation);
    (server, peer)
}
fn server_and_peer_with_probe(missing_presentation: bool) -> (Server, Peer, UnixStream) {
    let mut server = Server::new();
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    if !missing_presentation {
        register_test_presentation(&mut server);
    }
    let (native, stream) = UnixStream::pair().unwrap();
    let probe = native.try_clone().unwrap();
    server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let peer = Peer::start(stream);
    (server, peer, probe)
}
fn viewer_surface(server: &Server) -> WlSurface {
    assert_eq!(
        server.state.toplevels.len(),
        1,
        "fixture identifies sole viewer before adding interference"
    );
    server
        .state
        .toplevels
        .values()
        .next()
        .unwrap()
        .wl_surface()
        .clone()
}
fn take_feedback(surface: &WlSurface) -> Vec<PresentationFeedbackCallback> {
    with_states(surface, |states| {
        std::mem::take(
            &mut states
                .cached_state
                .get::<PresentationFeedbackCachedState>()
                .current()
                .callbacks,
        )
    })
}
fn wait_feedback(server: &mut Server, surface: &WlSurface) -> Vec<PresentationFeedbackCallback> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        server.roundtrip();
        let callbacks = take_feedback(surface);
        if !callbacks.is_empty() {
            assert_eq!(callbacks.len(), 1);
            return callbacks;
        }
        assert!(Instant::now() < deadline, "viewer never committed feedback");
        thread::sleep(Duration::from_millis(1));
    }
}
fn committed_buffer(surface: &WlSurface) -> WlBuffer {
    with_renderer_surface_state(surface, |s| s.buffer().map(|b| (**b).clone()))
        .flatten()
        .expect("actual committed viewer SHM buffer")
}
fn frame_callback_only(surface: &WlSurface) {
    with_states(surface, |states| {
        for callback in states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .frame_callbacks
            .drain(..)
        {
            callback.done(1);
        }
    });
}
fn present(server: &mut Server, feedback: Vec<PresentationFeedbackCallback>) {
    let output = crate::backend::wayland_udev::image_copy_capture::wire_test_client::test_output(
        "viewer-test-output",
    );
    for callback in feedback {
        callback.presented(
            &output,
            Duration::from_secs(1),
            Refresh::Unknown,
            1,
            Kind::empty(),
        );
    }
    server.roundtrip();
}
fn verify_actual_gl_pixels(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    capture: &mut Capture,
) {
    verify_actual_gl_color(server, gl, capture, false);
}
fn verify_actual_gl_color(
    server: &mut Server,
    gl: &mut HeadlessSmithayGles,
    capture: &mut Capture,
    blue: bool,
) {
    let (buffer, mut file) = shm_buffer_file(server, &mut capture.client, [0x5a; 4]);
    let frame = capture.client.new_id();
    capture.client.request(capture.session, 0, &[frame]);
    capture.client.request(frame, 1, &[buffer]);
    capture.client.request(frame, 3, &[]);
    server.roundtrip();
    let queue = server.state.image_capture_pending.as_ref().unwrap().clone();
    assert_eq!(queue.lock().unwrap().len(), 1);
    fulfill_test_toplevel_capture(&mut gl.renderer, &mut None, &server.state, &queue);
    server.roundtrip();
    let events = capture.client.events();
    assert!(events.iter().all(|e| !(e.sender == 1 && e.opcode == 0)));
    assert!(
        events.iter().any(|e| e.sender == frame && e.opcode == 3),
        "real renderer ready"
    );
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut pixels = Vec::new();
    file.read_to_end(&mut pixels).unwrap();
    assert_eq!(pixels.len(), 800 * 600 * 4);
    assert!(
        pixels.chunks_exact(4).all(|p| if blue {
            p[0] >= 250 && p[1] <= 3 && p[2] <= 3 && p[3] == 255
        } else {
            p[0] <= 3 && p[1] <= 3 && p[2] >= 250 && p[3] == 255
        }),
        "viewer must render selected red frame, not unrelated blue app"
    );
    capture.client.request(frame, 0, &[]);
    server.roundtrip();
}

#[test]
fn actual_wayland_viewer_requires_presentation_global_without_fallback() {
    let (mut server, peer) = server_and_peer(true);
    let reply = peer.recv(&mut server);
    assert!(
        matches!(reply,Reply::Failed(ref error) if error.to_lowercase().contains("presentation")),
        "missing presentation must fail explicitly: {reply:?}"
    );
}

#[test]
fn actual_wayland_viewer_release_and_frame_done_do_not_ack_and_presented_does() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    peer.command.send(Command::Draw(11)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    let buffer = committed_buffer(&surface);
    let mut capture = Capture::new(&mut server);
    let _blue = App::with_color(&mut server, [255, 0, 0, 255]);
    verify_actual_gl_pixels(&mut server, &mut gl, &mut capture);
    frame_callback_only(&surface);
    buffer.release();
    server.roundtrip();
    let before = peer.poll(&mut server);
    assert!(before.presented.is_empty());
    assert!(!before.can_submit);
    peer.command.send(Command::Draw(12)).unwrap();
    assert!(
        matches!(peer.recv(&mut server), Reply::Drawn(false)),
        "release before presented must not permit overwrite or replacement"
    );
    assert_eq!(committed_buffer(&surface).id(), buffer.id());
    present(&mut server, feedback);
    let after = peer.poll(&mut server);
    assert_eq!(after.presented, [11]);
    assert!(after.can_submit);
    assert!(
        peer.poll(&mut server).presented.is_empty(),
        "no duplicate ACK"
    );
    peer.stop(&mut server);
}

#[test]
fn actual_wayland_viewer_delayed_release_bounds_submission_and_close_stays_live() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    peer.command.send(Command::Draw(21)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    let held_first = with_renderer_surface_state(&surface, |s| s.buffer().cloned())
        .flatten()
        .unwrap();
    let mut capture = Capture::new(&mut server);
    verify_actual_gl_pixels(&mut server, &mut gl, &mut capture);
    present(&mut server, feedback);
    let state = peer.poll(&mut server);
    assert_eq!(state.presented, [21]);
    assert!(
        state.can_submit,
        "second bounded slot permits compositor replacement"
    );
    peer.command.send(Command::Draw(22)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let second = wait_feedback(&mut server, &surface);
    assert_ne!(
        held_first.id(),
        committed_buffer(&surface).id(),
        "unreleased storage must not be reused"
    );
    smithay::wayland::shm::with_buffer_contents(&held_first, |ptr, len, data| {
        assert_eq!((data.width, data.height, data.stride), (800, 600, 3200));
        let offset = usize::try_from(data.offset).unwrap();
        let bytes = 800 * 600 * 4;
        assert!(offset.checked_add(bytes).is_some_and(|end| end <= len));
        // SAFETY: the callback keeps the pool mapping alive and the byte range was checked above.
        let pixels = unsafe { std::slice::from_raw_parts(ptr.add(offset), bytes) };
        assert!(
            pixels.chunks_exact(4).all(|p| p[..3] == [0, 0, 255]),
            "second blue frame must not overwrite retained red SHM"
        );
    })
    .unwrap();
    verify_actual_gl_color(&mut server, &mut gl, &mut capture, true);
    present(&mut server, second);
    let full = peer.poll(&mut server);
    assert_eq!(full.presented, [22]);
    assert!(!full.can_submit, "two retained slots bound allocation");
    peer.command.send(Command::Draw(23)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(false)));
    drop(held_first);
    server.roundtrip();
    assert!(
        peer.poll(&mut server).can_submit,
        "real buffer release unblocks replacement"
    );
    server.state.toplevels.values().next().unwrap().send_close();
    server.roundtrip();
    assert!(
        peer.poll(&mut server).closed,
        "close must work while release is stalled"
    );
    peer.stop(&mut server);
}

/// Observe actual server resource identities, not client-side allocation counters
/// or protocol IDs which can be recycled after destroy. Keep each old Buffer
/// leased by the compositor until its replacement has really been imported.
#[test]
fn actual_wayland_viewer_stable_size_reuses_two_buffers_with_fresh_feedback() {
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    let mut capture = None;
    let mut previous: Option<smithay::backend::renderer::utils::Buffer> = None;
    let mut identities = Vec::new();
    for sequence in 101..=108 {
        peer.command.send(Command::Draw(sequence)).unwrap();
        assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
        let feedback = wait_feedback(&mut server, &surface);
        let held = with_renderer_surface_state(&surface, |s| s.buffer().cloned())
            .flatten()
            .unwrap();
        let identity = held.id();
        if !identities.contains(&identity) {
            identities.push(identity.clone());
        }
        assert!(
            identities.len() <= 2,
            "stable-size stream allocated a third wl_buffer"
        );
        if let Some(ref old) = previous {
            assert_ne!(
                old.id(),
                identity,
                "cannot overwrite the unreleased previous buffer"
            );
            smithay::wayland::shm::with_buffer_contents(old, |ptr, len, data| {
                assert_eq!((data.width, data.height, data.stride), (800, 600, 3200));
                let offset = usize::try_from(data.offset).unwrap();
                let bytes = 800 * 600 * 4;
                assert!(offset.checked_add(bytes).is_some_and(|end| end <= len));
                // SAFETY: the checked range stays in the live SHM mapping.
                let pixels = unsafe { std::slice::from_raw_parts(ptr.add(offset), bytes) };
                let expected = if (sequence - 1) % 2 == 0 {
                    [255, 0, 0]
                } else {
                    [0, 0, 255]
                };
                assert!(
                    pixels.chunks_exact(4).all(|p| p[..3] == expected),
                    "replacement must not alter the held previous frame"
                );
            })
            .unwrap();
        }
        let capture = capture.get_or_insert_with(|| Capture::new(&mut server));
        verify_actual_gl_color(&mut server, &mut gl, capture, sequence % 2 == 0);
        // A released OLD slot is reusable storage, not completion of the new
        // frame occupying the other slot. Neither it nor frame.done earns ACK.
        drop(previous.take());
        frame_callback_only(&surface);
        server.roundtrip();
        let waiting = peer.poll(&mut server);
        assert!(
            waiting.presented.is_empty(),
            "release cannot reuse an old presentation ACK"
        );
        assert!(!waiting.can_submit, "new feedback is still outstanding");
        present(&mut server, feedback);
        let done = peer.poll(&mut server);
        assert_eq!(done.presented, [sequence]);
        assert!(done.can_submit);
        assert!(
            peer.poll(&mut server).presented.is_empty(),
            "feedback is consumed exactly once"
        );
        previous = Some(held);
    }
    assert_eq!(
        identities.len(),
        2,
        "held-frame replacement must use exactly two resources"
    );
    // A discarded feedback on another reuse is still terminal, and cannot
    // inherit any of the preceding successful frame acknowledgments.
    peer.command.send(Command::Draw(109)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    assert!(identities.contains(&committed_buffer(&surface).id()));
    verify_actual_gl_color(&mut server, &mut gl, capture.as_mut().unwrap(), false);
    for callback in feedback {
        callback.discarded();
    }
    server.roundtrip();
    peer.command.send(Command::Poll).unwrap();
    let reply = peer.recv(&mut server);
    assert!(
        matches!(reply, Reply::Failed(ref error) if error.to_lowercase().contains("discard")),
        "reused buffer discard must fail, never ACK: {reply:?}"
    );
    peer.join_failed();
    drop(previous);
}

#[test]
fn actual_wayland_viewer_idle_resize_waits_for_destroy_sync_and_preserves_busy_storage() {
    use std::os::fd::AsRawFd;
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let (mut server, peer, probe) = server_and_peer_with_probe(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    peer.command.send(Command::Draw(101)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    let first = committed_buffer(&surface);
    let held_first = with_renderer_surface_state(&surface, |s| s.buffer().cloned())
        .flatten()
        .unwrap();
    let mut capture = Capture::new(&mut server);
    verify_actual_gl_color(&mut server, &mut gl, &mut capture, false);
    present(&mut server, feedback);
    assert_eq!(peer.poll(&mut server).presented, [101]);
    peer.command.send(Command::Draw(102)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    let held_second = with_renderer_surface_state(&surface, |s| s.buffer().cloned())
        .flatten()
        .unwrap();
    verify_actual_gl_color(&mut server, &mut gl, &mut capture, true);
    present(&mut server, feedback);
    assert_eq!(peer.poll(&mut server).presented, [102]);
    drop(held_first);
    server.roundtrip();
    assert!(peer.poll(&mut server).can_submit);
    let top = server.state.toplevels.values().next().unwrap();
    top.with_pending_state(|state| state.size = Some((640, 480).into()));
    top.send_configure();
    server.roundtrip();
    assert!(peer.poll(&mut server).presented.is_empty());
    // Drain the configure ACK before pausing server dispatch, so the following
    // observed wl_display.sync belongs specifically to retirement.
    server.roundtrip();
    peer.command.send(Command::Draw(103)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut bytes = [0u8; 4096];
        // SAFETY: valid socket and writable slice; MSG_PEEK does not consume
        // bytes or dispatch protocol requests. Retirement sends no SCM_RIGHTS.
        let count = unsafe {
            libc::recv(
                probe.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        let mut sync_seen = false;
        if count > 0 {
            let mut at = 0;
            while at + 8 <= count as usize {
                let object = u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
                let header = u32::from_ne_bytes(bytes[at + 4..at + 8].try_into().unwrap());
                let length = (header >> 16) as usize;
                assert!(length >= 8 && length % 4 == 0);
                if at + length > count as usize {
                    break;
                }
                sync_seen |= object == 1 && header & 0xffff == 0;
                at += length;
            }
        } else if count < 0 {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        if sync_seen {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "retirement did not send real wl_display.sync"
        );
        thread::yield_now();
    }
    assert!(
        matches!(peer.reply.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "cannot allocate/submit replacement before server sync acknowledgment"
    );
    assert!(
        first.is_alive(),
        "server has deliberately not processed destroy yet"
    );
    assert_eq!(committed_buffer(&surface).id(), held_second.id());
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    assert!(
        !first.is_alive(),
        "old idle resource must be destroyed before new allocation"
    );
    let replacement = committed_buffer(&surface);
    assert_ne!(replacement.id(), first.id());
    assert_ne!(replacement.id(), held_second.id());
    for (buffer, width, height, expected) in [
        (&replacement, 640, 480, [0, 0, 255]),
        (&*held_second, 800, 600, [255, 0, 0]),
    ] {
        smithay::wayland::shm::with_buffer_contents(buffer, |ptr, len, data| {
            assert_eq!(
                (data.width, data.height, data.stride),
                (width, height, width * 4)
            );
            let offset = usize::try_from(data.offset).unwrap();
            let bytes = usize::try_from(width * height * 4).unwrap();
            assert!(offset.checked_add(bytes).is_some_and(|end| end <= len));
            // SAFETY: validated byte range under the live SHM pool callback.
            let pixels = unsafe { std::slice::from_raw_parts(ptr.add(offset), bytes) };
            assert!(pixels.chunks_exact(4).all(|p| p[..3] == expected));
        })
        .unwrap();
    }
    assert!(peer.poll(&mut server).presented.is_empty());
    // This final assertion is a real SHM/layout test, not a GL/physical-present
    // claim. Do not manufacture presentation for the resized buffer.
    for callback in feedback {
        callback.discarded();
    }
    server.roundtrip();
    peer.command.send(Command::Poll).unwrap();
    assert!(
        matches!(peer.recv(&mut server), Reply::Failed(ref e) if e.to_lowercase().contains("discard"))
    );
    peer.join_failed();
}

#[test]
fn actual_wayland_viewer_discard_is_terminal_and_never_acknowledged() {
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    peer.command.send(Command::Draw(31)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    for feedback in wait_feedback(&mut server, &surface) {
        feedback.discarded();
    }
    server.roundtrip();
    peer.command.send(Command::Poll).unwrap();
    let reply = peer.recv(&mut server);
    assert!(
        matches!(reply,Reply::Failed(ref e) if e.to_lowercase().contains("discard")),
        "discard must fail without ACK: {reply:?}"
    );
}

#[test]
fn actual_wayland_viewer_resize_invalidates_pending_feedback_without_ack() {
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    let client = surface.client().expect("live viewer");
    assert!(
        server.state.seat.client_seats(&client).is_empty(),
        "readonly backend must not bind wl_seat"
    );
    peer.command.send(Command::Draw(41)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    let top = server.state.toplevels.values().next().unwrap();
    top.with_pending_state(|state| state.size = Some((640, 480).into()));
    top.send_configure();
    server.roundtrip();
    peer.command.send(Command::Poll).unwrap();
    let reply = peer.recv(&mut server);
    assert!(
        matches!(reply, Reply::Failed(ref error) if error == "Wayland surface changed before presentation completed"),
        "configure must return the expected lifecycle error, not panic or ACK: {reply:?}"
    );
    peer.join_failed();
    for callback in feedback {
        callback.discarded();
    }
}

/// Production authenticated TCP client, decoder and native viewer. The scripted
/// peer uses production authentication/encoding; only display completion timing
/// belongs to the explicitly test-owned compositor, not a claimed KMS device.
#[test]
fn actual_wayland_viewer_tcp_ack_waits_for_presented_not_release() {
    use super::*;
    use crate::remote::{
        client::{ClientOptions, run_client},
        tile::{TileEncodeRequest, TileEncoder},
        viewer_backend::{ViewerBackend, with_wayland_test_stream},
    };
    let Some(mut gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut server = Server::new();
    register_test_presentation(&mut server);
    server.state.image_capture_pending = Some(init_image_copy_capture(&server.display.handle()));
    let (native, stream) = UnixStream::pair().unwrap();
    server
        .display
        .handle()
        .insert_client(native, Arc::new(JwmClientState::default()))
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (records, observed) = mpsc::channel();
    let key = vec![0x4e; 32];
    let host_key = key.clone();
    let host = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut peer = authenticate_peer(stream, &host_key).unwrap();
        assert!(
            !peer.hello.request_input
                && !peer.hello.request_clipboard
                && !peer.hello.request_clipboard_files
        );
        assert!(
            !peer.hello.stream.wayland_keyboard
                && !peer.hello.stream.wayland_pointer
                && !peer.hello.stream.window_resize
        );
        peer.writer
            .write_message(
                MessageKind::HelloAck,
                &ServerHello {
                    stream: Default::default(),
                    pointer_enabled: false,
                    keyboard_enabled: false,
                    clipboard_enabled: false,
                    clipboard_files_enabled: false,
                }
                .encode(),
            )
            .unwrap();
        let catalog = SourceCatalog {
            current: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            entries: vec![SourceEntry {
                id: SourceId(1),
                label: "test-owned red source".into(),
            }],
        };
        peer.writer
            .write_message(MessageKind::SourceCatalog, &catalog.encode().unwrap())
            .unwrap();
        let frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 800,
            source_height: 600,
            image: RgbImage::from_pixel(800, 600, Rgb([255, 0, 0])),
        };
        let mut encoder = TileEncoder::new();
        encoder.plan(&frame, TileEncodeRequest::Keyframe).unwrap();
        let mut payload = Vec::new();
        encoder.encode_into(&mut payload, 71, &frame, 100).unwrap();
        peer.writer
            .write_message(
                MessageKind::Frame,
                &encode_source_payload(1, &payload).unwrap(),
            )
            .unwrap();
        peer.writer.flush().unwrap();
        loop {
            let (kind, payload) = peer.reader.read_message().unwrap();
            records.send((kind, payload)).unwrap();
            if kind == MessageKind::Close {
                break;
            }
        }
    });
    let (done_tx, done) = mpsc::channel();
    let viewer = thread::spawn(move || {
        let options = ClientOptions {
            viewer_backend: ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_pointer: false,
            wayland_scroll: false,
            resize_source: None,
            follow_source_size: false,
            window_size: None,
            window_position: None,
            frame_limits: Default::default(),
            address: address.clone(),
            peer_label: address,
            key: crate::remote::key::KeySource::Bytes(key),
            display: None,
            view_only: true,
            grab_input: false,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "Escape".into(),
        };
        let result = with_wayland_test_stream(stream, || run_client(options));
        done_tx.send(result.map_err(|e| e.to_string())).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let (surface, feedback) = loop {
        server.roundtrip();
        for top in server.state.toplevels.values() {
            if !top.is_initial_configure_sent() {
                top.with_pending_state(|s| s.size = Some((800, 600).into()));
                top.send_configure();
            }
        }
        server.roundtrip();
        if let Some(top) = server.state.toplevels.values().next() {
            let surface = top.wl_surface().clone();
            let feedback = take_feedback(&surface);
            if !feedback.is_empty() {
                break (surface, feedback);
            }
        }
        if let Ok(result) = done.try_recv() {
            panic!("viewer ended before drawing: {result:?}");
        }
        assert!(Instant::now() < deadline, "TCP viewer did not commit frame");
        thread::sleep(Duration::from_millis(1));
    };
    assert!(
        server
            .state
            .seat
            .client_seats(&surface.client().unwrap())
            .is_empty()
    );
    let mut capture = Capture::new(&mut server);
    let _blue = App::with_color(&mut server, [255, 0, 0, 255]);
    verify_actual_gl_pixels(&mut server, &mut gl, &mut capture);
    committed_buffer(&surface).release();
    server.roundtrip();
    let until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < until {
        server.roundtrip();
        for (kind, _) in observed.try_iter() {
            assert!(
                matches!(kind, MessageKind::Viewport | MessageKind::Heartbeat),
                "unexpected record before presentation: {kind:?}"
            );
        }
        thread::sleep(Duration::from_millis(1));
    }
    present(&mut server, feedback);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        server.roundtrip();
        match observed.try_recv() {
            Ok((MessageKind::FrameAck, payload)) => {
                assert_eq!(
                    crate::remote::messages::decode_source_frame_ack(&payload).unwrap(),
                    (1, 71)
                );
                break;
            }
            Ok((kind, _)) => assert!(matches!(
                kind,
                MessageKind::Viewport | MessageKind::Heartbeat
            )),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(e) => panic!("TCP peer ended: {e}"),
        }
        assert!(
            Instant::now() < deadline,
            "no TCP ACK after actual presentation feedback"
        );
        thread::sleep(Duration::from_millis(1));
    }
    server
        .state
        .toplevels
        .values()
        .find(|t| t.wl_surface() == &surface)
        .unwrap()
        .send_close();
    server.roundtrip();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        server.roundtrip();
        match done.try_recv() {
            Ok(result) => {
                result.unwrap();
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(e) => panic!("viewer result lost: {e}"),
        }
        assert!(Instant::now() < deadline, "viewer close stalled");
        thread::sleep(Duration::from_millis(1));
    }
    viewer.join().unwrap();
    host.join().unwrap();
    for (kind, _) in observed.try_iter() {
        assert!(
            matches!(
                kind,
                MessageKind::Close | MessageKind::Heartbeat | MessageKind::Viewport
            ),
            "duplicate ACK or input: {kind:?}"
        );
    }
}

#[test]
fn actual_wayland_viewer_source_switch_rejects_pending_presentation_without_ack() {
    let (mut server, peer) = server_and_peer(false);
    assert!(matches!(peer.recv(&mut server), Reply::Connected));
    let surface = viewer_surface(&server);
    peer.command.send(Command::Draw(51)).unwrap();
    assert!(matches!(peer.recv(&mut server), Reply::Drawn(true)));
    let feedback = wait_feedback(&mut server, &surface);
    committed_buffer(&surface).release();
    server.roundtrip();
    assert!(peer.poll(&mut server).presented.is_empty());
    peer.command.send(Command::InvalidateSource).unwrap();
    let reply = peer.recv(&mut server);
    assert!(
        matches!(reply, Reply::Failed(ref error) if error == "Wayland viewer source switching is unsupported; session closed"),
        "source switch must return its specific refusal, not panic or ACK: {reply:?}"
    );
    peer.join_failed();
    // An unpresented commit is discarded when the test-owned display session
    // ends. No success event is invented for the invalidated source.
    for callback in feedback {
        callback.discarded();
    }
}

#[test]
fn actual_wayland_viewer_nested_registry_does_not_advertise_unfulfilled_presentation() {
    let mut server = Server::new();
    let mut client = server.connect();
    // A second real registry request gives this test the exact wire globals,
    // independently of the fixture's cached private registry lookup.
    let registry = client.new_id();
    client.request(1, 1, &[registry]);
    server.roundtrip();
    let names: Vec<String> = client
        .events()
        .into_iter()
        .filter(|event| event.sender == registry && event.opcode == 0)
        .map(|event| {
            assert!(event.args.len() >= 3);
            let len = usize::try_from(event.args[1]).unwrap();
            let bytes: Vec<u8> = event.args[2..]
                .iter()
                .flat_map(|word| word.to_ne_bytes())
                .collect();
            assert!(len > 0 && len <= bytes.len());
            assert_eq!(bytes[len - 1], 0);
            String::from_utf8(bytes[..len - 1].to_vec()).unwrap()
        })
        .collect();
    assert!(
        names.iter().any(|name| name == "wl_compositor"),
        "real registry positive control"
    );
    assert!(
        !names.iter().any(|name| name == "wp_presentation"),
        "nested backend must not advertise feedback it cannot complete"
    );
}
