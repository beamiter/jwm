//! Trusted-LAN remote desktop host.

use super::RemoteResult;
use super::deadline::TcpStreamDeadline;
use super::frame::encode_frame;
use super::key::load_key_file;
use super::messages::{ClientHello, ServerHello, decode_input};
use super::protocol::{MessageKind, SessionReader, SessionWriter, server_handshake};
use super::x11_capture::{CaptureSource, CapturedFrame, X11Capture};
use super::x11_input::InputInjector;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
// The client cannot enter its heartbeat loop until the first frame is received,
// decoded and drawn.  Do not apply the held-input safety timeout before then.
const INITIAL_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

struct PendingFrame {
    frame: CapturedFrame,
    capture_elapsed: Duration,
    captured_at: Instant,
}

struct MailboxState<T> {
    latest: Option<T>,
    closed: bool,
}

/// A one-slot mailbox whose producer never waits for a slow consumer.
///
/// Replacing the queued item keeps latency bounded: the consumer works on at
/// most one item while exactly one newer item can wait behind it.
struct LatestMailbox<T> {
    state: Mutex<MailboxState<T>>,
    ready: Condvar,
    dropped: AtomicU64,
}

impl<T> LatestMailbox<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                latest: None,
                closed: false,
            }),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
        }
    }

    fn publish(&self, item: T) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.closed {
            return false;
        }
        let replaced = state.latest.replace(item);
        self.ready.notify_one();
        drop(state);
        if replaced.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    fn receive(&self) -> io::Result<Option<T>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame mailbox lock was poisoned"))?;
        loop {
            if state.closed {
                return Ok(None);
            }
            if let Some(item) = state.latest.take() {
                return Ok(Some(item));
            }
            state = self
                .ready
                .wait(state)
                .map_err(|_| io::Error::other("remote frame mailbox lock was poisoned"))?;
        }
    }

    fn close(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.closed = true;
        let pending = state.latest.take();
        self.ready.notify_all();
        drop(state);
        drop(pending);
    }

    fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::AcqRel)
    }
}

struct StopSessionOnDrop(Arc<AtomicBool>);

impl Drop for StopSessionOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum StopCause {
    None = 0,
    Graceful = 1,
    Input = 2,
    Sender = 3,
    Capture = 4,
    ThreadPanic = 5,
}

struct FirstStop(AtomicU8);

impl FirstStop {
    fn new() -> Self {
        Self(AtomicU8::new(StopCause::None as u8))
    }

