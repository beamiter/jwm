//! Trusted-LAN remote desktop host.

#[path = "host_control.rs"]
mod control;
#[path = "host_input_wake.rs"]
mod input_wake;
#[path = "host_text.rs"]
mod text;
use control::{ControlCommand, HostControl};
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) use input_wake::{InputWakeProbe, with_input_wake};

use super::RemoteResult;
use super::clipboard_policy::{self, ClipboardDirection, ClipboardMode};
use super::clipboard_sync::{
    ClipboardInbox, ClipboardOutbox, ClipboardSync, MAX_TRANSFER_RECORD_BYTES, Outgoing,
    SyncOptions,
};
use super::clipboard_wire::{ClipboardRestriction, OutboundGate};
use super::close_wire::{SessionClose, SessionCloseCode};
use super::deadline::TcpStreamDeadline;
use super::key::KeySource;
use super::messages::{
    ClientHello, MAX_CLIPBOARD_BYTES, MIN_VIEWPORT_HEIGHT, MIN_VIEWPORT_WIDTH, ServerHello,
    decode_source_frame_ack, decode_source_input, decode_viewport,
};
use super::pacing::capture_rate_limit_wait;
use super::protocol::{
    MessageKind, PayloadBufferRetention, ProtocolError, SessionReader, SessionWriter,
    server_handshake,
};
use super::source_session::{MAX_GRANTED_SOURCES, SourceId, SourceSession, SourceVersion};
use super::source_wire::{
    SourceCatalog, SourceEntry, SourceRequest, encode_source_changed, encode_source_payload,
};
use super::tile::{TileEncodeRequest, TileEncoder, TilePlan};
use super::usability::listener_bind_error;
use super::wayland_keyboard_wire::{
    DrawnScope, HostKeyboardState, Key as KeyboardKey, KeyboardRevoke, LeaseRequest, LeaseStatus,
    LeaseStatusKind,
};
use super::wayland_pointer_wire::{
    FRAME_MAX_AGE, HostPointerState, PointerAction, PointerRequest, PointerRevoke, PointerStatus,
    PointerStatusKind,
};
use super::window_resize::{HostResizeState, ResizeRequest, ResizeResult};
use super::x11_capture::{
    CaptureArea, CaptureImageRecycler, CaptureMode, CaptureOutcome, CaptureSource, CapturedFrame,
    ScalingPolicy, X11Capture, validate_capture_area,
};
use super::x11_input::{InputArea, InputError, InputInjector};
use super::x11_keymap::{KeymapMatch, KeymapTable};
use super::{host_capture::HostCapture, window_capture::WindowCapture};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HOST_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
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
const _: () = assert!(MAX_TRANSFER_RECORD_BYTES <= MAX_INBOUND_CLIPBOARD_PAYLOAD_LEN);
/// How long the clipboard forwarder waits before re-checking for shutdown.
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Reason sent to a second viewer while one is already connected.
const BUSY_REASON: &str = "another viewer is already connected to this host";
/// Reason sent to the viewer when the host is asked to stop.
const HOST_STOPPING_REASON: &str = "the host stopped sharing its desktop";
/// Encoding is terminal, but has no structured v13 code. Keep details local.
const ENCODING_FAILURE_REASON: &str = "host could not encode the shared display";
const INPUT_FAILURE_REASON: &str = "host input became unavailable";
const KEYMAP_VERIFY_FAILURE_REASON: &str = "host could not verify keyboard input";
const KEYMAP_CHANGED_REASON: &str =
    "host keyboard mapping changed; reconnect to re-verify keyboard input";
/// Budget for that one goodbye record. A peer too slow to take a few dozen
/// bytes must not hold up Ctrl+C; it then sees the connection drop instead.
const GOODBYE_WRITE_DEADLINE: Duration = Duration::from_millis(250);
/// Longest setup-failure reason sent to an authenticated viewer, in bytes.
///
/// The viewer accepts at most 1 KiB before its session is acknowledged and
/// shows at most 256 bytes of a refusal, so the host trims first.
const MAX_SETUP_REASON_BYTES: usize = 240;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Unauthenticated handshakes allowed to run at once.
///
/// Each runs on its own thread under `HANDSHAKE_TIMEOUT`, so a peer that
/// connects and says nothing can neither claim the viewer slot nor stall the
/// accept loop for the next connection. Connections beyond the cap are closed
/// at once; the cap only bounds the threads an unauthenticated flood can pin.
const MAX_PENDING_HANDSHAKES: usize = 8;
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
    /// Last requested bounds survive take(); lifecycle checks cannot forget history.
    requested: AtomicU32,
}

impl ViewportRequest {
    fn new() -> Self {
        Self {
            size: AtomicU32::new(0),
            requested: AtomicU32::new(0),
        }
    }

    fn seed(&self, initial: Option<(u16, u16)>) {
        if let Some((width, height)) = initial {
            self.requested.store(
                (u32::from(width) << 16) | u32::from(height),
                Ordering::Relaxed,
            );
        }
    }

    /// Return whether these bounds differ, independently of capture consumption.
    fn request(&self, width: u16, height: u16) -> bool {
        let packed = (u32::from(width) << 16) | u32::from(height);
        let changed = self.requested.swap(packed, Ordering::Relaxed) != packed;
        self.size.store(packed, Ordering::Relaxed);
        changed
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
// 256 default 16x16 tiles = at most 65,536 encoded pixels per refinement.
// Reuse the existing idle capture ticks instead of starting another producer.
const STATIC_REFINEMENT_INTERVAL: Duration = Duration::from_secs(2);
const STATIC_REFINEMENT_TILES: usize = 256;
// Static readback already exists; a larger bounded batch clears code/text
// promptly without introducing a second capture producer. Active damage uses
// the smaller budget to preserve interactive encoding latency.
const IDLE_REFINEMENT_TILES: usize = 4096;
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
    pointer_serial: Option<u64>,
    source_version: SourceVersion,
    frame: CapturedFrame,
    captured_at: Instant,
    capture_completed_at: Instant,
    recycler: Option<CaptureImageRecycler>,
}

impl Drop for PendingFrame {
    fn drop(&mut self) {
        if let Some(recycler) = &self.recycler {
            // The encoder borrows frames and copies committed dirty tiles to
            // its own reference. No sender or mailbox reader can retain this
            // image when PendingFrame is dropped, including early-error paths.
            recycler.recycle_image(std::mem::take(&mut self.frame.image));
        }
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone)]
pub(super) struct ProofFrameEvidence {
    pub native_serial: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub sequence: u64,
    pub keyframe: bool,
    pub dirty_tiles: u32,
    pub total_tiles: u32,
    pub encoded_bytes: usize,
    pub jpeg_ns: u64,
    pub captured_at: Instant,
    pub capture_completed_at: Instant,
    pub planned_at: Instant,
    pub flushed_at: Instant,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProofStageKind {
    AckReceived,
    ReadyPublished,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone)]
pub(super) struct ProofStageEvent {
    pub native_serial: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub sequence: u64,
    pub at: Instant,
    pub kind: ProofStageKind,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Default)]
