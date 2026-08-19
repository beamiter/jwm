//! Remote desktop viewer/client orchestration.

use super::deadline::TcpStreamDeadline;
use super::frame::{
    RecyclableDecodedFrame, SharedDecodeBufferPool, decode_frame_recyclable, new_decode_buffer_pool,
};
use super::key::load_key_file;
use super::messages::{
    ClientHello, MAX_INPUT_BATCH_EVENTS, ServerHello, encode_frame_ack, encode_input_batch_into,
};
use super::protocol::{
    MessageKind, PayloadBufferRetention, SessionReader, SessionWriter, client_handshake,
};
use super::x11_input::InputEvent;
use super::x11_keymap::fingerprint_display;
use super::x11_viewer::{Viewer, ViewerEvent};
use super::{RemoteResult, VIDEO_FRAME_IDLE_TIMEOUT};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::io::Write as _;
use std::io::{self, Read as _};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);
const RECEIVER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub address: String,
    pub key_file: PathBuf,
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
}

pub fn run_client(options: ClientOptions) -> RemoteResult<()> {
    let key = load_key_file(&options.key_file)?;
    let request_input = !options.view_only;
    let keymap_fingerprint = if request_input {
        fingerprint_display(options.display.as_deref())?
    } else {
        [0; 32]
    };
    let stream = TcpStream::connect(&options.address)?;
    let (mut reader, mut writer, hello) = negotiate_session(
        stream,
        &key,
        request_input,
        keymap_fingerprint,
        HANDSHAKE_TIMEOUT,
    )?;
    if request_input && !hello.pointer_enabled {
        eprintln!("jwm-remote: host accepted a view-only session; input will not be forwarded");
    } else if request_input && !hello.keyboard_enabled {
        eprintln!("jwm-remote: X11 keymaps differ; pointer works but keyboard is disabled");
    }

    let state = Arc::new(ReceiveState::new());
    let mut telemetry_reporter = ClientTelemetryReporter::new(&state.telemetry);
    let (kind, payload) = reader.read_message()?;
    if kind != MessageKind::Frame {
        return Err(invalid_data("host did not send an initial video frame").into());
    }
    state.telemetry.record_received();
    // SessionReader exposes this payload only after its record MAC succeeds;
    // no decoded-image allocation is checked out before authentication.
    let first_frame = decode_queued_frame(&payload, &state.telemetry, &state.decode_pool)?;
    reader
        .get_ref()
        .set_read_timeout(Some(VIDEO_FRAME_IDLE_TIMEOUT))?;
    let initial_width = u16::try_from(first_frame.frame.image().width())
        .map_err(|_| invalid_data("initial frame is wider than an X11 window"))?;
    let initial_height = u16::try_from(first_frame.frame.image().height())
        .map_err(|_| invalid_data("initial frame is taller than an X11 window"))?;
    let last_sequence = first_frame.frame.sequence();
    let mut viewer = Viewer::connect(
        options.display.as_deref(),
        initial_width,
        initial_height,
        hello.pointer_enabled || hello.keyboard_enabled,
        hello.keyboard_enabled,
        hello.keyboard_enabled.then_some(keymap_fingerprint),
        options.grab_input && (hello.pointer_enabled || hello.keyboard_enabled),
    )?;
    draw_and_ack(&mut writer, first_frame, &state.telemetry, |frame| {
        viewer.draw_recyclable(frame)
    })?;

    let control = writer.get_ref().try_clone()?;
    let mut cancel = SessionCancel::new(control, Arc::clone(&state));
    let (mut wake_receiver, wake_sender) = wake_pair()?;
    let receive_state = Arc::clone(&state);
    let receiver = ReceiverThread::spawn(move || {
        // Move the authenticated first-frame allocation into the receiver
        // thread. It remains thread-local while avoiding a second large
        // allocation for the next frame.
        receive_frames(reader, receive_state, last_sequence, payload, wake_sender);
    })?;

    let pointer_enabled = hello.pointer_enabled && !options.view_only;
    let keyboard_enabled = hello.keyboard_enabled && !options.view_only;
    let loop_result = viewer_loop(
        &mut viewer,
        &mut writer,
        &state,
        pointer_enabled,
        keyboard_enabled,
        &mut telemetry_reporter,
        &mut wake_receiver,
    );
    let graceful_close = matches!(
        &loop_result,
        Ok(ViewerLoopExit {
            close_release_flushed: true
        })
    );
    let cancel_result = cancel.cancel(&mut writer, graceful_close);
    let receiver_exit = receiver.join_timeout(RECEIVER_JOIN_TIMEOUT);
    telemetry_reporter.force_report();
    // Preserve the foreground/X11 first cause. Receiver cleanup failures only
    // replace a successful viewer-close result, matching the old precedence.
    loop_result?;
    if receiver_exit == ReceiverExit::Panicked {
        return Err(io::Error::other("remote video thread panicked").into());
    }
    if receiver_exit == ReceiverExit::TimedOut {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "remote video thread did not stop after session cancellation",
        )
        .into());
    }
    cancel_result
}

fn negotiate_session(
    mut stream: TcpStream,
    key: &[u8],
    request_input: bool,
    keymap_fingerprint: [u8; 32],
    negotiation_timeout: Duration,
) -> RemoteResult<(
    SessionReader<TcpStream>,
    SessionWriter<TcpStream>,
    ServerHello,
)> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(negotiation_timeout))?;
    stream.set_write_timeout(Some(negotiation_timeout))?;
    // Per-call socket timeouts can be extended indefinitely by a peer that
    // drips bytes.  Bound authentication and the versioned HelloAck together.
    let mut negotiation_deadline = TcpStreamDeadline::arm(&stream, negotiation_timeout)?;
    let session_keys = client_handshake(&mut stream, key, rand::random())?;

    let reader_stream = stream.try_clone()?;
    let (receive_key, send_key) = session_keys.into_client();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    let mut writer = SessionWriter::new(stream, send_key);
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            request_input,
            keymap_fingerprint,
        }
        .encode(),
    )?;
    writer.flush()?;
    let (kind, payload) = reader.read_message()?;
    if kind != MessageKind::HelloAck {
        return Err(invalid_data("host did not acknowledge remote session negotiation").into());
    }
    let hello = ServerHello::decode(&payload)?;
    negotiation_deadline.cancel();
    reader
        .get_ref()
        .set_read_timeout(Some(FIRST_FRAME_TIMEOUT))?;
    writer.get_ref().set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok((reader, writer, hello))
}