    fn record(&self, cause: StopCause) {
        debug_assert!(!matches!(cause, StopCause::None | StopCause::ThreadPanic));
        let _ = self.0.compare_exchange(
            StopCause::None as u8,
            cause as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn promote_graceful_to_input_error(&self) {
        let _ = self.0.compare_exchange(
            StopCause::Graceful as u8,
            StopCause::Input as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn record_thread_panic(&self) {
        self.0
            .store(StopCause::ThreadPanic as u8, Ordering::Release);
    }

    fn cause(&self) -> StopCause {
        match self.0.load(Ordering::Acquire) {
            value if value == StopCause::Graceful as u8 => StopCause::Graceful,
            value if value == StopCause::Input as u8 => StopCause::Input,
            value if value == StopCause::Sender as u8 => StopCause::Sender,
            value if value == StopCause::Capture as u8 => StopCause::Capture,
            value if value == StopCause::ThreadPanic as u8 => StopCause::ThreadPanic,
            _ => StopCause::None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub listen: String,
    pub key_file: PathBuf,
    pub display: Option<String>,
    pub fps: u16,
    pub jpeg_quality: u8,
    pub max_width: u16,
    pub capture_source: CaptureSource,
    pub allow_lan: bool,
    pub allow_input: bool,
    pub once: bool,
}

pub fn run_host(options: HostOptions) -> RemoteResult<()> {
    let key = load_key_file(&options.key_file)?;
    let listener = TcpListener::bind(&options.listen)?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    let local_address = listener.local_addr()?;
    if !local_address.ip().is_loopback() && !options.allow_lan {
        return Err(permission_denied(format!(
            "refusing non-loopback listener {local_address}; pass --allow-lan for a trusted LAN"
        ))
        .into());
    }

    eprintln!("jwm-remote: listening on {local_address}");
    if !local_address.ip().is_loopback() {
        eprintln!(
            "jwm-remote: trusted-LAN mode authenticates traffic but does not encrypt the screen or input"
        );
    }
    if !options.allow_input {
        eprintln!("jwm-remote: input control is disabled (view-only host)");
    }

    while !shutdown.load(Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let peer = stream.peer_addr()?;
        eprintln!("jwm-remote: connection from {peer}");
        if let Err(error) = serve_client(stream, &key, &options, &shutdown) {
            eprintln!("jwm-remote: session with {peer} ended: {error}");
        }
        if options.once {
            return Ok(());
        }
    }
    Ok(())
}

fn serve_client(
    mut stream: TcpStream,
    key: &[u8],
    options: &HostOptions,
    shutdown: &Arc<AtomicBool>,
) -> RemoteResult<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut handshake_deadline = TcpStreamDeadline::arm(&stream, HANDSHAKE_TIMEOUT)?;
    let session_keys = server_handshake(&mut stream, key, rand::random())?;
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (receive_key, send_key) = session_keys.into_server();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    let mut writer = SessionWriter::new(stream, send_key);
    writer.get_ref().set_write_timeout(Some(WRITE_TIMEOUT))?;

    let (kind, hello_payload) = reader.read_message()?;
    if kind != MessageKind::Hello {
        return Err(
            invalid_data("first authenticated client message must negotiate the session").into(),
        );
    }
    let hello = ClientHello::decode(&hello_payload)?;
    handshake_deadline.cancel();
    let mut capture = X11Capture::connect(
        options.display.as_deref(),
        options.capture_source,
        options.max_width,
    )?;
    let mut keyboard_enabled = false;
    let mut verified_keymap = None;
    let injector = if options.allow_input && hello.request_input {
        match InputInjector::connect(options.display.as_deref()) {
            Ok(injector) => {
                match injector.keymap_fingerprint() {
                    Ok(fingerprint) if fingerprint == hello.keymap_fingerprint => {
                        keyboard_enabled = true;
                        verified_keymap = Some(fingerprint);
                    }
                    Ok(_) => {
                        eprintln!(
                            "jwm-remote: peer X11 keymap differs; keyboard disabled, pointer remains enabled"
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "jwm-remote: could not verify the host X11 keymap; keyboard disabled: {error}"
                        );
                    }
                }
                Some(injector)
            }
            Err(error) => {
                eprintln!("jwm-remote: XTEST input unavailable; continuing view-only: {error}");
                None
            }
        }
    } else {
        None
    };
    let pointer_enabled = injector.is_some();
    writer.write_message(
        MessageKind::HelloAck,
        &ServerHello {
            pointer_enabled,
            keyboard_enabled,
        }
        .encode(),
    )?;
    writer.flush()?;
    eprintln!(
        "jwm-remote: authenticated session started ({})",
        if keyboard_enabled {
            "screen + pointer + keyboard"
        } else if pointer_enabled {
            "screen + pointer"
        } else {
            "view only"
        }
    );

    let control = writer.get_ref().try_clone()?;
    let running = Arc::new(AtomicBool::new(true));
    let first_stop = Arc::new(FirstStop::new());
    let input_running = Arc::clone(&running);
    let input_stop = Arc::clone(&first_stop);
    let input_thread = thread::Builder::new()
        .name("jwm-remote-input".into())
        .spawn(move || {
            let _stop_on_exit = StopSessionOnDrop(Arc::clone(&input_running));
            receive_input(
                reader,
                injector,
                keyboard_enabled,
                verified_keymap,
                INITIAL_ACTIVITY_TIMEOUT,
                SESSION_IDLE_TIMEOUT,
                &input_running,
                &input_stop,
            )
        })?;

    let stream_result = stream_frames(
        &mut capture,
        writer,
        &control,
        &running,
        &first_stop,
        shutdown,
        options,
    );
    running.store(false, Ordering::Release);
    let _ = control.shutdown(Shutdown::Both);
    let input_result = match input_thread.join() {
        Ok(result) => result,
        Err(_) => return Err(io::Error::other("remote input thread panicked").into()),
    };
    match first_stop.cause() {
        StopCause::Input => input_result,
        StopCause::Graceful => {
            if let Err(error) = input_result {
                eprintln!("jwm-remote: input receiver stopped during shutdown: {error}");
            }
            stream_result
        }
        StopCause::Capture | StopCause::Sender | StopCause::ThreadPanic | StopCause::None => {
            if let Err(error) = input_result {
                eprintln!("jwm-remote: input receiver stopped: {error}");
            }
            stream_result
        }
    }
}

fn stream_frames(
    capture: &mut X11Capture,
    writer: SessionWriter<TcpStream>,
    control: &TcpStream,
    running: &Arc<AtomicBool>,
    first_stop: &Arc<FirstStop>,
    shutdown: &AtomicBool,
    options: &HostOptions,
) -> RemoteResult<()> {
    let mailbox = Arc::new(LatestMailbox::new());
    let sender_mailbox = Arc::clone(&mailbox);
    let sender_running = Arc::clone(running);
    let sender_stop = Arc::clone(first_stop);
    let sender_control = match control.try_clone() {
        Ok(control) => control,
        Err(error) => {
            first_stop.record(StopCause::Sender);
            return Err(error.into());
        }
    };
    let jpeg_quality = options.jpeg_quality;
    let sender = match thread::Builder::new()
        .name("jwm-remote-video".into())
        .spawn(move || {
            let _stop_on_exit = StopSessionOnDrop(Arc::clone(&sender_running));
            let result = send_frames(sender_mailbox, writer, &sender_running, jpeg_quality);
            if result.is_err() {
                sender_stop.record(StopCause::Sender);
                sender_running.store(false, Ordering::Release);
                let _ = sender_control.shutdown(Shutdown::Both);
            }
            result
        }) {
        Ok(sender) => sender,
        Err(error) => {
            first_stop.record(StopCause::Sender);
            return Err(error.into());
        }
    };

    let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps.clamp(1, 60)));
    let mut next_frame = Instant::now();
    let capture_result = (|| -> RemoteResult<()> {
        while running.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
            let now = Instant::now();
            if now < next_frame {
                thread::sleep((next_frame - now).min(Duration::from_millis(20)));
                continue;
            }
            next_frame += interval;
            if next_frame < now {
                next_frame = now + interval;
            }

            let capture_started = Instant::now();
            let frame = capture.frame()?;
            let pending = PendingFrame {
                frame,
                capture_elapsed: capture_started.elapsed(),
                captured_at: Instant::now(),
            };
            if !mailbox.publish(pending) {
                if !running.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire) {
                    break;
                }
                return Err(io::Error::other("remote frame sender stopped unexpectedly").into());
            }
        }
        Ok(())
    })();

    if capture_result.is_err() {
        first_stop.record(StopCause::Capture);
    } else if shutdown.load(Ordering::Acquire) {
        first_stop.record(StopCause::Graceful);
    }
    running.store(false, Ordering::Release);
    mailbox.close();
    // Interrupt a send that is blocked behind a dead or very slow peer. This
    // also wakes the input reader so held input can be released promptly.
    let _ = control.shutdown(Shutdown::Both);
    let sender_result = match sender.join() {
        Ok(result) => result,
        Err(_) => {
            first_stop.record_thread_panic();
            Err(io::Error::other("remote video sender thread panicked").into())
        }
    };

    match first_stop.cause() {
        StopCause::Capture => capture_result,
        StopCause::Sender => sender_result,
        StopCause::Input | StopCause::Graceful => Ok(()),
        StopCause::ThreadPanic => sender_result,
        StopCause::None => capture_result.and(sender_result),
    }
}

fn send_frames<W: std::io::Write>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    mut writer: SessionWriter<W>,
    running: &AtomicBool,
    jpeg_quality: u8,
) -> RemoteResult<()> {
    let mut report_started = Instant::now();
    let mut report_frames = 0_u64;
    let mut report_capture = Duration::ZERO;
    let mut report_queue = Duration::ZERO;
    let mut report_encode = Duration::ZERO;
    let mut report_write = Duration::ZERO;
    let mut sequence = 0_u64;

    while running.load(Ordering::Acquire) {
        let Some(pending) = mailbox.receive()? else {
            break;
        };
        if !running.load(Ordering::Acquire) {
            break;
        }
        report_capture += pending.capture_elapsed;
        report_queue += pending.captured_at.elapsed();

        let encode_started = Instant::now();
        let payload = encode_frame(sequence, &pending.frame, jpeg_quality)?;
        report_encode += encode_started.elapsed();
        let write_started = Instant::now();
        writer.write_message(MessageKind::Frame, &payload)?;
        writer.flush()?;
        report_write += write_started.elapsed();
        sequence = sequence.wrapping_add(1);
        report_frames += 1;

        let report_elapsed = report_started.elapsed();
        if report_elapsed >= Duration::from_secs(5) {
            eprintln!(
                "jwm-remote: sent {:.1} fps, dropped {}, latest JPEG {} KiB, capture/queue/encode/write {:.1}/{:.1}/{:.1}/{:.1} ms",
                report_frames as f64 / report_elapsed.as_secs_f64(),
                mailbox.take_dropped(),
                payload.len().div_ceil(1024),
                average_millis(report_capture, report_frames),
                average_millis(report_queue, report_frames),
                average_millis(report_encode, report_frames),
                average_millis(report_write, report_frames),
            );
            report_started = Instant::now();
            report_frames = 0;
            report_capture = Duration::ZERO;
            report_queue = Duration::ZERO;
            report_encode = Duration::ZERO;
            report_write = Duration::ZERO;
        }
    }
    Ok(())
}

