//! Trusted-LAN remote desktop host.

use super::RemoteResult;
use super::deadline::TcpStreamDeadline;
use super::key::load_key_file;
use super::messages::{
    ClientHello, MAX_CLIPBOARD_BYTES, MIN_VIEWPORT_HEIGHT, MIN_VIEWPORT_WIDTH, ServerHello,
    decode_clipboard, decode_frame_ack, decode_input_batch, decode_viewport, encode_clipboard,
};
use super::protocol::{
    MessageKind, PayloadBufferRetention, ProtocolError, SessionReader, SessionWriter,
    server_handshake,
};
use super::tile::{TileEncodeRequest, TileEncoder, TilePlan};
use super::x11_capture::{
    CaptureArea, CaptureMode, CaptureOutcome, CaptureSource, CapturedFrame, X11Capture,
    validate_capture_area,
};
use super::x11_input::InputInjector;
use crate::backend::clipboard_x11::{Clipboard, ClipboardCaptures, ClipboardSetter};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_WRITE_DEADLINE: Duration = Duration::from_secs(10);
// The client cannot enter its heartbeat loop until the first frame is received,
// decoded and drawn.  Do not apply the held-input safety timeout before then.
const INITIAL_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(8);
/// How long the peer may go silent while it still holds a key or button.
///
/// The *host* X server generates autorepeat, so a partition with a key down
/// injected characters for the full session idle timeout — eight seconds of
/// repeat, or eight seconds of held Ctrl/Alt/Super, into whatever had focus.
const HELD_INPUT_SILENCE_TIMEOUT: Duration = Duration::from_millis(600);
const FRAME_ACK_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OUTSTANDING_FRAMES: u64 = 2;
/// Largest record the host ever legitimately receives.
///
/// The client sends only a hello, empty heartbeats, eight-byte frame
/// acknowledgements and input batches; the largest of those is a padded
/// 641-byte batch. Declaring that here stops an unauthenticated length field
/// from making the host reserve up to the global 32 MiB frame limit.
const MAX_INBOUND_PAYLOAD_LEN: usize = 1024;
/// Inbound ceiling once clipboard sharing is enabled.
///
/// Kept separate so a session without `--allow-clipboard` still refuses
/// anything larger than an input batch before allocating for it.
const MAX_INBOUND_CLIPBOARD_PAYLOAD_LEN: usize = MAX_CLIPBOARD_BYTES + 1024;
/// How long the clipboard forwarder waits before re-checking for shutdown.
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Longest single sleep while rate limited, so shutdown stays responsive.
const CAPTURE_RATE_LIMIT_SLICE: Duration = Duration::from_millis(20);

/// The encoded width the viewer has asked for, shared across session threads.
///
/// The input thread learns the viewer's size; the capture thread is what acts
/// on it. A single relaxed integer is enough: the value is idempotent, a lost
/// update is corrected by the next request, and neither side may block.
#[derive(Debug)]
struct ViewportRequest {
    /// Packed `width << 16 | height`, or zero while nothing has been asked.
    size: AtomicU32,
}

impl ViewportRequest {
    fn new() -> Self {
        Self {
            size: AtomicU32::new(0),
        }
    }

    fn request(&self, width: u16, height: u16) {
        let packed = (u32::from(width) << 16) | u32::from(height);
        self.size.store(packed, Ordering::Relaxed);
    }

    fn take(&self) -> Option<(u16, u16)> {
        match self.size.swap(0, Ordering::Relaxed) {
            0 => None,
            packed => Some(((packed >> 16) as u16, packed as u16)),
        }
    }
}

/// Resolve a viewer request against the operator's configured ceiling.
///
/// `--max-width` is a policy limit, so a request may only ever narrow it. A
/// peer that asks for more pixels than the operator allowed gets the ceiling,
/// not its request. Height has no configured counterpart, so the viewer's own
/// height bounds it directly -- which is also what stops one `--max-width`
/// meaning very different amounts of work on stacked or portrait roots.
fn effective_encoded_bounds(configured_width: u16, requested: (u16, u16)) -> (u16, u16) {
    let (width, height) = requested;
    let width = width.max(MIN_VIEWPORT_WIDTH);
    let height = height.max(MIN_VIEWPORT_HEIGHT);
    let width = if configured_width == 0 {
        // Native capture: any request narrows it.
        width
    } else {
        width.min(configured_width)
    };
    (width, height)
}
const ACCEPT_EXHAUSTION_BACKOFF: Duration = Duration::from_millis(200);
const UNCHANGED_FRAME_KEEPALIVE: Duration = Duration::from_secs(4);
const MIN_BACKPRESSURE_REFRESH: Duration = Duration::from_millis(250);
const MAX_BACKPRESSURE_REFRESH: Duration = Duration::from_secs(1);
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);
const JPEG_QUALITY_EVALUATION_INTERVAL: Duration = Duration::from_millis(500);
const JPEG_QUALITY_CONGESTED_ACK_RTT: Duration = Duration::from_millis(350);
const JPEG_QUALITY_HARD_ACK_RTT: Duration = Duration::from_millis(750);
const JPEG_QUALITY_BACKLOG_ACK_RTT: Duration = Duration::from_millis(200);
const JPEG_QUALITY_HEALTHY_ACK_RTT: Duration = Duration::from_millis(160);
const JPEG_QUALITY_PRESSURE_THRESHOLD: u64 = 6;
const JPEG_QUALITY_RECOVERY_DURATION: Duration = Duration::from_secs(3);
const JPEG_QUALITY_MIN_RECOVERY_ACKS: u64 = 4;
const JPEG_QUALITY_MAX_RECOVERY_ACKS: u64 = 24;
const JPEG_QUALITY_STEP_UP: u8 = 1;

trait SetWriteTimeout {
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl SetWriteTimeout for TcpStream {
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

/// Applies one absolute deadline to all writes and the flush for a record.
///
/// `SO_SNDTIMEO` alone restarts for every partial `write(2)`. Replacing it
/// with the time remaining before each call prevents a peer that reads a few
/// bytes at a time from extending a record indefinitely. A failed record
/// permanently poisons the writer because retrying after a partial record
/// would corrupt the authenticated stream.
struct DeadlineWriter<W> {
    inner: W,
    fallback_timeout: Duration,
    deadline: Option<Instant>,
    failed: bool,
}

impl<W> DeadlineWriter<W> {
    fn new(inner: W, fallback_timeout: Duration) -> Self {
        Self {
            inner,
            fallback_timeout,
            deadline: None,
            failed: false,
        }
    }

    fn get_ref(&self) -> &W {
        &self.inner
    }

    fn begin_record(&mut self, timeout: Duration) -> io::Result<()> {
        if self.failed {
            return Err(poisoned_writer());
        }
        if self.deadline.is_some() {
            self.failed = true;
            return Err(io::Error::other(
                "remote record writer already has an active deadline",
            ));
        }
        if timeout.is_zero() {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "remote video record write deadline expired",
            ));
        }
        self.deadline = Instant::now().checked_add(timeout);
        if self.deadline.is_none() {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote video record write deadline is too large",
            ));
        }
        Ok(())
    }

    fn finish_record(&mut self) -> io::Result<()>
    where
        W: SetWriteTimeout,
    {
        if self.failed {
            return Err(poisoned_writer());
        }
        if self.deadline.take().is_none() {
            self.failed = true;
            return Err(io::Error::other(
                "remote record writer has no active deadline",
            ));
        }
        if let Err(error) = self.inner.set_write_timeout(Some(self.fallback_timeout)) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn poison_record(&mut self) {
        self.failed = true;
        self.deadline = None;
    }
}

impl<W: std::io::Write + SetWriteTimeout> DeadlineWriter<W> {
    fn prepare_call(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(poisoned_writer());
        }
        let timeout = match self.deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    self.failed = true;
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "remote video record write deadline expired",
                    )
                })?,
            None => self.fallback_timeout,
        };
        if timeout.is_zero() {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "remote video record write deadline expired",
            ));
        }
        if let Err(error) = self.inner.set_write_timeout(Some(timeout)) {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn complete_call<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.failed = true;
                return Err(
                    if self.deadline.is_some()
                        && matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        )
                    {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "remote video record write deadline expired",
                        )
                    } else {
                        error
                    },
                );
            }
        };
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "remote video record write deadline expired",
            ));
        }
        Ok(value)
    }
}

impl<W: std::io::Write + SetWriteTimeout> std::io::Write for DeadlineWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.prepare_call()?;
        let result = self.inner.write(buffer);
        let written = self.complete_call(result)?;
        if written == 0 && !buffer.is_empty() {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "remote record writer made no progress",
            ));
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.prepare_call()?;
        let result = self.inner.flush();
        self.complete_call(result)
    }
}

fn poisoned_writer() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "remote record writer is unusable after a failed record",
    )
}

struct PendingFrame {
    frame: CapturedFrame,
    captured_at: Instant,
}

/// Chooses what each captured frame owes the viewer and encodes it.
///
/// The delta encoder subsumes the previous exact-duplicate suppressor: an
/// unchanged capture simply has no dirty tiles. Keepalive timing stays here
/// rather than in the codec because it is a property of the *session*, and it
/// must remain strictly inside the viewer's shared video idle timeout.
struct FrameSender {
    encoder: TileEncoder,
    committed_at: Option<Instant>,
}

impl FrameSender {
    fn new() -> Self {
        Self {
            encoder: TileEncoder::new(),
            committed_at: None,
        }
    }

    fn plan_at(&mut self, frame: &CapturedFrame, now: Instant) -> RemoteResult<TilePlan> {
        let request = match self.committed_at {
            None => TileEncodeRequest::Keyframe,
            Some(committed_at)
                if now.saturating_duration_since(committed_at) >= UNCHANGED_FRAME_KEEPALIVE =>
            {
                TileEncodeRequest::Keepalive
            }
            Some(_) => TileEncodeRequest::Delta,
        };
        self.encoder.plan(frame, request)
    }

    /// Adopt a frame whose authenticated record and flush both succeeded.
    fn commit_at(&mut self, frame: &CapturedFrame, committed_at: Instant) {
        self.encoder.commit(frame);
        self.committed_at = Some(committed_at);
    }

    /// Abandon an encoded-but-unsent frame, leaving the viewer's model intact.
    fn discard(&mut self) {
        self.encoder.discard();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HostTelemetryWindow {
    scheduled: u64,
    captured: u64,
    skipped: u64,
    damage_skipped: u64,
    published: u64,
    replaced: u64,
    dequeued: u64,
    unchanged_suppressed: u64,
    unchanged_keepalive: u64,
    encoded: u64,
    keyframes: u64,
    dirty_tiles: u64,
    total_tiles: u64,
    sent: u64,
    bytes: u64,
    drawn_acks: u64,
    drawn_bytes: u64,
    retired: u64,
    viewer_superseded: u64,
    capture_mode: CaptureMode,
    capture_elapsed: Duration,
    queue_elapsed: Duration,
    credit_wait_elapsed: Duration,
    encode_elapsed: Duration,
    write_elapsed: Duration,
    capture_to_ack: Duration,
    send_to_ack: Duration,
    max_outstanding: u64,
    max_queue_age: Duration,
}

impl HostTelemetryWindow {
    fn has_activity(self) -> bool {
        self.scheduled != 0
            || self.captured != 0
            || self.skipped != 0
            || self.damage_skipped != 0
            || self.published != 0
            || self.replaced != 0
            || self.dequeued != 0
            || self.unchanged_suppressed != 0
            || self.unchanged_keepalive != 0
            || self.encoded != 0
            || self.keyframes != 0
            || self.capture_mode != CaptureMode::empty()
            || self.sent != 0
            || self.bytes != 0
            || self.drawn_acks != 0
            || self.drawn_bytes != 0
            || self.retired != 0
            || self.max_outstanding != 0
    }
}

struct HostTelemetryState {
    window_started: Instant,
    window: HostTelemetryWindow,
}

struct HostTelemetry {
    state: Mutex<HostTelemetryState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostTelemetrySnapshot {
    elapsed: Duration,
    window: HostTelemetryWindow,
}

impl HostTelemetry {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            state: Mutex::new(HostTelemetryState {
                window_started: now,
                window: HostTelemetryWindow::default(),
            }),
        }
    }

    fn update(&self, update: impl FnOnce(&mut HostTelemetryWindow)) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        update(&mut state.window);
    }

    fn record_scheduled(&self) {
        self.update(|window| window.scheduled = window.scheduled.saturating_add(1));
    }

    fn record_skipped(&self) {
        self.update(|window| window.skipped = window.skipped.saturating_add(1));
    }

    fn record_capture_mode(&self, mode: CaptureMode) {
        self.update(|window| window.capture_mode = mode);
    }

    fn record_damage_skipped(&self) {
        self.update(|window| {
            window.damage_skipped = window.damage_skipped.saturating_add(1);
        });
    }

    fn record_captured(&self, elapsed: Duration) {
        self.update(|window| {
            window.captured = window.captured.saturating_add(1);
            window.capture_elapsed = window.capture_elapsed.saturating_add(elapsed);
        });
    }

    fn record_published(&self, replaced: bool) {
        self.update(|window| {
            window.published = window.published.saturating_add(1);
            if replaced {
                window.replaced = window.replaced.saturating_add(1);
            }
        });
    }