pub(super) struct ProofDeltaProbe {
    frames: Mutex<Vec<ProofFrameEvidence>>,
    events: Mutex<Vec<ProofStageEvent>>,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
impl ProofDeltaProbe {
    pub(super) fn events_snapshot(&self) -> Vec<ProofStageEvent> {
        self.events.lock().unwrap().clone()
    }
    fn record_stage(
        &self,
        native_serial: u64,
        scope: DrawnScope,
        at: Instant,
        kind: ProofStageKind,
    ) {
        let mut events = self.events.lock().unwrap();
        assert!(events.len() < 512, "proof stage evidence test bound");
        events.push(ProofStageEvent {
            native_serial,
            source_epoch: scope.source_epoch,
            geometry_epoch: scope.geometry_epoch,
            sequence: scope.frame_sequence,
            at,
            kind,
        });
    }
    pub(super) fn snapshot(&self) -> Vec<ProofFrameEvidence> {
        self.frames.lock().unwrap().clone()
    }
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {
    static PROOF_DELTA_TEST: std::cell::RefCell<Option<(bool, Arc<ProofDeltaProbe>)>> = const { std::cell::RefCell::new(None) };
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn with_proof_delta<T>(
    legacy: bool,
    probe: Arc<ProofDeltaProbe>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<(bool, Arc<ProofDeltaProbe>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PROOF_DELTA_TEST.with(|value| *value.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PROOF_DELTA_TEST.with(|value| value.replace(Some((legacy, probe)))));
    run()
}

/// Chooses what each captured frame owes the viewer and encodes it.
///
/// The delta encoder subsumes the previous exact-duplicate suppressor: an
/// unchanged capture simply has no dirty tiles. Keepalive timing stays here
/// rather than in the codec because it is a property of the *session*, and it
/// must remain strictly inside the viewer's shared video idle timeout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LosslessSchedule {
    #[default]
    New250,
    #[cfg(test)]
    Legacy100,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LosslessScheduleStats {
    pub encoded_jpeg: u64,
    pub encoded_png: u64,
    pub encode_ns: u128,
    pub jpeg_ns: u128,
    pub png_ns: u128,
    pub sent_bytes: u64,
    pub comparisons: u64,
    pub comparison_ns: u128,
    pub stale_png_dropped: u64,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Default)]
pub(crate) struct LosslessScheduleProbe {
    stats: Mutex<LosslessScheduleStats>,
    records: Mutex<Vec<LosslessScheduleRecord>>,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LosslessScheduleRecord {
    pub sequence: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub png: bool,
    pub bytes: usize,
    pub encode_ns: u128,
    pub jpeg_ns: u128,
    pub png_ns: u128,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
impl LosslessScheduleProbe {
    pub(crate) fn records(&self) -> Vec<LosslessScheduleRecord> {
        self.records.lock().unwrap().clone()
    }
    pub(crate) fn snapshot(&self) -> LosslessScheduleStats {
        *self.stats.lock().unwrap()
    }
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static LOSSLESS_SCHEDULE_TEST:std::cell::RefCell<Option<(LosslessSchedule,Arc<LosslessScheduleProbe>)>>=const{std::cell::RefCell::new(None)};}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_lossless_schedule<T>(
    policy: LosslessSchedule,
    probe: Arc<LosslessScheduleProbe>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<(LosslessSchedule, Arc<LosslessScheduleProbe>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LOSSLESS_SCHEDULE_TEST.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(LOSSLESS_SCHEDULE_TEST.with(|slot| slot.replace(Some((policy, probe)))));
    run()
}
fn newer_frame_supersedes_repair(current: &PendingFrame, newer: &PendingFrame) -> bool {
    current.source_version != newer.source_version
        || current.frame.geometry_epoch != newer.frame.geometry_epoch
        || (current.frame.source_width, current.frame.source_height)
            != (newer.frame.source_width, newer.frame.source_height)
        || current.frame.image != newer.frame.image
}
struct FrameSender {
    lossless_schedule: LosslessSchedule,
    recent_dynamic_at: Option<Instant>,
    scroll_copy: bool,
    static_lossless: bool,
    encoder: TileEncoder,
    committed_at: Option<Instant>,
    last_content_change: Option<Instant>,
    last_refinement: Option<Instant>,
    lossless_retry_after: Option<Instant>,
    lossless_next_batch: Option<Instant>,
    pending_refinement: bool,
    pending_refinement_at: Option<Instant>,
}

impl FrameSender {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_scroll_copy(false)
    }

    #[cfg(test)]
    fn with_scroll_copy(enabled: bool) -> Self {
        Self::with_codecs(enabled, false)
    }

    fn with_codecs(enabled: bool, static_lossless: bool) -> Self {
        let mut encoder = TileEncoder::new();
        encoder.set_scroll_copy_enabled(enabled);
        encoder.set_lossless_enabled(static_lossless);
        Self {
            lossless_schedule: LosslessSchedule::New250,
            recent_dynamic_at: None,
            scroll_copy: enabled,
            static_lossless,
            encoder,
            committed_at: None,
            last_content_change: None,
            last_refinement: None,
            lossless_retry_after: None,
            lossless_next_batch: None,
            pending_refinement: false,
            pending_refinement_at: None,
        }
    }

    #[cfg(test)]
    fn plan_at(&mut self, frame: &CapturedFrame, now: Instant) -> RemoteResult<TilePlan> {
        self.plan_record_at(frame, now, false, false)
    }

    fn plan_record_at(
        &mut self,
        frame: &CapturedFrame,
        now: Instant,
        mandatory: bool,
        legacy_keyframe: bool,
    ) -> RemoteResult<TilePlan> {
        if mandatory && legacy_keyframe {
            *self = Self::with_codecs(self.scroll_copy, self.static_lossless);
        }
        let request = match self.committed_at {
            None => TileEncodeRequest::Keyframe,
            Some(_) if mandatory => TileEncodeRequest::FreshProof,
            Some(committed_at)
                if now.saturating_duration_since(committed_at) >= UNCHANGED_FRAME_KEEPALIVE =>
            {
                TileEncodeRequest::Keepalive
            }
            Some(_) => TileEncodeRequest::Delta,
        };
        self.pending_refinement = false;
        self.pending_refinement_at = None;
        if self.static_lossless && mandatory {
            return Err(
                invalid_data("lossless repairs cannot be used as native input proofs").into(),
            );
        }
        let mut plan = self.encoder.plan_at(frame, request, now)?;
        if self.static_lossless {
            if self.encoder.detected_exact_change() {
                self.recent_dynamic_at = Some(now);
            }
            let quiet = self.lossless_schedule != LosslessSchedule::New250
                || self.recent_dynamic_at.is_some_and(|at| {
                    now.saturating_duration_since(at) >= Duration::from_millis(250)
                });
            // Tile-local exact stability allows static text to converge beside
            // animation, but any ordinary dirty work takes JPEG priority.
            let retry = self.lossless_retry_after.is_none_or(|at| now >= at)
                && self.lossless_next_batch.is_none_or(|at| now >= at);
            if quiet
                && !plan.keyframe
                && plan.dirty_tiles == 0
                && retry
                && self.encoder.has_stable_exact_debt(now)
            {
                plan = self
                    .encoder
                    .plan_at(frame, TileEncodeRequest::LosslessRepair, now)?;
                self.pending_refinement_at = Some(now);
            }
            return Ok(plan);
        }
        if plan.dirty_tiles > 0 {
            // Anchor the first repair deadline once. Repeated local damage
            // before the first repair must not postpone it indefinitely.
            self.last_content_change.get_or_insert(now);
        }
        Ok(plan)
    }

    fn refinement_due(&self, now: Instant) -> bool {
        static_refinement_due(self.last_content_change, self.last_refinement, now)
    }

    fn refine_at(&mut self, quality: u8, now: Instant) -> RemoteResult<TilePlan> {
        let limit = if self.encoder.detected_dirty_tiles() == 0 {
            IDLE_REFINEMENT_TILES
        } else {
            STATIC_REFINEMENT_TILES
        };
        let plan = self.encoder.select_refinement(Some((quality, limit)))?;
        self.pending_refinement = plan.dirty_tiles > 0;
        self.pending_refinement_at = self.pending_refinement.then_some(now);
        Ok(plan)
    }

    /// Adopt a frame whose authenticated record and flush both succeeded.
    fn commit_at(&mut self, frame: &CapturedFrame, committed_at: Instant) {
        self.encoder.commit(frame);
        self.committed_at = Some(committed_at);
        if let Some(planned_at) = self.pending_refinement_at.take() {
            if self.static_lossless {
                // Anchor to completed flush: a delayed writer never catches up
                // by bursting through missed batch deadlines.
                self.lossless_next_batch = Some(
                    committed_at
                        + if self.lossless_schedule == LosslessSchedule::New250 {
                            Duration::from_millis(250)
                        } else {
                            Duration::from_millis(100)
                        },
                );
            } else {
                self.last_refinement =
                    Some(refinement_clock_after_commit(planned_at, committed_at));
            }
        }
        self.pending_refinement = false;
    }

    /// Abandon an encoded-but-unsent frame, leaving the viewer's model intact.
    fn discard(&mut self) {
        self.encoder.discard();
        self.pending_refinement = false;
        self.pending_refinement_at = None;
    }
}

// Ordinary encode/write cost must not turn each existing two-second capture
// tick into a four-second refinement tick. Commit the start clock only after
// success. A write that consumed a whole interval starts a fresh cooldown;
// never replay missed deadlines or burst to catch up after a stalled write.
fn refinement_clock_after_commit(planned: Instant, committed: Instant) -> Instant {
    if committed.saturating_duration_since(planned) >= STATIC_REFINEMENT_INTERVAL {
        committed
    } else {
        planned
    }
}

fn static_refinement_due(changed: Option<Instant>, refined: Option<Instant>, now: Instant) -> bool {
    let Some(changed) = changed else {
        return false;
    };
    let last = refined.unwrap_or(changed);
    now.saturating_duration_since(last) >= STATIC_REFINEMENT_INTERVAL
}

/// Exact quantiles of the most recent 128 observations in each reporting window.
/// Fixed storage bounds memory even when a peer runs for a long time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StageSamples {
    micros: [u64; 128],
    count: usize,
    next: usize,
}

impl Default for StageSamples {
    fn default() -> Self {
        Self {
            micros: [0; 128],
            count: 0,
            next: 0,
        }
    }
}

impl StageSamples {
    fn record(&mut self, elapsed: Duration) {
        self.micros[self.next] = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.next = (self.next + 1) % self.micros.len();
        self.count = (self.count + 1).min(self.micros.len());
    }

    fn json(self) -> serde_json::Value {
        if self.count == 0 {
            return serde_json::json!({"samples": 0, "p50_ms": null, "p95_ms": null, "p99_ms": null});
        }
        let mut values = self.micros;
        values[..self.count].sort_unstable();
        let percentile =
            |percent: usize| values[(self.count * percent).div_ceil(100) - 1] as f64 / 1000.0;
        serde_json::json!({"samples": self.count, "p50_ms": percentile(50),
            "p95_ms": percentile(95), "p99_ms": percentile(99)})
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
    dirty_scan_elapsed: Duration,
    dirty_scans: u64,
    writer_wait_elapsed: Duration,
    socket_write_elapsed: Duration,
    source_dimensions: (u16, u16),
    encoded_dimensions: (u32, u32),
    capture_samples: StageSamples,
    queue_samples: StageSamples,
    credit_samples: StageSamples,
    dirty_scan_samples: StageSamples,
    encode_samples: StageSamples,
    atlas_encoded: u64,
    atlas_pack_elapsed: Duration,
    jpeg_elapsed: Duration,
    jpeg_encoded: u64,
    png_encoded: u64,
    png_elapsed: Duration,
    codec_elapsed: Duration,
    png_samples: StageSamples,
    codec_samples: StageSamples,
    atlas_pack_samples: StageSamples,
    jpeg_samples: StageSamples,
    writer_wait_samples: StageSamples,
    socket_write_samples: StageSamples,
    capture_to_ack_samples: StageSamples,
    send_to_ack_samples: StageSamples,
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
    stats_json: bool,
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
            stats_json: false,
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
            window.capture_samples.record(elapsed);
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
            window.queue_samples.record(queue_age);
            window.credit_samples.record(credit_wait);
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
            window.encode_samples.record(elapsed);
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

    fn record_encode_stages(&self, stages: super::tile::EncodeStageTimings) {
        self.update(|window| {
            window.atlas_encoded = window.atlas_encoded.saturating_add(1);
            window.atlas_pack_elapsed = window.atlas_pack_elapsed.saturating_add(stages.atlas_pack);
            window.atlas_pack_samples.record(stages.atlas_pack);
            let codec = if let Some(png) = stages.png {
                window.png_encoded = window.png_encoded.saturating_add(1);
                window.png_elapsed = window.png_elapsed.saturating_add(png);
                window.png_samples.record(png);
                png
            } else {
                window.jpeg_encoded = window.jpeg_encoded.saturating_add(1);
                window.jpeg_elapsed = window.jpeg_elapsed.saturating_add(stages.jpeg);
                window.jpeg_samples.record(stages.jpeg);
                stages.jpeg
            };
            window.codec_elapsed = window.codec_elapsed.saturating_add(codec);
            window.codec_samples.record(codec);
        });
    }

    fn record_dirty_scan(&self, elapsed: Duration) {
        self.update(|window| {
            window.dirty_scans = window.dirty_scans.saturating_add(1);
            window.dirty_scan_elapsed = window.dirty_scan_elapsed.saturating_add(elapsed);
            window.dirty_scan_samples.record(elapsed);
        });
    }

    fn record_dimensions(&self, frame: &CapturedFrame) {
        self.update(|window| {
            window.source_dimensions = (frame.source_width, frame.source_height);
            window.encoded_dimensions = frame.image.dimensions();
        });
    }

    fn record_outstanding(&self, outstanding: u64) {
        self.update(|window| window.max_outstanding = window.max_outstanding.max(outstanding));
    }

    /// Commit one completed write as a single reporting-window observation.
    /// The periodic reporter may reset the window at any lock acquisition, so
    /// counts, durations and samples must never be published separately.
    /// Total write time ends at successful flush (wait + socket), excluding
    /// this accounting update and the subsequent writer-lock release.
    fn record_sent(&self, bytes: usize, wait: Duration, socket: Duration) {
        self.update(|window| {
            window.sent = window.sent.saturating_add(1);
            window.bytes = window
                .bytes
                .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
            window.write_elapsed = window
                .write_elapsed
                .saturating_add(wait.saturating_add(socket));
            window.writer_wait_elapsed = window.writer_wait_elapsed.saturating_add(wait);
            window.socket_write_elapsed = window.socket_write_elapsed.saturating_add(socket);
            window.writer_wait_samples.record(wait);
            window.socket_write_samples.record(socket);
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
            window.capture_to_ack_samples.record(ack.capture_to_ack);
            window.send_to_ack_samples.record(ack.send_to_ack);
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

    fn inspect_latest(&self, inspect: impl FnOnce(Option<&T>) -> bool) -> io::Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("remote frame mailbox lock was poisoned"))?;
        Ok(inspect(state.latest.as_ref()))
    }
    fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        let pending = state.latest.take();
        state.queued_at = None;
        drop(state);
        drop(pending);
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
    source_epoch: u64,
    geometry_epoch: u64,
    sequence: u64,
    captured_at: Instant,
    sent_at: Instant,
    bytes: u64,
    jpeg_quality: u8,
    quality_epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AckObservation {
    source_epoch: u64,
    geometry_epoch: u64,
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
    last_healthy_ack: Option<Instant>,
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
                last_healthy_ack: None,
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

    /// Planning must not consume ACK observations or advance quality epochs.
    fn current_quality(&self) -> u8 {
        match self.state.lock() {
            Ok(state) => state.current,
            Err(poisoned) => poisoned.into_inner().current,
        }
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
                        state.last_healthy_ack = Some(observation.observed_at);
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
            && state.healthy_ack_streak
                >= observed_recovery_ack_target(
                    self.recovery_ack_target,
                    state.healthy_ack_streak,
                    state.healthy_since,
                    state.last_healthy_ack,
                )
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

/// Configured FPS is only an upper bound on ACK frequency. Static desktops
/// send keepalives far less often; require the same minimum of four independent
/// healthy samples and the existing three-second observation span, but use
/// their real same-epoch cadence. Bursty fast ACKs retain the24-sample ceiling.
fn observed_recovery_ack_target(
    configured: u64,
    samples: u64,
    first: Option<Instant>,
    last: Option<Instant>,
) -> u64 {
    let (Some(first), Some(last)) = (first, last) else {
        return configured;
    };
    if samples < 2 {
        return configured;
    }
    let interval = last.saturating_duration_since(first).as_nanos() / u128::from(samples - 1);
    let interval = Duration::from_nanos(u64::try_from(interval).unwrap_or(u64::MAX));
    configured.min(jpeg_quality_recovery_ack_target(interval))
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
    last_acked_epoch: Option<u64>,
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
                last_acked_epoch: None,
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

    #[cfg(test)]
    fn mark_sent(
        &self,
        sequence: u64,
        captured_at: Instant,
        sent_at: Instant,
        bytes: usize,
        jpeg_quality: u8,
        quality_epoch: u64,
    ) -> io::Result<u64> {
        self.mark_sent_scoped(
            sequence,
            captured_at,
            sent_at,
            bytes,
            jpeg_quality,
            quality_epoch,
            1,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn mark_sent_scoped(
        &self,
        sequence: u64,
        captured_at: Instant,
        sent_at: Instant,
        bytes: usize,
        jpeg_quality: u8,
        quality_epoch: u64,
        source_epoch: u64,
        geometry_epoch: u64,
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
            source_epoch,
            geometry_epoch,
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

    #[cfg(test)]
    fn acknowledge_at_with_observer(
        &self,
        sequence: u64,
        acknowledged_at: Instant,
        observer: impl FnOnce(AckObservation),
    ) -> io::Result<Option<AckObservation>> {
        self.acknowledge_scoped_at(sequence, None, acknowledged_at, observer)
    }

    fn acknowledge_scoped_at(
        &self,
        sequence: u64,
        source_epoch: Option<u64>,
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
            if source_epoch.is_some_and(|epoch| Some(epoch) != state.last_acked_epoch) {
                return Err(invalid_data("frame acknowledgement source epoch mismatch"));
            }
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
        if source_epoch.is_some_and(|epoch| epoch != target.source_epoch) {
            return Err(invalid_data("frame acknowledgement source epoch mismatch"));
        }
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
        state.last_acked_epoch = Some(target.source_epoch);
        let observation = AckObservation {
            source_epoch: target.source_epoch,
            geometry_epoch: target.geometry_epoch,
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

    /// A source barrier invalidates viewer presentations without claiming
    /// they were drawn. Retire their credit so suppressed stale ACKs cannot
    /// deadlock the first new-source keyframe; sequence numbers never reset.
    fn abandon_source(&self) {
        let mut state = self.state.lock().unwrap();
        state.last_acked_epoch = state
            .in_flight
            .back()
            .map(|frame| frame.source_epoch)
            .or(state.last_acked_epoch);
        state.last_acked = state.last_sent;
        state.in_flight.clear();
        self.available.notify_all();
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
    Clipboard = 6,
}

struct FirstStop(AtomicU8);

impl FirstStop {
    fn new() -> Self {
        Self(AtomicU8::new(StopCause::None as u8))
    }

    fn record(&self, cause: StopCause) -> bool {
        debug_assert!(!matches!(cause, StopCause::None | StopCause::ThreadPanic));
        self.0
            .compare_exchange(
                StopCause::None as u8,
                cause as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
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
            value if value == StopCause::Clipboard as u8 => StopCause::Clipboard,
            value if value == StopCause::ThreadPanic as u8 => StopCause::ThreadPanic,
            _ => StopCause::None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub frame_limits: super::messages::FrameLimits,
    pub listen: String,
    pub key: KeySource,
    pub display: Option<String>,
    pub wayland_select: bool,
    pub wayland_resize_follow: bool,
    pub wayland_popups: bool,
    pub wayland_display: Option<String>,
    pub fps: u16,
    pub jpeg_quality: u8,
    pub jpeg_quality_floor: u8,
    pub fixed_jpeg_quality: bool,
    pub max_width: u16,
    pub capture_source: CaptureSource,
    /// Which part of the root to share; the whole root by default.
    pub capture_area: CaptureArea,
    /// Explicit additional host grants, never inferred from viewer requests.
    pub shared_sources: Vec<CaptureArea>,
    /// Where frames are shrunk to the viewer's size.
    pub scaling: ScalingPolicy,
    pub allow_lan: bool,
    pub allow_input: bool,
    pub allow_window_resize: bool,
    pub allow_wayland_keyboard: bool,
    pub allow_wayland_pointer: bool,
    pub allow_wayland_control: bool,
    pub allow_wayland_text: bool,
    pub allow_wayland_scroll: bool,
    pub allow_wayland_horizontal_scroll: bool,
    /// Experimental video codec optimization, independent of input grants.
    pub scroll_copy: bool,
    pub allow_static_lossless: bool,
    pub allow_clipboard: bool,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_direction: ClipboardDirection,
    /// Largest file copy shared through the clipboard; 0 shares no files.
    pub clipboard_file_limit: u64,
    pub once: bool,
    /// Emit machine-readable host telemetry as JSON lines on stderr.
    pub stats_json: bool,
}

/// Build an immutable host-only grant set. Initial selection is always first;
/// nothing implicitly adds the whole root when a scoped source was selected.
fn host_source_grants(options: &HostOptions) -> RemoteResult<Vec<(SourceId, CaptureArea)>> {
    if options.shared_sources.len() >= MAX_GRANTED_SOURCES {
        return Err(invalid_data("too many explicit source grants").into());
    }
    let mut areas = vec![options.capture_area.clone()];
    for area in &options.shared_sources {
        if !areas.contains(area) {
            areas.push(area.clone());
        }
    }
    let grants: Vec<_> = areas
        .into_iter()
        .enumerate()
        .map(|(index, area)| (SourceId(index as u32 + 1), area))
        .collect();
    let _ = SourceSession::new(grants.clone(), SourceId(1))?;
    Ok(grants)
}

fn source_label(area: &CaptureArea) -> String {
    match area {
        CaptureArea::Root => "Whole desktop".into(),
        CaptureArea::Window(_) => "Authorized application window".into(),
        CaptureArea::Monitor(name) => name.clone(),
        CaptureArea::Region(region) => format!(
            "{}x{}{:+}{:+}",
            region.width, region.height, region.x, region.y
        ),
    }
}

struct HostSourceState {
    policy: SourceSession,
    pending: Option<(SourceVersion, CaptureArea)>,
    input_area: InputArea,
    geometry_epoch: u64,
    presented_geometry: Option<u64>,
    presented_frame: Option<DrawnScope>,
}

#[derive(Debug, PartialEq, Eq)]
enum SourceInputAdmission {
    Stale,
    ReleaseOnly,
    Inject(InputArea),
}

fn source_input_admission(
    state: &HostSourceState,
    source_epoch: u64,
    geometry_epoch: u64,
    events: &[super::x11_input::InputEvent],
) -> SourceInputAdmission {
    let version = state.policy.current();
    if source_epoch != version.epoch {
        return SourceInputAdmission::Stale;
    }
    if events == [super::x11_input::InputEvent::ReleaseAll] {
        // A safety fence cannot inject a coordinate or press, so it remains
        // meaningful when only geometry has changed. Never cross sources.
        return SourceInputAdmission::ReleaseOnly;
    }
    if !state.policy.accepts_input(version)
        || geometry_epoch != state.geometry_epoch
        || state.presented_geometry != Some(geometry_epoch)
    {
        return SourceInputAdmission::Stale;
    }
    SourceInputAdmission::Inject(state.input_area)
}

fn refresh_input_geometry<E>(
    previous: &mut u64,
    current: u64,
    release: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    if *previous != current {
        release()?;
        *previous = current;
    }
    Ok(())
}

enum KeyboardCommand {
    Request(LeaseRequest),
    Key(KeyboardKey),
}

enum PointerCommand {
    Request(PointerRequest),
    Action(PointerAction),
}
#[derive(Clone, Copy)]
struct PointerFrame {
    serial: u64,
    at: Instant,
    scope: Option<DrawnScope>,
    announced: bool,
}
// This only classifies a successfully acknowledged record. Credits validation
// remains the sole replay/future-ACK gate; a wake never grants Ready itself.
fn proof_ack_matches(frame: Option<PointerFrame>, scope: DrawnScope) -> bool {
    frame.is_some_and(|frame| !frame.announced && frame.scope == Some(scope))
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {
    static LEGACY_DISABLE_ACK_WAKE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn with_legacy_disable_ack_wake<T>(disabled: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            LEGACY_DISABLE_ACK_WAKE.with(|v| v.set(self.0));
        }
    }
    let _restore = Restore(LEGACY_DISABLE_ACK_WAKE.with(|v| v.replace(disabled)));
    run()
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {
    static LEGACY_RETAINED_PROOF_SLOTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(super) fn with_legacy_retained_proof_slots<T>(legacy: bool, run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            LEGACY_RETAINED_PROOF_SLOTS.with(|v| v.set(self.0));
        }
    }
    let _restore = Restore(LEGACY_RETAINED_PROOF_SLOTS.with(|v| v.replace(legacy)));
    run()
}
// Charge every ordinary attempt (including NoChange and backpressure). Only
// the explicit no-readback native proof outcome preserves the prior deadline.
fn retained_proof_deadline(
    previous: Instant,
    charged: Instant,
    blocked: bool,
    legacy: bool,
) -> Instant {
    if blocked && !legacy {
        previous
    } else {
        charged
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Default)]
pub(crate) struct PendingLosslessProbe {
    blocked: Mutex<Option<(u64, u64, u64)>>,
    released: Mutex<bool>,
    changed: Condvar,
    once: AtomicBool,
    publication: std::sync::atomic::AtomicU64,
    written: Mutex<Vec<(u64, u64, u64, bool, bool)>>,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
impl PendingLosslessProbe {
    pub(crate) fn observing_only() -> Self {
        Self {
            once: AtomicBool::new(true),
            ..Self::default()
        }
    }
    pub(crate) fn blocked(&self) -> Option<(u64, u64, u64)> {
        *self.blocked.lock().unwrap()
    }
    pub(crate) fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
    pub(crate) fn publication_epoch(&self) -> u64 {
        self.publication.load(Ordering::Acquire)
    }
    pub(crate) fn written(&self) -> Vec<(u64, u64, u64, bool, bool)> {
        self.written.lock().unwrap().clone()
    }
    fn pause(&self, sequence: u64, source_epoch: u64, epoch: u64) -> RemoteResult<()> {
        if self.once.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        *self.blocked.lock().unwrap() = Some((sequence, source_epoch, epoch));
        let released = self.released.lock().unwrap();
        let (released, _) = self
            .changed
            .wait_timeout_while(released, Duration::from_secs(5), |value| !*value)
            .unwrap();
        if !*released {
            return Err(invalid_data("test pending PNG barrier timed out").into());
        }
        Ok(())
    }
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static PENDING_LOSSLESS_PROBE:std::cell::RefCell<Option<Arc<PendingLosslessProbe>>>=const{std::cell::RefCell::new(None)};}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_pending_lossless_probe<T>(
    probe: Option<Arc<PendingLosslessProbe>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Arc<PendingLosslessProbe>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PENDING_LOSSLESS_PROBE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PENDING_LOSSLESS_PROBE.with(|slot| slot.replace(probe)));
    run()
}
struct SourceControl {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    lossless_schedule_test: Option<(LosslessSchedule, Arc<LosslessScheduleProbe>)>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    pending_lossless_probe: Option<Arc<PendingLosslessProbe>>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    legacy_retained_proof_slots: bool,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    legacy_disable_ack_wake: bool,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    proof_delta: Option<(bool, Arc<ProofDeltaProbe>)>,
    input_wake: std::sync::OnceLock<input_wake::InputWake>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    input_wake_probe: Option<Arc<InputWakeProbe>>,
    follow_geometry: AtomicBool,
    text_enabled: AtomicBool,
    text_attempted: AtomicBool,
    text_request: Mutex<Option<super::wayland_text_wire::TextCommit>>,
    control_enabled: AtomicBool,
    control_revoke: AtomicBool,
    control_queue: Mutex<VecDeque<ControlCommand>>,
    scroll_copy: AtomicBool,
    static_lossless: AtomicBool,
    pointer_enabled: AtomicBool,
    scroll_enabled: AtomicBool,
    horizontal_scroll_enabled: AtomicBool,
    pointer_revoke: AtomicBool,
    pointer_queue: Mutex<VecDeque<PointerCommand>>,
    pointer_frame: Mutex<Option<PointerFrame>>,
    state: Mutex<HostSourceState>,
    changed: Condvar,
    resize_enabled: AtomicBool,
    resize_request: Mutex<Option<ResizeRequest>>,
    keyboard_enabled: AtomicBool,
    keyboard_revoke: AtomicBool,
    keyboard_queue: Mutex<VecDeque<KeyboardCommand>>,
}

impl SourceControl {
    fn notify_proof_ack(&self, scope: DrawnScope) -> io::Result<()> {
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if self.legacy_disable_ack_wake {
            return Ok(());
        }
        let live = self.text_enabled.load(Ordering::Acquire)
            || (self.control_enabled.load(Ordering::Acquire)
                && !self.control_revoke.load(Ordering::Acquire))
            || (self.pointer_enabled.load(Ordering::Acquire)
                && !self.pointer_revoke.load(Ordering::Acquire));
        let matching = live && proof_ack_matches(*self.pointer_frame.lock().unwrap(), scope);
        // No state, proof or credit lock is held across eventfd notification.
        // Any concurrent revoke/replacement is rechecked by the capture loop.
        if matching {
            self.notify_input()?;
        }
        Ok(())
    }
    fn notify_input(&self) -> io::Result<()> {
        self.input_wake
            .get()
            .map_or(Ok(()), input_wake::InputWake::notify)
    }
    fn input_backlog(&self) -> bool {
        // Only actionable queue entries, never held keys or waiting-for-Ready
        // state. Disabled modes are terminal and their stale flags cannot spin.
        (self.text_enabled.load(Ordering::Acquire) && self.text_request.lock().unwrap().is_some())
            || (self.control_enabled.load(Ordering::Acquire)
                && (self.control_revoke.load(Ordering::Acquire)
                    || !self.control_queue.lock().unwrap().is_empty()))
            || (self.pointer_enabled.load(Ordering::Acquire)
                && (self.pointer_revoke.load(Ordering::Acquire)
                    || !self.pointer_queue.lock().unwrap().is_empty()))
            || (self.keyboard_enabled.load(Ordering::Acquire)
                && (self.keyboard_revoke.load(Ordering::Acquire)
                    || !self.keyboard_queue.lock().unwrap().is_empty()))
    }

    fn new(
        grants: Vec<(SourceId, CaptureArea)>,
        area: InputArea,
        geometry_epoch: u64,
    ) -> RemoteResult<Self> {
        Ok(Self {
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            proof_delta: PROOF_DELTA_TEST.with(|value| value.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            pending_lossless_probe: PENDING_LOSSLESS_PROBE.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            lossless_schedule_test: LOSSLESS_SCHEDULE_TEST.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            legacy_disable_ack_wake: LEGACY_DISABLE_ACK_WAKE.with(|v| v.get()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            legacy_retained_proof_slots: LEGACY_RETAINED_PROOF_SLOTS.with(|v| v.get()),
            input_wake: std::sync::OnceLock::new(),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            input_wake_probe: input_wake::test_probe(),
            follow_geometry: AtomicBool::new(false),
            text_enabled: AtomicBool::new(false),
            text_attempted: AtomicBool::new(false),
            text_request: Mutex::new(None),
            control_enabled: AtomicBool::new(false),
            control_revoke: AtomicBool::new(false),
            control_queue: Mutex::new(VecDeque::new()),
            scroll_copy: AtomicBool::new(false),
            static_lossless: AtomicBool::new(false),
            pointer_enabled: AtomicBool::new(false),
            scroll_enabled: AtomicBool::new(false),
            horizontal_scroll_enabled: AtomicBool::new(false),
            pointer_revoke: AtomicBool::new(false),
            pointer_queue: Mutex::new(VecDeque::new()),
            pointer_frame: Mutex::new(None),
            state: Mutex::new(HostSourceState {
                policy: SourceSession::new(grants, SourceId(1))?,
                pending: None,
                input_area: area,
                geometry_epoch,
                presented_geometry: None,
                presented_frame: None,
            }),
            changed: Condvar::new(),
            resize_enabled: AtomicBool::new(false),
            resize_request: Mutex::new(None),
            keyboard_enabled: AtomicBool::new(false),
            keyboard_revoke: AtomicBool::new(false),
            keyboard_queue: Mutex::new(VecDeque::new()),
        })
    }

    /// Keep geometry release, admission and native injection in one critical
    /// section. Capture cannot replace the input transform after validation
    /// but before XTEST submission. This lock never acquires the wire writer.
    fn apply_input(
        &self,
        previous_geometry: &mut u64,
        source_epoch: u64,
        geometry_epoch: u64,
        events: &[super::x11_input::InputEvent],
        mut apply: impl FnMut(SourceInputAdmission) -> RemoteResult<()>,
    ) -> RemoteResult<()> {
        let state = self.state.lock().unwrap();
        refresh_input_geometry(previous_geometry, state.geometry_epoch, || {
            apply(SourceInputAdmission::ReleaseOnly)
        })?;
        let admission = source_input_admission(&state, source_epoch, geometry_epoch, events);
        let result = if admission == SourceInputAdmission::Stale {
            Ok(())
        } else {
            apply(admission)
        };
        drop(state);
        result
    }

    /// No writer/credit lock is held while waiting for presentation. A new
    /// source request or shutdown is observed within one short bounded slice.
    fn sender_ready(&self, version: SourceVersion, running: &AtomicBool) -> RemoteResult<bool> {
        let deadline = Instant::now() + FRAME_ACK_TIMEOUT;
        let mut state = self.state.lock().unwrap();
        loop {
            if !running.load(Ordering::Acquire) || !state.policy.accepts_capture(version) {
                return Ok(false);
            }
            if !state.policy.awaiting_presentation() {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "source keyframe was not presented",
                )
                .into());
            }
            state = self
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(100)))
                .unwrap()
                .0;
        }
    }
}

/// Startup notice for a listener reachable beyond loopback.
/// Transport v3 authenticates with the PSK and uses fresh ephemeral Noise keys.
/// Later PSK disclosure alone does not recover completed sessions' traffic keys;
/// it still permits impersonation in future sessions. Endpoint compromise is not covered.
const TRUSTED_LAN_NOTICE: &str = "jwm-remote: trusted-LAN mode encrypts traffic with forward secrecy \
     and pre-shared-key authentication; a leaked key file permits future peer impersonation";

/// The notice to print for a listener bound to `address`, if any.
fn listener_security_notice(address: IpAddr) -> Option<&'static str> {
    (!address.is_loopback()).then_some(TRUSTED_LAN_NOTICE)
}

fn validate_window_policy(options: &HostOptions) -> RemoteResult<()> {
    if options.allow_static_lossless
        && (options.allow_input
            || options.allow_clipboard
            || options.allow_window_resize
            || options.allow_wayland_keyboard
            || options.allow_wayland_pointer
            || options.allow_wayland_control
            || options.allow_wayland_text
            || options.allow_wayland_scroll
            || options.allow_wayland_horizontal_scroll)
    {
        return Err(invalid_data("static lossless requires a read-only host without input, text, clipboard or remote resize permissions").into());
    }

    if options.allow_wayland_text
        && (!options.wayland_select
            || !options.once
            || options.wayland_popups
            || options.wayland_resize_follow
            || options.allow_input
            || options.allow_clipboard
            || options.allow_window_resize
            || options.allow_wayland_keyboard
            || options.allow_wayland_pointer
            || options.allow_wayland_control
            || options.allow_wayland_scroll
            || options.allow_wayland_horizontal_scroll)
    {
        return Err(invalid_data("text requires exclusive one-session native root selection without other input or clipboard").into());
    }

    if options.wayland_popups
        && (!options.wayland_select
            || !options.once
            || options.allow_input
            || options.allow_clipboard
            || options.allow_window_resize
            || options.allow_wayland_keyboard
            || options.allow_wayland_pointer
            || options.allow_wayland_control
            || options.allow_wayland_scroll
            || options.allow_wayland_horizontal_scroll)
    {
        return Err(permission_denied("Wayland popup capture requires an explicitly selected one-session read-only source without input, clipboard or remote resize").into());
    }

    if options.wayland_resize_follow
        && (!options.wayland_select
            || options.allow_input
            || options.allow_clipboard
            || options.allow_window_resize
            || options.allow_wayland_keyboard
            || options.allow_wayland_pointer
            || options.allow_wayland_control
            || options.allow_wayland_scroll
            || options.allow_wayland_horizontal_scroll)
    {
        return Err(permission_denied("Wayland resize-follow requires an explicitly selected read-only source without any input, clipboard or resize permission").into());
    }

    if (options.allow_wayland_scroll || options.allow_wayland_horizontal_scroll)
        && !options.allow_wayland_pointer
        && !options.allow_wayland_control
    {
        return Err(invalid_data("Wayland scroll requires explicit pointer permission").into());
    }
    if usize::from(options.allow_wayland_keyboard)
        + usize::from(options.allow_wayland_pointer)
        + usize::from(options.allow_wayland_control)
        > 1
    {
        return Err(invalid_data("native leases are mutually exclusive").into());
    }
    if options.wayland_select {
        if !cfg!(feature = "remote-wayland")
            || !options.once
            || options.allow_input
            || options.allow_clipboard
            || options.allow_window_resize
            || !options.shared_sources.is_empty()
            || options.display.is_some()
            || !matches!(options.capture_area, CaptureArea::Root)
            || !matches!(options.capture_source, CaptureSource::Auto)
            || matches!(options.key, KeySource::Stdin)
        {
            return Err(permission_denied("Wayland selection requires remote-wayland support, --once, file key, no legacy input/clipboard/resize and no X11/source options").into());
        }
        return Ok(());
    }
    if options.allow_wayland_keyboard
        || options.allow_wayland_pointer
        || options.allow_wayland_control
    {
        return Err(permission_denied(
            "Wayland keyboard requires explicit native Wayland selection",
        )
        .into());
    }
    if options.wayland_display.is_some() {
        return Err(invalid_data("--wayland-display requires --wayland-select").into());
    }
    let window = matches!(options.capture_area, CaptureArea::Window(_));
    if (options.allow_window_resize && !window)
        || options
            .shared_sources
            .iter()
            .any(|area| matches!(area, CaptureArea::Window(_)))
        || (window
            && (options.allow_input
                || options.allow_clipboard
                || !options.once
                || !options.shared_sources.is_empty()))
    {
        return Err(permission_denied(
            "window sharing requires --once, view-only, no clipboard and no additional sources",
        )
        .into());
    }
    Ok(())
}

// Only pre-authorized single-instance sources cross the accept-worker boundary.
// Desktop X11Capture contains thread-affine SHM mappings and stays local.
enum GrantedCapture {
    Window(Box<WindowCapture>),
    #[cfg(feature = "remote-wayland")]
    Wayland(Box<super::wayland_capture::WaylandCapture>),
}
impl GrantedCapture {
    fn into_capture(self) -> HostCapture {
        match self {
            Self::Window(capture) => HostCapture::Window(capture),
            #[cfg(feature = "remote-wayland")]
            Self::Wayland(capture) => HostCapture::Wayland(capture),
        }
    }
}

pub fn run_host(options: HostOptions) -> RemoteResult<()> {
    options.frame_limits.validate()?;
    JpegQualityController::new_at(
        options.jpeg_quality,
        options.jpeg_quality_floor,
        !options.fixed_jpeg_quality,
        target_frame_interval(options.fps),
        Instant::now(),
    )?;
    validate_window_policy(&options)?;
    let key = options.key.load()?;
    let grants = host_source_grants(&options)?;
    // Bind the instance before announcing readiness, not on viewer arrival.
    // Keep its X connection and pixmap alive throughout the waiting period.
    let window_capture = if options.wayland_select {
        #[cfg(feature = "remote-wayland")]
        {
            Some(GrantedCapture::Wayland(Box::new(
                super::wayland_capture::WaylandCapture::connect(
                    options.wayland_display.as_deref(),
                    options.max_width,
                    options.wayland_resize_follow,
                    options.wayland_popups,
                )?,
            )))
        }
        #[cfg(not(feature = "remote-wayland"))]
        {
            return Err(invalid_data("remote-wayland support is not compiled in").into());
        }
    } else if let CaptureArea::Window(id) = options.capture_area {
        let mut capture =
            WindowCapture::connect(options.display.as_deref(), id, options.max_width)?;
        if options.allow_window_resize {
            capture.enable_native_resize()?;
        }
        Some(GrantedCapture::Window(Box::new(capture)))
    } else {
        for (_, area) in &grants {
            validate_capture_area(options.display.as_deref(), area)?;
        }
        None
    };
    let window_capture = Mutex::new(window_capture);
    let listener = TcpListener::bind(&options.listen)
        .map_err(|error| listener_bind_error(&options.listen, error))?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    if matches!(options.key, KeySource::Stdin) {
        stop_when_stdin_closes(Arc::clone(&shutdown))?;
    }
    let local_address = listener.local_addr()?;
    if !local_address.ip().is_loopback() && !options.allow_lan {
        return Err(permission_denied(format!(
            "refusing non-loopback listener {local_address}; pass --allow-lan for a trusted LAN"
        ))
        .into());
    }

    eprintln!("jwm-remote: listening on {local_address}");
    if let Some(notice) = listener_security_notice(local_address.ip()) {
        eprintln!("{notice}");
    }
    if !options.allow_input {
        eprintln!("jwm-remote: input control is disabled (view-only host)");
    }

    let key = Arc::new(key);
    let options = Arc::new(options);
    let once = options.once;
    let session_shutdown = Arc::clone(&shutdown);
    accept_connections(&listener, key, &shutdown, once, move |peer| {
        serve_authenticated(peer, &options, &session_shutdown, &window_capture)
    })
}

/// Stop the host once standard input reaches end of file.
///
/// A host that read its key from stdin was started by `jwm-remote ssh`, and
/// stdin is the SSH channel. When the viewer side goes away -- closed, killed,
/// or its network gone -- sshd closes that channel, and a host listening on
/// would be left behind holding a key nobody can use again.
fn stop_when_stdin_closes(shutdown: Arc<AtomicBool>) -> io::Result<()> {
    thread::Builder::new()
        .name("jwm-remote-stdin".into())
        .spawn(move || {
            let mut discard = [0_u8; 64];
            let mut stdin = io::stdin();
            loop {
                match std::io::Read::read(&mut stdin, &mut discard) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            if !shutdown.swap(true, Ordering::AcqRel) {
                eprintln!("jwm-remote: the controlling SSH session closed; stopping");
            }
        })
        .map(drop)
}

/// Releases the single viewer slot when the session that claimed it ends,
/// including by panic.
const VIEWER_IDLE: u8 = 0;
const VIEWER_ACTIVE: u8 = 1;
const VIEWER_POISONED: u8 = 2;

struct ViewerSlot(Arc<AtomicU8>);

impl ViewerSlot {
    /// Claim the slot, or `None` while another authenticated viewer holds it.
    fn claim(active: &Arc<AtomicU8>) -> Option<Self> {
        active
            .compare_exchange(
                VIEWER_IDLE,
                VIEWER_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
            .then(|| Self(Arc::clone(active)))
    }
}

impl Drop for ViewerSlot {
    fn drop(&mut self) {
        // A poisoned listener never becomes available again, even when the
        // detached worker eventually exits. Restart the host to recover.
        let _ = self.0.compare_exchange(
            VIEWER_ACTIVE,
            VIEWER_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Poison admission while still holding the slot. Unwinding or unconfirmed
/// cleanup can leave an injector alive; it must never overlap a new session.
fn run_admitted_session(
    slot: ViewerSlot,
    serve: impl FnOnce() -> RemoteResult<()>,
) -> RemoteResult<()> {
    let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(serve)) {
        Ok(outcome) => outcome,
        Err(_) => {
            slot.0.store(VIEWER_POISONED, Ordering::Release);
            Err(io::Error::other("remote session thread panicked; host admission disabled").into())
        }
    };
    if outcome
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<HostCleanupError>())
    {
        slot.0.store(VIEWER_POISONED, Ordering::Release);
    }
    drop(slot);
    outcome
}

/// Counts one in-flight connection thread against `MAX_PENDING_HANDSHAKES`
/// until authentication finishes or fails.
struct PendingHandshake(Arc<std::sync::atomic::AtomicUsize>);

impl PendingHandshake {
    fn admit(pending: &Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        let mut count = pending.load(Ordering::Acquire);
        while count < MAX_PENDING_HANDSHAKES {
            // Recheck the cap after contention (or a spurious failure). Only
            // the successful CAS owns a permit; keep the MSRV at Rust 1.89.
            match pending.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(Arc::clone(pending))),
                Err(observed) => count = observed,
            }
        }
        None
    }
}

impl Drop for PendingHandshake {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Accept connections until shutdown, authenticating each on its own thread.
///
/// The viewer slot is claimed only by a peer that completed the handshake and
/// sent its `Hello`: an idle or unauthenticated connection used to hold it for
/// the whole handshake timeout, so a real viewer arriving meanwhile was told
/// the host was busy, and the "busy" handshake ran inline on this thread,
/// stalling every later connection. Authenticating in parallel also means a
/// peer without the key cannot time the greeting to learn whether a session
/// is running. An authenticated peer that finds the slot taken is told so
/// and disconnected.
///
/// With `once`, the host returns the first authenticated session's own result;
/// failed or unauthenticated connections do not count.
fn accept_connections<S>(
    listener: &TcpListener,
    key: Arc<Vec<u8>>,
    shutdown: &Arc<AtomicBool>,
    once: bool,
    serve: S,
) -> RemoteResult<()>
where
    S: Fn(AuthenticatedPeer) -> RemoteResult<()> + Send + Sync + 'static,
{
    let serve = Arc::new(serve);
    let session_active = Arc::new(AtomicU8::new(VIEWER_IDLE));
    let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (finished_tx, finished_rx) = std::sync::mpsc::channel::<RemoteResult<()>>();
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut stopped_by_shutdown = false;

    let result = loop {
        if shutdown.load(Ordering::Acquire) {
            stopped_by_shutdown = true;
            break Ok(());
        }
        if once && let Ok(outcome) = finished_rx.try_recv() {
            // `--once` is documented for tests and scripts, so a failed
            // session has to be distinguishable from a clean one by exit code.
            break outcome;
        }
        workers.retain(|worker| !worker.is_finished());

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
            Err(error) => break Err(error.into()),
        };
        eprintln!("jwm-remote: connection from {peer}");
        // Accepted sockets inherit the listener's non-blocking mode on some
        // platforms; every handshake below relies on blocking reads.
        if let Err(error) = stream.set_nonblocking(false) {
            eprintln!("jwm-remote: dropped {peer}: {error}");
            continue;
        }
        let Some(admission) = PendingHandshake::admit(&pending) else {
            eprintln!("jwm-remote: dropped {peer}: too many unauthenticated connections");
            continue;
        };
        let worker_key = Arc::clone(&key);
        let worker_serve = Arc::clone(&serve);
        let worker_slot = Arc::clone(&session_active);
        let worker_finished = finished_tx.clone();
        let spawned = thread::Builder::new()
            .name("jwm-remote-session".into())
            .spawn(move || {
                let authenticated = authenticate_peer(stream, &worker_key);
                drop(admission);
                let mut peer_session = match authenticated {
                    Ok(peer_session) => peer_session,
                    Err(error) => {
                        eprintln!("jwm-remote: {peer} was not admitted: {error}");
                        return;
                    }
                };
                let Some(slot) = ViewerSlot::claim(&worker_slot) else {
                    let close = admission_refusal(worker_slot.load(Ordering::Acquire));
                    let reason = &close.reason;
                    let refused = peer_session
                        .writer
                        .write_message(MessageKind::SessionClose, &close.encode())
                        .and_then(|()| peer_session.writer.flush());
                    match refused {
                        Ok(()) => {
                            eprintln!("jwm-remote: refused {peer}: {reason}")
                        }
                        Err(error) => eprintln!("jwm-remote: refused {peer}: {error}"),
                    }
                    return;
                };
                let outcome = run_admitted_session(slot, || worker_serve(peer_session));
                if let Err(error) = &outcome {
                    eprintln!("jwm-remote: session with {peer} ended: {error}");
                }
                let _ = worker_finished.send(outcome);
            });
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(error) => eprintln!("jwm-remote: dropped {peer}: {error}"),
        }
    };
    // A signal during a session still waits for it to unwind: the session owns
    // releasing any input it injected. Its result is reported but not
    // propagated -- the operator asked the host to stop, so stopping is a
    // success, and a long-running host must not start exiting nonzero just
    // because whatever session happened to be open was cut short.
    // Unauthenticated handshakes end within their own timeout.
    for worker in workers {
        if worker.join().is_err() {
            eprintln!("jwm-remote: connection thread panicked");
        }
    }
    // A simultaneous shutdown must not hide a failed admitted --once session.
    if once && stopped_by_shutdown {
        finished_rx.try_recv().unwrap_or(result)
    } else {
        result
    }
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

/// A peer that proved it holds the key and sent a well-formed `Hello`.
struct AuthenticatedPeer {
    reader: SessionReader<TcpStream>,
    writer: SessionWriter<DeadlineWriter<TcpStream>>,
    hello: ClientHello,
}

/// Run the handshake and read the peer's `Hello`, all under one absolute
/// `HANDSHAKE_TIMEOUT` so a peer that drips bytes cannot extend it.
fn authenticate_peer(mut stream: TcpStream, key: &[u8]) -> RemoteResult<AuthenticatedPeer> {
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
    let hello = match ClientHello::decode(&hello_payload) {
        Ok(hello) => hello,
        Err(error) => {
            // Usually a viewer from another release. It holds the key, and
            // every release prints a refusal reason, so say which version
            // this host speaks rather than hanging up on it.
            decline_legacy_session(
                &mut writer,
                &format!("{error}; update jwm-remote on both machines together"),
            );
            return Err(error);
        }
    };
    handshake_deadline.cancel();
    Ok(AuthenticatedPeer {
        reader,
        writer,
        hello,
    })
}

fn serve_authenticated(
    peer: AuthenticatedPeer,
    options: &HostOptions,
    shutdown: &Arc<AtomicBool>,
    window_capture: &Mutex<Option<GrantedCapture>>,
) -> RemoteResult<()> {
    let AuthenticatedPeer {
        mut reader,
        mut writer,
        hello,
    } = peer;
    // Both sides must have opted in: the host's flag is policy, the client's
    // request is consent. Sending clipboard records to a peer that asked for
    // none would be a protocol violation on arrival.
    let clipboard_policy = clipboard_policy::intersect(
        (
            hello.stream.clipboard_content,
            hello.stream.clipboard_directions,
        ),
        clipboard_policy::requested(
            options.allow_clipboard,
            options.clipboard_mode,
            options.clipboard_direction,
            options.clipboard_file_limit,
        ),
    );
    let clipboard_enabled = clipboard_policy.0 != 0;
    let file_limit = hello
        .stream
        .negotiate_file_limit(options.clipboard_file_limit);
    let files_enabled = clipboard_policy.0 & 4 != 0 && file_limit > 0;
    let capture_result =
        if options.wayland_select || matches!(options.capture_area, CaptureArea::Window(_)) {
            window_capture
                .lock()
                .unwrap()
                .take()
                .map(GrantedCapture::into_capture)
                .ok_or_else(|| permission_denied("window instance grant already consumed").into())
        } else {
            X11Capture::connect(
                options.display.as_deref(),
                options.capture_source,
                options.max_width,
                options.capture_area.clone(),
                options.scaling,
            )
            .map(|capture| HostCapture::Desktop(Box::new(capture)))
        };
    let mut capture = match capture_result {
        Ok(capture) => capture,
        Err(error) => {
            // Dropping the socket here left the viewer with a bare
            // end-of-file and a hint about keys and versions. It has proved
            // it holds the key, so it may learn why the host cannot serve it.
            decline_session(&mut writer, capture_refusal(&error.to_string()));
            return Err(error);
        }
    };
    let frame_limits = hello.stream.frame_limits.intersect(options.frame_limits);
    capture.set_frame_limits(frame_limits);
    if let Some(requested) = hello.initial_viewport {
        let (width, height) = effective_encoded_bounds(options.max_width, requested);
        capture.set_encoded_bounds(width, height);
    }
    let mut negotiated_stream = hello.stream;
    negotiated_stream.frame_limits = frame_limits;
    negotiated_stream.scroll_copy = hello.stream.negotiate_scroll_copy(options.scroll_copy);
    negotiated_stream.static_lossless =
        hello.stream.static_lossless && options.allow_static_lossless;
    if hello.stream.static_lossless
        && (hello.request_input || hello.request_clipboard || hello.request_clipboard_files)
    {
        return Err(invalid_data(
            "static lossless cannot combine legacy input or clipboard requests",
        )
        .into());
    }

    negotiated_stream.window_resize = hello.stream.window_resize && options.allow_window_resize;
    negotiated_stream.wayland_keyboard =
        hello.stream.wayland_keyboard && options.allow_wayland_keyboard;
    negotiated_stream.wayland_pointer =
        hello.stream.wayland_pointer && options.allow_wayland_pointer;
    negotiated_stream.wayland_text = hello.stream.wayland_text && options.allow_wayland_text;
    if negotiated_stream.wayland_text {
        if hello.request_input
            || hello.request_clipboard
            || hello.stream.window_resize
            || hello.stream.wayland_keyboard
            || hello.stream.wayland_pointer
            || hello.stream.wayland_control
        {
            return Err(invalid_data("text cannot combine other input permissions").into());
        }
        if let Err(error) = capture.enable_text() {
            decline_session(
                &mut writer,
                capture_refusal("native text context unavailable or refused"),
            );
            return Err(error);
        }
    }
    negotiated_stream.wayland_control =
        hello.stream.wayland_control && options.allow_wayland_control;
    negotiated_stream.wayland_scroll = hello.stream.wayland_scroll
        && options.allow_wayland_scroll
        && (negotiated_stream.wayland_pointer || negotiated_stream.wayland_control);
    negotiated_stream.wayland_horizontal_scroll = hello.stream.wayland_horizontal_scroll
        && options.allow_wayland_horizontal_scroll
        && (negotiated_stream.wayland_pointer || negotiated_stream.wayland_control);
    if negotiated_stream.wayland_control {
        if hello.request_input
            || hello.request_clipboard
            || hello.stream.window_resize
            || hello.stream.wayland_keyboard
            || hello.stream.wayland_pointer
        {
            return Err(invalid_data("control cannot combine other input capabilities").into());
        }
        capture.enable_control(
            negotiated_stream.wayland_scroll,
            negotiated_stream.wayland_horizontal_scroll,
        )?;
    }
    if negotiated_stream.wayland_pointer {
        if hello.request_input
            || hello.request_clipboard
            || hello.stream.window_resize
            || hello.stream.wayland_keyboard
        {
            return Err(invalid_data("native pointer cannot combine other capabilities").into());
        }
        capture.enable_pointer()?;
        if negotiated_stream.wayland_horizontal_scroll {
            capture.enable_horizontal_scroll()?;
        }
        if negotiated_stream.wayland_scroll {
            capture.enable_scroll()?;
        }
    }
    if negotiated_stream.wayland_keyboard {
        if hello.request_input || hello.request_clipboard || hello.stream.window_resize {
            return Err(
                invalid_data("Wayland keyboard cannot combine other remote capabilities").into(),
            );
        }
        capture.enable_keyboard()?;
    }

    if !negotiated_stream.window_resize {
        capture.disable_native_resize();
    }
    let can_separate = hello.stream.cursor == super::messages::CursorMode::Independent
        && capture.enable_cursor_plane();
    negotiated_stream.cursor = hello.stream.cursor.negotiate(can_separate);
    // Injected pointer coordinates arrive in shared-area space; XTEST wants
    // root coordinates. Resolved once here rather than per event.
    let input_area = capture.input_area();
    let mut keyboard_enabled = false;
    let mut verified_keymap = None;
    let injector = if options.allow_input && hello.request_input {
        match InputInjector::connect(options.display.as_deref()) {
            Ok(injector) => {
                match injector.keymap() {
                    Ok(host_keymap) => match hello.keymap.compare(&host_keymap) {
                        KeymapMatch::Compatible => {
                            keyboard_enabled = true;
                            // Mid-session checks compare the host's map
                            // against the viewer's, as negotiated.
                            verified_keymap = Some(hello.keymap.clone());
                        }
                        mismatch => eprintln!(
                            "jwm-remote: peer X11 keymap differs ({mismatch}); keyboard disabled, \
                             pointer remains enabled"
                        ),
                    },
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
    // Clipboard sharing needs its own X connection and threads; the watcher
    // must not be able to delay a frame, and a conversion blocks until the
    // current selection owner answers. The host does not push what was
    // already copied here: the viewer's clipboard is the one shared on
    // connect, so the two machines do not swap clipboards.
    let (mut clipboard_sync, clipboard_inbox, clipboard_outbox) = if clipboard_enabled {
        match ClipboardSync::start(
            options.display.as_deref(),
            SyncOptions {
                send: clipboard_policy.1 & 2 != 0,
                receive: clipboard_policy.1 & 1 != 0,
                images: clipboard_policy.0 & 2 != 0,
                files: files_enabled,
                file_limit,
                share_existing: false,
            },
            None,
        ) {
            Ok((sync, inbox, outbox)) => {
                eprintln!(
                    "jwm-remote: clipboard sharing enabled ({})",
                    clipboard_policy::label(clipboard_policy.0, clipboard_policy.1)
                );
                (Some(sync), Some(inbox), Some(outbox))
            }
            Err(error) => {
                // Startup currently returns an opaque error after potentially
                // spawning threads. It cannot certify that no ownership remains,
                // even when the failure was simply an unavailable X11 backend.
                decline_session(&mut writer, clipboard_refusal());
                return Err(host_clipboard_start_failed(error));
            }
        }
    } else {
        (None, None, None)
    };
    let result = (|| -> RemoteResult<()> {
        let clipboard_enabled = clipboard_sync.is_some();
        let files_enabled = files_enabled && clipboard_enabled;
        negotiated_stream.clipboard_file_limit = if files_enabled { file_limit } else { 0 };
        negotiated_stream.clipboard_content = if clipboard_enabled {
            clipboard_policy.0
        } else {
            0
        };
        negotiated_stream.clipboard_directions = if clipboard_enabled {
            clipboard_policy.1
        } else {
            0
        };
        if clipboard_enabled {
            reader.set_max_payload_len(MAX_INBOUND_CLIPBOARD_PAYLOAD_LEN);
        } else if negotiated_stream.wayland_text {
            reader.set_max_payload_len(super::wayland_text_wire::MAX_TEXT_BYTES + 26);
        }
        let grants = host_source_grants(options)?;
        let catalog = SourceCatalog {
            current: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            entries: grants
                .iter()
                .map(|(id, area)| SourceEntry {
                    id: *id,
                    label: if options.wayland_select {
                        "Authorized Wayland surface".into()
                    } else {
                        source_label(area)
                    },
                })
                .collect(),
        };
        let sources = Arc::new(SourceControl::new(
            grants,
            input_area,
            capture.geometry_epoch(),
        )?);
        if input_wake::enabled()
            && (negotiated_stream.wayland_text
                || negotiated_stream.wayland_control
                || negotiated_stream.wayland_keyboard
                || negotiated_stream.wayland_pointer)
        {
            sources
                .input_wake
                .set(input_wake::InputWake::new()?)
                .map_err(|_| io::Error::other("native input wake initialized twice"))?;
        }
        sources
            .follow_geometry
            .store(options.wayland_resize_follow, Ordering::Release);
        sources
            .text_enabled
            .store(negotiated_stream.wayland_text, Ordering::Release);
        sources
            .control_enabled
            .store(negotiated_stream.wayland_control, Ordering::Release);
        sources
            .resize_enabled
            .store(negotiated_stream.window_resize, Ordering::Release);
        sources
            .keyboard_enabled
            .store(negotiated_stream.wayland_keyboard, Ordering::Release);
        sources
            .pointer_enabled
            .store(negotiated_stream.wayland_pointer, Ordering::Release);
        sources
            .scroll_enabled
            .store(negotiated_stream.wayland_scroll, Ordering::Release);
        sources.horizontal_scroll_enabled.store(
            negotiated_stream.wayland_horizontal_scroll,
            Ordering::Release,
        );
        sources
            .static_lossless
            .store(negotiated_stream.static_lossless, Ordering::Release);
        sources
            .scroll_copy
            .store(negotiated_stream.scroll_copy, Ordering::Release);
        writer.write_message(
            MessageKind::HelloAck,
            &ServerHello {
                stream: negotiated_stream,
                pointer_enabled,
                keyboard_enabled,
                clipboard_enabled,
                clipboard_files_enabled: files_enabled,
            }
            .encode(),
        )?;
        writer.write_message(MessageKind::SourceCatalog, &catalog.encode()?)?;
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
        let mut telemetry = HostTelemetry::new();
        telemetry.stats_json = options.stats_json;
        let telemetry = Arc::new(telemetry);
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

        let writer: SharedWriter = Arc::new(Mutex::new(writer));
        let input_writer = Arc::clone(&writer);
        let input_running = Arc::clone(&running);
        let input_stop = Arc::clone(&first_stop);
        let input_credits = Arc::clone(&credits);
        let input_telemetry = Arc::clone(&telemetry);
        let input_jpeg_quality = Arc::clone(&jpeg_quality);
        let viewport = Arc::new(ViewportRequest::new());
        viewport.seed(hello.initial_viewport);
        let input_viewport = Arc::clone(&viewport);
        let input_sources = Arc::clone(&sources);
        let input_thread = thread::Builder::new()
            .name("jwm-remote-input".into())
            .spawn(move || {
                let _stop_on_exit = StopSessionOnDrop(Arc::clone(&input_running));
                receive_input_scoped(
                    reader,
                    injector,
                    input_area,
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
                    clipboard_inbox.as_ref(),
                    Some(&input_sources),
                )
                .report(|close| say_session_close(&input_writer, close))
            })?;

        let stream_result = stream_frames(
            &sources,
            &viewport,
            clipboard_outbox,
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
        // This bounds post-stream cleanup only. Synchronous capture and the
        // sender/outer connection joins have separate, potentially unbounded work.
        let result = cleanup_host_session(
            Instant::now() + HOST_CLEANUP_TIMEOUT,
            first_stop.cause(),
            stream_result,
            input_thread,
            |remaining| {
                clipboard_sync
                    .take()
                    .map_or(Ok(()), |mut sync| sync.shutdown_timeout(remaining))
            },
        );
        report_host_final(&telemetry, &credits, Instant::now());
        result
    })();
    // Setup errors after clipboard startup must also confirm its shutdown.
    // The normal post-stream path takes the sync, so it is never joined twice.
    let cleanup = clipboard_sync
        .as_mut()
        .map_or(Ok(()), |sync| sync.shutdown_timeout(HOST_CLEANUP_TIMEOUT));
    combine_host_cleanup(result, cleanup)
}

fn host_clipboard_start_failed(error: String) -> super::RemoteError {
    Box::new(HostCleanupError {
        primary: Some(io::Error::other(format!("clipboard startup failed: {error}")).into()),
        cleanup: io::Error::other("clipboard startup cleanup unconfirmed; host admission disabled")
            .into(),
    })
}

/// A reaper acknowledges *join*, not just return from the worker body: thread
/// local destructors may still own input resources or block indefinitely.
fn join_host_input_until(
    worker: thread::JoinHandle<RemoteResult<()>>,
    deadline: Instant,
) -> RemoteResult<thread::Result<RemoteResult<()>>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    thread::Builder::new()
        .name("jwm-remote-input-reaper".into())
        .spawn(move || {
            let _ = tx.send(worker.join());
        })
        .map_err(|error| {
            io::Error::other(format!("start input reaper (worker detached): {error}"))
        })?;
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| io::Error::other(format!(
            "input cleanup unconfirmed ({error}); worker/reaper may remain detached; input release is not confirmed"
        )).into())
}

fn cleanup_host_session(
    deadline: Instant,
    cause: StopCause,
    stream_result: RemoteResult<()>,
    input: thread::JoinHandle<RemoteResult<()>>,
    clipboard_cleanup: impl FnOnce(Duration) -> RemoteResult<()>,
) -> RemoteResult<()> {
    let input_result = join_host_input_until(input, deadline);
    // Always signal and attempt clipboard shutdown, even after the input
    // consumed the whole budget. Never grant a fresh timeout to later work.
    let clipboard_result = clipboard_cleanup(deadline.saturating_duration_since(Instant::now()));
    match input_result {
        Ok(input_result) => {
            finish_host_session(cause, stream_result, input_result, clipboard_result)
        }
        Err(cleanup) => {
            let result = Err(Box::new(HostCleanupError {
                primary: stream_result.err(),
                cleanup,
            }) as super::RemoteError);
            combine_host_cleanup(result, clipboard_result)
        }
    }
}

fn combine_host_cleanup(result: RemoteResult<()>, cleanup: RemoteResult<()>) -> RemoteResult<()> {
    match cleanup {
        Ok(()) => result,
        Err(cleanup) => Err(Box::new(HostCleanupError {
            primary: result.err(),
            cleanup,
        })),
    }
}

fn finish_host_session(
    cause: StopCause,
    stream_result: RemoteResult<()>,
    input_result: thread::Result<RemoteResult<()>>,
    clipboard_result: RemoteResult<()>,
) -> RemoteResult<()> {
    let input_panicked = input_result.is_err();
    let input_result = match input_result {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("remote input thread panicked").into()),
    };
    let result = if input_panicked {
        // A panic can interrupt input release. Preserve an earlier stream
        // failure while marking this listener unsafe for replacement sessions.
        Err(Box::new(HostCleanupError {
            primary: stream_result.err(),
            cleanup: input_result.unwrap_err(),
        }) as super::RemoteError)
    } else if input_result
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<HostCleanupError>())
    {
        // Even a joined input worker may report an unconfirmed X11 release.
        // Do not hide that failure behind graceful/capture/sender termination.
        combine_host_cleanup(stream_result, input_result)
    } else {
        match cause {
            StopCause::Input => input_result,
            StopCause::Graceful => {
                // Shutting the socket down is what ends the input reader, so the
                // end of stream it reports is the expected outcome, not news.
                if let Err(error) = input_result
                    && !is_end_of_stream(&error)
                {
                    eprintln!("jwm-remote: input receiver stopped during shutdown: {error}");
                }
                stream_result
            }
            StopCause::Capture
            | StopCause::Sender
            | StopCause::Clipboard
            | StopCause::ThreadPanic
            | StopCause::None => {
                if let Err(error) = input_result {
                    eprintln!("jwm-remote: input receiver stopped: {error}");
                }
                stream_result
            }
        }
    };
    combine_host_cleanup(result, clipboard_result)
}

/// Whether this error is the host's typed unconfirmed-cleanup result.
/// This does not infer cleanup state from arbitrary error text.
#[must_use]
pub fn cleanup_incomplete(error: &(dyn std::error::Error + 'static)) -> bool {
    error.is::<HostCleanupError>()
}

#[derive(Debug)]
struct HostCleanupError {
    primary: Option<super::RemoteError>,
    cleanup: super::RemoteError,
}

impl std::fmt::Display for HostCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(primary) = &self.primary {
            write!(f, "{primary}; ")?;
        }
        write!(f, "{}", self.cleanup)
    }
}
impl std::error::Error for HostCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.primary.as_deref().map(|error| error as _)
    }
}

fn is_end_of_stream(error: &super::RemoteError) -> bool {
    match error.downcast_ref::<ProtocolError>() {
        Some(ProtocolError::Io(error)) => error.kind() == io::ErrorKind::UnexpectedEof,
        _ => error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof),
    }
}

/// Internal notification choice. Encoding and input/keymap failures have no
/// structured v13 code and use the existing terminal legacy Close.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RuntimeClose {
    Structured(SessionClose),
    Encoding,
    InputUnavailable,
    KeymapUnavailable,
    KeymapChanged,
}

impl From<SessionClose> for RuntimeClose {
    fn from(close: SessionClose) -> Self {
        Self::Structured(close)
    }
}

/// A known local producer failure, not an arbitrary error in its surrounding
/// loop (which also writes to the transport and coordinates other workers).
/// Keep the detailed error local; the wire gets only a fixed public reason.
#[derive(Debug)]
struct LocalRuntimeFailure {
    close: RuntimeClose,
    source: super::RemoteError,
}

impl LocalRuntimeFailure {
    /// Injection and event polling also report peer-data validation failures.
    /// Only actual X11 transport/reply failures establish backend loss.
    fn input(source: InputError) -> super::RemoteError {
        match source {
            source @ (InputError::Connection(_) | InputError::Reply(_)) => Box::new(Self {
                close: RuntimeClose::InputUnavailable,
                source: source.into(),
            }),
            source => source.into(),
        }
    }

    fn keymap(source: super::RemoteError) -> super::RemoteError {
        Box::new(Self {
            close: RuntimeClose::KeymapUnavailable,
            source,
        })
    }

    fn encoding(source: super::RemoteError) -> super::RemoteError {
        Box::new(Self {
            close: RuntimeClose::Encoding,
            source,
        })
    }

    fn capture(source: super::RemoteError) -> super::RemoteError {
        Box::new(Self {
            close: SessionClose::new(
                SessionCloseCode::CaptureUnavailable,
                "host capture became unavailable",
            )
            .into(),
            source,
        })
    }

    fn clipboard(source: super::RemoteError) -> super::RemoteError {
        Box::new(Self {
            close: SessionClose::new(
                SessionCloseCode::ClipboardUnavailable,
                "host clipboard became unavailable",
            )
            .into(),
            source,
        })
    }
}

impl std::fmt::Display for LocalRuntimeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for LocalRuntimeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn runtime_close(error: &super::RemoteError) -> Option<RuntimeClose> {
    error
        .downcast_ref::<LocalRuntimeFailure>()
        .map(|failure| failure.close.clone())
}

/// Tell the viewer why the session ended, best effort. Never wait for an
/// occupied/poisoned mutex or append to a failed authenticated record.
fn say_session_close<W: std::io::Write + SetWriteTimeout>(
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    close: impl Into<RuntimeClose>,
) {
    if let Ok(mut writer) = writer.try_lock() {
        let (kind, payload) = match close.into() {
            RuntimeClose::Structured(close) => (MessageKind::SessionClose, close.encode()),
            RuntimeClose::Encoding => (
                MessageKind::Close,
                ENCODING_FAILURE_REASON.as_bytes().to_vec(),
            ),
            RuntimeClose::InputUnavailable => {
                (MessageKind::Close, INPUT_FAILURE_REASON.as_bytes().to_vec())
            }
            RuntimeClose::KeymapUnavailable => (
                MessageKind::Close,
                KEYMAP_VERIFY_FAILURE_REASON.as_bytes().to_vec(),
            ),
            RuntimeClose::KeymapChanged => (
                MessageKind::Close,
                KEYMAP_CHANGED_REASON.as_bytes().to_vec(),
            ),
        };
        let _ = write_session_record(&mut writer, kind, &payload, GOODBYE_WRITE_DEADLINE);
    }
}

/// Wake the input reader before spending any time on the outgoing close, so
/// held-input release does not wait behind this best-effort write. Callers
/// publish stopped state and close mailbox/credit waiters first. Another stop
/// path may shut down the socket concurrently; delivery is never guaranteed.
fn shutdown_stream<W: std::io::Write + SetWriteTimeout>(
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    close: Option<RuntimeClose>,
    mut shutdown: impl FnMut(Shutdown),
) {
    shutdown(Shutdown::Read);
    if let Some(close) = close {
        say_session_close(writer, close);
    }
    shutdown(Shutdown::Both);
}

fn capture_stop_notice(
    result: &RemoteResult<()>,
    shutdown: bool,
    first_stop: &FirstStop,
) -> Option<RuntimeClose> {
    if let Err(error) = result {
        // The loop also carries mailbox, source-state and transport errors.
        // Only tagged backend operations can claim CaptureUnavailable.
        return first_stop
            .record(StopCause::Capture)
            .then(|| runtime_close(error))
            .flatten();
    }
    if shutdown {
        first_stop.record(StopCause::Graceful);
    }
    (first_stop.cause() == StopCause::Graceful)
        .then(|| SessionClose::new(SessionCloseCode::HostStopping, HOST_STOPPING_REASON).into())
}

fn admission_refusal(state: u8) -> SessionClose {
    if state == VIEWER_POISONED {
        SessionClose::new(
            SessionCloseCode::AdmissionDisabled,
            "host cleanup is incomplete; restart the host before reconnecting",
        )
    } else {
        SessionClose::new(SessionCloseCode::Busy, BUSY_REASON)
    }
}

fn capture_refusal(error: &str) -> SessionClose {
    SessionClose::new(
        SessionCloseCode::CaptureUnavailable,
        &format!("host cannot capture its display: {error}"),
    )
}

fn clipboard_refusal() -> SessionClose {
    SessionClose::new(
        SessionCloseCode::ClipboardUnavailable,
        "host clipboard startup failed; restart the host before reconnecting",
    )
}

/// Tell a version-compatible viewer why its session cannot start, best effort.
fn decline_session<W: std::io::Write>(writer: &mut SessionWriter<W>, close: SessionClose) {
    let _ = writer
        .write_message(MessageKind::SessionClose, &close.encode())
        .and_then(|()| writer.flush());
}

/// Old or malformed Hello records cannot negotiate structured close support.
fn decline_legacy_session<W: std::io::Write>(writer: &mut SessionWriter<W>, reason: &str) {
    let mut end = reason.len().min(MAX_SETUP_REASON_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let _ = writer
        .write_message(MessageKind::Close, &reason.as_bytes()[..end])
        .and_then(|()| writer.flush());
}

/// The session's single wire writer.
///
/// Records must not interleave and sequence numbers come from one writer, so
/// clipboard traffic shares the video sender's writer under a mutex rather
/// than opening a second one. The lock is held for exactly one record, and
/// clipboard records are rare and tiny.
type SharedWriter = Arc<Mutex<SessionWriter<DeadlineWriter<TcpStream>>>>;

fn send_resize_result(writer: &SharedWriter, result: ResizeResult) -> RemoteResult<()> {
    write_session_record(
        &mut lock_writer(writer),
        MessageKind::WindowResizeResult,
        &result.encode()?,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}

fn send_pointer_status(writer: &SharedWriter, status: PointerStatus) -> RemoteResult<()> {
    write_session_record(
        &mut lock_writer(writer),
        MessageKind::WaylandPointerStatus,
        &status.encode()?,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
fn stop_pointer(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostPointerState,
) -> RemoteResult<()> {
    sources.pointer_enabled.store(false, Ordering::Release);
    sources.pointer_revoke.store(true, Ordering::Release);
    sources.pointer_queue.lock().unwrap().clear();
    let frame = sources.pointer_frame.lock().unwrap().take();
    state.revoke();
    capture.revoke_pointer();
    if let Some(scope) = frame.and_then(|frame| frame.scope).or(state.scope()) {
        send_pointer_status(
            writer,
            PointerStatus {
                lease_epoch: state.epoch().unwrap_or(0),
                scope,
                status: PointerStatusKind::Revoked,
            },
        )?;
    }
    Ok(())
}
fn process_pointer(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostPointerState,
    size: (u16, u16),
) -> RemoteResult<()> {
    if !sources.pointer_enabled.load(Ordering::Acquire) {
        return Ok(());
    }
    let now = Instant::now();
    if sources.pointer_revoke.load(Ordering::Acquire)
        || capture.pointer_revoked()
        || (state.epoch().is_some() && !state.active(now))
    {
        #[cfg(test)]
        eprintln!(
            "pointer host stop: remote_revoke={}, native_revoke={}, expired={}",
            sources.pointer_revoke.load(Ordering::Acquire),
            capture.pointer_revoked(),
            state.epoch().is_some() && !state.active(now)
        );
        return stop_pointer(sources, capture, writer, state);
    }
    let tracked = *sources.pointer_frame.lock().unwrap();
    let drawn = sources.state.lock().unwrap().presented_frame;
    let command = sources.pointer_queue.lock().unwrap().pop_front();
    if let Some(command) = command {
        let result = (|| -> RemoteResult<()> {
            let frame = tracked.ok_or_else(|| invalid_data("missing pointer capture proof"))?;
            let scope = frame
                .scope
                .ok_or_else(|| invalid_data("pointer frame not published"))?;
            if drawn != Some(scope)
                || capture.pointer_frame().map(|f| f.0) != Some(frame.serial)
                || now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE
            {
                return Err(invalid_data("pointer action lacks a fresh drawn frame").into());
            }
            match command {
                PointerCommand::Request(request) => {
                    state.request(request, scope)?;
                    let epoch = capture.grant_pointer()?;
                    state.grant(epoch, Instant::now())?;
                    let ready = state.ready(scope, frame.serial, frame.at, Instant::now())?;
                    if let Some(frame) = sources.pointer_frame.lock().unwrap().as_mut() {
                        frame.announced = true;
                    }
                    send_pointer_status(writer, ready)?;
                }
                PointerCommand::Action(action) => {
                    if !frame.announced {
                        return Err(invalid_data("pointer frame is not ready").into());
                    }
                    let serial = state.consume(action, size, Instant::now())?;
                    capture.pointer_action(&action, serial)?;
                    sources.pointer_frame.lock().unwrap().take();
                    send_pointer_status(
                        writer,
                        PointerStatus {
                            lease_epoch: action.lease_epoch,
                            scope,
                            status: PointerStatusKind::Waiting,
                        },
                    )?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            stop_pointer(sources, capture, writer, state)?;
            return Err(error);
        }
        return Ok(());
    }
    if let Some(frame) = tracked {
        let expired = now.saturating_duration_since(frame.at) >= FRAME_MAX_AGE;
        // The owned-viewer regression may only shorten this one session's
        // proof lifetime. It still requires a real new capture and drawn ACK.
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        let forced_refresh = host_viewer_integration::take_forced_refresh();
        #[cfg(not(all(test, feature = "remote-wayland", feature = "backend-wayland-udev")))]
        let forced_refresh = false;
        if expired || forced_refresh {
            // Only the controlled test refresh omits this advisory notification,
            // isolating cancellation by a real new frame rather than Waiting.
            // Host/native permission is still discarded before any new capture.
            if !forced_refresh && let (Some(epoch), Some(scope)) = (state.epoch(), frame.scope) {
                send_pointer_status(
                    writer,
                    PointerStatus {
                        lease_epoch: epoch,
                        scope,
                        status: PointerStatusKind::Waiting,
                    },
                )?;
            }
            state.clear_ready();
            sources.pointer_frame.lock().unwrap().take();
            capture.discard_pointer_frame();
        } else if state.active(now)
            && !frame.announced
            && frame.scope.is_some()
            && drawn == frame.scope
        {
            let status = state.ready(frame.scope.unwrap(), frame.serial, frame.at, now)?;
            if let Some(frame) = sources.pointer_frame.lock().unwrap().as_mut() {
                frame.announced = true;
            }
            send_pointer_status(writer, status)?;
        }
    }
    Ok(())
}

fn send_keyboard_status(writer: &SharedWriter, status: LeaseStatus) -> RemoteResult<()> {
    write_session_record(
        &mut lock_writer(writer),
        MessageKind::WaylandKeyboardStatus,
        &status.encode()?,
        FRAME_WRITE_DEADLINE,
    )?;
    Ok(())
}
fn stop_keyboard(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostKeyboardState,
) -> RemoteResult<()> {
    sources.keyboard_enabled.store(false, Ordering::Release);
    sources.keyboard_revoke.store(true, Ordering::Release);
    sources.keyboard_queue.lock().unwrap().clear();
    let _ = state.revoke(); // Native lease owns cleanup; never global key-up.
    capture.revoke_keyboard();
    if let Some(mut status) = state.status() {
        if status.lease_epoch == 0 {
            status.status = LeaseStatusKind::Denied;
        }
        send_keyboard_status(writer, status)?;
    }
    Ok(())
}
fn process_keyboard(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
    state: &mut HostKeyboardState,
) -> RemoteResult<()> {
    if !sources.keyboard_enabled.load(Ordering::Acquire) {
        return Ok(());
    }
    let current = {
        let source = sources.state.lock().unwrap();
        DrawnScope {
            source_epoch: source.policy.current().epoch,
            geometry_epoch: capture.geometry_epoch(),
            frame_sequence: 0,
        }
    };
    if sources.keyboard_revoke.load(Ordering::Acquire)
        || capture.keyboard_revoked()
        || (state.status().is_some_and(|s| s.lease_epoch != 0)
            && state.check_scope(current, Instant::now()).is_err())
    {
        return stop_keyboard(sources, capture, writer, state);
    }
    // One native edge per capture tick bounds work and lets revocations dispatch.
    let command = sources.keyboard_queue.lock().unwrap().pop_front();
    let Some(command) = command else {
        return Ok(());
    };
    let result = match command {
        KeyboardCommand::Request(request) => {
            let drawn = sources.state.lock().unwrap().presented_frame;
            // A missing/stale presentation claim is a protocol violation, not
            // a native denial. End the session explicitly; otherwise a rejected
            // request with no recorded scope could silently stream forever.
            if let Err(error) = admit_keyboard_request(state, request, drawn, current) {
                stop_keyboard(sources, capture, writer, state)?;
                return Err(error);
            }
            (|| -> RemoteResult<()> {
                let epoch = capture.arm_keyboard(request.keymap_digest)?;
                if sources.keyboard_revoke.load(Ordering::Acquire) {
                    return Err(invalid_data("keyboard canceled while arming").into());
                }
                let status = state.grant(epoch, Instant::now())?;
                send_keyboard_status(writer, status)
            })()
        }
        KeyboardCommand::Key(key) => (|| -> RemoteResult<()> {
            state.key(key, current, &keyboard_allowlist(), Instant::now())?;
            capture.keyboard_key(&key)
        })(),
    };
    if result.is_err() {
        stop_keyboard(sources, capture, writer, state)?;
    }
    Ok(())
}
fn admit_keyboard_request(
    state: &mut HostKeyboardState,
    request: LeaseRequest,
    drawn: Option<DrawnScope>,
    current: DrawnScope,
) -> RemoteResult<()> {
    let result = (|| {
        let drawn = drawn.ok_or_else(|| invalid_data("keyboard source was not drawn"))?;
        if drawn.source_epoch != current.source_epoch
            || drawn.geometry_epoch != current.geometry_epoch
        {
            return Err(invalid_data("keyboard drawn geometry is stale").into());
        }
        state.request(request, drawn)
    })();
    if result.is_err() {
        let _ = state.revoke();
    }
    result
}

fn keyboard_allowlist() -> BTreeSet<u16> {
    #[cfg(feature = "remote-wayland")]
    {
        (1..=247)
            .filter(|key| crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(*key)))
            .collect()
    }
    #[cfg(not(feature = "remote-wayland"))]
    {
        BTreeSet::new()
    }
}

fn stream_frames(
    sources: &Arc<SourceControl>,
    viewport: &ViewportRequest,
    clipboard_outbox: Option<ClipboardOutbox>,
    capture: &mut HostCapture,
    writer: SharedWriter,
    control: &TcpStream,
    running: &Arc<AtomicBool>,
    first_stop: &Arc<FirstStop>,
    credits: &Arc<FrameCredits>,
    telemetry: &Arc<HostTelemetry>,
    jpeg_quality: &Arc<JpegQualityController>,
    shutdown: &AtomicBool,
    options: &HostOptions,
) -> RemoteResult<()> {
    let sender_sources = Arc::clone(sources);
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
            run_video_worker(
                || {
                    send_frames_scoped(
                        Arc::clone(&sender_mailbox),
                        Arc::clone(&sender_writer),
                        &sender_running,
                        &sender_credits,
                        &sender_telemetry,
                        &sender_jpeg_quality,
                        Some(&sender_sources),
                    )
                },
                &sender_running,
                &sender_stop,
                |close| {
                    sender_mailbox.close();
                    sender_credits.close();
                    shutdown_stream(&sender_writer, close, |how| {
                        let _ = sender_control.shutdown(how);
                    });
                },
            )
        }) {
        Ok(sender) => sender,
        Err(error) => {
            first_stop.record(StopCause::Sender);
            return Err(error.into());
        }
    };

    let clipboard_thread = clipboard_outbox.map(|outbox| -> RemoteResult<_> {
        let clipboard_control = control.try_clone()?;
        let writer = Arc::clone(&writer);
        let running = Arc::clone(running);
        let first_stop = Arc::clone(first_stop);
        let mailbox = Arc::clone(&mailbox);
        let credits = Arc::clone(credits);
        thread::Builder::new()
            .name("jwm-remote-clipboard".into())
            .spawn(move || {
                run_clipboard_worker(
                    || forward_clipboard(&outbox, &writer, &running),
                    &running,
                    &first_stop,
                    |close| {
                        mailbox.close();
                        credits.close();
                        shutdown_stream(&writer, close, |how| {
                            let _ = clipboard_control.shutdown(how);
                        });
                    },
                )
            })
            .map_err(|error| LocalRuntimeFailure::clipboard(error.into()))
    });
    // Negotiation already promised clipboard support. A failed worker start
    // must enter the ordinary teardown path, joining the video sender that
    // was started above, rather than leaving a half-working session alive.
    let (clipboard_thread, clipboard_start_result) =
        admit_clipboard_worker(clipboard_thread, running, first_stop, |close| {
            mailbox.close();
            credits.close();
            shutdown_stream(&writer, close, |how| {
                let _ = control.shutdown(how);
            });
        });

    let interval = target_frame_interval(options.fps);
    let backpressure_refresh = backpressure_refresh_interval(interval);
    // One absolute deadline bounds capture starts. X11 activity is consumed
    // during the remaining wait; once due, poll capture/cursor state without
    // tacking on a second idle interval. The Damage gate still skips readback
    // when the surface and composited cursor have not changed.
    let mut next_capture_allowed = Instant::now();
    let mut reported_mode: Option<CaptureMode> = None;
    let mut cursor_sender = CursorSender::default();
    let mut next_cursor_poll = Instant::now();
    let cursor_interval = Duration::from_millis(16);
    let mut keyboard = HostKeyboardState::new();
    let mut bundle_control = HostControl::default();
    let mut pointer = HostPointerState::default();
    let mut resize = HostResizeState::new(sources.resize_enabled.load(Ordering::Acquire));
    let mut source_size = (0u16, 0u16);
    let capture_result = (|| -> RemoteResult<()> {
        while running.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
            if let Some(wake) = sources.input_wake.get() {
                wake.drain()?;
            }
            let now = Instant::now();
            if let Some(result) = resize.expire(now) {
                capture.disable_native_resize();
                sources.resize_enabled.store(false, Ordering::Release);
                send_resize_result(&writer, result)?;
            }
            let request = sources.resize_request.lock().unwrap().take();
            if let Some(request) = request {
                let epoch = sources.state.lock().unwrap().policy.current().epoch;
                match resize.accept(request, epoch, capture.geometry_epoch(), source_size, now)? {
                    Some(result) => send_resize_result(&writer, result)?,
                    None => {
                        let applied = capture
                            .request_native_resize(source_size, (request.width, request.height));
                        let result = if let Some(expired) = resize.expire(Instant::now()) {
                            Some(expired)
                        } else {
                            match applied {
                                Ok(size) => resize.configured(size)?,
                                Err(_) => resize.fail(),
                            }
                        };
                        if !resize.enabled() {
                            capture.disable_native_resize();
                            sources.resize_enabled.store(false, Ordering::Release);
                        }
                        if let Some(result) = result {
                            send_resize_result(&writer, result)?;
                        }
                    }
                }
            }
            // Announce every capture-path transition, not just the first. Each
            // of these degradations is large and was previously visible only
            // as a single line that had long since scrolled away.
            let switch = sources.state.lock().unwrap().pending.take();
            if let Some((version, area)) = switch {
                // Input was released before this request became visible.
                // A failed install closes the session, never restores old pixels.
                capture
                    .switch_area(area)
                    .map_err(LocalRuntimeFailure::capture)?;
                let mut writer = lock_writer(&writer);
                mailbox.clear();
                credits.abandon_source();
                cursor_sender = CursorSender::default();
                {
                    let mut state = sources.state.lock().unwrap();
                    state.input_area = capture.input_area();
                    state.geometry_epoch = capture.geometry_epoch();
                    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                    if let Some(probe) = &sources.pending_lossless_probe {
                        probe
                            .publication
                            .store(state.geometry_epoch, Ordering::Release);
                    }
                    state.presented_geometry = None;
                    state
                        .policy
                        .finish_switch(version, |_| Ok::<(), io::Error>(()))??;
                }
                write_session_record(
                    &mut writer,
                    MessageKind::SourceChanged,
                    &encode_source_changed(version)?,
                    FRAME_WRITE_DEADLINE,
                )?;
                sources.changed.notify_all();
            }
            if let Some(requested) = viewport.take() {
                let (width, height) = effective_encoded_bounds(options.max_width, requested);
                if capture.set_encoded_bounds(width, height) {
                    eprintln!("jwm-remote: encoding to fit {width}x{height} for this viewer");
                }
            }
            if options.wayland_resize_follow {
                let _publication = lock_writer(&writer);
                let mut state = sources.state.lock().unwrap();
                if state.geometry_epoch != capture.geometry_epoch() {
                    state.geometry_epoch = capture.geometry_epoch();
                    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                    if let Some(probe) = &sources.pending_lossless_probe {
                        probe
                            .publication
                            .store(state.geometry_epoch, Ordering::Release);
                    }
                    state.presented_geometry = None;
                    mailbox.clear();
                }
            }
            process_keyboard(sources, capture, &writer, &mut keyboard)?;
            text::process(sources, capture, &writer)?;
            control::process(sources, capture, &writer, &mut bundle_control, source_size)?;
            process_pointer(sources, capture, &writer, &mut pointer, source_size)?;
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
            // Cursor movement has its own bounded cadence, independent of the
            // configured video FPS and video-credit backpressure.
            if capture.cursor_plane_enabled() && Instant::now() >= next_cursor_poll {
                if let Some(sample) = capture
                    .poll_cursor_plane()
                    .map_err(LocalRuntimeFailure::capture)?
                {
                    let mut writer = writer.lock().unwrap();
                    let state = sources.state.lock().unwrap();
                    let version = state.policy.current();
                    if state.policy.accepts_capture(version) {
                        drop(state);
                        cursor_sender.send_scoped(&mut writer, sample, Some(version.epoch))?;
                    }
                }
                next_cursor_poll = Instant::now() + cursor_interval;
            }

            if let Some(wait) = capture_rate_limit_wait(
                Instant::now(),
                next_capture_allowed,
                CAPTURE_RATE_LIMIT_SLICE,
            ) {
                // Drain coalesced work without sleeping, but only while a
                // capture is rate-limited. Once due, capture retains its turn
                // even under a continuous legal keyboard stream.
                if sources.input_wake.get().is_some() && sources.input_backlog() {
                    continue;
                }
                // X11 events may wake early, but cannot bypass the rate limit.
                // Recheck the absolute deadline rather than adding another
                // full wait after it. Cursor motion need not signal XDamage.
                let wait = if capture.cursor_plane_enabled() {
                    wait.min(next_cursor_poll.saturating_duration_since(Instant::now()))
                } else {
                    wait
                };
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(probe) = &sources.input_wake_probe {
                    probe.record_wait();
                }
                capture
                    .wait_for_activity_with_wake(
                        wait,
                        sources.input_wake.get().map(input_wake::InputWake::as_fd),
                    )
                    .map_err(LocalRuntimeFailure::capture)?;
                continue;
            }
            if !running.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire) {
                break;
            }
            let previous_capture_deadline = next_capture_allowed;
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
            let source_version = {
                let state = sources.state.lock().unwrap();
                let version = state.policy.current();
                if !state.policy.accepts_capture(version) {
                    continue;
                }
                version
            };
            let capture_started = Instant::now();
            let outcome = capture.frame().map_err(LocalRuntimeFailure::capture)?;
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            let legacy_retained_slots = sources.legacy_retained_proof_slots;
            #[cfg(not(all(test, feature = "remote-wayland", feature = "backend-wayland-udev")))]
            let legacy_retained_slots = false;
            let retained_blocked = matches!(outcome, CaptureOutcome::RetainedProofBlocked);
            next_capture_allowed = retained_proof_deadline(
                previous_capture_deadline,
                next_capture_allowed,
                retained_blocked,
                legacy_retained_slots,
            );
            if retained_blocked && !legacy_retained_slots {
                telemetry.record_damage_skipped();
                // read_frame already pumped native lifecycle/expiry. Wait on
                // the existing bounded activity path, not the expired FPS
                // deadline. Input/ACK hints can interrupt this wait; the next
                // loop always processes control/revocation before readback.
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(probe) = &sources.input_wake_probe {
                    probe.record_wait();
                }
                capture
                    .wait_for_activity_with_wake(
                        CAPTURE_RATE_LIMIT_SLICE,
                        sources.input_wake.get().map(input_wake::InputWake::as_fd),
                    )
                    .map_err(LocalRuntimeFailure::capture)?;
                continue;
            }
            if options.wayland_resize_follow {
                let _publication = lock_writer(&writer);
                let mut state = sources.state.lock().unwrap();
                if state.geometry_epoch != capture.geometry_epoch() {
                    state.geometry_epoch = capture.geometry_epoch();
                    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                    if let Some(probe) = &sources.pending_lossless_probe {
                        probe
                            .publication
                            .store(state.geometry_epoch, Ordering::Release);
                    }
                    state.presented_geometry = None;
                    mailbox.clear();
                }
            }
            let Some(frame) = captured_frame_from_outcome(outcome) else {
                telemetry.record_damage_skipped();
                continue;
            };
            let capture_completed_at = Instant::now();
            source_size = (frame.source_width, frame.source_height);
            if let Some(result) = resize.observe_frame(
                source_version.epoch,
                frame.geometry_epoch,
                source_size,
                capture_completed_at,
            ) {
                if !resize.enabled() {
                    capture.disable_native_resize();
                    sources.resize_enabled.store(false, Ordering::Release);
                }
                send_resize_result(&writer, result)?;
            }
            telemetry.record_captured(capture_completed_at.duration_since(capture_started));
            telemetry.record_dimensions(&frame);
            {
                let mut state = sources.state.lock().unwrap();
                if !state.policy.accepts_capture(source_version) {
                    continue;
                }
                state.geometry_epoch = frame.geometry_epoch;
                state.input_area = capture.input_area();
            }
            let pointer_serial = capture
                .control_frame()
                .or_else(|| capture.pointer_frame())
                .or_else(|| capture.text_frame())
                .map(|(serial, at)| {
                    *sources.pointer_frame.lock().unwrap() = Some(PointerFrame {
                        serial,
                        at,
                        scope: None,
                        announced: false,
                    });
                    serial
                });
            let pending = PendingFrame {
                pointer_serial,
                source_version,
                frame,
                captured_at: capture_started,
                capture_completed_at,
                recycler: Some(capture.image_recycler()),
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

    let close = capture_stop_notice(
        &capture_result,
        shutdown.load(Ordering::Acquire),
        first_stop,
    );
    running.store(false, Ordering::Release);
    mailbox.close();
    credits.close();
    // Input release starts before the bounded close, then interrupt any send
    // blocked behind a slow peer. A failed close never replaces the cause.
    shutdown_stream(&writer, close, |how| {
        let _ = control.shutdown(how);
    });
    let sender_result = match sender.join() {
        Ok(result) => result,
        Err(_) => {
            first_stop.record_thread_panic();
            Err(io::Error::other("remote video sender thread panicked").into())
        }
    };
    // The forwarder holds a reference to the shared writer, so it must not
    // outlive the session. It notices `running` within one poll interval.
    let clipboard_result = finish_clipboard_worker(
        clipboard_start_result,
        clipboard_thread.map(thread::JoinHandle::join),
        first_stop,
    );

    stream_result_for_stop(
        first_stop.cause(),
        capture_result,
        sender_result,
        clipboard_result,
    )
}

fn stream_result_for_stop(
    cause: StopCause,
    capture_result: RemoteResult<()>,
    sender_result: RemoteResult<()>,
    clipboard_result: RemoteResult<()>,
) -> RemoteResult<()> {
    match cause {
        StopCause::Capture => capture_result,
        StopCause::Sender => sender_result,
        StopCause::Clipboard => clipboard_result,
        StopCause::Input | StopCause::Graceful => Ok(()),
        StopCause::ThreadPanic => sender_result.and(clipboard_result),
        StopCause::None => capture_result.and(sender_result).and(clipboard_result),
    }
}

fn captured_frame_from_outcome(outcome: CaptureOutcome) -> Option<CapturedFrame> {
    match outcome {
        CaptureOutcome::Frame(frame) => Some(frame),
        CaptureOutcome::NoChange | CaptureOutcome::RetainedProofBlocked => None,
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

#[cfg(test)]
fn send_frames<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
) -> RemoteResult<()> {
    send_frames_scoped(
        mailbox,
        writer,
        running,
        credits,
        telemetry,
        jpeg_quality,
        None,
    )
}

#[cfg(test)]
fn send_frames_with_encoder<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    sender: &mut FrameSender,
) -> RemoteResult<()> {
    send_frames_with_encoder_scoped(
        mailbox,
        writer,
        running,
        credits,
        telemetry,
        jpeg_quality,
        sender,
        None,
    )
}

fn send_frames_scoped<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    sources: Option<&SourceControl>,
) -> RemoteResult<()> {
    let mut sender = FrameSender::with_codecs(
        sources.is_some_and(|source| source.scroll_copy.load(Ordering::Acquire)),
        sources.is_some_and(|source| source.static_lossless.load(Ordering::Acquire)),
    );
    send_frames_with_encoder_scoped(
        mailbox,
        writer,
        running,
        credits,
        telemetry,
        jpeg_quality,
        &mut sender,
        sources,
    )
}

fn send_frames_with_encoder_scoped<W: std::io::Write + SetWriteTimeout>(
    mailbox: Arc<LatestMailbox<PendingFrame>>,
    writer: Arc<Mutex<SessionWriter<DeadlineWriter<W>>>>,
    running: &AtomicBool,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    sender: &mut FrameSender,
    sources: Option<&SourceControl>,
) -> RemoteResult<()> {
    let mut source_epoch = None;
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
        telemetry.record_dequeued(pending.capture_completed_at.elapsed(), ack_wait);
        if !running.load(Ordering::Acquire) {
            break;
        }
        if let Some(sources) = sources {
            if !sources.sender_ready(pending.source_version, running)? {
                continue;
            }
            if source_epoch != Some(pending.source_version.epoch) {
                *sender = FrameSender::with_codecs(sender.scroll_copy, sender.static_lossless);
                source_epoch = Some(pending.source_version.epoch);
            }
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some((policy, _)) = sources.and_then(|s| s.lossless_schedule_test.as_ref()) {
            sender.lossless_schedule = *policy;
        }
        // A retained proof requires a fresh record/sequence/presentation, not
        // a fresh codec reference. FreshProof forces exact damage and emission through both
        // refinement passes, including a legitimate zero-dirty frame.
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        let legacy_keyframe =
            sources.is_some_and(|s| s.proof_delta.as_ref().is_some_and(|(legacy, _)| *legacy));
        #[cfg(not(all(test, feature = "remote-wayland", feature = "backend-wayland-udev")))]
        let legacy_keyframe = false;
        let planned_at = Instant::now();
        let mut plan = sender
            .plan_record_at(
                &pending.frame,
                planned_at,
                pending.pointer_serial.is_some(),
                legacy_keyframe,
            )
            .map_err(LocalRuntimeFailure::encoding)?;
        if !sender.static_lossless && !plan.keyframe && sender.refinement_due(planned_at) {
            let current = jpeg_quality.current_quality();
            if sender.encoder.has_quality_debt(current) {
                plan = sender
                    .refine_at(current, planned_at)
                    .map_err(LocalRuntimeFailure::encoding)?;
            }
        }
        telemetry.record_dirty_scan(planned_at.elapsed());
        if !plan.emit {
            telemetry.record_unchanged_suppressed();
            continue;
        }
        let quality = jpeg_quality.quality_before_encode_at(Instant::now());
        if sender.pending_refinement {
            // Pending ACKs can lower quality after the read-only debt check.
            // Replan at the actual epoch's quality, never upgrading debt using
            // a stale pre-congestion target or overwriting a sharper tile.
            plan = sender
                .refine_at(quality.quality, planned_at)
                .map_err(LocalRuntimeFailure::encoding)?;
        }
        let unchanged_keepalive = plan.dirty_tiles == 0;
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
        if !plan.emit {
            telemetry.record_unchanged_suppressed();
            continue;
        }
        let encode_started = Instant::now();
        if let Err(error) =
            sender
                .encoder
                .encode_into(&mut payload, sequence, &pending.frame, quality.quality)
        {
            if sender.static_lossless && sender.encoder.lossless_encode_rejected() {
                sender.discard();
                sender.lossless_retry_after = Some(Instant::now() + STATIC_REFINEMENT_INTERVAL);
                continue;
            }
            return Err(LocalRuntimeFailure::encoding(error));
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some((_, probe)) = sources.and_then(|s| s.lossless_schedule_test.as_ref()) {
            let mut stats = probe.stats.lock().unwrap();
            stats.encode_ns += encode_started.elapsed().as_nanos();
            if let Some(t) = sender.encoder.encode_timings() {
                if let Some(png) = t.png {
                    stats.encoded_png += 1;
                    stats.png_ns += png.as_nanos();
                } else {
                    stats.encoded_jpeg += 1;
                    stats.jpeg_ns += t.jpeg.as_nanos();
                }
            }
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        let schedule_encode_ns = encode_started.elapsed().as_nanos();
        telemetry.record_encoded(encode_started.elapsed(), plan);
        if let Some(stages) = sender.encoder.encode_timings() {
            telemetry.record_encode_stages(stages);
        }
        if sources.is_some() {
            if payload.len() > super::source_wire::MAX_SOURCE_INNER_PAYLOAD_LEN {
                return Err(invalid_data("source frame exceeds record budget").into());
            }
            // Reuse the encoded payload allocation instead of allocating and
            // copying a second frame-sized envelope on every frame.
            payload.splice(..0, pending.source_version.epoch.to_be_bytes());
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if sender
            .encoder
            .encode_timings()
            .is_some_and(|t| t.png.is_some())
            && let Some(probe) = sources.and_then(|s| s.pending_lossless_probe.as_ref())
        {
            // No publication, source-state or writer lock is held here.
            probe.pause(
                sequence,
                pending.source_version.epoch,
                pending.frame.geometry_epoch,
            )?;
        }
        if sender.lossless_schedule == LosslessSchedule::New250
            && sender
                .encoder
                .encode_timings()
                .is_some_and(|t| t.png.is_some())
        {
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            let compare_started = Instant::now();
            let superseded = mailbox.inspect_latest(|latest| {
                latest.is_some_and(|newer| newer_frame_supersedes_repair(&pending, newer))
            })?;
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            if let Some((_, probe)) = sources.and_then(|s| s.lossless_schedule_test.as_ref()) {
                let mut stats = probe.stats.lock().unwrap();
                stats.comparisons += 1;
                stats.comparison_ns += compare_started.elapsed().as_nanos();
                stats.stale_png_dropped += u64::from(superseded);
            }
            if superseded {
                sender.discard();
                continue;
            }
        }
        let wire_payload = payload.as_slice();
        let write_started = Instant::now();
        let write_result = {
            let mut writer = lock_writer(&writer);
            // The writer is the source publication barrier. A sender already
            // encoding old pixels must recheck before reserving credit or
            // writing, and can never appear after SourceChanged.
            let mut source_state = sources.map(|sources| sources.state.lock().unwrap());
            if source_state.as_ref().is_some_and(|state| {
                !state.policy.accepts_capture(pending.source_version)
                    || (sources
                        .is_some_and(|sources| sources.follow_geometry.load(Ordering::Acquire))
                        && state.geometry_epoch != pending.frame.geometry_epoch)
            }) {
                sender.discard();
                continue;
            }
            if let (Some(sources), Some(serial)) = (sources, pending.pointer_serial) {
                let mut retained = sources.pointer_frame.lock().unwrap();
                let Some(frame) = retained.as_mut().filter(|frame| frame.serial == serial) else {
                    // Capture-age expiry can retire a frame while JPEG encoding
                    // runs. Drop it before reserving credit, never reuse its proof.
                    sender.discard();
                    continue;
                };
                frame.scope = Some(DrawnScope {
                    source_epoch: pending.source_version.epoch,
                    geometry_epoch: pending.frame.geometry_epoch,
                    frame_sequence: sequence,
                });
            }
            let sent_at = Instant::now();
            let outstanding = credits.mark_sent_scoped(
                sequence,
                pending.captured_at,
                sent_at,
                wire_payload.len(),
                quality.quality,
                quality.epoch,
                pending.source_version.epoch,
                pending.frame.geometry_epoch,
            )?;
            if let Some(state) = source_state.as_mut() {
                if state.policy.needs_keyframe() {
                    if !plan.keyframe {
                        return Err(invalid_data("new source requires a full keyframe").into());
                    }
                    state
                        .policy
                        .publish_keyframe(pending.source_version, sequence)?;
                }
            }
            drop(source_state);
            telemetry.record_outstanding(outstanding);
            let socket_started = Instant::now();
            let result = write_frame_record(&mut writer, wire_payload, FRAME_WRITE_DEADLINE);
            if result.is_ok() {
                telemetry.record_sent(
                    wire_payload.len(),
                    socket_started.duration_since(write_started),
                    socket_started.elapsed(),
                );
            }
            result
        };
        if let Err(error) = write_result {
            // The viewer never applied these tiles, so the encoder's model of
            // its canvas must not advance past them.
            sender.discard();
            return Err(error.into());
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        let flushed_at = Instant::now();
        if unchanged_keepalive {
            telemetry.record_unchanged_keepalive();
        }
        // Commit by move only after the complete authenticated record and its
        // flush succeeded. A partial/failed write therefore cannot suppress a
        // future frame in a replacement session or test harness.
        sender.commit_at(&pending.frame, Instant::now());
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some((_, probe)) = sources.and_then(|s| s.lossless_schedule_test.as_ref()) {
            probe.stats.lock().unwrap().sent_bytes += wire_payload.len() as u64;
            let stages = sender.encoder.encode_timings();
            let mut records = probe.records.lock().unwrap();
            assert!(
                records.len() < 4096,
                "test lossless schedule ledger exhausted"
            );
            records.push(LosslessScheduleRecord {
                sequence,
                source_epoch: pending.source_version.epoch,
                geometry_epoch: pending.frame.geometry_epoch,
                png: stages.is_some_and(|t| t.png.is_some()),
                bytes: wire_payload.len(),
                encode_ns: schedule_encode_ns,
                jpeg_ns: stages.map_or(0, |t| t.jpeg.as_nanos()),
                png_ns: stages.and_then(|t| t.png).map_or(0, |d| d.as_nanos()),
            });
        }

        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let Some(probe) = sources.and_then(|s| s.pending_lossless_probe.as_ref()) {
            let mut written = probe.written.lock().unwrap();
            assert!(
                written.len() < 4096,
                "test pending PNG write ledger exhausted"
            );
            written.push((
                sequence,
                pending.source_version.epoch,
                pending.frame.geometry_epoch,
                sender
                    .encoder
                    .encode_timings()
                    .is_some_and(|t| t.png.is_some()),
                plan.keyframe,
            ));
        }
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        if let (Some(serial), Some((_, probe))) = (
            pending.pointer_serial,
            sources.and_then(|s| s.proof_delta.as_ref()),
        ) {
            let mut frames = probe.frames.lock().unwrap();
            assert!(frames.len() < 512, "proof evidence test bound");
            frames.push(ProofFrameEvidence {
                captured_at: pending.captured_at,
                capture_completed_at: pending.capture_completed_at,
                planned_at,
                flushed_at,
                native_serial: serial,
                source_epoch: pending.source_version.epoch,
                geometry_epoch: pending.frame.geometry_epoch,
                sequence,
                keyframe: plan.keyframe,
                dirty_tiles: plan.dirty_tiles,
                total_tiles: plan.total_tiles,
                encoded_bytes: wire_payload.len(),
                jpeg_ns: sender
                    .encoder
                    .encode_timings()
                    .map_or(0, |t| t.jpeg.as_nanos().min(u128::from(u64::MAX)) as u64),
            });
        }
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("remote frame sequence exhausted"))?;
        payload_retention.observe(&mut payload);
    }
    Ok(())
}

/// Report only a known local encoding failure, after all sender/encoder,
/// source-state and writer guards have unwound. Transport/ACK/coordination
/// errors still stop the session without claiming an encoding failure.
fn run_video_worker(
    task: impl FnOnce() -> RemoteResult<()>,
    running: &Arc<AtomicBool>,
    first_stop: &FirstStop,
    shutdown: impl FnOnce(Option<RuntimeClose>),
) -> RemoteResult<()> {
    let _stop_on_exit = StopSessionOnDrop(Arc::clone(running));
    let result = task();
    if let Err(error) = &result {
        let close = first_stop
            .record(StopCause::Sender)
            .then(|| runtime_close(error))
            .flatten();
        running.store(false, Ordering::Release);
        shutdown(close);
    }
    result
}

/// Stop on clipboard failure without reclassifying secondary teardown errors.
/// Only the first cause may attempt a terminal record, and only a confirmed
/// local backend/start failure gets a ClipboardUnavailable code.
fn stop_clipboard_session(
    running: &AtomicBool,
    first_stop: &FirstStop,
    close: Option<RuntimeClose>,
    shutdown: impl FnOnce(Option<RuntimeClose>),
) {
    if running.load(Ordering::Acquire) {
        // Publish the cause before capture/input can observe stopped state.
        let first = first_stop.record(StopCause::Clipboard);
        if running.swap(false, Ordering::AcqRel) {
            shutdown(if first { close } else { None });
        }
    }
}

fn run_clipboard_worker(
    task: impl FnOnce() -> RemoteResult<()>,
    running: &Arc<AtomicBool>,
    first_stop: &FirstStop,
    shutdown: impl FnOnce(Option<RuntimeClose>),
) -> RemoteResult<()> {
    let _stop_on_exit = StopSessionOnDrop(Arc::clone(running));
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)) {
        Ok(result) => {
            if let Err(error) = &result {
                stop_clipboard_session(running, first_stop, runtime_close(error), shutdown);
            }
            result
        }
        Err(panic) => {
            // A panic may occur in the writer. Its ownership and record state
            // are unknown, so do not synthesize a clipboard backend failure.
            stop_clipboard_session(running, first_stop, None, shutdown);
            std::panic::resume_unwind(panic)
        }
    }
}

fn admit_clipboard_worker<T>(
    worker: Option<RemoteResult<T>>,
    running: &AtomicBool,
    first_stop: &FirstStop,
    shutdown: impl FnOnce(Option<RuntimeClose>),
) -> (Option<T>, RemoteResult<()>) {
    match worker {
        Some(Ok(worker)) => (Some(worker), Ok(())),
        None => (None, Ok(())),
        Some(Err(error)) => {
            stop_clipboard_session(running, first_stop, runtime_close(&error), shutdown);
            (None, Err(error))
        }
    }
}

fn finish_clipboard_worker(
    started: RemoteResult<()>,
    joined: Option<thread::Result<RemoteResult<()>>>,
    first_stop: &FirstStop,
) -> RemoteResult<()> {
    match joined {
        Some(Ok(result)) => started.and(result),
        Some(Err(_)) => {
            // Preserve an earlier input/capture/sender/graceful outcome.
            first_stop.record(StopCause::Clipboard);
            Err(io::Error::other("remote clipboard thread panicked").into())
        }
        None => started,
    }
}

/// Forward clipboard records independently of video credits and capture, so
/// an idle desktop does not park copy/paste behind its next frame.
fn forward_clipboard<W: std::io::Write + SetWriteTimeout>(
    outbox: &ClipboardOutbox,
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    running: &AtomicBool,
) -> RemoteResult<()> {
    forward_clipboard_with_policy(
        |timeout| outbox.next_timeout(timeout),
        writer,
        running,
        Some(&outbox.control()),
    )
}

/// Write each clipboard record the worker queues, one record per lock, so a
/// frame can always slip in between two chunks of a large copy.
#[cfg(test)]
fn forward_clipboard_from<W: std::io::Write + SetWriteTimeout>(
    receive: impl FnMut(Duration) -> Result<Option<Outgoing>, std::sync::mpsc::RecvError>,
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    running: &AtomicBool,
) -> RemoteResult<()> {
    forward_clipboard_with_policy(receive, writer, running, None)
}

fn forward_clipboard_with_policy<W: std::io::Write + SetWriteTimeout>(
    mut receive: impl FnMut(Duration) -> Result<Option<Outgoing>, std::sync::mpsc::RecvError>,
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
    running: &AtomicBool,
    control: Option<&super::clipboard_sync::ClipboardControl>,
) -> RemoteResult<()> {
    let mut gate = OutboundGate::default();
    while running.load(Ordering::Acquire) {
        let record = match receive(CLIPBOARD_POLL_INTERVAL) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(_) if !running.load(Ordering::Acquire) => return Ok(()),
            Err(_) => {
                return Err(LocalRuntimeFailure::clipboard(
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "local clipboard capture worker stopped",
                    )
                    .into(),
                ));
            }
        };
        let result = {
            let mut writer = lock_writer(writer);
            // Check after obtaining actual writer ownership. Bytes already in a
            // native write precede this stream's restriction acknowledgement.
            let payload = if let Some(control) = control {
                gate.filter(record.kind, &record.payload, control.policy().content())?
            } else {
                Some(record.payload.clone())
            };
            if record.kind == MessageKind::ClipboardRestricted {
                let restriction = ClipboardRestriction::decode(&record.payload)?;
                control
                    .ok_or_else(|| invalid_data("clipboard barrier without control"))?
                    .with_written_barrier(restriction, || {
                        write_session_record(
                            &mut writer,
                            record.kind,
                            &record.payload,
                            FRAME_WRITE_DEADLINE,
                        )
                        .map_err(Into::into)
                    })?;
                eprintln!(
                    "jwm-remote: clipboard restriction applied (revision {}, content mask {})",
                    restriction.revision, restriction.content
                );
            } else if let Some(payload) = payload {
                write_session_record(&mut writer, record.kind, &payload, FRAME_WRITE_DEADLINE)?;
            }
            Ok::<_, super::RemoteError>(())
        };
        if let Err(error) = result {
            return Err(error.into());
        }
    }
    Ok(())
}

fn lock_writer<W>(
    writer: &Mutex<SessionWriter<DeadlineWriter<W>>>,
) -> std::sync::MutexGuard<'_, SessionWriter<DeadlineWriter<W>>> {
    match writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Default)]
struct CursorSender {
    sequence: u64,
    shape_id: Option<u64>,
}

impl CursorSender {
    #[cfg(test)]
    fn send<W: std::io::Write + SetWriteTimeout>(
        &mut self,
        writer: &mut SessionWriter<DeadlineWriter<W>>,
        sample: super::x11_capture::CursorSample,
    ) -> RemoteResult<()> {
        self.send_scoped(writer, sample, None)
    }

    fn send_scoped<W: std::io::Write + SetWriteTimeout>(
        &mut self,
        writer: &mut SessionWriter<DeadlineWriter<W>>,
        sample: super::x11_capture::CursorSample,
        source_epoch: Option<u64>,
    ) -> RemoteResult<()> {
        let shape = sample
            .shape
            .ok_or_else(|| invalid_data("cursor sample has no shape"))?;
        if shape.id != sample.shape_id {
            return Err(invalid_data("cursor sample shape id mismatch").into());
        }
        let next = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("cursor sequence exhausted"))?;
        let position = super::cursor::CursorPosition {
            sequence: self.sequence,
            geometry_epoch: sample.geometry_epoch,
            shape_id: sample.shape_id,
            source_width: sample.source_width,
            source_height: sample.source_height,
            x: sample.x,
            y: sample.y,
            visible: sample.visible,
        }
        .encode()?;
        let position = match source_epoch {
            Some(epoch) => encode_source_payload(epoch, &position)?,
            None => position.to_vec(),
        };
        if self.shape_id != Some(shape.id) {
            let encoded = shape.encode()?;
            let encoded = match source_epoch {
                Some(epoch) => encode_source_payload(epoch, &encoded)?,
                None => encoded,
            };
            write_session_record(
                writer,
                MessageKind::CursorShape,
                &encoded,
                FRAME_WRITE_DEADLINE,
            )?;
        }
        write_session_record(
            writer,
            MessageKind::CursorPosition,
            &position,
            FRAME_WRITE_DEADLINE,
        )?;
        self.shape_id = Some(shape.id);
        self.sequence = next;
        Ok(())
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
    print_host_telemetry(snapshot, outstanding, telemetry.stats_json);
}

fn report_host_final(telemetry: &HostTelemetry, credits: &FrameCredits, now: Instant) {
    let Some(snapshot) = telemetry.take_final_at(now) else {
        return;
    };
    let outstanding = credits.outstanding();
    print_host_telemetry(snapshot, outstanding, telemetry.stats_json);
}

fn host_telemetry_json(snapshot: HostTelemetrySnapshot, outstanding: u64) -> serde_json::Value {
    let w = snapshot.window;
    serde_json::json!({
        "type": "jwm_remote_host_stats", "schema_version": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "commit": option_env!("JWM_BUILD_COMMIT"),
        "window_seconds": snapshot.elapsed.as_secs_f64(), "mode": w.capture_mode.describe(),
        "source_dimensions": w.source_dimensions, "encoded_dimensions": w.encoded_dimensions,
        "scheduled": w.scheduled, "captured": w.captured, "sent": w.sent, "bytes": w.bytes,
        "replaced": w.replaced, "unchanged_suppressed": w.unchanged_suppressed,
        "dirty_tiles": w.dirty_tiles, "total_tiles": w.total_tiles,
        "jpeg_atlases": w.jpeg_encoded, "png_atlases": w.png_encoded,
        "legacy_stage_aliases": {"atlas_pack_and_jpeg_ms": "atlas_pack_and_codec_ms; historical name includes all codecs and empty-frame encode work"},
        "drawn_acks": w.drawn_acks, "viewer_superseded": w.viewer_superseded,
        "outstanding": outstanding, "max_outstanding": w.max_outstanding,
        "quantile_policy": "nearest-rank, most recent 128 observations per stage per window",
        "stages": {
            "capture_ms": {"mean": average_millis(w.capture_elapsed, w.captured), "quantiles": w.capture_samples.json()},
            "mailbox_age_ms": {"mean": average_millis(w.queue_elapsed, w.dequeued), "quantiles": w.queue_samples.json()},
            "credit_wait_ms": {"mean": average_millis(w.credit_wait_elapsed, w.dequeued), "quantiles": w.credit_samples.json()},
            "dirty_scan_ms": {"mean": average_millis(w.dirty_scan_elapsed, w.dirty_scans), "quantiles": w.dirty_scan_samples.json()},
            "atlas_pack_ms": {"mean": average_millis(w.atlas_pack_elapsed, w.atlas_encoded), "quantiles": w.atlas_pack_samples.json()},
            "jpeg_ms": {"mean": average_millis(w.jpeg_elapsed, w.jpeg_encoded), "quantiles": w.jpeg_samples.json()},
            "png_ms": {"mean": average_millis(w.png_elapsed, w.png_encoded), "quantiles": w.png_samples.json()},
            "codec_ms": {"mean": average_millis(w.codec_elapsed, w.atlas_encoded), "quantiles": w.codec_samples.json()},
            "atlas_pack_and_codec_ms": {"mean": average_millis(w.encode_elapsed, w.encoded), "quantiles": w.encode_samples.json()},
            "atlas_pack_and_jpeg_ms": {"mean": average_millis(w.encode_elapsed, w.encoded), "quantiles": w.encode_samples.json()},
            "writer_wait_ms": {"mean": average_millis(w.writer_wait_elapsed, w.sent), "quantiles": w.writer_wait_samples.json()},
            "socket_write_ms": {"mean": average_millis(w.socket_write_elapsed, w.sent), "quantiles": w.socket_write_samples.json()},
            "capture_start_to_ack_ms": {"mean": average_millis(w.capture_to_ack, w.drawn_acks), "quantiles": w.capture_to_ack_samples.json()},
            "send_to_ack_ms": {"mean": average_millis(w.send_to_ack, w.drawn_acks), "quantiles": w.send_to_ack_samples.json()}
        }
    })
}

fn print_host_telemetry(snapshot: HostTelemetrySnapshot, outstanding: u64, stats_json: bool) {
    if stats_json {
        eprintln!("{}", host_telemetry_json(snapshot, outstanding));
        return;
    }
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
        "jwm-remote: host {:.1}s mode {} scheduled {} captured {} skipped {} damage-skipped {} published {} replaced {} dequeued {} unchanged-suppressed {} unchanged-keepalive {} encoded {} keyframes {} tiles {}/{} ({tile_percent:.1}% dirty) sent {} ({sent_fps:.1} fps, {megabits_per_second:.2} Mbit/s) bytes {} drawn-acks {} retired {} viewer-superseded {} drawn-bytes {}; avg capture/queue/credit-wait/encode/write/capture-start-to-ACK/send-to-ACK {:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1}/{:.1} ms; outstanding current/max {outstanding}/{}, max-queue {:.1} ms",
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

fn receive_input_scoped(
    mut reader: SessionReader<TcpStream>,
    mut injector: Option<InputInjector>,
    input_area: InputArea,
    keyboard_enabled: bool,
    verified_keymap: Option<KeymapTable>,
    initial_activity_timeout: Duration,
    session_idle_timeout: Duration,
    running: &AtomicBool,
    first_stop: &FirstStop,
    credits: &FrameCredits,
    telemetry: &HostTelemetry,
    jpeg_quality: &JpegQualityController,
    viewport: &ViewportRequest,
    clipboard: Option<&ClipboardInbox>,
    sources: Option<&SourceControl>,
) -> InputExit {
    let mut payload = Vec::new();
    let mut input_geometry =
        sources.map_or(0, |sources| sources.state.lock().unwrap().geometry_epoch);
    let session_result = (|| -> RemoteResult<()> {
        reader
            .get_ref()
            .set_read_timeout(Some(initial_activity_timeout))?;
        let mut awaiting_first_activity = true;
        while running.load(Ordering::Acquire) {
            // Complete authenticated activity renews the held-input lease.
            // Readability alone is insufficient: a stalled partial record
            // must not extend a held key to the eight-second socket timeout.
            let held_deadline = injector
                .as_ref()
                .is_some_and(InputInjector::has_pressed)
                .then(|| Instant::now() + HELD_INPUT_SILENCE_TIMEOUT);
            if held_deadline.is_some()
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
            let kind = read_input_record(&mut reader, &mut payload, held_deadline)?;
            if awaiting_first_activity {
                reader
                    .get_ref()
                    .set_read_timeout(Some(session_idle_timeout))?;
                awaiting_first_activity = false;
            }
            if let Some(sources) = sources {
                let geometry = sources.state.lock().unwrap().geometry_epoch;
                refresh_input_geometry(&mut input_geometry, geometry, || {
                    injector
                        .as_mut()
                        .map_or(Ok(()), InputInjector::release_all)
                        .map_err(LocalRuntimeFailure::input)
                })?;
            }
            match kind {
                MessageKind::InputBatch => {
                    let (source_epoch, geometry_epoch, events) = decode_source_input(&payload)?;
                    let Some(injector) = injector.as_mut() else {
                        return Err(
                            invalid_data("input was not negotiated for this session").into()
                        );
                    };
                    let mut apply = |admission| -> RemoteResult<()> {
                        match admission {
                            SourceInputAdmission::Stale => Ok(()),
                            SourceInputAdmission::ReleaseOnly => {
                                injector.release_all().map_err(LocalRuntimeFailure::input)?;
                                Ok(())
                            }
                            SourceInputAdmission::Inject(area) => {
                                let contains_keyboard = events.iter().any(|event| {
                                    matches!(event, super::x11_input::InputEvent::Key { .. })
                                });
                                if contains_keyboard && !keyboard_enabled {
                                    return Err(
                                        invalid_data("keyboard input was not negotiated").into()
                                    );
                                }
                                if keyboard_enabled {
                                    verify_injector_keymap(injector, verified_keymap.as_ref())?;
                                } else {
                                    let _ = injector
                                        .take_keymap_change()
                                        .map_err(LocalRuntimeFailure::input)?;
                                }
                                injector
                                    .inject_batch(&events, area)
                                    .map_err(LocalRuntimeFailure::input)?;
                                Ok(())
                            }
                        }
                    };
                    if let Some(sources) = sources {
                        sources.apply_input(
                            &mut input_geometry,
                            source_epoch,
                            geometry_epoch,
                            &events,
                            apply,
                        )?;
                    } else {
                        apply(SourceInputAdmission::Inject(input_area))?;
                    }
                }
                MessageKind::Pointer
                | MessageKind::Key
                | MessageKind::Button
                | MessageKind::ReleaseAll => {
                    return Err(invalid_data(
                        "legacy single-event input is invalid since application protocol v4",
                    )
                    .into());
                }
                MessageKind::Close => break,
                MessageKind::Heartbeat if payload.is_empty() => {
                    if keyboard_enabled && let Some(injector) = injector.as_mut() {
                        verify_injector_keymap(injector, verified_keymap.as_ref())?;
                    }
                }
                MessageKind::Heartbeat => {
                    return Err(invalid_data("client heartbeat payload must be empty").into());
                }
                MessageKind::ClipboardRestrict => {
                    let request = ClipboardRestriction::decode(&payload)?;
                    clipboard
                        .ok_or_else(|| invalid_data("clipboard sharing was not negotiated"))?
                        .restrict(request)?;
                }
                MessageKind::Clipboard | MessageKind::ClipboardTransfer => {
                    match clipboard {
                        Some(clipboard) => clipboard.deliver(kind, &payload)?,
                        // Negotiation already told the peer clipboard sharing
                        // is off; a record arriving anyway is a protocol
                        // violation, not something to silently absorb.
                        None => {
                            return Err(invalid_data("clipboard sharing was not negotiated").into());
                        }
                    }
                }
                MessageKind::WaylandControlRequest | MessageKind::WaylandControlAction => {
                    let sources = sources.ok_or_else(|| invalid_data("control unavailable"))?;
                    if !sources.control_enabled.load(Ordering::Acquire)
                        || sources.control_revoke.load(Ordering::Acquire)
                    {
                        return Err(invalid_data("control not negotiated or revoked").into());
                    }
                    let command = if kind == MessageKind::WaylandControlRequest {
                        ControlCommand::Request(
                            super::wayland_control_wire::ControlRequest::decode(&payload)?,
                        )
                    } else {
                        ControlCommand::Action(super::wayland_control_wire::ControlAction::decode(
                            &payload,
                        )?)
                    };
                    let mut queue = sources.control_queue.lock().unwrap();
                    if queue.len() >= 32 {
                        sources.control_revoke.store(true, Ordering::Release);
                        return Err(invalid_data("control queue exceeded").into());
                    }
                    queue.push_back(command);
                    drop(queue);
                    sources.notify_input()?;
                }
                MessageKind::WaylandTextCommit => {
                    let sources = sources.ok_or_else(|| invalid_data("text source unavailable"))?;
                    if !sources.text_enabled.load(Ordering::Acquire)
                        || sources.text_attempted.swap(true, Ordering::AcqRel)
                    {
                        return Err(invalid_data(
                            "one-shot text not authorized or already attempted",
                        )
                        .into());
                    }
                    let request = super::wayland_text_wire::TextCommit::decode(&payload)?;
                    *sources.text_request.lock().unwrap() = Some(request);
                    sources.notify_input()?;
                }
                MessageKind::WaylandControlRevoke => {
                    let _ = super::wayland_control_wire::ControlRevoke::decode(&payload)?;
                    let sources = sources.ok_or_else(|| invalid_data("control unavailable"))?;
                    sources.control_revoke.store(true, Ordering::Release);
                    sources.control_queue.lock().unwrap().clear();
                    sources.notify_input()?;
                }
                MessageKind::WaylandPointerRequest | MessageKind::WaylandPointerAction => {
                    let sources =
                        sources.ok_or_else(|| invalid_data("native pointer unavailable"))?;
                    if !sources.pointer_enabled.load(Ordering::Acquire)
                        || sources.pointer_revoke.load(Ordering::Acquire)
                    {
                        return Err(invalid_data("native pointer not negotiated or revoked").into());
                    }
                    let command = if kind == MessageKind::WaylandPointerRequest {
                        PointerCommand::Request(PointerRequest::decode(&payload)?)
                    } else {
                        let action = PointerAction::decode(&payload)?;
                        if action.kind.is_scroll()
                            && !(if action.kind.is_horizontal() {
                                sources.horizontal_scroll_enabled.load(Ordering::Acquire)
                            } else {
                                sources.scroll_enabled.load(Ordering::Acquire)
                            })
                        {
                            sources.pointer_revoke.store(true, Ordering::Release);
                            return Err(invalid_data(
                                "Wayland scroll was not separately negotiated",
                            )
                            .into());
                        }
                        PointerCommand::Action(action)
                    };
                    let mut queue = sources.pointer_queue.lock().unwrap();
                    if !queue.is_empty() {
                        sources.pointer_revoke.store(true, Ordering::Release);
                        return Err(invalid_data("pointer inflight budget exceeded").into());
                    }
                    queue.push_back(command);
                    drop(queue);
                    sources.notify_input()?;
                }
                MessageKind::WaylandPointerRevoke => {
                    let _ = PointerRevoke::decode(&payload)?;
                    let sources =
                        sources.ok_or_else(|| invalid_data("native pointer unavailable"))?;
                    sources.pointer_revoke.store(true, Ordering::Release);
                    sources.pointer_queue.lock().unwrap().clear();
                    sources.notify_input()?;
                }
                MessageKind::WaylandKeyboardRequest | MessageKind::WaylandKeyboardKey => {
                    let sources =
                        sources.ok_or_else(|| invalid_data("Wayland keyboard unavailable"))?;
                    if !sources.keyboard_enabled.load(Ordering::Acquire)
                        || sources.keyboard_revoke.load(Ordering::Acquire)
                    {
                        return Err(invalid_data(
                            "Wayland keyboard was not negotiated or was revoked",
                        )
                        .into());
                    }
                    let command = if kind == MessageKind::WaylandKeyboardRequest {
                        KeyboardCommand::Request(LeaseRequest::decode(&payload)?)
                    } else {
                        KeyboardCommand::Key(KeyboardKey::decode(&payload)?)
                    };
                    let mut queue = sources.keyboard_queue.lock().unwrap();
                    if queue.len() >= 32 {
                        sources.keyboard_revoke.store(true, Ordering::Release);
                        queue.clear();
                        return Err(invalid_data("Wayland keyboard command budget exceeded").into());
                    }
                    queue.push_back(command);
                    drop(queue);
                    sources.notify_input()?;
                }
                MessageKind::WaylandKeyboardRevoke => {
                    let _ = KeyboardRevoke::decode(&payload)?;
                    let sources =
                        sources.ok_or_else(|| invalid_data("Wayland keyboard unavailable"))?;
                    sources.keyboard_revoke.store(true, Ordering::Release);
                    sources.keyboard_queue.lock().unwrap().clear();
                    sources.notify_input()?;
                }
                MessageKind::WindowResizeRequest => {
                    let sources =
                        sources.ok_or_else(|| invalid_data("window resize unavailable"))?;
                    if !sources.resize_enabled.load(Ordering::Acquire) {
                        return Err(invalid_data("window resize was not negotiated").into());
                    }
                    let request = ResizeRequest::decode(&payload)?;
                    let mut slot = sources.resize_request.lock().unwrap();
                    if slot.is_some() {
                        return Err(invalid_data("window resize intake overflow").into());
                    }
                    *slot = Some(request);
                }
                MessageKind::Viewport => {
                    let (width, height) = decode_viewport(&payload)?;
                    #[cfg(test)]
                    let previous = viewport.requested.load(Ordering::Relaxed);
                    let changed = viewport.request(width, height);
                    #[cfg(test)]
                    eprintln!(
                        "host viewport previous={}x{}, requested={width}x{height}, changed={changed}",
                        previous >> 16,
                        previous & 0xffff
                    );
                    if let Some(sources) = sources {
                        if changed && sources.pointer_enabled.load(Ordering::Acquire) {
                            // Never clear this latch: change then restore cannot
                            // rehabilitate the prior native pointer authorization.
                            sources.pointer_revoke.store(true, Ordering::Release);
                        }
                    }
                }
                MessageKind::SourceRequest => {
                    let request = SourceRequest::decode(&payload)?;
                    let sources =
                        sources.ok_or_else(|| invalid_data("source switching unavailable"))?;
                    let (version, area) = {
                        let mut state = sources.state.lock().unwrap();
                        let version = state
                            .policy
                            .begin_switch(request.expected_epoch, request.target)?;
                        (version, state.policy.selected_area().clone())
                    };
                    // This thread owns every injection, so no input edge can
                    // race this release or enter the new mapping before ACK.
                    if let Some(injector) = injector.as_mut() {
                        injector.release_all().map_err(LocalRuntimeFailure::input)?;
                    }
                    let mut state = sources.state.lock().unwrap();
                    state.presented_geometry = None;
                    state.pending = Some((version, area));
                    sources.changed.notify_all();
                }
                MessageKind::FrameAck => {
                    let (epoch, sequence) = decode_source_frame_ack(&payload)?;
                    if let Some(sources) = sources {
                        let state = sources.state.lock().unwrap();
                        let current = state.policy.current();
                        if epoch < current.epoch {
                            continue;
                        }
                        if epoch != current.epoch {
                            return Err(invalid_data("future source ACK").into());
                        }
                    }
                    let acknowledged_at = Instant::now();
                    if let Some(ack) = credits.acknowledge_scoped_at(
                        sequence,
                        Some(epoch),
                        acknowledged_at,
                        |ack| jpeg_quality.observe_ack_at(ack, acknowledged_at),
                    )? {
                        if let Some(sources) = sources {
                            let mut state = sources.state.lock().unwrap();
                            let version = state.policy.current();
                            if ack.source_epoch != version.epoch {
                                return Err(invalid_data("stale source ACK metadata").into());
                            }
                            if state.policy.awaiting_presentation() {
                                state.policy.acknowledge_presentation(version, sequence)?;
                            }
                            state.presented_geometry = Some(ack.geometry_epoch);
                            state.presented_frame = Some(DrawnScope {
                                source_epoch: ack.source_epoch,
                                geometry_epoch: ack.geometry_epoch,
                                frame_sequence: sequence,
                            });
                            sources.changed.notify_all();
                            let scope = state.presented_frame.unwrap();
                            drop(state);
                            #[cfg(all(
                                test,
                                feature = "remote-wayland",
                                feature = "backend-wayland-udev"
                            ))]
                            {
                                if let Some((_, probe)) = &sources.proof_delta {
                                    // Copy under the proof lock, then release it before
                                    // recording: observation must add no nested lock edges.
                                    let serial = sources
                                        .pointer_frame
                                        .lock()
                                        .unwrap()
                                        .as_ref()
                                        .filter(|frame| frame.scope == Some(scope))
                                        .map(|frame| frame.serial);
                                    if let Some(serial) = serial {
                                        probe.record_stage(
                                            serial,
                                            scope,
                                            acknowledged_at,
                                            ProofStageKind::AckReceived,
                                        );
                                    }
                                }
                            }
                            sources.notify_proof_ack(scope)?;
                        }
                        telemetry.record_ack(ack);
                    }
                }
                MessageKind::Hello
                | MessageKind::HelloAck
                | MessageKind::Frame
                | MessageKind::CursorShape
                | MessageKind::CursorPosition
                | MessageKind::SourceCatalog
                | MessageKind::SourceChanged
                | MessageKind::SessionClose
                | MessageKind::ClipboardRestricted
                | MessageKind::WindowResizeResult
                | MessageKind::WaylandKeyboardStatus
                | MessageKind::WaylandPointerStatus
                | MessageKind::WaylandControlStatus
                | MessageKind::WaylandTextResult => {
                    return Err(invalid_data(format!("unexpected client message: {kind:?}")).into());
                }
            }
        }
        Ok(())
    })();

    let exit = finalize_input(
        session_result,
        injector,
        running,
        first_stop,
        credits,
        |injector| {
            injector
                .as_mut()
                .map_or(Ok(()), InputInjector::release_all)
                .map_err(Into::into)
        },
    );
    // Finalize publishes stopped state and releases input before the wake.
    // Errors/overflow use this same path. A notification failure must never
    // skip cleanup; its health latch is checked by the bounded capture loop.
    if let Some(sources) = sources {
        let _ = sources.notify_input();
    }
    exit
}

/// This value can leave the input receiver only after its injector is gone.
/// It conveys no claim about successful release or any other worker's cleanup.
struct InputExit {
    result: RemoteResult<()>,
    notice: Option<RuntimeClose>,
}

impl InputExit {
    fn report(self, notify: impl FnOnce(RuntimeClose)) -> RemoteResult<()> {
        if let Some(close) = self.notice {
            notify(close);
        }
        self.result
    }
}

/// Stop producers before release work, and finish both the explicit release
/// and injector destruction before returning a report to the outer wrapper.
/// InputInjector::Drop retries release_all, so moving notification into this
/// helper before drop would put a socket write ahead of held-input cleanup.
fn finalize_input<I>(
    session_result: RemoteResult<()>,
    mut injector: I,
    running: &AtomicBool,
    first_stop: &FirstStop,
    credits: &FrameCredits,
    release: impl FnOnce(&mut I) -> RemoteResult<()>,
) -> InputExit {
    let session_failed = session_result.is_err();
    let first = first_stop.record(if session_failed {
        StopCause::Input
    } else {
        StopCause::Graceful
    });
    // Capture ownership and the original directly tagged error, not a later
    // atomic cause or a cleanup wrapper that might hide or change its meaning.
    let notice = first
        .then(|| session_result.as_ref().err().and_then(runtime_close))
        .flatten()
        .filter(|close| {
            matches!(
                close,
                RuntimeClose::InputUnavailable
                    | RuntimeClose::KeymapUnavailable
                    | RuntimeClose::KeymapChanged
            )
        });
    running.store(false, Ordering::Release);
    credits.close();

    let release_result = release(&mut injector);
    if !session_failed && release_result.is_err() {
        first_stop.promote_graceful_to_input_error();
    }
    let result = combine_host_cleanup(session_result, release_result);
    drop(injector);
    InputExit { result, notice }
}

/// A partial record cannot be resynchronized, so expiry closes the session;
/// receive_input's existing error cleanup then releases every held edge.
/// The same budget includes the readability wait, header, ciphertext and tag.
fn read_input_record(
    reader: &mut SessionReader<TcpStream>,
    payload: &mut Vec<u8>,
    held_deadline: Option<Instant>,
) -> RemoteResult<MessageKind> {
    let mut guard = held_deadline
        .map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            TcpStreamDeadline::arm(reader.get_ref(), remaining)
        })
        .transpose()?;
    let kind = reader.read_message_into(payload)?;
    if let Some(guard) = guard.as_mut() {
        guard.cancel();
    }
    Ok(kind)
}

fn verify_injector_keymap(
    injector: &mut InputInjector,
    viewer_keymap: Option<&KeymapTable>,
) -> RemoteResult<()> {
    let changed = injector
        .take_keymap_change()
        .map_err(LocalRuntimeFailure::input)?;
    verify_current_keymap(changed, viewer_keymap, || injector.keymap())
}

fn verify_current_keymap(
    changed: bool,
    viewer_keymap: Option<&KeymapTable>,
    current_keymap: impl FnOnce() -> RemoteResult<KeymapTable>,
) -> RemoteResult<()> {
    if !changed {
        return Ok(());
    }
    let viewer_keymap =
        viewer_keymap.ok_or_else(|| invalid_data("keyboard input has no verified keymap"))?;
    // Re-run the negotiation comparison: a spare keycode bound since then by
    // a typing tool on this desktop keeps the session, a remapped key ends it.
    match viewer_keymap.compare(&current_keymap().map_err(LocalRuntimeFailure::keymap)?) {
        KeymapMatch::Compatible => Ok(()),
        mismatch => Err(Box::new(LocalRuntimeFailure {
            close: RuntimeClose::KeymapChanged,
            source: invalid_data(format!(
                "host X11 keymap changed ({mismatch}); reconnect to re-verify keyboard input"
            ))
            .into(),
        })),
    }
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
    #[test]
    fn static_lossless_requires_two_seconds_exact_stability_and_committed_repair() {
        let at = Instant::now();
        let mut frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 16,
            source_height: 16,
            image: RgbImage::from_pixel(16, 16, Rgb([80, 100, 120])),
        };
        let mut sender = FrameSender::with_codecs(false, true);
        let mut decoder = TileDecoder::new();
        decoder.set_lossless_enabled(true);
        let pool = super::super::frame::new_decode_buffer_pool();
        let mut bytes = Vec::new();
        sender.plan_at(&frame, at).unwrap();
        sender
            .encoder
            .encode_into(&mut bytes, 0, &frame, 70)
            .unwrap();
        decoder.decode_into(&bytes, Arc::clone(&pool)).unwrap();
        assert!(!decoder.last_frame_was_lossless());
        sender.commit_at(&frame, at);
        assert!(sender.encoder.has_exact_debt());
        frame.image.as_mut()[0] += 1;
        sender.plan_at(&frame, at + Duration::from_secs(1)).unwrap();
        assert!(
            !sender
                .encoder
                .has_stable_exact_debt(at + Duration::from_secs(1))
        );
        sender
            .plan_at(&frame, at + Duration::from_millis(2999))
            .unwrap();
        assert!(sender.pending_refinement_at.is_none());
        sender.plan_at(&frame, at + Duration::from_secs(3)).unwrap();
        assert!(sender.pending_refinement_at.is_some());
        sender
            .encoder
            .encode_into(&mut bytes, 1, &frame, 70)
            .unwrap();
        assert!(sender.encoder.has_exact_debt());
        sender.discard();
        assert!(sender.encoder.has_exact_debt());
        sender.plan_at(&frame, at + Duration::from_secs(3)).unwrap();
        sender
            .encoder
            .encode_into(&mut bytes, 1, &frame, 70)
            .unwrap();
        let repaired = decoder.decode_into(&bytes, pool).unwrap();
        assert!(decoder.last_frame_was_lossless());
        assert_eq!(repaired.image(), &frame.image);
        sender.commit_at(&frame, at + Duration::from_secs(3));
        assert!(!sender.encoder.has_exact_debt());
        assert!(
            sender
                .plan_record_at(&frame, at + Duration::from_secs(4), true, false)
                .is_err()
        );
    }

    #[test]
    fn stable_lossless_batches_do_not_restart_idle_delay_but_failures_back_off() {
        let at = Instant::now();
        let mut frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 272,
            source_height: 256,
            image: RgbImage::from_pixel(272, 256, Rgb([90, 110, 130])),
        };
        let mut sender = FrameSender::with_codecs(false, true);
        let mut bytes = Vec::new();
        sender.plan_at(&frame, at).unwrap();
        sender
            .encoder
            .encode_into(&mut bytes, 0, &frame, 70)
            .unwrap();
        sender.commit_at(&frame, at);
        let stable = at + STATIC_REFINEMENT_INTERVAL;
        let first = sender.plan_at(&frame, stable).unwrap();
        assert_eq!(first.dirty_tiles, 256);
        sender
            .encoder
            .encode_into(&mut bytes, 1, &frame, 70)
            .unwrap();
        let flushed = stable + Duration::from_millis(700);
        sender.commit_at(&frame, flushed);
        assert!(sender.encoder.has_exact_debt());
        sender
            .plan_at(&frame, flushed + Duration::from_millis(249))
            .unwrap();
        assert!(sender.pending_refinement_at.is_none());
        let next = flushed + Duration::from_millis(250);
        let second = sender.plan_at(&frame, next).unwrap();
        assert_eq!(second.dirty_tiles, 16);
        assert!(sender.pending_refinement_at.is_some());
        // Simulate the production typed-rejection path; it cannot discharge debt.
        sender.discard();
        sender.lossless_retry_after = Some(next + STATIC_REFINEMENT_INTERVAL);
        sender
            .plan_at(&frame, next + Duration::from_millis(34))
            .unwrap();
        assert!(sender.pending_refinement_at.is_none());
        // A raw change must not erase failed-encode backoff.
        frame.image.as_mut()[0] += 1;
        sender
            .plan_at(&frame, next + Duration::from_millis(50))
            .unwrap();
        assert_eq!(
            sender.lossless_retry_after,
            Some(next + STATIC_REFINEMENT_INTERVAL)
        );
        assert!(sender.pending_refinement_at.is_none());
        let recovered = next + STATIC_REFINEMENT_INTERVAL + Duration::from_millis(50);
        sender.plan_at(&frame, recovered).unwrap();
        assert!(sender.pending_refinement_at.is_some());
    }

    #[test]
    fn tile_local_lossless_repairs_static_neighbor_only_after_dirty_jpeg() {
        let at = Instant::now();
        let mut frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 32,
            source_height: 16,
            image: RgbImage::from_pixel(32, 16, Rgb([80, 100, 120])),
        };
        let mut sender = FrameSender::with_codecs(false, true);
        let mut decoder = TileDecoder::new();
        decoder.set_lossless_enabled(true);
        let pool = super::super::frame::new_decode_buffer_pool();
        let mut bytes = Vec::new();
        sender.plan_at(&frame, at).unwrap();
        sender
            .encoder
            .encode_into(&mut bytes, 0, &frame, 70)
            .unwrap();
        decoder.decode_into(&bytes, Arc::clone(&pool)).unwrap();
        sender.commit_at(&frame, at);
        for (sequence, seconds) in [(1, 1), (2, 2)] {
            frame.image.as_mut()[0] += 20;
            let now = at + Duration::from_secs(seconds);
            let plan = sender.plan_at(&frame, now).unwrap();
            assert_eq!(plan.dirty_tiles, 1);
            assert!(sender.pending_refinement_at.is_none());
            sender
                .encoder
                .encode_into(&mut bytes, sequence, &frame, 70)
                .unwrap();
            decoder.decode_into(&bytes, Arc::clone(&pool)).unwrap();
            assert!(!decoder.last_frame_was_lossless());
            sender.commit_at(&frame, now);
        }
        let idle = at + Duration::from_millis(2250);
        let repair = sender.plan_at(&frame, idle).unwrap();
        assert_eq!(repair.dirty_tiles, 1);
        assert!(sender.pending_refinement_at.is_some());
        sender
            .encoder
            .encode_into(&mut bytes, 3, &frame, 70)
            .unwrap();
        let result = decoder.decode_into(&bytes, Arc::clone(&pool)).unwrap();
        assert!(decoder.last_frame_was_lossless());
        for y in 0..16 {
            for x in 16..32 {
                assert_eq!(result.image().get_pixel(x, y), frame.image.get_pixel(x, y));
            }
        }
        sender.commit_at(&frame, idle);
        assert!(sender.encoder.has_exact_debt());
        sender
            .plan_at(&frame, at + Duration::from_millis(2400))
            .unwrap();
        assert!(sender.pending_refinement_at.is_none());
        sender.plan_at(&frame, at + Duration::from_secs(4)).unwrap();
        assert!(sender.pending_refinement_at.is_some());
        sender
            .encoder
            .encode_into(&mut bytes, 4, &frame, 70)
            .unwrap();
        let result = decoder.decode_into(&bytes, pool).unwrap();
        assert_eq!(result.image(), &frame.image);
        sender.commit_at(&frame, at + Duration::from_secs(4));
        assert!(!sender.encoder.has_exact_debt());
    }

    #[test]
    fn new_lossless_quiet_gate_is_exact_and_legacy_policy_remains_available() {
        for policy in [LosslessSchedule::New250, LosslessSchedule::Legacy100] {
            let base = Instant::now();
            let mut frame = proof_test_frame();
            let mut sender = FrameSender::with_codecs(false, true);
            sender.lossless_schedule = policy;
            let mut bytes = Vec::new();
            sender.plan_at(&frame, base).unwrap();
            sender
                .encoder
                .encode_into(&mut bytes, 0, &frame, 70)
                .unwrap();
            sender.commit_at(&frame, base);
            let changed = base + Duration::from_secs(3);
            frame.image.as_mut()[0] += 1;
            sender.plan_at(&frame, changed).unwrap();
            assert_eq!(
                sender.pending_refinement_at.is_some(),
                policy == LosslessSchedule::Legacy100
            );
            sender.discard();
            sender
                .plan_at(&frame, changed + Duration::from_millis(249))
                .unwrap();
            assert_eq!(
                sender.pending_refinement_at.is_some(),
                policy == LosslessSchedule::Legacy100
            );
            sender.discard();
            sender
                .plan_at(&frame, changed + Duration::from_millis(250))
                .unwrap();
            assert!(sender.pending_refinement_at.is_some());
        }
    }
    #[test]
    fn pending_png_supersession_compares_scope_and_rgb_without_consuming_latest() {
        let current = test_pending_frame(90);
        let mailbox = LatestMailbox::new();
        mailbox.publish(test_pending_frame(90)).unwrap();
        assert!(
            !mailbox
                .inspect_latest(
                    |next| next.is_some_and(|n| newer_frame_supersedes_repair(&current, n))
                )
                .unwrap()
        );
        let same = mailbox.receive().unwrap().unwrap();
        assert!(!newer_frame_supersedes_repair(&current, &same));
        let mut newer = test_pending_frame(90);
        newer.source_version.epoch += 1;
        assert!(newer_frame_supersedes_repair(&current, &newer));
        let mut newer = test_pending_frame(90);
        newer.frame.geometry_epoch += 1;
        assert!(newer_frame_supersedes_repair(&current, &newer));
        let mut newer = test_pending_frame(90);
        newer.frame.source_width += 1;
        assert!(newer_frame_supersedes_repair(&current, &newer));
        let mut newer = test_pending_frame(90);
        newer.frame.image = RgbImage::from_pixel(3, 2, Rgb([90, 0, 0]));
        assert!(newer_frame_supersedes_repair(&current, &newer));
        mailbox.publish(test_pending_frame(91)).unwrap();
        assert!(
            mailbox
                .inspect_latest(
                    |next| next.is_some_and(|n| newer_frame_supersedes_repair(&current, n))
                )
                .unwrap()
        );
        assert_eq!(
            mailbox
                .receive()
                .unwrap()
                .unwrap()
                .frame
                .image
                .get_pixel(0, 0)[0],
            91
        );
    }

    fn proof_test_frame() -> CapturedFrame {
        CapturedFrame {
            geometry_epoch: 1,
            source_width: 128,
            source_height: 96,
            image: RgbImage::from_pixel(128, 96, Rgb([80, 100, 120])),
        }
    }
    #[test]
    fn proof_delta_static_new_sequence_exact_small_damage_and_refinement() {
        let now = Instant::now();
        let source = proof_test_frame();
        let mut sender = FrameSender::new();
        let mut decoder = TileDecoder::new();
        let mut bytes = Vec::new();
        assert!(
            sender
                .plan_record_at(&source, now, true, false)
                .unwrap()
                .keyframe
        );
        sender
            .encoder
            .encode_into(&mut bytes, 1, &source, 70)
            .unwrap();
        let (_, original) = decode_wire(&mut decoder, &bytes);
        sender.commit_at(&source, now);
        let static_plan = sender.plan_record_at(&source, now, true, false).unwrap();
        assert!(static_plan.emit && !static_plan.keyframe && static_plan.dirty_tiles == 0);
        // Both quality replans retain mandatory emission, even if debt vanishes.
        sender.refine_at(90, now).unwrap();
        let lower = sender.refine_at(60, now).unwrap();
        assert!(lower.emit && lower.dirty_tiles == 0);
        sender
            .encoder
            .encode_into(&mut bytes, 2, &source, 60)
            .unwrap();
        let (seq, identical) = decode_wire(&mut decoder, &bytes);
        assert_eq!(seq, 2);
        assert_eq!(identical, original);
        sender.commit_at(&source, now);
        let mut changed = proof_test_frame();
        changed.image.get_pixel_mut(2, 2).0[0] += 1;
        assert!(
            !sender.plan_at(&changed, now).unwrap().emit,
            "ordinary tolerance unchanged"
        );
        let proof = sender.plan_record_at(&changed, now, true, false).unwrap();
        assert!(
            proof.emit
                && !proof.keyframe
                && proof.dirty_tiles > 0
                && proof.dirty_tiles < proof.total_tiles
        );
        sender
            .encoder
            .encode_into(&mut bytes, 3, &changed, 90)
            .unwrap();
        assert_eq!(decode_wire(&mut decoder, &bytes).0, 3);
        sender.commit_at(&changed, now);
        assert_eq!(
            sender
                .plan_record_at(&changed, now, true, false)
                .unwrap()
                .dirty_tiles,
            0
        );
    }
    #[test]
    fn proof_delta_failed_flush_does_not_commit_and_resets_still_keyframe() {
        let now = Instant::now();
        let source = proof_test_frame();
        let mut sender = FrameSender::new();
        let mut bytes = Vec::new();
        sender.plan_record_at(&source, now, true, false).unwrap();
        sender
            .encoder
            .encode_into(&mut bytes, 1, &source, 70)
            .unwrap();
        sender.commit_at(&source, now);
        let mut changed = proof_test_frame();
        changed.image.get_pixel_mut(2, 2).0[0] += 1;
        let dirty = sender
            .plan_record_at(&changed, now, true, false)
            .unwrap()
            .dirty_tiles;
        sender
            .encoder
            .encode_into(&mut bytes, 2, &changed, 70)
            .unwrap();
        let mut writer = SessionWriter::new(
            DeadlineWriter::new(FlushFailWriter::default(), WRITE_TIMEOUT),
            [7; 32],
        );
        assert!(write_frame_record(&mut writer, &bytes, WRITE_TIMEOUT).is_err());
        sender.discard();
        assert_eq!(sender.committed_at, Some(now));
        assert_eq!(
            sender
                .plan_record_at(&changed, now, true, false)
                .unwrap()
                .dirty_tiles,
            dirty
        );
        sender.discard();
        changed.geometry_epoch += 1;
        assert!(
            sender
                .plan_record_at(&changed, now, true, false)
                .unwrap()
                .keyframe
        );
        sender.discard();
        let mut resized = proof_test_frame();
        resized.image = RgbImage::from_pixel(96, 96, Rgb([80, 100, 120]));
        resized.source_width = 96;
        assert!(
            sender
                .plan_record_at(&resized, now, true, false)
                .unwrap()
                .keyframe
        );
        sender = FrameSender::with_codecs(sender.scroll_copy, sender.static_lossless); // source-switch reset
        assert!(
            sender
                .plan_record_at(&source, now, true, false)
                .unwrap()
                .keyframe
        );
        sender
            .encoder
            .encode_into(&mut bytes, 3, &source, 70)
            .unwrap();
        sender.commit_at(&source, now);
        assert!(
            sender
                .plan_record_at(&source, now, true, true)
                .unwrap()
                .keyframe,
            "test-only baseline remains forced keyframe"
        );
    }
    #[test]
    fn retained_proof_deadline_model_preserves_real_fps_slots_only() {
        let base = Instant::now();
        for interval in [Duration::from_millis(33), Duration::from_secs(1)] {
            let real_deadline = base + interval;
            assert!(
                capture_rate_limit_wait(base, real_deadline, CAPTURE_RATE_LIMIT_SLICE).is_some()
            );
            let idle_attempt = real_deadline + Duration::from_millis(2);
            let charged = idle_attempt + interval;
            let blocked = retained_proof_deadline(real_deadline, charged, true, false);
            assert_eq!(blocked, real_deadline);
            // Repeated no-readback service cannot postpone the next real start.
            assert_eq!(
                retained_proof_deadline(blocked, charged + interval, true, false),
                real_deadline
            );
            assert!(
                capture_rate_limit_wait(idle_attempt, blocked, CAPTURE_RATE_LIMIT_SLICE).is_none()
            );
            // A real attempt, ordinary NoChange, or backpressure keeps its charge.
            assert_eq!(
                retained_proof_deadline(real_deadline, charged, false, false),
                charged
            );
            assert_eq!(
                retained_proof_deadline(real_deadline, charged, true, true),
                charged
            );
            let next_real_deadline = idle_attempt + interval;
            assert!(
                capture_rate_limit_wait(idle_attempt, next_real_deadline, CAPTURE_RATE_LIMIT_SLICE)
                    .is_some()
            );
        }
        assert!(captured_frame_from_outcome(CaptureOutcome::RetainedProofBlocked).is_none());
    }
    #[test]
    fn proof_ack_wake_predicate_requires_exact_unannounced_scope() {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 7,
            frame_sequence: 9,
        };
        let frame = PointerFrame {
            serial: 12,
            at: Instant::now(),
            scope: Some(scope),
            announced: false,
        };
        assert!(proof_ack_matches(Some(frame), scope));
        assert!(!proof_ack_matches(None, scope));
        assert!(!proof_ack_matches(
            Some(PointerFrame {
                announced: true,
                ..frame
            }),
            scope
        ));
        assert!(!proof_ack_matches(
            Some(PointerFrame {
                scope: None,
                ..frame
            }),
            scope
        ));
        for mismatch in [
            DrawnScope {
                source_epoch: 2,
                ..scope
            },
            DrawnScope {
                geometry_epoch: 8,
                ..scope
            },
            DrawnScope {
                frame_sequence: 8,
                ..scope
            },
            DrawnScope {
                frame_sequence: 10,
                ..scope
            },
        ] {
            assert!(!proof_ack_matches(Some(frame), mismatch));
        }
    }
    // Real eventfd regression: CI only, not part of local pure tests.
    #[test]
    fn proof_ack_wake_fd_live_scope_only_and_terminal_modes_stay_quiet() {
        use nix::poll::{PollFd, PollFlags, poll};
        let sources = test_source_control();
        assert!(
            sources
                .input_wake
                .set(input_wake::InputWake::new().unwrap())
                .is_ok()
        );
        let wake = sources.input_wake.get().unwrap();
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 7,
            frame_sequence: 9,
        };
        *sources.pointer_frame.lock().unwrap() = Some(PointerFrame {
            serial: 12,
            at: Instant::now(),
            scope: Some(scope),
            announced: false,
        });
        let mut fds = [PollFd::new(wake.as_fd(), PollFlags::POLLIN)];
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
        sources.control_enabled.store(true, Ordering::Release);
        sources
            .notify_proof_ack(DrawnScope {
                frame_sequence: 8,
                ..scope
            })
            .unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 1);
        wake.drain().unwrap();
        sources
            .pointer_frame
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .announced = true;
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
        sources
            .pointer_frame
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .announced = false;
        sources.control_revoke.store(true, Ordering::Release);
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
        sources.control_enabled.store(false, Ordering::Release);
        sources.pointer_enabled.store(true, Ordering::Release);
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 1);
        wake.drain().unwrap();
        sources.pointer_revoke.store(true, Ordering::Release);
        sources.notify_proof_ack(scope).unwrap();
        assert_eq!(poll(&mut fds, 0u8).unwrap(), 0);
    }
    #[test]
    fn input_wake_model_backlog_excludes_disabled_and_waiting_modes() {
        let sources = test_source_control();
        // No descriptor is created by SourceControl::new.
        assert!(sources.input_wake.get().is_none());
        assert!(!sources.input_backlog());
        sources.control_revoke.store(true, Ordering::Release);
        assert!(
            !sources.input_backlog(),
            "terminal disabled mode must not spin"
        );
        sources.control_enabled.store(true, Ordering::Release);
        assert!(
            sources.input_backlog(),
            "live revoke must be handled before waiting"
        );
        sources.control_revoke.store(false, Ordering::Release);
        assert!(
            !sources.input_backlog(),
            "enabled but awaiting fresh frame is not queued work"
        );
        sources.pointer_enabled.store(true, Ordering::Release);
        sources.pointer_revoke.store(true, Ordering::Release);
        assert!(sources.input_backlog());
        sources.pointer_enabled.store(false, Ordering::Release);
        assert!(!sources.input_backlog());
    }
    #[test]
    fn host_cleanup_clipboard_startup_failure_preserves_error_and_disables_admission() {
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        let error = run_admitted_session(slot, || {
            Err(host_clipboard_start_failed("backend unavailable".into()))
        })
        .unwrap_err();
        assert!(cleanup_incomplete(error.as_ref()));
        assert!(error.to_string().contains("backend unavailable"));
        assert!(error.to_string().contains("startup cleanup unconfirmed"));
        assert!(ViewerSlot::claim(&admission).is_none());
    }

    #[test]
    fn host_cleanup_classification_requires_typed_evidence() {
        let plain = io::Error::other("input cleanup unconfirmed");
        assert!(!cleanup_incomplete(&plain));
        let typed = combine_host_cleanup(Ok(()), Err(plain.into())).unwrap_err();
        assert!(cleanup_incomplete(typed.as_ref()));
        let diagnostic = crate::remote::client::classify_failure(typed.as_ref());
        assert_eq!(diagnostic.category, "cleanup_failed");
        assert_eq!(diagnostic.cleanup, "incomplete");
        assert!(!diagnostic.reconnect_certified);
    }

    #[test]
    fn host_cleanup_completed_worker_allows_replacement() {
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        let mut clipboard_called = false;
        run_admitted_session(slot, || {
            cleanup_host_session(
                Instant::now() + Duration::from_secs(2),
                StopCause::Graceful,
                Ok(()),
                thread::spawn(|| Ok(())),
                |remaining| {
                    clipboard_called = true;
                    assert!(remaining <= Duration::from_secs(2));
                    Ok(())
                },
            )
        })
        .unwrap();
        assert!(clipboard_called);
        assert!(ViewerSlot::claim(&admission).is_some());
    }

    #[test]
    fn host_cleanup_blocked_input_shares_deadline_preserves_errors_and_poison() {
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let (exited, exit) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            blocked.recv().unwrap();
            exited.send(()).unwrap();
            Ok(())
        });
        let mut clipboard_called = false;
        let error = run_admitted_session(slot, || {
            cleanup_host_session(
                Instant::now(),
                StopCause::Capture,
                Err(io::Error::other("primary capture failure").into()),
                worker,
                |remaining| {
                    clipboard_called = true;
                    assert!(
                        remaining.is_zero(),
                        "clipboard cannot receive a fresh budget"
                    );
                    Err(io::Error::other("clipboard failed too").into())
                },
            )
        })
        .unwrap_err();
        assert!(clipboard_called);
        let message = error.to_string();
        assert!(message.contains("primary capture failure"));
        assert!(message.contains("input cleanup unconfirmed"));
        assert!(message.contains("input release is not confirmed"));
        assert!(message.contains("clipboard failed too"));
        assert!(ViewerSlot::claim(&admission).is_none());
        release.send(()).unwrap();
        exit.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(admission.load(Ordering::Acquire), VIEWER_POISONED);
        assert!(ViewerSlot::claim(&admission).is_none());
    }