fn average_millis(duration: Duration, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        duration.as_secs_f64() * 1000.0 / count as f64
    }
}

fn receive_input(
    mut reader: SessionReader<TcpStream>,
    mut injector: Option<InputInjector>,
    keyboard_enabled: bool,
    verified_keymap: Option<[u8; 32]>,
    initial_activity_timeout: Duration,
    session_idle_timeout: Duration,
    running: &AtomicBool,
    first_stop: &FirstStop,
) -> RemoteResult<()> {
    let session_result = (|| -> RemoteResult<()> {
        reader
            .get_ref()
            .set_read_timeout(Some(initial_activity_timeout))?;
        let mut awaiting_first_activity = true;
        while running.load(Ordering::Acquire) {
            let (kind, payload) = reader.read_message()?;
            if awaiting_first_activity {
                reader
                    .get_ref()
                    .set_read_timeout(Some(session_idle_timeout))?;
                awaiting_first_activity = false;
            }
            match kind {
                MessageKind::Pointer
                | MessageKind::Key
                | MessageKind::Button
                | MessageKind::ReleaseAll => {
                    let event = decode_input(kind, &payload)?;
                    if matches!(event, super::x11_input::InputEvent::Key { .. })
                        && !keyboard_enabled
                    {
                        return Err(invalid_data("keyboard input was not negotiated").into());
                    }
                    if let Some(injector) = injector.as_mut() {
                        if matches!(event, super::x11_input::InputEvent::Key { .. }) {
                            verify_injector_keymap(injector, verified_keymap)?;
                        }
                        injector.inject(event)?;
                    }
                }
                MessageKind::Close => break,
                MessageKind::Heartbeat if payload.is_empty() => {
                    if keyboard_enabled && let Some(injector) = injector.as_ref() {
                        verify_injector_keymap(injector, verified_keymap)?;
                    }
                }
                MessageKind::Heartbeat => {
                    return Err(invalid_data("client heartbeat payload must be empty").into());
                }
                MessageKind::Hello | MessageKind::HelloAck | MessageKind::Frame => {
                    return Err(invalid_data(format!("unexpected client message: {kind:?}")).into());
                }
            }
        }
        Ok(())
    })();

    let session_failed = session_result.is_err();
    first_stop.record(if session_failed {
        StopCause::Input
    } else {
        StopCause::Graceful
    });
    running.store(false, Ordering::Release);

    let release_result = match injector.as_mut() {
        Some(injector) => injector.release_all(),
        None => Ok(()),
    };
    if !session_failed && release_result.is_err() {
        first_stop.promote_graceful_to_input_error();
    }
    let outcome = match (session_result, release_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => {
            eprintln!(
                "jwm-remote: failed to release remote input after session error: {release_error}"
            );
            Err(error)
        }
    };
    outcome
}

