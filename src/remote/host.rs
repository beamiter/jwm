//! Trusted-LAN remote desktop host.

use super::RemoteResult;
use super::deadline::TcpStreamDeadline;
use super::frame::encode_frame_into;
use super::key::load_key_file;
use super::messages::{ClientHello, ServerHello, decode_frame_ack, decode_input_batch};
use super::protocol::{
    MessageKind, PayloadBufferRetention, ProtocolError, SessionReader, SessionWriter,
    server_handshake,
};
use super::x11_capture::{CaptureSource, CapturedFrame, X11Capture};
use super::x11_input::InputInjector;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::collections::VecDeque;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
const FRAME_ACK_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OUTSTANDING_FRAMES: u64 = 2;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HostTelemetryWindow {
    scheduled: u64,
    captured: u64,
    skipped: u64,
    published: u64,
    replaced: u64,
    dequeued: u64,
    encoded: u64,
    sent: u64,
    bytes: u64,
    drawn_acks: u64,
    drawn_bytes: u64,
    retired: u64,
    viewer_superseded: u64,
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
            || self.published != 0
            || self.replaced != 0
            || self.dequeued != 0
            || self.encoded != 0
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

    fn record_encoded(&self, elapsed: Duration) {
        self.update(|window| {
            window.encoded = window.encoded.saturating_add(1);
            window.encode_elapsed = window.encode_elapsed.saturating_add(elapsed);
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
    viewer_superseded: u64,
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
        self.viewer_superseded = self
            .viewer_superseded
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
    pub allow_lan: bool,
    pub allow_input: bool,
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
    let mut writer = SessionWriter::new(DeadlineWriter::new(stream, WRITE_TIMEOUT), send_key);

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
    let input_running = Arc::clone(&running);
    let input_stop = Arc::clone(&first_stop);
    let input_credits = Arc::clone(&credits);
    let input_telemetry = Arc::clone(&telemetry);
    let input_jpeg_quality = Arc::clone(&jpeg_quality);
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
                &input_credits,
                &input_telemetry,
                &input_jpeg_quality,
            )
        })?;

    let stream_result = stream_frames(
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

fn stream_frames(
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
                writer,
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

    let interval = target_frame_interval(options.fps);
    let backpressure_refresh = backpressure_refresh_interval(interval);
    let mut next_frame = Instant::now();
    let capture_result = (|| -> RemoteResult<()> {
        while running.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
            let now = Instant::now();
            report_host_if_due(telemetry, credits, now);
            if now < next_frame {
                thread::sleep((next_frame - now).min(Duration::from_millis(20)));
                continue;
            }
            next_frame += interval;
            if next_frame < now {
                next_frame = now + interval;
            }
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
            let frame = capture.frame()?;
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

    match first_stop.cause() {
        StopCause::Capture => capture_result,
        StopCause::Sender => sender_result,
        StopCause::Input | StopCause::Graceful => Ok(()),
        StopCause::ThreadPanic => sender_result,
        StopCause::None => capture_result.and(sender_result),
    }
}

fn backpressure_refresh_interval(frame_interval: Duration) -> Duration {
    frame_interval
        .saturating_mul(3)
        .clamp(MIN_BACKPRESSURE_REFRESH, MAX_BACKPRESSURE_REFRESH)
}

fn target_frame_interval(fps: u16) -> Duration {
    Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 60)))
}

fn send_frames<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    mut writer: SessionWriter<DeadlineWriter<W>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
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

        let quality = jpeg_quality.quality_before_encode_at(Instant::now());
        if let Some(adjustment) = quality.adjustment {
            let direction = match adjustment {
                JpegQualityAdjustment::Decreased => "decreased",
                JpegQualityAdjustment::Increased => "increased",
            };
            eprintln!(
                "jwm-remote: adaptive JPEG quality {direction} {} -> {} (ACKs {}, pressure {}, max send-to-ACK {:.1} ms, payload {} bytes, same-epoch outstanding {}, viewer-superseded {})",
                quality.previous,
                quality.quality,
                quality.signals.acknowledgements,
                quality.accumulated_pressure,
                quality.signals.max_ack_rtt.as_secs_f64() * 1000.0,
                quality.signals.max_payload_bytes,
                quality.signals.max_outstanding,
                quality.signals.viewer_superseded,
            );
        }
        let encode_started = Instant::now();
        encode_frame_into(&mut payload, sequence, &pending.frame, quality.quality)?;
        telemetry.record_encoded(encode_started.elapsed());
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
        write_frame_record(&mut writer, &payload, FRAME_WRITE_DEADLINE)?;
        telemetry.record_sent(payload.len(), write_started.elapsed());
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("remote frame sequence exhausted"))?;
        payload_retention.observe(&mut payload);
    }
    Ok(())
}