    fn record_dequeued(&self, queue_age: Duration, credit_wait: Duration) {
        self.update(|window| {
            window.dequeued = window.dequeued.saturating_add(1);
            window.queue_elapsed = window.queue_elapsed.saturating_add(queue_age);
            window.credit_wait_elapsed = window.credit_wait_elapsed.saturating_add(credit_wait);
            window.max_queue_age = window.max_queue_age.max(queue_age);
        });
    }

    fn record_unchanged_suppressed(&self) {
        self.update(|window| {
            window.unchanged_suppressed = window.unchanged_suppressed.saturating_add(1);
        });
    }

    fn record_unchanged_keepalive(&self) {
        self.update(|window| {
            window.unchanged_keepalive = window.unchanged_keepalive.saturating_add(1);
        });
    }

    fn record_encoded(&self, elapsed: Duration, plan: TilePlan) {
        self.update(|window| {
            window.encoded = window.encoded.saturating_add(1);
            window.encode_elapsed = window.encode_elapsed.saturating_add(elapsed);
            if plan.keyframe {
                window.keyframes = window.keyframes.saturating_add(1);
            }
            window.dirty_tiles = window
                .dirty_tiles
                .saturating_add(u64::from(plan.dirty_tiles));
            window.total_tiles = window
                .total_tiles
                .saturating_add(u64::from(plan.total_tiles));
        });
    }

    fn record_outstanding(&self, outstanding: u64) {
        self.update(|window| window.max_outstanding = window.max_outstanding.max(outstanding));
    }

    fn record_sent(&self, bytes: usize, elapsed: Duration) {
        self.update(|window| {
            window.sent = window.sent.saturating_add(1);
            window.bytes = window
                .bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            window.write_elapsed = window.write_elapsed.saturating_add(elapsed);
        });
    }

    fn record_ack(&self, ack: AckObservation) {
        self.update(|window| {
            window.drawn_acks = window.drawn_acks.saturating_add(1);
            window.drawn_bytes = window.drawn_bytes.saturating_add(ack.bytes);
            window.retired = window.retired.saturating_add(ack.retired);
            window.viewer_superseded = window
                .viewer_superseded
                .saturating_add(ack.retired.saturating_sub(1));
            window.capture_to_ack = window.capture_to_ack.saturating_add(ack.capture_to_ack);
            window.send_to_ack = window.send_to_ack.saturating_add(ack.send_to_ack);
        });
    }

    fn take_due_at(&self, now: Instant) -> Option<HostTelemetrySnapshot> {
        self.take_at(now, false)
    }

    fn take_final_at(&self, now: Instant) -> Option<HostTelemetrySnapshot> {
        self.take_at(now, true)
    }

    fn take_at(&self, now: Instant, final_snapshot: bool) -> Option<HostTelemetrySnapshot> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let elapsed = now.saturating_duration_since(state.window_started);
        if !final_snapshot && elapsed < TELEMETRY_INTERVAL {
            return None;
        }
        if final_snapshot && !state.window.has_activity() {
            return None;
        }
        let window = std::mem::take(&mut state.window);
        state.window_started = now;
        Some(HostTelemetrySnapshot { elapsed, window })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureDecision {
    Capture,
    Skip,
    Closed,
}

struct MailboxState<T> {
    latest: Option<T>,
    queued_at: Option<Instant>,
    closed: bool,
}

/// A one-slot mailbox whose producer never waits for a slow consumer.
///
/// Replacing the queued item keeps latency bounded: the consumer works on at
/// most one item while exactly one newer item can wait behind it.
struct LatestMailbox<T> {
    state: Mutex<MailboxState<T>>,
    ready: Condvar,
}

impl<T> LatestMailbox<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                latest: None,
                queued_at: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn publish(&self, item: T) -> Option<bool> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.closed {
            return None;
        }
        let replaced = state.latest.replace(item);
        state.queued_at = Some(Instant::now());
        let did_replace = replaced.is_some();
        self.ready.notify_one();
        drop(state);
        // Dropping a full-resolution RGB frame can release a large allocation;
        // do it after unlocking so the sender never waits on the allocator.
        drop(replaced);
        Some(did_replace)
    }

    fn capture_decision(&self, refresh_after: Duration) -> io::Result<CaptureDecision> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame mailbox lock was poisoned"))?;
        if state.closed {
            return Ok(CaptureDecision::Closed);
        }
        let capture = state.latest.is_none()
            || state
                .queued_at
                .is_none_or(|queued_at| queued_at.elapsed() >= refresh_after);
        if capture {
            return Ok(CaptureDecision::Capture);
        }
        Ok(CaptureDecision::Skip)
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
                state.queued_at = None;
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
        state.queued_at = None;
        self.ready.notify_all();
        drop(state);
        drop(pending);
    }
}

#[derive(Clone, Copy, Debug)]
struct InFlightFrame {
    sequence: u64,
    captured_at: Instant,
    sent_at: Instant,
    bytes: u64,
    jpeg_quality: u8,
    quality_epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AckObservation {
    retired: u64,
    bytes: u64,
    same_epoch_retired: u64,
    same_epoch_outstanding_before: u64,
    jpeg_quality: u8,
    quality_epoch: u64,
    capture_to_ack: Duration,
    send_to_ack: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct JpegQualitySignals {
    acknowledgements: u64,
    same_epoch_superseded: u64,
    pressure: u64,
    max_ack_rtt: Duration,
    max_payload_bytes: u64,
    max_outstanding: u64,
}

impl JpegQualitySignals {
    fn pressure_for_ack(ack: AckObservation) -> u64 {
        let superseded = ack.same_epoch_retired > 1;
        if ack.send_to_ack >= JPEG_QUALITY_HARD_ACK_RTT || superseded {
            3
        } else if ack.same_epoch_outstanding_before >= MAX_OUTSTANDING_FRAMES
            && ack.send_to_ack >= JPEG_QUALITY_BACKLOG_ACK_RTT
        {
            2
        } else if ack.send_to_ack >= JPEG_QUALITY_CONGESTED_ACK_RTT {
            1
        } else {
            0
        }
    }

    fn record_ack(&mut self, ack: AckObservation, pressure: u64) {
        self.acknowledgements = self.acknowledgements.saturating_add(1);
        self.same_epoch_superseded = self
            .same_epoch_superseded
            .saturating_add(ack.same_epoch_retired.saturating_sub(1));
        self.pressure = self.pressure.saturating_add(pressure);
        self.max_ack_rtt = self.max_ack_rtt.max(ack.send_to_ack);
        self.max_payload_bytes = self.max_payload_bytes.max(ack.bytes);
        self.max_outstanding = self.max_outstanding.max(ack.same_epoch_outstanding_before);
    }
}

#[derive(Clone, Copy, Debug)]
struct TimedAckObservation {
    ack: AckObservation,
    observed_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JpegQualityAdjustment {
    Decreased,
    Increased,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JpegQualityDecision {
    quality: u8,
    epoch: u64,
    previous: u8,
    adjustment: Option<JpegQualityAdjustment>,
    signals: JpegQualitySignals,
    accumulated_pressure: u64,
}

struct JpegQualityState {
    current: u8,
    epoch: u64,
    last_evaluated_at: Instant,
    pressure: u64,
    healthy_ack_streak: u64,
    healthy_since: Option<Instant>,
    signals: JpegQualitySignals,
    pending_acks: VecDeque<TimedAckObservation>,
}

/// ACK-driven JPEG quality with one writer: only the video sender changes or
/// reads `current`; the input thread merely appends small scalar observations.
struct JpegQualityController {
    maximum: u8,
    floor: u8,
    adaptive: bool,
    recovery_ack_target: u64,
    state: Mutex<JpegQualityState>,
}

impl JpegQualityController {
    fn new_at(
        maximum: u8,
        floor: u8,
        adaptive: bool,
        frame_interval: Duration,
        now: Instant,
    ) -> io::Result<Self> {
        if !(1..=100).contains(&maximum) {
            return Err(invalid_data("JPEG quality must be between 1 and 100"));
        }
        if !(1..=maximum).contains(&floor) {
            return Err(invalid_data(format!(
                "JPEG quality floor {floor} exceeds the configured maximum {maximum}"
            )));
        }
        Ok(Self {
            maximum,
            floor,
            adaptive,
            recovery_ack_target: jpeg_quality_recovery_ack_target(frame_interval),
            state: Mutex::new(JpegQualityState {
                current: maximum,
                epoch: 0,
                last_evaluated_at: now,
                pressure: 0,
                healthy_ack_streak: 0,
                healthy_since: None,
                signals: JpegQualitySignals::default(),
                pending_acks: VecDeque::with_capacity(MAX_OUTSTANDING_FRAMES as usize),
            }),
        })
    }

    fn observe_ack_at(&self, ack: AckObservation, now: Instant) {
        if !self.adaptive {
            return;
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.pending_acks.push_back(TimedAckObservation {
            ack,
            observed_at: now,
        });
    }

    fn quality_before_encode_at(&self, now: Instant) -> JpegQualityDecision {
        if !self.adaptive {
            return JpegQualityDecision {
                quality: self.maximum,
                epoch: 0,
                previous: self.maximum,
                adjustment: None,
                signals: JpegQualitySignals::default(),
                accumulated_pressure: 0,
            };
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        while let Some(observation) = state.pending_acks.pop_front() {
            let ack = observation.ack;
            if ack.quality_epoch == state.epoch && ack.jpeg_quality == state.current {
                let pressure = JpegQualitySignals::pressure_for_ack(ack);
                state.signals.record_ack(ack, pressure);
                if pressure != 0 {
                    state.pressure = state.pressure.saturating_add(pressure);
                    state.healthy_ack_streak = 0;
                    state.healthy_since = None;
                } else if ack.send_to_ack <= JPEG_QUALITY_HEALTHY_ACK_RTT {
                    if state.pressure != 0 {
                        state.pressure = state.pressure.saturating_sub(1);
                        state.healthy_ack_streak = 0;
                        state.healthy_since = None;
                    } else {
                        state.healthy_ack_streak = state.healthy_ack_streak.saturating_add(1);
                        state.healthy_since.get_or_insert(observation.observed_at);
                    }
                } else {
                    state.healthy_ack_streak = 0;
                    state.healthy_since = None;
                }
            }
        }
        let previous = state.current;
        if now.saturating_duration_since(state.last_evaluated_at) < JPEG_QUALITY_EVALUATION_INTERVAL
        {
            return JpegQualityDecision {
                quality: previous,
                epoch: state.epoch,
                previous,
                adjustment: None,
                signals: state.signals,
                accumulated_pressure: state.pressure,
            };
        }
        state.last_evaluated_at = now;
        let signals = std::mem::take(&mut state.signals);
        let accumulated_pressure = state.pressure;
        if state.pressure >= JPEG_QUALITY_PRESSURE_THRESHOLD {
            state.current = jpeg_quality_decrease(state.current, self.floor);
            state.pressure = 0;
            state.healthy_ack_streak = 0;
            state.healthy_since = None;
        } else if state.current < self.maximum
            && state.healthy_ack_streak >= self.recovery_ack_target
            && state.healthy_since.is_some_and(|since| {
                now.saturating_duration_since(since) >= JPEG_QUALITY_RECOVERY_DURATION
            })
        {
            state.current = state
                .current
                .saturating_add(JPEG_QUALITY_STEP_UP)
                .min(self.maximum);
            state.pressure = 0;
            state.healthy_ack_streak = 0;
            state.healthy_since = None;
        }
        let adjustment = match state.current.cmp(&previous) {
            std::cmp::Ordering::Less => Some(JpegQualityAdjustment::Decreased),
            std::cmp::Ordering::Greater => Some(JpegQualityAdjustment::Increased),
            std::cmp::Ordering::Equal => None,
        };
        if adjustment.is_some() {
            state.epoch = state.epoch.wrapping_add(1);
        }
        JpegQualityDecision {
            quality: state.current,
            epoch: state.epoch,
            previous,
            adjustment,
            signals,
            accumulated_pressure,
        }
    }
}

fn jpeg_quality_recovery_ack_target(frame_interval: Duration) -> u64 {
    let interval_nanos = frame_interval.as_nanos().max(1);
    let target = JPEG_QUALITY_RECOVERY_DURATION
        .as_nanos()
        .div_ceil(interval_nanos);
    u64::try_from(target).unwrap_or(u64::MAX).clamp(
        JPEG_QUALITY_MIN_RECOVERY_ACKS,
        JPEG_QUALITY_MAX_RECOVERY_ACKS,
    )
}

fn jpeg_quality_decrease(current: u8, floor: u8) -> u8 {
    let step = current.div_ceil(10).max(2);
    current.saturating_sub(step).max(floor)
}

struct FrameCreditState {
    last_sent: Option<u64>,
    last_acked: Option<u64>,
    in_flight: VecDeque<InFlightFrame>,
    closed: bool,
}

/// Cumulative display acknowledgements bound video work beyond the frame the
/// viewer has actually drawn, rather than merely what its TCP stack accepted.
struct FrameCredits {
    state: Mutex<FrameCreditState>,
    available: Condvar,
}

impl FrameCredits {
    fn new() -> Self {
        Self {
            state: Mutex::new(FrameCreditState {
                last_sent: None,
                last_acked: None,
                in_flight: VecDeque::with_capacity(MAX_OUTSTANDING_FRAMES as usize),
                closed: false,
            }),
            available: Condvar::new(),
        }
    }

    fn wait_for_credit(
        &self,
        running: &AtomicBool,
        timeout: Duration,
    ) -> io::Result<Option<Duration>> {
        let started = Instant::now();
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame credit lock was poisoned"))?;
        loop {
            if state.closed || !running.load(Ordering::Acquire) {
                return Ok(None);
            }
            if outstanding_frames(&state) < MAX_OUTSTANDING_FRAMES {
                return Ok(Some(started.elapsed()));
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote viewer stopped acknowledging displayed frames",
                ));
            }
            let (next_state, wait) = self
                .available
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("remote frame credit lock was poisoned"))?;
            state = next_state;
            if wait.timed_out()
                && !state.closed
                && running.load(Ordering::Acquire)
                && outstanding_frames(&state) >= MAX_OUTSTANDING_FRAMES
            {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote viewer stopped acknowledging displayed frames",
                ));
            }
        }
    }

    fn mark_sent(
        &self,
        sequence: u64,
        captured_at: Instant,
        sent_at: Instant,
        bytes: usize,
        jpeg_quality: u8,
        quality_epoch: u64,
    ) -> io::Result<u64> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame credit lock was poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "remote frame credits are closed",
            ));
        }
        let expected = state
            .last_sent
            .map_or(Some(0), |last| last.checked_add(1))
            .ok_or_else(|| io::Error::other("remote frame sequence exhausted"))?;
        if sequence != expected {
            return Err(io::Error::other(format!(
                "remote frame sender sequence mismatch: expected {expected}, got {sequence}"
            )));
        }
        if outstanding_frames(&state) >= MAX_OUTSTANDING_FRAMES {
            return Err(io::Error::other(
                "remote frame sender exceeded its display credits",
            ));
        }
        state.last_sent = Some(sequence);
        state.in_flight.push_back(InFlightFrame {
            sequence,
            captured_at,
            sent_at,
            bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
            jpeg_quality,
            quality_epoch,
        });
        Ok(outstanding_frames(&state))
    }

    #[cfg(test)]
    fn acknowledge(&self, sequence: u64) -> io::Result<Option<AckObservation>> {
        self.acknowledge_at(sequence, Instant::now())
    }

    #[cfg(test)]
    fn acknowledge_at(
        &self,
        sequence: u64,
        acknowledged_at: Instant,
    ) -> io::Result<Option<AckObservation>> {
        self.acknowledge_at_with_observer(sequence, acknowledged_at, |_| {})
    }

    fn acknowledge_at_with_observer(
        &self,
        sequence: u64,
        acknowledged_at: Instant,
        observer: impl FnOnce(AckObservation),
    ) -> io::Result<Option<AckObservation>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame credit lock was poisoned"))?;
        let last_sent = state.last_sent.ok_or_else(|| {
            invalid_data("remote viewer acknowledged a frame before one was sent")
        })?;
        if sequence > last_sent {
            return Err(invalid_data(format!(
                "remote viewer acknowledged future frame {sequence}; last sent was {last_sent}"
            )));
        }
        if state.last_acked.is_some_and(|last| sequence <= last) {
            return Ok(None);
        }
        let target_index = state
            .in_flight
            .iter()
            .position(|frame| frame.sequence == sequence)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "remote frame credit metadata is missing sequence {sequence}"
                ))
            })?;
        let target = state.in_flight[target_index];
        let retired = u64::try_from(target_index + 1).unwrap_or(u64::MAX);
        let same_epoch_retired = u64::try_from(
            state
                .in_flight
                .iter()
                .take(target_index + 1)
                .filter(|frame| frame.quality_epoch == target.quality_epoch)
                .count(),
        )
        .unwrap_or(u64::MAX);
        let same_epoch_outstanding_before = u64::try_from(
            state
                .in_flight
                .iter()
                .filter(|frame| frame.quality_epoch == target.quality_epoch)
                .count(),
        )
        .unwrap_or(u64::MAX);
        for _ in 0..=target_index {
            let _ = state.in_flight.pop_front();
        }
        state.last_acked = Some(sequence);
        let observation = AckObservation {
            retired,
            bytes: target.bytes,
            same_epoch_retired,
            same_epoch_outstanding_before,
            jpeg_quality: target.jpeg_quality,
            quality_epoch: target.quality_epoch,
            capture_to_ack: acknowledged_at.saturating_duration_since(target.captured_at),
            send_to_ack: acknowledged_at.saturating_duration_since(target.sent_at),
        };
        // The video sender can proceed as soon as this credit becomes visible.
        // Queue its quality feedback first, while the credit mutex excludes the
        // waiter; the observer only takes the independent scalar controller
        // lock, and no quality-controller path takes credits in reverse order.
        observer(observation);
        self.available.notify_all();
        Ok(Some(observation))
    }

    fn outstanding(&self) -> u64 {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        outstanding_frames(&state)
    }

    fn close(&self) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.closed = true;
        self.available.notify_all();
    }
}