fn verify_injector_keymap(
    injector: &InputInjector,
    expected: Option<[u8; 32]>,
) -> RemoteResult<()> {
    if !injector.take_keymap_change()? {
        return Ok(());
    }
    let expected = expected.ok_or_else(|| invalid_data("keyboard input has no verified keymap"))?;
    if injector.keymap_fingerprint()? != expected {
        return Err(
            invalid_data("host X11 keymap changed; reconnect to re-verify keyboard input").into(),
        );
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn permission_denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::frame::decode_frame;
    use image::{Rgb, RgbImage};
    use std::io::{Cursor, Write};
    use std::net::TcpListener;

    #[derive(Default)]
    struct GateState {
        open: bool,
        write_calls: usize,
        flushes: usize,
        bytes: Vec<u8>,
    }

    #[derive(Clone)]
    struct GateWriter(Arc<(Mutex<GateState>, Condvar)>);

    impl Write for GateWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let (lock, ready) = &*self.0;
            let mut state = lock.lock().unwrap();
            state.write_calls += 1;
            ready.notify_all();
            while !state.open {
                state = ready.wait(state).unwrap();
            }
            state.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            let (lock, ready) = &*self.0;
            let mut state = lock.lock().unwrap();
            state.flushes += 1;
            ready.notify_all();
            Ok(())
        }
    }

    fn test_pending_frame(red: u8) -> PendingFrame {
        PendingFrame {
            frame: CapturedFrame {
                image: RgbImage::from_pixel(2, 2, Rgb([red, 0, 0])),
                source_width: 2,
                source_height: 2,
            },
            capture_elapsed: Duration::from_millis(1),
            captured_at: Instant::now(),
        }
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connector = thread::spawn(move || TcpStream::connect(address).unwrap());
        let (accepted, _) = listener.accept().unwrap();
        (connector.join().unwrap(), accepted)
    }

    #[test]
    fn defaults_must_explicitly_opt_into_lan_and_input() {
        let options = HostOptions {
            listen: "127.0.0.1:48221".into(),
            key_file: PathBuf::from("key"),
            display: None,
            fps: 12,
            jpeg_quality: 70,
            max_width: 1280,
            capture_source: CaptureSource::Auto,
            allow_lan: false,
            allow_input: false,
            once: false,
        };
        assert!(!options.allow_lan);
        assert!(!options.allow_input);
    }

    #[test]
    fn latest_mailbox_replaces_stale_items_without_blocking() {
        let mailbox = LatestMailbox::new();
        assert!(mailbox.publish(1_u64));
        assert!(mailbox.publish(2_u64));
        assert_eq!(mailbox.take_dropped(), 1);
        assert_eq!(mailbox.receive().unwrap(), Some(2));
        assert_eq!(mailbox.take_dropped(), 0);
    }

    #[test]
    fn closing_mailbox_discards_pending_and_wakes_receiver() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert!(mailbox.publish(7_u64));
        mailbox.close();
        assert_eq!(mailbox.receive().unwrap(), None);
        assert!(!mailbox.publish(8));

        let waiting_mailbox = Arc::new(LatestMailbox::<u64>::new());
        let waiter_mailbox = Arc::clone(&waiting_mailbox);
        let waiter = thread::spawn(move || waiter_mailbox.receive().unwrap());
        waiting_mailbox.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn session_stop_guard_clears_running_on_every_exit_path() {
        let running = Arc::new(AtomicBool::new(true));
        drop(StopSessionOnDrop(Arc::clone(&running)));
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn first_stop_cause_is_never_overwritten_by_cleanup_failures() {
        let first_stop = FirstStop::new();
        first_stop.record(StopCause::Sender);
        first_stop.record(StopCause::Input);
        first_stop.record(StopCause::Capture);
        first_stop.promote_graceful_to_input_error();
        assert_eq!(first_stop.cause(), StopCause::Sender);

        let graceful_cleanup = FirstStop::new();
        graceful_cleanup.record(StopCause::Graceful);
        graceful_cleanup.promote_graceful_to_input_error();
        assert_eq!(graceful_cleanup.cause(), StopCause::Input);

        first_stop.record_thread_panic();
        assert_eq!(first_stop.cause(), StopCause::ThreadPanic);
    }

    #[test]
    fn dropped_frames_do_not_create_wire_sequence_gaps() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert!(mailbox.publish(test_pending_frame(10)));
        let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        let running = Arc::new(AtomicBool::new(true));
        let sender_mailbox = Arc::clone(&mailbox);
        let sender_running = Arc::clone(&running);
        let writer = SessionWriter::new(GateWriter(Arc::clone(&gate)), [0x5a; 32]);
        let sender =
            thread::spawn(move || send_frames(sender_mailbox, writer, &sender_running, 100));

        let (lock, ready) = &*gate;
        let state = lock.lock().unwrap();
        let (state, timeout) = ready
            .wait_timeout_while(state, Duration::from_secs(1), |state| {
                state.write_calls == 0
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "sender never started its first record"
        );
        drop(state);

        assert!(mailbox.publish(test_pending_frame(20)));
        assert!(mailbox.publish(test_pending_frame(30)));
        assert!(mailbox.publish(test_pending_frame(40)));
        assert_eq!(mailbox.take_dropped(), 2);

        let mut state = lock.lock().unwrap();
        state.open = true;
        ready.notify_all();
        let (state, timeout) = ready
            .wait_timeout_while(state, Duration::from_secs(2), |state| state.flushes < 2)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "sender did not flush both selected frames"
        );
        let wire = state.bytes.clone();
        drop(state);

        running.store(false, Ordering::Release);
        mailbox.close();
        sender.join().unwrap().unwrap();

        let mut reader = SessionReader::new(Cursor::new(wire), [0x5a; 32]);
        let (first_kind, first_payload) = reader.read_message().unwrap();
        let (second_kind, second_payload) = reader.read_message().unwrap();
        assert_eq!(first_kind, MessageKind::Frame);
        assert_eq!(second_kind, MessageKind::Frame);
        assert_eq!(decode_frame(&first_payload).unwrap().sequence, 0);
        assert_eq!(decode_frame(&second_payload).unwrap().sequence, 1);
    }

    #[test]
    fn first_activity_gets_a_grace_period_then_switches_to_steady_idle() {
        let (client_stream, server_stream) = loopback_pair();
        let key = [0x47; 32];
        let reader = SessionReader::new(server_stream, key);
        let mut writer = SessionWriter::new(client_stream, key);
        let running = Arc::new(AtomicBool::new(true));
        let receiver_running = Arc::clone(&running);
        let first_stop = Arc::new(FirstStop::new());
        let receiver_stop = Arc::clone(&first_stop);
        let receiver = thread::spawn(move || {
            receive_input(
                reader,
                None,
                false,
                None,
                Duration::from_millis(600),
                Duration::from_millis(60),
                &receiver_running,
                &receiver_stop,
            )
        });

        thread::sleep(Duration::from_millis(120));
        assert!(running.load(Ordering::Acquire));
        writer.write_message(MessageKind::Heartbeat, &[]).unwrap();
        writer.flush().unwrap();
        let steady_started = Instant::now();
        let result = receiver.join().unwrap();
        assert!(result.is_err());
        assert!(steady_started.elapsed() < Duration::from_millis(400));
        assert!(!running.load(Ordering::Acquire));
        assert_eq!(first_stop.cause(), StopCause::Input);
    }
}