fn write_frame_record<W: std::io::Write + SetWriteTimeout>(
    writer: &mut SessionWriter<DeadlineWriter<W>>,
    payload: &[u8],
    timeout: Duration,
) -> Result<(), ProtocolError> {
    writer.get_mut().begin_record(timeout)?;
    let result = writer
        .write_message(MessageKind::Frame, payload)
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
    eprintln!(
        "jwm-remote: host {:.1}s scheduled {} captured {} skipped {} published {} replaced {} dequeued {} encoded {} sent {} ({sent_fps:.1} fps, {megabits_per_second:.2} Mbit/s) bytes {} drawn-acks {} retired {} viewer-superseded {} drawn-bytes {}; avg capture/queue/credit-wait/encode/write/capture-to-ACK/send-to-ACK {:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1} ms; outstanding current/max {outstanding}/{}, max-queue {:.1} ms",
        seconds,
        window.scheduled,
        window.captured,
        window.skipped,
        window.published,
        window.replaced,
        window.dequeued,
        window.encoded,
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
    keyboard_enabled: bool,
    verified_keymap: Option<[u8; 32]>,
    initial_activity_timeout: Duration,
    session_idle_timeout: Duration,
    running: &AtomicBool,
    first_stop: &FirstStop,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
) -> RemoteResult<()> {
    let mut payload = Vec::new();
    let session_result = (|| -> RemoteResult<()> {
        reader
            .get_ref()
            .set_read_timeout(Some(initial_activity_timeout))?;
        let mut awaiting_first_activity = true;
        while running.load(Ordering::Acquire) {
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
                    injector.inject_batch(&events)?;
                }
                MessageKind::Pointer
                | MessageKind::Key
                | MessageKind::Button
                | MessageKind::ReleaseAll => {
                    return Err(invalid_data(
                        "legacy single-event input is invalid in application protocol v3",
                    )
                    .into());
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

    fn test_pending_frame(red: u8) -> PendingFrame {
        PendingFrame {
            frame: CapturedFrame {
                image: RgbImage::from_pixel(2, 2, Rgb([red, 0, 0])),
                source_width: 2,
                source_height: 2,
            },
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
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(83)),
            Duration::from_millis(250)
        );
        assert_eq!(
            backpressure_refresh_interval(Duration::from_millis(16)),
            Duration::from_millis(250)
        );
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
        for elapsed in [3, 4, 5] {
            telemetry.record_captured(Duration::from_millis(elapsed));
        }
        telemetry.record_published(false);
        telemetry.record_published(true);
        telemetry.record_published(false);
        telemetry.record_dequeued(Duration::from_millis(10), Duration::from_millis(1));
        telemetry.record_dequeued(Duration::from_millis(20), Duration::from_millis(2));
        telemetry.record_encoded(Duration::from_millis(6));
        telemetry.record_encoded(Duration::from_millis(7));

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
        assert_eq!(sent_window.published, 3);
        assert_eq!(sent_window.replaced, 1);
        assert_eq!(sent_window.dequeued, 2);
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
        let writer = SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0x5a; 32],
        );
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
        assert_eq!(decode_frame(&first_payload).unwrap().sequence, 0);
        assert_eq!(decode_frame(&second_payload).unwrap().sequence, 1);
        let third = decode_frame(&third_payload).unwrap();
        assert_eq!(third.sequence, 2);
        assert!(third.image.get_pixel(0, 0).0[0].abs_diff(70) <= 2);
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
        let error = write_frame_record(&mut writer, b"frame", Duration::from_millis(40))
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
                false,
                None,
                Duration::from_millis(600),
                Duration::from_millis(60),
                &receiver_running,
                &receiver_stop,
                &receiver_credits,
                &receiver_telemetry,
                &receiver_jpeg_quality,
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