    #[test]
    fn host_cleanup_waits_for_thread_local_destructors() {
        struct BlockOnDrop(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
        impl Drop for BlockOnDrop {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
                self.1.recv().unwrap();
            }
        }
        thread_local! {
            static BLOCK: std::cell::RefCell<Option<BlockOnDrop>> = const { std::cell::RefCell::new(None) };
        }
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            BLOCK.with(|value| *value.borrow_mut() = Some(BlockOnDrop(entered, blocked)));
            Ok(())
        });
        entering.recv_timeout(Duration::from_secs(2)).unwrap();
        let outcome = join_host_input_until(worker, Instant::now());
        release.send(()).unwrap();
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("input cleanup unconfirmed")
        );
    }

    #[test]
    fn host_cleanup_input_panic_preserves_primary_and_disables_admission() {
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        let error = run_admitted_session(slot, || {
            cleanup_host_session(
                Instant::now() + Duration::from_secs(2),
                StopCause::Capture,
                Err(io::Error::other("capture failed first").into()),
                thread::spawn(|| panic!("input panic")),
                |_| Ok(()),
            )
        })
        .unwrap_err();
        assert!(error.to_string().contains("capture failed first"));
        assert!(error.to_string().contains("input thread panicked"));
        assert!(ViewerSlot::claim(&admission).is_none());
    }

    #[test]
    fn host_cleanup_release_failure_is_never_hidden_by_stop_cause() {
        for cause in [
            StopCause::Input,
            StopCause::Graceful,
            StopCause::Capture,
            StopCause::Sender,
            StopCause::Clipboard,
            StopCause::ThreadPanic,
            StopCause::None,
        ] {
            let input = combine_host_cleanup(
                Err(io::Error::other("input protocol failure").into()),
                Err(io::Error::other("X11 release unconfirmed").into()),
            );
            let error = finish_host_session(
                cause,
                Err(io::Error::other("stream failure").into()),
                Ok(input),
                Ok(()),
            )
            .unwrap_err();
            assert!(error.is::<HostCleanupError>());
            for message in [
                "input protocol failure",
                "X11 release unconfirmed",
                "stream failure",
            ] {
                assert!(error.to_string().contains(message));
            }
        }
    }

    #[test]
    fn host_cleanup_once_outcome_is_failure_after_clean_stream() {
        // Exercise the exact admitted outcome sent to the --once listener.
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        let outcome = run_admitted_session(slot, || {
            cleanup_host_session(
                Instant::now() + Duration::from_secs(2),
                StopCause::Graceful,
                Ok(()),
                thread::spawn(|| Ok(())),
                |_| Err(io::Error::other("clipboard incomplete").into()),
            )
        });
        assert!(outcome.is_err());
        assert!(ViewerSlot::claim(&admission).is_none());
    }

    #[test]
    fn host_cleanup_session_unwind_disables_admission_but_ordinary_error_does_not() {
        let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
        let slot = ViewerSlot::claim(&admission).unwrap();
        assert!(run_admitted_session(slot, || Err(io::Error::other("bad peer").into())).is_err());
        let slot = ViewerSlot::claim(&admission).unwrap();
        assert!(run_admitted_session(slot, || panic!("session panic")).is_err());
        assert!(ViewerSlot::claim(&admission).is_none());
    }

    #[test]
    fn clipboard_cleanup_preserves_host_panic_for_every_stop_cause() {
        for cause in [
            StopCause::Input,
            StopCause::Graceful,
            StopCause::Capture,
            StopCause::Sender,
            StopCause::Clipboard,
            StopCause::ThreadPanic,
            StopCause::None,
        ] {
            let error =
                finish_host_session(cause, Ok(()), Err(Box::new("panic")), Ok(())).unwrap_err();
            assert!(error.to_string().contains("input thread panicked"));
            let error = finish_host_session(
                cause,
                Ok(()),
                Err(Box::new("panic")),
                Err(io::Error::other("clipboard cleanup incomplete").into()),
            )
            .unwrap_err();
            let cleanup = error.downcast_ref::<HostCleanupError>().unwrap();
            assert!(
                cleanup
                    .primary
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("input thread panicked")
            );
            assert!(error.to_string().contains("clipboard cleanup incomplete"));
        }
    }

    use super::*;
    use crate::remote::frame::new_decode_buffer_pool;
    use crate::remote::tile::TileDecoder;
    use image::{Rgb, RgbImage};
    use std::io::{Cursor, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU64;

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

    #[test]
    fn scoped_sender_switches_same_size_with_fresh_keyframe_and_no_old_credit_deadlock() {
        let mailbox = Arc::new(LatestMailbox::new());
        let gate = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..GateState::default()
            }),
            Condvar::new(),
        ));
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0xa7; 32],
        )));
        let running = Arc::new(AtomicBool::new(true));
        let credits = Arc::new(FrameCredits::new());
        let sources = Arc::new(test_source_control());
        let sender_mailbox = Arc::clone(&mailbox);
        let sender_writer = Arc::clone(&writer);
        let sender_running = Arc::clone(&running);
        let sender_credits = Arc::clone(&credits);
        let sender_sources = Arc::clone(&sources);
        let sender = thread::spawn(move || {
            let quality = JpegQualityController::new_at(
                70,
                40,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap();
            send_frames_scoped(
                sender_mailbox,
                sender_writer,
                &sender_running,
                &sender_credits,
                &HostTelemetry::new(),
                &quality,
                Some(&sender_sources),
            )
        });
        let wait_flushes = |count| {
            let state = gate.0.lock().unwrap();
            let (state, timeout) = gate
                .1
                .wait_timeout_while(state, Duration::from_secs(2), |state| state.flushes < count)
                .unwrap();
            assert!(
                !timeout.timed_out(),
                "source sender did not reach flush {count}"
            );
            drop(state);
        };
        mailbox.publish(test_pending_frame(10)).unwrap();
        wait_flushes(1);
        mailbox.publish(test_pending_frame(20)).unwrap();
        {
            let state = gate.0.lock().unwrap();
            let (state, timeout) = gate
                .1
                .wait_timeout_while(state, Duration::from_millis(30), |state| state.flushes < 2)
                .unwrap();
            assert!(
                timeout.timed_out(),
                "second frame bypassed first-presentation barrier"
            );
            assert_eq!(state.flushes, 1);
        }
        credits
            .acknowledge_scoped_at(0, Some(1), Instant::now(), |_| {})
            .unwrap();
        {
            let mut state = sources.state.lock().unwrap();
            let version = state.policy.current();
            state.policy.acknowledge_presentation(version, 0).unwrap();
        }
        sources.changed.notify_all();
        wait_flushes(2);
        // Do not ACK frame1: client invalidation suppresses its old completion.
        let next = sources
            .state
            .lock()
            .unwrap()
            .policy
            .begin_switch(1, SourceId(2))
            .unwrap();
        {
            let mut writer = lock_writer(&writer);
            mailbox.clear();
            credits.abandon_source();
            sources
                .state
                .lock()
                .unwrap()
                .policy
                .finish_switch(next, |_| Ok::<(), ()>(()))
                .unwrap()
                .unwrap();
            write_session_record(
                &mut writer,
                MessageKind::SourceChanged,
                &encode_source_changed(next).unwrap(),
                FRAME_WRITE_DEADLINE,
            )
            .unwrap();
        }
        let mut pending = test_pending_frame(90);
        pending.source_version = next;
        mailbox.publish(pending).unwrap();
        sources.changed.notify_all();
        wait_flushes(4);
        running.store(false, Ordering::Release);
        mailbox.close();
        credits.close();
        sources.changed.notify_all();
        sender.join().unwrap().unwrap();
        let mut reader = SessionReader::new(
            Cursor::new(gate.0.lock().unwrap().bytes.clone()),
            [0xa7; 32],
        );
        let mut decoder = TileDecoder::new();
        for sequence in [0, 1] {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Frame);
            let (epoch, inner) =
                super::super::source_wire::decode_source_payload(&payload).unwrap();
            assert_eq!(epoch, 1);
            assert_eq!(decode_wire(&mut decoder, inner).0, sequence);
        }
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::SourceChanged);
        assert_eq!(
            super::super::source_wire::decode_source_changed(&payload).unwrap(),
            next
        );
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Frame);
        let (epoch, inner) = super::super::source_wire::decode_source_payload(&payload).unwrap();
        assert_eq!(epoch, 2);
        let (sequence, image) = decode_wire(&mut TileDecoder::new(), inner);
        assert_eq!(sequence, 2);
        assert!(image.get_pixel(0, 0).0[0].abs_diff(90) <= 2);
        assert!(reader.read_message().is_err());
    }

    fn test_source_control() -> SourceControl {
        SourceControl::new(
            vec![
                (SourceId(1), CaptureArea::Monitor("DP-1".into())),
                (SourceId(2), CaptureArea::Monitor("DP-2".into())),
            ],
            InputArea {
                origin: (100, 20),
                size: Some((640, 480)),
            },
            7,
        )
        .unwrap()
    }

    #[test]
    fn source_input_fences_geometry_and_source_but_preserves_safe_release() {
        use super::super::x11_input::InputEvent;
        let control = test_source_control();
        let mut state = control.state.lock().unwrap();
        let first = state.policy.current();
        let press = [InputEvent::Button {
            button: 1,
            pressed: true,
        }];
        assert_eq!(
            source_input_admission(&state, 1, 7, &press),
            SourceInputAdmission::Stale
        );
        state.policy.publish_keyframe(first, 0).unwrap();
        state.policy.acknowledge_presentation(first, 0).unwrap();
        state.presented_geometry = Some(7);
        assert_eq!(
            source_input_admission(&state, 1, 7, &press),
            SourceInputAdmission::Inject(state.input_area)
        );
        state.geometry_epoch = 8;
        assert_eq!(
            source_input_admission(&state, 1, 7, &press),
            SourceInputAdmission::Stale
        );
        assert_eq!(
            source_input_admission(&state, 1, 7, &[InputEvent::ReleaseAll]),
            SourceInputAdmission::ReleaseOnly
        );
        state.policy.begin_switch(1, SourceId(2)).unwrap();
        assert_eq!(
            source_input_admission(&state, 1, 7, &[InputEvent::ReleaseAll]),
            SourceInputAdmission::Stale
        );
        assert_eq!(
            source_input_admission(&state, 2, 8, &press),
            SourceInputAdmission::Stale
        );
    }

    #[test]
    fn geometry_release_is_serialized_and_failed_release_does_not_advance() {
        let mut geometry = 7;
        let mut released = 0;
        refresh_input_geometry(&mut geometry, 7, || {
            released += 1;
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!(released, 0);
        assert_eq!(
            refresh_input_geometry(&mut geometry, 8, || Err("release failed")),
            Err("release failed")
        );
        assert_eq!(geometry, 7);
        refresh_input_geometry(&mut geometry, 8, || {
            released += 1;
            Ok::<(), ()>(())
        })
        .unwrap();
        assert_eq!((geometry, released), (8, 1));
    }

    #[test]
    fn input_geometry_guard_remains_owned_through_release_and_injection() {
        use super::super::x11_input::InputEvent;
        let control = test_source_control();
        {
            let mut state = control.state.lock().unwrap();
            let version = state.policy.current();
            state.policy.publish_keyframe(version, 0).unwrap();
            state.policy.acknowledge_presentation(version, 0).unwrap();
            state.presented_geometry = Some(7);
        }
        let mut geometry = 6;
        let mut actions = Vec::new();
        control
            .apply_input(
                &mut geometry,
                1,
                7,
                &[InputEvent::Pointer { x: 1, y: 2 }],
                |admission| {
                    assert!(
                        control.state.try_lock().is_err(),
                        "capture must not replace geometry while injecting"
                    );
                    actions.push(admission);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(
            actions,
            vec![
                SourceInputAdmission::ReleaseOnly,
                SourceInputAdmission::Inject(InputArea {
                    origin: (100, 20),
                    size: Some((640, 480))
                })
            ]
        );
        assert_eq!(geometry, 7);
        assert!(control.state.try_lock().is_ok());
    }

    #[test]
    fn source_credit_barrier_retires_without_ack_and_rejects_wrong_epoch() {
        let credits = FrameCredits::new();
        let now = Instant::now();
        credits
            .mark_sent_scoped(0, now, now, 100, 70, 0, 1, 7)
            .unwrap();
        credits
            .mark_sent_scoped(1, now, now, 100, 70, 0, 1, 7)
            .unwrap();
        assert!(
            credits
                .acknowledge_scoped_at(1, Some(2), now, |_| {})
                .is_err()
        );
        assert_eq!(credits.outstanding(), 2);
        credits.abandon_source();
        assert_eq!(credits.outstanding(), 0);
        credits
            .mark_sent_scoped(2, now, now, 100, 70, 0, 2, 9)
            .unwrap();
        assert!(
            credits
                .acknowledge_scoped_at(2, Some(1), now, |_| {})
                .is_err()
        );
        assert_eq!(credits.outstanding(), 1);
        let ack = credits
            .acknowledge_scoped_at(2, Some(2), now, |_| {})
            .unwrap()
            .unwrap();
        assert_eq!(
            (ack.source_epoch, ack.geometry_epoch, ack.retired),
            (2, 9, 1)
        );
        assert_eq!(credits.outstanding(), 0);
    }

    #[test]
    fn pending_source_and_same_sized_switch_keep_input_closed_until_exact_ack() {
        let control = test_source_control();
        let running = AtomicBool::new(true);
        let first = control.state.lock().unwrap().policy.current();
        assert!(control.sender_ready(first, &running).unwrap());
        {
            let mut state = control.state.lock().unwrap();
            state.policy.publish_keyframe(first, 0).unwrap();
            state.policy.acknowledge_presentation(first, 0).unwrap();
            let second = state.policy.begin_switch(1, SourceId(2)).unwrap();
            assert!(!state.policy.accepts_capture(second));
            state
                .policy
                .finish_switch(second, |_| Ok::<(), ()>(()))
                .unwrap()
                .unwrap();
            state.policy.publish_keyframe(second, 1).unwrap();
            assert!(state.policy.acknowledge_presentation(first, 0).is_err());
            assert!(state.policy.acknowledge_presentation(second, 0).is_err());
            assert!(!state.policy.accepts_input(second));
            state.policy.acknowledge_presentation(second, 1).unwrap();
            assert!(state.policy.accepts_input(second));
        }
        assert!(!control.sender_ready(first, &running).unwrap());
    }

    #[test]
    fn source_grants_never_add_root_to_scoped_startup() {
        let mut options = test_host_source_options();
        let grants = host_source_grants(&options).unwrap();
        assert_eq!(
            grants,
            vec![(SourceId(1), CaptureArea::Monitor("DP-1".into()))]
        );
        options.shared_sources = vec![
            CaptureArea::Monitor("DP-1".into()),
            CaptureArea::Monitor("DP-2".into()),
        ];
        let grants = host_source_grants(&options).unwrap();
        assert_eq!(grants.len(), 2);
        assert!(!grants.iter().any(|(_, area)| *area == CaptureArea::Root));
        options.shared_sources.push(CaptureArea::Root);
        assert_eq!(
            host_source_grants(&options).unwrap()[2],
            (SourceId(3), CaptureArea::Root)
        );
        options.shared_sources = vec![CaptureArea::Root; MAX_GRANTED_SOURCES];
        assert!(host_source_grants(&options).is_err());
        options.shared_sources = vec![CaptureArea::Monitor("bad\nlabel".into())];
        assert!(host_source_grants(&options).is_err());
    }

    #[test]
    fn window_grants_are_exclusive_view_only_single_use() {
        let mut options = test_host_source_options();
        options.capture_area = CaptureArea::Window(42);
        assert!(validate_window_policy(&options).is_ok());
        for denied in 0..4 {
            let mut options = test_host_source_options();
            options.capture_area = CaptureArea::Window(42);
            match denied {
                0 => options.allow_input = true,
                1 => options.allow_clipboard = true,
                2 => options.once = false,
                _ => options.shared_sources.push(CaptureArea::Root),
            }
            assert!(validate_window_policy(&options).is_err());
        }
        let mut options = test_host_source_options();
        options.shared_sources.push(CaptureArea::Window(42));
        assert!(validate_window_policy(&options).is_err());
    }

    #[test]
    fn explicit_resize_permission_does_not_expand_window_scope() {
        let mut options = test_host_source_options();
        options.allow_window_resize = true;
        assert!(validate_window_policy(&options).is_err());
        options.capture_area = CaptureArea::Window(42);
        assert!(validate_window_policy(&options).is_ok());
        options.allow_input = true;
        assert!(validate_window_policy(&options).is_err());
        options.allow_input = false;
        options.allow_clipboard = true;
        assert!(validate_window_policy(&options).is_err());
        options.allow_clipboard = false;
        options.once = false;
        assert!(validate_window_policy(&options).is_err());
    }

    #[test]
    fn popup_host_permission_is_explicit_exclusive_read_only() {
        let mut options = test_host_source_options();
        options.capture_area = CaptureArea::Root;
        options.wayland_select = true;
        options.wayland_popups = true;
        assert_eq!(
            validate_window_policy(&options).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for follow in [false, true] {
            options.wayland_resize_follow = follow;
            assert_eq!(
                validate_window_policy(&options).is_ok(),
                cfg!(feature = "remote-wayland")
            );
            for case in 0..10 {
                let mut denied = options.clone();
                match case {
                    0 => denied.wayland_select = false,
                    1 => denied.once = false,
                    2 => denied.allow_input = true,
                    3 => denied.allow_clipboard = true,
                    4 => denied.allow_window_resize = true,
                    5 => denied.allow_wayland_keyboard = true,
                    6 => denied.allow_wayland_pointer = true,
                    7 => denied.allow_wayland_control = true,
                    8 => denied.allow_wayland_scroll = true,
                    _ => denied.allow_wayland_horizontal_scroll = true,
                }
                assert!(validate_window_policy(&denied).is_err(), "case {case}");
            }
        }
    }
    #[test]
    fn resize_follow_host_permission_is_explicit_read_only() {
        let mut options = test_host_source_options();
        options.capture_area = CaptureArea::Root;
        options.wayland_select = true;
        options.wayland_resize_follow = true;
        assert_eq!(
            validate_window_policy(&options).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for case in 0..8 {
            let mut denied = options.clone();
            match case {
                0 => denied.wayland_select = false,
                1 => denied.allow_input = true,
                2 => denied.allow_clipboard = true,
                3 => denied.allow_window_resize = true,
                4 => denied.allow_wayland_keyboard = true,
                5 => denied.allow_wayland_pointer = true,
                6 => denied.allow_wayland_control = true,
                _ => denied.allow_wayland_scroll = true,
            }
            assert!(validate_window_policy(&denied).is_err(), "case {case}");
        }
    }
    #[test]
    fn wayland_selection_is_exclusive_local_and_never_accepts_stdin_key() {
        let mut options = test_host_source_options();
        options.capture_area = CaptureArea::Root;
        options.wayland_select = true;
        assert_eq!(
            validate_window_policy(&options).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for case in 0..8 {
            let mut denied = options.clone();
            match case {
                0 => denied.key = KeySource::Stdin,
                1 => denied.display = Some(":0".into()),
                2 => denied.allow_window_resize = true,
                3 => denied.allow_input = true,
                4 => denied.allow_clipboard = true,
                5 => denied.once = false,
                6 => denied.capture_area = CaptureArea::Window(7),
                _ => denied.shared_sources.push(CaptureArea::Root),
            }
            assert!(validate_window_policy(&denied).is_err());
        }
        options.wayland_select = false;
        options.wayland_display = Some("wayland-0".into());
        assert!(validate_window_policy(&options).is_err());
    }

    #[test]
    fn keyboard_lease_requires_explicit_native_wayland_host() {
        let mut options = test_host_source_options();
        options.allow_wayland_keyboard = true;
        assert!(validate_window_policy(&options).is_err());
        options.wayland_select = true;
        options.capture_area = CaptureArea::Root;
        assert_eq!(
            validate_window_policy(&options).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        options.allow_input = true;
        assert!(validate_window_policy(&options).is_err());
        options.allow_input = false;
        options.allow_window_resize = true;
        assert!(validate_window_policy(&options).is_err());
    }

    #[test]
    fn keyboard_drawn_gate_errors_are_terminal_and_cannot_be_retried() {
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        let valid = LeaseRequest {
            keymap_digest: [1; 32],
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        for (request, drawn, current) in [
            (valid, None, scope),
            (
                LeaseRequest {
                    geometry_epoch: 3,
                    ..valid
                },
                Some(scope),
                scope,
            ),
            (
                valid,
                Some(scope),
                DrawnScope {
                    geometry_epoch: 3,
                    ..scope
                },
            ),
            (
                LeaseRequest {
                    frame_sequence: 1,
                    ..valid
                },
                Some(scope),
                scope,
            ),
        ] {
            let mut state = HostKeyboardState::new();
            assert!(admit_keyboard_request(&mut state, request, drawn, current).is_err());
            assert!(state.grant(1, Instant::now()).is_err());
            assert!(admit_keyboard_request(&mut state, valid, Some(scope), scope).is_err());
        }
        let mut valid_state = HostKeyboardState::new();
        assert!(admit_keyboard_request(&mut valid_state, valid, Some(scope), scope).is_ok());
    }

    fn test_host_source_options() -> HostOptions {
        HostOptions {
            frame_limits: super::super::messages::FrameLimits::default(),
            listen: "127.0.0.1:0".into(),
            key: KeySource::File("/unused".into()),
            display: None,
            wayland_select: false,
            wayland_resize_follow: false,
            wayland_popups: false,
            wayland_display: None,
            fps: 12,
            jpeg_quality: 70,
            jpeg_quality_floor: 30,
            fixed_jpeg_quality: true,
            max_width: 1280,
            capture_source: CaptureSource::Auto,
            capture_area: CaptureArea::Monitor("DP-1".into()),
            shared_sources: Vec::new(),
            scaling: ScalingPolicy::Auto,
            allow_lan: false,
            allow_input: false,
            allow_window_resize: false,
            allow_wayland_keyboard: false,
            allow_wayland_pointer: false,
            allow_wayland_control: false,
            allow_wayland_text: false,
            allow_static_lossless: false,
            allow_wayland_scroll: false,
            allow_wayland_horizontal_scroll: false,
            scroll_copy: false,
            allow_clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            once: true,
            stats_json: false,
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

    struct CountedClipboardWriter<W> {
        inner: W,
        writes: Arc<AtomicU64>,
        flushes: Arc<AtomicU64>,
    }

    impl<W: SetWriteTimeout> SetWriteTimeout for CountedClipboardWriter<W> {
        fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.inner.set_write_timeout(timeout)
        }
    }

    impl<W: Write> Write for CountedClipboardWriter<W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.inner.write(bytes)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            self.inner.flush()
        }
    }

    fn text_record(text: &str) -> Outgoing {
        Outgoing {
            kind: MessageKind::Clipboard,
            payload: text.as_bytes().to_vec(),
        }
    }

    fn assert_clipboard_failure_stops_session<W: Write + SetWriteTimeout>(inner: W) {
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let mailbox = LatestMailbox::<PendingFrame>::new();
        let credits = FrameCredits::new();
        let stopped = std::cell::Cell::new(false);
        let writes = Arc::new(AtomicU64::new(0));
        let flushes = Arc::new(AtomicU64::new(0));
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(
                CountedClipboardWriter {
                    inner,
                    writes: Arc::clone(&writes),
                    flushes: Arc::clone(&flushes),
                },
                WRITE_TIMEOUT,
            ),
            [0x93; 32],
        ));
        let mut text = Some(text_record("clipboard text"));
        let result = run_clipboard_worker(
            || forward_clipboard_from(|_| Ok(text.take()), &writer, &running),
            &running,
            &first_stop,
            |close| {
                assert!(close.is_none(), "transport failures are not clipboard loss");
                assert!(writer.try_lock().is_ok(), "release writer before teardown");
                mailbox.close();
                credits.close();
                shutdown_stream(&writer, close, |_| {});
                stopped.set(true);
            },
        );
        assert!(runtime_close(result.as_ref().unwrap_err()).is_none());
        assert!(stopped.get());
        assert_eq!(first_stop.cause(), StopCause::Clipboard);
        assert!(mailbox.receive().unwrap().is_none());
        assert!(
            credits
                .wait_for_credit(&running, Duration::ZERO)
                .unwrap()
                .is_none()
        );
        assert!(lock_writer(&writer).get_mut().failed);
        let before = (
            writes.load(Ordering::Relaxed),
            flushes.load(Ordering::Relaxed),
        );
        say_session_close(
            &writer,
            SessionClose::new(SessionCloseCode::ClipboardUnavailable, "unavailable"),
        );
        assert!(
            write_session_record(
                &mut lock_writer(&writer),
                MessageKind::Clipboard,
                &[],
                FRAME_WRITE_DEADLINE
            )
            .is_err()
        );
        assert_eq!(
            (
                writes.load(Ordering::Relaxed),
                flushes.load(Ordering::Relaxed)
            ),
            before,
            "a poisoned record cannot touch the underlying writer again"
        );
        assert!(
            !running.load(Ordering::Acquire),
            "poisoned clipboard record must stop an otherwise static session"
        );
    }

    #[test]
    fn clipboard_write_failure_stops_an_otherwise_static_session() {
        assert_clipboard_failure_stops_session(FailingWriter);
    }

    #[test]
    fn clipboard_flush_failure_stops_an_otherwise_static_session() {
        assert_clipboard_failure_stops_session(FlushFailWriter::default());
    }

    #[test]
    fn clipboard_start_failure_stops_and_preserves_the_actual_error() {
        let running = AtomicBool::new(true);
        let first_stop = FirstStop::new();
        let mailbox = LatestMailbox::<PendingFrame>::new();
        let credits = FrameCredits::new();
        let stopped = std::cell::Cell::new(false);
        let writer = runtime_record_writer([0x97; 32]);
        let (worker, result) = admit_clipboard_worker::<()>(
            Some(Err(LocalRuntimeFailure::clipboard(
                io::Error::new(io::ErrorKind::WouldBlock, "injected spawn refusal").into(),
            ))),
            &running,
            &first_stop,
            |close| {
                mailbox.close();
                credits.close();
                assert!(!running.load(Ordering::Acquire));
                shutdown_stream(&writer, close, |_| {});
                stopped.set(true);
            },
        );
        assert!(worker.is_none());
        let wire = lock_writer(&writer).get_ref().get_ref().bytes.clone();
        crate::remote::client::tests::assert_runtime_terminal_record(
            &wire,
            [0x97; 32],
            SessionCloseCode::ClipboardUnavailable,
            "host clipboard became unavailable",
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected spawn refusal")
        );
        assert!(stopped.get());
        assert!(!running.load(Ordering::Acquire));
        assert_eq!(first_stop.cause(), StopCause::Clipboard);
        assert!(mailbox.receive().unwrap().is_none());
        assert!(
            credits
                .wait_for_credit(&running, Duration::ZERO)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn clipboard_disabled_or_started_does_not_stop_the_session() {
        for candidate in [None, Some(Ok(7_u8))] {
            let expected = candidate.as_ref().map(|_| 7);
            let running = AtomicBool::new(true);
            let first_stop = FirstStop::new();
            let (worker, result) = admit_clipboard_worker(candidate, &running, &first_stop, |_| {
                panic!("a successful or disabled clipboard does not shut down")
            });
            assert_eq!(worker, expected);
            assert!(result.is_ok());
            assert!(running.load(Ordering::Acquire));
            assert_eq!(first_stop.cause(), StopCause::None);
        }
    }

    #[test]
    fn clipboard_worker_failure_keeps_first_stop_and_does_not_repeat_shutdown() {
        for prior in [
            StopCause::Input,
            StopCause::Capture,
            StopCause::Sender,
            StopCause::Graceful,
        ] {
            for was_running in [false, true] {
                let running = Arc::new(AtomicBool::new(was_running));
                let first_stop = FirstStop::new();
                first_stop.record(prior);
                let calls = std::cell::Cell::new(0);
                assert!(
                    run_clipboard_worker(
                        || Err(io::Error::other("clipboard failure after first outcome").into()),
                        &running,
                        &first_stop,
                        |close| {
                            assert!(close.is_none());
                            calls.set(calls.get() + 1);
                        },
                    )
                    .is_err()
                );
                assert_eq!(first_stop.cause(), prior);
                assert_eq!(calls.get(), usize::from(was_running));
                assert!(!running.load(Ordering::Acquire));
            }
        }
    }

    #[test]
    fn clipboard_join_panic_is_reported_without_replacing_a_prior_cause() {
        for prior in [StopCause::None, StopCause::Input, StopCause::Graceful] {
            let first_stop = FirstStop::new();
            if prior != StopCause::None {
                first_stop.record(prior);
            }
            let joined: thread::Result<RemoteResult<()>> = Err(Box::new("injected panic"));
            let error = finish_clipboard_worker(Ok(()), Some(joined), &first_stop).unwrap_err();
            assert!(error.to_string().contains("clipboard thread panicked"));
            assert_eq!(
                first_stop.cause(),
                if prior == StopCause::None {
                    StopCause::Clipboard
                } else {
                    prior
                }
            );
        }
    }

    #[test]
    fn clipboard_capture_close_stops_worker_after_queued_text_without_spinning() {
        let gate = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0x95; 32],
        ));
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let calls = std::cell::Cell::new(0);
        let stopped = std::cell::Cell::new(false);
        let result = run_clipboard_worker(
            || {
                forward_clipboard_from(
                    |_| {
                        calls.set(calls.get() + 1);
                        match calls.get() {
                            1 => Ok(None),
                            2 => Ok(Some(text_record("last text"))),
                            3 => Err(std::sync::mpsc::RecvError),
                            _ => panic!("closed capture must not spin"),
                        }
                    },
                    &writer,
                    &running,
                )
            },
            &running,
            &first_stop,
            |close| {
                assert!(writer.try_lock().is_ok());
                assert!(!running.load(Ordering::Acquire));
                shutdown_stream(&writer, close, |_| {});
                stopped.set(true);
            },
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("capture worker stopped")
        );
        assert_eq!(calls.get(), 3);
        assert!(stopped.get());
        assert!(!running.load(Ordering::Acquire));
        assert_eq!(first_stop.cause(), StopCause::Clipboard);
        assert!(!lock_writer(&writer).get_mut().failed);
        let bytes = gate.0.lock().unwrap().bytes.clone();
        let mut reader = SessionReader::new(std::io::Cursor::new(bytes), [0x95; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Clipboard);
        assert_eq!(payload, b"last text");
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::SessionClose);
        let close = SessionClose::decode(&payload).unwrap();
        assert_eq!(close.code, SessionCloseCode::ClipboardUnavailable);
        assert_eq!(close.reason, "host clipboard became unavailable");
        assert!(reader.read_message().is_err(), "only one terminal record");
    }

    #[test]
    fn clipboard_capture_close_after_session_stop_is_graceful() {
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(FailingWriter, WRITE_TIMEOUT),
            [0x96; 32],
        ));
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        first_stop.record(StopCause::Graceful);
        run_clipboard_worker(
            || {
                forward_clipboard_from(
                    |_| {
                        running.store(false, Ordering::Release);
                        Err(std::sync::mpsc::RecvError)
                    },
                    &writer,
                    &running,
                )
            },
            &running,
            &first_stop,
            |_| panic!("already stopped"),
        )
        .unwrap();
        assert_eq!(first_stop.cause(), StopCause::Graceful);
        assert!(!lock_writer(&writer).get_mut().failed);
    }

    #[test]
    fn clipboard_normal_stop_delivers_text_without_fabricating_a_failure() {
        let state = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&state)), WRITE_TIMEOUT),
            [0x94; 32],
        ));
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let mut text = Some(text_record("normal clipboard"));
        run_clipboard_worker(
            || {
                forward_clipboard_from(
                    |_| {
                        if text.is_none() {
                            running.store(false, Ordering::Release);
                        }
                        Ok(text.take())
                    },
                    &writer,
                    &running,
                )
            },
            &running,
            &first_stop,
            |_| panic!("normal stop must not report a worker failure"),
        )
        .unwrap();
        assert_eq!(first_stop.cause(), StopCause::None);
        assert!(!lock_writer(&writer).get_mut().failed);
        let wire = state.0.lock().unwrap().bytes.clone();
        let mut reader = SessionReader::new(Cursor::new(wire), [0x94; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Clipboard);
        assert_eq!(payload, b"normal clipboard");
        let stopped = AtomicBool::new(false);
        forward_clipboard_from(|_| panic!("no receive after stop"), &writer, &stopped).unwrap();
    }

    fn runtime_record_writer(key: [u8; 32]) -> Mutex<SessionWriter<DeadlineWriter<DripWriter>>> {
        Mutex::new(SessionWriter::new(
            DeadlineWriter::new(
                DripWriter {
                    bytes: Vec::new(),
                    max_chunk: usize::MAX,
                    write_delay: Duration::ZERO,
                    flush_delay: Duration::ZERO,
                    flushes: 0,
                },
                WRITE_TIMEOUT,
            ),
            key,
        ))
    }

    struct InputDropAction<F: FnOnce()>(Option<F>);

    impl<F: FnOnce()> Drop for InputDropAction<F> {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }

    fn input_backend_failure() -> super::super::RemoteError {
        LocalRuntimeFailure::input(InputError::Connection(
            io::Error::other("PRIVATE /host/input/path").into(),
        ))
    }

    fn test_keymap_bytes() -> Vec<u8> {
        let mut bytes = vec![8, 32];
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&[1; 32 * super::super::x11_keymap::KEY_DIGEST_LEN]);
        bytes
    }

    #[test]
    fn runtime_input_backend_tags_exclude_validation_setup_and_transport_errors() {
        for error in [
            InputError::Connection(io::Error::other("private X11 connection").into()),
            InputError::Reply(io::Error::other("private X11 reply").into()),
        ] {
            let message = error.to_string();
            let error = LocalRuntimeFailure::input(error);
            assert_eq!(runtime_close(&error), Some(RuntimeClose::InputUnavailable));
            assert_eq!(error.to_string(), message);
            assert!(error.source().unwrap().is::<InputError>());
        }
        for error in [
            InputError::Connect(x11rb::rust_connection::ConnectError::InvalidScreen),
            InputError::ScreenUnavailable { screen: 2 },
            InputError::UnsupportedXtestVersion { major: 1, minor: 0 },
            InputError::InvalidKeycode {
                keycode: 1,
                min: 8,
                max: 255,
            },
            InputError::InvalidButton { button: 0 },
            InputError::UnmappedButton { button: 5 },
            InputError::CoordinateOutOfRange {
                axis: 'x',
                value: u16::MAX,
            },
        ] {
            let error = LocalRuntimeFailure::input(error);
            assert!(error.is::<InputError>());
            assert!(runtime_close(&error).is_none());
        }
        let unrelated: Vec<super::super::RemoteError> = vec![
            io::Error::new(io::ErrorKind::TimedOut, INPUT_FAILURE_REASON).into(),
            io::Error::new(io::ErrorKind::UnexpectedEof, KEYMAP_CHANGED_REASON).into(),
            ProtocolError::AuthenticationFailed.into(),
            ProtocolError::Io(io::ErrorKind::ConnectionReset.into()).into(),
            decode_source_input(&[]).unwrap_err(),
            invalid_data("keyboard input was not negotiated").into(),
            permission_denied("source is not granted").into(),
            invalid_data("future source ACK").into(),
            invalid_data("clipboard sharing was not negotiated").into(),
            combine_host_cleanup(
                Err(input_backend_failure()),
                Err(io::Error::other("release failed").into()),
            )
            .unwrap_err(),
            LocalRuntimeFailure::encoding(io::Error::other("wrong producer").into()),
        ];
        for error in unrelated {
            let running = AtomicBool::new(true);
            let credits = FrameCredits::new();
            let exit = finalize_input(
                Err(error),
                (),
                &running,
                &FirstStop::new(),
                &credits,
                |_| Ok(()),
            );
            assert!(exit.notice.is_none());
            assert!(exit.result.is_err());
            assert!(!running.load(Ordering::Acquire));
            assert!(credits.state.lock().unwrap().closed);
        }
    }

    #[test]
    fn runtime_keymap_tags_require_a_local_read_failure_or_confirmed_mismatch() {
        let bytes = test_keymap_bytes();
        let viewer = KeymapTable::decode(&bytes).unwrap();
        verify_current_keymap(false, None, || panic!("no read without notification")).unwrap();
        let invariant =
            verify_current_keymap(true, None, || panic!("missing verified map")).unwrap_err();
        assert!(runtime_close(&invariant).is_none());
        verify_current_keymap(true, Some(&viewer), || Ok(viewer.clone())).unwrap();
        let read = verify_current_keymap(true, Some(&viewer), || {
            Err(io::Error::other("PRIVATE /host/keymap/path").into())
        })
        .unwrap_err();
        assert_eq!(runtime_close(&read), Some(RuntimeClose::KeymapUnavailable));
        assert!(read.to_string().contains("PRIVATE"));
        for changed in [2, 34, 35] {
            let mut different = bytes.clone();
            if changed == 35 {
                different[34..].fill(0);
            } else {
                different[changed] = 2;
            }
            let host = KeymapTable::decode(&different).unwrap();
            assert!(!matches!(viewer.compare(&host), KeymapMatch::Compatible));
            let error = verify_current_keymap(true, Some(&viewer), || Ok(host)).unwrap_err();
            assert_eq!(runtime_close(&error), Some(RuntimeClose::KeymapChanged));
            assert!(error.to_string().contains("host X11 keymap changed"));
        }
    }

    #[test]
    fn runtime_input_notice_follows_release_and_drop_and_preserves_cleanup_failure() {
        for close in [
            RuntimeClose::InputUnavailable,
            RuntimeClose::KeymapUnavailable,
            RuntimeClose::KeymapChanged,
        ] {
            for release_fails in [false, true] {
                let running = AtomicBool::new(true);
                let first_stop = FirstStop::new();
                let credits = FrameCredits::new();
                let events = Mutex::new(Vec::new());
                let injector = InputDropAction(Some(|| {
                    events.lock().unwrap().push("drop completed");
                }));
                let key = [0xb1; 32];
                let writer = runtime_record_writer(key);
                let primary = Box::new(LocalRuntimeFailure {
                    close: close.clone(),
                    source: io::Error::other("PRIVATE /host/input/path").into(),
                });
                let result = finalize_input(
                    Err(primary),
                    injector,
                    &running,
                    &first_stop,
                    &credits,
                    |_| {
                        assert!(!running.load(Ordering::Acquire));
                        assert!(credits.state.lock().unwrap().closed);
                        assert_eq!(first_stop.cause(), StopCause::Input);
                        events.lock().unwrap().push("release");
                        if release_fails {
                            Err(io::Error::other("PRIVATE release failed").into())
                        } else {
                            Ok(())
                        }
                    },
                )
                .report(|notice| {
                    assert_eq!(*events.lock().unwrap(), ["release", "drop completed"]);
                    events.lock().unwrap().push("notice");
                    assert_eq!(notice, close);
                    say_session_close(&writer, notice);
                });
                let error = result.unwrap_err();
                assert_eq!(
                    *events.lock().unwrap(),
                    ["release", "drop completed", "notice"]
                );
                assert!(error.to_string().contains("PRIVATE /host/input/path"));
                if release_fails {
                    let cleanup = error.downcast_ref::<HostCleanupError>().unwrap();
                    assert_eq!(
                        runtime_close(cleanup.primary.as_ref().unwrap()),
                        Some(close.clone())
                    );
                    assert!(cleanup.cleanup.to_string().contains("release failed"));
                } else {
                    assert_eq!(runtime_close(&error), Some(close.clone()));
                }
                let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
                let slot = ViewerSlot::claim(&admission).unwrap();
                run_admitted_session(slot, || {
                    finish_host_session(StopCause::Input, Ok(()), Ok(Err(error)), Ok(()))
                })
                .unwrap_err();
                assert_eq!(ViewerSlot::claim(&admission).is_none(), release_fails);
                let wire = lock_writer(&writer).get_ref().get_ref().bytes.clone();
                let reason = match close {
                    RuntimeClose::InputUnavailable => INPUT_FAILURE_REASON,
                    RuntimeClose::KeymapUnavailable => KEYMAP_VERIFY_FAILURE_REASON,
                    RuntimeClose::KeymapChanged => KEYMAP_CHANGED_REASON,
                    _ => unreachable!(),
                };
                crate::remote::client::tests::assert_runtime_terminal_record(
                    &wire, key, None, reason,
                );
            }
        }
    }

    #[test]
    fn runtime_input_blocked_release_or_destructor_keeps_stop_observable_without_notice() {
        for block_drop in [false, true] {
            let running = Arc::new(AtomicBool::new(true));
            let credits = Arc::new(FrameCredits::new());
            let first_stop = Arc::new(FirstStop::new());
            let notices = Arc::new(AtomicU32::new(0));
            let dropped = Arc::new(AtomicBool::new(false));
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let (entered, entering) = std::sync::mpsc::channel();
            let worker_running = Arc::clone(&running);
            let worker_credits = Arc::clone(&credits);
            let worker_stop = Arc::clone(&first_stop);
            let worker_notices = Arc::clone(&notices);
            let worker_dropped = Arc::clone(&dropped);
            let worker_gate = Arc::clone(&gate);
            let worker = thread::spawn(move || {
                let block = || {
                    entered.send(()).unwrap();
                    let open = worker_gate.0.lock().unwrap();
                    let (_open, timeout) = worker_gate
                        .1
                        .wait_timeout_while(open, Duration::from_secs(5), |open| !*open)
                        .unwrap();
                    assert!(!timeout.timed_out(), "test must unblock cleanup");
                };
                let injector = InputDropAction(Some(|| {
                    if block_drop {
                        block();
                    }
                    worker_dropped.store(true, Ordering::Release);
                }));
                finalize_input(
                    Err(input_backend_failure()),
                    injector,
                    &worker_running,
                    &worker_stop,
                    &worker_credits,
                    |_| {
                        if !block_drop {
                            block();
                        }
                        Ok(())
                    },
                )
                .report(|_| {
                    assert!(worker_dropped.load(Ordering::Acquire));
                    worker_notices.fetch_add(1, Ordering::AcqRel);
                })
            });
            let blocked = entering.recv_timeout(Duration::from_secs(2));
            let stopped = !running.load(Ordering::Acquire);
            let credits_closed = credits.state.lock().unwrap().closed;
            let cause = first_stop.cause();
            let early_notices = notices.load(Ordering::Acquire);
            let early_drop = dropped.load(Ordering::Acquire);
            // Always unblock before asserting, including failed observations.
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            let result = worker.join().unwrap();
            blocked.unwrap();
            assert!(stopped && credits_closed);
            assert_eq!(cause, StopCause::Input);
            assert_eq!(early_notices, 0);
            assert!(!early_drop);
            assert!(result.is_err());
            assert!(dropped.load(Ordering::Acquire));
            assert_eq!(notices.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn runtime_input_release_or_destructor_panic_skips_notice_and_disables_admission() {
        for release_panics in [false, true] {
            let running = Arc::new(AtomicBool::new(true));
            let credits = Arc::new(FrameCredits::new());
            let notices = Arc::new(AtomicU32::new(0));
            let worker_running = Arc::clone(&running);
            let worker_credits = Arc::clone(&credits);
            let worker_notices = Arc::clone(&notices);
            let worker = thread::spawn(move || {
                let injector = InputDropAction(Some(|| {
                    // Exactly one panic per fixture: a double panic would abort.
                    assert!(release_panics, "injector destructor panicked");
                }));
                finalize_input(
                    Err(input_backend_failure()),
                    injector,
                    &worker_running,
                    &FirstStop::new(),
                    &worker_credits,
                    |_| {
                        assert!(!release_panics, "explicit release panicked");
                        Ok(())
                    },
                )
                .report(|_| {
                    worker_notices.fetch_add(1, Ordering::AcqRel);
                })
            });
            let admission = Arc::new(AtomicU8::new(VIEWER_IDLE));
            let slot = ViewerSlot::claim(&admission).unwrap();
            let error = run_admitted_session(slot, || {
                cleanup_host_session(
                    Instant::now() + Duration::from_secs(2),
                    StopCause::Input,
                    Ok(()),
                    worker,
                    |_| Ok(()),
                )
            })
            .unwrap_err();
            assert!(error.is::<HostCleanupError>());
            assert!(error.to_string().contains("input thread panicked"));
            assert!(ViewerSlot::claim(&admission).is_none());
            assert!(!running.load(Ordering::Acquire));
            assert!(credits.state.lock().unwrap().closed);
            assert_eq!(notices.load(Ordering::Acquire), 0);
        }
    }

    #[test]
    fn runtime_input_notice_ownership_comes_from_record_not_later_cause() {
        for cause in [
            StopCause::Input,
            StopCause::Graceful,
            StopCause::Capture,
            StopCause::Sender,
            StopCause::Clipboard,
            StopCause::ThreadPanic,
        ] {
            let first_stop = FirstStop::new();
            if cause == StopCause::ThreadPanic {
                first_stop.record_thread_panic();
            } else {
                assert!(first_stop.record(cause));
            }
            let exit = finalize_input(
                Err(input_backend_failure()),
                (),
                &AtomicBool::new(true),
                &first_stop,
                &FrameCredits::new(),
                |_| Ok(()),
            );
            assert!(exit.notice.is_none(), "earlier {cause:?} owns the stop");
            assert_eq!(first_stop.cause(), cause);
        }
        let first_stop = FirstStop::new();
        let exit = finalize_input(
            Err(input_backend_failure()),
            (),
            &AtomicBool::new(true),
            &first_stop,
            &FrameCredits::new(),
            |_| {
                first_stop.record_thread_panic();
                Ok(())
            },
        );
        assert_eq!(first_stop.cause(), StopCause::ThreadPanic);
        assert_eq!(exit.notice, Some(RuntimeClose::InputUnavailable));

        for earlier_graceful in [false, true] {
            let first_stop = FirstStop::new();
            if earlier_graceful {
                assert!(first_stop.record(StopCause::Graceful));
            }
            let exit = finalize_input(
                Ok(()),
                (),
                &AtomicBool::new(true),
                &first_stop,
                &FrameCredits::new(),
                |_| Err(input_backend_failure()),
            );
            assert_eq!(first_stop.cause(), StopCause::Input);
            assert!(
                exit.notice.is_none(),
                "cleanup-only failure cannot become a runtime report"
            );
            assert!(exit.result.unwrap_err().is::<HostCleanupError>());
        }
    }

    #[test]
    fn runtime_input_notice_skips_busy_or_poisoned_writer_without_retry() {
        for poison in [false, true] {
            let writer = runtime_record_writer([0xb2; 32]);
            if poison {
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _held = writer.lock().unwrap();
                    panic!("writer ownership interrupted");
                }));
                assert!(panic.is_err());
            }
            let held = (!poison).then(|| writer.lock().unwrap());
            let running = AtomicBool::new(true);
            let first_stop = FirstStop::new();
            let credits = FrameCredits::new();
            let started = Instant::now();
            finalize_input(
                Err(input_backend_failure()),
                (),
                &running,
                &first_stop,
                &credits,
                |_| Ok(()),
            )
            .report(|close| say_session_close(&writer, close))
            .unwrap_err();
            assert!(started.elapsed() < Duration::from_secs(1));
            drop(held);
            assert!(lock_writer(&writer).get_ref().get_ref().bytes.is_empty());
            finalize_input(
                Err(input_backend_failure()),
                (),
                &running,
                &first_stop,
                &credits,
                |_| Ok(()),
            )
            .report(|_| panic!("no retry after the writer becomes available"))
            .unwrap_err();
        }
    }

    #[test]
    fn local_plan_refinement_and_jpeg_failures_send_legacy_close_without_committing() {
        // Exercise the actual four tagged operations. The JPEG fixture uses
        // the real bounded writer, and each refinement loses its detection
        // only at the selected call. No large image or X server is needed.
        for (stage, had_baseline) in [(0, false), (1, true), (2, true), (3, false), (3, true)] {
            let key = [0xa1; 32];
            let writer = Arc::new(runtime_record_writer(key));
            let mailbox = Arc::new(LatestMailbox::new());
            let running = Arc::new(AtomicBool::new(true));
            let first_stop = FirstStop::new();
            let credits = FrameCredits::new();
            let telemetry = HostTelemetry::new();
            let quality = JpegQualityController::new_at(
                85,
                85,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap();
            let mut sender = FrameSender::new();
            let baseline_at = Instant::now() - Duration::from_secs(3);
            let baseline = test_captured_frame(10);
            if had_baseline {
                accept_frame(&mut sender, &baseline, baseline_at);
            }
            let mut pending = test_pending_frame(if stage == 3 { 20 } else { 10 });
            let expected = match stage {
                0 => {
                    pending.frame.source_width = 0;
                    "remote frame has an empty dimension"
                }
                1 | 2 => {
                    sender.encoder.forget_detection_on_refinement = Some(stage);
                    "tile refinement selected without detection"
                }
                3 => {
                    sender.encoder.jpeg_payload_limit = Some(0);
                    "encoded JPEG frame exceeds protocol limit of 0 bytes"
                }
                _ => unreachable!(),
            };
            mailbox.publish(pending).unwrap();
            let sender = std::cell::RefCell::new(sender);
            let mut shutdowns = Vec::new();
            let result = run_video_worker(
                || {
                    send_frames_with_encoder(
                        Arc::clone(&mailbox),
                        Arc::clone(&writer),
                        &running,
                        &credits,
                        &telemetry,
                        &quality,
                        &mut sender.borrow_mut(),
                    )
                },
                &running,
                &first_stop,
                |close| {
                    assert_eq!(close, Some(RuntimeClose::Encoding));
                    assert_eq!(first_stop.cause(), StopCause::Sender);
                    assert!(!running.load(Ordering::Acquire));
                    assert!(sender.try_borrow_mut().is_ok(), "encoder must be released");
                    assert!(writer.try_lock().is_ok(), "writer must be released");
                    mailbox.close();
                    credits.close();
                    shutdown_stream(&writer, close, |how| {
                        assert!(mailbox.state.lock().unwrap().closed);
                        assert!(credits.state.lock().unwrap().closed);
                        let written = lock_writer(&writer).get_ref().get_ref().bytes.len();
                        assert_eq!(written == 0, how == Shutdown::Read);
                        shutdowns.push(how);
                    });
                },
            );
            assert_eq!(shutdowns, [Shutdown::Read, Shutdown::Both]);
            let wire = lock_writer(&writer).get_ref().get_ref().bytes.clone();
            let mut reader = SessionReader::new(Cursor::new(&wire), key);
            let (kind, reason) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Close);
            assert_eq!(reason, ENCODING_FAILURE_REASON.as_bytes());
            assert!(
                reader.read_message().is_err(),
                "exactly one terminal record"
            );
            crate::remote::client::tests::assert_runtime_terminal_record(
                &wire,
                key,
                None,
                ENCODING_FAILURE_REASON,
            );
            let error = stream_result_for_stop(
                first_stop.cause(),
                Err(io::Error::other("secondary capture failure").into()),
                result,
                Err(io::Error::other("secondary clipboard failure").into()),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), expected);
            let source = error.source().unwrap();
            if stage == 3 {
                let image::ImageError::IoError(source) =
                    source.downcast_ref::<image::ImageError>().unwrap()
                else {
                    panic!("expected the real bounded JPEG writer failure")
                };
                assert_eq!(source.kind(), io::ErrorKind::InvalidData);
            } else {
                assert_eq!(
                    source.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::InvalidData
                );
            }
            assert_eq!(credits.outstanding(), 0);
            assert_eq!(telemetry.state.lock().unwrap().window.sent, 0);
            let mut sender = sender.into_inner();
            assert_eq!(sender.committed_at, had_baseline.then_some(baseline_at));
            assert!(sender.last_refinement.is_none());
            sender.discard();
            let next = sender.plan_at(&baseline, baseline_at).unwrap();
            assert_eq!(next.keyframe, !had_baseline);
            assert_eq!(
                next.emit, !had_baseline,
                "failed encoding must not change the baseline"
            );
            if had_baseline {
                assert!(
                    sender.encoder.has_quality_debt(85),
                    "failed repair cannot pay quality debt"
                );
            }
        }
    }

    #[test]
    fn video_transport_and_credit_errors_stay_unclassified_and_release_guards() {
        for credit_error in [false, true] {
            let key = [0xa2; 32];
            let writer = Arc::new(Mutex::new(SessionWriter::new(
                DeadlineWriter::new(FlushFailWriter::default(), WRITE_TIMEOUT),
                key,
            )));
            let mailbox = Arc::new(LatestMailbox::new());
            mailbox.publish(test_pending_frame(10)).unwrap();
            let running = Arc::new(AtomicBool::new(true));
            let first_stop = FirstStop::new();
            let credits = FrameCredits::new();
            if credit_error {
                let now = Instant::now();
                credits.mark_sent(0, now, now, 1, 70, 0).unwrap();
            }
            let sources = test_source_control();
            let telemetry = HostTelemetry::new();
            let quality = JpegQualityController::new_at(
                70,
                70,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap();
            let result = run_video_worker(
                || {
                    send_frames_scoped(
                        Arc::clone(&mailbox),
                        Arc::clone(&writer),
                        &running,
                        &credits,
                        &telemetry,
                        &quality,
                        Some(&sources),
                    )
                },
                &running,
                &first_stop,
                |close| {
                    assert!(
                        close.is_none(),
                        "transport/credit failures are not encoding failures"
                    );
                    assert!(writer.try_lock().is_ok());
                    assert!(
                        sources.state.try_lock().is_ok(),
                        "source guard must unwind too"
                    );
                    mailbox.close();
                    credits.close();
                    let before = lock_writer(&writer).get_ref().get_ref().bytes.len();
                    shutdown_stream(&writer, close, |_| {});
                    assert_eq!(lock_writer(&writer).get_ref().get_ref().bytes.len(), before);
                },
            );
            let error = result.unwrap_err();
            assert!(runtime_close(&error).is_none());
            assert!(error.to_string().contains(if credit_error {
                "remote frame sender sequence mismatch"
            } else {
                "injected flush failure"
            }));
            assert!(!running.load(Ordering::Acquire));
            assert_eq!(first_stop.cause(), StopCause::Sender);
        }
    }

    #[test]
    fn video_ack_and_source_presentation_timeouts_never_claim_encoding_failure() {
        // Both real sender waits run concurrently. Their production timeout
        // remains untouched instead of adding a test-only scheduler path.
        thread::scope(|scope| {
            for source_timeout in [false, true] {
                scope.spawn(move || {
                    let writer = Arc::new(runtime_record_writer([0xa3; 32]));
                    let mailbox = Arc::new(LatestMailbox::new());
                    mailbox.publish(test_pending_frame(10)).unwrap();
                    let running = Arc::new(AtomicBool::new(true));
                    let first_stop = FirstStop::new();
                    let credits = FrameCredits::new();
                    let sources = test_source_control();
                    if source_timeout {
                        let mut state = sources.state.lock().unwrap();
                        let version = state.policy.current();
                        state.policy.publish_keyframe(version, 0).unwrap();
                    } else {
                        let now = Instant::now();
                        for sequence in 0..MAX_OUTSTANDING_FRAMES {
                            credits.mark_sent(sequence, now, now, 1, 70, 0).unwrap();
                        }
                    }
                    let telemetry = HostTelemetry::new();
                    let quality = JpegQualityController::new_at(
                        70,
                        70,
                        false,
                        Duration::from_millis(100),
                        Instant::now(),
                    )
                    .unwrap();
                    let result = run_video_worker(
                        || {
                            send_frames_scoped(
                                Arc::clone(&mailbox),
                                Arc::clone(&writer),
                                &running,
                                &credits,
                                &telemetry,
                                &quality,
                                source_timeout.then_some(&sources),
                            )
                        },
                        &running,
                        &first_stop,
                        |close| {
                            assert!(close.is_none());
                            assert!(sources.state.try_lock().is_ok());
                            mailbox.close();
                            credits.close();
                            shutdown_stream(&writer, close, |_| {});
                        },
                    );
                    let error = result.unwrap_err();
                    assert!(runtime_close(&error).is_none());
                    assert_eq!(
                        error.downcast_ref::<io::Error>().unwrap().kind(),
                        io::ErrorKind::TimedOut
                    );
                    assert!(error.to_string().contains(if source_timeout {
                        "source keyframe was not presented"
                    } else {
                        "remote viewer stopped acknowledging displayed frames"
                    }));
                    assert!(lock_writer(&writer).get_ref().get_ref().bytes.is_empty());
                    assert_eq!(first_stop.cause(), StopCause::Sender);
                });
            }
        });
    }

    #[test]
    fn video_encoder_report_requires_first_cause_ownership_and_never_reports_panic() {
        for prior in [
            StopCause::Input,
            StopCause::Capture,
            StopCause::Clipboard,
            StopCause::Graceful,
            StopCause::Sender,
        ] {
            let running = Arc::new(AtomicBool::new(true));
            let first_stop = FirstStop::new();
            first_stop.record(prior);
            let result = run_video_worker(
                || {
                    Err(LocalRuntimeFailure::encoding(
                        io::Error::other("private encoder failure").into(),
                    ))
                },
                &running,
                &first_stop,
                |close| {
                    assert!(
                        close.is_none(),
                        "only the successful first-stop owner may notify"
                    )
                },
            );
            assert_eq!(result.unwrap_err().to_string(), "private encoder failure");
            assert_eq!(first_stop.cause(), prior);
            assert!(!running.load(Ordering::Acquire));
        }
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_video_worker(
                || panic!("injected encoder panic"),
                &running,
                &first_stop,
                |_| panic!("panic cannot fabricate a terminal encoding report"),
            )
        }));
        assert!(panic.is_err());
        assert_eq!(first_stop.cause(), StopCause::None);
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn runtime_capture_backend_failure_is_terminal_without_exposing_local_error() {
        let key = [0x98; 32];
        let writer = runtime_record_writer(key);
        let first_stop = FirstStop::new();
        let original = io::Error::new(io::ErrorKind::NotFound, "private display detail");
        let result = Err(LocalRuntimeFailure::capture(original.into()));
        let close = capture_stop_notice(&result, false, &first_stop);
        let mut shutdowns = Vec::new();
        shutdown_stream(&writer, close, |how| {
            let written = lock_writer(&writer).get_ref().get_ref().bytes.len();
            if how == Shutdown::Read {
                assert_eq!(written, 0, "wake input before outgoing close");
            } else {
                assert!(written > 0, "finish close before disabling writes");
            }
            shutdowns.push(how);
        });
        assert_eq!(shutdowns, [Shutdown::Read, Shutdown::Both]);
        let wire = lock_writer(&writer).get_ref().get_ref().bytes.clone();
        crate::remote::client::tests::assert_runtime_terminal_record(
            &wire,
            key,
            SessionCloseCode::CaptureUnavailable,
            "host capture became unavailable",
        );
        assert_eq!(first_stop.cause(), StopCause::Capture);
        assert!(capture_stop_notice(&result, false, &first_stop).is_none());
        let error = stream_result_for_stop(first_stop.cause(), result, Ok(()), Ok(())).unwrap_err();
        assert_eq!(error.to_string(), "private display detail");
        let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn runtime_capture_transport_and_secondary_errors_never_claim_backend_loss() {
        let result =
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "cursor transport failed").into());
        let first_stop = FirstStop::new();
        assert!(capture_stop_notice(&result, false, &first_stop).is_none());
        assert_eq!(first_stop.cause(), StopCause::Capture);
        let backend = Err(LocalRuntimeFailure::capture(
            io::Error::other("capture failed").into(),
        ));
        for prior in [
            StopCause::Capture,
            StopCause::Clipboard,
            StopCause::Input,
            StopCause::Sender,
            StopCause::Graceful,
        ] {
            let first_stop = FirstStop::new();
            first_stop.record(prior);
            assert!(capture_stop_notice(&backend, false, &first_stop).is_none());
            assert_eq!(first_stop.cause(), prior);
        }
    }

    #[test]
    fn runtime_clipboard_socket_clone_failure_remains_unclassified() {
        let running = AtomicBool::new(true);
        let first_stop = FirstStop::new();
        let (_, result) = admit_clipboard_worker::<()>(
            Some(Err(io::Error::other("socket clone failed").into())),
            &running,
            &first_stop,
            |close| assert!(close.is_none()),
        );
        assert_eq!(result.unwrap_err().to_string(), "socket clone failed");
        assert!(!running.load(Ordering::Acquire));
    }

    #[test]
    fn runtime_clipboard_outbox_loss_reaches_client_as_terminal() {
        let key = [0x99; 32];
        let writer = runtime_record_writer(key);
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let result = run_clipboard_worker(
            || forward_clipboard_from(|_| Err(std::sync::mpsc::RecvError), &writer, &running),
            &running,
            &first_stop,
            |close| {
                assert!(!running.load(Ordering::Acquire));
                shutdown_stream(&writer, close, |_| {});
            },
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "local clipboard capture worker stopped"
        );
        let wire = lock_writer(&writer).get_ref().get_ref().bytes.clone();
        crate::remote::client::tests::assert_runtime_terminal_record(
            &wire,
            key,
            SessionCloseCode::ClipboardUnavailable,
            "host clipboard became unavailable",
        );
        assert!(capture_stop_notice(&Ok(()), false, &first_stop).is_none());
    }

    #[test]
    fn runtime_close_skips_contended_or_poisoned_writer_without_retry() {
        for poison in [false, true] {
            let writer = runtime_record_writer([0x9a; 32]);
            if poison {
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _held = writer.lock().unwrap();
                    panic!("writer ownership interrupted");
                }));
                assert!(panic.is_err());
            }
            let held = (!poison).then(|| writer.lock().unwrap());
            let result = Err(LocalRuntimeFailure::capture(
                io::Error::other("primary capture failure").into(),
            ));
            let first_stop = FirstStop::new();
            let started = Instant::now();
            let mut shutdowns = Vec::new();
            shutdown_stream(
                &writer,
                capture_stop_notice(&result, false, &first_stop),
                |how| shutdowns.push(how),
            );
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(shutdowns, [Shutdown::Read, Shutdown::Both]);
            drop(held);
            assert!(lock_writer(&writer).get_ref().get_ref().bytes.is_empty());
            assert!(capture_stop_notice(&result, false, &first_stop).is_none());
            assert_eq!(result.unwrap_err().to_string(), "primary capture failure");
        }
    }

    #[test]
    fn runtime_close_read_shutdown_wakes_tcp_reader_before_a_blocked_write() {
        use std::io::Read;
        for close in [
            SessionClose::new(
                SessionCloseCode::CaptureUnavailable,
                "host capture became unavailable",
            )
            .into(),
            RuntimeClose::Encoding,
        ] {
            let (_peer, control) = loopback_pair();
            let mut input = control.try_clone().unwrap();
            input
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (released_tx, released_rx) = std::sync::mpsc::channel();
            let input_thread = thread::spawn(move || {
                started_tx.send(()).unwrap();
                let result = input.read(&mut [0_u8; 1]);
                released_tx.send(result).unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let gate = Arc::new((Mutex::new(GateState::default()), Condvar::new()));
            let writer = Mutex::new(SessionWriter::new(
                DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
                [0x9b; 32],
            ));
            let reporter = thread::spawn(move || {
                shutdown_stream(&writer, Some(close), |how| control.shutdown(how).unwrap());
            });
            let (lock, ready) = &*gate;
            let state = lock.lock().unwrap();
            let (mut state, _) = ready
                .wait_timeout_while(state, Duration::from_secs(2), |state| {
                    state.write_calls == 0
                })
                .unwrap();
            let writing = state.write_calls > 0;
            drop(state);
            // Sample while the outgoing write is still blocked, then always open
            // the gate before asserting so a failed test cannot strand the worker.
            let released = released_rx.recv_timeout(Duration::from_secs(1));
            state = lock.lock().unwrap();
            state.open = true;
            ready.notify_all();
            drop(state);
            reporter.join().unwrap();
            input_thread.join().unwrap();
            assert!(writing);
            assert_eq!(released.unwrap().unwrap(), 0);
        }
    }

    #[test]
    fn runtime_close_survives_tcp_read_shutdown_and_arrives_before_eof() {
        let (peer, control) = loopback_pair();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let key = [0x9c; 32];
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(control.try_clone().unwrap(), WRITE_TIMEOUT),
            key,
        ));
        let result = Err(LocalRuntimeFailure::capture(
            io::Error::other("private backend failure").into(),
        ));
        shutdown_stream(
            &writer,
            capture_stop_notice(&result, false, &FirstStop::new()),
            |how| control.shutdown(how).unwrap(),
        );
        let mut reader = SessionReader::new(peer, key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::SessionClose);
        assert_eq!(
            SessionClose::decode(&payload).unwrap().code,
            SessionCloseCode::CaptureUnavailable
        );
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn legacy_encoding_close_survives_read_shutdown_and_arrives_before_eof() {
        let (peer, control) = loopback_pair();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let key = [0xa4; 32];
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(control.try_clone().unwrap(), WRITE_TIMEOUT),
            key,
        ));
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        run_video_worker(
            || {
                Err(LocalRuntimeFailure::encoding(
                    io::Error::other("private encoder detail").into(),
                ))
            },
            &running,
            &first_stop,
            |close| shutdown_stream(&writer, close, |how| control.shutdown(how).unwrap()),
        )
        .unwrap_err();
        let mut reader = SessionReader::new(peer, key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Close);
        assert_eq!(payload, ENCODING_FAILURE_REASON.as_bytes());
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn legacy_encoding_close_skips_contended_or_poisoned_writer_without_retry() {
        for poison in [false, true] {
            let writer = runtime_record_writer([0xa5; 32]);
            if poison {
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _held = writer.lock().unwrap();
                    panic!("writer ownership interrupted");
                }));
                assert!(panic.is_err());
            }
            let held = (!poison).then(|| writer.lock().unwrap());
            let running = Arc::new(AtomicBool::new(true));
            let first_stop = FirstStop::new();
            let started = Instant::now();
            let mut shutdowns = Vec::new();
            let result = run_video_worker(
                || {
                    Err(LocalRuntimeFailure::encoding(
                        io::Error::other("private encoder detail").into(),
                    ))
                },
                &running,
                &first_stop,
                |close| shutdown_stream(&writer, close, |how| shutdowns.push(how)),
            );
            assert_eq!(result.unwrap_err().to_string(), "private encoder detail");
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(shutdowns, [Shutdown::Read, Shutdown::Both]);
            drop(held);
            assert!(lock_writer(&writer).get_ref().get_ref().bytes.is_empty());
            run_video_worker(
                || {
                    Err(LocalRuntimeFailure::encoding(
                        io::Error::other("secondary encoder detail").into(),
                    ))
                },
                &running,
                &first_stop,
                |close| {
                    assert!(
                        close.is_none(),
                        "no retry after the writer becomes available"
                    );
                    shutdown_stream(&writer, close, |_| {});
                },
            )
            .unwrap_err();
            assert!(lock_writer(&writer).get_ref().get_ref().bytes.is_empty());
        }
    }

    #[test]
    fn a_stopping_host_says_goodbye_unless_the_writer_is_busy() {
        let state = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..GateState::default()
            }),
            Condvar::new(),
        ));
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&state)), WRITE_TIMEOUT),
            [0x92; 32],
        ));
        say_session_close(
            &writer,
            SessionClose::new(SessionCloseCode::HostStopping, HOST_STOPPING_REASON),
        );
        let wire = state.0.lock().unwrap().bytes.clone();
        let mut reader = SessionReader::new(Cursor::new(wire), [0x92; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::SessionClose);
        let close = SessionClose::decode(&payload).unwrap();
        assert_eq!(close.code, SessionCloseCode::HostStopping);
        assert_eq!(close.reason, HOST_STOPPING_REASON);

        // A sender mid-record keeps the writer; the goodbye is skipped
        // rather than queued behind a possibly stuck write.
        let written = state.0.lock().unwrap().bytes.len();
        let held = writer.lock().unwrap();
        say_session_close(
            &writer,
            SessionClose::new(SessionCloseCode::HostStopping, HOST_STOPPING_REASON),
        );
        drop(held);
        assert_eq!(state.0.lock().unwrap().bytes.len(), written);
    }

    #[test]
    fn structured_refusals_preserve_exact_producer_codes_and_reasons() {
        for (close, code, reason) in [
            (
                admission_refusal(VIEWER_ACTIVE),
                SessionCloseCode::Busy,
                "another viewer is already connected to this host",
            ),
            (
                admission_refusal(VIEWER_POISONED),
                SessionCloseCode::AdmissionDisabled,
                "host cleanup is incomplete; restart the host before reconnecting",
            ),
            (
                capture_refusal("backend unavailable"),
                SessionCloseCode::CaptureUnavailable,
                "host cannot capture its display: backend unavailable",
            ),
            (
                clipboard_refusal(),
                SessionCloseCode::ClipboardUnavailable,
                "host clipboard startup failed; restart the host before reconnecting",
            ),
        ] {
            let mut writer = SessionWriter::new(Vec::new(), [0x93; 32]);
            decline_session(&mut writer, close);
            let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), [0x93; 32]);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::SessionClose);
            let decoded = SessionClose::decode(&payload).unwrap();
            assert_eq!(decoded.code, code);
            assert_eq!(decoded.reason, reason);
        }
    }

    #[test]
    fn terminal_notice_deadline_never_retries_partial_write_or_expired_flush() {
        for close in [
            SessionClose::new(SessionCloseCode::HostStopping, "host became unavailable").into(),
            SessionClose::new(
                SessionCloseCode::CaptureUnavailable,
                "host became unavailable",
            )
            .into(),
            SessionClose::new(
                SessionCloseCode::ClipboardUnavailable,
                "host became unavailable",
            )
            .into(),
            RuntimeClose::Encoding,
            RuntimeClose::InputUnavailable,
            RuntimeClose::KeymapUnavailable,
            RuntimeClose::KeymapChanged,
        ] {
            for slow_flush in [false, true] {
                let writer = Mutex::new(SessionWriter::new(
                    DeadlineWriter::new(
                        DripWriter {
                            bytes: Vec::new(),
                            max_chunk: if slow_flush { usize::MAX } else { 1 },
                            write_delay: if slow_flush {
                                Duration::ZERO
                            } else {
                                Duration::from_millis(8)
                            },
                            flush_delay: if slow_flush {
                                GOODBYE_WRITE_DEADLINE + Duration::from_millis(10)
                            } else {
                                Duration::ZERO
                            },
                            flushes: 0,
                        },
                        WRITE_TIMEOUT,
                    ),
                    [0x92; 32],
                ));
                let started = Instant::now();
                say_session_close(&writer, close.clone());
                assert!(started.elapsed() < Duration::from_secs(1));
                let before = {
                    let writer = writer.lock().unwrap();
                    assert!(writer.get_ref().failed);
                    // The record sequence advances after complete bytes, not
                    // flush. Either failure still poisons all later writes.
                    assert_eq!(writer.next_sequence(), u64::from(slow_flush));
                    let inner = writer.get_ref().get_ref();
                    (inner.bytes.len(), inner.flushes)
                };
                assert!(before.0 > 0);
                assert_eq!(before.1, usize::from(slow_flush));
                say_session_close(&writer, close.clone());
                let writer = writer.lock().unwrap();
                let inner = writer.get_ref().get_ref();
                assert_eq!((inner.bytes.len(), inner.flushes), before);
            }
        }
    }

    #[test]
    fn runtime_concurrent_failures_allow_only_the_first_cause_to_notify() {
        for _ in 0..16 {
            let running = Arc::new(AtomicBool::new(true));
            let first_stop = Arc::new(FirstStop::new());
            let barrier = Arc::new(std::sync::Barrier::new(5));
            let notices = Mutex::new(Vec::new());
            thread::scope(|scope| {
                scope.spawn(|| {
                    barrier.wait();
                    finalize_input(
                        Err(input_backend_failure()),
                        (),
                        &running,
                        &first_stop,
                        &FrameCredits::new(),
                        |_| Ok(()),
                    )
                    .report(|close| notices.lock().unwrap().push(close))
                    .unwrap_err();
                });
                scope.spawn(|| {
                    barrier.wait();
                    let result = Err(LocalRuntimeFailure::capture(
                        io::Error::other("capture primary").into(),
                    ));
                    if let Some(close) = capture_stop_notice(&result, false, &first_stop) {
                        notices.lock().unwrap().push(close);
                    }
                    running.store(false, Ordering::Release);
                });
                scope.spawn(|| {
                    barrier.wait();
                    let result = run_clipboard_worker(
                        || {
                            Err(LocalRuntimeFailure::clipboard(
                                io::Error::other("clipboard primary").into(),
                            ))
                        },
                        &running,
                        &first_stop,
                        |close| {
                            if let Some(close) = close {
                                notices.lock().unwrap().push(close);
                            }
                        },
                    );
                    assert!(result.is_err());
                });
                scope.spawn(|| {
                    barrier.wait();
                    let result = run_video_worker(
                        || {
                            Err(LocalRuntimeFailure::encoding(
                                io::Error::other("sender primary").into(),
                            ))
                        },
                        &running,
                        &first_stop,
                        |close| {
                            if let Some(close) = close {
                                notices.lock().unwrap().push(close);
                            }
                        },
                    );
                    assert!(result.is_err());
                });
                barrier.wait();
            });
            assert!(!running.load(Ordering::Acquire));
            let notices = notices.into_inner().unwrap();
            assert!(notices.len() <= 1);
            if let Some(code) = notices.first() {
                assert_eq!(
                    *code,
                    match first_stop.cause() {
                        StopCause::Capture => SessionClose::new(
                            SessionCloseCode::CaptureUnavailable,
                            "host capture became unavailable",
                        )
                        .into(),
                        StopCause::Clipboard => SessionClose::new(
                            SessionCloseCode::ClipboardUnavailable,
                            "host clipboard became unavailable",
                        )
                        .into(),
                        StopCause::Sender => RuntimeClose::Encoding,
                        StopCause::Input => RuntimeClose::InputUnavailable,
                        other => panic!("unclassified first cause {other:?} cannot notify"),
                    }
                );
            }
            let stream_result = stream_result_for_stop(
                first_stop.cause(),
                Err(io::Error::other("capture primary").into()),
                Err(io::Error::other("sender primary").into()),
                Err(io::Error::other("clipboard primary").into()),
            );
            let selected = finish_host_session(
                first_stop.cause(),
                stream_result,
                Ok(Err(io::Error::other("input primary").into())),
                Ok(()),
            )
            .unwrap_err();
            assert_eq!(
                selected.to_string(),
                match first_stop.cause() {
                    StopCause::Capture => "capture primary",
                    StopCause::Clipboard => "clipboard primary",
                    StopCause::Sender => "sender primary",
                    StopCause::Input => "input primary",
                    other => panic!("unexpected cause {other:?}"),
                }
            );
        }
    }

    #[test]
    fn host_rejects_structured_close_in_the_client_direction() {
        let (client, server) = loopback_pair();
        let key = [0x48; 32];
        let mut writer = SessionWriter::new(client, key);
        writer
            .write_message(
                MessageKind::SessionClose,
                &SessionClose::new(SessionCloseCode::HostStopping, "stopping").encode(),
            )
            .unwrap();
        writer.flush().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = Arc::new(FirstStop::new());
        let credits = Arc::new(FrameCredits::new());
        let telemetry = Arc::new(HostTelemetry::new());
        let quality = Arc::new(
            JpegQualityController::new_at(
                70,
                70,
                false,
                Duration::from_millis(100),
                Instant::now(),
            )
            .unwrap(),
        );
        let error = receive_input_scoped(
            SessionReader::new(server, key),
            None,
            (0, 0).into(),
            false,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            &running,
            &first_stop,
            &credits,
            &telemetry,
            &quality,
            &ViewportRequest::new(),
            None,
            None,
        )
        .report(|_| panic!("invalid client records cannot report backend loss"))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unexpected client message: SessionClose")
        );
        assert!(!running.load(Ordering::Acquire));
        assert_eq!(first_stop.cause(), StopCause::Input);
    }

    #[test]
    fn a_setup_failure_reaches_the_viewer_as_a_bounded_close_reason() {
        let mut writer = SessionWriter::new(Vec::new(), [0x93; 32]);
        // Multi-byte text straddling the cap must not be cut mid-character.
        let reason = format!("host cannot capture its display: {}", "显示".repeat(200));
        decline_legacy_session(&mut writer, &reason);
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), [0x93; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Close);
        assert!(payload.len() <= MAX_SETUP_REASON_BYTES);
        let shown = std::str::from_utf8(&payload).unwrap();
        assert!(shown.starts_with("host cannot capture its display: 显示"));
    }

    #[test]
    fn clipboard_panic_publishes_its_cause_and_wakes_before_unwind_escapes() {
        let running = Arc::new(AtomicBool::new(true));
        let first_stop = FirstStop::new();
        let stopped = std::cell::Cell::new(false);
        let mailbox = LatestMailbox::<PendingFrame>::new();
        let credits = FrameCredits::new();
        let joined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_clipboard_worker(
                || panic!("injected clipboard worker panic"),
                &running,
                &first_stop,
                |close| {
                    assert!(close.is_none(), "panic ownership is unknown");
                    assert_eq!(first_stop.cause(), StopCause::Clipboard);
                    assert!(!running.load(Ordering::Acquire));
                    first_stop.record(StopCause::Graceful);
                    assert_eq!(first_stop.cause(), StopCause::Clipboard);
                    mailbox.close();
                    credits.close();
                    stopped.set(true);
                },
            )
        }));
        assert!(joined.is_err());
        assert!(stopped.get());
        assert!(mailbox.receive().unwrap().is_none());
        assert!(
            credits
                .wait_for_credit(&running, Duration::ZERO)
                .unwrap()
                .is_none()
        );
        let error = finish_clipboard_worker(Ok(()), Some(joined), &first_stop);
        assert!(stream_result_for_stop(first_stop.cause(), Ok(()), Ok(()), error).is_err());
    }

    #[test]
    fn clipboard_failure_cause_precedes_stopped_state_publication() {
        let body = include_str!("host.rs")
            .split_once("fn stop_clipboard_session(")
            .unwrap()
            .1
            .split_once("fn run_clipboard_worker(")
            .unwrap()
            .0;
        assert!(
            body.find("first_stop.record(StopCause::Clipboard)")
                .unwrap()
                < body.find("running.swap(false").unwrap()
        );
    }

    #[test]
    fn clipboard_result_selection_preserves_first_failure_and_graceful_stop() {
        for (cause, expected) in [
            (StopCause::Capture, Some("capture")),
            (StopCause::Sender, Some("sender")),
            (StopCause::Clipboard, Some("clipboard")),
            (StopCause::Input, None),
            (StopCause::Graceful, None),
        ] {
            let result = stream_result_for_stop(
                cause,
                Err(io::Error::other("capture").into()),
                Err(io::Error::other("sender").into()),
                Err(io::Error::other("clipboard").into()),
            );
            match expected {
                Some(expected) => assert_eq!(result.unwrap_err().to_string(), expected),
                None => assert!(result.is_ok()),
            }
        }
        assert!(
            stream_result_for_stop(
                StopCause::None,
                Ok(()),
                Ok(()),
                Err(io::Error::other("clipboard").into())
            )
            .is_err()
        );
    }

    #[test]
    fn frame_sender_scroll_copy_opt_in_survives_source_resets() {
        for enabled in [false, true] {
            let key = [0x64; 32];
            let mut writer = SessionWriter::new(
                DeadlineWriter::new(
                    DripWriter {
                        bytes: Vec::new(),
                        max_chunk: usize::MAX,
                        write_delay: Duration::ZERO,
                        flush_delay: Duration::ZERO,
                        flushes: 0,
                    },
                    WRITE_TIMEOUT,
                ),
                key,
            );
            let mut sender = FrameSender::with_scroll_copy(enabled);
            let now = Instant::now();
            let mut sequence = 1;
            // Source changes reset reference and force a keyframe while
            // preserving immutable copy permission. Native proofs do not reset.
            for reset in 0..3 {
                if reset != 0 {
                    sender = FrameSender::with_codecs(sender.scroll_copy, sender.static_lossless);
                }
                for (step, offset) in [14u32, 21, 14].into_iter().enumerate() {
                    let frame = CapturedFrame {
                        geometry_epoch: 1,
                        source_width: 96,
                        source_height: 80,
                        image: RgbImage::from_fn(96, 80, |x, y| {
                            let y = y + offset;
                            Rgb([
                                (x * 17 + y * 29) as u8,
                                (y * 53 + x * 3) as u8,
                                (x ^ (y * 11)) as u8,
                            ])
                        }),
                    };
                    let plan = sender.plan_at(&frame, now).unwrap();
                    assert!(plan.emit);
                    assert_eq!(plan.keyframe, step == 0);
                    let mut payload = Vec::new();
                    sender
                        .encoder
                        .encode_into(&mut payload, sequence, &frame, 85)
                        .unwrap();
                    write_frame_record(&mut writer, &payload, WRITE_TIMEOUT).unwrap();
                    sender.commit_at(&frame, now);
                    sequence += 1;
                }
            }
            assert_eq!(writer.get_ref().get_ref().flushes, 9);
            let bytes = &writer.get_ref().get_ref().bytes;
            let mut reader = SessionReader::new(Cursor::new(bytes), key);
            let mut decoder = TileDecoder::new();
            decoder.set_scroll_copy_enabled(enabled);
            for index in 0..9 {
                let (kind, payload) = reader.read_message().unwrap();
                assert_eq!(kind, MessageKind::Frame);
                let keyframe = index % 3 == 0;
                assert_eq!(payload[17] & 1 != 0, keyframe);
                assert_eq!(payload[17] & 2 != 0, enabled && !keyframe);
                if enabled && !keyframe {
                    assert_eq!(
                        u64::from_be_bytes(payload[32..40].try_into().unwrap()),
                        index
                    );
                    assert_eq!(
                        i16::from_be_bytes(payload[40..42].try_into().unwrap()),
                        if index % 3 == 1 { 7 } else { -7 }
                    );
                }
                let decoded = decoder
                    .decode_into(&payload, new_decode_buffer_pool())
                    .unwrap();
                assert_eq!(decoded.sequence(), index + 1);
                assert_eq!(decoded.dimensions(), (96, 80));
            }
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
    fn cursor_sender_caches_shapes_and_orders_positions_without_video_credit() {
        let key = [0x5d; 32];
        let mut writer = SessionWriter::new(
            DeadlineWriter::new(
                DripWriter {
                    bytes: Vec::new(),
                    max_chunk: usize::MAX,
                    write_delay: Duration::ZERO,
                    flush_delay: Duration::ZERO,
                    flushes: 0,
                },
                WRITE_TIMEOUT,
            ),
            key,
        );
        let mut sender = CursorSender::default();
        for x in [10, 20] {
            sender
                .send(
                    &mut writer,
                    super::super::x11_capture::CursorSample {
                        geometry_epoch: 2,
                        source_width: 640,
                        source_height: 480,
                        shape_id: 1,
                        shape: Some(super::super::cursor::CursorShape {
                            id: 1,
                            width: 1,
                            height: 1,
                            hotspot_x: 0,
                            hotspot_y: 0,
                            rgba: vec![255; 4],
                        }),
                        x,
                        y: 10,
                        visible: true,
                    },
                )
                .unwrap();
        }
        let mut reader =
            SessionReader::new(Cursor::new(writer.get_ref().get_ref().bytes.clone()), key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::CursorShape);
        assert_eq!(
            super::super::cursor::CursorShape::decode(&payload)
                .unwrap()
                .id,
            1
        );
        for (sequence, x) in [(0, 10), (1, 20)] {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::CursorPosition);
            let p = super::super::cursor::CursorPosition::decode(&payload).unwrap();
            assert_eq!((p.sequence, p.x, p.geometry_epoch), (sequence, x, 2));
        }
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn initial_viewport_is_applied_before_ack_and_capture_worker_start() {
        // Protect orchestration order without requiring a live X server: the
        // hello must narrow the scaler synchronously, not race the input thread.
        let source = include_str!("host.rs");
        let body = source
            .split("fn serve_authenticated(")
            .nth(1)
            .unwrap()
            .split("fn stream_frames(")
            .next()
            .unwrap();
        let initial = body
            .find("if let Some(requested) = hello.initial_viewport")
            .unwrap();
        let bounds = body
            .find("capture.set_encoded_bounds(width, height)")
            .unwrap();
        let ack = body.find("MessageKind::HelloAck").unwrap();
        let stream = body.find("let stream_result = stream_frames(").unwrap();
        let limits = body.find("capture.set_frame_limits(frame_limits)").unwrap();
        assert!(limits < initial && initial < bounds && bounds < ack && ack < stream);
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
    fn native_fit_respects_viewer_source_rotation_and_safety_limits() {
        use super::super::frame::validate_dimensions;
        use super::super::x11_capture::scaled_dimensions;
        for (viewport, expected) in [
            ((1590, 1344), (1590, 993)),
            ((2560, 1600), (2560, 1600)),
            ((1600, 2560), (1600, 1000)),
            ((u16::MAX, u16::MAX), (2560, 1600)),
            ((0, 0), (320, 200)),
        ] {
            let bounds = effective_encoded_bounds(0, viewport);
            let encoded = scaled_dimensions(2560, 1600, bounds.0, bounds.1);
            assert_eq!(encoded, expected);
            assert!(validate_dimensions(encoded.0, encoded.1).is_ok());
        }
        let bounds = effective_encoded_bounds(1280, (2560, 1600));
        assert_eq!(
            scaled_dimensions(2560, 1600, bounds.0, bounds.1),
            (1280, 800)
        );
        let bounds = effective_encoded_bounds(0, (1600, 900));
        assert_eq!(
            scaled_dimensions(1600, 2560, bounds.0, bounds.1),
            (562, 900)
        );
        assert!(validate_dimensions(u16::MAX, 1).is_err());
        assert!(validate_dimensions(16384, 16384).is_err());
    }

    #[test]
    fn pointer_duplicate_initial_viewport_preserves_gate_but_changes_latch() {
        let viewport = ViewportRequest::new();
        viewport.seed(Some((800, 600)));
        assert!(!viewport.request(800, 600));
        assert_eq!(viewport.take(), Some((800, 600)));
        assert!(!viewport.request(800, 600), "take must not erase history");
        let mut revoked = viewport.request(640, 480);
        assert!(revoked);
        revoked |= viewport.request(800, 600);
        revoked |= viewport.request(800, 600);
        assert!(revoked, "restore or duplicate must not reauthorize");
        let unknown = ViewportRequest::new();
        assert!(
            unknown.request(800, 600),
            "no initial bounds is not proof of equivalence"
        );
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
            geometry_epoch: 0,
            image: RgbImage::from_pixel(2, 2, Rgb([red, 0, 0])),
            source_width: 2,
            source_height: 2,
        }
    }

    fn test_pending_frame(red: u8) -> PendingFrame {
        PendingFrame {
            pointer_serial: None,
            source_version: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            frame: test_captured_frame(red),
            captured_at: Instant::now(),
            capture_completed_at: Instant::now(),
            recycler: None,
        }
    }

    #[test]
    fn pending_frame_returns_storage_on_drop_and_mailbox_replacement() {
        let recycler = CaptureImageRecycler::default();
        let recycled_frame = || {
            let mut pending = test_pending_frame(3);
            pending.recycler = Some(recycler.clone());
            pending
        };
        let mailbox = LatestMailbox::new();
        assert_eq!(mailbox.publish(recycled_frame()), Some(false));
        assert_eq!(recycler.cached_images(), 0);
        assert_eq!(mailbox.publish(recycled_frame()), Some(true));
        assert_eq!(recycler.cached_images(), 1);
        drop(mailbox.receive().unwrap().unwrap());
        assert_eq!(recycler.cached_images(), 2);
        // Closing/rejecting a late publication also drops its owned frame.
        mailbox.close();
        assert_eq!(mailbox.publish(recycled_frame()), None);
        assert_eq!(recycler.cached_images(), 2, "pool remains bounded");
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connector = thread::spawn(move || TcpStream::connect(address).unwrap());
        let (accepted, _) = listener.accept().unwrap();
        (connector.join().unwrap(), accepted)
    }

    mod pending_handshake_tests {
        use super::*;
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        #[test]
        fn cap_rejects_without_mutation_and_drop_allows_readmission() {
            let pending = Arc::new(AtomicUsize::new(0));
            let mut permits: Vec<_> = (0..MAX_PENDING_HANDSHAKES)
                .map(|_| PendingHandshake::admit(&pending).unwrap())
                .collect();
            for _ in 0..32 {
                assert!(PendingHandshake::admit(&pending).is_none());
                assert_eq!(pending.load(Ordering::Acquire), MAX_PENDING_HANDSHAKES);
            }
            drop(permits.pop());
            let replacement = PendingHandshake::admit(&pending).unwrap();
            assert!(PendingHandshake::admit(&pending).is_none());
            drop(permits);
            assert_eq!(pending.load(Ordering::Acquire), 1);
            drop(replacement);
            assert_eq!(pending.load(Ordering::Acquire), 0);
        }

        #[test]
        fn concurrent_contenders_never_exceed_cap_or_leak_permits() {
            const CONTENDERS: usize = MAX_PENDING_HANDSHAKES * 4;
            let pending = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(CONTENDERS));
            thread::scope(|scope| {
                for _ in 0..CONTENDERS {
                    let pending = &pending;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut counts_correct = true;
                        for _ in 0..64 {
                            barrier.wait();
                            let permit = PendingHandshake::admit(pending);
                            // Hold every successful permit until all contenders
                            // have attempted admission, then inspect saturation.
                            barrier.wait();
                            counts_correct &=
                                pending.load(Ordering::Acquire) == MAX_PENDING_HANDSHAKES;
                            barrier.wait();
                            drop(permit);
                            barrier.wait();
                            counts_correct &= pending.load(Ordering::Acquire) == 0;
                            barrier.wait();
                        }
                        // Assert only after the final barrier so a regression
                        // cannot strand the other contenders at a rendezvous.
                        assert!(
                            counts_correct,
                            "admission exceeded the cap or leaked a permit"
                        );
                    });
                }
            });
            assert_eq!(pending.load(Ordering::Acquire), 0);
        }

        #[test]
        fn disconnected_handshake_releases_permit_and_allows_readmission() {
            let pending = Arc::new(AtomicUsize::new(0));
            let held: Vec<_> = (0..MAX_PENDING_HANDSHAKES - 1)
                .map(|_| PendingHandshake::admit(&pending).unwrap())
                .collect();
            for _ in 0..16 {
                let admission = PendingHandshake::admit(&pending).unwrap();
                let (peer, stream) = loopback_pair();
                drop(peer);
                thread::spawn(move || {
                    let authenticated = authenticate_peer(stream, &[0x5a; 32]);
                    drop(admission);
                    assert!(authenticated.is_err());
                })
                .join()
                .unwrap();
                assert_eq!(pending.load(Ordering::Acquire), MAX_PENDING_HANDSHAKES - 1);
            }
            drop(held);
            assert_eq!(pending.load(Ordering::Acquire), 0);
        }

        #[test]
        fn unwind_and_unstarted_worker_release_their_permits() {
            let pending = Arc::new(AtomicUsize::new(0));
            let admission = PendingHandshake::admit(&pending).unwrap();
            // A failed spawn drops its captured closure without running it.
            let unstarted_worker = move || drop(admission);
            drop(unstarted_worker);
            assert_eq!(pending.load(Ordering::Acquire), 0);
            let result = std::panic::catch_unwind(|| {
                let _admission = PendingHandshake::admit(&pending).unwrap();
                panic!("handshake worker failed");
            });
            assert!(result.is_err());
            assert_eq!(pending.load(Ordering::Acquire), 0);
            assert!(PendingHandshake::admit(&pending).is_some());
            assert_eq!(pending.load(Ordering::Acquire), 0);
        }
    }

    mod accept_loop {
        use super::*;
        use crate::remote::protocol::{NONCE_LEN, PSK_LEN, client_handshake};
        use std::io::Read;
        use std::net::SocketAddr;

        const KEY: [u8; PSK_LEN] = [0x5a; PSK_LEN];

        fn spawn_loop<S>(
            once: bool,
            serve: S,
        ) -> (
            SocketAddr,
            Arc<AtomicBool>,
            thread::JoinHandle<RemoteResult<()>>,
        )
        where
            S: Fn(AuthenticatedPeer) -> RemoteResult<()> + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let loop_shutdown = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                accept_connections(
                    &listener,
                    Arc::new(KEY.to_vec()),
                    &loop_shutdown,
                    once,
                    serve,
                )
            });
            (address, shutdown, handle)
        }

        fn acknowledge(mut peer: AuthenticatedPeer) -> RemoteResult<()> {
            let hello = ServerHello {
                stream: Default::default(),
                pointer_enabled: false,
                keyboard_enabled: false,
                clipboard_enabled: false,
                clipboard_files_enabled: false,
            };
            peer.writer
                .write_message(MessageKind::HelloAck, &hello.encode())?;
            peer.writer.flush()?;
            Ok(())
        }

        /// Authenticate, send `Hello`, and return the host's first record
        /// with the stream that carried it.
        fn negotiate(address: SocketAddr) -> (MessageKind, Vec<u8>, TcpStream) {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let keys = client_handshake(&mut stream, &KEY, [0x11; NONCE_LEN]).unwrap();
            let (receive, send) = keys.into_client();
            let mut reader = SessionReader::new(stream.try_clone().unwrap(), receive);
            let mut writer = SessionWriter::new(stream.try_clone().unwrap(), send);
            let hello = ClientHello {
                stream: Default::default(),
                initial_viewport: None,
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            };
            writer
                .write_message(MessageKind::Hello, &hello.encode())
                .unwrap();
            writer.flush().unwrap();
            let (kind, payload) = reader.read_message().unwrap();
            (kind, payload, stream)
        }

        #[test]
        fn a_viewer_from_another_release_is_told_which_version_this_host_speaks() {
            let (address, shutdown, handle) = spawn_loop(false, |_| {
                panic!("an unsupported viewer must not be served")
            });
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let keys = client_handshake(&mut stream, &KEY, [0x12; NONCE_LEN]).unwrap();
            let (receive, send) = keys.into_client();
            let mut reader = SessionReader::new(stream.try_clone().unwrap(), receive);
            let mut writer = SessionWriter::new(stream.try_clone().unwrap(), send);
            // A version-4 viewer: same prefix, the older version number and
            // its single 32-byte keymap digest.
            let mut hello = ClientHello {
                stream: Default::default(),
                initial_viewport: None,
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            }
            .encode();
            hello.truncate(11);
            hello[8..10].copy_from_slice(&4_u16.to_be_bytes());
            hello.extend([0; 32]);
            writer.write_message(MessageKind::Hello, &hello).unwrap();
            writer.flush().unwrap();
            let (kind, reason) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Close);
            let reason = String::from_utf8(reason).unwrap();
            assert!(
                reason.contains(&format!(
                    "version 4; expected {}",
                    crate::remote::messages::APPLICATION_VERSION
                )),
                "{reason}"
            );
            assert!(
                reason.contains("update jwm-remote on both machines"),
                "{reason}"
            );

            shutdown.store(true, Ordering::Release);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn prior_version_and_malformed_current_hello_receive_legacy_text_close() {
            let valid = ClientHello {
                stream: Default::default(),
                initial_viewport: None,
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            }
            .encode();
            assert!(ClientHello::decode(&valid).is_ok());
            // v13 has the same Hello layout; only its application version differs.
            let mut prior_version = valid.clone();
            prior_version[8..10].copy_from_slice(&13_u16.to_be_bytes());
            let mut malformed = valid;
            malformed[10] |= 0x80;
            for (hello, expected) in [
                (
                    prior_version,
                    format!(
                        "unsupported JWM remote application version 13; expected {}",
                        crate::remote::messages::APPLICATION_VERSION
                    ),
                ),
                (
                    malformed,
                    "peer advertised unknown remote capabilities".to_owned(),
                ),
            ] {
                let (mut client, server) = loopback_pair();
                client
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                // Authenticate only: neither refusal may reach capture or session setup.
                let host = thread::spawn(move || {
                    authenticate_peer(server, &KEY)
                        .err()
                        .expect("invalid Hello must not create an authenticated session")
                        .to_string()
                });
                let keys = client_handshake(&mut client, &KEY, [0x13; NONCE_LEN]).unwrap();
                let (receive, send) = keys.into_client();
                let mut reader = SessionReader::new(client.try_clone().unwrap(), receive);
                let mut writer = SessionWriter::new(client, send);
                writer.write_message(MessageKind::Hello, &hello).unwrap();
                writer.flush().unwrap();
                let (kind, payload) = reader.read_message().unwrap();
                assert_eq!(kind, MessageKind::Close);
                let reason = String::from_utf8(payload).unwrap();
                assert_eq!(
                    reason,
                    format!("{expected}; update jwm-remote on both machines together")
                );
                assert_eq!(host.join().unwrap(), expected);
            }
        }

        #[test]
        fn an_idle_unauthenticated_peer_cannot_make_the_real_viewer_busy() {
            let (address, shutdown, handle) = spawn_loop(false, acknowledge);
            let mut idle = TcpStream::connect(address).unwrap();
            idle.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut greeting = [0_u8; 64];
            assert!(
                idle.read(&mut greeting).unwrap() > 0,
                "the idle peer is being served"
            );

            let (kind, _, _) = negotiate(address);
            assert_eq!(kind, MessageKind::HelloAck);

            drop(idle);
            shutdown.store(true, Ordering::Release);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn a_second_authenticated_viewer_is_told_the_host_is_busy() {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let release_rx = Mutex::new(release_rx);
            let (address, shutdown, handle) = spawn_loop(false, move |peer| {
                acknowledge(peer)?;
                let _ = release_rx.lock().unwrap().recv();
                Ok(())
            });
            let (first, _, _first_stream) = negotiate(address);
            assert_eq!(first, MessageKind::HelloAck);
            let (second, reason, _) = negotiate(address);
            assert_eq!(second, MessageKind::SessionClose);
            let close = SessionClose::decode(&reason).unwrap();
            assert_eq!(close.code, SessionCloseCode::Busy);
            assert_eq!(close.reason, BUSY_REASON);

            release_tx.send(()).unwrap();
            shutdown.store(true, Ordering::Release);
            handle.join().unwrap().unwrap();
        }

        #[test]
        fn once_counts_only_authenticated_sessions() {
            let (address, _shutdown, handle) = spawn_loop(true, acknowledge);
            // A port scanner, and a peer that does not hold the key.
            drop(TcpStream::connect(address).unwrap());
            let mut garbage = TcpStream::connect(address).unwrap();
            garbage.write_all(&[0xff; 96]).unwrap();
            drop(garbage);

            let (kind, _, _) = negotiate(address);
            assert_eq!(kind, MessageKind::HelloAck);
            handle
                .join()
                .unwrap()
                .expect("the authenticated session's own result");

            let (address, _shutdown, handle) =
                spawn_loop(true, |_| Err(io::Error::other("session failed").into()));
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let keys = client_handshake(&mut stream, &KEY, [0x22; NONCE_LEN]).unwrap();
            let (_, send) = keys.into_client();
            let mut writer = SessionWriter::new(stream.try_clone().unwrap(), send);
            let hello = ClientHello {
                stream: Default::default(),
                initial_viewport: None,
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            };
            writer
                .write_message(MessageKind::Hello, &hello.encode())
                .unwrap();
            writer.flush().unwrap();
            assert!(
                handle.join().unwrap().is_err(),
                "a failed session exits nonzero"
            );
        }
    }

    #[test]
    fn defaults_must_explicitly_opt_into_lan_and_input() {
        let options = HostOptions {
            frame_limits: Default::default(),
            listen: "127.0.0.1:48221".into(),
            key: KeySource::File(std::path::PathBuf::from("key")),
            display: None,
            wayland_select: false,
            wayland_resize_follow: false,
            wayland_popups: false,
            wayland_display: None,
            fps: 30,
            jpeg_quality: 85,
            jpeg_quality_floor: 40,
            fixed_jpeg_quality: false,
            max_width: 0,
            capture_source: CaptureSource::Auto,
            capture_area: CaptureArea::Root,
            shared_sources: Vec::new(),
            scaling: ScalingPolicy::Auto,
            allow_lan: false,
            allow_input: false,
            allow_window_resize: false,
            allow_wayland_keyboard: false,
            allow_wayland_pointer: false,
            allow_wayland_control: false,
            allow_wayland_text: false,
            allow_static_lossless: false,
            allow_wayland_scroll: false,
            allow_wayland_horizontal_scroll: false,
            scroll_copy: false,
            allow_clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            once: false,
            stats_json: false,
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
            geometry_epoch: 0,
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
            source_epoch: 1,
            geometry_epoch: 0,
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
    fn idle_healthy_acks_recover_without_waiting_for_configured_fps_sample_count() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(70, 40, true, Duration::from_millis(33), base).unwrap();
        for millis in [0, 1] {
            controller.observe_ack_at(
                quality_ack(Duration::from_millis(800), 200_000, 1, 1, 70, 0),
                base + Duration::from_millis(millis),
            );
        }
        assert_eq!(
            controller
                .quality_before_encode_at(base + Duration::from_secs(1))
                .quality,
            63
        );
        assert_eq!(controller.recovery_ack_target, 24);
        for second in [4, 8, 12] {
            let at = base + Duration::from_secs(second);
            controller.observe_ack_at(quality_ack(Duration::from_millis(20), 100, 1, 1, 63, 1), at);
            assert_eq!(
                controller.quality_before_encode_at(at).quality,
                63,
                "three idle samples cannot recover"
            );
        }
        let at = base + Duration::from_secs(16);
        controller.observe_ack_at(quality_ack(Duration::from_millis(20), 100, 1, 1, 63, 1), at);
        let decision = controller.quality_before_encode_at(at);
        assert_eq!((decision.quality, decision.epoch), (64, 2));
        // Previously collected healthy samples cannot recover the new epoch.
        assert_eq!(
            controller
                .quality_before_encode_at(at + Duration::from_secs(4))
                .quality,
            64
        );
    }

    #[test]
    fn observed_recovery_cadence_does_not_turn_a_fast_ack_burst_into_idle_proof() {
        let base = Instant::now();
        assert_eq!(
            observed_recovery_ack_target(
                24,
                4,
                Some(base),
                Some(base + Duration::from_millis(100))
            ),
            24
        );
        assert_eq!(
            observed_recovery_ack_target(24, 4, Some(base), Some(base + Duration::from_secs(12))),
            4
        );
        assert_eq!(
            observed_recovery_ack_target(24, 1, Some(base), Some(base)),
            24
        );
        assert_eq!(
            observed_recovery_ack_target(4, 24, Some(base), Some(base)),
            4
        );
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
    fn telemetry_completed_write_is_whole_on_either_side_of_reset() {
        let base = Instant::now();
        for reset_first in [false, true] {
            let telemetry = HostTelemetry::new_at(base);
            if reset_first {
                let empty = telemetry.take_due_at(base + TELEMETRY_INTERVAL).unwrap();
                assert_eq!(empty.window.sent, 0);
                assert_eq!(empty.window.writer_wait_samples.count, 0);
                assert_eq!(empty.window.socket_write_samples.count, 0);
            }
            telemetry.record_sent(100, Duration::from_millis(2), Duration::from_millis(3));
            let snapshot = telemetry
                .take_due_at(base + TELEMETRY_INTERVAL * 2)
                .unwrap();
            let window = snapshot.window;
            assert_eq!(window.sent, 1);
            assert_eq!(window.bytes, 100);
            assert_eq!(window.write_elapsed, Duration::from_millis(5));
            assert_eq!(window.writer_wait_samples.count, 1);
            assert_eq!(window.socket_write_samples.count, 1);
            let json = host_telemetry_json(snapshot, 0);
            assert_eq!(json["stages"]["writer_wait_ms"]["mean"], 2.0);
            assert_eq!(json["stages"]["socket_write_ms"]["mean"], 3.0);
            assert!(
                telemetry
                    .take_final_at(base + TELEMETRY_INTERVAL * 2)
                    .is_none()
            );
        }
    }

    #[test]
    fn telemetry_concurrent_resets_never_split_completed_write_observations() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        let start = std::sync::Barrier::new(2);
        let done = AtomicBool::new(false);
        let validate = |window: HostTelemetryWindow| {
            let count = u32::try_from(window.sent).unwrap();
            assert_eq!(window.bytes, window.sent * 100);
            assert_eq!(window.write_elapsed, Duration::from_millis(5) * count);
            assert_eq!(window.writer_wait_elapsed, Duration::from_millis(2) * count);
            assert_eq!(
                window.socket_write_elapsed,
                Duration::from_millis(3) * count
            );
            assert_eq!(window.writer_wait_samples.count, (count as usize).min(128));
            assert_eq!(window.socket_write_samples.count, (count as usize).min(128));
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                start.wait();
                for _ in 0..4096 {
                    telemetry.record_sent(100, Duration::from_millis(2), Duration::from_millis(3));
                    std::thread::yield_now();
                }
                done.store(true, Ordering::Release);
            });
            start.wait();
            let mut sent = 0;
            let mut tick = 1;
            while !done.load(Ordering::Acquire) {
                let snapshot = telemetry
                    .take_due_at(base + TELEMETRY_INTERVAL * tick)
                    .unwrap();
                validate(snapshot.window);
                sent += snapshot.window.sent;
                tick += 1;
                std::thread::yield_now();
            }
            if let Some(snapshot) = telemetry.take_final_at(base + TELEMETRY_INTERVAL * tick) {
                validate(snapshot.window);
                sent += snapshot.window.sent;
            }
            assert_eq!(sent, 4096);
        });
    }

    #[test]
    fn telemetry_quantiles_are_bounded_and_empty_values_are_null() {
        let mut samples = StageSamples::default();
        assert_eq!(samples.json()["samples"], 0);
        assert!(samples.json()["p99_ms"].is_null());
        for millis in 1..=256 {
            samples.record(Duration::from_millis(millis));
        }
        let json = samples.json();
        assert_eq!(json["samples"], 128);
        assert_eq!(json["p50_ms"], 192.0);
        assert_eq!(json["p95_ms"], 250.0);
        assert_eq!(json["p99_ms"], 255.0);
    }

    #[test]
    fn png_stage_samples_do_not_pollute_jpeg_means_or_quantiles() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        telemetry.record_encode_stages(super::super::tile::EncodeStageTimings {
            atlas_pack: Duration::from_millis(1),
            jpeg: Duration::from_millis(4),
            png: None,
        });
        telemetry.record_encode_stages(super::super::tile::EncodeStageTimings {
            atlas_pack: Duration::from_millis(2),
            jpeg: Duration::ZERO,
            png: Some(Duration::from_millis(12)),
        });
        let snapshot = telemetry.take_due_at(base + TELEMETRY_INTERVAL).unwrap();
        let json = host_telemetry_json(snapshot, 0);
        assert_eq!(json["jpeg_atlases"], 1);
        assert_eq!(json["png_atlases"], 1);
        assert_eq!(json["stages"]["jpeg_ms"]["mean"], 4.0);
        assert_eq!(json["stages"]["jpeg_ms"]["quantiles"]["p99_ms"], 4.0);
        assert_eq!(json["stages"]["png_ms"]["mean"], 12.0);
        assert_eq!(json["stages"]["codec_ms"]["mean"], 8.0);
        assert_eq!(json["stages"]["atlas_pack_ms"]["mean"], 1.5);
        telemetry.record_encode_stages(super::super::tile::EncodeStageTimings {
            atlas_pack: Duration::ZERO,
            jpeg: Duration::ZERO,
            png: Some(Duration::from_millis(5)),
        });
        let only_png = host_telemetry_json(
            telemetry
                .take_due_at(base + TELEMETRY_INTERVAL * 2)
                .unwrap(),
            0,
        );
        assert_eq!(only_png["jpeg_atlases"], 0);
        assert_eq!(only_png["png_atlases"], 1);
        assert_eq!(only_png["stages"]["jpeg_ms"]["mean"], 0.0);
        assert_eq!(only_png["stages"]["png_ms"]["mean"], 5.0);
    }

    #[test]
    fn telemetry_json_reports_distinct_pipeline_stages_and_resets_samples() {
        let base = Instant::now();
        let telemetry = HostTelemetry::new_at(base);
        telemetry.record_dimensions(&test_captured_frame(0));
        telemetry.record_captured(Duration::from_millis(30));
        telemetry.record_dequeued(Duration::from_millis(5), Duration::from_millis(7));
        telemetry.record_dirty_scan(Duration::from_millis(4));
        telemetry.record_encoded(Duration::from_millis(6), test_tile_plan(1, 4));
        telemetry.record_encode_stages(super::super::tile::EncodeStageTimings {
            atlas_pack: Duration::from_millis(2),
            jpeg: Duration::from_millis(4),
            png: None,
        });
        telemetry.record_sent(100, Duration::from_millis(2), Duration::from_millis(3));
        let snapshot = telemetry.take_due_at(base + TELEMETRY_INTERVAL).unwrap();
        let json = host_telemetry_json(snapshot, 1);
        let parsed: serde_json::Value = serde_json::from_str(&json.to_string()).unwrap();
        assert_eq!(parsed["type"], "jwm_remote_host_stats");
        assert_eq!(parsed["source_dimensions"], serde_json::json!([2, 2]));
        assert_eq!(parsed["stages"]["capture_ms"]["mean"], 30.0);
        assert_eq!(parsed["stages"]["mailbox_age_ms"]["mean"], 5.0);
        assert_eq!(parsed["stages"]["dirty_scan_ms"]["mean"], 4.0);
        assert_eq!(parsed["stages"]["atlas_pack_ms"]["mean"], 2.0);
        assert_eq!(parsed["stages"]["jpeg_ms"]["quantiles"]["p99_ms"], 4.0);
        assert_eq!(parsed["stages"]["writer_wait_ms"]["mean"], 2.0);
        assert_eq!(parsed["stages"]["socket_write_ms"]["mean"], 3.0);
        let reset = telemetry
            .take_due_at(base + TELEMETRY_INTERVAL * 2)
            .unwrap();
        assert_eq!(reset.window.capture_samples.count, 0);
        assert_eq!(reset.window.dirty_scans, 0);
        assert_eq!(reset.window.atlas_encoded, 0);
        assert_eq!(reset.window.jpeg_samples.count, 0);
    }

    #[test]
    fn capture_start_ack_latency_includes_readback_but_queue_age_does_not() {
        let base = Instant::now();
        let pending = PendingFrame {
            pointer_serial: None,
            source_version: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            frame: test_captured_frame(0),
            captured_at: base,
            capture_completed_at: base + Duration::from_millis(30),
            recycler: None,
        };
        let dequeued = base + Duration::from_millis(35);
        assert_eq!(
            dequeued.duration_since(pending.capture_completed_at),
            Duration::from_millis(5)
        );
        let credits = FrameCredits::new();
        credits
            .mark_sent(
                0,
                pending.captured_at,
                base + Duration::from_millis(45),
                100,
                70,
                0,
            )
            .unwrap();
        let ack = credits
            .acknowledge_at(0, base + Duration::from_millis(60))
            .unwrap()
            .unwrap();
        assert_eq!(ack.capture_to_ack, Duration::from_millis(60));
        assert_eq!(ack.send_to_ack, Duration::from_millis(15));
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
        telemetry.record_sent(100, Duration::ZERO, Duration::from_millis(8));
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
        telemetry.record_sent(200, Duration::ZERO, Duration::from_millis(9));

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
        assert_eq!(window.bytes, 0);
        assert_eq!(window.write_elapsed, Duration::ZERO);
        assert_eq!(window.writer_wait_elapsed, Duration::ZERO);
        assert_eq!(window.socket_write_elapsed, Duration::ZERO);
        assert_eq!(window.writer_wait_samples.count, 0);
        assert_eq!(window.socket_write_samples.count, 0);
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
        assert_eq!(window.bytes, 0);
        assert_eq!(window.write_elapsed, Duration::ZERO);
        assert_eq!(window.writer_wait_elapsed, Duration::ZERO);
        assert_eq!(window.socket_write_elapsed, Duration::ZERO);
        assert_eq!(window.writer_wait_samples.count, 0);
        assert_eq!(window.socket_write_samples.count, 0);
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
    fn partial_input_record_cannot_outlive_the_held_input_lease() {
        let (receiver, mut peer) = loopback_pair();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut writer = SessionWriter::new(Vec::new(), [0x5a; 32]);
        writer.write_message(MessageKind::Heartbeat, &[]).unwrap();
        let bytes = writer.into_inner();
        let drip = thread::spawn(move || {
            for byte in bytes {
                if peer.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut reader = SessionReader::new(receiver, [0x5a; 32]);
        let started = Instant::now();
        let deadline = started + Duration::from_millis(40);
        assert!(read_input_record(&mut reader, &mut Vec::new(), Some(deadline)).is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        drip.join().unwrap();
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
            receive_input_scoped(
                reader,
                None,
                (0, 0).into(),
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
                None,
            )
            .report(|_| panic!("network timeout cannot report backend loss"))
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

    #[test]
    fn lan_listener_notice_matches_the_encrypted_transport() {
        use std::net::{Ipv4Addr, Ipv6Addr};

        // The notice must describe what the wire actually carries: a payload
        // written through the session writer never appears in cleartext.
        let secret = b"typed-password-and-screen-pixels";
        let mut writer = SessionWriter::new(Vec::new(), [0x5a; 32]);
        writer
            .write_message(MessageKind::Frame, secret)
            .expect("seal a small frame record");
        let wire = writer.into_inner();
        assert!(!wire.windows(secret.len()).any(|window| window == secret));

        for address in [
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 20)),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        ] {
            let notice = listener_security_notice(address)
                .unwrap_or_else(|| panic!("{address} is reachable beyond loopback"));
            assert!(notice.contains("encrypts"), "{notice}");
            assert!(!notice.contains("does not encrypt"), "{notice}");
            assert!(notice.contains("forward secrecy"), "{notice}");
            assert!(notice.contains("leaked key file"), "{notice}");
            assert!(notice.contains("future peer impersonation"), "{notice}");
            assert!(!notice.contains("without forward secrecy"), "{notice}");
        }

        assert_eq!(
            listener_security_notice(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            None
        );
        assert_eq!(
            listener_security_notice(IpAddr::V6(Ipv6Addr::LOCALHOST)),
            None
        );
    }
    #[test]
    fn static_refinement_commits_its_rate_limit_without_waiting_for_global_quiet() {
        let base = Instant::now();
        let source = test_captured_frame(10);
        let mut sender = FrameSender::new();
        accept_frame(&mut sender, &source, base);
        assert!(
            !sender.refinement_due(base + STATIC_REFINEMENT_INTERVAL - Duration::from_nanos(1))
        );
        assert!(sender.refinement_due(base + STATIC_REFINEMENT_INTERVAL));
        let at = base + STATIC_REFINEMENT_INTERVAL;
        assert!(!sender.plan_at(&source, at).unwrap().emit);
        let plan = sender.refine_at(85, at).unwrap();
        assert_eq!(plan.dirty_tiles, 1);
        let mut payload = Vec::new();
        sender
            .encoder
            .encode_into(&mut payload, 1, &source, 85)
            .unwrap();
        sender.discard();
        assert!(sender.refinement_due(at));
        assert!(sender.encoder.has_quality_debt(85));
        sender.plan_at(&source, at).unwrap();
        sender.refine_at(85, at).unwrap();
        sender
            .encoder
            .encode_into(&mut payload, 1, &source, 85)
            .unwrap();
        sender.commit_at(&source, at);
        assert!(!sender.refinement_due(at + Duration::from_secs(1)));
        assert!(!sender.encoder.has_quality_debt(85));
        assert!(
            !sender
                .plan_at(&source, at + Duration::from_secs(2))
                .unwrap()
                .emit
        );
        let changed = test_captured_frame(20);
        sender
            .plan_at(&changed, at + Duration::from_secs(2))
            .unwrap();
        assert!(sender.refinement_due(at + Duration::from_secs(3)));
    }

    #[test]
    fn refinement_quality_peek_preserves_acks_and_downgrade_cancels_debt() {
        let base = Instant::now();
        let controller =
            JpegQualityController::new_at(85, 40, true, Duration::from_millis(33), base).unwrap();
        controller.observe_ack_at(
            quality_ack(Duration::from_millis(800), 100, 1, 1, 85, 0),
            base,
        );
        assert_eq!(controller.current_quality(), 85);
        let state = controller.state.lock().unwrap();
        assert_eq!(state.pending_acks.len(), 1);
        assert_eq!(state.last_evaluated_at, base);
        drop(state);
        let mut sender = FrameSender::new();
        let source = test_captured_frame(10);
        accept_frame(&mut sender, &source, base);
        let at = base + STATIC_REFINEMENT_INTERVAL;
        sender.plan_at(&source, at).unwrap();
        assert!(sender.refine_at(85, at).unwrap().emit);
        assert!(sender.pending_refinement);
        assert_eq!(sender.encoder.detection_scans, 1);
        assert!(!sender.refine_at(60, at).unwrap().emit);
        assert_eq!(
            sender.encoder.detection_scans, 1,
            "quality epoch changes must reuse the capture's dirty detection"
        );
        assert!(!sender.pending_refinement);
        assert!(sender.encoder.has_quality_debt(85));
        let at = base + UNCHANGED_FRAME_KEEPALIVE;
        sender.plan_at(&source, at).unwrap();
        let plan = sender.refine_at(60, at).unwrap();
        assert!(
            plan.emit && plan.dirty_tiles == 0,
            "cancelled debt must not starve keepalive"
        );
    }
    #[test]
    fn continuous_damage_cannot_postpone_even_the_first_refinement() {
        let base = Instant::now();
        let mut sender = FrameSender::new();
        let first = test_captured_frame(10);
        accept_frame(&mut sender, &first, base);
        for step in 1..=20 {
            let now = base + STATIC_REFINEMENT_INTERVAL * step / 10;
            sender
                .plan_at(&test_captured_frame(step as u8 + 20), now)
                .unwrap();
            if step >= 10 {
                assert!(sender.refinement_due(now));
            }
        }
    }

    #[test]
    fn static_refinement_clock_cannot_be_starved_by_new_local_damage() {
        let base = Instant::now();
        let interval = STATIC_REFINEMENT_INTERVAL;
        assert!(!static_refinement_due(None, None, base + interval));
        assert!(!static_refinement_due(Some(base), None, base));
        assert!(!static_refinement_due(
            Some(base),
            None,
            base + interval - Duration::from_nanos(1)
        ));
        assert!(static_refinement_due(Some(base), None, base + interval));
        assert!(static_refinement_due(
            Some(base + interval),
            Some(base),
            base + interval
        ));
        assert!(!static_refinement_due(
            Some(base),
            Some(base + interval),
            base + interval
        ));
        assert!(static_refinement_due(
            Some(base),
            Some(base + interval),
            base + interval * 2
        ));
        assert!(!static_refinement_due(Some(base + interval), None, base));
    }

    #[test]
    fn refinement_flush_failure_preserves_debt_reference_and_cooldown() {
        let mailbox = Arc::new(LatestMailbox::new());
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let running = AtomicBool::new(true);
        let credits = FrameCredits::new();
        let telemetry = HostTelemetry::new();
        let jpeg_quality =
            JpegQualityController::new_at(85, 40, true, Duration::from_millis(33), Instant::now())
                .unwrap();
        let committed_at = Instant::now()
            .checked_sub(STATIC_REFINEMENT_INTERVAL + Duration::from_millis(1))
            .unwrap();
        let mut sender = FrameSender::new();
        let source = test_captured_frame(10);
        accept_frame(&mut sender, &source, committed_at);
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(FlushFailWriter::default(), WRITE_TIMEOUT),
            [0x42; 32],
        )));
        assert!(
            send_frames_with_encoder(
                mailbox,
                writer,
                &running,
                &credits,
                &telemetry,
                &jpeg_quality,
                &mut sender
            )
            .is_err()
        );
        assert_eq!(sender.committed_at, Some(committed_at));
        assert_eq!(sender.last_refinement, None);
        assert!(sender.encoder.has_quality_debt(85));
        assert!(!sender.pending_refinement);
        assert_eq!(telemetry.state.lock().unwrap().window.sent, 0);
        sender.plan_at(&source, Instant::now()).unwrap();
        assert!(sender.refine_at(85, Instant::now()).unwrap().dirty_tiles > 0);
    }
    #[test]
    fn refinement_commit_clock_avoids_double_tick_and_stall_catchup() {
        let base = Instant::now();
        let tick = STATIC_REFINEMENT_INTERVAL;
        let planned = base + tick;
        let committed = planned + Duration::from_millis(50);
        let clock = refinement_clock_after_commit(planned, committed);
        assert_eq!(clock, planned);
        assert!(static_refinement_due(
            Some(base),
            Some(clock),
            base + tick * 2
        ));
        assert!(!static_refinement_due(
            Some(base),
            Some(clock),
            planned + Duration::from_secs(1)
        ));
        let stalled = planned + tick * 3;
        let clock = refinement_clock_after_commit(planned, stalled);
        assert_eq!(clock, stalled);
        assert!(!static_refinement_due(Some(base), Some(clock), stalled));
        assert!(!static_refinement_due(
            Some(base),
            Some(clock),
            stalled + tick - Duration::from_nanos(1)
        ));
        assert!(static_refinement_due(
            Some(base),
            Some(clock),
            stalled + tick
        ));
    }

    #[test]
    fn refinement_sender_flushes_decodable_delta_then_resumes_suppression() {
        let mailbox = Arc::new(LatestMailbox::new());
        let source = test_captured_frame(10);
        let mut sender = FrameSender::new();
        let base = Instant::now()
            .checked_sub(STATIC_REFINEMENT_INTERVAL + Duration::from_millis(1))
            .unwrap();
        sender.plan_at(&source, base).unwrap();
        let mut baseline = Vec::new();
        sender
            .encoder
            .encode_into(&mut baseline, 0, &source, 70)
            .unwrap();
        sender.commit_at(&source, base);
        let gate = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..GateState::default()
            }),
            Condvar::new(),
        ));
        let writer = Arc::new(Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(Arc::clone(&gate)), WRITE_TIMEOUT),
            [0x43; 32],
        )));
        let running = Arc::new(AtomicBool::new(true));
        let credits = Arc::new(FrameCredits::new());
        let telemetry = Arc::new(HostTelemetry::new());
        let jpeg_quality = Arc::new(
            JpegQualityController::new_at(85, 40, true, Duration::from_millis(33), Instant::now())
                .unwrap(),
        );
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let sender_mailbox = Arc::clone(&mailbox);
        let sender_running = Arc::clone(&running);
        let sender_credits = Arc::clone(&credits);
        let sender_telemetry = Arc::clone(&telemetry);
        let handle = thread::spawn(move || {
            send_frames_with_encoder(
                sender_mailbox,
                writer,
                &sender_running,
                &sender_credits,
                &sender_telemetry,
                &jpeg_quality,
                &mut sender,
            )
            .unwrap();
            sender
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while telemetry.state.lock().unwrap().window.sent < 1 {
            assert!(Instant::now() < deadline, "refinement was not sent");
            thread::sleep(Duration::from_millis(1));
        }
        let ack = credits.acknowledge(0).unwrap().unwrap();
        assert_eq!((ack.jpeg_quality, ack.quality_epoch), (85, 0));
        assert_eq!(mailbox.publish(test_pending_frame(10)), Some(false));
        let deadline = Instant::now() + Duration::from_secs(2);
        while telemetry.state.lock().unwrap().window.unchanged_suppressed < 1 {
            assert!(
                Instant::now() < deadline,
                "debt-free frame was not suppressed"
            );
            thread::sleep(Duration::from_millis(1));
        }
        running.store(false, Ordering::Release);
        mailbox.close();
        credits.close();
        let mut sender = handle.join().unwrap();
        assert!(!sender.encoder.has_quality_debt(85));
        assert!(sender.last_refinement.is_some());
        let state = gate.0.lock().unwrap();
        assert_eq!(state.flushes, 1);
        let mut reader = SessionReader::new(Cursor::new(state.bytes.clone()), [0x43; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Frame);
        let mut decoder = TileDecoder::new();
        decode_wire(&mut decoder, &baseline);
        let (sequence, image) = decode_wire(&mut decoder, &payload);
        assert_eq!(sequence, 0);
        assert_eq!(image.dimensions(), source.image.dimensions());
        assert_eq!(telemetry.state.lock().unwrap().window.sent, 1);
        let next_idle = sender.committed_at.unwrap() + UNCHANGED_FRAME_KEEPALIVE;
        let keepalive = sender.plan_at(&source, next_idle).unwrap();
        assert!(keepalive.emit && keepalive.dirty_tiles == 0);
    }
    #[test]
    fn host_final_clipboard_writer_gates_queued_payload_on_both_sides_of_ack() {
        use super::super::clipboard_sync::{ClipboardControl, TransferKind, TransferOp};
        let native = Arc::new((
            Mutex::new(GateState {
                open: true,
                ..Default::default()
            }),
            Condvar::new(),
        ));
        let writer = Mutex::new(SessionWriter::new(
            DeadlineWriter::new(GateWriter(native.clone()), WRITE_TIMEOUT),
            [0x79; 32],
        ));
        let control = ClipboardControl::for_test(7);
        let request = ClipboardRestriction {
            revision: 1,
            content: 0,
        };
        control
            .restrict(request, MessageKind::ClipboardRestricted)
            .unwrap();
        let record = |op: TransferOp| Outgoing {
            kind: MessageKind::ClipboardTransfer,
            payload: op.encode(),
        };
        let mut records = std::collections::VecDeque::from([
            record(TransferOp::Begin {
                id: 1,
                kind: TransferKind::Files,
                length: 3,
                entries: 1,
                items: 1,
            }),
            record(TransferOp::Data {
                id: 1,
                bytes: b"old".to_vec(),
            }),
            record(TransferOp::End { id: 1 }),
            Outgoing {
                kind: MessageKind::ClipboardRestricted,
                payload: request.encode().to_vec(),
            },
            text_record("must not follow confirmed off"),
            record(TransferOp::Begin {
                id: 2,
                kind: TransferKind::Image,
                length: 4,
                entries: 0,
                items: 1,
            }),
            record(TransferOp::Data {
                id: 2,
                bytes: b"late".to_vec(),
            }),
            record(TransferOp::End { id: 2 }),
        ]);
        let running = AtomicBool::new(true);
        forward_clipboard_with_policy(
            |_| {
                let record = records.pop_front();
                if record.is_none() {
                    running.store(false, Ordering::Release);
                }
                Ok(record)
            },
            &writer,
            &running,
            Some(&control),
        )
        .unwrap();
        let bytes = native.0.lock().unwrap().bytes.clone();
        let mut reader = SessionReader::new(std::io::Cursor::new(bytes), [0x79; 32]);
        for expected in [
            TransferOp::Cancel { id: 1 },
            TransferOp::Skip { id: 1, bytes: 3 },
            TransferOp::Cancel { id: 1 },
        ] {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::ClipboardTransfer);
            assert_eq!(TransferOp::decode(&payload).unwrap(), expected);
        }
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::ClipboardRestricted);
        assert_eq!(ClipboardRestriction::decode(&payload).unwrap(), request);
        for expected in [
            TransferOp::Cancel { id: 2 },
            TransferOp::Skip { id: 2, bytes: 4 },
            TransferOp::Cancel { id: 2 },
        ] {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::ClipboardTransfer);
            assert_eq!(TransferOp::decode(&payload).unwrap(), expected);
        }
        assert!(reader.read_message().is_err());
        assert!(
            control.restriction_written(request).is_err(),
            "ACK cleared pending atomically"
        );
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[path = "host_keyboard_integration.rs"]
mod host_keyboard_integration;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[path = "host_pointer_integration.rs"]
mod host_pointer_integration;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[path = "host_viewer_integration.rs"]
mod host_viewer_integration;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[path = "host_keyboard_viewer_integration.rs"]
mod host_keyboard_viewer_integration;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[path = "wayland_viewer_integration.rs"]
mod wayland_viewer_integration;
