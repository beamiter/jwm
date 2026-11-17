//! Internal adapter -> real native v2 wire -> production offscreen GL.
//! Root-only adapter evidence: no arm/action, TCP, viewer presentation or GTK claim.
use super::wayland_capture::WaylandCapture;
use crate::backend::wayland::state::JwmClientState;
use crate::backend::wayland_udev::{
    backend::fulfill_test_toplevel_capture,
    compositor::headless_render::HeadlessSmithayGles,
    image_copy_capture::{
        authority_tests::App,
        init_image_copy_capture,
        wire_test_client::{Client, Event, Server},
    },
    remote_popup,
};
use std::{
    os::unix::net::UnixStream,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
const W: u32 = 32;
const H: u32 = 24;
enum Command {
    Capture,
    Discard,
    DestroyOriginal,
    RejectInputUpgrades,
}
#[derive(Debug)]
struct Observation {
    epoch: u32,
    cookie: u32,
    sequence: u64,
    serial: u64,
    generation: u64,
    root: (u32, u32),
    has_popup: bool,
    pixels: Vec<u8>,
}
enum Reply {
    Connected,
    Frame(Observation),
    Done,
    Failed(Option<std::io::ErrorKind>, String),
}
fn failure(error: super::RemoteError) -> Reply {
    Reply::Failed(
        error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        error.to_string(),
    )
}
fn terminal(reply: Reply, ttl: bool) {
    match reply {
        Reply::Failed(kind, message) => {
            assert_eq!(
                kind,
                Some(std::io::ErrorKind::InvalidData),
                "expected authority failure, not I/O loss"
            );
            assert!(
                message == "interactive popup lease revoked"
                    || (!ttl && message == "Wayland image capture session stopped")
                    || (ttl && message == "interactive popup lease expired"),
                "unexpected terminal category: {message}"
            );
        }
        _ => panic!("terminal source must not yield pixels or reconnect"),
    }
}
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
            "unexpected native protocol error"
        );
        let complete = events.iter().any(|e| e.sender == done && e.opcode == 0);
        all.extend(events);
        if complete {
            return all;
        }
        assert!(Instant::now() < deadline, "native fence deadline");
        thread::yield_now();
    }
}
struct Harness {
    server: Server,
    app: App,
    other: Option<App>,
    gl: HeadlessSmithayGles,
    commands: Option<mpsc::Sender<Command>>,
    replies: mpsc::Receiver<Reply>,
    worker: Option<thread::JoinHandle<()>>,
    pointer: u32,
    other_pointer: u32,
}
impl Harness {
    fn new(gl: HeadlessSmithayGles) -> Self {
        let mut server = Server::new();
        server.state.image_capture_pending =
            Some(init_image_copy_capture(&server.display.handle()));
        let mut app = App::with_size_color(&mut server, [0, 0, 255, 255], W, H);
        fence(&mut server, &mut app.client);
        let (native, peer) = UnixStream::pair().unwrap();
        server
            .display
            .handle()
            .insert_client(native, Arc::new(JwmClientState::default()))
            .unwrap();
        let (commands, requests) = mpsc::channel();
        let (results, replies) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut adapter = match WaylandCapture::connect_test_stream_interactive_popup(peer, 1) {
                Ok(a) => a,
                Err(e) => {
                    let _ = results.send(failure(e));
                    return;
                }
            };
            if results.send(Reply::Connected).is_err() {
                return;
            }
            while let Ok(command) = requests.recv() {
                let reply = match command {
                    Command::Capture => match adapter.capture_interactive_popup_frame() {
                        Ok(value) => Reply::Frame(Observation {
                            epoch: value.proof.epoch,
                            cookie: value.proof.cookie,
                            sequence: value.proof.capture_sequence,
                            serial: value.proof.native_ready_serial,
                            generation: value.proof.generation,
                            root: value.proof.root_size,
                            has_popup: value.proof.popup_rect.is_some(),
                            pixels: value.frame.image.into_raw(),
                        }),
                        Err(e) => failure(e),
                    },
                    Command::RejectInputUpgrades => {
                        for (result, expected) in [
                            (
                                adapter.enable_keyboard(),
                                "interactive family cannot upgrade input",
                            ),
                            (
                                adapter.enable_pointer(),
                                "native pointer unavailable or enable too late",
                            ),
                            (
                                adapter.enable_control(false, false),
                                "native control manager unavailable or enable too late",
                            ),
                            (
                                adapter.enable_text(),
                                "text requires an unused exclusive native source",
                            ),
                        ] {
                            let error = result.expect_err(
                                "interactive family must not enable another input mode",
                            );
                            assert_eq!(
                                error
                                    .downcast_ref::<std::io::Error>()
                                    .map(std::io::Error::kind),
                                Some(std::io::ErrorKind::InvalidData)
                            );
                            assert_eq!(error.to_string(), expected);
                        }
                        Reply::Done
                    }
                    Command::Discard => {
                        adapter.discard_popup_frame();
                        Reply::Done
                    }
                    Command::DestroyOriginal => {
                        adapter.destroy_original_popup_source_for_test();
                        Reply::Done
                    }
                };
                if results.send(reply).is_err() {
                    break;
                }
            }
        });
        let mut h = Self {
            server,
            app,
            other: None,
            gl,
            commands: Some(commands),
            replies,
            worker: Some(worker),
            pointer: 0,
            other_pointer: 0,
        };
        assert!(
            matches!(h.receive(), Reply::Connected),
            "real popup adapter connection"
        );
        let events = fence(&mut h.server, &mut h.app.client);
        let seats: Vec<_> = events
            .iter()
            .filter(|e| e.sender == 2 && e.opcode == 0)
            .filter_map(|e| {
                let len = e.args[1] as usize;
                let bytes: Vec<_> = e.args[2..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                (&bytes[..len - 1] == b"wl_seat").then_some(e.args[0])
            })
            .collect();
        assert_eq!(seats.len(), 1, "one target-private lease seat");
        let seat = h.app.client.new_id();
        h.app.client.request(
            2,
            0,
            &[
                seats[0],
                8,
                u32::from_ne_bytes(*b"wl_s"),
                u32::from_ne_bytes(*b"eat\0"),
                7,
                seat,
            ],
        );
        h.pointer = h.app.client.new_id();
        h.app.client.request(seat, 0, &[h.pointer]);
        h.assert_zero_input();
        let mut other = App::with_size_color(&mut h.server, [255, 0, 0, 255], W, H);
        assert_eq!(
            other.client.global_count("wl_seat"),
            1,
            "bystander sees no family seat"
        );
        let seat = other.client.bind("wl_seat", 7);
        h.other_pointer = other.client.new_id();
        other.client.request(seat, 0, &[h.other_pointer]);
        fence(&mut h.server, &mut other.client);
        h.other = Some(other);
        h
    }
    fn receive(&mut self) -> Reply {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match self.replies.try_recv() {
                Ok(r) => return r,
                Err(mpsc::TryRecvError::Disconnected) => panic!("adapter exited before reply"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            self.server.dispatch_timers(Duration::from_millis(1));
            let queue = self.server.state.image_capture_pending.as_ref().unwrap();
            fulfill_test_toplevel_capture(
                &mut self.gl.renderer,
                &mut None,
                &self.server.state,
                queue,
            );
            self.server.roundtrip();
            assert!(Instant::now() < deadline, "bounded popup adapter operation");
        }
    }
    fn command(&mut self, c: Command) -> Reply {
        self.commands.as_ref().unwrap().send(c).unwrap();
        self.receive()
    }
    fn capture(&mut self) -> Observation {
        match self.command(Command::Capture) {
            Reply::Frame(o) => o,
            Reply::Failed(kind, e) => panic!("actual adapter frame failed ({kind:?}): {e}"),
            _ => panic!("frame response required"),
        }
    }
    fn discard(&mut self) {
        assert!(matches!(self.command(Command::Discard), Reply::Done));
    }
    fn assert_zero_input(&mut self) {
        let events = fence(&mut self.server, &mut self.app.client);
        assert!(
            !events.iter().any(|e| e.sender == self.pointer),
            "capture/export must not enter, move, click or frame pointer"
        );
        if let Some(other) = &mut self.other {
            let events = fence(&mut self.server, &mut other.client);
            assert!(
                !events.iter().any(|e| e.sender == self.other_pointer),
                "bystander pointer must receive no events"
            );
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(w) = self.worker.take() {
            w.join().expect("popup adapter thread");
        }
    }
}
fn root(o: &Observation, epoch: u32) {
    assert_eq!(o.epoch, epoch);
    assert_eq!(o.root, (W, H));
    assert!(!o.has_popup);
    assert!(o.cookie > 0 && o.sequence > 0 && o.serial > 0 && o.generation > 0);
    assert_eq!(o.pixels.len(), (W * H * 3) as usize);
    assert!(
        o.pixels.chunks_exact(3).all(|p| p == [255, 0, 0]),
        "only selected root pixels"
    );
}
#[test]
fn actual_popup_adapter_exports_root_metadata_without_consuming_input_or_renewing_lease() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    let timing = remote_popup::lease_timing_for_test(&h.server.state).unwrap();
    assert!(matches!(
        h.command(Command::RejectInputUpgrades),
        Reply::Done
    ));
    let first = h.capture();
    root(&first, timing.0);
    h.assert_zero_input();
    h.discard();
    let second = h.capture();
    root(&second, timing.0);
    assert!(
        second.serial > first.serial
            && second.sequence > first.sequence
            && second.cookie > first.cookie
    );
    assert_eq!(second.generation, first.generation);
    assert_eq!(
        remote_popup::lease_timing_for_test(&h.server.state),
        Some(timing)
    );
    h.assert_zero_input();
}
#[test]
fn actual_popup_adapter_original_source_destruction_is_terminal() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    h.capture();
    h.discard();
    assert!(matches!(h.command(Command::DestroyOriginal), Reply::Done));
    for _ in 0..2 {
        terminal(h.command(Command::Capture), false);
        h.discard();
    }
    assert!(!remote_popup::active(&h.server.state));
    h.assert_zero_input();
}
#[test]
fn actual_popup_adapter_root_unmap_remap_never_restores_old_family() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    h.capture();
    h.discard();
    h.app.unmap(&mut h.server);
    h.app.remap(&mut h.server);
    for _ in 0..2 {
        terminal(h.command(Command::Capture), false);
        h.discard();
    }
    assert!(!remote_popup::active(&h.server.state));
    h.assert_zero_input();
}
#[test]
fn actual_popup_adapter_original_thirty_second_expiry_is_terminal_without_input() {
    let Some(gl) = HeadlessSmithayGles::new() else {
        return;
    };
    let mut h = Harness::new(gl);
    let timing = remote_popup::lease_timing_for_test(&h.server.state).unwrap();
    h.capture();
    h.discard();
    while remote_popup::active(&h.server.state) {
        h.server.dispatch_timers(Duration::from_millis(10));
        if let Some(current) = remote_popup::lease_timing_for_test(&h.server.state) {
            assert_eq!(current, timing);
        } else {
            assert!(Instant::now() >= timing.2, "no early lease termination");
        }
        assert!(
            Instant::now() < timing.2 + Duration::from_secs(2),
            "native original TTL cleanup"
        );
    }
    for _ in 0..2 {
        terminal(h.command(Command::Capture), true);
        h.discard();
    }
    h.assert_zero_input();
}