fn outstanding_frames(state: &FrameCreditState) -> u64 {
    let Some(last_sent) = state.last_sent else {
        return 0;
    };
    match state.last_acked {
        Some(last_acked) => last_sent.saturating_sub(last_acked),
        None => last_sent.saturating_add(1),
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
    pub jpeg_quality_floor: u8,
    pub fixed_jpeg_quality: bool,
    pub max_width: u16,
    pub capture_source: CaptureSource,
    /// Which part of the root to share; the whole root by default.
    pub capture_area: CaptureArea,
    pub allow_lan: bool,
    pub allow_input: bool,
    pub allow_clipboard: bool,
    pub once: bool,
}

pub fn run_host(options: HostOptions) -> RemoteResult<()> {
    JpegQualityController::new_at(
        options.jpeg_quality,
        options.jpeg_quality_floor,
        !options.fixed_jpeg_quality,
        target_frame_interval(options.fps),
        Instant::now(),
    )?;
    let key = load_key_file(&options.key_file)?;
    validate_capture_area(options.display.as_deref(), &options.capture_area)?;
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
        // Take the address `accept` already resolved. Calling `peer_addr` on
        // the accepted socket instead is a liveness question, not a naming
        // one: a peer that sends RST before we get here makes it fail with
        // ENOTCONN, and propagating that killed the whole host. One
        // unauthenticated connect-then-reset from any port scanner was enough.
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) if accept_error_is_transient(&error) => {
                // Aborted connections and interrupted syscalls are routine.
                // Descriptor or memory exhaustion is transient too, but
                // spinning on it burns a core, so back off before retrying.
                if accept_error_needs_backoff(&error) {
                    eprintln!("jwm-remote: accept deferred ({error}); retrying");
                    thread::sleep(ACCEPT_EXHAUSTION_BACKOFF);
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        eprintln!("jwm-remote: connection from {peer}");
        let outcome = serve_client(stream, &key, &options, &shutdown);
        if let Err(error) = &outcome {
            eprintln!("jwm-remote: session with {peer} ended: {error}");
        }
        if options.once {
            // `--once` is documented for tests and scripts, so a failed
            // session has to be distinguishable from a clean one by exit code.
            return outcome;
        }
    }
    Ok(())
}

/// Accept errors that concern one connection rather than the listener.
fn accept_error_is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::NotConnected
    ) || accept_error_needs_backoff(error)
}

