//! Remote desktop viewer/client orchestration.

use super::RemoteResult;
use super::deadline::TcpStreamDeadline;
use super::frame::{DecodedFrame, decode_frame};
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
use std::io;
use std::io::Write as _;
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    let first_frame = decode_queued_frame(&payload, &state.telemetry)?;
    reader
        .get_ref()
        .set_read_timeout(Some(FRAME_IDLE_TIMEOUT))?;
    let initial_width = u16::try_from(first_frame.frame.image.width())
        .map_err(|_| invalid_data("initial frame is wider than an X11 window"))?;
    let initial_height = u16::try_from(first_frame.frame.image.height())
        .map_err(|_| invalid_data("initial frame is taller than an X11 window"))?;
    let last_sequence = first_frame.frame.sequence;
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
        viewer.draw(frame)
    })?;

    let receive_state = Arc::clone(&state);
    let control = writer.get_ref().try_clone()?;
    let receiver = thread::Builder::new()
        .name("jwm-remote-video".into())
        // Move the authenticated first-frame allocation into the receiver
        // thread. It remains thread-local while avoiding a second large
        // allocation for the next frame.
        .spawn(move || receive_frames(reader, receive_state, last_sequence, payload))?;

    let pointer_enabled = hello.pointer_enabled && !options.view_only;
    let keyboard_enabled = hello.keyboard_enabled && !options.view_only;
    let loop_result = viewer_loop(
        &mut viewer,
        &mut writer,
        &state,
        pointer_enabled,
        keyboard_enabled,
        &mut telemetry_reporter,
    );
    state.stopping.store(true, Ordering::Release);
    if pointer_enabled || keyboard_enabled {
        let mut payload = Vec::new();
        let _ = write_input_batch(&mut writer, &[InputEvent::ReleaseAll], &mut payload);
    }
    let _ = writer.write_message(MessageKind::Close, &[]);
    let _ = writer.flush();
    let _ = control.shutdown(Shutdown::Both);

    let receiver_result = receiver.join();
    telemetry_reporter.force_report();
    match receiver_result {
        Ok(()) => {}
        Err(_) if loop_result.is_ok() => {
            return Err(io::Error::other("remote video thread panicked").into());
        }
        Err(_) => {}
    }
    loop_result
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
) -> RemoteResult<()> {
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut input_batch = Vec::with_capacity(MAX_INPUT_BATCH_EVENTS);
    let mut input_payload = Vec::new();
    loop {
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
            return Ok(());
        }

        if let Some(frame) = take_latest(state) {
            draw_and_ack(writer, frame, &state.telemetry, |frame| viewer.draw(frame))?;
        }

        if !state.alive.load(Ordering::Acquire) && state.latest.lock().unwrap().is_none() {
            if state.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            let message = state
                .error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| "host closed the remote session".to_string());
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, message).into());
        }
        telemetry_reporter.maybe_report();
        thread::sleep(Duration::from_millis(4));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InputPollOutcome {
    wrote_input: bool,
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
            batch.clear();
        }
        batch.push(input);
        pointer_run_open = matches!(input, InputEvent::Pointer { .. });
    }

    if !batch.is_empty() {
        emit(batch)?;
        outcome.wrote_input = true;
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
    draw: impl FnOnce(DecodedFrame) -> RemoteResult<bool>,
) -> RemoteResult<()> {
    let sequence = queued.frame.sequence;
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
    frame: DecodedFrame,
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
}

impl ReceiveState {
    fn new() -> Self {
        Self {
            latest: Mutex::new(None),
            error: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
            telemetry: ClientTelemetry::default(),
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

fn decode_queued_frame(payload: &[u8], telemetry: &ClientTelemetry) -> RemoteResult<QueuedFrame> {
    let decode_started = Instant::now();
    let frame = decode_frame(payload)?;
    let ready_at = Instant::now();
    telemetry.record_decoded_at(decode_started, ready_at);
    Ok(QueuedFrame { frame, ready_at })
}

fn receive_frames(
    mut reader: SessionReader<TcpStream>,
    state: Arc<ReceiveState>,
    mut last_sequence: u64,
    mut payload: Vec<u8>,
) {
    let mut payload_retention = PayloadBufferRetention::default();
    let result: RemoteResult<()> = (|| loop {
        let kind = reader.read_message_into(&mut payload)?;
        match kind {
            MessageKind::Frame => {
                state.telemetry.record_received();
                let queued = decode_queued_frame(&payload, &state.telemetry)?;
                let expected = last_sequence
                    .checked_add(1)
                    .ok_or_else(|| invalid_data("video frame sequence exhausted"))?;
                if queued.frame.sequence != expected {
                    return Err(invalid_data(format!(
                        "video frame sequence mismatch: expected {expected}, got {}",
                        queued.frame.sequence
                    ))
                    .into());
                }
                last_sequence = queued.frame.sequence;
                let replaced = replace_latest(&state.latest, queued);
                if replaced.is_some() {
                    state.telemetry.record_replaced();
                }
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

    if let Err(error) = result
        && !state.stopping.load(Ordering::Acquire)
    {
        *state.error.lock().unwrap() = Some(error.to_string());
    }
    state.alive.store(false, Ordering::Release);
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::messages::decode_frame_ack;
    use crate::remote::protocol::{PSK_LEN, server_handshake};
    use image::RgbImage;
    use std::io::Cursor;
    use std::io::Write;
    use std::net::TcpListener;

    const TEST_PSK: [u8; PSK_LEN] = [0x5a; PSK_LEN];

    fn decoded_test_frame(sequence: u64) -> DecodedFrame {
        DecodedFrame {
            sequence,
            source_width: 1,
            source_height: 1,
            image: RgbImage::new(1, 1),
        }
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
        assert!(decode_queued_frame(b"not a frame", &telemetry).is_err());

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