fn viewer_loop(
    viewer: &mut Viewer,
    writer: &mut SessionWriter<TcpStream>,
    state: &ReceiveState,
    pointer_enabled: bool,
    keyboard_enabled: bool,
    telemetry_reporter: &mut ClientTelemetryReporter<'_>,
    wake: &mut WakeReceiver,
) -> RemoteResult<ViewerLoopExit> {
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut input_batch = Vec::with_capacity(MAX_INPUT_BATCH_EVENTS);
    let mut input_payload = Vec::new();
    loop {
        wake.drain()?;
        let events = viewer.poll_events()?;
        let outcome = for_each_input_batch(
            events,
            pointer_enabled,
            keyboard_enabled,
            &mut input_batch,
            |batch| write_input_batch(writer, batch, &mut input_payload),
        )?;
        let mut wrote = outcome.wrote_input;
        if Instant::now() >= next_heartbeat {
            writer.write_message(MessageKind::Heartbeat, &[])?;
            wrote = true;
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }
        if wrote {
            writer.flush()?;
        }
        if outcome.close {
            let input_enabled = pointer_enabled || keyboard_enabled;
            if input_enabled && !outcome.wrote_release_all {
                return Err(invalid_data(
                    "viewer closed without flushing the required ReleaseAll edge",
                )
                .into());
            }
            return Ok(ViewerLoopExit {
                close_release_flushed: true,
            });
        }

        if let Some(frame) = take_latest(state) {
            draw_and_ack(writer, frame, &state.telemetry, |frame| {
                viewer.draw_recyclable(frame)
            })?;
        }

        if !state.alive.load(Ordering::Acquire) && state.latest.lock().unwrap().is_none() {
            let message = state
                .error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "host closed the remote session".to_string());
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, message).into());
        }
        telemetry_reporter.maybe_report();
        viewer.prepare_wait()?;
        if viewer.has_pending_events()? {
            continue;
        }
        let mut deadline = next_heartbeat.min(telemetry_reporter.next_deadline());
        if let Some(viewer_deadline) = viewer.next_event_deadline() {
            deadline = deadline.min(viewer_deadline);
        }
        if wait_for_activity(viewer.connection_fd(), wake.as_fd(), deadline)?
            == WaitOutcome::X11Hangup
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "local X11 viewer connection closed",
            )
            .into());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewerLoopExit {
    close_release_flushed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InputPollOutcome {
    wrote_input: bool,
    wrote_release_all: bool,
    close: bool,
}

fn for_each_input_batch(
    events: impl IntoIterator<Item = ViewerEvent>,
    pointer_enabled: bool,
    keyboard_enabled: bool,
    batch: &mut Vec<InputEvent>,
    mut emit: impl FnMut(&[InputEvent]) -> RemoteResult<()>,
) -> RemoteResult<InputPollOutcome> {
    batch.clear();
    let mut outcome = InputPollOutcome::default();
    let mut pointer_run_open = false;

    for event in events {
        if event == ViewerEvent::Close {
            outcome.close = true;
            break;
        }
        let input = match event {
            ViewerEvent::Pointer { x, y } if pointer_enabled => InputEvent::Pointer { x, y },
            ViewerEvent::Key { keycode, pressed } if keyboard_enabled => {
                pointer_run_open = false;
                InputEvent::Key { keycode, pressed }
            }
            ViewerEvent::Button { button, pressed } if pointer_enabled => {
                pointer_run_open = false;
                InputEvent::Button { button, pressed }
            }
            ViewerEvent::ReleaseAll if pointer_enabled || keyboard_enabled => {
                pointer_run_open = false;
                InputEvent::ReleaseAll
            }
            ViewerEvent::Key { .. } | ViewerEvent::Button { .. } | ViewerEvent::ReleaseAll => {
                // Capability filtering must not accidentally merge pointer
                // positions that were separated by a local input edge.
                pointer_run_open = false;
                continue;
            }
            ViewerEvent::Pointer { .. } => continue,
            ViewerEvent::Close => unreachable!(),
        };

        if matches!(input, InputEvent::Pointer { .. })
            && pointer_run_open
            && let Some(previous @ InputEvent::Pointer { .. }) = batch.last_mut()
        {
            *previous = input;
            continue;
        }
        if batch.len() == MAX_INPUT_BATCH_EVENTS {
            emit(batch)?;
            outcome.wrote_input = true;
            outcome.wrote_release_all |= batch.contains(&InputEvent::ReleaseAll);
            batch.clear();
        }
        batch.push(input);
        pointer_run_open = matches!(input, InputEvent::Pointer { .. });
    }

    if !batch.is_empty() {
        emit(batch)?;
        outcome.wrote_input = true;
        outcome.wrote_release_all |= batch.contains(&InputEvent::ReleaseAll);
        batch.clear();
    }
    Ok(outcome)
}

fn write_input_batch<W: std::io::Write>(
    writer: &mut SessionWriter<W>,
    events: &[InputEvent],
    payload: &mut Vec<u8>,
) -> RemoteResult<()> {
    encode_input_batch_into(events, payload)?;
    writer.write_message(MessageKind::InputBatch, payload)?;
    Ok(())
}

fn draw_and_ack<W: std::io::Write>(
    writer: &mut SessionWriter<W>,
    queued: QueuedFrame,
    telemetry: &ClientTelemetry,
    draw: impl FnOnce(RecyclableDecodedFrame) -> RemoteResult<bool>,
) -> RemoteResult<()> {
    let sequence = queued.frame.sequence();
    let draw_started = Instant::now();
    let draw_result = draw(queued.frame);
    let draw_finished = Instant::now();
    if !draw_result? {
        return Ok(());
    }
    telemetry.record_drawn_at(queued.ready_at, draw_started, draw_finished);
    writer.write_message(MessageKind::FrameAck, &encode_frame_ack(sequence))?;
    writer.flush()?;
    telemetry.record_acked();
    Ok(())
}

struct QueuedFrame {
    frame: RecyclableDecodedFrame,
    ready_at: Instant,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct ClientTelemetryWindow {
    received: u64,
    decoded: u64,
    replaced: u64,
    drawn: u64,
    acked: u64,
    decode_elapsed: Duration,
    queue_elapsed: Duration,
    draw_elapsed: Duration,
}

impl ClientTelemetryWindow {
    fn is_empty(self) -> bool {
        self.received == 0
            && self.decoded == 0
            && self.replaced == 0
            && self.drawn == 0
            && self.acked == 0
    }
}

#[derive(Default)]
struct ClientTelemetry {
    window: Mutex<ClientTelemetryWindow>,
}

impl ClientTelemetry {
    fn record_received(&self) {
        let mut window = self.lock_window();
        window.received = window.received.saturating_add(1);
    }

    fn record_decoded_at(&self, decode_started: Instant, ready_at: Instant) {
        let mut window = self.lock_window();
        window.decoded = window.decoded.saturating_add(1);
        window.decode_elapsed = window
            .decode_elapsed
            .saturating_add(ready_at.saturating_duration_since(decode_started));
    }

    fn record_replaced(&self) {
        let mut window = self.lock_window();
        window.replaced = window.replaced.saturating_add(1);
    }

    fn record_drawn_at(&self, ready_at: Instant, draw_started: Instant, draw_finished: Instant) {
        let mut window = self.lock_window();
        window.drawn = window.drawn.saturating_add(1);
        window.queue_elapsed = window
            .queue_elapsed
            .saturating_add(draw_started.saturating_duration_since(ready_at));
        window.draw_elapsed = window
            .draw_elapsed
            .saturating_add(draw_finished.saturating_duration_since(draw_started));
    }

    fn record_acked(&self) {
        let mut window = self.lock_window();
        window.acked = window.acked.saturating_add(1);
    }

    fn take_window(&self) -> ClientTelemetryWindow {
        std::mem::take(&mut *self.lock_window())
    }

    fn lock_window(&self) -> std::sync::MutexGuard<'_, ClientTelemetryWindow> {
        match self.window.lock() {
            Ok(window) => window,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

struct ClientTelemetryReport {
    window: ClientTelemetryWindow,
    elapsed: Duration,
}

impl ClientTelemetryReport {
    fn emit(self, final_report: bool) {
        let label = if final_report {
            "viewer final"
        } else {
            "viewer"
        };
        let _ = writeln!(
            io::stderr().lock(),
            "jwm-remote: {label} received {:.1} fps ({}), decoded {}, replaced {}, drawn {}, acked {}, decode/queue/draw {:.1}/{:.1}/{:.1} ms",
            per_second(self.window.received, self.elapsed),
            self.window.received,
            self.window.decoded,
            self.window.replaced,
            self.window.drawn,
            self.window.acked,
            average_millis(self.window.decode_elapsed, self.window.decoded),
            average_millis(self.window.queue_elapsed, self.window.drawn),
            average_millis(self.window.draw_elapsed, self.window.drawn),
        );
    }
}

struct ClientTelemetryReporter<'a> {
    telemetry: &'a ClientTelemetry,
    window_started: Instant,
    finished: bool,
}

impl<'a> ClientTelemetryReporter<'a> {
    fn new(telemetry: &'a ClientTelemetry) -> Self {
        Self::new_at(telemetry, Instant::now())
    }

    fn new_at(telemetry: &'a ClientTelemetry, window_started: Instant) -> Self {
        Self {
            telemetry,
            window_started,
            finished: false,
        }
    }

    fn maybe_report(&mut self) {
        if self.finished {
            return;
        }
        if let Some(report) = self.maybe_report_at(Instant::now(), false) {
            report.emit(false);
        }
    }

    fn force_report(&mut self) {
        if self.finished {
            return;
        }
        if let Some(report) = self.maybe_report_at(Instant::now(), true) {
            report.emit(true);
        }
        self.finished = true;
    }

    fn next_deadline(&self) -> Instant {
        self.window_started + TELEMETRY_INTERVAL
    }

    fn maybe_report_at(&mut self, now: Instant, force: bool) -> Option<ClientTelemetryReport> {
        let elapsed = now.saturating_duration_since(self.window_started);
        if !force && elapsed < TELEMETRY_INTERVAL {
            return None;
        }
        self.window_started = now;
        let window = self.telemetry.take_window();
        (!window.is_empty()).then_some(ClientTelemetryReport { window, elapsed })
    }
}

impl Drop for ClientTelemetryReporter<'_> {
    fn drop(&mut self) {
        // Reporting is diagnostic only: early startup/session errors still
        // flush a partial window, while an empty/already-finished window is a
        // no-op and can never change the session result.
        self.force_report();
    }
}

struct WakeSender {
    stream: UnixStream,
}

impl WakeSender {
    fn signal(&self) -> io::Result<()> {
        loop {
            match (&self.stream).write(&[1]) {
                Ok(1) => return Ok(()),
                Ok(_) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // A full wake socket already represents a pending wakeup.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

struct WakeReceiver {
    stream: UnixStream,
}

impl WakeReceiver {
    fn drain(&mut self) -> io::Result<()> {
        let mut buffer = [0_u8; 64];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }

    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

fn wake_pair() -> io::Result<(WakeReceiver, WakeSender)> {
    let (receiver, sender) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((
        WakeReceiver { stream: receiver },
        WakeSender { stream: sender },
    ))
}

fn wait_for_activity(
    x11_fd: BorrowedFd<'_>,
    wake_fd: BorrowedFd<'_>,
    deadline: Instant,
) -> io::Result<WaitOutcome> {
    loop {
        let interests = PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP;
        let mut descriptors = [
            PollFd::new(x11_fd, interests),
            PollFd::new(wake_fd, interests),
        ];
        match poll(
            &mut descriptors,
            poll_timeout_until(Instant::now(), deadline),
        ) {
            Ok(_) => {
                if descriptors.iter().any(|descriptor| {
                    descriptor
                        .revents()
                        .is_some_and(|events| events.contains(PollFlags::POLLNVAL))
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote viewer poll received an invalid file descriptor",
                    ));
                }
                if descriptors[0].revents().is_some_and(|events| {
                    events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP)
                }) {
                    return Ok(WaitOutcome::X11Hangup);
                }
                return Ok(WaitOutcome::Activity);
            }
            Err(Errno::EINTR) => continue,
            Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitOutcome {
    Activity,
    X11Hangup,
}

fn poll_timeout_until(now: Instant, deadline: Instant) -> PollTimeout {
    let remaining = deadline.saturating_duration_since(now);
    if remaining.is_zero() {
        return PollTimeout::ZERO;
    }
    // poll(2) takes whole milliseconds. Round up so a sub-millisecond
    // remainder cannot turn into an immediate busy loop.
    let millis = remaining.as_nanos().div_ceil(1_000_000);
    PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX)
}

struct SessionCancel {
    control: TcpStream,
    state: Arc<ReceiveState>,
    cancelled: bool,
    close_attempted: bool,
    shutdown: bool,
}

impl SessionCancel {
    fn new(control: TcpStream, state: Arc<ReceiveState>) -> Self {
        Self {
            control,
            state,
            cancelled: false,
            close_attempted: false,
            shutdown: false,
        }
    }

    fn cancel(
        &mut self,
        writer: &mut SessionWriter<TcpStream>,
        graceful_close: bool,
    ) -> RemoteResult<()> {
        if self.cancelled {
            return Ok(());
        }
        self.cancelled = true;
        self.state.stopping.store(true, Ordering::Release);

        let mut first_error = None;
        if graceful_close && !self.close_attempted {
            self.close_attempted = true;
            let close_result: RemoteResult<()> = (|| {
                writer.write_message(MessageKind::Close, &[])?;
                writer.flush()?;
                Ok(())
            })();
            if let Err(error) = close_result {
                first_error = Some(error);
            }
        }
        if let Err(error) = self.shutdown_once()
            && first_error.is_none()
        {
            first_error = Some(error.into());
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn shutdown_once(&mut self) -> io::Result<()> {
        if self.shutdown {
            return Ok(());
        }
        self.shutdown = true;
        match self.control.shutdown(Shutdown::Both) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for SessionCancel {
    fn drop(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        let _ = self.shutdown_once();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiverExit {
    Completed,
    Panicked,
    TimedOut,
}

struct ReceiverThread {
    handle: Option<thread::JoinHandle<()>>,
    done: mpsc::Receiver<()>,
}

impl ReceiverThread {
    fn spawn(task: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        let (done_tx, done) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("jwm-remote-video".into())
            .spawn(move || {
                let _done = ThreadDone(Some(done_tx));
                task();
            })?;
        Ok(Self {
            handle: Some(handle),
            done,
        })
    }

    fn join_timeout(mut self, timeout: Duration) -> ReceiverExit {
        match self.done.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if self
                    .handle
                    .take()
                    .is_some_and(|handle| handle.join().is_err())
                {
                    ReceiverExit::Panicked
                } else {
                    ReceiverExit::Completed
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Dropping JoinHandle detaches the thread; the session socket
                // was already shut down, so a well-behaved receiver exits.
                self.handle.take();
                ReceiverExit::TimedOut
            }
        }
    }
}

struct ThreadDone(Option<mpsc::Sender<()>>);

impl Drop for ThreadDone {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn per_second(count: u64, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        0.0
    } else {
        count as f64 / elapsed.as_secs_f64()
    }
}

fn average_millis(duration: Duration, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        duration.as_secs_f64() * 1000.0 / count as f64
    }
}

struct ReceiveState {
    latest: Mutex<Option<QueuedFrame>>,
    error: Mutex<Option<String>>,
    alive: AtomicBool,
    stopping: AtomicBool,
    telemetry: ClientTelemetry,
    decode_pool: SharedDecodeBufferPool,
}

impl ReceiveState {
    fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            error: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
            telemetry: ClientTelemetry::default(),
            decode_pool: new_decode_buffer_pool(),
        }
    }
}

fn take_latest(state: &ReceiveState) -> Option<QueuedFrame> {
    state.latest.lock().unwrap().take()
}

fn replace_latest<T>(latest: &Mutex<Option<T>>, next: T) -> Option<T> {
    let mut latest = latest.lock().unwrap();
    latest.replace(next)
}

fn decode_queued_frame(
    payload: &[u8],
    telemetry: &ClientTelemetry,
    pool: &SharedDecodeBufferPool,
) -> RemoteResult<QueuedFrame> {
    let decode_started = Instant::now();
    let frame = decode_frame_recyclable(payload, Arc::clone(pool))?;
    let ready_at = Instant::now();
    telemetry.record_decoded_at(decode_started, ready_at);
    Ok(QueuedFrame { frame, ready_at })
}

struct ReceiveCompletion {
    state: Arc<ReceiveState>,
    wake: WakeSender,
    finished: bool,
}

impl ReceiveCompletion {
    fn new(state: Arc<ReceiveState>, wake: WakeSender) -> Self {
        Self {
            state,
            wake,
            finished: false,
        }
    }

    fn signal(&self) -> io::Result<()> {
        self.wake.signal()
    }

    fn finish(&mut self, result: RemoteResult<()>) {
        let error = result.err().map(|error| error.to_string());
        self.publish_terminal(error);
    }

    fn publish_terminal(&mut self, error: Option<String>) {
        if self.finished {
            return;
        }
        // Mark first so even a poisoned diagnostic lock cannot recurse into
        // terminal publication while unwinding.
        self.finished = true;
        if let Some(error) = error
            && !self.state.stopping.load(Ordering::Acquire)
        {
            let mut slot = match self.state.error.lock() {
                Ok(slot) => slot,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = Some(error);
        }
        self.state.alive.store(false, Ordering::Release);
        let _ = self.wake.signal();
    }
}

impl Drop for ReceiveCompletion {
    fn drop(&mut self) {
        if !self.finished {
            self.publish_terminal(Some("remote video thread panicked".to_string()));
        }
    }
}

fn receive_frames(
    mut reader: SessionReader<TcpStream>,
    state: Arc<ReceiveState>,
    mut last_sequence: u64,
    mut payload: Vec<u8>,
    wake: WakeSender,
) {
    let mut completion = ReceiveCompletion::new(Arc::clone(&state), wake);
    let mut payload_retention = PayloadBufferRetention::default();
    let result: RemoteResult<()> = (|| loop {
        let kind = reader.read_message_into(&mut payload)?;
        match kind {
            MessageKind::Frame => {
                // read_message_into authenticated the complete record before
                // returning Frame, so pool checkout starts only on trusted
                // header/JPEG bytes.
                state.telemetry.record_received();
                let queued = decode_queued_frame(&payload, &state.telemetry, &state.decode_pool)?;
                let expected = last_sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("video frame sequence exhausted"))?;
                if queued.frame.sequence() != expected {
                    return Err(invalid_data(format!(
                        "video frame sequence mismatch: expected {expected}, got {}",
                        queued.frame.sequence()
                    ))
                    .into());
                }
                last_sequence = queued.frame.sequence();
                let replaced = replace_latest(&state.latest, queued);
                if replaced.is_some() {
                    state.telemetry.record_replaced();
                }
                completion.signal()?;
                // A decoded RGB frame can own a large allocation. The latest
                // slot is unlocked before the stale frame reaches Drop.
                drop(replaced);
            }
            MessageKind::Close => break Ok(()),
            MessageKind::FrameAck => {
                return Err(invalid_data("host sent an unexpected frame acknowledgement").into());
            }
            MessageKind::Hello
            | MessageKind::HelloAck
            | MessageKind::Pointer
            | MessageKind::Key
            | MessageKind::Button
            | MessageKind::ReleaseAll
            | MessageKind::InputBatch => {
                return Err(invalid_data(format!("unexpected host message: {kind:?}")).into());
            }
            MessageKind::Heartbeat => {
                return Err(invalid_data("host sent an unexpected heartbeat").into());
            }
        }
        payload_retention.observe(&mut payload);
    })();

    completion.finish(result);
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::frame::{DecodedFrame, decode_pool_snapshot, encode_frame};
    use crate::remote::messages::decode_frame_ack;
    use crate::remote::protocol::{PSK_LEN, server_handshake};
    use crate::remote::x11_capture::CapturedFrame;
    use image::Rgb;
    use image::RgbImage;
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;

    const TEST_PSK: [u8; PSK_LEN] = [0x5a; PSK_LEN];

    fn decoded_test_frame(sequence: u64) -> RecyclableDecodedFrame {
        decoded_test_frame_in_pool(sequence, new_decode_buffer_pool())
    }

    fn decoded_test_frame_in_pool(
        sequence: u64,
        pool: SharedDecodeBufferPool,
    ) -> RecyclableDecodedFrame {
        RecyclableDecodedFrame::from_decoded(
            DecodedFrame {
                sequence,
                source_width: 1,
                source_height: 1,
                image: RgbImage::new(1, 1),
            },
            pool,
        )
    }

    fn queued_test_frame(sequence: u64) -> QueuedFrame {
        QueuedFrame {
            frame: decoded_test_frame(sequence),
            ready_at: Instant::now(),
        }
    }

    #[derive(Default)]
    struct FlushFailWriter(Vec<u8>);

    impl Write for FlushFailWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic flush failure"))
        }
    }

    fn pointer(position: u16) -> ViewerEvent {
        ViewerEvent::Pointer {
            x: position,
            y: position + 1,
        }
    }

    fn input_pointer(position: u16) -> InputEvent {
        InputEvent::Pointer {
            x: position,
            y: position + 1,
        }
    }

    fn collect_input_batches(
        events: impl IntoIterator<Item = ViewerEvent>,
        pointer_enabled: bool,
        keyboard_enabled: bool,
    ) -> (Vec<Vec<InputEvent>>, InputPollOutcome) {
        let mut scratch = Vec::with_capacity(MAX_INPUT_BATCH_EVENTS);
        let mut batches = Vec::new();
        let outcome = for_each_input_batch(
            events,
            pointer_enabled,
            keyboard_enabled,
            &mut scratch,
            |batch| {
                assert!(!batch.is_empty());
                assert!(batch.len() <= MAX_INPUT_BATCH_EVENTS);
                batches.push(batch.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert!(scratch.is_empty());
        (batches, outcome)
    }

    #[test]
    fn drag_motion_coalesces_only_between_button_edges() {
        let (batches, outcome) = collect_input_batches(
            [
                pointer(1),
                pointer(2),
                ViewerEvent::Button {
                    button: 1,
                    pressed: true,
                },
                pointer(3),
                pointer(4),
                ViewerEvent::Button {
                    button: 1,
                    pressed: false,
                },
            ],
            true,
            false,
        );

        assert_eq!(
            batches,
            [vec![
                input_pointer(2),
                InputEvent::Button {
                    button: 1,
                    pressed: true,
                },
                input_pointer(4),
                InputEvent::Button {
                    button: 1,
                    pressed: false,
                },
            ]]
        );
        assert_eq!(
            outcome,
            InputPollOutcome {
                wrote_input: true,
                wrote_release_all: false,
                close: false,
            }
        );
    }

    #[test]
    fn scroll_button_edges_keep_each_pointer_position() {
        let (batches, _) = collect_input_batches(
            [
                pointer(10),
                ViewerEvent::Button {
                    button: 4,
                    pressed: true,
                },
                pointer(11),
                ViewerEvent::Button {
                    button: 4,
                    pressed: false,
                },
                pointer(12),
                ViewerEvent::Button {
                    button: 5,
                    pressed: true,
                },
                pointer(13),
                ViewerEvent::Button {
                    button: 5,
                    pressed: false,
                },
            ],
            true,
            false,
        );

        assert_eq!(
            batches,
            [vec![
                input_pointer(10),
                InputEvent::Button {
                    button: 4,
                    pressed: true,
                },
                input_pointer(11),
                InputEvent::Button {
                    button: 4,
                    pressed: false,
                },
                input_pointer(12),
                InputEvent::Button {
                    button: 5,
                    pressed: true,
                },
                input_pointer(13),
                InputEvent::Button {
                    button: 5,
                    pressed: false,
                },
            ]]
        );
    }

    #[test]
    fn f12_release_all_partitions_pointer_runs() {
        let (batches, _) = collect_input_batches(
            [
                ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                pointer(20),
                pointer(21),
                // The viewer keeps F12 local and represents it upstream as
                // this ReleaseAll edge.
                ViewerEvent::ReleaseAll,
                pointer(22),
                pointer(23),
            ],
            true,
            true,
        );

        assert_eq!(
            batches,
            [vec![
                InputEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                input_pointer(21),
                InputEvent::ReleaseAll,
                input_pointer(23),
            ]]
        );
    }

    #[test]
    fn all_forwarded_edges_partition_pointer_runs_in_order() {
        let (batches, _) = collect_input_batches(
            [
                pointer(30),
                pointer(31),
                ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                pointer(32),
                pointer(33),
                ViewerEvent::Key {
                    keycode: 38,
                    pressed: false,
                },
                pointer(34),
                pointer(35),
                ViewerEvent::Button {
                    button: 1,
                    pressed: true,
                },
                pointer(36),
                pointer(37),
                ViewerEvent::Button {
                    button: 1,
                    pressed: false,
                },
                pointer(38),
                pointer(39),
                ViewerEvent::ReleaseAll,
                pointer(40),
                pointer(41),
            ],
            true,
            true,
        );

        assert_eq!(
            batches.concat(),
            vec![
                input_pointer(31),
                InputEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                input_pointer(33),
                InputEvent::Key {
                    keycode: 38,
                    pressed: false,
                },
                input_pointer(35),
                InputEvent::Button {
                    button: 1,
                    pressed: true,
                },
                input_pointer(37),
                InputEvent::Button {
                    button: 1,
                    pressed: false,
                },
                input_pointer(39),
                InputEvent::ReleaseAll,
                input_pointer(41),
            ]
        );
    }

    #[test]
    fn close_preserves_release_all_and_discards_later_events() {
        let (batches, outcome) = collect_input_batches(
            [
                pointer(50),
                pointer(51),
                ViewerEvent::ReleaseAll,
                ViewerEvent::Close,
                pointer(52),
                ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
            ],
            true,
            true,
        );

        assert_eq!(batches, [vec![input_pointer(51), InputEvent::ReleaseAll]]);
        assert!(outcome.close);
        assert!(outcome.wrote_input);
        assert!(outcome.wrote_release_all);
    }

    #[test]
    fn batches_are_bounded_and_full_tail_pointer_stays_latest_wins() {
        let mut events = vec![ViewerEvent::ReleaseAll; MAX_INPUT_BATCH_EVENTS - 1];
        let mut expected = vec![InputEvent::ReleaseAll; MAX_INPUT_BATCH_EVENTS - 1];
        events.extend([pointer(60), pointer(61)]);
        expected.push(input_pointer(61));
        for index in 0..(MAX_INPUT_BATCH_EVENTS * 2) {
            let event = ViewerEvent::Key {
                keycode: 38,
                pressed: index % 2 == 0,
            };
            events.push(event);
            expected.push(InputEvent::Key {
                keycode: 38,
                pressed: index % 2 == 0,
            });
        }

        let (batches, outcome) = collect_input_batches(events, true, true);
        assert_eq!(
            batches.iter().map(Vec::len).collect::<Vec<_>>(),
            [128, 128, 128]
        );
        assert_eq!(batches.concat(), expected);
        assert!(outcome.wrote_input);
        assert!(!outcome.close);
    }

    #[test]
    fn frame_is_acknowledged_only_after_a_successful_draw() {
        let key = [0x31; 32];
        let telemetry = ClientTelemetry::default();
        let mut writer = SessionWriter::new(Vec::new(), key);
        draw_and_ack(&mut writer, queued_test_frame(9), &telemetry, |_| Ok(true)).unwrap();
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::FrameAck);
        assert_eq!(decode_frame_ack(&payload).unwrap(), 9);
        let window = telemetry.take_window();
        assert_eq!((window.drawn, window.acked), (1, 1));

        let mut writer = SessionWriter::new(Vec::new(), key);
        let result = draw_and_ack(&mut writer, queued_test_frame(10), &telemetry, |_| {
            Err(io::Error::other("synthetic draw failure").into())
        });
        assert!(result.is_err());
        assert!(writer.into_inner().is_empty());

        let mut writer = SessionWriter::new(Vec::new(), key);
        draw_and_ack(&mut writer, queued_test_frame(11), &telemetry, |_| {
            Ok(false)
        })
        .unwrap();
        assert!(writer.into_inner().is_empty());
        assert!(telemetry.take_window().is_empty());

        let mut writer = SessionWriter::new(FlushFailWriter::default(), key);
        let result = draw_and_ack(&mut writer, queued_test_frame(12), &telemetry, |_| Ok(true));
        assert!(result.is_err());
        let window = telemetry.take_window();
        assert_eq!((window.drawn, window.acked), (1, 0));
    }

    #[test]
    fn closed_or_failed_draw_returns_recyclable_frame_without_ack() {
        let key = [0x37; 32];
        let telemetry = ClientTelemetry::default();
        let pool = new_decode_buffer_pool();
        let queued = QueuedFrame {
            frame: decoded_test_frame_in_pool(20, Arc::clone(&pool)),
            ready_at: Instant::now(),
        };
        let mut writer = SessionWriter::new(Vec::new(), key);
        draw_and_ack(&mut writer, queued, &telemetry, |_| Ok(false)).unwrap();
        assert!(writer.into_inner().is_empty());
        assert_eq!(decode_pool_snapshot(&pool).0, 1);

        let queued = QueuedFrame {
            frame: decoded_test_frame_in_pool(21, Arc::clone(&pool)),
            ready_at: Instant::now(),
        };
        let mut writer = SessionWriter::new(Vec::new(), key);
        assert!(
            draw_and_ack(&mut writer, queued, &telemetry, |_| {
                Err(io::Error::other("synthetic viewer error").into())
            })
            .is_err()
        );
        assert!(writer.into_inner().is_empty());
        assert_eq!(decode_pool_snapshot(&pool).0, 2);
        assert!(telemetry.take_window().is_empty());
    }

    #[test]
    fn telemetry_windows_use_exact_stage_times_and_skip_empty_final_reports() {
        let telemetry = ClientTelemetry::default();
        let started = Instant::now();
        telemetry.record_received();
        telemetry.record_decoded_at(
            started + Duration::from_millis(2),
            started + Duration::from_millis(7),
        );
        telemetry.record_replaced();
        telemetry.record_drawn_at(
            started + Duration::from_millis(7),
            started + Duration::from_millis(18),
            started + Duration::from_millis(21),
        );
        telemetry.record_acked();

        let mut reporter = ClientTelemetryReporter::new_at(&telemetry, started);
        assert!(
            reporter
                .maybe_report_at(
                    started + TELEMETRY_INTERVAL - Duration::from_nanos(1),
                    false
                )
                .is_none()
        );
        let report = reporter
            .maybe_report_at(started + TELEMETRY_INTERVAL, false)
            .unwrap();
        assert_eq!(report.elapsed, TELEMETRY_INTERVAL);
        assert_eq!(
            (
                report.window.received,
                report.window.decoded,
                report.window.replaced,
                report.window.drawn,
                report.window.acked,
            ),
            (1, 1, 1, 1, 1)
        );
        assert_eq!(report.window.decode_elapsed, Duration::from_millis(5));
        assert_eq!(report.window.queue_elapsed, Duration::from_millis(11));
        assert_eq!(report.window.draw_elapsed, Duration::from_millis(3));

        // A forced cleanup immediately after a periodic report must not emit
        // a duplicate all-zero window.
        assert!(
            reporter
                .maybe_report_at(started + TELEMETRY_INTERVAL, true)
                .is_none()
        );
    }

    #[test]
    fn authenticated_frame_is_received_even_when_jpeg_decode_fails() {
        let telemetry = ClientTelemetry::default();
        telemetry.record_received();
        assert!(
            decode_queued_frame(b"not a frame", &telemetry, &new_decode_buffer_pool()).is_err()
        );

        let window = telemetry.take_window();
        assert_eq!(window.received, 1);
        assert_eq!(window.decoded, 0);
        assert_eq!(window.decode_elapsed, Duration::ZERO);
    }

    #[test]
    fn replacing_latest_returns_the_stale_value_after_unlocking() {
        let latest = Mutex::new(Some(vec![1_u8; 1024]));
        let stale = replace_latest(&latest, vec![2_u8; 2048]);

        assert_eq!(stale.as_ref().map(Vec::len), Some(1024));
        let current = latest
            .try_lock()
            .expect("latest-frame lock must be released before stale frame drop");
        assert_eq!(current.as_ref().map(Vec::len), Some(2048));
        drop(current);
        drop(stale);
    }

    #[test]
    fn receiver_sequence_error_recycles_frame_without_publishing_latest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        let mut writer = SessionWriter::new(sender, TEST_PSK);
        let payload = encode_frame(
            2,
            &CapturedFrame {
                image: RgbImage::from_pixel(32, 18, Rgb([20, 40, 80])),
                source_width: 32,
                source_height: 18,
            },
            80,
        )
        .unwrap();
        writer.write_message(MessageKind::Frame, &payload).unwrap();
        writer.flush().unwrap();

        let state = Arc::new(ReceiveState::new());
        let (_wake_receiver, wake_sender) = wake_pair().unwrap();
        receive_frames(
            SessionReader::new(receiver, TEST_PSK),
            Arc::clone(&state),
            0,
            Vec::new(),
            wake_sender,
        );

        assert!(!state.alive.load(Ordering::Acquire));
        assert!(state.latest.lock().unwrap().is_none());
        assert!(
            state
                .error
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| error.contains("expected 1, got 2"))
        );
        assert_eq!(decode_pool_snapshot(&state.decode_pool).0, 1);
    }

    #[test]
    fn receiver_authentication_failure_never_checks_out_a_decode_buffer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        let mut writer = SessionWriter::new(sender, [0x91; PSK_LEN]);
        let payload = encode_frame(
            1,
            &CapturedFrame {
                image: RgbImage::from_pixel(32, 18, Rgb([20, 40, 80])),
                source_width: 32,
                source_height: 18,
            },
            80,
        )
        .unwrap();
        writer.write_message(MessageKind::Frame, &payload).unwrap();
        writer.flush().unwrap();

        let state = Arc::new(ReceiveState::new());
        let (_wake_receiver, wake_sender) = wake_pair().unwrap();
        receive_frames(
            SessionReader::new(receiver, TEST_PSK),
            Arc::clone(&state),
            0,
            Vec::new(),
            wake_sender,
        );

        assert!(!state.alive.load(Ordering::Acquire));
        assert!(state.latest.lock().unwrap().is_none());
        assert_eq!(decode_pool_snapshot(&state.decode_pool).0, 0);
        assert_eq!(state.telemetry.take_window().received, 0);
    }

    #[test]
    fn poll_timeout_rounds_up_and_never_sleeps_past_due() {
        let now = Instant::now();
        assert_eq!(poll_timeout_until(now, now), PollTimeout::ZERO);
        assert_eq!(
            poll_timeout_until(now, now + Duration::from_nanos(1)).as_millis(),
            Some(1)
        );
        assert_eq!(
            poll_timeout_until(now, now + Duration::from_millis(1)).as_millis(),
            Some(1)
        );
        assert_eq!(
            poll_timeout_until(
                now,
                now + Duration::from_millis(1) + Duration::from_nanos(1)
            )
            .as_millis(),
            Some(2)
        );
    }

    #[test]
    fn receiver_wake_is_coalesced_and_visible_to_poll() {
        let (mut receiver, sender) = wake_pair().unwrap();
        let (x11_reader, _x11_writer) = UnixStream::pair().unwrap();
        sender.signal().unwrap();
        sender.signal().unwrap();
        assert_eq!(
            wait_for_activity(
                x11_reader.as_fd(),
                receiver.as_fd(),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap(),
            WaitOutcome::Activity
        );
        receiver.drain().unwrap();
    }

    #[test]
    fn publication_after_a_drain_cannot_lose_its_wake() {
        let (mut receiver, sender) = wake_pair().unwrap();
        let (x11_reader, _x11_writer) = UnixStream::pair().unwrap();
        receiver.drain().unwrap();
        let published = Arc::new(AtomicBool::new(false));
        let worker_published = Arc::clone(&published);
        let (start_tx, start_rx) = mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            start_rx.recv().unwrap();
            worker_published.store(true, Ordering::Release);
            sender.signal().unwrap();
        });

        start_tx.send(()).unwrap();
        assert_eq!(
            wait_for_activity(
                x11_reader.as_fd(),
                receiver.as_fd(),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap(),
            WaitOutcome::Activity
        );
        assert!(published.load(Ordering::Acquire));
        worker.join().unwrap();
        receiver.drain().unwrap();
    }

    #[test]
    fn x11_poll_hangup_is_terminal_instead_of_spinning() {
        let (x11_reader, x11_writer) = UnixStream::pair().unwrap();
        let (wake_receiver, _wake_sender) = wake_pair().unwrap();
        drop(x11_writer);
        assert_eq!(
            wait_for_activity(
                x11_reader.as_fd(),
                wake_receiver.as_fd(),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap(),
            WaitOutcome::X11Hangup
        );
    }

    #[test]
    fn receiver_completion_publishes_terminal_state_before_wake() {
        let state = Arc::new(ReceiveState::new());
        let (mut wake_receiver, wake_sender) = wake_pair().unwrap();
        {
            // Dropping an unfinished completion guard models stack unwinding
            // from a panic in the receiver body.
            let _completion = ReceiveCompletion::new(Arc::clone(&state), wake_sender);
        }
        let (x11_reader, _x11_writer) = UnixStream::pair().unwrap();
        wait_for_activity(
            x11_reader.as_fd(),
            wake_receiver.as_fd(),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        wake_receiver.drain().unwrap();
        assert!(!state.alive.load(Ordering::Acquire));
        assert_eq!(
            state.error.lock().unwrap().as_deref(),
            Some("remote video thread panicked")
        );
    }

    #[test]
    fn receiver_join_reports_completion_panic_and_timeout() {
        let completed = ReceiverThread::spawn(|| {}).unwrap();
        assert_eq!(
            completed.join_timeout(Duration::from_secs(1)),
            ReceiverExit::Completed
        );

        let panicked = ReceiverThread::spawn(|| panic!("synthetic receiver panic")).unwrap();
        assert_eq!(
            panicked.join_timeout(Duration::from_secs(1)),
            ReceiverExit::Panicked
        );

        let (release_tx, release_rx) = mpsc::channel();
        let blocked = ReceiverThread::spawn(move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        assert_eq!(blocked.join_timeout(Duration::ZERO), ReceiverExit::TimedOut);
        release_tx.send(()).unwrap();
    }

    #[test]
    fn session_cancel_sends_close_once_only_on_graceful_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let control = client.try_clone().unwrap();
        let state = Arc::new(ReceiveState::new());
        let mut writer = SessionWriter::new(client, TEST_PSK);
        let mut cancel = SessionCancel::new(control, Arc::clone(&state));

        cancel.cancel(&mut writer, true).unwrap();
        cancel.cancel(&mut writer, true).unwrap();
        let mut wire = Vec::new();
        server.read_to_end(&mut wire).unwrap();
        let mut reader = SessionReader::new(Cursor::new(wire), TEST_PSK);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Close);
        assert!(payload.is_empty());
        assert!(reader.read_message().is_err());
        assert!(state.stopping.load(Ordering::Acquire));
    }

    #[test]
    fn fatal_session_cancel_only_shuts_down_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let control = client.try_clone().unwrap();
        let state = Arc::new(ReceiveState::new());
        let mut writer = SessionWriter::new(client, TEST_PSK);
        let mut cancel = SessionCancel::new(control, Arc::clone(&state));

        cancel.cancel(&mut writer, false).unwrap();
        cancel.cancel(&mut writer, true).unwrap();
        let mut wire = Vec::new();
        server.read_to_end(&mut wire).unwrap();
        assert!(wire.is_empty());
        assert!(state.stopping.load(Ordering::Acquire));
    }

    #[test]
    fn negotiation_deadline_cannot_be_extended_by_a_slow_nonce() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for byte in 0_u8..32 {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });

        let stream = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        let result =
            negotiate_session(stream, &TEST_PSK, false, [0; 32], Duration::from_millis(80));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }

    #[test]
    fn negotiation_deadline_also_covers_a_slow_hello_ack() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x24; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            ClientHello::decode(&payload).unwrap();

            let mut encoded = SessionWriter::new(Vec::new(), send_key);
            encoded
                .write_message(
                    MessageKind::HelloAck,
                    &ServerHello {
                        pointer_enabled: false,
                        keyboard_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            for byte in encoded.into_inner() {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });

        let stream = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        let result = negotiate_session(
            stream,
            &TEST_PSK,
            false,
            [0; 32],
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }

    #[test]
    fn successful_negotiation_cancels_its_absolute_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x33; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            ClientHello::decode(&payload).unwrap();
            writer
                .write_message(
                    MessageKind::HelloAck,
                    &ServerHello {
                        pointer_enabled: false,
                        keyboard_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            writer.flush().unwrap();
            thread::sleep(Duration::from_millis(160));
            writer.write_message(MessageKind::Close, &[]).unwrap();
            writer.flush().unwrap();
        });

        let stream = TcpStream::connect(address).unwrap();
        let (mut reader, _writer, hello) =
            negotiate_session(stream, &TEST_PSK, false, [0; 32], Duration::from_millis(80))
                .unwrap();
        assert!(!hello.pointer_enabled);
        assert!(!hello.keyboard_enabled);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Close);
        assert!(payload.is_empty());
        server.join().unwrap();
    }
}