/// Resource exhaustion: retryable, but only after yielding for a moment.
fn accept_error_needs_backoff(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
    )
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
    reader.set_max_payload_len(MAX_INBOUND_PAYLOAD_LEN);
    let mut writer = SessionWriter::new(DeadlineWriter::new(stream, WRITE_TIMEOUT), send_key);

    let (kind, hello_payload) = reader.read_message()?;
    if kind != MessageKind::Hello {
        return Err(
            invalid_data("first authenticated client message must negotiate the session").into(),
        );
    }
    let hello = ClientHello::decode(&hello_payload)?;
    handshake_deadline.cancel();
    // Both sides must have opted in: the host's flag is policy, the client's
    // request is consent. Sending clipboard records to a peer that asked for
    // none would be a protocol violation on arrival.
    let clipboard_enabled = options.allow_clipboard && hello.request_clipboard;
    let mut capture = X11Capture::connect(
        options.display.as_deref(),
        options.capture_source,
        options.max_width,
        options.capture_area.clone(),
    )?;
    // Injected pointer coordinates arrive in shared-area space; XTEST wants
    // root coordinates. Resolved once here rather than per event.
    let input_origin = capture.origin();
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
            clipboard_enabled,
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

    let control = writer.get_ref().get_ref().try_clone()?;
    let running = Arc::new(AtomicBool::new(true));
    let first_stop = Arc::new(FirstStop::new());
    let credits = Arc::new(FrameCredits::new());
    let telemetry = Arc::new(HostTelemetry::new());
    let jpeg_quality = Arc::new(JpegQualityController::new_at(
        options.jpeg_quality,
        options.jpeg_quality_floor,
        !options.fixed_jpeg_quality,
        target_frame_interval(options.fps),
        Instant::now(),
    )?);
    if options.fixed_jpeg_quality {
        eprintln!("jwm-remote: JPEG quality fixed at {}", options.jpeg_quality);
    } else {
        eprintln!(
            "jwm-remote: adaptive JPEG quality active ({}..={})",
            options.jpeg_quality_floor, options.jpeg_quality
        );
    }
    // Clipboard sharing needs its own X connection and thread; the watcher
    // must not be able to delay a frame, and a conversion blocks until the
    // current selection owner answers.
    let (clipboard_captures, clipboard_setter) = if clipboard_enabled {
        match Clipboard::start(options.display.as_deref()) {
            Ok(clipboard) => {
                let (captures, setter) = clipboard.split();
                eprintln!("jwm-remote: clipboard sharing enabled");
                (Some(captures), Some(setter))
            }
            Err(error) => {
                eprintln!(
                    "jwm-remote: clipboard sharing unavailable ({error}); continuing without it"
                );
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    if clipboard_setter.is_some() {
        reader.set_max_payload_len(MAX_INBOUND_CLIPBOARD_PAYLOAD_LEN);
    }

    let input_running = Arc::clone(&running);
    let input_stop = Arc::clone(&first_stop);
    let input_credits = Arc::clone(&credits);
    let input_telemetry = Arc::clone(&telemetry);
    let input_jpeg_quality = Arc::clone(&jpeg_quality);
    let viewport = Arc::new(ViewportRequest::new());
    let input_viewport = Arc::clone(&viewport);
    let input_thread = thread::Builder::new()
        .name("jwm-remote-input".into())
        .spawn(move || {
            let _stop_on_exit = StopSessionOnDrop(Arc::clone(&input_running));
            receive_input(
                reader,
                injector,
                input_origin,
                keyboard_enabled,
                verified_keymap,
                INITIAL_ACTIVITY_TIMEOUT,
                SESSION_IDLE_TIMEOUT,
                &input_running,
                &input_stop,
                &input_credits,
                &input_telemetry,
                &input_jpeg_quality,
                &input_viewport,
                clipboard_setter.as_ref(),
            )
        })?;

    let stream_result = stream_frames(
        &viewport,
        clipboard_captures,
        &mut capture,
        writer,
        &control,
        &running,
        &first_stop,
        &credits,
        &telemetry,
        &jpeg_quality,
        shutdown,
        options,
    );
    running.store(false, Ordering::Release);
    let _ = control.shutdown(Shutdown::Both);
    let input_result = input_thread.join();
    report_host_final(&telemetry, &credits, Instant::now());
    let input_result = match input_result {
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

/// The session's single wire writer.
///
/// Records must not interleave and sequence numbers come from one writer, so
/// clipboard traffic shares the video sender's writer under a mutex rather
/// than opening a second one. The lock is held for exactly one record, and
/// clipboard records are rare and tiny.
type SharedWriter = Arc<Mutex<SessionWriter<DeadlineWriter<TcpStream>>>>;

fn stream_frames(
    viewport: &ViewportRequest,
    clipboard_captures: Option<ClipboardCaptures>,
    capture: &mut X11Capture,
    writer: SessionWriter<DeadlineWriter<TcpStream>>,
    control: &TcpStream,
    running: &Arc<AtomicBool>,
    first_stop: &Arc<FirstStop>,
    credits: &Arc<FrameCredits>,
    telemetry: &Arc<HostTelemetry>,
    jpeg_quality: &Arc<JpegQualityController>,
    shutdown: &AtomicBool,
    options: &HostOptions,
) -> RemoteResult<()> {
    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let sender_writer = Arc::clone(&writer);
    let mailbox = Arc::new(LatestMailbox::new());
    let sender_mailbox = Arc::clone(&mailbox);
    let sender_running = Arc::clone(running);
    let sender_stop = Arc::clone(first_stop);
    let sender_credits = Arc::clone(credits);
    let sender_telemetry = Arc::clone(telemetry);
    let sender_jpeg_quality = Arc::clone(jpeg_quality);
    let sender_control = match control.try_clone() {
        Ok(control) => control,
        Err(error) => {
            first_stop.record(StopCause::Sender);
            return Err(error.into());
        }
    };
    let sender = match thread::Builder::new()
        .name("jwm-remote-video".into())
        .spawn(move || {
            let _stop_on_exit = StopSessionOnDrop(Arc::clone(&sender_running));
            let result = send_frames(
                sender_mailbox,
                sender_writer,
                &sender_running,
                &sender_credits,
                &sender_telemetry,
                &sender_jpeg_quality,
            );
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

    let clipboard_thread: Option<std::io::Result<thread::JoinHandle<()>>> =
        clipboard_captures.map(|captures| {
            let writer = Arc::clone(&writer);
            let running = Arc::clone(running);
            thread::Builder::new()
                .name("jwm-remote-clipboard".into())
                .spawn(move || forward_clipboard(&captures, &writer, &running))
        });

    let interval = target_frame_interval(options.fps);
    let backpressure_refresh = backpressure_refresh_interval(interval);
    // `interval` is a rate limiter, not a schedule. Capturing on a fixed grid
    // made every interaction wait for the next grid point -- a mean of half an
    // interval, 42 ms at the default 12 fps -- for a tick that had nothing to
    // do with it. The loop now waits on the X connection instead and captures
    // as soon as the server reports something, never faster than `interval`.
    let mut next_capture_allowed = Instant::now();
    let mut reported_mode: Option<CaptureMode> = None;
    let capture_result = (|| -> RemoteResult<()> {
        while running.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
            let now = Instant::now();
            // Announce every capture-path transition, not just the first. Each
            // of these degradations is large and was previously visible only
            // as a single line that had long since scrolled away.
            if let Some(requested) = viewport.take() {
                let (width, height) = effective_encoded_bounds(options.max_width, requested);
                if capture.set_encoded_bounds(width, height) {
                    eprintln!("jwm-remote: encoding to fit {width}x{height} for this viewer");
                }
            }
            let mode = capture.mode();
            // Record every tick: the telemetry window resets on each report, so
            // recording only on change left later windows claiming a fully
            // degraded path that nothing had actually degraded.
            telemetry.record_capture_mode(mode);
            if reported_mode != Some(mode) {
                if reported_mode.is_some() {
                    eprintln!("jwm-remote: capture mode now {}", mode.describe());
                }
                reported_mode = Some(mode);
            }
            report_host_if_due(telemetry, credits, now);
            if now < next_capture_allowed {
                // Rate limited. Damage arriving now stays pending in the gate,
                // so nothing is lost by sleeping through it.
                thread::sleep((next_capture_allowed - now).min(CAPTURE_RATE_LIMIT_SLICE));
                continue;
            }
            // Bounded by one interval so that even if an event were somehow
            // missed, this degrades to exactly the old fixed-grid cadence
            // rather than stalling. The damage gate still decides whether the
            // wake is worth a readback, including its forced refresh and
            // cursor probing.
            capture.wait_for_activity(interval)?;
            if !running.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire) {
                break;
            }
            next_capture_allowed = Instant::now() + interval;
            telemetry.record_scheduled();

            match mailbox.capture_decision(backpressure_refresh)? {
                CaptureDecision::Capture => {}
                CaptureDecision::Skip => {
                    telemetry.record_skipped();
                    continue;
                }
                CaptureDecision::Closed => break,
            }
            let capture_started = Instant::now();
            let Some(frame) = captured_frame_from_outcome(capture.frame()?) else {
                telemetry.record_damage_skipped();
                continue;
            };
            telemetry.record_captured(capture_started.elapsed());
            let pending = PendingFrame {
                frame,
                captured_at: Instant::now(),
            };
            let Some(replaced) = mailbox.publish(pending) else {
                if !running.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire) {
                    break;
                }
                return Err(io::Error::other("remote frame sender stopped unexpectedly").into());
            };
            telemetry.record_published(replaced);
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
    credits.close();
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
    // The forwarder holds a reference to the shared writer, so it must not
    // outlive the session. It notices `running` within one poll interval.
    if let Some(Ok(clipboard_thread)) = clipboard_thread {
        let _ = clipboard_thread.join();
    }

    match first_stop.cause() {
        StopCause::Capture => capture_result,
        StopCause::Sender => sender_result,
        StopCause::Input | StopCause::Graceful => Ok(()),
        StopCause::ThreadPanic => sender_result,
        StopCause::None => capture_result.and(sender_result),
    }
}

fn captured_frame_from_outcome(outcome: CaptureOutcome) -> Option<CapturedFrame> {
    match outcome {
        CaptureOutcome::Frame(frame) => Some(frame),
        CaptureOutcome::NoChange => None,
    }
}

fn backpressure_refresh_interval(frame_interval: Duration) -> Duration {
    // Never slower than the requested rate allows: a fixed 250 ms floor turned
    // a 60 fps session's natural 50 ms refresh into fifteen frame-times of
    // added staleness while backpressured.
    let floor = MIN_BACKPRESSURE_REFRESH.min(frame_interval);
    frame_interval
        .saturating_mul(3)
        .clamp(floor, MAX_BACKPRESSURE_REFRESH)
}

fn target_frame_interval(fps: u16) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 60)))
}

fn send_frames<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
) -> RemoteResult<()> {
    let mut sender = FrameSender::new();
    send_frames_with_encoder(
        mailbox,
        writer,
        running,
        credits,
        telemetry,
        jpeg_quality,
        &mut sender,
    )
}

fn send_frames_with_encoder<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    sender: &mut FrameSender,
) -> RemoteResult<()> {
    let mut sequence = 0_u64;
    let mut payload = Vec::new();
    let mut payload_retention = PayloadBufferRetention::default();

    while running.load(Ordering::Acquire) {
        let Some(ack_wait) = credits.wait_for_credit(running, FRAME_ACK_TIMEOUT)? else {
            break;
        };
        let Some(pending) = mailbox.receive()? else {
            break;
        };
        telemetry.record_dequeued(pending.captured_at.elapsed(), ack_wait);
        if !running.load(Ordering::Acquire) {
            break;
        }
        let plan = sender.plan_at(&pending.frame, Instant::now())?;
        if !plan.emit {
            telemetry.record_unchanged_suppressed();
            continue;
        }
        let unchanged_keepalive = plan.dirty_tiles == 0;

        let quality = jpeg_quality.quality_before_encode_at(Instant::now());
        if let Some(adjustment) = quality.adjustment {
            let direction = match adjustment {
                JpegQualityAdjustment::Decreased => "decreased",
                JpegQualityAdjustment::Increased => "increased",
            };
            eprintln!(
                "jwm-remote: adaptive JPEG quality {direction} {} -> {} (ACKs {}, pressure {}, max send-to-ACK {:.1} ms, payload {} bytes, same-epoch outstanding {}, same-epoch-superseded {})",
                quality.previous,
                quality.quality,
                quality.signals.acknowledgements,
                quality.accumulated_pressure,
                quality.signals.max_ack_rtt.as_secs_f64() * 1000.0,
                quality.signals.max_payload_bytes,
                quality.signals.max_outstanding,
                quality.signals.same_epoch_superseded,
            );
        }
        let encode_started = Instant::now();
        sender
            .encoder
            .encode_into(&mut payload, sequence, &pending.frame, quality.quality)?;
        telemetry.record_encoded(encode_started.elapsed(), plan);
        // Publish the application sequence before the bytes reach the peer so
        // an immediate cumulative ACK can never race ahead of host state.
        let sent_at = Instant::now();
        let outstanding = credits.mark_sent(
            sequence,
            pending.captured_at,
            sent_at,
            payload.len(),
            quality.quality,
            quality.epoch,
        )?;
        telemetry.record_outstanding(outstanding);
        let write_started = Instant::now();
        // Held for exactly one record: interleaving would desynchronise the
        // authenticated stream, and clipboard traffic shares this writer.
        let write_result = {
            let mut writer = lock_writer(&writer);
            write_frame_record(&mut writer, &payload, FRAME_WRITE_DEADLINE)
        };
        if let Err(error) = write_result {
            // The viewer never applied these tiles, so the encoder's model of
            // its canvas must not advance past them.
            sender.discard();
            return Err(error.into());
        }
        telemetry.record_sent(payload.len(), write_started.elapsed());
        if unchanged_keepalive {
            telemetry.record_unchanged_keepalive();
        }
        // Commit by move only after the complete authenticated record and its
        // flush succeeded. A partial/failed write therefore cannot suppress a
        // future frame in a replacement session or test harness.
        sender.commit_at(&pending.frame, Instant::now());
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("remote frame sequence exhausted"))?;
        payload_retention.observe(&mut payload);
    }
    Ok(())
}

/// Lock the shared writer, tolerating a poisoned mutex.
///
/// A panicking writer thread already tears the session down through its own
/// stop path; refusing to write here as well would only turn one failure into
/// two different ones.
/// Forward locally copied text to the peer until the session ends.
///
/// Runs on its own thread because the video sender parks on display credit and
/// on its capture mailbox, either of which can be seconds long on an idle
/// desktop; queueing clipboard text behind that would make copy-paste feel
/// broken rather than merely delayed.
fn forward_clipboard<W: std::io::Write + SetWriteTimeout>(
    captures: &ClipboardCaptures,
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    running: &AtomicBool,
) {
    while running.load(Ordering::Acquire) {
        let Some(text) = captures.recv_timeout(CLIPBOARD_POLL_INTERVAL) else {
            continue;
        };
        let Ok(payload) = encode_clipboard(&text) else {
            eprintln!("jwm-remote: local clipboard text is too large to share; skipping it");
            continue;
        };
        let result = {
            let mut writer = lock_writer(writer);
            write_session_record(
                &mut writer,
                MessageKind::Clipboard,
                payload,
                FRAME_WRITE_DEADLINE,
            )
        };
        if let Err(error) = result {
            // The session owns teardown; stopping here is enough.
            eprintln!("jwm-remote: clipboard send failed: {error}");
            return;
        }
    }
}

fn lock_writer<W>(
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
) -> std::sync::MutexGuard<'_, SessionWriter<DeadlineWriter<W>>> {
    match writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_frame_record<W: std::io::Write + SetWriteTimeout>(
    writer: &mut SessionWriter<DeadlineWriter<W>>,
    payload: &[u8],
    timeout: Duration,
) -> Result<(), ProtocolError> {
    write_session_record(writer, MessageKind::Frame, payload, timeout)
}

/// Write one complete authenticated record under an absolute deadline.
fn write_session_record<W: std::io::Write + SetWriteTimeout>(
    writer: &mut SessionWriter<DeadlineWriter<W>>,
    kind: MessageKind,
    payload: &[u8],
    timeout: Duration,
) -> Result<(), ProtocolError> {
    writer.get_mut().begin_record(timeout)?;
    let result = writer
        .write_message(kind, payload)
        .and_then(|()| writer.flush());
    if let Err(error) = result {
        // A transport error may follow a partial header, payload, or MAC. The
        // authenticated record stream cannot be resynchronised, so make any
        // accidental retry fail closed and let the sender tear down both
        // halves of the session through its existing FirstStop path.
        writer.get_mut().poison_record();
        return Err(error);
    }
    writer.get_mut().finish_record()?;
    Ok(())
}

fn average_millis(duration: Duration, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        duration.as_secs_f64() * 1000.0 / count as f64
    }
}

fn report_host_if_due(telemetry: &HostTelemetry, credits: &FrameCredits, now: Instant) {
    let Some(snapshot) = telemetry.take_due_at(now) else {
        return;
    };
    let outstanding = credits.outstanding();
    print_host_telemetry(snapshot, outstanding);
}

fn report_host_final(telemetry: &HostTelemetry, credits: &FrameCredits, now: Instant) {
    let Some(snapshot) = telemetry.take_final_at(now) else {
        return;
    };
    let outstanding = credits.outstanding();
    print_host_telemetry(snapshot, outstanding);
}

fn print_host_telemetry(snapshot: HostTelemetrySnapshot, outstanding: u64) {
    let window = snapshot.window;
    let seconds = snapshot.elapsed.as_secs_f64();
    let sent_fps = if seconds == 0.0 {
        0.0
    } else {
        window.sent as f64 / seconds
    };
    let megabits_per_second = if seconds == 0.0 {
        0.0
    } else {
        window.bytes as f64 * 8.0 / seconds / 1_000_000.0
    };
    let tile_percent = if window.total_tiles == 0 {
        0.0
    } else {
        window.dirty_tiles as f64 * 100.0 / window.total_tiles as f64
    };
    eprintln!(
        "jwm-remote: host {:.1}s mode {} scheduled {} captured {} skipped {} damage-skipped {} published {} replaced {} dequeued {} unchanged-suppressed {} unchanged-keepalive {} encoded {} keyframes {} tiles {}/{} ({tile_percent:.1}% dirty) sent {} ({sent_fps:.1} fps, {megabits_per_second:.2} Mbit/s) bytes {} drawn-acks {} retired {} viewer-superseded {} drawn-bytes {}; avg capture/queue/credit-wait/encode/write/capture-to-ACK/send-to-ACK {:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1} ms; outstanding current/max {outstanding}/{}, max-queue {:.1} ms",
        seconds,
        window.capture_mode.describe(),
        window.scheduled,
        window.captured,
        window.skipped,
        window.damage_skipped,
        window.published,
        window.replaced,
        window.dequeued,
        window.unchanged_suppressed,
        window.unchanged_keepalive,
        window.encoded,
        window.keyframes,
        window.dirty_tiles,
        window.total_tiles,
        window.sent,
        window.bytes,
        window.drawn_acks,
        window.retired,
        window.viewer_superseded,
        window.drawn_bytes,
        average_millis(window.capture_elapsed, window.captured),
        average_millis(window.queue_elapsed, window.dequeued),
        average_millis(window.credit_wait_elapsed, window.dequeued),
        average_millis(window.encode_elapsed, window.encoded),
        average_millis(window.write_elapsed, window.sent),
        average_millis(window.capture_to_ack, window.drawn_acks),
        average_millis(window.send_to_ack, window.drawn_acks),
        window.max_outstanding,
        window.max_queue_age.as_secs_f64() * 1000.0,
    );
}

fn receive_input(
    mut reader: SessionReader<TcpStream>,
    mut injector: Option<InputInjector>,
    input_origin: (i16, i16),
    keyboard_enabled: bool,
    verified_keymap: Option<[u8; 32]>,
    initial_activity_timeout: Duration,
    session_idle_timeout: Duration,
    running: &AtomicBool,
    first_stop: &FirstStop,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    viewport: &ViewportRequest,
    clipboard: Option<&ClipboardSetter>,
) -> RemoteResult<()> {
    let mut payload = Vec::new();
    let session_result = (|| -> RemoteResult<()> {
        reader
            .get_ref()
            .set_read_timeout(Some(initial_activity_timeout))?;
        let mut awaiting_first_activity = true;
        while running.load(Ordering::Acquire) {
            // While the peer holds something down, wait for readability in a
            // short slice first and release on silence. Waiting for the socket
            // to become readable — rather than shortening the read timeout —
            // means a record is never interrupted part-way through, which the
            // authenticated stream could not resynchronise from.
            if injector.as_ref().is_some_and(InputInjector::has_pressed)
                && !wait_readable(reader.get_ref(), HELD_INPUT_SILENCE_TIMEOUT)?
            {
                if let Some(injector) = injector.as_mut() {
                    eprintln!(
                        "jwm-remote: controller silent for {} ms with input held; releasing keys and buttons",
                        HELD_INPUT_SILENCE_TIMEOUT.as_millis()
                    );
                    if let Err(error) = injector.release_all() {
                        eprintln!("jwm-remote: releasing held input failed: {error}");
                    }
                }
                continue;
            }
            let kind = reader.read_message_into(&mut payload)?;
            if awaiting_first_activity {
                reader
                    .get_ref()
                    .set_read_timeout(Some(session_idle_timeout))?;
                awaiting_first_activity = false;
            }
            match kind {
                MessageKind::InputBatch => {
                    let events = decode_input_batch(&payload)?;
                    let Some(injector) = injector.as_mut() else {
                        return Err(
                            invalid_data("input was not negotiated for this session").into()
                        );
                    };
                    let contains_keyboard = events
                        .iter()
                        .any(|event| matches!(event, super::x11_input::InputEvent::Key { .. }));
                    if contains_keyboard && !keyboard_enabled {
                        return Err(invalid_data("keyboard input was not negotiated").into());
                    }
                    if contains_keyboard {
                        verify_injector_keymap(injector, verified_keymap)?;
                    }
                    injector.inject_batch(&events, input_origin)?;
                }
                MessageKind::Pointer
                | MessageKind::Key
                | MessageKind::Button
                | MessageKind::ReleaseAll => {
                    return Err(invalid_data(
                        "legacy single-event input is invalid in application protocol v4",
                    )
                    .into());
                }
                MessageKind::Close => break,
                MessageKind::Heartbeat if payload.is_empty() => {
                    if keyboard_enabled && let Some(injector) = injector.as_mut() {
                        verify_injector_keymap(injector, verified_keymap)?;
                    }
                }
                MessageKind::Heartbeat => {
                    return Err(invalid_data("client heartbeat payload must be empty").into());
                }
                MessageKind::Clipboard => {
                    let text = decode_clipboard(&payload)?;
                    match clipboard.as_ref() {
                        Some(clipboard) => {
                            clipboard.set_text(&text);
                        }
                        // Negotiation already told the peer clipboard sharing
                        // is off; a record arriving anyway is a protocol
                        // violation, not something to silently absorb.
                        None => {
                            return Err(invalid_data("clipboard sharing was not negotiated").into());
                        }
                    }
                }
                MessageKind::Viewport => {
                    let (width, height) = decode_viewport(&payload)?;
                    viewport.request(width, height);
                }
                MessageKind::FrameAck => {
                    let acknowledged_at = Instant::now();
                    if let Some(ack) = credits.acknowledge_at_with_observer(
                        decode_frame_ack(&payload)?,
                        acknowledged_at,
                        |ack| jpeg_quality.observe_ack_at(ack, acknowledged_at),
                    )? {
                        telemetry.record_ack(ack);
                    }
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
    credits.close();

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
    injector: &mut InputInjector,
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

/// Wait for `stream` to become readable, returning false on timeout.
///
/// Errors and hangups count as readable so the following read reports them
/// through the normal path instead of being swallowed here.
fn wait_readable(stream: &TcpStream, timeout: Duration) -> RemoteResult<bool> {
    let fd = stream.as_fd();
    let interests = PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP;
    let mut fds = [PollFd::new(fd, interests)];
    let timeout = PollTimeout::try_from(timeout.as_millis().min(u128::from(u16::MAX)) as u16)
        .unwrap_or(PollTimeout::MAX);
    loop {
        return match poll(&mut fds, timeout) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(true),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => Err(io::Error::from(error).into()),
        };
    }
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
    use crate::remote::frame::new_decode_buffer_pool;
    use crate::remote::tile::TileDecoder;
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

    impl SetWriteTimeout for GateWriter {
        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

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

    struct DripWriter {
        bytes: Vec<u8>,
        max_chunk: usize,
        write_delay: Duration,
        flush_delay: Duration,
        flushes: usize,
    }

    impl SetWriteTimeout for DripWriter {
        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for DripWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            thread::sleep(self.write_delay);
            let written = buffer.len().min(self.max_chunk);
            self.bytes.extend_from_slice(&buffer[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            thread::sleep(self.flush_delay);
            Ok(())
        }
    }

    struct FailingWriter;

    impl SetWriteTimeout for FailingWriter {
        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected record failure",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FlushFailWriter {
        bytes: Vec<u8>,
    }

    impl SetWriteTimeout for FlushFailWriter {
        fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for FlushFailWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected flush failure",
            ))
        }
    }

    /// Plan, encode and commit one frame the way the sender loop does.
    ///
    /// Committing without encoding would silently do nothing, so tests must
    /// walk the same three steps as production.
    fn accept_frame(sender: &mut FrameSender, frame: &CapturedFrame, at: Instant) -> TilePlan {
        let plan = sender.plan_at(frame, at).expect("planning succeeds");
        if plan.emit {
            let mut payload = Vec::new();
            sender
                .encoder
                .encode_into(&mut payload, 0, frame, 70)
                .expect("encoding succeeds");
            sender.commit_at(frame, at);
        }
        plan
    }

    /// Decode a captured wire stream in order; deltas need their predecessors.
    fn decode_wire(decoder: &mut TileDecoder, payload: &[u8]) -> (u64, RgbImage) {
        let frame = decoder
            .decode_into(payload, new_decode_buffer_pool())
            .expect("host wire frames decode");
        (frame.sequence(), frame.image().clone())
    }

    #[test]
    fn a_viewport_request_can_only_narrow_the_operator_ceiling() {
        // A peer asking for more pixels than the operator allowed gets the
        // ceiling. Otherwise the viewer would dictate host CPU, readback size
        // and bandwidth, which is exactly what --max-width exists to bound.
        assert_eq!(effective_encoded_bounds(1280, (2560, 1440)).0, 1280);
        assert_eq!(effective_encoded_bounds(1280, (1280, 720)).0, 1280);
        assert_eq!(effective_encoded_bounds(1280, (640, 400)).0, 640);

        // Native capture has no ceiling, so any request narrows it.
        assert_eq!(effective_encoded_bounds(0, (1600, 900)).0, 1600);
        assert_eq!(effective_encoded_bounds(0, (16384, 900)).0, 16384);

        // The floors match the CLI's, so a peer cannot drive the encoder below
        // a size the rest of the pipeline supports.
        assert_eq!(
            effective_encoded_bounds(1280, (1, 1)),
            (MIN_VIEWPORT_WIDTH, MIN_VIEWPORT_HEIGHT)
        );
        assert_eq!(
            effective_encoded_bounds(0, (0, 0)),
            (MIN_VIEWPORT_WIDTH, MIN_VIEWPORT_HEIGHT)
        );
        // ...and the ceiling still wins when it is itself below the floor.
        assert_eq!(effective_encoded_bounds(320, (8, 8)).0, MIN_VIEWPORT_WIDTH);

        // Height is bounded by the viewer directly: it has no operator flag.
        assert_eq!(effective_encoded_bounds(1280, (900, 600)).1, 600);
    }

    #[test]
    fn viewport_requests_are_idempotent_and_consumed_once() {
        let viewport = ViewportRequest::new();
        assert_eq!(viewport.take(), None, "nothing requested yet");

        viewport.request(900, 600);
        assert_eq!(viewport.take(), Some((900, 600)));
        assert_eq!(viewport.take(), None, "a request is acted on once");

        // A newer request supersedes an unread one; the capture thread only
        // ever needs the latest size.
        viewport.request(800, 500);
        viewport.request(1024, 768);
        assert_eq!(viewport.take(), Some((1024, 768)));
    }

    #[test]
    fn per_connection_accept_errors_never_kill_the_listener() {
        // A peer that resets before we look at the socket makes peer_addr fail
        // with ENOTCONN; treating that as fatal let one unauthenticated packet
        // from a port scanner take down the whole host.
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
            io::ErrorKind::NotConnected,
        ] {
            let error = io::Error::new(kind, "synthetic");
            assert!(
                accept_error_is_transient(&error),
                "{kind:?} must not be fatal"
            );
            assert!(
                !accept_error_needs_backoff(&error),
                "{kind:?} needs no backoff"
            );
        }

        for raw in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            let error = io::Error::from_raw_os_error(raw);
            assert!(
                accept_error_is_transient(&error),
                "errno {raw} must not be fatal"
            );
            assert!(
                accept_error_needs_backoff(&error),
                "errno {raw} must back off rather than spin a core"
            );
        }

        // A genuinely broken listener still has to surface.
        let fatal = io::Error::from_raw_os_error(libc::EINVAL);
        assert!(!accept_error_is_transient(&fatal));
    }

    fn test_tile_plan(dirty_tiles: u32, total_tiles: u32) -> TilePlan {
        TilePlan {
            keyframe: dirty_tiles == total_tiles,
            dirty_tiles,
            total_tiles,
            emit: true,
        }
    }

    fn test_captured_frame(red: u8) -> CapturedFrame {
        CapturedFrame {
            image: RgbImage::from_pixel(2, 2, Rgb([red, 0, 0])),
            source_width: 2,
            source_height: 2,
        }
    }

    fn test_pending_frame(red: u8) -> PendingFrame {
        PendingFrame {
            frame: test_captured_frame(red),
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
            jpeg_quality_floor: 40,
            fixed_jpeg_quality: false,
            max_width: 1280,
            capture_source: CaptureSource::Auto,
            capture_area: CaptureArea::Root,
            allow_lan: false,
            allow_input: false,
            allow_clipboard: false,
            once: false,
        };
        assert!(!options.allow_lan);
        assert!(!options.allow_input);
    }

    #[test]
    fn an_unchanged_capture_sends_nothing_until_the_keepalive_boundary() {
        let base = Instant::now();
        let mut sender = FrameSender::new();
        let first = test_captured_frame(10);

        let plan = accept_frame(&mut sender, &first, base);
        assert!(
            plan.emit && plan.keyframe,
            "the first frame is self-contained"
        );

        for elapsed in [
            Duration::from_secs(1),
            Duration::from_secs(2),
            UNCHANGED_FRAME_KEEPALIVE - Duration::from_millis(1),
        ] {
            let plan = sender
                .plan_at(&test_captured_frame(10), base + elapsed)
                .unwrap();
            assert!(!plan.emit, "an unchanged capture has no dirty tiles");
            assert_eq!(plan.dirty_tiles, 0);
            assert_eq!(
                sender.committed_at,
                Some(base),
                "suppression must never refresh the keepalive clock"
            );
        }

        let plan = sender
            .plan_at(&test_captured_frame(10), base + UNCHANGED_FRAME_KEEPALIVE)
            .unwrap();
        assert!(
            plan.emit && plan.dirty_tiles == 0,
            "the boundary sends an empty liveness frame, not a whole picture"
        );
    }

    #[test]
    fn every_kind_of_change_marks_tiles_dirty() {
        let base = Instant::now();
        let mut sender = FrameSender::new();
        let first = test_captured_frame(10);
        accept_frame(&mut sender, &first, base);

        let mut raw_changed = test_captured_frame(10);
        raw_changed.image.as_mut()[0] = 200;
        let plan = sender
            .plan_at(&raw_changed, base + Duration::from_secs(1))
            .unwrap();
        assert!(plan.emit && !plan.keyframe && plan.dirty_tiles == 1);

        let mut source_changed = test_captured_frame(10);
        source_changed.source_width = 3;
        let plan = sender
            .plan_at(&source_changed, base + Duration::from_secs(1))
            .unwrap();
        assert!(
            plan.emit && plan.keyframe,
            "a changed root geometry invalidates the reference"
        );

        let image_geometry_changed = CapturedFrame {
            image: RgbImage::from_pixel(1, 4, Rgb([10, 0, 0])),
            source_width: 2,
            source_height: 2,
        };
        let plan = sender
            .plan_at(&image_geometry_changed, base + Duration::from_secs(1))
            .unwrap();
        assert!(
            plan.emit && plan.keyframe,
            "a resize invalidates the reference"
        );
    }

    #[test]
    fn an_uncommitted_send_attempt_cannot_change_the_viewer_model() {
        let base = Instant::now();
        let mut sender = FrameSender::new();
        let first = test_captured_frame(10);

        assert!(sender.plan_at(&first, base).unwrap().emit);
        // A failed first wire attempt never calls commit_at, so the next plan
        // must still be a self-contained keyframe.
        let plan = sender
            .plan_at(&first, base + Duration::from_secs(1))
            .unwrap();
        assert!(plan.emit && plan.keyframe);

        accept_frame(&mut sender, &first, base);
        let changed = test_captured_frame(20);
        assert!(
            sender
                .plan_at(&changed, base + Duration::from_secs(1))
                .unwrap()
                .emit
        );
        // Simulate a failed changed-frame write: the committed frame is still
        // what the viewer holds, so the old pixels are still unchanged...
        sender.discard();
        let plan = sender
            .plan_at(&test_captured_frame(10), base + Duration::from_secs(2))
            .unwrap();
        assert!(!plan.emit);
        // ...and the changed frame is still owed to the viewer.
        let plan = sender
            .plan_at(&changed, base + Duration::from_secs(2))
            .unwrap();
        assert!(plan.emit && plan.dirty_tiles == 1);
    }

    #[test]
    fn unchanged_keepalive_cadence_stays_inside_viewer_frame_idle_budget() {
        let scheduling_margin = Duration::from_secs(1);
        assert!(
            UNCHANGED_FRAME_KEEPALIVE
                + crate::remote::x11_capture::DAMAGE_FORCE_REFRESH
                + scheduling_margin
                < crate::remote::VIDEO_FRAME_IDLE_TIMEOUT
        );
    }

    #[test]
    fn damage_no_change_never_requeues_the_previous_image() {
        assert!(captured_frame_from_outcome(CaptureOutcome::NoChange).is_none());
        let frame = test_captured_frame(17);
        assert_eq!(
            captured_frame_from_outcome(CaptureOutcome::Frame(frame))
                .unwrap()
                .image
                .get_pixel(0, 0)
                .0,
            [17, 0, 0]
        );
    }

    #[test]
    fn damage_skips_are_activity_but_not_mailbox_skips() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        telemetry.record_damage_skipped();
        let window = telemetry
            .take_final_at(base + Duration::from_millis(1))
            .unwrap()
            .window;
        assert_eq!(window.damage_skipped, 1);
        assert_eq!(window.skipped, 0);
        assert_eq!(window.captured, 0);
    }

    #[test]
    fn latest_mailbox_replaces_stale_items_without_blocking() {
        let mailbox = LatestMailbox::new();
        assert_eq!(mailbox.publish(1_u64), Some(false));
        assert_eq!(mailbox.publish(2_u64), Some(true));
        assert_eq!(mailbox.receive().unwrap(), Some(2));
    }

    #[test]
    fn fresh_pending_frame_throttles_capture_until_the_sender_consumes_it() {
        let mailbox = LatestMailbox::new();
        assert_eq!(
            mailbox.capture_decision(Duration::from_secs(1)).unwrap(),
            CaptureDecision::Capture
        );
        assert_eq!(mailbox.publish(1_u64), Some(false));
        assert_eq!(
            mailbox.capture_decision(Duration::from_secs(1)).unwrap(),
            CaptureDecision::Skip
        );
        assert_eq!(
            mailbox.capture_decision(Duration::ZERO).unwrap(),
            CaptureDecision::Capture
        );
        assert_eq!(mailbox.receive().unwrap(), Some(1));
        assert_eq!(
            mailbox.capture_decision(Duration::from_secs(1)).unwrap(),
            CaptureDecision::Capture
        );
    }

    #[test]
    fn backpressure_refresh_scales_with_requested_frame_interval() {
        assert_eq!(
            backpressure_refresh_interval(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(500)),
            Duration::from_secs(1)
        );
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(125)),
            Duration::from_millis(375)
        );
        // The floor may never be slower than the requested rate itself.
        // Clamping every fast session up to a fixed 250 ms added fifteen
        // frame-times of staleness at 60 fps for no benefit.
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(83)),
            Duration::from_millis(249),
            "a 12 fps session keeps its natural three-frame refresh"
        );
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(16)),
            Duration::from_millis(48),
            "a 60 fps session refreshes at its own cadence, not a fixed floor"
        );
        for fps in 1..=60_u16 {
            let interval = target_frame_interval(fps);
            let refresh = backpressure_refresh_interval(interval);
            assert!(
                refresh >= interval,
                "{fps} fps: refresh {refresh:?} must not outpace the frame interval"
            );
            assert!(
                refresh <= MAX_BACKPRESSURE_REFRESH,
                "{fps} fps: refresh {refresh:?} must stay inside the staleness ceiling"
            );
        }
    }

    #[test]
    fn closing_mailbox_discards_pending_and_wakes_receiver() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(7_u64), Some(false));
        mailbox.close();
        assert_eq!(
            mailbox.capture_decision(Duration::ZERO).unwrap(),
            CaptureDecision::Closed
        );
        assert_eq!(mailbox.receive().unwrap(), None);
        assert_eq!(mailbox.publish(8), None);

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
    fn frame_credits_are_cumulative_bounded_and_validate_future_acks() {
        let credits = FrameCredits::new();
        let running = AtomicBool::new(true);
        let now = Instant::now();
        assert_eq!(credits.outstanding(), 0);
        credits.mark_sent(0, now, now, 100, 70, 0).unwrap();
        credits.mark_sent(1, now, now, 200, 70, 0).unwrap();
        assert_eq!(credits.outstanding(), MAX_OUTSTANDING_FRAMES);
        assert!(credits.mark_sent(2, now, now, 300, 70, 0).is_err());
        let error = credits
            .wait_for_credit(&running, Duration::from_millis(20))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        assert!(credits.acknowledge(0).unwrap().is_some());
        assert!(
            credits
                .wait_for_credit(&running, Duration::from_millis(20))
                .unwrap()
                .is_some()
        );
        credits.mark_sent(2, now, now, 300, 70, 0).unwrap();
        assert!(credits.acknowledge(0).unwrap().is_none());
        assert_eq!(credits.outstanding(), 2);
        assert!(credits.acknowledge(3).is_err());
        assert!(credits.acknowledge(2).unwrap().is_some());
        assert_eq!(credits.outstanding(), 0);
    }

    #[test]
    fn cumulative_ack_attributes_draw_and_rtt_only_to_its_target_frame() {
        let credits = FrameCredits::new();
        let base = Instant::now();
        credits
            .mark_sent(0, base, base + Duration::from_millis(10), 100, 70, 3)
            .unwrap();
        credits
            .mark_sent(
                1,
                base + Duration::from_millis(20),
                base + Duration::from_millis(30),
                200,
                65,
                4,
            )
            .unwrap();

        let ack = credits
            .acknowledge_at(1, base + Duration::from_millis(80))
            .unwrap()
            .unwrap();
        assert_eq!(ack.retired, 2);
        assert_eq!(ack.bytes, 200);
        assert_eq!(ack.same_epoch_retired, 1);
        assert_eq!(ack.same_epoch_outstanding_before, 1);
        assert_eq!(ack.jpeg_quality, 65);
        assert_eq!(ack.quality_epoch, 4);
        assert_eq!(ack.capture_to_ack, Duration::from_millis(60));
        assert_eq!(ack.send_to_ack, Duration::from_millis(50));
        assert_eq!(credits.outstanding(), 0);
        assert!(
            credits
                .acknowledge_at(1, base + Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn two_in_flight_old_epoch_acks_can_trigger_only_one_quality_drop() {
        let base = Instant::now();
        let credits = FrameCredits::new();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap();
        credits.mark_sent(0, base, base, 100, 70, 0).unwrap();
        credits
            .mark_sent(1, base, base + Duration::from_millis(10), 100, 70, 0)
            .unwrap();

        let first_ack = credits
            .acknowledge_at(0, base + Duration::from_millis(800))
            .unwrap()
            .unwrap();
        controller.observe_ack_at(first_ack, base + Duration::from_millis(800));
        let first = controller.quality_before_encode_at(base + Duration::from_secs(1));
        assert_eq!((first.quality, first.epoch), (70, 0));

        credits
            .mark_sent(
                2,
                base + Duration::from_secs(1),
                base + Duration::from_millis(1_100),
                100,
                first.quality,
                first.epoch,
            )
            .unwrap();
        let second_ack = credits
            .acknowledge_at(1, base + Duration::from_millis(1_300))
            .unwrap()
            .unwrap();
        controller.observe_ack_at(second_ack, base + Duration::from_millis(1_300));
        let lowered = controller.quality_before_encode_at(base + Duration::from_millis(1_500));
        assert_eq!((lowered.quality, lowered.epoch), (63, 1));

        let late_old_ack = credits
            .acknowledge_at(2, base + Duration::from_millis(1_800))
            .unwrap()
            .unwrap();
        assert_eq!(
            (late_old_ack.jpeg_quality, late_old_ack.quality_epoch),
            (70, 0)
        );
        controller.observe_ack_at(late_old_ack, base + Duration::from_millis(1_800));
        let ignored = controller.quality_before_encode_at(base + Duration::from_secs(2));
        assert_eq!((ignored.quality, ignored.epoch), (63, 1));
    }

    fn quality_ack(
        send_to_ack: Duration,
        bytes: u64,
        retired: u64,
        same_epoch_outstanding_before: u64,
        jpeg_quality: u8,
        quality_epoch: u64,
    ) -> AckObservation {
        AckObservation {
            retired,
            bytes,
            same_epoch_retired: retired,
            same_epoch_outstanding_before,
            jpeg_quality,
            quality_epoch,
            capture_to_ack: send_to_ack,
            send_to_ack,
        }
    }

    #[test]
    fn adaptive_quality_drops_fast_but_respects_interval_and_floor() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 62, true, Duration::from_millis(100), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 500_000, 1, 1, 70, 0),
            base + Duration::from_millis(100),
        );
        let early = controller.quality_before_encode_at(
            base + JPEG_QUALITY_EVALUATION_INTERVAL - Duration::from_nanos(1),
        );
        assert_eq!(early.quality, 70);
        assert_eq!(early.adjustment, None);

        let isolated = controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL);
        assert_eq!(isolated.quality, 70, "one hard RTT spike must not degrade");

        controller.observe_ack_at(
            quality_ack(Duration::from_millis(810), 500_000, 1, 1, 70, 0),
            base + Duration::from_millis(600),
        );

        let first =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 2);
        assert_eq!(first.quality, 63);
        assert_eq!(first.epoch, 1);
        assert_eq!(first.adjustment, Some(JpegQualityAdjustment::Decreased));

        controller.observe_ack_at(
            quality_ack(Duration::from_secs(1), 2_000_000, 2, 2, 63, 1),
            base + Duration::from_millis(1_100),
        );
        controller.observe_ack_at(
            quality_ack(Duration::from_secs(1), 2_000_000, 1, 1, 63, 1),
            base + Duration::from_millis(1_200),
        );
        let second =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 3);
        assert_eq!(second.quality, 62, "the decrease must clamp at the floor");

        controller.observe_ack_at(
            quality_ack(Duration::from_secs(1), 2_000_000, 2, 2, 62, 2),
            base + Duration::from_millis(1_600),
        );
        controller.observe_ack_at(
            quality_ack(Duration::from_secs(1), 2_000_000, 1, 1, 62, 2),
            base + Duration::from_millis(1_700),
        );
        let floor =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 4);
        assert_eq!(floor.quality, 62);
        assert_eq!(floor.adjustment, None);
    }

    #[test]
    fn adaptive_quality_recovers_at_one_fps_after_time_and_sample_boundaries() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_secs(1), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 200_000, 1, 1, 70, 0),
            base,
        );
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 200_000, 1, 1, 70, 0),
            base + Duration::from_millis(1),
        );
        assert_eq!(
            controller
                .quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL)
                .quality,
            63
        );

        assert_eq!(controller.recovery_ack_target, 4);
        for second in 1_u64..=3 {
            let observed_at = base + Duration::from_secs(second);
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(20), 3_000_000, 1, 1, 63, 1),
                observed_at,
            );
            let decision = controller.quality_before_encode_at(observed_at);
            assert_eq!(decision.quality, 63);
            assert_eq!(decision.adjustment, None);
        }
        let elapsed_but_short = controller.quality_before_encode_at(base + Duration::from_secs(4));
        assert_eq!(
            elapsed_but_short.quality, 63,
            "target - 1 ACKs cannot recover"
        );

        let fourth_at = base + Duration::from_millis(4_001);
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(20), 3_000_000, 1, 1, 63, 1),
            fourth_at,
        );
        let recovered = controller.quality_before_encode_at(base + Duration::from_millis(4_500));
        assert_eq!(recovered.quality, 64);
        assert_eq!(recovered.adjustment, Some(JpegQualityAdjustment::Increased));
    }

    #[test]
    fn adaptive_quality_uses_same_epoch_backlog_but_not_payload_alone() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(210), 4 * 1024 * 1024, 1, 1, 70, 0),
            base + Duration::from_millis(100),
        );
        let sparse = controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL);
        assert_eq!(
            sparse.quality, 70,
            "a large payload is diagnostic data, not a quality threshold"
        );

        for sample in 1_u32..=3 {
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(210), 100_000, 1, 2, 70, 0),
                base + JPEG_QUALITY_EVALUATION_INTERVAL * sample + Duration::from_millis(100),
            );
            let decision = controller
                .quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * (sample + 1));
            if sample < 3 {
                assert_eq!(decision.quality, 70);
            } else {
                assert_eq!(decision.quality, 63);
            }
        }
    }

    #[test]
    fn adaptive_quality_ignores_late_feedback_from_an_older_quality_epoch() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 200_000, 1, 1, 70, 0),
            base + Duration::from_millis(100),
        );
        let isolated = controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL);
        assert_eq!((isolated.quality, isolated.epoch), (70, 0));

        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 200_000, 1, 2, 70, 0),
            base + Duration::from_millis(600),
        );
        let lowered =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 2);
        assert_eq!((lowered.quality, lowered.epoch), (63, 1));

        controller.observe_ack_at(
            quality_ack(Duration::from_secs(2), 4_000_000, 1, 1, 70, 0),
            base + Duration::from_millis(1_100),
        );
        let ignored =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 3);
        assert_eq!((ignored.quality, ignored.epoch), (63, 1));

        for offset in [1_600, 1_700] {
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(800), 200_000, 1, 1, 63, 1),
                base + Duration::from_millis(offset),
            );
        }
        let current =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 4);
        assert_eq!((current.quality, current.epoch), (56, 2));
    }

    #[test]
    fn isolated_same_epoch_supersede_needs_a_second_pressure_sample() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(20), 100_000, 2, 2, 70, 0),
            base + Duration::from_millis(100),
        );
        let isolated = controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL);
        assert_eq!(isolated.quality, 70);

        controller.observe_ack_at(
            quality_ack(Duration::from_millis(20), 100_000, 2, 2, 70, 0),
            base + Duration::from_millis(600),
        );
        let repeated =
            controller.quality_before_encode_at(base + JPEG_QUALITY_EVALUATION_INTERVAL * 2);
        assert_eq!(repeated.quality, 63);
    }

    #[test]
    fn healthy_acks_first_decay_pressure_without_counting_toward_recovery() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_secs(1), base).unwrap();
        for offset in [100, 200] {
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(800), 100_000, 1, 1, 70, 0),
                base + Duration::from_millis(offset),
            );
        }
        let lowered = controller.quality_before_encode_at(base + Duration::from_millis(500));
        assert_eq!((lowered.quality, lowered.epoch), (63, 1));

        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 100_000, 1, 1, 63, 1),
            base + Duration::from_millis(600),
        );
        let pressured = controller.quality_before_encode_at(base + Duration::from_secs(1));
        assert_eq!(pressured.accumulated_pressure, 3);

        for offset in [1_100, 1_200, 1_300] {
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(20), 100_000, 1, 1, 63, 1),
                base + Duration::from_millis(offset),
            );
        }
        let decayed = controller.quality_before_encode_at(base + Duration::from_millis(1_500));
        assert_eq!(decayed.quality, 63);
        assert_eq!(decayed.accumulated_pressure, 0);
        let state = controller.state.lock().unwrap();
        assert_eq!(state.healthy_ack_streak, 0);
        assert!(state.healthy_since.is_none());
    }

    #[test]
    fn recovery_ack_target_scales_with_fps_and_is_bounded() {
        assert_eq!(jpeg_quality_recovery_ack_target(Duration::from_secs(1)), 4);
        assert_eq!(
            jpeg_quality_recovery_ack_target(Duration::from_millis(500)),
            6
        );
        assert_eq!(
            jpeg_quality_recovery_ack_target(Duration::from_millis(125)),
            24
        );
        assert_eq!(
            jpeg_quality_recovery_ack_target(Duration::from_millis(1)),
            24
        );
    }

    #[test]
    fn multiplicative_quality_decrease_respects_low_quality_and_floor_bounds() {
        assert_eq!(jpeg_quality_decrease(100, 40), 90);
        assert_eq!(jpeg_quality_decrease(40, 1), 36);
        assert_eq!(jpeg_quality_decrease(42, 40), 40);
        assert_eq!(jpeg_quality_decrease(40, 40), 40);
    }

    #[test]
    fn fixed_quality_ignores_all_feedback_and_invalid_ranges_fail() {
        let base = Instant::now();
        assert!(JpegQualityController::new_at(0, 1, true, Duration::from_secs(1), base).is_err());
        assert!(JpegQualityController::new_at(70, 71, true, Duration::from_secs(1), base).is_err());
        let controller =
            JpegQualityController::new_at(35, 35, false, Duration::from_secs(1), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_secs(10), u64::MAX, 2, 2, 35, 0),
            base + Duration::from_secs(1),
        );
        let decision = controller.quality_before_encode_at(base + Duration::from_secs(10));
        assert_eq!(decision.quality, 35);
        assert_eq!(decision.adjustment, None);
    }

    #[test]
    fn telemetry_reports_zero_send_windows_and_deduplicates_empty_final() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        assert!(
            telemetry
                .take_due_at(base + TELEMETRY_INTERVAL - Duration::from_nanos(1))
                .is_none()
        );
        let zero = telemetry
            .take_due_at(base + TELEMETRY_INTERVAL)
            .expect("the periodic reporter must emit an idle zero-send window");
        assert_eq!(zero.elapsed, TELEMETRY_INTERVAL);
        assert_eq!(zero.window.sent, 0);
        assert!(!zero.window.has_activity());
        assert!(
            telemetry.take_final_at(base + TELEMETRY_INTERVAL).is_none(),
            "cleanup must not duplicate a just-reported empty window"
        );

        telemetry.record_outstanding(1);
        let outstanding_snapshot = telemetry
            .take_final_at(base + TELEMETRY_INTERVAL + Duration::from_millis(1))
            .expect("an outstanding-only final window must not be discarded");
        assert_eq!(outstanding_snapshot.window.max_outstanding, 1);

        telemetry.record_scheduled();
        let final_snapshot = telemetry
            .take_final_at(base + TELEMETRY_INTERVAL + Duration::from_millis(2))
            .expect("unreported activity needs one final partial snapshot");
        assert_eq!(final_snapshot.window.scheduled, 1);
        assert!(
            telemetry
                .take_final_at(base + TELEMETRY_INTERVAL + Duration::from_millis(2))
                .is_none()
        );
    }

    #[test]
    fn telemetry_preserves_stage_counts_and_cross_window_ack_conservation() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        let credits = FrameCredits::new();

        for _ in 0..4 {
            telemetry.record_scheduled();
        }
        telemetry.record_skipped();
        telemetry.record_damage_skipped();
        for elapsed in [3, 4, 5] {
            telemetry.record_captured(Duration::from_millis(elapsed));
        }
        telemetry.record_published(false);
        telemetry.record_published(true);
        telemetry.record_published(false);
        telemetry.record_dequeued(Duration::from_millis(10), Duration::from_millis(1));
        telemetry.record_dequeued(Duration::from_millis(20), Duration::from_millis(2));
        telemetry.record_unchanged_suppressed();
        telemetry.record_unchanged_keepalive();
        telemetry.record_encoded(Duration::from_millis(6), test_tile_plan(1, 4));
        telemetry.record_encoded(Duration::from_millis(7), test_tile_plan(2, 4));

        let outstanding = credits
            .mark_sent(
                0,
                base + Duration::from_millis(1),
                base + Duration::from_millis(2),
                100,
                70,
                0,
            )
            .unwrap();
        telemetry.record_outstanding(outstanding);
        telemetry.record_sent(100, Duration::from_millis(8));
        let outstanding = credits
            .mark_sent(
                1,
                base + Duration::from_millis(2),
                base + Duration::from_millis(3),
                200,
                70,
                0,
            )
            .unwrap();
        telemetry.record_outstanding(outstanding);
        telemetry.record_sent(200, Duration::from_millis(9));

        let sent_window = telemetry
            .take_due_at(base + TELEMETRY_INTERVAL)
            .unwrap()
            .window;
        assert_eq!(sent_window.scheduled, 4);
        assert_eq!(sent_window.captured, 3);
        assert_eq!(sent_window.skipped, 1);
        assert_eq!(sent_window.damage_skipped, 1);
        assert_eq!(sent_window.published, 3);
        assert_eq!(sent_window.replaced, 1);
        assert_eq!(sent_window.dequeued, 2);
        assert_eq!(sent_window.unchanged_suppressed, 1);
        assert_eq!(sent_window.unchanged_keepalive, 1);
        assert_eq!(sent_window.encoded, 2);
        assert_eq!(sent_window.sent, 2);
        assert_eq!(sent_window.bytes, 300);
        assert_eq!(sent_window.capture_elapsed, Duration::from_millis(12));
        assert_eq!(sent_window.queue_elapsed, Duration::from_millis(30));
        assert_eq!(sent_window.credit_wait_elapsed, Duration::from_millis(3));
        assert_eq!(sent_window.encode_elapsed, Duration::from_millis(13));
        assert_eq!(sent_window.write_elapsed, Duration::from_millis(17));
        assert_eq!(sent_window.max_outstanding, 2);
        assert_eq!(sent_window.max_queue_age, Duration::from_millis(20));

        let ack = credits
            .acknowledge_at(1, base + Duration::from_secs(6))
            .unwrap()
            .unwrap();
        telemetry.record_ack(ack);
        let ack_window = telemetry
            .take_due_at(base + TELEMETRY_INTERVAL * 2)
            .unwrap()
            .window;
        assert_eq!(ack_window.drawn_acks, 1);
        assert_eq!(ack_window.retired, sent_window.sent);
        assert_eq!(ack_window.viewer_superseded, 1);
        assert_eq!(ack_window.drawn_bytes, 200);
        assert_eq!(ack_window.capture_to_ack, Duration::from_millis(5_998));
        assert_eq!(ack_window.send_to_ack, Duration::from_millis(5_997));
        assert_eq!(credits.outstanding(), 0);
    }

    #[test]
    fn closing_frame_credits_wakes_a_waiting_sender() {
        let credits = Arc::new(FrameCredits::new());
        let now = Instant::now();
        credits.mark_sent(0, now, now, 100, 70, 0).unwrap();
        credits.mark_sent(1, now, now, 100, 70, 0).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let waiting_credits = Arc::clone(&credits);
        let waiting_running = Arc::clone(&running);
        let waiter = thread::spawn(move || {
            waiting_credits
                .wait_for_credit(&waiting_running, Duration::from_secs(1))
                .unwrap()
        });
        thread::sleep(Duration::from_millis(20));
        credits.close();
        assert_eq!(waiter.join().unwrap(), None);
    }

    #[test]
    fn credit_feedback_is_visible_before_a_cumulative_ack_wakes_the_sender() {
        let base = Instant::now();
        let credits = Arc::new(FrameCredits::new());
        credits.mark_sent(0, base, base, 100, 70, 0).unwrap();
        credits
            .mark_sent(1, base, base + Duration::from_millis(10), 100, 70, 0)
            .unwrap();
        let controller = Arc::new(
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap(),
        );
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 100, 1, 1, 70, 0),
            base + Duration::from_millis(100),
        );
        let seed = controller.quality_before_encode_at(base + Duration::from_millis(500));
        assert_eq!((seed.quality, seed.accumulated_pressure), (70, 3));

        let observed = Arc::new(AtomicBool::new(false));
        let waiter_credits = Arc::clone(&credits);
        let waiter_controller = Arc::clone(&controller);
        let waiter_observed = Arc::clone(&observed);
        let running = Arc::new(AtomicBool::new(true));
        let waiter_running = Arc::clone(&running);
        let waiter = thread::spawn(move || {
            assert!(
                waiter_credits
                    .wait_for_credit(&waiter_running, Duration::from_secs(1))
                    .unwrap()
                    .is_some()
            );
            assert!(
                waiter_observed.load(Ordering::Acquire),
                "credit became visible before its quality observation"
            );
            waiter_controller.quality_before_encode_at(base + Duration::from_secs(1))
        });

        thread::sleep(Duration::from_millis(20));
        let observer_controller = Arc::clone(&controller);
        let observer_flag = Arc::clone(&observed);
        let ack = credits
            .acknowledge_at_with_observer(1, base + Duration::from_millis(900), move |ack| {
                observer_controller.observe_ack_at(ack, base + Duration::from_millis(900));
                observer_flag.store(true, Ordering::Release);
            })
            .unwrap()
            .unwrap();
        assert_eq!(ack.retired, 2);
        let decision = waiter.join().unwrap();
        assert_eq!((decision.quality, decision.epoch), (63, 1));
    }

    #[test]
    fn display_credits_bound_wire_frames_and_keep_latest_sequence_continuous() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
        let running = Arc::new(AtomicBool::new(true));
        let credits = Arc::new(FrameCredits::new());
        let telemetry = Arc::new(HostTelemetry::new());
        let sender_mailbox = Arc::clone(&mailbox);
        let sender_running = Arc::clone(&running);
        let sender_credits = Arc::clone(&credits);
        let sender_telemetry = Arc::clone(&telemetry);
        let jpeg_quality = Arc::new(
            JpegQualityController::new_at(
                100,
                100,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap(),
        );
        let sender_jpeg_quality = Arc::clone(&jpeg_quality);
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0x5a; 32],
        )));
        let sender = thread::spawn(move || {
            send_frames(
                sender_mailbox,
                writer,
                &sender_running,
                &sender_credits,
                &sender_telemetry,
                &sender_jpeg_quality,
            )
        });

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

        assert_eq!(mailbox.publish(test_pending_frame(20)), Some(false));
        assert_eq!(mailbox.publish(test_pending_frame(30)), Some(true));
        assert_eq!(mailbox.publish(test_pending_frame(40)), Some(true));

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
        drop(state);

        assert_eq!(mailbox.publish(test_pending_frame(50)), Some(false));
        assert_eq!(mailbox.publish(test_pending_frame(60)), Some(true));
        assert_eq!(mailbox.publish(test_pending_frame(70)), Some(true));
        let state = lock.lock().unwrap();
        let (state, timeout) = ready
            .wait_timeout_while(state, Duration::from_millis(60), |state| state.flushes < 3)
            .unwrap();
        assert!(timeout.timed_out());
        assert_eq!(state.flushes, 2, "sender exceeded its display credits");
        drop(state);

        assert!(credits.acknowledge(1).unwrap().is_some());
        let state = lock.lock().unwrap();
        let (state, timeout) = ready
            .wait_timeout_while(state, Duration::from_secs(2), |state| state.flushes < 3)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "sender did not resume after frame ACK"
        );
        let wire = state.bytes.clone();
        drop(state);

        running.store(false, Ordering::Release);
        mailbox.close();
        credits.close();
        sender.join().unwrap().unwrap();

        let mut reader = SessionReader::new(Cursor::new(wire), [0x5a; 32]);
        let (first_kind, first_payload) = reader.read_message().unwrap();
        let (second_kind, second_payload) = reader.read_message().unwrap();
        let (third_kind, third_payload) = reader.read_message().unwrap();
        assert_eq!(first_kind, MessageKind::Frame);
        assert_eq!(second_kind, MessageKind::Frame);
        assert_eq!(third_kind, MessageKind::Frame);
        let mut decoder = TileDecoder::new();
        assert_eq!(decode_wire(&mut decoder, &first_payload).0, 0);
        assert_eq!(decode_wire(&mut decoder, &second_payload).0, 1);
        let (sequence, image) = decode_wire(&mut decoder, &third_payload);
        assert_eq!(sequence, 2);
        assert!(image.get_pixel(0, 0).0[0].abs_diff(70) <= 2);
    }

    #[test]
    fn unchanged_sender_frames_skip_quality_encoding_credit_and_sequence() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let gate = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..GateState::default()
            }),
            Condvar::new(),
        ));
        let running = Arc::new(AtomicBool::new(true));
        let credits = Arc::new(FrameCredits::new());
        let telemetry = Arc::new(HostTelemetry::new());
        let base = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the monotonic clock must cover the quality interval");
        let jpeg_quality = Arc::new(
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(100), base).unwrap(),
        );
        let sender_mailbox = Arc::clone(&mailbox);
        let sender_running = Arc::clone(&running);
        let sender_credits = Arc::clone(&credits);
        let sender_telemetry = Arc::clone(&telemetry);
        let sender_quality = Arc::clone(&jpeg_quality);
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0x6b; 32],
        )));
        let sender = thread::spawn(move || {
            send_frames(
                sender_mailbox,
                writer,
                &sender_running,
                &sender_credits,
                &sender_telemetry,
                &sender_quality,
            )
        });

        let (lock, ready) = &*gate;
        let state = lock.lock().unwrap();
        let (state, timeout) = ready
            .wait_timeout_while(state, Duration::from_secs(2), |state| state.flushes < 1)
            .unwrap();
        assert!(!timeout.timed_out(), "first frame did not reach the wire");
        drop(state);
        credits.acknowledge(0).unwrap().unwrap();

        // If quality_before_encode_at runs, these two hard samples are removed
        // from the pending queue and cross the controller's pressure threshold.
        let observed_at = Instant::now();
        for _ in 0..2 {
            jpeg_quality.observe_ack_at(
                quality_ack(Duration::from_millis(800), 100, 1, 1, 70, 0),
                observed_at,
            );
        }
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let suppressed = telemetry.state.lock().unwrap().window.unchanged_suppressed;
            if suppressed == 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "duplicate frame was not suppressed"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(lock.lock().unwrap().flushes, 1);
        assert_eq!(credits.outstanding(), 0);
        {
            let mut state = jpeg_quality.state.lock().unwrap();
            assert_eq!(state.current, 70);
            assert_eq!(state.pending_acks.len(), 2);
            // Keep this test about whether the quality lease was read, not
            // about the controller's separately-tested evaluation cadence.
            state.last_evaluated_at = Instant::now() + Duration::from_secs(60);
        }

        assert_eq!(mailbox.publish(test_pending_frame(20)), Some(false));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if telemetry.state.lock().unwrap().window.sent == 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "changed frame did not reach the wire"
            );
            thread::sleep(Duration::from_millis(1));
        }
        {
            let state = credits.state.lock().unwrap();
            assert_eq!(state.in_flight.len(), 1);
            assert_eq!(state.in_flight[0].sequence, 1);
            assert_eq!(state.in_flight[0].jpeg_quality, 70);
        }
        {
            let state = jpeg_quality.state.lock().unwrap();
            assert!(state.pending_acks.is_empty());
            assert_eq!(state.pressure, JPEG_QUALITY_PRESSURE_THRESHOLD);
        }

        let wire = lock.lock().unwrap().bytes.clone();
        running.store(false, Ordering::Release);
        mailbox.close();
        credits.close();
        sender.join().unwrap().unwrap();

        let mut reader = SessionReader::new(Cursor::new(wire), [0x6b; 32]);
        let (_, first_payload) = reader.read_message().unwrap();
        let (_, second_payload) = reader.read_message().unwrap();
        let mut decoder = TileDecoder::new();
        assert_eq!(decode_wire(&mut decoder, &first_payload).0, 0);
        let (sequence, image) = decode_wire(&mut decoder, &second_payload);
        assert_eq!(sequence, 1);
        assert!(image.get_pixel(0, 0).0[0].abs_diff(20) <= 2);
        let window = telemetry.state.lock().unwrap().window;
        assert_eq!(window.dequeued, 3);
        assert_eq!(window.unchanged_suppressed, 1);
        assert_eq!(window.encoded, 2);
        assert_eq!(window.sent, 2);
    }

    #[test]
    fn flush_failure_does_not_commit_keepalive_or_dedup_cache() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let running = AtomicBool::new(true);
        let credits = FrameCredits::new();
        let telemetry = HostTelemetry::new();
        let jpeg_quality = JpegQualityController::new_at(
            70,
            70,
            false,
            Duration::from_millis(100),
            Instant::now(),
        )
        .unwrap();
        let committed_at = Instant::now()
            .checked_sub(UNCHANGED_FRAME_KEEPALIVE + Duration::from_millis(1))
            .expect("the monotonic clock must cover the keepalive interval");
        let mut sender = FrameSender::new();
        let baseline = test_captured_frame(10);
        accept_frame(&mut sender, &baseline, committed_at);
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(FlushFailWriter::default(), WRITE_TIMEOUT),
            [0x7c; 32],
        )));

        assert!(
            send_frames_with_encoder(
                mailbox,
                writer,
                &running,
                &credits,
                &telemetry,
                &jpeg_quality,
                &mut sender,
            )
            .is_err()
        );
        assert_eq!(sender.committed_at, Some(committed_at));
        assert!(
            !sender
                .plan_at(&test_captured_frame(10), committed_at)
                .unwrap()
                .emit,
            "a failed keepalive write must leave the viewer model exactly as it was"
        );
        let window = telemetry.state.lock().unwrap().window;
        assert_eq!(window.encoded, 1);
        assert_eq!(window.sent, 0);
        assert_eq!(window.unchanged_keepalive, 0);
        assert_eq!(window.unchanged_suppressed, 0);
        assert_eq!(credits.outstanding(), 1);
    }

    #[test]
    fn write_failure_does_not_create_a_dedup_baseline() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let running = AtomicBool::new(true);
        let credits = FrameCredits::new();
        let telemetry = HostTelemetry::new();
        let jpeg_quality = JpegQualityController::new_at(
            70,
            70,
            false,
            Duration::from_millis(100),
            Instant::now(),
        )
        .unwrap();
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(FailingWriter, WRITE_TIMEOUT),
            [0x8d; 32],
        )));
        let mut sender = FrameSender::new();

        assert!(
            send_frames_with_encoder(
                mailbox,
                writer,
                &running,
                &credits,
                &telemetry,
                &jpeg_quality,
                &mut sender,
            )
            .is_err()
        );
        assert!(
            sender.committed_at.is_none(),
            "a frame that never reached the wire cannot become the viewer model"
        );
        assert!(
            sender
                .plan_at(&test_captured_frame(10), Instant::now())
                .unwrap()
                .keyframe,
            "the next attempt must still be self-contained"
        );
        let window = telemetry.state.lock().unwrap().window;
        assert_eq!(window.encoded, 1);
        assert_eq!(window.sent, 0);
        assert_eq!(window.unchanged_keepalive, 0);
        assert_eq!(window.unchanged_suppressed, 0);
    }

    #[test]
    fn absolute_record_deadline_stops_a_slow_drip_and_poison_prevents_retry() {
        let drip = DripWriter {
            bytes: Vec::new(),
            max_chunk: 1,
            write_delay: Duration::from_millis(8),
            flush_delay: Duration::ZERO,
            flushes: 0,
        };
        let mut writer = SessionWriter::new(
            DeadlineWriter::new(drip, Duration::from_secs(1)),
            [0x6b; 32],
        );
        let started = Instant::now();
        let error = write_frame_record(&mut writer, b"slow", Duration::from_millis(35))
            .expect_err("slow-drip record must exceed its absolute deadline");
        assert!(matches!(
            error,
            ProtocolError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(writer.next_sequence(), 0);
        let partial_len = writer.get_ref().get_ref().bytes.len();
        assert!(partial_len > 0);
        assert!(partial_len < 13 + b"slow".len() + 32);

        let retry = write_frame_record(&mut writer, b"retry", Duration::from_secs(1))
            .expect_err("a partial authenticated record must never be retried");
        assert!(matches!(
            retry,
            ProtocolError::Io(ref error) if error.kind() == io::ErrorKind::BrokenPipe
        ));
        assert_eq!(writer.get_ref().get_ref().bytes.len(), partial_len);
    }

    #[test]
    fn absolute_record_deadline_includes_flush_after_all_record_writes() {
        let drip = DripWriter {
            bytes: Vec::new(),
            max_chunk: usize::MAX,
            write_delay: Duration::from_millis(8),
            flush_delay: Duration::from_millis(30),
            flushes: 0,
        };
        let mut writer = SessionWriter::new(
            DeadlineWriter::new(drip, Duration::from_secs(1)),
            [0x7c; 32],
        );
        // One 8 ms write now carries the whole record, so the budget has to be
        // tight enough that only the 30 ms flush can overrun it.
        let error = write_frame_record(&mut writer, b"frame", Duration::from_millis(20))
            .expect_err("flush must share the record's absolute deadline");
        assert!(matches!(
            error,
            ProtocolError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert_eq!(writer.get_ref().get_ref().flushes, 1);
        assert_eq!(writer.next_sequence(), 1);
    }

    #[test]
    fn absolute_deadline_interrupts_a_blocked_loopback_write() {
        let (stream, _peer) = loopback_pair();
        let mut writer = DeadlineWriter::new(stream, Duration::from_secs(1));
        writer.begin_record(Duration::from_millis(60)).unwrap();
        // This is intentionally larger than loopback's bounded socket queues.
        // The peer keeps the connection open without reading, so the write
        // eventually blocks in the kernel and must use the remaining record
        // budget rather than a fresh timeout for every partial write.
        let payload = vec![0x5a; 16 * 1024 * 1024];
        let started = Instant::now();
        let error = writer
            .write_all(&payload)
            .expect_err("an unread loopback peer must hit the absolute deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
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
        let credits = Arc::new(FrameCredits::new());
        let receiver_credits = Arc::clone(&credits);
        let telemetry = Arc::new(HostTelemetry::new());
        let receiver_telemetry = Arc::clone(&telemetry);
        let jpeg_quality = Arc::new(
            JpegQualityController::new_at(
                70,
                70,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap(),
        );
        let receiver_jpeg_quality = Arc::clone(&jpeg_quality);
        let receiver = thread::spawn(move || {
            receive_input(
                reader,
                None,
                (0, 0),
                false,
                None,
                Duration::from_millis(600),
                Duration::from_millis(60),
                &receiver_running,
                &receiver_stop,
                &receiver_credits,
                &receiver_telemetry,
                &receiver_jpeg_quality,
                &ViewportRequest::new(),
                None,
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
