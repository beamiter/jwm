//! Remote desktop viewer/client orchestration.

#[cfg(test)]
#[path = "client_cleanup_probe.rs"]
pub(super) mod cleanup_probe;

#[path = "client_control.rs"]
mod control;
#[path = "client_popup.rs"]
mod popup;
use control::ClientControl;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) use control::{with_control_lifetime_observer, with_control_repair_observer};
use popup::ClientPopup;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) use popup::{
    PopupLifetimeFailureReason, PopupLifetimeObservation, with_popup_lifetime_observer,
};
#[path = "client_text.rs"]
mod text;
use super::wayland_keyboard_wire::DrawnScope;
use text::ClientText;
#[path = "client_pointer_repair.rs"]
mod pointer_repair;
use pointer_repair::PointerRepair;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) use pointer_repair::{with_pointer_lifetime_observer, with_pointer_repair_observer};

use super::clipboard_policy::{self, ClipboardDirection, ClipboardMode};
use super::clipboard_sync::{
    ClipboardInbox, ClipboardOutbox, ClipboardSync, Outgoing, SyncOptions, SyncStatus,
};
use super::clipboard_wire::{ClipboardRestriction, RestrictionState};
use super::close_wire::{SessionClose, SessionCloseCode};
use super::connect_toggle::ConnectToggle;
use super::deadline::TcpStreamDeadline;
use super::engine::ClipboardSnapshot;
#[cfg(feature = "remote-h264")]
use super::frame::FrameInfo;
use super::frame::{
    FrameLease, RecyclableDecodedFrame, SharedDecodeBufferPool, new_decode_buffer_pool,
};
#[cfg(feature = "remote-h264")]
use super::h264_wire::{self, HardwareFrameHeader, VideoPayload};
use super::key::KeySource;
use super::messages::{
    ClientHello, FrameCodec, InputMode, MAX_INPUT_BATCH_EVENTS, ServerHello,
    encode_input_batch_into, encode_source_frame_ack, encode_viewport,
};
use super::protocol::{
    MAX_PAYLOAD_LEN, MessageKind, PayloadBufferRetention, ProtocolError, SessionReader,
    SessionWriter, client_handshake,
};
use super::sender::{RecordWriter, Sender, reap_worker};
use super::source_wire::{
    FramePurpose, SourceCatalog, SourceRequest, decode_frame_payload, decode_source_changed,
    decode_source_payload,
};
use super::tile::TileDecoder;
use super::usability::{ClientStage, failure_hint, session_mode};
use super::viewer_backend::{SeamlessInput, Viewer, ViewerBackend};
use super::x11_input::InputEvent;
use super::x11_keymap::{KeymapTable, read_display};
use super::x11_viewer::{ViewerEvent, validate_escape_key};
use super::{RemoteError, RemoteResult, VIDEO_FRAME_IDLE_TIMEOUT};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::io::Write as _;
use std::io::{self, Read as _};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::messages::{ApplicationProfile, ViewerFrameFeedback};

#[cfg(feature = "remote-h264")]
#[path = "client_native_video.rs"]
mod native_video;

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type LosslessFrameObserver = Arc<Mutex<Vec<(u64, bool)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static LOSSLESS_FRAME_OBSERVER: std::cell::RefCell<Option<LosslessFrameObserver>>=const {std::cell::RefCell::new(None)};}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_static_lossless_frame_observer<T>(
    observer: LosslessFrameObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<LosslessFrameObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LOSSLESS_FRAME_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(LOSSLESS_FRAME_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type FramePurposeObserver = Arc<Mutex<Vec<(u64, FramePurpose)>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static FRAME_PURPOSE_OBSERVER: std::cell::RefCell<Option<FramePurposeObserver>>=const {std::cell::RefCell::new(None)};}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_frame_purpose_observer<T>(
    observer: FramePurposeObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<FramePurposeObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            FRAME_PURPOSE_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(FRAME_PURPOSE_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone, Debug)]
pub(crate) struct ScrollDecodeObservation {
    pub seq: u64,
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub scroll_dy: Option<i16>,
    pub dirty_tiles: u32,
    pub total_tiles: u32,
    pub tile_log2: u8,
    pub lossless: bool,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
type ScrollDecodeObserver = Arc<Mutex<Vec<ScrollDecodeObservation>>>;
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! {static SCROLL_DECODE_OBSERVER: std::cell::RefCell<Option<ScrollDecodeObserver>>=const {std::cell::RefCell::new(None)};}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_scroll_decode_observer<T>(
    observer: ScrollDecodeObserver,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<ScrollDecodeObserver>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCROLL_DECODE_OBSERVER.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(SCROLL_DECODE_OBSERVER.with(|slot| slot.replace(Some(observer))));
    run()
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
fn observe_decoded_scroll(
    observer: &Option<ScrollDecodeObserver>,
    bytes: &[u8],
    source_epoch: u64,
    frame: &RecyclableDecodedFrame,
    lossless: bool,
) {
    let Some(observer) = observer else {
        return;
    };
    // Read-only extraction AFTER production decode accepted the complete header,
    // atlas and scope. Never grants permission, changes pixels or generates ACKs.
    assert!(bytes.len() >= 32);
    let tile_log2 = bytes[16];
    let side = 1u32 << tile_log2;
    let (width, height) = frame.dimensions();
    let row = ScrollDecodeObservation {
        seq: frame.sequence(),
        source_epoch,
        geometry_epoch: frame.geometry_epoch(),
        scroll_dy: (bytes[17] & 2 != 0)
            .then(|| i16::from_be_bytes(bytes[40..42].try_into().unwrap())),
        dirty_tiles: u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        total_tiles: width.div_ceil(side) * height.div_ceil(side),
        tile_log2,
        lossless,
    };
    let mut rows = observer.lock().unwrap();
    assert!(
        rows.len() < 4096,
        "scroll decode observation budget exceeded"
    );
    rows.push(row);
}
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
// Stay comfortably inside the host's 600 ms held-input safety lease even
// while a modifier is held over a static desktop and no frame ACKs arrive.
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(200);
// Bound clipboard work before the next input pass, frame ACK and heartbeat:
// sixteen records cover a whole transfer window of chunks plus small control
// records, and leftovers stay queued for the next pass.
const MAX_VIEWER_CLIPBOARD_RECORDS_PER_PASS: usize = 16;
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);
const RECEIVER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub stats_json: bool,
    /// Explicit immutable requested duration; never inferred from host permission.
    pub control_duration: super::resumable_control::ControlDuration,
    pub viewer_backend: ViewerBackend,
    pub viewer_wayland_display: Option<String>,
    pub wayland_keyboard: bool,
    pub wayland_pointer: bool,
    pub wayland_control: bool,
    pub wayland_gestures: bool,
    /// Explicit version-1 resumable protocol opt-in; never implied by control.
    pub resumable_control: bool,
    /// Explicit JWM compositor seamless handoff; never implied by resumable control.
    pub wayland_seamless_input: bool,
    pub wayland_popup_pointer: bool,
    pub static_lossless: bool,
    pub wayland_text: Option<super::wayland_text_wire::ExplicitText>,
    pub wayland_scroll: bool,
    pub wayland_horizontal_scroll: bool,
    /// One explicit source-size operation; never replayed or saved in a profile.
    pub resize_source: Option<super::window_size::WindowSize>,
    pub follow_source_size: bool,
    /// Explicit local client-area startup size; None preserves automatic sizing.
    pub window_size: Option<super::window_size::WindowSize>,
    /// Explicit local best-effort startup position; coordinates are bounded by i16.
    pub window_position: Option<super::window_position::WindowPosition>,
    /// Native XEmbed container on the viewer's local X11 display.
    pub x11_parent_window: Option<std::num::NonZeroU32>,
    pub frame_limits: super::messages::FrameLimits,
    pub address: String,
    /// What to call the host in messages and the window title: the address
    /// as typed, or the SSH destination when the address is a local tunnel.
    pub peer_label: String,
    pub key: KeySource,
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
    pub seamless_input: Option<SeamlessInput>,
    pub clipboard: bool,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_direction: ClipboardDirection,
    /// Largest file copy shared through the clipboard; 0 shares no files.
    pub clipboard_file_limit: u64,
    /// Base X keysym of the Ctrl+Super local grab-release shortcut.
    pub escape_key: String,
    pub scaling: super::scaling::ScalingPolicy,
}

#[derive(Debug)]
struct ClientFailure {
    stage: ClientStage,
    cause: RemoteError,
}

impl std::fmt::Display for ClientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = self.cause.downcast_ref::<io::Error>().map(io::Error::kind);
        write!(
            f,
            "{:?}: {}\n{}",
            self.stage,
            describe_cause(&self.cause),
            failure_hint(self.stage, kind)
        )
    }
}

/// The cause as a person should read it.
///
/// A peer that vanished mid-record surfaces as Rust's `failed to fill whole
/// buffer`, which reads like a bug in the viewer. A host that stops on purpose
/// now says goodbye first, so a bare end of stream means it did not get to.
fn describe_cause(cause: &RemoteError) -> String {
    let protocol = cause
        .downcast_ref::<ProtocolError>()
        .or_else(|| {
            cause
                .downcast_ref::<ReceiverTransportLoss>()
                .map(|error| &error.0)
        })
        .or_else(|| {
            cause
                .downcast_ref::<CleanTransportDisconnect>()
                .map(|error| &error.0)
        });
    let vanished = match protocol {
        Some(ProtocolError::Io(error)) => error.kind() == io::ErrorKind::UnexpectedEof,
        _ => cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::UnexpectedEof),
    };
    if vanished {
        "the connection closed without a goodbye from the host: its process ended or the \
         network dropped"
            .to_owned()
    } else {
        cause.to_string()
    }
}

impl std::error::Error for ClientFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

#[derive(Debug)]
pub(super) struct HardwareDecodeFailure(io::Error);

impl std::fmt::Display for HardwareDecodeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "H.264 hardware decoder failed: {}", self.0)
    }
}

impl std::error::Error for HardwareDecodeFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Only a direct decoder failure after cleanup permits a fresh JPEG session.
/// Nested failures inside SessionCleanupError cannot authorize another attempt.
pub(super) fn hardware_decode_failure_clean(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<ClientFailure>()
        .is_some_and(|failure| failure.cause.is::<HardwareDecodeFailure>())
}

#[cfg(feature = "remote-h264")]
fn hardware_decoder_error(error: io::Error) -> RemoteError {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
    ) {
        Box::new(error)
    } else {
        Box::new(HardwareDecodeFailure(error))
    }
}

fn strip_hardware_retry(error: RemoteError) -> RemoteError {
    match error.downcast::<HardwareDecodeFailure>() {
        Ok(decoder) => Box::new(decoder.0),
        Err(error) => error,
    }
}

/// Only the direct authenticated terminal cause qualifies. Cleanup wrappers,
/// formatted messages and nested arbitrary sources are deliberately excluded.
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn popup_delivered_terminal(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<ClientFailure>()
        .is_some_and(|failure| popup::is_delivered_terminal(failure.cause.as_ref()))
}

/// Content-free terminal classification. Every string is a fixed vocabulary;
/// neither error formatting nor peer-supplied text participates in this record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FailureDiagnostic {
    pub stage: Option<&'static str>,
    pub category: &'static str,
    pub io_kind: Option<&'static str>,
    pub peer_close_code: Option<SessionCloseCode>,
    pub cleanup: &'static str,
    pub reconnect_certified: bool,
    #[cfg(test)]
    #[serde(skip)]
    pub cleanup_sender_transport_only: bool,
}

/// Classify without changing retry policy or making teardown claims from an
/// ordinary I/O error. Traversal is bounded even for a cyclic custom source.
pub fn classify_failure(error: &(dyn std::error::Error + 'static)) -> FailureDiagnostic {
    let certified = error
        .downcast_ref::<ClientFailure>()
        .is_some_and(|failure| failure.cause.is::<CleanTransportDisconnect>());
    let mut result = FailureDiagnostic {
        stage: None,
        category: "unknown",
        io_kind: None,
        peer_close_code: None,
        cleanup: if certified { "complete" } else { "unknown" },
        reconnect_certified: certified,
        #[cfg(test)]
        cleanup_sender_transport_only: false,
    };
    let mut current = Some(error);
    for _ in 0..32 {
        let Some(error) = current else { break };
        #[cfg(feature = "remote-wayland")]
        if result.category == "unknown" {
            use super::wayland_viewer::{PresentationDiscarded, PresentationTimeout};
            if error.is::<PresentationDiscarded>() {
                result.category = "wayland_presentation_discarded";
            } else if error.is::<PresentationTimeout>() {
                result.category = "wayland_presentation_timeout";
            }
        }
        if let Some(failure) = error.downcast_ref::<ClientFailure>() {
            result.stage.get_or_insert(match failure.stage {
                ClientStage::Setup => "setup",
                ClientStage::Connect => "connect",
                ClientStage::Negotiate => "negotiate",
                ClientStage::FirstFrame => "first_frame",
                ClientStage::LocalClipboard => "local_clipboard",
                ClientStage::Display => "display",
                ClientStage::FirstFrameAck => "first_frame_ack",
                ClientStage::Session => "session",
            });
        } else if error.is::<SessionCleanupError>() || super::host::cleanup_incomplete(error) {
            result.cleanup = "incomplete";
            result.reconnect_certified = false;
            result.category = "cleanup_failed";
        } else if error.is::<CleanTransportDisconnect>() || error.is::<ReceiverTransportLoss>() {
            if result.category == "unknown" {
                result.category = "transport_loss";
            }
        } else if let Some(peer) = error.downcast_ref::<PeerClosed>() {
            result.peer_close_code = peer.code;
            result.reconnect_certified = false;
            if result.category == "unknown" {
                use super::wayland_control_wire::{
                    GESTURE_CONSENT_REQUIRED, GESTURE_NATIVE_V6_REQUIRED,
                };
                result.category = match (
                    peer.code,
                    peer.reason
                        .strip_prefix("host cannot capture its display: "),
                ) {
                    (
                        Some(SessionCloseCode::CaptureUnavailable),
                        Some(GESTURE_CONSENT_REQUIRED),
                    ) => "wayland_gestures_permission_required",
                    (
                        Some(SessionCloseCode::CaptureUnavailable),
                        Some(GESTURE_NATIVE_V6_REQUIRED),
                    ) => "native_wayland_gestures_unavailable",
                    _ => "peer_closed",
                };
            }
        } else if error.is::<UserClosed>() {
            if result.category == "unknown" {
                result.category = "user_closed";
            }
        } else if let Some(refusal) = error.downcast_ref::<ConfirmedReleaseUnavailable>() {
            if result.category == "unknown" {
                result.category = if refusal.gestures {
                    "managed_wayland_gestures_unavailable"
                } else {
                    "wayland_release_confirmation_unavailable"
                };
            }
        } else if let Some(protocol) = error.downcast_ref::<ProtocolError>() {
            let category = match protocol {
                ProtocolError::Io(error) => {
                    result.io_kind = Some(diagnostic_io_kind(error.kind()));
                    "protocol_io"
                }
                ProtocolError::InvalidPskLength { .. } => "invalid_psk_length",
                ProtocolError::AuthenticationFailed => "authentication_failed",
                ProtocolError::UnknownMessageKind(_) => "unknown_message_kind",
                ProtocolError::PayloadTooLarge { .. } => "payload_too_large",
                ProtocolError::SequenceMismatch { .. } => "sequence_mismatch",
                ProtocolError::SequenceExhausted => "sequence_exhausted",
                ProtocolError::MalformedPadding => "malformed_padding",
                ProtocolError::UnsupportedTransportVersion { .. } => {
                    "unsupported_transport_version"
                }
                ProtocolError::BadTransportMagic => "bad_transport_magic",
            };
            if result.category == "unknown" {
                result.category = category;
            }
        } else if let Some(io) = error.downcast_ref::<io::Error>() {
            if result.category == "unknown" {
                result.category = "io";
            }
            result.io_kind.get_or_insert(diagnostic_io_kind(io.kind()));
            // io::Error's source may skip the contained wrapper itself.
            current = io
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static));
            continue;
        }
        current = error.source();
    }
    #[cfg(test)]
    {
        // Accept exactly one cleanup wrapper with a direct authenticated close
        // primary. Nested/host/unknown cleanup cannot acquire this evidence.
        let mut current = Some(error);
        let mut cleanup_count = 0;
        let mut sender_only = false;
        for _ in 0..32 {
            let Some(error) = current else { break };
            if let Some(cleanup) = error.downcast_ref::<SessionCleanupError>() {
                cleanup_count += 1;
                sender_only = cleanup.sender_transport_only
                    && cleanup
                        .primary
                        .as_ref()
                        .is_some_and(|p| p.is::<PeerClosed>());
            } else if super::host::cleanup_incomplete(error) {
                cleanup_count += 1;
                sender_only = false;
            }
            current = if let Some(io) = error.downcast_ref::<io::Error>() {
                io.get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static))
            } else {
                error.source()
            };
        }
        result.cleanup_sender_transport_only =
            current.is_none() && cleanup_count == 1 && sender_only;
    }
    result
}

fn diagnostic_io_kind(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::ConnectionRefused => "connection_refused",
        io::ErrorKind::ConnectionReset => "connection_reset",
        io::ErrorKind::ConnectionAborted => "connection_aborted",
        io::ErrorKind::NotConnected => "not_connected",
        io::ErrorKind::AddrInUse => "addr_in_use",
        io::ErrorKind::AddrNotAvailable => "addr_not_available",
        io::ErrorKind::BrokenPipe => "broken_pipe",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::WouldBlock => "would_block",
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::WriteZero => "write_zero",
        io::ErrorKind::Interrupted => "interrupted",
        io::ErrorKind::UnexpectedEof => "unexpected_eof",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::OutOfMemory => "out_of_memory",
        _ => "other",
    }
}

pub fn run_client(options: ClientOptions) -> RemoteResult<()> {
    run_client_with_hardware(options, false)
}

pub fn run_client_with_hardware(options: ClientOptions, hardware_h264: bool) -> RemoteResult<()> {
    let preferred = if hardware_h264 {
        FrameCodec::H264
    } else {
        FrameCodec::JpegTiles
    };
    run_client_with_video_codec(options, preferred)
}

pub(super) fn run_client_with_video_codec(
    options: ClientOptions,
    preferred: FrameCodec,
) -> RemoteResult<()> {
    run_client_attempt(options, preferred, None, None)
}

fn run_client_attempt(
    options: ClientOptions,
    preferred: FrameCodec,
    toggle: Option<&ConnectToggle>,
    window: Option<&mut Option<Viewer>>,
) -> RemoteResult<()> {
    if toggle.is_some_and(|toggle| !toggle.begin_attempt()) {
        return Ok(());
    }
    let mut stage = ClientStage::Setup;
    if options.wayland_gestures && !toggle.is_some_and(|toggle| !toggle.owns_shortcut()) {
        return Err(invalid_data("Wayland gestures require a managed session").into());
    }
    let result = run_client_session(options, preferred, &mut stage, toggle, window);
    match result {
        Ok(()) => {
            eprintln!("jwm-remote: session ended");
            Ok(())
        }
        Err(_)
            if stage != ClientStage::Session
                && toggle.is_some_and(ConnectToggle::close_requested) =>
        {
            Ok(())
        }
        Err(cause) if cause.is::<ClientFailure>() => Err(cause),
        Err(cause) => Err(Box::new(ClientFailure { stage, cause })),
    }
}

/// Explicit direct-connection retry. SSH and saved profiles keep using the
/// single-session entry point. The budget counts additional sessions and never
/// resets, even when a replacement session runs successfully for a long time.
pub fn run_client_reconnecting(options: ClientOptions, attempts: u8) -> RemoteResult<()> {
    run_client_reconnecting_with_hardware(options, attempts, false)
}

pub fn run_client_reconnecting_with_hardware(
    options: ClientOptions,
    attempts: u8,
    hardware_h264: bool,
) -> RemoteResult<()> {
    let preferred = if hardware_h264 {
        FrameCodec::H264
    } else {
        FrameCodec::JpegTiles
    };
    run_client_reconnecting_with_video_codec(options, attempts, preferred)
}

pub fn run_client_reconnecting_with_video_codec(
    options: ClientOptions,
    attempts: u8,
    preferred: FrameCodec,
) -> RemoteResult<()> {
    reconnect_client(options, attempts, preferred, None, false)
}

/// Toggle one direct viewer per X11 screen and endpoint, or cycle its shared source.
pub fn run_client_toggling_with_video_codec(
    options: ClientOptions,
    attempts: u8,
    preferred: FrameCodec,
    next_source: bool,
) -> RemoteResult<()> {
    if options.viewer_backend != ViewerBackend::X11 {
        return Err(invalid_data("--toggle requires the X11 viewer").into());
    }
    let Some(toggle) =
        ConnectToggle::claim(options.display.as_deref(), &options.address, next_source)?
    else {
        return Ok(());
    };
    reconnect_client(options, attempts, preferred, Some(&toggle), false)
}

pub(super) fn reconnect_client(
    options: ClientOptions,
    attempts: u8,
    mut preferred: FrameCodec,
    toggle: Option<&ConnectToggle>,
    retain_x11_window: bool,
) -> RemoteResult<()> {
    if (options.follow_source_size
        || options.resize_source.is_some()
        || options.wayland_keyboard
        || options.wayland_control
        || options.wayland_text.is_some()
        || options.wayland_pointer
        || options.wayland_scroll
        || options.wayland_horizontal_scroll
        || options.viewer_backend == ViewerBackend::Wayland)
        && attempts != 0
    {
        return Err(invalid_data("automatic reconnect is unavailable for source resize, scoped input leases or the Wayland viewer").into());
    }
    if attempts > 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconnect attempts must be 0..=5",
        )
        .into());
    }
    let keep_window = attempts != 0
        && options.viewer_backend == ViewerBackend::X11
        && options.view_only
        && !options.clipboard
        && (retain_x11_window
            || std::env::var_os("JWM_REMOTE_KEEP_WINDOW").as_deref() == Some("1".as_ref()));
    let saved_window = std::cell::RefCell::new(None);
    let backoff_outcome = std::cell::RefCell::new(None);
    reconnect_with(
        attempts,
        || {
            if let Some(outcome) = backoff_outcome.borrow_mut().take() {
                return outcome;
            }
            let mut window = saved_window.borrow_mut();
            let outcome = run_client_attempt(
                options.clone(),
                preferred,
                toggle,
                keep_window.then_some(&mut *window),
            );
            if outcome.as_ref().err().is_some_and(|error| {
                error
                    .downcast_ref::<ClientFailure>()
                    .is_some_and(|failure| {
                        failure.stage == ClientStage::Negotiate
                            && failure.cause.is::<LegacyVideoProfileRefused>()
                    })
            }) {
                preferred = FrameCodec::JpegTiles;
                eprintln!("jwm-remote: host requires the JPEG stream profile; retrying with JPEG");
                return run_client_attempt(
                    options.clone(),
                    preferred,
                    toggle,
                    keep_window.then_some(&mut *window),
                );
            }
            outcome
        },
        |attempt, delay| {
            if toggle.is_some_and(|toggle| !toggle.begin_backoff()) {
                return;
            }
            eprintln!(
                "jwm-remote: transport lost; reconnect attempt {attempt}/{attempts} in {} seconds (Ctrl+C to stop)",
                delay.as_secs()
            );
            if let Some(window) = saved_window.borrow_mut().as_mut() {
                match wait_for_recovery_window(window, toggle, delay) {
                    Ok(true) => {}
                    Ok(false) => *backoff_outcome.borrow_mut() = Some(Ok(())),
                    Err(error) => *backoff_outcome.borrow_mut() = Some(Err(error)),
                }
            } else if let Some(toggle) = toggle {
                toggle.wait_to_retry(delay);
            } else {
                thread::sleep(delay);
            }
        },
    )
}

fn wait_for_recovery_window(
    window: &mut Viewer,
    toggle: Option<&ConnectToggle>,
    delay: Duration,
) -> RemoteResult<bool> {
    let deadline = Instant::now() + delay;
    loop {
        if toggle.is_some_and(ConnectToggle::close_requested)
            || !window.poll_disconnected_window()?
        {
            return Ok(false);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(true);
        }
        window.prepare_wait()?;
        super::x11_capture::wait_readable(
            window.connection_fd(),
            remaining.min(Duration::from_millis(25)),
        )?;
    }
}

/// Keep the retained X11 shell on this thread while the existing socket
/// deadlines cover the authenticated setup work on a scoped worker.
fn run_recovery_io<T: Send, S: Send>(
    viewer: Option<&mut Viewer>,
    connection: S,
    socket: impl FnOnce(&S) -> &TcpStream,
    operation: impl FnOnce(S) -> RemoteResult<T> + Send,
) -> RemoteResult<Option<T>> {
    let Some(viewer) = viewer else {
        return operation(connection).map(Some);
    };
    let cancel = socket(&connection).try_clone()?;
    let (wake, completion) = wake_pair()?;
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("jwm-remote-recovery-io".into())
            .spawn_scoped(scope, move || {
                let result = operation(connection);
                // Closing this end wakes the main poll even if setup failed.
                drop(completion);
                result
            })?;
        let ready: RemoteResult<bool> = (|| {
            loop {
                if !viewer.poll_disconnected_window()? {
                    return Ok(false);
                }
                if worker.is_finished() {
                    return Ok(true);
                }
                viewer.prepare_wait()?;
                if viewer.has_pending_events()? {
                    continue;
                }
                if wait_for_activity(
                    viewer.connection_fd(),
                    wake.as_fd(),
                    Instant::now() + Duration::from_millis(25),
                )? == WaitOutcome::ViewerHangup
                {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "X11 viewer connection closed during recovery",
                    )
                    .into());
                }
            }
        })();
        if !matches!(ready, Ok(true)) {
            let _ = cancel.shutdown(Shutdown::Both);
        }
        let result = worker
            .join()
            .map_err(|_| io::Error::other("viewer recovery I/O worker panicked"))?;
        if ready? { result.map(Some) } else { Ok(None) }
    })
}

fn reconnect_with(
    attempts: u8,
    mut run: impl FnMut() -> RemoteResult<()>,
    mut wait: impl FnMut(u8, Duration),
) -> RemoteResult<()> {
    let mut remaining = attempts;
    loop {
        match run() {
            Ok(()) => return Ok(()),
            Err(error) => {
                let certified = error
                    .downcast_ref::<ClientFailure>()
                    .is_some_and(|failure| failure.cause.is::<CleanTransportDisconnect>());
                if remaining == 0 || !certified {
                    return Err(error);
                }
                let attempt = attempts - remaining + 1;
                wait(attempt, Duration::from_secs(1 << (attempt - 1).min(3)));
                remaining -= 1;
            }
        }
    }
}

/// Constructed only at the established session's authenticated record reader.
/// It is not itself retry authorization: all session owners must first stop.
#[derive(Debug)]
struct ReceiverTransportLoss(ProtocolError);

/// Constructed only by finish_session after successful explicit teardown.
#[derive(Debug)]
struct CleanTransportDisconnect(ProtocolError);

macro_rules! transport_error {
    ($kind:ty) => {
        impl std::fmt::Display for $kind {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl std::error::Error for $kind {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
    };
}
transport_error!(ReceiverTransportLoss);
transport_error!(CleanTransportDisconnect);

fn receiver_read_error(error: ProtocolError) -> RemoteError {
    if matches!(&error, ProtocolError::Io(error) if matches!(error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted))
    {
        Box::new(ReceiverTransportLoss(error))
    } else {
        Box::new(error)
    }
}

fn suppress_reconnect(result: RemoteResult<()>, suppress: bool) -> RemoteResult<()> {
    result.map_err(|error| {
        if suppress {
            match error.downcast::<ReceiverTransportLoss>() {
                Ok(transport) => Box::new(transport.0) as RemoteError,
                Err(error) => strip_hardware_retry(error),
            }
        } else {
            error
        }
    })
}

fn validate_viewer_backend(options: &ClientOptions) -> RemoteResult<()> {
    if options.x11_parent_window.is_some()
        && (options.viewer_backend != ViewerBackend::X11
            || options.window_size.is_some()
            || options.window_position.is_some())
    {
        return Err(invalid_data("X11 embedding requires X11 and container-owned geometry").into());
    }
    if options.wayland_gestures
        && (!options.wayland_control
            || options.resumable_control
            || options.static_lossless
            || options.view_only
            || options.viewer_backend != ViewerBackend::Wayland)
    {
        return Err(
            invalid_data("Wayland gestures require one-shot managed Wayland control").into(),
        );
    }
    if options.control_duration.is_extended() && !options.wayland_seamless_input {
        return Err(invalid_data(
            "extended control duration requires explicit Wayland seamless input",
        )
        .into());
    }
    if options.wayland_seamless_input
        && (!options.resumable_control
            || !options.wayland_control
            || options.viewer_backend != ViewerBackend::Wayland
            || options.view_only
            || options.clipboard
            || options.grab_input
            || options.static_lossless)
    {
        return Err(invalid_data(
            "native seamless input requires explicit resumable combined Wayland control without clipboard, grabs or repair",
        ).into());
    }
    if options.resumable_control && (!options.wayland_control || options.static_lossless) {
        return Err(invalid_data(
            "resumable control requires explicit combined control without repair",
        )
        .into());
    }
    options.viewer_backend.validate_seamless_input(
        options.seamless_input,
        options.grab_input,
        options.view_only,
    )?;
    if options.wayland_popup_pointer
        && (options.viewer_backend != ViewerBackend::Wayland
            || options.view_only
            || options.wayland_keyboard
            || options.wayland_pointer
            || options.wayland_control
            || options.wayland_text.is_some()
            || options.wayland_scroll
            || options.wayland_horizontal_scroll
            || options.static_lossless
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some()
            || options.follow_source_size)
    {
        return Err(invalid_data(
            "popup pointer requires exclusive native Wayland viewer permission",
        )
        .into());
    }
    let lossless_keyboard = options.wayland_keyboard
        && options.viewer_backend == ViewerBackend::Wayland
        && !options.view_only;
    let lossless_control = options.wayland_control
        && options.viewer_backend == ViewerBackend::Wayland
        && !options.view_only
        && !options.wayland_keyboard
        && !options.wayland_pointer
        && !options.wayland_scroll
        && !options.wayland_horizontal_scroll;
    let lossless_pointer = options.wayland_pointer
        && options.viewer_backend == ViewerBackend::Wayland
        && !options.view_only
        && !options.wayland_keyboard;
    if options.static_lossless
        && (!(options.view_only || lossless_keyboard || lossless_pointer || lossless_control)
            || (options.wayland_pointer && !lossless_pointer)
            || ((options.wayland_scroll || options.wayland_horizontal_scroll) && !lossless_pointer)
            || (options.wayland_control && !lossless_control)
            || options.wayland_text.is_some()
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some()
            || options.follow_source_size)
    {
        return Err(invalid_data("static lossless requires view-only or exclusive Wayland keyboard, pointer or control mode without text, clipboard or remote resize; scroll requires exclusive pointer mode").into());
    }

    if options.wayland_text.is_some()
        && (options.viewer_backend != ViewerBackend::Wayland
            || options.view_only
            || options.wayland_control
            || options.wayland_keyboard
            || options.wayland_pointer
            || options.wayland_scroll
            || options.wayland_horizontal_scroll
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some()
            || options.follow_source_size)
    {
        return Err(invalid_data(
            "text requires exclusive native viewer mode without other input, clipboard or resize",
        )
        .into());
    }

    if options.wayland_control
        && (options.viewer_backend != ViewerBackend::Wayland
            || options.view_only
            || options.wayland_keyboard
            || options.wayland_pointer
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some()
            || options.follow_source_size)
    {
        return Err(invalid_data(
            "control requires exclusive native Wayland viewer without other permissions",
        )
        .into());
    }
    if (options.wayland_scroll || options.wayland_horizontal_scroll)
        && !options.wayland_pointer
        && !options.wayland_control
    {
        return Err(
            invalid_data("Wayland scroll requires explicit pointer or control mode").into(),
        );
    }
    if options.follow_source_size
        && (options.viewer_backend != ViewerBackend::X11
            || !options.view_only
            || options.resize_source.is_some()
            || options.clipboard
            || options.grab_input
            || options.wayland_keyboard
            || options.wayland_pointer)
    {
        return Err(invalid_data("follow-source-size requires X11 view-only without input, clipboard, source resize or grabs").into());
    }
    if options.viewer_backend == ViewerBackend::X11 && options.viewer_wayland_display.is_some() {
        return Err(
            invalid_data("--viewer-wayland-display requires Wayland viewer backend").into(),
        );
    }
    if options.viewer_backend == ViewerBackend::Wayland {
        if !super::capabilities::NATIVE_WAYLAND
            || usize::from(options.view_only)
                + usize::from(options.wayland_keyboard)
                + usize::from(options.wayland_pointer)
                + usize::from(options.wayland_control)
                + usize::from(options.wayland_text.is_some())
                + usize::from(options.wayland_popup_pointer)
                != 1
            || options.clipboard
            || options.resize_source.is_some()
            || options.grab_input
            || options.window_position.is_some()
            || options.display.is_some()
        {
            return Err(invalid_data("Wayland viewer requires exactly one of view-only, scoped Wayland keyboard, scoped Wayland pointer or explicit control, without clipboard, source resize, grab, X11 display or window positioning").into());
        }
    }
    Ok(())
}

/// Immutable for the entire connection, including after following stops.
/// Native resize owns geometry in follow mode: viewport-fit encoding would be
/// a second asynchronous geometry writer with no revision proof on the wire.
#[derive(Clone, Copy)]
enum ViewportPolicy {
    Fit,
    NativeSizeFollow,
}
impl ViewportPolicy {
    fn for_follow(follow: bool) -> Self {
        if follow {
            Self::NativeSizeFollow
        } else {
            Self::Fit
        }
    }
    fn initial(self, size: (u16, u16)) -> Option<(u16, u16)> {
        match self {
            Self::Fit => Some(size),
            Self::NativeSizeFollow => None,
        }
    }
    fn report(
        self,
        writer: &mut impl RecordWriter,
        previous: &mut Option<(u16, u16)>,
        size: (u16, u16),
    ) -> RemoteResult<bool> {
        if matches!(self, Self::NativeSizeFollow) || *previous == Some(size) {
            return Ok(false);
        }
        writer.write_message(MessageKind::Viewport, &encode_viewport(size.0, size.1))?;
        *previous = Some(size);
        Ok(true)
    }
}

fn requested_video_codec(options: &ClientOptions, preferred: FrameCodec) -> FrameCodec {
    if !preferred.is_hardware() {
        return FrameCodec::JpegTiles;
    }
    if (options.viewer_backend != ViewerBackend::X11 && !options.view_only)
        || options.static_lossless
        || options.wayland_keyboard
        || options.wayland_pointer
        || options.wayland_control
        || options.wayland_scroll
        || options.wayland_horizontal_scroll
        || options.wayland_popup_pointer
        || options.wayland_text.is_some()
    {
        eprintln!("jwm-remote: using JPEG for the requested viewer or proof mode");
        return FrameCodec::JpegTiles;
    }
    #[cfg(feature = "remote-h264")]
    match super::capabilities::probe_decoder(FrameCodec::H264) {
        Ok(_) => {
            if preferred == FrameCodec::Hevc444
                && super::capabilities::probe_decoder(FrameCodec::Hevc444).is_ok()
            {
                return FrameCodec::Hevc444;
            }
            return FrameCodec::H264;
        }
        Err(error) => {
            eprintln!("jwm-remote: H.264 hardware decoder unavailable, using JPEG: {error}")
        }
    }
    #[cfg(not(feature = "remote-h264"))]
    eprintln!("jwm-remote: this build uses JPEG; H.264 hardware support was disabled");
    FrameCodec::JpegTiles
}

fn run_client_session(
    options: ClientOptions,
    preferred: FrameCodec,
    stage: &mut ClientStage,
    toggle: Option<&ConnectToggle>,
    mut recovery_window: Option<&mut Option<Viewer>>,
) -> RemoteResult<()> {
    if let Some(viewer) = recovery_window.as_deref_mut().and_then(Option::as_mut)
        && !viewer.poll_disconnected_window()?
    {
        return Ok(());
    }
    if let Some(size) = options.window_size {
        size.validate()?;
    }
    if let Some(size) = options.resize_source {
        size.validate()?;
        if !options.view_only || options.clipboard || options.grab_input {
            return Err(invalid_data(
                "explicit source resize requires view-only without clipboard or input grab",
            )
            .into());
        }
    }
    validate_viewer_backend(&options)?;
    validate_escape_key(&options.escape_key)?;
    let key = options.key.load()?;
    if options.wayland_keyboard
        && (!cfg!(feature = "remote-wayland")
            || options.view_only
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some())
    {
        return Err(invalid_data("Wayland keyboard requires compiled support and excludes legacy input, clipboard, resize and grabs").into());
    }
    if options.wayland_pointer
        && (!cfg!(feature = "remote-wayland")
            || options.wayland_keyboard
            || options.view_only
            || options.clipboard
            || options.grab_input
            || options.resize_source.is_some())
    {
        return Err(invalid_data("scoped pointer requires exclusive explicit permission without clipboard, resize, grabs or other input").into());
    }
    let request_input = !options.view_only
        && !options.wayland_keyboard
        && !options.wayland_pointer
        && !options.wayland_control
        && options.wayland_text.is_none()
        && !options.wayland_popup_pointer;
    let keymap = if request_input {
        read_display(options.display.as_deref())?
    } else {
        KeymapTable::empty()
    };
    let (mut initial_viewport, request_cursor_plane) = match options.viewer_backend {
        ViewerBackend::X11 => (
            match options.x11_parent_window {
                Some(parent) => super::x11_viewer::initial_embedded_viewport(
                    options.display.as_deref(),
                    parent,
                )?,
                None => super::x11_viewer::initial_viewport_with_position(
                    options.display.as_deref(),
                    options.window_size,
                    options.window_position,
                )?,
            },
            super::x11_viewer::supports_cursor_plane(options.display.as_deref())?,
        ),
        ViewerBackend::Wayland => (
            super::viewer_backend::wayland_initial_size(options.window_size).dimensions(),
            false,
        ),
    };
    if let Some(viewer) = recovery_window.as_deref().and_then(Option::as_ref) {
        initial_viewport = viewer.viewport();
    }
    let requested_codec = requested_video_codec(&options, preferred);
    if toggle.is_some_and(ConnectToggle::close_requested) {
        return Ok(());
    }
    *stage = ClientStage::Connect;
    eprintln!("jwm-remote: connecting to {}", options.peer_label);
    let stream = connect_with_timeout(&options.address, CONNECT_TIMEOUT)?;
    if let Some(toggle) = toggle
        && !toggle.watch_startup(&stream)?
    {
        return Ok(());
    }
    *stage = ClientStage::Negotiate;
    eprintln!("jwm-remote: connection opened; authenticating and negotiating permissions");
    let popup_requested_at = Instant::now();
    #[cfg(feature = "remote-wayland")]
    let control_budget_started = Instant::now();
    let input_mode = if options.wayland_gestures {
        InputMode::WaylandControlWithGestures
    } else if options.wayland_control && toggle.is_some_and(|t| !t.owns_shortcut()) {
        InputMode::WaylandControlWithConfirmedRelease
    } else {
        InputMode::X11Keycodes
    };
    let Some((mut reader, writer, hello, correlation_id, application_profile)) = run_recovery_io(
        recovery_window.as_deref_mut().and_then(Option::as_mut),
        stream,
        |stream| stream,
        |stream| {
            negotiate_session(
                options.resize_source.is_some() || options.follow_source_size,
                options.wayland_keyboard,
                options.wayland_pointer,
                options.wayland_scroll,
                options.wayland_horizontal_scroll,
                options.wayland_control,
                input_mode,
                options.resumable_control,
                options.control_duration,
                options.wayland_text.is_some(),
                options.static_lossless,
                options.wayland_popup_pointer,
                stream,
                &key,
                request_input,
                options.clipboard,
                options.clipboard_file_limit,
                options.clipboard_mode,
                options.clipboard_direction,
                &keymap,
                ViewportPolicy::for_follow(options.follow_source_size).initial(initial_viewport),
                request_cursor_plane,
                options.frame_limits,
                requested_codec,
                HANDSHAKE_TIMEOUT,
            )
        },
    )?
    else {
        return Ok(());
    };
    if requested_codec.is_hardware() && hello.stream.codec != requested_codec {
        eprintln!(
            "jwm-remote: host selected {} video",
            hello.stream.codec.name()
        );
    }
    if application_profile == ApplicationProfile::VideoFeedback {
        eprintln!("jwm-remote: viewer rate feedback negotiated (application v36)");
    }
    if (options.resize_source.is_some() || options.follow_source_size)
        && !hello.stream.window_resize
    {
        return Err(invalid_data("host has not authorized native window resizing").into());
    }
    *stage = ClientStage::FirstFrame;
    eprintln!("jwm-remote: authenticated; waiting for the first desktop frame");
    if request_input && !(hello.pointer_enabled || hello.keyboard_enabled) {
        eprintln!("jwm-remote: host accepted a view-only session; input will not be forwarded");
    } else if request_input && !hello.keyboard_enabled {
        eprintln!(
            "jwm-remote: X11 keymaps differ; pointer works but keyboard is disabled \
             (the host terminal names the keys that differ)"
        );
    }

    // Its own X connection and thread, so a slow selection owner on either
    // desktop can never delay a frame or an input event.
    if options.clipboard && !hello.clipboard_enabled {
        eprintln!(
            "jwm-remote: clipboard sharing is off; check host consent and matching clipboard directions"
        );
    }
    if options.clipboard
        && options.clipboard_mode == ClipboardMode::All
        && options.clipboard_file_limit > 0
        && hello.clipboard_enabled
        && !hello.clipboard_files_enabled
    {
        eprintln!("jwm-remote: clipboard files are not enabled for this session");
    }
    let (mut wake_receiver, wake_sender) = wake_pair()?;
    let managed_clipboard = toggle
        .filter(|toggle| !toggle.owns_shortcut() && options.viewer_backend == ViewerBackend::X11);
    let (mut clipboard_sync, clipboard_inbox, clipboard_outbox) = if hello.clipboard_enabled {
        *stage = ClientStage::LocalClipboard;
        let clipboard_wake = Arc::new(wake_sender.try_clone()?);
        let record_wake = Arc::clone(&clipboard_wake);
        match ClipboardSync::start(
            options.display.as_deref(),
            SyncOptions {
                send: hello.stream.clipboard_directions & 1 != 0,
                receive: hello.stream.clipboard_directions & 2 != 0,
                images: hello.stream.clipboard_content & 2 != 0,
                files: hello.clipboard_files_enabled,
                file_limit: hello.stream.clipboard_file_limit,
                // What you copied just before connecting is what you most
                // likely mean to paste over there.
                share_existing: hello.stream.clipboard_directions & 1 != 0,
            },
            Some(Box::new(move || {
                let _ = record_wake.signal();
            })),
        ) {
            Ok((sync, inbox, outbox)) => {
                if managed_clipboard.is_some() {
                    sync.status().wake_on_progress(move || {
                        let _ = clipboard_wake.signal();
                    });
                }
                eprintln!(
                    "jwm-remote: clipboard sharing enabled ({})",
                    clipboard_policy::label(
                        hello.stream.clipboard_content,
                        hello.stream.clipboard_directions
                    )
                );
                (Some(sync), Some(inbox), Some(outbox))
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "negotiated clipboard sharing could not start: {error}"
                ))
                .into());
            }
        }
    } else {
        (None, None, None)
    };
    let clipboard_status = clipboard_sync.as_ref().map(ClipboardSync::status);

    *stage = ClientStage::FirstFrame;
    let mut initial_state = ReceiveState::new();
    initial_state.session_epoch = correlation_id;
    #[cfg(feature = "remote-h264")]
    let mut native = native_video::Presenter::configure(&options, hello.stream.codec);
    #[cfg(feature = "remote-h264")]
    {
        initial_state.native_video = native.as_ref().map(native_video::Presenter::decoder_config);
        initial_state.vaapi_direct_map = options.viewer_backend == ViewerBackend::X11
            && native.is_none()
            && std::env::var_os("JWM_REMOTE_VAAPI_DIRECT_MAP").as_deref() == Some("1".as_ref());
    }
    let engine_toggle = toggle.filter(|toggle| !toggle.owns_shortcut());
    let detailed_telemetry = options.stats_json || engine_toggle.is_some();
    initial_state.telemetry.detailed = detailed_telemetry;
    initial_state.read_ahead = (options.viewer_backend == ViewerBackend::X11 || options.view_only)
        && !hello.stream.wayland_keyboard
        && !hello.stream.wayland_pointer
        && !hello.stream.wayland_control
        && !hello.stream.wayland_text
        && !hello.stream.wayland_popup_pointer
        && std::env::var_os("JWM_REMOTE_READ_AHEAD").as_deref() == Some("1".as_ref());
    if initial_state.read_ahead {
        eprintln!("jwm-remote: authenticated reader and ordered decoder running independently");
    }
    initial_state
        .telemetry
        .expect_first_frame(1, Instant::now());
    if options.wayland_popup_pointer {
        initial_state
            .wayland_popup
            .get_mut()
            .unwrap()
            .enable(popup_requested_at);
    }
    initial_state.wayland_text.get_mut().unwrap().payload = options.wayland_text.clone();
    initial_state.frame_limits = hello.stream.frame_limits;
    initial_state.scroll_copy = hello.stream.scroll_copy;
    initial_state.static_lossless = hello.stream.static_lossless;
    initial_state.frame_codec = hello.stream.codec;
    initial_state.telemetry.video_feedback =
        application_profile == ApplicationProfile::VideoFeedback;
    initial_state.resize.get_mut().unwrap().requested = options.resize_source;
    if options.follow_source_size {
        initial_state.resize.get_mut().unwrap().follow =
            Some(super::follow_source_size::FollowSourceSize::default());
        eprintln!("jwm-remote: follow enabled; waiting for a presented viewport");
    }
    *initial_state.clipboard_restriction.get_mut().unwrap() = hello
        .clipboard_enabled
        .then(|| RestrictionState::new(hello.stream.clipboard_content));
    let state = Arc::new(initial_state);
    state.cursor_enabled.store(
        hello.stream.cursor == super::messages::CursorMode::Independent,
        Ordering::Release,
    );
    let mut telemetry_reporter = ClientTelemetryReporter::new(
        &state.telemetry,
        ViewerTelemetryContext {
            correlation_id,
            backend: options.viewer_backend,
            negotiated_codec: hello.stream.codec,
            limits: hello.stream.frame_limits,
            input_mode: negotiated_input_mode(&hello),
        },
        options.stats_json,
        engine_toggle,
    );
    let Some(payload) = run_recovery_io(
        recovery_window.as_deref_mut().and_then(Option::as_mut),
        &mut reader,
        |reader| reader.get_ref(),
        |reader| {
            read_initial_frame(
                reader,
                hello.clipboard_enabled,
                state.cursor_enabled.load(Ordering::Acquire),
                FIRST_FRAME_TIMEOUT,
                |kind, payload| {
                    if kind == MessageKind::SourceCatalog {
                        return receive_catalog(&state, payload);
                    }
                    if state.catalog.lock().unwrap().is_none() {
                        return Err(invalid_data("host record precedes source catalog").into());
                    }
                    if matches!(kind, MessageKind::CursorShape | MessageKind::CursorPosition) {
                        return receive_cursor(&state, kind, payload);
                    }
                    clipboard_inbox
                        .as_ref()
                        .ok_or_else(|| invalid_data("clipboard sharing was not negotiated"))?
                        .deliver(kind, payload)
                },
            )
        },
    )?
    else {
        return Ok(());
    };
    state.telemetry.record_received();
    // SessionReader exposes this payload only after its record MAC succeeds;
    // no decoded-image allocation is checked out before authentication.
    let mut video_decoder = state.new_video_decoder()?;
    let (initial_epoch, initial_purpose, initial_payload) = decode_frame_payload(&payload)?;
    if initial_epoch != 1 || state.catalog.lock().unwrap().is_none() {
        return Err(invalid_data("initial frame missing source catalog or initial epoch").into());
    }
    let mut first_frame = match decode_popup_queued_frame(
        &state,
        &mut video_decoder,
        initial_payload,
        initial_purpose,
        initial_epoch,
    ) {
        Ok(frame) => frame,
        Err(error) if error.is::<HardwareDecodeFailure>() => {
            // The first picture precedes worker/session cleanup. Explicitly
            // retire any clipboard worker before SSH can reuse this grant.
            let clipboard_cleanup = clipboard_sync
                .as_mut()
                .map_or(Ok(()), |sync| sync.shutdown_timeout(RECEIVER_JOIN_TIMEOUT));
            return include_local_cleanup(Err(error), clipboard_cleanup);
        }
        Err(error) => return Err(error),
    };
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    if let Some(observer) = &state.lossless_observer {
        observer.lock().unwrap().push((
            first_frame.frame.sequence(),
            video_decoder.jpeg.last_frame_was_lossless(),
        ));
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    if let Some(observer) = &state.purpose_observer {
        observer
            .lock()
            .unwrap()
            .push((first_frame.frame.sequence(), first_frame.purpose));
    }
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    if state.frame_codec == FrameCodec::JpegTiles
        && let Some(cpu) = first_frame.frame.cpu()
    {
        observe_decoded_scroll(
            &state.scroll_decode_observer,
            initial_payload,
            initial_epoch,
            cpu,
            video_decoder.jpeg.last_frame_was_lossless(),
        );
    }
    first_frame.frame = first_frame
        .frame
        .with_scope(state.session_epoch, initial_epoch);
    state
        .telemetry
        .record_frame_scope(&first_frame.frame, video_decoder.active_codec(&state));
    reader
        .get_ref()
        .set_read_timeout(Some(VIDEO_FRAME_IDLE_TIMEOUT))?;
    let (width, height) = first_frame.frame.dimensions();
    let initial_width = u16::try_from(width)
        .map_err(|_| invalid_data("initial frame is wider than an X11 window"))?;
    let initial_height = u16::try_from(height)
        .map_err(|_| invalid_data("initial frame is taller than an X11 window"))?;
    let last_sequence = first_frame.frame.sequence();
    *stage = ClientStage::Display;
    if toggle.is_some_and(ConnectToggle::close_requested) {
        return Ok(());
    }
    let mut viewer = if let Some(mut viewer) = recovery_window.as_deref_mut().and_then(Option::take)
    {
        if !viewer.resume_disconnected_window(current_source_label(&state))? {
            return Ok(());
        }
        viewer
    } else {
        Viewer::connect(
            &options,
            initial_width,
            initial_height,
            hello.pointer_enabled,
            hello.keyboard_enabled,
            hello.keyboard_enabled.then_some(keymap),
        )?
    };
    if recovery_window.is_some() {
        viewer.keep_window_on_retry();
    }
    if let Some(toggle) = toggle {
        if toggle.owns_shortcut() {
            viewer.enable_toggle_shortcut()?;
        }
        if !toggle.begin_viewing(&wake_sender)? {
            return Ok(());
        }
    }
    #[cfg(feature = "remote-wayland")]
    if options.wayland_control {
        let digest = viewer.enable_wayland_control(
            options.wayland_scroll,
            options.wayland_horizontal_scroll,
            hello.stream.input.managed_wayland(),
            options.wayland_gestures,
        )?;
        let mut control = state.wayland_control.lock().unwrap();
        control.digest = Some(digest);
        if hello.stream.resumable_control {
            use super::resumable_control::{LocalOptIn, Negotiated, VERSION};
            let permission = Negotiated::with_duration(
                LocalOptIn::Explicit,
                Some(VERSION),
                hello.stream.control_duration,
                options.control_duration,
            )?;
            control.configure_resumable(permission, control_budget_started)?;
        }
        control.seamless_enabled = options.wayland_seamless_input;
        control.repair.enabled = hello.stream.static_lossless;
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        control.capture_repair_observer();
        control.scroll_enabled = options.wayland_scroll;
        control.horizontal_enabled = options.wayland_horizontal_scroll;
        control.gestures = options.wayland_gestures;
    }
    #[cfg(feature = "remote-wayland")]
    if options.wayland_seamless_input {
        let (deadline, remaining_ms) = state
            .wayland_control
            .lock()
            .unwrap()
            .local_budget(Instant::now())?;
        viewer.enable_wayland_seamless_input(
            deadline,
            remaining_ms,
            options.control_duration.is_extended(),
        )?;
    }
    #[cfg(feature = "remote-wayland")]
    if options.wayland_keyboard {
        let digest = viewer.enable_wayland_keyboard(options.display.as_deref())?;
        state.wayland_keyboard.lock().unwrap().enable(digest);
    }
    if options.wayland_popup_pointer {
        viewer.enable_wayland_pointer(false, false)?;
    }
    if options.wayland_pointer {
        viewer.enable_wayland_pointer(options.wayland_scroll, options.wayland_horizontal_scroll)?;
        let mut pointer = state.wayland_pointer.lock().unwrap();
        pointer.enabled = true;
        pointer.repair.enabled = hello.stream.static_lossless;
        #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
        pointer.capture_repair_observer();
        pointer.repair_frame(
            initial_purpose,
            super::pointer_repair_phase::Scope {
                source: initial_epoch,
                geometry: first_frame.frame.geometry_epoch(),
                sequence: first_frame.frame.sequence(),
            },
        )?;
        pointer.scroll_enabled = options.wayland_scroll;
        pointer.horizontal_enabled = options.wayland_horizontal_scroll;
    }
    viewer.set_clipboard_status(hello.clipboard_enabled);
    viewer.enable_clipboard_narrowing(hello.clipboard_enabled);
    viewer.set_clipboard_policy(clipboard_policy::label(
        hello.stream.clipboard_content,
        hello.stream.clipboard_directions,
    ));
    let clipboard_snapshot = managed_clipboard.map(|controller| {
        let mut snapshot = ClipboardSnapshot::negotiated(
            hello.stream.clipboard_content,
            hello.stream.clipboard_directions,
            hello.stream.clipboard_file_limit,
        );
        snapshot.update_transfer(
            clipboard_status
                .as_deref()
                .and_then(SyncStatus::viewer_transfer),
        );
        controller.publish_clipboard(Some(snapshot));
        snapshot
    });
    viewer.set_source_label(current_source_label(&state));
    if state.cursor_enabled.load(Ordering::Acquire) {
        viewer.enable_cursor_plane()?;
        apply_pending_cursor(&mut viewer, &state, 1)?;
    }
    let control = writer.get_ref().try_clone()?;
    let sender_state = Arc::clone(&state);
    let mut writer = Sender::spawn(
        writer,
        hello.pointer_enabled || hello.keyboard_enabled,
        detailed_telemetry,
        move |kind, timing| {
            if let Some(timing) = timing {
                sender_state.telemetry.record_send(kind, timing);
            }
            if kind == MessageKind::FrameAck {
                sender_state.telemetry.record_acked();
            }
            if kind == MessageKind::ClipboardRestrict {
                let mut restriction = sender_state.clipboard_restriction.lock().unwrap();
                let result = restriction
                    .as_mut()
                    .ok_or_else(|| invalid_data("clipboard barrier without consent").into())
                    .and_then(|state| {
                        let request = state
                            .pending()
                            .ok_or_else(|| invalid_data("clipboard barrier without request"))?;
                        state.local_complete(request)
                    });
                if let Err(error) = result {
                    *sender_state.error.lock().unwrap() = Some(error);
                    sender_state.alive.store(false, Ordering::Release);
                }
            }
        },
    )?;
    if let Some(outbox) = &clipboard_outbox {
        writer.set_clipboard_policy(outbox.control().policy().shared_content())?;
    }
    let mut pending_presentations = std::collections::VecDeque::new();
    let first_revision = {
        let mut resize = state.resize.lock().unwrap();
        if let Some(follow) = resize.follow.as_mut() {
            follow.viewport(viewer.viewport());
            follow.revision()
        } else {
            0
        }
    };
    let mut cancel = SessionCancel::new(control, Arc::clone(&state));
    let mut preparation = None;
    let mut receiver = None;
    // The first submit can already schedule native allocation. Use session
    // cleanup for an initial draw or receiver-start failure too.
    let loop_result = (|| {
        if !submit_frame_with_revision(
            &mut viewer,
            first_frame,
            &mut pending_presentations,
            first_revision,
            &state,
            None,
            #[cfg(feature = "remote-h264")]
            native.as_mut(),
        )? {
            return Ok(ViewerLoopExit {
                close_release_flushed: false,
            });
        }

        *stage = ClientStage::Session;
        eprintln!(
            "jwm-remote: connected; {}; clipboard {}",
            if hello.stream.wayland_control {
                "scoped Wayland control pending local and host authorization"
            } else {
                negotiated_session_label(
                    hello.stream.wayland_popup_pointer,
                    hello.pointer_enabled && request_input,
                    hello.keyboard_enabled && request_input,
                )
            },
            clipboard_policy::label(
                hello.stream.clipboard_content,
                hello.stream.clipboard_directions
            )
        );
        if options.wayland_seamless_input {
            eprintln!(
                "jwm-remote: experimental JWM Wayland handoff: enter the remote image with all keys/buttons released to control; leave to release. After a black-bar exit, leave the whole surface before re-entering. Ctrl+Super+F12 ends control permanently; configured lock shortcuts stay local. The original approved budget is at most {} seconds, never renewed. Focus/geometry changes end control; Super is not forwarded and ends control. No drag or modifier-click.",
                options.control_duration.as_secs()
            );
        } else if options.seamless_input == Some(SeamlessInput::Hover) {
            eprintln!(
                "jwm-remote: seamless input: enter remote image to control, leave to release; Ctrl+Super+{} releases until you leave and enter again",
                options.escape_key
            );
        } else if options.seamless_input == Some(SeamlessInput::Click) {
            eprintln!(
                "jwm-remote: click the remote image to control. Move out of the image or press Ctrl+Super+{} to release input",
                options.escape_key
            );
        } else if options.grab_input && (hello.pointer_enabled || hello.keyboard_enabled) {
            eprintln!(
                "jwm-remote: click the viewer to control; Ctrl+Super+{} to release input",
                options.escape_key
            );
        }

        preparation = if options.viewer_backend == ViewerBackend::X11
            && matches!(hello.stream.codec, FrameCodec::H264 | FrameCodec::Hevc444)
            && std::env::var_os("JWM_REMOTE_X11_PREPARE").as_deref() == Some("1".as_ref())
        {
            match wake_sender
                .try_clone()
                .and_then(X11PreparationWorker::start)
            {
                Ok(worker) => Some(worker),
                Err(error) => {
                    eprintln!("jwm-remote: X11 CPU preparation worker unavailable ({error})");
                    None
                }
            }
        } else {
            None
        };
        let receive_state = Arc::clone(&state);
        #[cfg(test)]
        let cleanup_probe = cleanup_probe::current();
        receiver = Some(ReceiverThread::spawn(move || {
            // Move the authenticated first-frame allocation into the receiver
            // thread. It remains thread-local while avoiding a second large
            // allocation for the next frame.
            receive_frames(
                reader,
                receive_state,
                last_sequence,
                payload,
                wake_sender,
                video_decoder,
                clipboard_inbox,
            );
            #[cfg(test)]
            if let Some(probe) = cleanup_probe {
                probe.receiver_finished();
            }
        })?);

        let pointer_enabled = hello.pointer_enabled && !options.view_only;
        let keyboard_enabled = hello.keyboard_enabled && !options.view_only;
        viewer_loop(
            &mut viewer,
            &mut writer,
            &state,
            pointer_enabled,
            keyboard_enabled,
            &mut telemetry_reporter,
            &mut wake_receiver,
            clipboard_outbox.as_ref(),
            clipboard_status.as_deref(),
            hello.stream.clipboard_directions,
            clipboard_snapshot,
            &mut pending_presentations,
            toggle,
            preparation.as_mut(),
            #[cfg(feature = "remote-h264")]
            native.as_mut(),
        )
    })();
    if let Some(controller) = managed_clipboard {
        controller.publish_clipboard(None);
    }
    let graceful_close = matches!(
        &loop_result,
        Ok(ViewerLoopExit {
            close_release_flushed: true
        })
    );
    // Return local input authority before any transport/clipboard join. Capture the
    // failure instead of skipping the remaining mandatory cleanup steps.
    let local_cleanup = viewer.begin_local_cleanup();
    let (local_cleanup, (cancel_result, receiver_exit, sender_result, clipboard_result)) =
        run_remaining_cleanup(local_cleanup, || {
            let cancel_result = cancel.cancel(&mut writer, graceful_close);
            let receiver_exit = receiver.map_or(ReceiverExit::Completed, |receiver| {
                receiver.join_timeout(RECEIVER_JOIN_TIMEOUT)
            });
            let sender_result = writer.shutdown_timeout(RECEIVER_JOIN_TIMEOUT);
            let clipboard_result = clipboard_sync
                .as_mut()
                .map_or(Ok(()), |sync| sync.shutdown_timeout(RECEIVER_JOIN_TIMEOUT));
            (
                cancel_result,
                receiver_exit,
                sender_result,
                clipboard_result,
            )
        });
    telemetry_reporter.force_report();
    let result = finish_session(
        suppress_reconnect(
            loop_result.map(|_| ()),
            viewer.user_close_requested()
                || viewer.user_released_grab()
                || clipboard_was_restricted(&state, hello.stream.clipboard_content),
        ),
        cancel_result,
        receiver_exit,
        sender_result,
        clipboard_result,
    );
    let result = include_local_cleanup(result, local_cleanup);
    let result = include_local_cleanup(result, preparation.map_or(Ok(()), |worker| worker.close()));
    #[cfg(feature = "remote-h264")]
    let result = include_local_cleanup(
        result,
        native
            .as_mut()
            .map_or(Ok(()), native_video::Presenter::close_prefetch),
    );
    let result = finalize_retry_intent(result, || {
        if toggle.is_some_and(ConnectToggle::close_requested) {
            Ok(false)
        } else {
            viewer.reconnect_allowed_after_cleanup()
        }
    });
    if let Some(window) = recovery_window
        && result
            .as_ref()
            .is_err_and(|error| error.is::<CleanTransportDisconnect>())
    {
        if let Err(error) = viewer.freeze_disconnected_window() {
            return include_local_cleanup(result, Err(error));
        }
        *window = Some(viewer);
    }
    result
}

/// Reconnecting with the original CLI grant could silently undo a runtime
/// restriction. Once reduced, only a fresh explicit connect may renegotiate.
fn clipboard_was_restricted(state: &ReceiveState, initial: u8) -> bool {
    state
        .clipboard_restriction
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|restriction| restriction.content() != initial)
}

/// The old window remains visible while workers join. Consume local intent
/// that arrived during that interval before returning a retry certificate.
fn finalize_retry_intent(
    result: RemoteResult<()>,
    check: impl FnOnce() -> RemoteResult<bool>,
) -> RemoteResult<()> {
    match result {
        Err(error)
            if error.is::<CleanTransportDisconnect>() || error.is::<HardwareDecodeFailure>() =>
        {
            match check() {
                Ok(true) => Err(error),
                Ok(false) => Err(match error.downcast::<CleanTransportDisconnect>() {
                    Ok(transport) => Box::new(transport.0),
                    Err(error) => strip_hardware_retry(error),
                }),
                Err(cleanup) => Err(Box::new(SessionCleanupError {
                    #[cfg(test)]
                    sender_transport_only: false,
                    primary: Some(error),
                    failures: vec![cleanup],
                })),
            }
        }
        result => result,
    }
}

/// Explicit viewer closure remains terminal even when release cleanup fails.
#[derive(Debug)]
struct UserClosed;

impl std::fmt::Display for UserClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user closed the remote viewer")
    }
}
impl std::error::Error for UserClosed {}

fn user_close_cleanup_failure(error: RemoteError) -> RemoteError {
    Box::new(SessionCleanupError {
        #[cfg(test)]
        sender_transport_only: false,
        primary: Some(Box::new(UserClosed)),
        failures: vec![error],
    })
}

/// Any valid authenticated close, including a legacy empty reason, is terminal.
/// The code is separate from display-only, sanitized peer text.
#[derive(Debug)]
struct PeerClosed {
    code: Option<SessionCloseCode>,
    reason: String,
}

impl PeerClosed {
    fn legacy(payload: &[u8]) -> Self {
        Self {
            code: None,
            reason: refusal_reason_for_display(payload),
        }
    }

    fn decode(kind: MessageKind, payload: &[u8]) -> RemoteResult<Self> {
        match kind {
            MessageKind::Close => Ok(Self::legacy(payload)),
            MessageKind::SessionClose => {
                let close = SessionClose::decode(payload)?;
                Ok(Self {
                    code: Some(close.code),
                    reason: refusal_reason_for_display(close.reason.as_bytes()),
                })
            }
            _ => Err(invalid_data("not a host close message").into()),
        }
    }
}

impl std::fmt::Display for PeerClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host closed the remote session")?;
        if !self.reason.trim().is_empty() {
            write!(f, ": {}", self.reason)?;
        }
        Ok(())
    }
}
impl std::error::Error for PeerClosed {}

/// Preserve the first cause and every cleanup failure. In particular, a
/// timeout must not be hidden by a transport error that a caller might retry.
#[derive(Debug)]
struct SessionCleanupError {
    #[cfg(test)]
    sender_transport_only: bool,
    primary: Option<RemoteError>,
    failures: Vec<RemoteError>,
}
impl std::fmt::Display for SessionCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(primary) = &self.primary {
            write!(f, "{}; ", describe_cause(primary))?;
        }
        write!(f, "session cleanup incomplete")?;
        for failure in &self.failures {
            write!(f, "; {failure}")?;
        }
        Ok(())
    }
}
impl std::error::Error for SessionCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.primary.as_deref().map(|error| error as _)
    }
}

/// A local release error is data, not an early-return path around worker cleanup.
fn run_remaining_cleanup<T>(
    local: RemoteResult<()>,
    remaining: impl FnOnce() -> T,
) -> (RemoteResult<()>, T) {
    (local, remaining())
}

/// Preserve every completed cleanup result and never certify retry after a
/// local release failure. Existing worker failures remain visible together.
fn include_local_cleanup(result: RemoteResult<()>, local: RemoteResult<()>) -> RemoteResult<()> {
    let Err(local) = local else {
        return result;
    };
    let primary = match result {
        Err(error) => match error.downcast::<SessionCleanupError>() {
            Ok(mut cleanup) => {
                cleanup.failures.push(local);
                #[cfg(test)]
                {
                    cleanup.sender_transport_only = false;
                }
                return Err(cleanup);
            }
            Err(error) => Some(error),
        },
        Ok(()) => None,
    };
    Err(Box::new(SessionCleanupError {
        #[cfg(test)]
        sender_transport_only: false,
        primary,
        failures: vec![local],
    }))
}

fn finish_session(
    foreground: RemoteResult<()>,
    cancel: RemoteResult<()>,
    receiver: ReceiverExit,
    sender: RemoteResult<()>,
    clipboard: RemoteResult<()>,
) -> RemoteResult<()> {
    #[cfg(test)]
    let sender_transport_only = cancel.is_ok()
        && matches!(&receiver, ReceiverExit::Completed)
        && clipboard.is_ok()
        && sender.as_ref().err().is_some_and(|error| {
            let kind = error
                .downcast_ref::<super::sender::SenderShutdownTransportError>()
                .map(|e| e.kind);
            matches!(
                kind,
                Some(io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset)
            )
        });
    let mut failures = Vec::new();
    if let Err(error) = cancel {
        failures.push(error);
    }
    match receiver {
        ReceiverExit::Completed => {}
        ReceiverExit::ReaperFailed(error) => failures.push(
            io::Error::other(format!(
                "receiver join failed (worker may remain detached): {error}"
            ))
            .into(),
        ),
        ReceiverExit::Panicked => {
            failures.push(io::Error::other("remote video thread panicked").into())
        }
        ReceiverExit::TimedOut => failures.push(
            io::Error::new(
                io::ErrorKind::TimedOut,
                "remote video thread did not stop after session cancellation (detached)",
            )
            .into(),
        ),
    }
    if let Err(error) = sender {
        failures.push(error);
    }
    if let Err(error) = clipboard {
        failures.push(error);
    }
    if failures.is_empty() {
        foreground.map_err(|error| match error.downcast::<ReceiverTransportLoss>() {
            Ok(transport) => Box::new(CleanTransportDisconnect(transport.0)) as RemoteError,
            Err(error) => error,
        })
    } else {
        Err(Box::new(SessionCleanupError {
            #[cfg(test)]
            sender_transport_only,
            primary: foreground.err(),
            failures,
        }))
    }
}

fn negotiated_session_label(popup: bool, pointer: bool, keyboard: bool) -> &'static str {
    if popup {
        "popup pointer negotiated; awaiting presented-frame authorization"
    } else {
        session_mode(pointer, keyboard)
    }
}

fn validate_scroll_negotiation(
    pointer_requested: bool,
    scroll_requested: bool,
    scroll_granted: bool,
) -> RemoteResult<()> {
    if (scroll_requested && !pointer_requested) || scroll_granted != scroll_requested {
        return Err(invalid_data("Wayland scroll permission was not mutually negotiated").into());
    }
    Ok(())
}

fn requested_frame_stream(
    popup_pointer: bool,
    frame_limits: super::messages::FrameLimits,
    codec: FrameCodec,
) -> super::messages::StreamCapabilities {
    super::messages::StreamCapabilities {
        codec,
        scroll_copy: codec == FrameCodec::JpegTiles && !popup_pointer,
        frame_limits,
        ..Default::default()
    }
}

fn negotiate_session(
    request_window_resize: bool,
    request_wayland_keyboard: bool,
    request_wayland_pointer: bool,
    request_wayland_scroll: bool,
    request_wayland_horizontal_scroll: bool,
    request_wayland_control: bool,
    input_mode: InputMode,
    request_resumable_control: bool,
    requested_control_duration: super::resumable_control::ControlDuration,
    request_wayland_text: bool,
    request_static_lossless: bool,
    request_wayland_popup_pointer: bool,
    mut stream: TcpStream,
    key: &[u8],
    request_input: bool,
    request_clipboard: bool,
    clipboard_file_limit: u64,
    clipboard_mode: ClipboardMode,
    clipboard_direction: ClipboardDirection,
    keymap: &KeymapTable,
    initial_viewport: Option<(u16, u16)>,
    request_cursor_plane: bool,
    frame_limits: super::messages::FrameLimits,
    requested_codec: FrameCodec,
    negotiation_timeout: Duration,
) -> RemoteResult<(
    SessionReader<TcpStream>,
    SessionWriter<TcpStream>,
    ServerHello,
    u128,
    ApplicationProfile,
)> {
    frame_limits.validate()?;
    let requested_policy = clipboard_policy::requested(
        request_clipboard,
        clipboard_mode,
        clipboard_direction,
        clipboard_file_limit,
    );
    let request_clipboard_files = requested_policy.0 & 4 != 0;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(negotiation_timeout))?;
    stream.set_write_timeout(Some(negotiation_timeout))?;
    // Per-call socket timeouts can be extended indefinitely by a peer that
    // drips bytes.  Bound authentication and the versioned HelloAck together.
    let mut negotiation_deadline = TcpStreamDeadline::arm(&stream, negotiation_timeout)?;
    let session_keys = client_handshake(&mut stream, key, rand::random())
        .map_err(|error| explain_negotiation_eof(error, false))?;

    let reader_stream = stream.try_clone()?;
    let correlation_id = session_keys.correlation_id;
    let (receive_key, send_key) = session_keys.into_client();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    // The acknowledgement and a refusal are both tiny; frames come later.
    reader.set_max_payload_len(MAX_NEGOTIATION_PAYLOAD_LEN);
    let mut writer = SessionWriter::new(stream, send_key);
    let application_offer = if requested_codec.is_hardware()
        && !request_window_resize
        && std::env::var_os("JWM_REMOTE_ADAPT_FEEDBACK").as_deref() == Some("1".as_ref())
    {
        ApplicationProfile::VideoFeedback
    } else {
        ApplicationProfile::Standard
    };
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            stream: super::messages::StreamCapabilities {
                window_resize: request_window_resize,
                wayland_keyboard: request_wayland_keyboard,
                wayland_pointer: request_wayland_pointer,
                wayland_scroll: request_wayland_scroll,
                wayland_horizontal_scroll: request_wayland_horizontal_scroll,
                wayland_control: request_wayland_control,
                input: input_mode,
                resumable_control: request_resumable_control,
                control_duration: requested_control_duration,
                wayland_popup_pointer: request_wayland_popup_pointer,
                wayland_text: request_wayland_text,
                static_lossless: request_static_lossless,
                clipboard_content: requested_policy.0,
                clipboard_directions: requested_policy.1,
                clipboard_file_limit: if request_clipboard_files {
                    clipboard_file_limit
                } else {
                    0
                },
                cursor: if request_cursor_plane {
                    super::messages::CursorMode::Independent
                } else {
                    super::messages::CursorMode::Composited
                },
                ..requested_frame_stream(
                    request_wayland_popup_pointer,
                    frame_limits,
                    requested_codec,
                )
            },
            initial_viewport,
            request_input,
            request_clipboard,
            request_clipboard_files,
            keymap: keymap.clone(),
        }
        .encode_with_profile(application_offer),
    )?;
    writer.flush()?;
    let confirmed_release = input_mode.managed_wayland();
    let gestures = input_mode == InputMode::WaylandControlWithGestures;
    let (hello, application_profile) =
        read_server_hello(&mut reader, requested_codec, application_offer).map_err(|error| {
            if confirmed_release
                && error.downcast_ref::<PeerClosed>().is_some_and(|peer| {
                    peer.code.is_none() && peer.reason.as_bytes() == LEGACY_STREAM_PROFILE_REFUSAL
                })
            {
                return Box::new(ConfirmedReleaseUnavailable { gestures }) as RemoteError;
            }
            error
        })?;
    if hello.stream.input != input_mode {
        if confirmed_release {
            return Err(ConfirmedReleaseUnavailable { gestures }.into());
        }
        return Err(invalid_data("host did not confirm the requested input mode").into());
    }
    requested_codec.confirm(hello.stream.codec)?;
    validate_server_frame_limits(&hello, frame_limits)?;
    if hello.stream.static_lossless != request_static_lossless
        || (request_static_lossless
            && (hello.pointer_enabled
                || hello.keyboard_enabled
                || hello.clipboard_enabled
                || (hello.stream.wayland_control
                    && (!request_wayland_control
                        || hello.stream.wayland_keyboard
                        || hello.stream.wayland_pointer
                        || hello.stream.wayland_scroll
                        || hello.stream.wayland_horizontal_scroll))
                || hello.stream.wayland_text
                || hello.stream.window_resize
                || ((hello.stream.wayland_scroll || hello.stream.wayland_horizontal_scroll)
                    && (!request_wayland_pointer
                        || !hello.stream.wayland_pointer
                        || hello.stream.wayland_keyboard))))
    {
        return Err(invalid_data(
            "static lossless permission was not mutually negotiated exclusively",
        )
        .into());
    }

    hello
        .stream
        .confirm_control_duration(requested_control_duration, request_resumable_control)?;
    if hello.stream.resumable_control != request_resumable_control {
        return Err(invalid_data("resumable control was not mutually negotiated").into());
    }
    if hello.stream.wayland_control != request_wayland_control
        || (request_wayland_control
            && (hello.stream.wayland_keyboard
                || hello.stream.wayland_pointer
                || hello.stream.window_resize
                || hello.clipboard_enabled
                || hello.pointer_enabled
                || hello.keyboard_enabled))
    {
        return Err(
            invalid_data("control permission mismatch or mixed legacy capabilities").into(),
        );
    }

    if hello.stream.wayland_text != request_wayland_text
        || (request_wayland_text
            && (hello.pointer_enabled
                || hello.keyboard_enabled
                || hello.clipboard_enabled
                || hello.stream.window_resize
                || hello.stream.wayland_keyboard
                || hello.stream.wayland_pointer
                || hello.stream.wayland_control))
    {
        return Err(invalid_data(
            "one-shot text permission was not mutually negotiated exclusively",
        )
        .into());
    }
    validate_scroll_negotiation(
        request_wayland_pointer || request_wayland_control,
        request_wayland_scroll,
        hello.stream.wayland_scroll,
    )?;
    if (request_wayland_horizontal_scroll || hello.stream.wayland_horizontal_scroll)
        && (!(request_wayland_pointer || request_wayland_control)
            || request_wayland_horizontal_scroll != hello.stream.wayland_horizontal_scroll)
    {
        return Err(invalid_data(
            "Wayland horizontal scroll permission was not mutually negotiated",
        )
        .into());
    }
    if hello.stream.wayland_pointer != request_wayland_pointer
        || (request_wayland_pointer
            && (hello.stream.wayland_keyboard
                || hello.keyboard_enabled
                || hello.pointer_enabled
                || hello.clipboard_enabled
                || hello.stream.window_resize))
    {
        return Err(invalid_data(
            "scoped pointer permission mismatch or unsafe combined permissions",
        )
        .into());
    }
    if hello.stream.wayland_keyboard != request_wayland_keyboard {
        return Err(invalid_data("Wayland keyboard permission was not mutually negotiated").into());
    }
    if request_wayland_keyboard
        && (hello.pointer_enabled
            || hello.keyboard_enabled
            || hello.clipboard_enabled
            || hello.stream.window_resize)
    {
        return Err(invalid_data(
            "Wayland keyboard cannot include legacy input or other permissions",
        )
        .into());
    }
    if hello.stream.cursor == super::messages::CursorMode::Independent && !request_cursor_plane {
        return Err(invalid_data("host enabled independent cursor without viewer support").into());
    }
    if hello.stream.window_resize && !request_window_resize {
        return Err(invalid_data("host granted unrequested native resize capability").into());
    }
    if hello.stream.wayland_popup_pointer != request_wayland_popup_pointer
        || (request_wayland_popup_pointer
            && (hello.pointer_enabled
                || hello.keyboard_enabled
                || hello.clipboard_enabled
                || hello.stream.window_resize
                || hello.stream.static_lossless
                || hello.stream.wayland_keyboard
                || hello.stream.wayland_pointer
                || hello.stream.wayland_control
                || hello.stream.wayland_text
                || hello.stream.wayland_scroll
                || hello.stream.wayland_horizontal_scroll))
    {
        return Err(invalid_data("popup pointer permission was not mutually negotiated").into());
    }
    validate_server_capabilities(
        &hello,
        request_input,
        request_clipboard,
        request_clipboard_files,
    )?;
    validate_server_file_limit(&hello, clipboard_file_limit)?;
    validate_server_clipboard_policy(&hello, requested_policy)?;
    negotiation_deadline.cancel();
    reader.set_max_payload_len(MAX_PAYLOAD_LEN);
    reader
        .get_ref()
        .set_read_timeout(Some(FIRST_FRAME_TIMEOUT))?;
    writer.get_ref().set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok((reader, writer, hello, correlation_id, application_profile))
}

#[derive(Debug)]
struct LegacyVideoProfileRefused;

impl std::fmt::Display for LegacyVideoProfileRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host refused the hardware video stream profile before admission")
    }
}

impl std::error::Error for LegacyVideoProfileRefused {}

#[derive(Debug)]
struct ConfirmedReleaseUnavailable {
    gestures: bool,
}

impl std::fmt::Display for ConfirmedReleaseUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.gestures {
            f.write_str("update the host's jwm-remote to use managed Wayland gestures")
        } else {
            f.write_str("update the host's jwm-remote to use managed Wayland control")
        }
    }
}

impl std::error::Error for ConfirmedReleaseUnavailable {}

const LEGACY_STREAM_PROFILE_REFUSAL: &[u8] =
    b"unsupported remote codec, cursor or input mode; update jwm-remote on both machines together";

/// Read only authenticated records, accepting explicit terminal responses before
/// HelloAck as well as after negotiation. The phase remains in ClientFailure.
fn read_server_hello<R: io::Read>(
    reader: &mut SessionReader<R>,
    requested_codec: FrameCodec,
    offered: ApplicationProfile,
) -> RemoteResult<(ServerHello, ApplicationProfile)> {
    let (kind, payload) = reader
        .read_message()
        .map_err(|error| explain_negotiation_eof(error, true))?;
    match kind {
        // Pre-hardware hosts reject codec IDs in Hello before admitting a
        // session. Match their exact authenticated legacy response, not EOF,
        // authentication failures or any later refusal.
        MessageKind::Close
            if requested_codec.is_hardware() && payload == LEGACY_STREAM_PROFILE_REFUSAL =>
        {
            Err(LegacyVideoProfileRefused.into())
        }
        MessageKind::Close | MessageKind::SessionClose => {
            let mut close = PeerClosed::decode(kind, &payload)?;
            if close.reason.trim().is_empty() {
                close.reason = "host declined the session without a reason".to_owned();
            }
            Err(close.into())
        }
        MessageKind::HelloAck => ServerHello::decode_response(&payload, offered),
        _ => Err(invalid_data("host did not acknowledge remote session negotiation").into()),
    }
}

/// Say what a hang-up during negotiation most likely means.
///
/// The host closes without a word on a wrong key, and Rust words that as
/// `failed to fill whole buffer`, which reads like a local bug. Where the
/// connection was cut says more than the error does: before the host proved
/// it holds the key, the two key files most likely differ; after, the key is
/// right and something on the host's side ended the session.
fn explain_negotiation_eof(error: ProtocolError, host_proved_key: bool) -> RemoteError {
    match &error {
        ProtocolError::Io(io_error) if io_error.kind() == io::ErrorKind::UnexpectedEof => {
            let message = if host_proved_key {
                "host accepted the key but closed before acknowledging the session; \
                 its terminal says why (an older jwm-remote host also does this)"
            } else {
                "host closed the connection during authentication; the two key files \
                 most likely differ (compare `sha256sum` of the key on both machines)"
            };
            io::Error::new(io::ErrorKind::ConnectionAborted, message).into()
        }
        _ => error.into(),
    }
}

/// Largest record accepted before the host acknowledges the session.
const MAX_NEGOTIATION_PAYLOAD_LEN: usize = 1024;
/// Longest refusal reason shown to the user, in bytes.
const MAX_REFUSAL_REASON_BYTES: usize = 256;

/// The host's refusal reason as it may reach the terminal.
///
/// Any key holder can send any text here. Printed raw it could clear the
/// screen, forge `jwm-remote:` diagnostic lines, or drive terminal escape
/// sequences (OSC 52 clipboard writes, hyperlinks). Control characters are
/// shown escaped, like every other diagnostic value, and the text is capped.
fn refusal_reason_for_display(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut shown = String::new();
    for ch in text.chars() {
        let piece: String = if ch.is_control() {
            ch.escape_default().collect()
        } else {
            ch.to_string()
        };
        if shown.len() + piece.len() > MAX_REFUSAL_REASON_BYTES {
            shown.push('…');
            break;
        }
        shown.push_str(&piece);
    }
    shown
}

/// A peer can grant only capabilities the local user requested.
fn validate_server_frame_limits(
    hello: &ServerHello,
    offered: super::messages::FrameLimits,
) -> RemoteResult<()> {
    let selected = hello.stream.frame_limits;
    selected.validate()?;
    if selected.intersect(offered) != selected {
        return Err(invalid_data("host enlarged the viewer decoded frame limits").into());
    }
    Ok(())
}

fn validate_server_clipboard_policy(hello: &ServerHello, request: (u8, u8)) -> RemoteResult<()> {
    if hello.stream.clipboard_content & !request.0 != 0
        || hello.stream.clipboard_directions & !request.1 != 0
    {
        return Err(invalid_data("host enlarged clipboard content or direction consent").into());
    }
    Ok(())
}

fn validate_server_file_limit(hello: &ServerHello, clipboard_file_limit: u64) -> RemoteResult<()> {
    if hello.stream.clipboard_file_limit > clipboard_file_limit
        || (hello.clipboard_files_enabled && hello.stream.clipboard_file_limit == 0)
        || (!hello.clipboard_files_enabled && hello.stream.clipboard_file_limit != 0)
    {
        return Err(invalid_data("host acknowledged an invalid clipboard file limit").into());
    }
    Ok(())
}

fn validate_server_capabilities(
    hello: &ServerHello,
    request_input: bool,
    request_clipboard: bool,
    request_clipboard_files: bool,
) -> RemoteResult<()> {
    if hello.clipboard_enabled && !request_clipboard {
        return Err(invalid_data("host enabled clipboard sharing without client consent").into());
    }
    if hello.clipboard_files_enabled && !request_clipboard_files {
        return Err(invalid_data("host enabled file sharing without client consent").into());
    }
    if (hello.pointer_enabled || hello.keyboard_enabled) && !request_input {
        return Err(invalid_data("host enabled input without client consent").into());
    }
    Ok(())
}

/// Clipboard and video share the host writer and may arrive in either order.
/// One absolute budget covers all records, including a partial first frame.
fn read_initial_frame(
    reader: &mut SessionReader<TcpStream>,
    clipboard_enabled: bool,
    cursor_enabled: bool,
    timeout: Duration,
    deliver_clipboard: impl FnMut(MessageKind, &[u8]) -> RemoteResult<()>,
) -> RemoteResult<Vec<u8>> {
    let mut deadline = TcpStreamDeadline::arm(reader.get_ref(), timeout)?;
    let payload =
        read_initial_frame_records(reader, clipboard_enabled, cursor_enabled, deliver_clipboard)?;
    deadline.cancel();
    Ok(payload)
}

fn read_initial_frame_records<R: io::Read>(
    reader: &mut SessionReader<R>,
    clipboard_enabled: bool,
    cursor_enabled: bool,
    mut deliver_clipboard: impl FnMut(MessageKind, &[u8]) -> RemoteResult<()>,
) -> RemoteResult<Vec<u8>> {
    let mut payload = Vec::new();
    loop {
        match reader.read_message_into(&mut payload)? {
            MessageKind::Frame => return Ok(payload),
            MessageKind::SourceCatalog => deliver_clipboard(MessageKind::SourceCatalog, &payload)?,
            kind @ (MessageKind::Clipboard | MessageKind::ClipboardTransfer)
                if clipboard_enabled =>
            {
                deliver_clipboard(kind, &payload)?;
            }
            kind @ (MessageKind::CursorShape | MessageKind::CursorPosition) if cursor_enabled => {
                deliver_clipboard(kind, &payload)?;
            }
            kind @ (MessageKind::Close | MessageKind::SessionClose) => {
                let mut close = PeerClosed::decode(kind, &payload)?;
                if close.reason.trim().is_empty() {
                    close.reason = "host closed before sending an initial video frame".to_owned();
                }
                return Err(close.into());
            }
            _ => return Err(invalid_data("host did not send an initial video frame").into()),
        }
    }
}

fn release_managed_wayland_control(
    viewer: &mut Viewer,
    writer: &mut Sender,
    state: &ReceiveState,
    controller: &ConnectToggle,
) -> RemoteResult<bool> {
    let record = state
        .wayland_control
        .lock()
        .unwrap()
        .begin_managed_release(Instant::now());
    let Some(record) = record else {
        return Ok(false);
    };
    viewer.release_managed_wayland_control()?;
    writer.write_message(MessageKind::WaylandControlRevoke, &record.encode()?)?;
    writer.flush()?;
    controller.publish_wayland_input(super::engine::WaylandInputState::Releasing);
    Ok(true)
}

fn send_control_action(
    control: &mut ClientControl,
    writer: &mut Sender,
    action: super::wayland_control_wire::ControlAction,
    now: Instant,
) -> RemoteResult<()> {
    control.repair_sent(&action, now)?;
    if let Some(session) = control.resumable.as_mut() {
        let envelope = session.wrap_action(&action.encode()?, now)?;
        writer.write_message(MessageKind::WaylandResumableControl, &envelope.encode())?;
    } else {
        writer.write_message(MessageKind::WaylandControlAction, &action.encode()?)?;
    }
    writer.flush()?;
    Ok(())
}

fn viewer_loop(
    viewer: &mut Viewer,
    writer: &mut Sender,
    state: &ReceiveState,
    pointer_enabled: bool,
    keyboard_enabled: bool,
    telemetry_reporter: &mut ClientTelemetryReporter<'_>,
    wake: &mut WakeReceiver,
    clipboard: Option<&ClipboardOutbox>,
    clipboard_status: Option<&SyncStatus>,
    clipboard_directions: u8,
    mut clipboard_snapshot: Option<ClipboardSnapshot>,
    pending_presentations: &mut std::collections::VecDeque<PendingFrameTiming>,
    toggle: Option<&ConnectToggle>,
    mut preparation: Option<&mut X11PreparationWorker>,
    #[cfg(feature = "remote-h264")] mut native: Option<&mut native_video::Presenter>,
) -> RemoteResult<ViewerLoopExit> {
    let mut clipboard_status_seen = 0;
    let mut reported_control_generation = None;
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut input_batch = Vec::with_capacity(MAX_INPUT_BATCH_EVENTS);
    let mut input_payload = Vec::new();
    // Tell the host how much this window can actually show. Without it the
    // host encodes to a fixed width forever, so a small window receives detail
    // it immediately throws away.
    let mut reported_viewport = None;
    // Follow state is retained even when terminal; never restore dynamic
    // Viewport records after stop and race the final native request.
    let viewport_policy = ViewportPolicy::for_follow(state.resize.lock().unwrap().follow.is_some());
    let mut source_epoch = 1;
    let mut presented_scope = (1, 1);
    let mut switching = false;
    let mut requested_source: Option<(u64, super::source_session::SourceId)> = None;
    let mut source_ready = false;
    let source_cycle_available = state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|catalog| catalog.entries.len() > 1);
    let local_controller = toggle.filter(|toggle| !toggle.owns_shortcut());
    let fixed_control_geometry = viewer.managed_wayland_layout_generation().is_some();
    let managed_source_control =
        fixed_control_geometry && source_cycle_available && local_controller.is_some();
    if managed_source_control {
        local_controller.unwrap().enable_wayland_source_control();
    }
    let (managed_scroll, managed_horizontal, managed_gestures) = {
        let control = state.wayland_control.lock().unwrap();
        (
            control.scroll_enabled,
            control.horizontal_enabled,
            control.gestures,
        )
    };
    let mut used_control_sources =
        std::collections::BTreeSet::from([super::source_session::SourceId(1)]);
    let mut deferred_selection = None;
    let mut current_wayland_grant = super::source_session::SourceId(1);
    let mut last_presented = None;
    let mut rearm: Option<(Instant, bool)> = None;
    #[allow(unused_labels)] // Native control cancellation rejoins the close path.
    'viewer: loop {
        state.wayland_text.lock().unwrap().check()?;
        wake.drain()?;
        if toggle.is_some_and(ConnectToggle::close_requested) {
            viewer.request_close();
        }
        if managed_source_control
            && !viewer.user_close_requested()
            && let Some((epoch, target)) =
                local_controller.and_then(ConnectToggle::take_request_input)
        {
            if !source_ready
                || switching
                || epoch != source_epoch
                || target != current_wayland_grant
                || used_control_sources.contains(&target)
            {
                return Err(invalid_data("input request lost its presented Wayland source").into());
            }
            let digest = viewer.enable_wayland_control(
                managed_scroll,
                managed_horizontal,
                true,
                managed_gestures,
            )?;
            let mut control = state.wayland_control.lock().unwrap();
            *control = ClientControl::default();
            control.digest = Some(digest);
            control.scroll_enabled = managed_scroll;
            control.horizontal_enabled = managed_horizontal;
            control.gestures = managed_gestures;
            rearm = Some((Instant::now() + Duration::from_secs(30), false));
        }
        let mut release_input = local_controller.is_some_and(ConnectToggle::take_release_input);
        if let Some(controller) = local_controller {
            let input = state
                .wayland_control
                .lock()
                .unwrap()
                .managed_input()
                .or_else(|| {
                    (managed_source_control && source_ready && !switching).then(|| {
                        if used_control_sources.contains(&current_wayland_grant) {
                            super::engine::WaylandInputState::Unavailable
                        } else {
                            super::engine::WaylandInputState::Available
                        }
                    })
                });
            if let Some(input) = input {
                let releasing = std::mem::take(&mut release_input)
                    && !viewer.user_close_requested()
                    && release_managed_wayland_control(viewer, writer, state, controller)?;
                if !releasing {
                    controller.publish_wayland_input(input);
                }
            }
        }
        #[cfg(feature = "remote-wayland")]
        {
            let (seamless, may_enter) = {
                let control = state.wayland_control.lock().unwrap();
                (
                    control.seamless_enabled,
                    control.epoch.is_none()
                        || control.resumable.as_ref().is_some_and(|session| {
                            session.phase() == super::resumable_control::Phase::Paused
                        }),
                )
            };
            if seamless {
                let active = viewer.drive_wayland_handoff(may_enter)?;
                state.wayland_control.lock().unwrap().local_handoff_active = active;
            }
        }
        {
            let (configured, ended, epoch, activated) = {
                let mut control = state.wayland_control.lock().unwrap();
                control.check(presented_scope, Instant::now())?;
                (
                    control.configured(),
                    control.ended,
                    control.epoch,
                    control.activated,
                )
            };
            if configured {
                if ended {
                    if local_controller.is_some() {
                        viewer.release_managed_wayland_control()?;
                    } else {
                        #[cfg(feature = "remote-wayland")]
                        viewer.disable_wayland_control();
                    }
                } else if epoch.is_some() && !activated && viewer.wayland_handoff_active() {
                    // Do not hold the shared state while a real compositor fence waits.
                    #[cfg(feature = "remote-wayland")]
                    {
                        if viewer.check_wayland_control(true)? {
                            let mut control = state.wayland_control.lock().unwrap();
                            control.check(presented_scope, Instant::now())?;
                            if !control.ended && control.epoch == epoch && !control.activated {
                                control.activated = true;
                                if control.gestures {
                                    eprintln!(
                                        "jwm-remote: control ready for Wayland modifier clicks and dragging"
                                    );
                                } else {
                                    eprintln!(
                                        "jwm-remote: control ready for sequential typing and pointer actions"
                                    );
                                }
                            }
                        } else if viewer.user_close_requested() {
                            continue 'viewer;
                        }
                    }
                }
            }
        }

        {
            let popup = state.wayland_popup.lock().unwrap();
            if popup.enabled {
                popup.check(Instant::now())?;
                viewer.set_wayland_pointer_status(
                    if popup.ready_sequence(Instant::now()).is_some() {
                        "Popup pointer: Ready (one click)"
                    } else {
                        "Popup pointer: Waiting for presented frame"
                    },
                );
                #[cfg(test)]
                viewer.observe_pointer_ready(popup.ready_sequence(Instant::now()))?;
            }
        }
        state.resize.lock().unwrap().check_timeout(Instant::now())?;
        {
            let mut keyboard = state.wayland_keyboard.lock().unwrap();
            keyboard.check_timeout(Instant::now())?;
            if keyboard.configured() && keyboard.ended {
                #[cfg(feature = "remote-wayland")]
                viewer.disable_wayland_keyboard();
            } else if keyboard.epoch.is_some() && !keyboard.activated {
                #[cfg(feature = "remote-wayland")]
                viewer.check_wayland_keyboard(true)?;
                keyboard.activated = true;
                eprintln!(
                    "jwm-remote: Wayland keyboard ready; you may type (fixed source; no automatic renewal)"
                );
            }
            #[cfg(test)]
            if keyboard.configured() {
                viewer.observe_keyboard_state(
                    keyboard.activated
                        && keyboard.epoch.is_some()
                        && keyboard.request.is_some()
                        && !keyboard.ended,
                    keyboard.ended,
                )?;
            }
        }
        {
            let mut pointer = state.wayland_pointer.lock().unwrap();
            if pointer.enabled {
                let now = Instant::now();
                pointer.repair_check(now)?;
                if let Some(revoke) = pointer.take_expiry_revoke() {
                    writer.write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                    writer.flush()?;
                } else if pointer.expired(now) {
                    #[cfg(test)]
                    eprintln!("viewer pointer revoke: lease expiry");
                    if let Some(revoke) = pointer.revoke() {
                        writer
                            .write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                        writer.flush()?;
                    } else {
                        return Err(
                            invalid_data("pointer lease expired before authorization").into()
                        );
                    }
                }
                viewer.set_wayland_pointer_status(pointer.label(Instant::now()));
                #[cfg(test)]
                viewer.observe_pointer_candidate(
                    pointer.click.map(|(scope, _, _, _)| scope.frame_sequence),
                    pointer.left_down,
                    pointer.sequence,
                )?;
                #[cfg(test)]
                viewer.observe_scroll(pointer.scroll_observed)?;
                #[cfg(test)]
                viewer.observe_pointer_ready(if pointer.fresh(Instant::now()) {
                    pointer.ready.map(|scope| scope.frame_sequence)
                } else {
                    None
                })?;
            }
        }
        let received_epoch = state.source_epoch.load(Ordering::Acquire);
        if received_epoch != source_epoch {
            if let Some(toggle) = toggle {
                toggle.invalidate_sources();
            }
            if let Some(follow) = state.resize.lock().unwrap().follow.as_mut() {
                follow.stop("source changed");
            }
            let mut pointer = state.wayland_pointer.lock().unwrap();
            if pointer.enabled && !pointer.ended {
                #[cfg(test)]
                eprintln!("viewer pointer revoke: source epoch change");
                if let Some(revoke) = pointer.revoke() {
                    writer.write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                    writer.flush()?;
                } else {
                    return Err(invalid_data("pointer source changed before grant").into());
                }
            }
            drop(pointer);
            state
                .wayland_keyboard
                .lock()
                .unwrap()
                .check_scope((received_epoch, presented_scope.1))?;
            let selection = requested_source.take();
            let explicitly_selected = selection.is_some_and(|(epoch, id)| {
                epoch.checked_add(1) == Some(received_epoch)
                    && state
                        .catalog
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|catalog| {
                            catalog.current.epoch == received_epoch && catalog.current.source == id
                        })
            });
            source_epoch = received_epoch;
            switching = true;
            pending_presentations.clear();
            viewer.confirm_source_change(current_source_label(state), explicitly_selected)?;
            if managed_source_control && explicitly_selected {
                current_wayland_grant = selection.unwrap().1;
                *state.wayland_control.lock().unwrap() = ClientControl::default();
                last_presented = None;
            }
            write_source_release(writer, presented_scope, pointer_enabled || keyboard_enabled)?;
        }
        // Snapshot only: the native Ready transition may synchronize with the
        // local compositor. Never hold the receive-thread lease mutex across it.
        let scroll_ready = {
            let pointer = state.wayland_pointer.lock().unwrap();
            if pointer.enabled && (pointer.scroll_enabled || pointer.horizontal_enabled) {
                Some(if pointer.fresh(Instant::now()) {
                    pointer.ready.map(|scope| scope.frame_sequence)
                } else {
                    None
                })
            } else {
                None
            }
        };
        let scroll_ready = scroll_ready.or_else(|| {
            let control = state.wayland_control.lock().unwrap();
            (control.configured() && (control.scroll_enabled || control.horizontal_enabled))
                .then(|| control.ready_sequence())
        });
        if let Some(ready) = scroll_ready {
            viewer.wayland_pointer_ready(ready)?;
        }
        // A receive-thread ACK cannot retroactively authorize a batch that
        // was collected before its resumed generation/local validation.
        let polled_control_generation = state
            .wayland_control
            .lock()
            .unwrap()
            .active_input_generation();
        let mut events = match viewer.poll_events() {
            Ok(events) => events,
            Err(error) if viewer.user_close_requested() => {
                return Err(user_close_cleanup_failure(error));
            }
            Err(error) => {
                if viewer.managed_wayland_focus_left()
                    && let Some(controller) = local_controller
                    && release_managed_wayland_control(viewer, writer, state, controller)?
                {
                    Vec::new()
                } else {
                    return Err(error);
                }
            }
        };
        let polled_layout = viewer.managed_wayland_layout_generation();
        if let Some((deadline, prepared)) = rearm
            && !events.contains(&ViewerEvent::Close)
        {
            let granted = state.wayland_control.lock().unwrap().activated;
            if granted {
                rearm = None;
            } else if Instant::now() >= deadline {
                if prepared {
                    return Err(invalid_data("Wayland replacement input proof timed out").into());
                }
                *state.wayland_control.lock().unwrap() = ClientControl::default();
                viewer.release_managed_wayland_control()?;
                rearm = None;
                events.clear();
            } else if !prepared && viewer.managed_wayland_focus_ready() {
                let scope = last_presented.ok_or_else(|| {
                    invalid_data("Wayland request without a current presentation")
                })?;
                writer.write_message(
                    MessageKind::WaylandControlPrepare,
                    &super::wayland_control_wire::encode_prepare(scope)?,
                )?;
                writer.flush()?;
                used_control_sources.insert(current_wayland_grant);
                rearm = Some((
                    Instant::now() + super::wayland_control_wire::MANAGED_PREPARE_TIMEOUT,
                    true,
                ));
            }
        }
        if let Some(toggle) = local_controller {
            if release_input && !events.contains(&ViewerEvent::Close) {
                events.clear();
                viewer.release_managed_input(&mut events)?;
            }
            toggle.publish_input(viewer.managed_input_grab());
        }
        if reported_viewport.is_some_and(|previous| previous != viewer.viewport()) {
            if state.wayland_popup.lock().unwrap().enabled {
                return Err(invalid_data("popup viewer viewport changed").into());
            }
            let mut pointer = state.wayland_pointer.lock().unwrap();
            if pointer.enabled && !pointer.ended {
                #[cfg(test)]
                eprintln!(
                    "viewer pointer revoke: viewport change {:?} -> {:?}",
                    reported_viewport,
                    viewer.viewport()
                );
                if let Some(revoke) = pointer.revoke() {
                    writer.write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                    writer.flush()?;
                } else if pointer.requested.is_some() {
                    return Err(invalid_data(
                        "pointer viewport changed while authorization pending",
                    )
                    .into());
                }
            }
        }
        {
            {
                let mut control = state.wayland_control.lock().unwrap();
                if control.seamless_enabled {
                    control.local_handoff_active = viewer.wayland_handoff_active();
                }
            }
            let check_local = {
                let control = state.wayland_control.lock().unwrap();
                control.configured()
                    && control.accepts_input()
                    && !events.contains(&ViewerEvent::Close)
            };
            if check_local {
                #[cfg(feature = "remote-wayland")]
                match viewer.check_wayland_control(false) {
                    Ok(true) => {}
                    Ok(false) => {
                        events.clear();
                        events.push(ViewerEvent::Close);
                    }
                    Err(error) => {
                        if viewer.managed_wayland_focus_left()
                            && let Some(controller) = local_controller
                            && release_managed_wayland_control(viewer, writer, state, controller)?
                        {
                            events.clear();
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            let validate_resume = events.is_empty()
                && viewer.wayland_handoff_active()
                && state
                    .wayland_control
                    .lock()
                    .unwrap()
                    .resumable_local_validation_pending();
            if validate_resume {
                // The fresh same-connection fence must observe clean local
                // focus and held state after ACK, not merely before Resume.
                #[cfg(feature = "remote-wayland")]
                if !viewer.check_wayland_control(true)? {
                    continue 'viewer;
                }
                #[cfg(not(feature = "remote-wayland"))]
                return Err(invalid_data("native resume validation unavailable").into());
                #[cfg(feature = "remote-wayland")]
                state
                    .wayland_control
                    .lock()
                    .unwrap()
                    .complete_resumable_local_validation(Instant::now())?;
            }
            if polled_layout.is_some()
                && polled_layout != viewer.managed_wayland_layout_generation()
            {
                // The compositor fence above may have accepted a configure
                // since polling. Do not send coordinates from the old layout.
                events.retain(|event| {
                    !matches!(
                        event,
                        ViewerEvent::WaylandPointer { .. }
                            | ViewerEvent::WaylandPointerCancel
                            | ViewerEvent::WaylandPointerButtonCancel { .. }
                    )
                });
            }
            let mut control = state.wayland_control.lock().unwrap();
            // Both local protocol fences above may dispatch a late leave. Never
            // authorize the polled batch using the pre-fence local snapshot.
            if control.seamless_enabled {
                control.local_handoff_active = viewer.wayland_handoff_active();
            }
            control.check(presented_scope, Instant::now())?;
            // Observable status only: this does not grant input or change a
            // deadline. Report after the receiver ACK, clean local fence and
            // fresh presented proof all agree, once per active generation.
            if let Some(generation) = control.ready_input_generation()
                && reported_control_generation != Some(generation)
            {
                eprintln!(
                    "jwm-remote: control generation {generation} ready; original deadline unchanged"
                );
                reported_control_generation = Some(generation);
            }
            if control.configured() && control.managed_release_deadline().is_none() {
                // Terminal local intent goes straight to revoke below. Never
                // race a new Pause ACK against close cleanup or resume on exit.
                if control.seamless_enabled
                    && !events.iter().any(|event| {
                        matches!(
                            event,
                            ViewerEvent::ReleaseAll
                                | ViewerEvent::Close
                                | ViewerEvent::CycleSource
                                | ViewerEvent::WaylandPointerRevoke
                        )
                    })
                {
                    let phase = control.resumable.as_ref().map(|session| session.phase());
                    let transition = if !control.local_handoff_active
                        && phase == Some(super::resumable_control::Phase::Active)
                    {
                        Some(false)
                    } else if control.local_handoff_active
                        && events.is_empty()
                        && control.resumable_resume_proof_ready()
                    {
                        Some(true)
                    } else {
                        None
                    };
                    if let Some(resume) = transition {
                        #[cfg(feature = "remote-wayland")]
                        if resume {
                            if !viewer.check_wayland_control(true)? {
                                continue 'viewer;
                            }
                        }
                        if resume && !viewer.wayland_handoff_active() {
                            return Err(
                                invalid_data("local handoff changed during resume fence").into()
                            );
                        }
                        let record = control.resumable_transition(resume, Instant::now())?;
                        writer.write_message(
                            MessageKind::WaylandResumableControl,
                            &record.encode()?,
                        )?;
                        writer.flush()?;
                    }
                }
                // Test driver selects only a real production protocol transition.
                // It cannot forge focus, presentation, native cleanup or ACKs.
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(driver) = &state.resumable_test_driver {
                    let mut commands = driver.commands.lock().unwrap();
                    if let Some(&resume) = commands.front() {
                        if !resume
                            || (events.is_empty()
                                && viewer.wayland_control_clean_for_resume()
                                && control.resumable_resume_proof_ready())
                        {
                            commands.pop_front();
                            let record = control.resumable_transition(resume, Instant::now())?;
                            writer.write_message(
                                MessageKind::WaylandResumableControl,
                                &record.encode()?,
                            )?;
                            writer.flush()?;
                        }
                    }
                }
                if events.contains(&ViewerEvent::WaylandDragRelease { cancelled: true }) {
                    control.discard_drag_motion();
                }
                for event in &events {
                    if matches!(
                        event,
                        ViewerEvent::ReleaseAll
                            | ViewerEvent::Close
                            | ViewerEvent::CycleSource
                            | ViewerEvent::WaylandPointerRevoke
                    ) {
                        if let Some(revoke) = control.revoke_local() {
                            writer.write_message(
                                MessageKind::WaylandControlRevoke,
                                &revoke.encode()?,
                            )?;
                            writer.flush()?;
                        }
                        #[cfg(feature = "remote-wayland")]
                        viewer.disable_wayland_control();
                    } else {
                        // Polling still updates the local seat's held state while
                        // paused, but local releases must not be forwarded after
                        // the native pause already released their remote pair.
                        if !control.resumable_input_active()
                            || (rearm.is_some() && !control.activated)
                            || (control.resumable_enabled
                                && polled_control_generation != control.active_input_generation())
                        {
                            continue;
                        }
                        let now = Instant::now();
                        if matches!(
                            event,
                            ViewerEvent::Key { .. }
                                | ViewerEvent::WaylandDragRelease { cancelled: false }
                        ) && let Some(motion) = control.take_drag_motion(now, true)?
                        {
                            send_control_action(&mut control, writer, motion, now)?;
                        }
                        let action = match control.event(event, now) {
                            Ok(action) => action,
                            Err(error) => {
                                // A real input during a closed repair phase is
                                // terminal, never retained for later replay.
                                if let Some(revoke) = control.revoke() {
                                    writer.write_message(
                                        MessageKind::WaylandControlRevoke,
                                        &revoke.encode()?,
                                    )?;
                                    writer.flush()?;
                                }
                                return Err(error);
                            }
                        };
                        if let Some(action) = action {
                            send_control_action(&mut control, writer, action, now)?;
                        }
                    }
                }
                let now = Instant::now();
                if let Some(motion) = control.take_drag_motion(now, false)? {
                    send_control_action(&mut control, writer, motion, now)?;
                }
                if managed_gestures && let Some(controller) = local_controller {
                    controller.publish_wayland_gesture_ready(control.ready_sequence());
                }
                if let Some((kind, token)) =
                    control.repair_outgoing(Instant::now(), viewer.wayland_control_repair_busy())?
                {
                    writer.write_message(kind, &token.encode()?)?;
                    writer.flush()?;
                }
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                viewer.observe_control_state(control.ready_sequence());
                events.retain(|event| matches!(event, ViewerEvent::Close));
            }
        }
        {
            let mut keyboard = state.wayland_keyboard.lock().unwrap();
            if keyboard.configured() {
                #[cfg(feature = "remote-wayland")]
                if !keyboard.ended {
                    viewer.check_wayland_keyboard(false)?;
                }
                let revoke = events.iter().any(|event| {
                    matches!(
                        event,
                        ViewerEvent::ReleaseAll | ViewerEvent::Close | ViewerEvent::CycleSource
                    )
                });
                if revoke {
                    if let Some(record) = keyboard.cancel_for_local_event()? {
                        writer
                            .write_message(MessageKind::WaylandKeyboardRevoke, &record.encode()?)?;
                        writer.flush()?;
                    }
                    #[cfg(feature = "remote-wayland")]
                    viewer.disable_wayland_keyboard();
                } else {
                    for event in &events {
                        if let ViewerEvent::Key { keycode, pressed } = event
                            && let Some(key) = keyboard.key(*keycode, *pressed)?
                        {
                            writer
                                .write_message(MessageKind::WaylandKeyboardKey, &key.encode()?)?;
                        }
                    }
                }
                #[cfg(test)]
                viewer.observe_keyboard_state(
                    keyboard.activated
                        && keyboard.epoch.is_some()
                        && keyboard.request.is_some()
                        && !keyboard.ended,
                    keyboard.ended,
                )?;
                // These events never enter legacy InputBatch or pointer paths.
                events.retain(|event| matches!(event, ViewerEvent::Close));
            }
        }
        {
            let mut popup = state.wayland_popup.lock().unwrap();
            if popup.enabled {
                popup.check(Instant::now())?;
                if events.iter().any(|event| {
                    matches!(
                        event,
                        ViewerEvent::WaylandPointerRevoke
                            | ViewerEvent::Close
                            | ViewerEvent::ReleaseAll
                            | ViewerEvent::CycleSource
                    )
                }) {
                    if let Some(binding) = popup.revoke() {
                        writer
                            .write_message(MessageKind::WaylandPopupRevoke, &binding.encode()?)?;
                        writer.flush()?;
                    }
                    if !events
                        .iter()
                        .any(|event| matches!(event, ViewerEvent::Close))
                    {
                        return Err(invalid_data("popup pointer local authority revoked").into());
                    }
                } else {
                    for event in &events {
                        if let Some(click) = popup.event(event, Instant::now())? {
                            writer
                                .write_message(MessageKind::WaylandPopupClick, &click.encode()?)?;
                            writer.flush()?;
                        }
                    }
                }
                events.retain(|event| matches!(event, ViewerEvent::Close));
            }
        }
        {
            let mut pointer = state.wayland_pointer.lock().unwrap();
            if pointer.enabled {
                if events.iter().any(|e| {
                    matches!(
                        e,
                        ViewerEvent::WaylandPointerRevoke
                            | ViewerEvent::Close
                            | ViewerEvent::ReleaseAll
                            | ViewerEvent::CycleSource
                    )
                }) {
                    #[cfg(test)]
                    eprintln!("viewer pointer revoke: local event batch {events:?}");
                    if let Some(revoke) = pointer.revoke_local() {
                        writer
                            .write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                        writer.flush()?;
                    } else if pointer.requested.is_some() && pointer.epoch.is_none() {
                        return Err(
                            invalid_data("pointer authorization cancelled while pending").into(),
                        );
                    }
                } else {
                    for event in &events {
                        if let Some(action) = pointer.event(event, Instant::now())? {
                            if !pointer.repair_sent(&action, Instant::now())? {
                                continue;
                            }
                            if let Err(error) = (|| -> RemoteResult<()> {
                                writer.write_message(
                                    MessageKind::WaylandPointerAction,
                                    &action.encode()?,
                                )?;
                                writer.flush()?;
                                Ok(())
                            })() {
                                pointer.revoke();
                                return Err(error);
                            }
                        }
                    }
                }
                if let Some((kind, token)) =
                    pointer.repair_outgoing(Instant::now(), viewer.wayland_pointer_repair_busy())?
                {
                    if let Err(error) = (|| -> RemoteResult<()> {
                        writer.write_message(kind, &token.encode()?)?;
                        writer.flush()?;
                        Ok(())
                    })() {
                        pointer.revoke();
                        return Err(error);
                    }
                }
                viewer.set_wayland_pointer_status(pointer.label(Instant::now()));
                #[cfg(test)]
                viewer.observe_pointer_candidate(
                    pointer.click.map(|(scope, _, _, _)| scope.frame_sequence),
                    pointer.left_down,
                    pointer.sequence,
                )?;
                #[cfg(test)]
                viewer.observe_scroll(pointer.scroll_observed)?;
                #[cfg(test)]
                viewer.observe_pointer_ready(if pointer.fresh(Instant::now()) {
                    pointer.ready.map(|scope| scope.frame_sequence)
                } else {
                    None
                })?;
                events.retain(|event| matches!(event, ViewerEvent::Close));
            }
        }
        if events.contains(&ViewerEvent::NarrowClipboard) && !events.contains(&ViewerEvent::Close) {
            let mut restriction = state.clipboard_restriction.lock().unwrap();
            if let Some(restriction) = restriction.as_mut()
                && let Some(request) = restriction.begin_next(Instant::now())?
            {
                let clipboard =
                    clipboard.ok_or_else(|| invalid_data("clipboard control without consent"))?;
                clipboard
                    .control()
                    .restrict(request, MessageKind::ClipboardRestrict)?;
                if let (Some(controller), Some(snapshot)) =
                    (local_controller, clipboard_snapshot.as_mut())
                {
                    snapshot.narrow(request.content);
                    controller.publish_clipboard(Some(*snapshot));
                }
                write_source_release(writer, presented_scope, pointer_enabled || keyboard_enabled)?;
                let label = clipboard_policy::label(request.content, clipboard_directions);
                viewer
                    .set_clipboard_policy(format!("restricting to {label}; confirmation pending"));
                eprintln!("jwm-remote: restricting clipboard to {label}; confirmation pending");
            }
        }
        // The host accepts selection only after the current source's first
        // drawn ACK. Keep a shortcut pressed during startup or a switch queued.
        if source_ready && !switching && !events.contains(&ViewerEvent::Close) {
            let (selected_source, external_cycle) =
                toggle.map_or((None, false), ConnectToggle::take_next_source);
            if external_cycle {
                viewer.request_activation()?;
            }
            let next = deferred_selection.take().or_else(|| {
                if selected_source.is_some()
                    || external_cycle
                    || events.contains(&ViewerEvent::CycleSource)
                {
                    state.catalog.lock().unwrap().as_ref().and_then(|catalog| {
                        if let Some(target) = selected_source {
                            if catalog.current.epoch != source_epoch {
                                return None;
                            }
                            catalog.entries.iter().find_map(|entry| {
                                (entry.id == target).then(|| (entry.id, entry.label.clone()))
                            })
                        } else {
                            next_source(catalog, source_epoch)
                        }
                    })
                } else {
                    None
                }
            });
            if let Some((target, label)) = next {
                let waiting = {
                    let control = state.wayland_control.lock().unwrap();
                    managed_source_control && control.configured() && !control.ended
                };
                if waiting {
                    let controller = local_controller.unwrap();
                    release_managed_wayland_control(viewer, writer, state, controller)?;
                    deferred_selection = Some((target, label));
                } else {
                    if let Some(toggle) = toggle {
                        toggle.invalidate_sources();
                    }
                    if let Some(follow) = state.resize.lock().unwrap().follow.as_mut() {
                        follow.stop("source change requested");
                    }
                    write_source_release(
                        writer,
                        presented_scope,
                        pointer_enabled || keyboard_enabled,
                    )?;
                    viewer.begin_source_change(label)?;
                    writer.write_message(
                        MessageKind::SourceRequest,
                        &SourceRequest {
                            expected_epoch: source_epoch,
                            target,
                        }
                        .encode()?,
                    )?;
                    pending_presentations.clear();
                    requested_source = Some((source_epoch, target));
                    switching = true;
                }
            }
        }
        if switching {
            events.retain(|event| *event == ViewerEvent::Close);
        }
        let mut scoped_writer = ScopedInputWriter {
            writer,
            scope: presented_scope,
        };
        let outcome = dispatch_viewer_input(
            events,
            pointer_enabled && !switching,
            keyboard_enabled && !switching,
            &mut input_batch,
            &mut input_payload,
            &mut scoped_writer,
        )?;
        if outcome.close {
            return Ok(ViewerLoopExit {
                close_release_flushed: true,
            });
        }
        let mut wrote = outcome.wrote_input;
        // Sent after poll_events, which coalesces an interactive resize burst
        // into its final size, so a drag produces one request rather than one
        // per intermediate size.
        let viewport = viewer.viewport();
        if let Some(follow) = state.resize.lock().unwrap().follow.as_mut() {
            follow.viewport(viewport);
        }
        if !outcome.close {
            if state.wayland_popup.lock().unwrap().enabled {
                // Popup proofs describe the fixed native canvas. Record the
                // local viewport for the invalidation check, never request
                // remote scaling through the ordinary viewport protocol.
                reported_viewport = Some(viewport);
            } else if !fixed_control_geometry
                || (viewer.managed_wayland_layout_generation().is_none() && {
                    let control = state.wayland_control.lock().unwrap();
                    !control.configured() || control.ended
                })
            {
                // Native keys and capture proofs are anchored to the lease's
                // host geometry. The managed viewer scales locally until the
                // host confirms that input authority has ended.
                wrote |= viewport_policy.report(writer, &mut reported_viewport, viewport)?;
            }
        }
        if let Some(clipboard) = clipboard {
            // Closing the viewer wins over a simultaneous local worker exit.
            wrote |= forward_viewer_clipboard_from(
                || {
                    let record = clipboard.try_next()?;
                    if let Some(record) = &record
                        && record.kind == MessageKind::ClipboardRestrict
                    {
                        // The sender's on_written callback confirms this local barrier
                        // only after its real FIFO write, never at UI enqueue time.
                        let restriction = ClipboardRestriction::decode(&record.payload)
                            .map_err(|_| std::sync::mpsc::RecvError)?;
                        clipboard
                            .control()
                            .restriction_written(restriction)
                            .map_err(|_| std::sync::mpsc::RecvError)?;
                    }
                    Ok(record)
                },
                writer,
                outcome.close,
            )?;
        }
        if let Some((version, line)) =
            clipboard_status.and_then(|status| status.changed_since(clipboard_status_seen))
        {
            clipboard_status_seen = version;
            viewer.set_clipboard_activity(line);
        }
        if let (Some(controller), Some(snapshot)) = (local_controller, clipboard_snapshot.as_mut())
            && snapshot.update_transfer(clipboard_status.and_then(SyncStatus::viewer_transfer))
        {
            controller.publish_clipboard(Some(*snapshot));
        }
        {
            let mut restriction = state.clipboard_restriction.lock().unwrap();
            if let Some(restriction) = restriction.as_mut() {
                restriction.check_timeout(Instant::now())?;
                if let Some(confirmed) = restriction.take_confirmed() {
                    let label = clipboard_policy::label(confirmed.content, clipboard_directions);
                    viewer.set_clipboard_policy(format!("{label}; restriction confirmed"));
                    viewer.set_clipboard_status(confirmed.content != 0);
                    viewer.enable_clipboard_narrowing(confirmed.content != 0);
                    eprintln!("jwm-remote: clipboard restriction confirmed: {label}");
                }
            }
        }
        if Instant::now() >= next_heartbeat {
            writer.write_message(MessageKind::Heartbeat, &[])?;
            wrote = true;
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }
        if wrote {
            writer.flush()?;
        }

        apply_pending_cursor(viewer, state, source_epoch)?;
        for sequence in viewer.take_presented_sequences()? {
            let purpose = pending_presentations
                .iter()
                .find(|timing| timing.sequence == sequence)
                .ok_or_else(|| invalid_data("presentation lacks authenticated frame purpose"))?
                .purpose;
            if let Some(timing) = pending_presentations
                .iter()
                .find(|timing| timing.sequence == sequence)
            {
                if timing.source_epoch != source_epoch {
                    continue;
                }
                if purpose != FramePurpose::VisualRepair {
                    presented_scope = (timing.source_epoch, timing.geometry_epoch);
                    last_presented = Some(DrawnScope {
                        source_epoch: presented_scope.0,
                        geometry_epoch: presented_scope.1,
                        frame_sequence: sequence,
                    });
                    if let Some(follow) = state.resize.lock().unwrap().follow.as_mut() {
                        follow.presented(
                            presented_scope,
                            timing.source_size,
                            timing.viewport_revision,
                            Instant::now(),
                        );
                    }
                }
            }
            // Publish actual NativeProof presentation evidence before its ACK
            // can cause the host to reply Ready on the independent reader thread.
            // Keep the initial Request after ACK, and never record this receipt
            // a second time after a newer decoded frame invalidates its gate.
            let pointer_request = if purpose == FramePurpose::NativeProof {
                let mut pointer = state.wayland_pointer.lock().unwrap();
                if pointer.requested.is_none_or(|(initial, _)| {
                    (initial.source_epoch, initial.geometry_epoch) == presented_scope
                }) {
                    pointer.presented(
                        super::wayland_pointer_wire::DrawnScope {
                            source_epoch: presented_scope.0,
                            geometry_epoch: presented_scope.1,
                            frame_sequence: sequence,
                        },
                        Instant::now(),
                    )
                } else {
                    None
                }
            } else {
                None
            };
            let control_request = if purpose == FramePurpose::NativeProof {
                let mut control = state.wayland_control.lock().unwrap();
                control.check(presented_scope, Instant::now())?;
                if control.seamless_enabled {
                    control.local_handoff_active = viewer.wayland_handoff_active();
                }
                control.presented(
                    super::wayland_control_wire::DrawnScope {
                        source_epoch: presented_scope.0,
                        geometry_epoch: presented_scope.1,
                        frame_sequence: sequence,
                    },
                    Instant::now(),
                )
            } else {
                None
            };
            if state.wayland_popup.lock().unwrap().enabled {
                state
                    .wayland_popup
                    .lock()
                    .unwrap()
                    .presented(sequence, Instant::now())?;
                viewer.wayland_pointer_presented(sequence);
            }
            acknowledge_presentation(writer, sequence, pending_presentations, &state.telemetry)?;
            // A repair is real display progress and returns video credit, but
            // cannot create, renew, or revive any native-input authority.
            // Existing keyboard lease/held releases continue through their
            // unchanged event path and original deadline.
            if purpose == FramePurpose::VisualRepair {
                continue;
            }
            if let Some(request) = state.wayland_text.lock().unwrap().presented(DrawnScope {
                source_epoch: presented_scope.0,
                geometry_epoch: presented_scope.1,
                frame_sequence: sequence,
            }) {
                writer.write_message(MessageKind::WaylandTextCommit, &request.encode()?)?;
                writer.flush()?;
            }
            {
                let (enabled, needs_request, epoch) = {
                    let mut control = state.wayland_control.lock().unwrap();
                    control.check(presented_scope, Instant::now())?;
                    (
                        control.configured() && control.accepts_input(),
                        control.needs_request(),
                        control.epoch,
                    )
                };
                if enabled && purpose == FramePurpose::NativeProof {
                    viewer.wayland_pointer_presented(sequence);
                    if (needs_request || control_request.is_some())
                        && viewer.wayland_handoff_active()
                    {
                        #[cfg(feature = "remote-wayland")]
                        if !viewer.check_wayland_control(true)? {
                            if !viewer.user_close_requested()
                                && let Some(request) = control_request
                            {
                                state
                                    .wayland_control
                                    .lock()
                                    .unwrap()
                                    .cancel_unsent_request(request);
                            }
                            continue 'viewer;
                        }
                    }
                    let mut control = state.wayland_control.lock().unwrap();
                    control.check(presented_scope, Instant::now())?;
                    if control.ended || control.epoch != epoch {
                        continue;
                    }
                    if let Some(request) = control_request {
                        if control.seamless_enabled && !viewer.wayland_handoff_active() {
                            return Err(invalid_data(
                                "local handoff changed before one-shot control request",
                            )
                            .into());
                        }
                        writer.write_message(
                            MessageKind::WaylandControlRequest,
                            &request.encode()?,
                        )?;
                        writer.flush()?;
                    }
                }
            }

            {
                let mut keyboard = state.wayland_keyboard.lock().unwrap();
                keyboard.check_scope(presented_scope)?;
                if keyboard.needs_request() {
                    #[cfg(feature = "remote-wayland")]
                    viewer.check_wayland_keyboard(true)?;
                    if let Some(request) = keyboard.after_presentation_for_purpose(
                        presented_scope,
                        sequence,
                        Instant::now(),
                        purpose,
                    ) {
                        writer.write_message(
                            MessageKind::WaylandKeyboardRequest,
                            &request.encode()?,
                        )?;
                        writer.flush()?;
                    }
                }
            }

            if let Some(request) = state
                .resize
                .lock()
                .unwrap()
                .after_presentation(presented_scope, Instant::now())
            {
                writer.write_message(MessageKind::WindowResizeRequest, &request.encode()?)?;
                writer.flush()?;
            }
            {
                let mut pointer = state.wayland_pointer.lock().unwrap();
                if pointer.requested.is_some_and(|(initial, _)| {
                    (initial.source_epoch, initial.geometry_epoch) != presented_scope
                }) && !pointer.ended
                {
                    #[cfg(test)]
                    eprintln!("viewer pointer revoke: presented geometry change");
                    if let Some(revoke) = pointer.revoke() {
                        writer
                            .write_message(MessageKind::WaylandPointerRevoke, &revoke.encode()?)?;
                        writer.flush()?;
                    } else {
                        return Err(invalid_data("pointer geometry changed before grant").into());
                    }
                }
                if purpose == FramePurpose::NativeProof {
                    if let Some(request) = pointer_request {
                        writer.write_message(
                            MessageKind::WaylandPointerRequest,
                            &request.encode()?,
                        )?;
                        writer.flush()?;
                    }
                    if pointer.enabled && !pointer.ended {
                        viewer.wayland_pointer_presented(sequence);
                        viewer.set_wayland_pointer_status(pointer.label(Instant::now()));
                        #[cfg(test)]
                        viewer.observe_pointer_candidate(
                            pointer.click.map(|(scope, _, _, _)| scope.frame_sequence),
                            pointer.left_down,
                            pointer.sequence,
                        )?;
                        #[cfg(test)]
                        viewer.observe_scroll(pointer.scroll_observed)?;
                        #[cfg(test)]
                        viewer.observe_pointer_ready(if pointer.fresh(Instant::now()) {
                            pointer.ready.map(|scope| scope.frame_sequence)
                        } else {
                            None
                        })?;
                    }
                }
            }
            viewer.source_presented(presented_scope.1);
            viewer.enable_source_cycle(source_cycle_available);
            if (!source_ready || switching)
                && let Some(toggle) = toggle
                && !toggle.owns_shortcut()
            {
                let sources = state
                    .catalog
                    .lock()
                    .unwrap()
                    .as_ref()
                    .filter(|catalog| catalog.current.epoch == presented_scope.0)
                    .map(|catalog| super::engine::SessionSources {
                        session_id: state.session_epoch,
                        catalog: catalog.clone(),
                    });
                if managed_source_control && !state.wayland_control.lock().unwrap().configured() {
                    toggle.publish_wayland_input(
                        if used_control_sources.contains(&current_wayland_grant) {
                            super::engine::WaylandInputState::Unavailable
                        } else {
                            super::engine::WaylandInputState::Available
                        },
                    );
                }
                toggle.presented_source(sources);
            }
            switching = false;
            source_ready = true;
        }
        if viewer.can_submit_frame() {
            let item = match preparation.as_deref_mut() {
                Some(worker) => {
                    worker.next_frame(state, source_epoch, requested_source.is_none())?
                }
                None => take_latest(state, source_epoch).map(|frame| (frame, None)),
            };
            if let Some((frame, mut prepared)) = item {
                if frame.frame.source_epoch() == source_epoch && requested_source.is_none() {
                    let frame = match preparation.as_deref_mut() {
                        Some(worker) if prepared.is_none() && source_ready && !switching => {
                            worker.schedule(viewer, state, presented_scope, frame)?
                        }
                        _ => Some(frame),
                    };
                    if let Some(frame) = frame {
                        state.wayland_keyboard.lock().unwrap().check_scope((
                            frame.frame.source_epoch(),
                            frame.frame.geometry_epoch(),
                        ))?;
                        if viewer.prepare_frame_geometry(frame.frame.geometry_epoch())? {
                            write_source_release(
                                writer,
                                presented_scope,
                                pointer_enabled || keyboard_enabled,
                            )?;
                        }
                        let revision = state
                            .resize
                            .lock()
                            .unwrap()
                            .follow
                            .as_ref()
                            .map_or(0, |follow| follow.revision());
                        submit_frame_with_revision(
                            viewer,
                            frame,
                            pending_presentations,
                            revision,
                            state,
                            prepared.as_ref(),
                            #[cfg(feature = "remote-h264")]
                            native.as_deref_mut(),
                        )?;
                    }
                }
                if let Some(output) = prepared.take()
                    && let Some(worker) = preparation.as_deref_mut()
                {
                    worker.recycle(output);
                }
            } else if preparation.as_ref().is_none_or(|worker| !worker.busy())
                || matches!(viewer, Viewer::X11(x11) if x11.can_refresh_while_preparing())
            {
                #[cfg(all(feature = "remote-h264", feature = "remote-wayland"))]
                if let Some(presenter) = native.as_deref_mut()
                    && let Some((sequence, submission)) = presenter.refresh_wayland(
                        viewer,
                        &state.decode_pool,
                        state.telemetry.detailed,
                    )?
                {
                    if let Some(timings) = submission.fallback {
                        state.telemetry.record_hardware_decode(timings);
                    }
                    if submission.drawn
                        && let Some(pending) = pending_presentations
                            .iter_mut()
                            .find(|pending| pending.sequence == sequence)
                    {
                        pending.presentation_path =
                            submission.native.map_or("rgb", |(path, _)| path);
                    }
                }
                viewer.refresh_pending_redraw()?;
            }
        }

        if !switching
            && requested_source.is_none()
            && state.source_epoch.load(Ordering::Acquire) == source_epoch
        {
            let request = state
                .resize
                .lock()
                .unwrap()
                .follow
                .as_mut()
                .and_then(|follow| follow.next(Instant::now()));
            if let Some(request) = request {
                writer.write_message(MessageKind::WindowResizeRequest, &request.encode()?)?;
                writer.flush()?;
            }
        }
        // A source barrier may race the loop-top epoch snapshot. Never wait
        // after leaving its first keyframe/cursor queued for the next pass.
        if state.source_epoch.load(Ordering::Acquire) != source_epoch
            || (source_ready
                && !switching
                && toggle.is_some_and(ConnectToggle::next_source_pending))
        {
            continue;
        }
        if !state.alive.load(Ordering::Acquire) && state.latest.lock().unwrap().is_none() {
            let error = state
                .error
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Box::new(PeerClosed::legacy(&[])));
            return Err(error);
        }
        telemetry_reporter.maybe_report();
        viewer.prepare_wait()?;
        if viewer.has_pending_events()? {
            continue;
        }
        let mut deadline = next_heartbeat.min(telemetry_reporter.next_deadline());
        if let Some((rearm_deadline, _)) = rearm {
            deadline = deadline.min(rearm_deadline);
        }
        {
            let control = state.wayland_control.lock().unwrap();
            if let Some(release) = control.managed_release_deadline() {
                deadline = deadline.min(release);
            }
            if let Some(motion) = control.next_drag_motion() {
                deadline = deadline.min(motion);
            }
        }
        {
            let text = state.wayland_text.lock().unwrap();
            text.check()?;
            if let Some(at) = text.deadline() {
                deadline = deadline.min(at);
            }
        }
        if let Some(popup_deadline) = state.wayland_popup.lock().unwrap().deadline() {
            deadline = deadline.min(popup_deadline);
        }
        if let Some(follow_deadline) = state
            .resize
            .lock()
            .unwrap()
            .follow
            .as_ref()
            .and_then(|follow| follow.deadline())
        {
            deadline = deadline.min(follow_deadline);
        }
        if let Some(viewer_deadline) = viewer.next_event_deadline() {
            deadline = deadline.min(viewer_deadline);
        }
        if wait_for_activity(viewer.connection_fd(), wake.as_fd(), deadline)?
            == WaitOutcome::ViewerHangup
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "local viewer display connection closed",
            )
            .into());
        }
    }
}

fn forward_viewer_clipboard_from(
    mut receive: impl FnMut() -> Result<Option<Outgoing>, std::sync::mpsc::RecvError>,
    writer: &mut impl RecordWriter,
    closing: bool,
) -> RemoteResult<bool> {
    if closing {
        return Ok(false);
    }
    let mut wrote = false;
    for _ in 0..MAX_VIEWER_CLIPBOARD_RECORDS_PER_PASS {
        let record = match receive() {
            Ok(Some(record)) => record,
            Ok(None) => return Ok(wrote),
            Err(_) => {
                return Err(Box::new(ClientFailure {
                    stage: ClientStage::LocalClipboard,
                    cause: io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "local clipboard capture worker stopped",
                    )
                    .into(),
                }));
            }
        };
        writer.write_message(record.kind, &record.payload)?;
        wrote = true;
    }
    // Queued records remain in FIFO order for the next viewer pass. The
    // worker woke this loop for each of them, so the pass after this one
    // starts without waiting.
    Ok(wrote)
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

/// Latch close intent from the whole poll before the first fallible write:
/// a full batch can fail before iteration reaches the Close event.
fn dispatch_viewer_input(
    events: Vec<ViewerEvent>,
    pointer_enabled: bool,
    keyboard_enabled: bool,
    batch: &mut Vec<InputEvent>,
    payload: &mut Vec<u8>,
    writer: &mut impl RecordWriter,
) -> RemoteResult<InputPollOutcome> {
    let closing = events.contains(&ViewerEvent::Close);
    let result = (|| {
        let outcome =
            for_each_input_batch(events, pointer_enabled, keyboard_enabled, batch, |batch| {
                write_input_batch(writer, batch, payload)
            })?;
        if closing {
            if (pointer_enabled || keyboard_enabled) && !outcome.wrote_release_all {
                return Err(invalid_data(
                    "viewer closed without flushing the required ReleaseAll edge",
                )
                .into());
            }
            if outcome.wrote_input {
                writer.flush()?;
            }
        }
        Ok(outcome)
    })();
    result.map_err(|error| {
        if closing {
            user_close_cleanup_failure(error)
        } else {
            error
        }
    })
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
            ViewerEvent::CycleSource | ViewerEvent::NarrowClipboard => continue,
            // Scoped Wayland records must never become global legacy input.
            ViewerEvent::WaylandPointer { .. }
            | ViewerEvent::WaylandPointerCancel
            | ViewerEvent::WaylandPointerButtonCancel { .. }
            | ViewerEvent::WaylandDragMotion { .. }
            | ViewerEvent::WaylandDragRelease { .. }
            | ViewerEvent::WaylandPointerRevoke => {
                pointer_run_open = false;
                continue;
            }
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

fn write_input_batch(
    writer: &mut impl RecordWriter,
    events: &[InputEvent],
    payload: &mut Vec<u8>,
) -> RemoteResult<()> {
    encode_input_batch_into(events, payload)?;
    writer.write_message(MessageKind::InputBatch, payload)?;
    Ok(())
}

fn next_source(
    catalog: &SourceCatalog,
    epoch: u64,
) -> Option<(super::source_session::SourceId, String)> {
    if catalog.entries.len() <= 1 || catalog.current.epoch != epoch {
        return None;
    }
    let index = catalog
        .entries
        .iter()
        .position(|entry| entry.id == catalog.current.source)?;
    let next = &catalog.entries[(index + 1) % catalog.entries.len()];
    Some((next.id, next.label.clone()))
}

#[cfg(test)]
#[test]
fn in_process_selection_expires_with_its_connection_and_source() {
    use super::engine::SessionSources;
    use super::source_session::{SourceId, SourceVersion};
    use super::source_wire::SourceEntry;

    let (toggle, commands) = ConnectToggle::local();
    let (_receiver, sender) = wake_pair().unwrap();
    assert!(toggle.begin_viewing(&sender).unwrap());
    let catalog = SourceCatalog {
        current: SourceVersion {
            source: SourceId(1),
            epoch: 1,
        },
        entries: vec![
            SourceEntry {
                id: SourceId(1),
                label: "One".into(),
            },
            SourceEntry {
                id: SourceId(2),
                label: "Two".into(),
            },
        ],
    };
    assert!(commands.select_source(10, 1, SourceId(2)).is_err());
    toggle.presented_source(Some(SessionSources {
        session_id: 10,
        catalog: catalog.clone(),
    }));
    assert!(commands.select_source(10, 1, SourceId(3)).is_err());
    commands.select_source(10, 1, SourceId(2)).unwrap();
    assert!(toggle.next_source_pending());
    assert_eq!(toggle.take_next_source(), (Some(SourceId(2)), false));
    assert!(commands.sources().is_none());
    let mut changed = catalog.clone();
    changed.current = SourceVersion {
        source: SourceId(2),
        epoch: 2,
    };
    toggle.presented_source(Some(SessionSources {
        session_id: 10,
        catalog: changed,
    }));
    assert!(commands.select_source(10, 1, SourceId(1)).is_err());

    assert!(toggle.begin_backoff());
    assert!(commands.sources().is_none());
    assert!(toggle.begin_attempt());
    assert!(toggle.begin_viewing(&sender).unwrap());
    toggle.presented_source(Some(SessionSources {
        session_id: 11,
        catalog,
    }));
    assert!(commands.select_source(10, 1, SourceId(2)).is_err());
    assert!(!toggle.next_source_pending());
    commands.select_source(11, 1, SourceId(2)).unwrap();
    assert!(toggle.next_source_pending());
    commands.close();
    assert!(!toggle.next_source_pending());
}

fn write_source_release(
    writer: &mut impl RecordWriter,
    scope: (u64, u64),
    input_enabled: bool,
) -> RemoteResult<()> {
    if input_enabled {
        let payload =
            super::messages::encode_source_input(scope.0, scope.1, &[InputEvent::ReleaseAll])?;
        writer.write_message(MessageKind::InputBatch, &payload)?;
    }
    Ok(())
}

struct ScopedInputWriter<'a, W> {
    writer: &'a mut W,
    scope: (u64, u64),
}
impl<W: RecordWriter> RecordWriter for ScopedInputWriter<'_, W> {
    fn write_message(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
        if kind == MessageKind::InputBatch {
            let events = super::messages::decode_input_batch(payload)?;
            let bytes = super::messages::encode_source_input(self.scope.0, self.scope.1, &events)?;
            self.writer.write_message(kind, &bytes)
        } else {
            self.writer.write_message(kind, payload)
        }
    }
    fn flush(&mut self) -> RemoteResult<()> {
        self.writer.flush()
    }
}

struct PendingFrameTiming {
    purpose: FramePurpose,
    source_size: (u16, u16),
    viewport_revision: u64,
    source_epoch: u64,
    geometry_epoch: u64,
    sequence: u64,
    ready_at: Instant,
    decode_elapsed: Duration,
    draw_started: Instant,
    presentation_path: &'static str,
}

fn submit_frame_with_revision(
    viewer: &mut Viewer,
    queued: QueuedFrame,
    pending: &mut std::collections::VecDeque<PendingFrameTiming>,
    viewport_revision: u64,
    state: &ReceiveState,
    prepared: Option<&PreparedX11Rgb>,
    #[cfg(feature = "remote-h264")] native: Option<&mut native_video::Presenter>,
) -> RemoteResult<bool> {
    let info = queued.frame.info();
    if info.session_epoch != state.session_epoch {
        return Err(invalid_data("received frame belongs to a different session").into());
    }
    let x11_presenter = matches!(viewer, Viewer::X11(_));
    let draw_started = Instant::now();
    let (drawn, presentation_path) = match queued.frame {
        FrameLease::Cpu(frame) => {
            let (drawn, used_preparation) = match (viewer, prepared) {
                (Viewer::X11(x11), Some(output)) if output.packed => {
                    x11.draw_prepared(frame, output.plan, &output.pixels)?
                }
                (viewer, _) => (viewer.draw_recyclable(frame)?, false),
            };
            if let Some(output) = prepared
                && drawn
            {
                state.telemetry.record_x11_prepare_submission(
                    used_preparation,
                    output.plan.viewport_revision(),
                );
            }
            (drawn, "rgb")
        }
        #[cfg(feature = "remote-h264")]
        FrameLease::Vaapi { pixels, info } => {
            let presenter = native.ok_or_else(|| invalid_data("missing native frame presenter"))?;
            let submitted = presenter.draw(
                viewer,
                pixels,
                info,
                &state.decode_pool,
                state.telemetry.detailed,
            )?;
            if let Some(timings) = submitted.fallback {
                state.telemetry.record_hardware_decode(timings);
            }
            let path = if let Some((path, elapsed)) = submitted.native {
                state.telemetry.record_native_submission(
                    elapsed,
                    x11_presenter,
                    submitted.native_stages,
                );
                path
            } else {
                "rgb"
            };
            (submitted.drawn, path)
        }
    };
    if x11_presenter {
        state
            .telemetry
            .record_x11_presenter_submit(draw_started.elapsed());
    }
    if !drawn {
        return Ok(false);
    }
    let timing = PendingFrameTiming {
        purpose: queued.purpose,
        source_size: info.source_size,
        viewport_revision,
        source_epoch: info.source_epoch,
        geometry_epoch: info.geometry_epoch,
        sequence: info.sequence,
        ready_at: queued.ready_at,
        decode_elapsed: queued.decode_elapsed,
        draw_started,
        presentation_path,
    };
    pending.push_back(timing);
    Ok(true)
}

fn acknowledge_presentation(
    writer: &mut impl RecordWriter,
    sequence: u64,
    pending: &mut std::collections::VecDeque<PendingFrameTiming>,
    telemetry: &ClientTelemetry,
) -> RemoteResult<()> {
    let position = pending
        .iter()
        .position(|timing| timing.sequence == sequence)
        .ok_or_else(|| invalid_data("unexpected X11 frame completion"))?;
    // Accelerator recovery redraws the latest retained canvas. Earlier queued
    // frames are superseded, never reported as drawn or acknowledged separately.
    for _ in 0..position {
        pending.pop_front();
    }
    let timing = pending.pop_front().unwrap();
    let finished_at = Instant::now();
    telemetry.record_drawn_at(timing.ready_at, timing.draw_started, finished_at);
    telemetry.record_presented_scope(&timing, finished_at);
    if telemetry.video_feedback {
        let feedback = ViewerFrameFeedback {
            decode: timing.decode_elapsed,
            presentation_queue: timing
                .draw_started
                .saturating_duration_since(timing.ready_at),
            presentation: finished_at.saturating_duration_since(timing.draw_started),
        };
        writer.write_message(
            MessageKind::FrameAck,
            &feedback.encode_ack(timing.source_epoch, sequence)?,
        )?;
    } else {
        writer.write_message(
            MessageKind::FrameAck,
            &encode_source_frame_ack(timing.source_epoch, sequence)?,
        )?;
    }
    writer.flush()?;
    if !writer.queued() {
        telemetry.record_acked();
    }
    Ok(())
}

#[cfg(test)]
fn draw_and_ack(
    writer: &mut impl RecordWriter,
    queued: QueuedFrame,
    telemetry: &ClientTelemetry,
    draw: impl FnOnce(RecyclableDecodedFrame) -> RemoteResult<bool>,
) -> RemoteResult<()> {
    let sequence = queued.frame.sequence();
    let source_epoch = queued.frame.source_epoch();
    let draw_started = Instant::now();
    #[allow(irrefutable_let_patterns)]
    let FrameLease::Cpu(frame) = queued.frame else {
        panic!("CPU-only fixture")
    };
    let draw_result = draw(frame);
    let draw_finished = Instant::now();
    if !draw_result? {
        return Ok(());
    }
    telemetry.record_drawn_at(queued.ready_at, draw_started, draw_finished);
    writer.write_message(
        MessageKind::FrameAck,
        &encode_source_frame_ack(source_epoch, sequence)?,
    )?;
    writer.flush()?;
    if !writer.queued() {
        telemetry.record_acked();
    }
    Ok(())
}

struct QueuedFrame {
    purpose: FramePurpose,
    frame: FrameLease,
    ready_at: Instant,
    decode_elapsed: Duration,
}

struct PreparedX11Rgb {
    plan: super::x11_viewer::CpuPreparation,
    pixels: Vec<u8>,
    packed: bool,
}

struct X11PreparationJob {
    queued: QueuedFrame,
    output: PreparedX11Rgb,
    work_elapsed: Duration,
}

struct X11PreparationWorker {
    jobs: mpsc::SyncSender<X11PreparationJob>,
    ready: mpsc::Receiver<X11PreparationJob>,
    thread: ReceiverThread,
    scratch: Vec<u8>,
    pending_scope: Option<(u64, u64)>,
    obsolete: bool,
}

impl X11PreparationWorker {
    fn start(wake: WakeSender) -> io::Result<Self> {
        let (jobs, requests) = mpsc::sync_channel::<X11PreparationJob>(1);
        let (replies, ready) = mpsc::sync_channel(1);
        let thread = ReceiverThread::spawn_named("jwm-remote-x11-prepare", move || {
            while let Ok(mut job) = requests.recv() {
                let started = Instant::now();
                #[allow(irrefutable_let_patterns)]
                if let FrameLease::Cpu(frame) = &job.queued.frame {
                    job.output.packed = job
                        .output
                        .plan
                        .prepare(frame.image(), &mut job.output.pixels)
                        .is_ok();
                }
                job.work_elapsed = started.elapsed();
                if replies.send(job).is_err() || wake.signal().is_err() {
                    return;
                }
            }
        })?;
        Ok(Self {
            jobs,
            ready,
            thread,
            scratch: Vec::new(),
            pending_scope: None,
            obsolete: false,
        })
    }

    fn busy(&self) -> bool {
        self.pending_scope.is_some()
    }

    fn schedule(
        &mut self,
        viewer: &Viewer,
        state: &ReceiveState,
        presented_scope: (u64, u64),
        queued: QueuedFrame,
    ) -> RemoteResult<Option<QueuedFrame>> {
        let info = queued.frame.info();
        let scope = (info.source_epoch, info.geometry_epoch);
        if self.busy()
            || scope != presented_scope
            || queued.purpose != FramePurpose::Video
            || !state.alive.load(Ordering::Acquire)
        {
            return Ok(Some(queued));
        }
        let plan = match (viewer, &queued.frame) {
            (Viewer::X11(x11), FrameLease::Cpu(frame)) => x11.cpu_preparation(frame),
            #[allow(unreachable_patterns)]
            _ => None,
        };
        let Some(plan) = plan else {
            return Ok(Some(queued));
        };
        let job = X11PreparationJob {
            queued,
            output: PreparedX11Rgb {
                plan,
                pixels: std::mem::take(&mut self.scratch),
                packed: false,
            },
            work_elapsed: Duration::ZERO,
        };
        self.jobs.try_send(job).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "X11 CPU preparation worker stopped",
            )
        })?;
        self.pending_scope = Some(scope);
        self.obsolete = false;
        state.telemetry.record_x11_prepare_scheduled();
        Ok(None)
    }

    fn next_frame(
        &mut self,
        state: &ReceiveState,
        source_epoch: u64,
        accepting: bool,
    ) -> RemoteResult<Option<(QueuedFrame, Option<PreparedX11Rgb>)>> {
        if let Some((source, geometry)) = self.pending_scope {
            let reply = self.ready.try_recv();
            self.obsolete |= !accepting
                || source != source_epoch
                || source != state.source_epoch.load(Ordering::Acquire);
            let next = if !self.obsolete {
                // A new geometry or proof takes precedence over old CPU work.
                // Old materializations can finish but cannot reenter the lane.
                state
                    .latest
                    .lock()
                    .unwrap()
                    .take_when(source_epoch, |queued| {
                        queued.purpose != FramePurpose::Video
                            || queued.frame.geometry_epoch() != geometry
                    })
            } else {
                None
            };
            self.obsolete |= next.is_some() || source != state.source_epoch.load(Ordering::Acquire);
            match reply {
                Ok(job) => {
                    self.pending_scope = None;
                    state.telemetry.record_x11_prepare_finished(
                        job.output.packed,
                        self.obsolete,
                        job.work_elapsed,
                    );
                    if !self.obsolete {
                        return Ok(Some((job.queued, Some(job.output))));
                    }
                    self.recycle(job.output);
                }
                Err(mpsc::TryRecvError::Empty) if !self.obsolete => return Ok(None),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "X11 CPU preparation worker stopped",
                    )
                    .into());
                }
            }
            if let Some(next) = next {
                return Ok(Some((next, None)));
            }
        }
        Ok(take_latest(state, source_epoch).map(|queued| (queued, None)))
    }

    fn recycle(&mut self, output: PreparedX11Rgb) {
        self.scratch = output.pixels;
    }

    fn close(self) -> RemoteResult<()> {
        let Self { jobs, thread, .. } = self;
        drop(jobs);
        match thread.join_timeout(RECEIVER_JOIN_TIMEOUT) {
            ReceiverExit::Completed => Ok(()),
            exit => Err(io::Error::other(format!(
                "X11 CPU preparation worker did not close cleanly: {exit:?}"
            ))
            .into()),
        }
    }
}

#[derive(Default)]
struct PresentationMailbox {
    latest: Option<QueuedFrame>,
    repair: Option<QueuedFrame>,
}

impl PresentationMailbox {
    fn is_none(&self) -> bool {
        self.latest.is_none() && self.repair.is_none()
    }

    fn publish(&mut self, next: QueuedFrame) -> [Option<QueuedFrame>; 2] {
        let unchanged = self
            .latest
            .as_ref()
            .is_some_and(|old| next.frame.shares_unchanged_tiles(&old.frame));
        // A native proof may immediately follow a PNG repair carrying the
        // same pixels. Keep both receipts in presentation order. They share
        // one immutable tile snapshot. Real damage or a new scope discards
        // the reserved repair and takes the normal latest-frame path.
        let keep_repair = unchanged
            && next.purpose == FramePurpose::NativeProof
            && self
                .latest
                .as_ref()
                .is_some_and(|old| old.purpose == FramePurpose::VisualRepair);
        let previous = self.latest.replace(next);
        if keep_repair {
            return [std::mem::replace(&mut self.repair, previous), None];
        }
        let stale_repair = if unchanged { None } else { self.repair.take() };
        [previous, stale_repair]
    }

    fn take(&mut self, epoch: u64) -> Option<QueuedFrame> {
        self.take_when(epoch, |_| true)
    }

    fn take_when(
        &mut self,
        epoch: u64,
        condition: impl FnOnce(&QueuedFrame) -> bool,
    ) -> Option<QueuedFrame> {
        if self
            .latest
            .as_ref()
            .is_some_and(|queued| queued.frame.source_epoch() > epoch)
        {
            return None;
        }
        if !condition(self.repair.as_ref().or(self.latest.as_ref())?) {
            return None;
        }
        self.repair
            .take()
            .or_else(|| self.latest.take())
            .filter(|queued| queued.frame.source_epoch() == epoch)
    }
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
    decode_samples: super::telemetry::StageStats,
    presentation_queue: super::telemetry::StageStats,
    presentation_completion: super::telemetry::StageStats,
    x11_cpu_prepare: super::telemetry::StageStats,
    x11_presenter_submit: super::telemetry::StageStats,
    x11_cpu_preparation: X11PreparationStats,
    cursor_interval: super::telemetry::StageStats,
    hardware_decode: super::telemetry::StageStats,
    hardware_download: super::telemetry::StageStats,
    hardware_direct_map: super::telemetry::StageStats,
    hardware_conversion: super::telemetry::StageStats,
    hardware_rgb_copy: super::telemetry::StageStats,
    hardware_rgb_work: HardwareRgbStats,
    first_presentation: super::telemetry::StageStats,
    source_switch_presentation: super::telemetry::StageStats,
    decode_queue_wait: super::telemetry::StageStats,
    frame_header_wait: super::telemetry::StageStats,
    frame_body_receive: super::telemetry::StageStats,
    frame_authentication: super::telemetry::StageStats,
    decode_queue_records: usize,
    decode_queue_bytes: usize,
    last_frame: Option<super::engine::ViewerFrameScope>,
    last_viewport_revision: Option<u64>,
    last_presentation_path: Option<&'static str>,
    native_presented: u64,
    native_preparation_and_submit: super::telemetry::StageStats,
    #[cfg(feature = "remote-h264")]
    native_x11_stages: super::telemetry::NativeStageStats,
    #[cfg(all(feature = "remote-h264", feature = "remote-wayland"))]
    native_wayland_stages: super::telemetry::NativeStageStats,
    sender: ViewerSendStats,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct X11PreparationStats {
    scheduled: u64,
    prepared: u64,
    submitted: u64,
    fallbacks: u64,
    stale: u64,
    last_submitted_viewport_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HardwareRgbStats {
    fitting_checkouts: u64,
    fresh_reserves: u64,
    growth_reserves: u64,
    requested_bytes: u64,
    added_capacity_bytes: u64,
    download_targets_populated: u64,
    download_targets_reused: u64,
    download_targets_replaced: u64,
    direct_map_frames: u64,
    direct_map_staged_bytes: u64,
    direct_map_fallback_frames: u64,
    full_frame_copies: u64,
    full_frame_copy_bytes: u64,
    banded_copies: u64,
    banded_copy_bytes: u64,
}

impl HardwareRgbStats {
    #[cfg(feature = "remote-h264")]
    fn record(&mut self, work: super::h264::HardwareRgbWork) {
        use super::{
            frame::DecodeBufferAllocation,
            h264::{DownloadTarget, RgbCopy, RgbInput},
        };

        // A native redraw can materialize the same surface again. Count the
        // work independently of decoded or presented sequences.
        let Some(checkout) = work.checkout else {
            return;
        };
        match checkout.allocation {
            DecodeBufferAllocation::Reused => self.fitting_checkouts += 1,
            DecodeBufferAllocation::Fresh => self.fresh_reserves += 1,
            DecodeBufferAllocation::Grown => self.growth_reserves += 1,
        }
        self.requested_bytes += checkout.requested_bytes as u64;
        self.added_capacity_bytes += checkout.capacity_added_bytes as u64;
        let download = match work.input {
            RgbInput::Download(target) => Some(target),
            RgbInput::DirectMap(bytes) => {
                self.direct_map_frames += 1;
                self.direct_map_staged_bytes += bytes as u64;
                None
            }
            RgbInput::MapFallback(target) => {
                self.direct_map_fallback_frames += 1;
                Some(target)
            }
        };
        match download {
            Some(DownloadTarget::Populated) => self.download_targets_populated += 1,
            Some(DownloadTarget::Reused) => self.download_targets_reused += 1,
            Some(DownloadTarget::Replaced) => self.download_targets_replaced += 1,
            None => {}
        }
        match work.copy {
            RgbCopy::FullFrame(bytes) => {
                self.full_frame_copies += 1;
                self.full_frame_copy_bytes += bytes as u64;
            }
            RgbCopy::Banded(bytes) => {
                self.banded_copies += 1;
                self.banded_copy_bytes += bytes as u64;
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.full_frame_copies == 0 && self.banded_copies == 0
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "pool": {
                "fitting_checkouts": self.fitting_checkouts,
                "fresh_reserves": self.fresh_reserves,
                "growth_reserves": self.growth_reserves,
                "requested_bytes": self.requested_bytes,
                "added_capacity_bytes": self.added_capacity_bytes,
            },
            "download": {
                "empty_targets_populated": self.download_targets_populated,
                "retained_targets_reused": self.download_targets_reused,
                "populated_targets_replaced": self.download_targets_replaced,
            },
            "vaapi_map": {
                "mapped_frames": self.direct_map_frames,
                "staged_bytes": self.direct_map_staged_bytes,
                "fallback_frames": self.direct_map_fallback_frames,
            },
            "copy": {
                "full_frame_copies": self.full_frame_copies,
                "full_frame_bytes": self.full_frame_copy_bytes,
                "banded_copies": self.banded_copies,
                "banded_bytes": self.banded_copy_bytes,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ViewerSendStats {
    control_wait: super::telemetry::StageStats,
    input_wait: super::telemetry::StageStats,
    bulk_wait: super::telemetry::StageStats,
    control_write: super::telemetry::StageStats,
    bulk_write: super::telemetry::StageStats,
    oldest_queued: super::telemetry::StageStats,
    control_records: usize,
    bulk_records: usize,
    control_bytes: usize,
    bulk_bytes: usize,
}

impl ViewerSendStats {
    fn record(&mut self, kind: MessageKind, timing: super::sender::SendTiming) {
        if matches!(
            kind,
            MessageKind::Clipboard | MessageKind::ClipboardTransfer
        ) {
            self.bulk_wait.record(timing.queue_wait);
            self.bulk_write.record(timing.write_elapsed);
        } else {
            self.control_wait.record(timing.queue_wait);
            self.control_write.record(timing.write_elapsed);
        }
        if matches!(
            kind,
            MessageKind::InputBatch
                | MessageKind::WaylandKeyboardKey
                | MessageKind::WaylandPointerAction
                | MessageKind::WaylandControlAction
                | MessageKind::WaylandKeyboardRevoke
                | MessageKind::WaylandPointerRevoke
                | MessageKind::WaylandControlRevoke
                | MessageKind::WaylandPopupRevoke
        ) {
            self.input_wait.record(timing.queue_wait);
        }
        self.oldest_queued.record(timing.oldest_queued);
        self.control_records = self.control_records.max(timing.control_records);
        self.bulk_records = self.bulk_records.max(timing.bulk_records);
        self.control_bytes = self.control_bytes.max(timing.control_bytes);
        self.bulk_bytes = self.bulk_bytes.max(timing.bulk_bytes);
    }
}

impl ClientTelemetryWindow {
    fn is_empty(self) -> bool {
        let empty = self.received == 0
            && self.decoded == 0
            && self.replaced == 0
            && self.drawn == 0
            && self.acked == 0
            && self.cursor_interval.observations == 0
            && self.frame_authentication.observations == 0
            && self.x11_cpu_preparation == X11PreparationStats::default()
            && self.sender.oldest_queued.observations == 0
            && self.hardware_rgb_work.is_empty();
        #[cfg(feature = "remote-h264")]
        let empty = empty && self.native_x11_stages.is_empty();
        #[cfg(all(feature = "remote-h264", feature = "remote-wayland"))]
        let empty = empty && self.native_wayland_stages.is_empty();
        empty
    }
}

#[derive(Default)]
struct ClientTelemetry {
    detailed: bool,
    video_feedback: bool,
    window: Mutex<ClientTelemetryWindow>,
    last_cursor: Mutex<Option<(u64, Instant)>>,
    awaiting_presentation: Mutex<Option<(u64, Instant)>>,
}

impl ClientTelemetry {
    fn record_x11_prepare_scheduled(&self) {
        if self.detailed {
            self.lock_window().x11_cpu_preparation.scheduled += 1;
        }
    }

    fn record_x11_prepare_finished(&self, packed: bool, stale: bool, elapsed: Duration) {
        if self.detailed {
            let mut window = self.lock_window();
            window.x11_cpu_prepare.record(elapsed);
            window.x11_cpu_preparation.prepared += u64::from(packed);
            window.x11_cpu_preparation.stale += u64::from(stale);
        }
    }

    fn record_x11_prepare_submission(&self, used: bool, viewport_revision: u64) {
        if self.detailed {
            let mut window = self.lock_window();
            let counters = &mut window.x11_cpu_preparation;
            if used {
                counters.submitted += 1;
                counters.last_submitted_viewport_revision = Some(viewport_revision);
            } else {
                counters.fallbacks += 1;
            }
        }
    }

    fn record_x11_presenter_submit(&self, elapsed: Duration) {
        if self.detailed {
            self.lock_window().x11_presenter_submit.record(elapsed);
        }
    }

    fn read_record<R: io::Read>(
        &self,
        reader: &mut SessionReader<R>,
        payload: &mut Vec<u8>,
    ) -> Result<MessageKind, ProtocolError> {
        if !self.detailed {
            return reader.read_message_into(payload);
        }
        let (kind, timing) = reader.read_message_into_timed(payload)?;
        if kind == MessageKind::Frame {
            let mut window = self.lock_window();
            window.frame_header_wait.record(timing.header_wait);
            window.frame_body_receive.record(timing.body_receive);
            window.frame_authentication.record(timing.authentication);
        }
        Ok(kind)
    }

    fn record_received(&self) {
        let mut window = self.lock_window();
        window.received = window.received.saturating_add(1);
    }

    fn record_decoded_at(&self, decode_started: Instant, ready_at: Instant) {
        let mut window = self.lock_window();
        let elapsed = ready_at.saturating_duration_since(decode_started);
        window.decoded = window.decoded.saturating_add(1);
        window.decode_elapsed = window.decode_elapsed.saturating_add(elapsed);
        if self.detailed {
            window.decode_samples.record(elapsed);
        }
    }

    fn record_frame_scope(&self, frame: &FrameLease, codec: FrameCodec) {
        if self.detailed {
            let info = frame.info();
            self.lock_window().last_frame = Some(super::engine::ViewerFrameScope {
                source_epoch: info.source_epoch,
                geometry_epoch: info.geometry_epoch,
                sequence: info.sequence,
                codec: codec.name(),
                source_dimensions: info.source_size,
                encoded_dimensions: info.encoded_size,
            });
        }
    }

    fn expect_first_frame(&self, epoch: u64, now: Instant) {
        if self.detailed {
            *self.awaiting_presentation.lock().unwrap() = Some((epoch, now));
        }
    }

    fn record_presented_scope(&self, timing: &PendingFrameTiming, finished: Instant) {
        if !self.detailed {
            return;
        }
        let mut awaiting = self.awaiting_presentation.lock().unwrap();
        let mut window = self.lock_window();
        window.last_viewport_revision = Some(timing.viewport_revision);
        window.last_presentation_path = Some(timing.presentation_path);
        if matches!(
            timing.presentation_path,
            "x11-vaapi-dmabuf-dri3" | "wayland-vaapi-dmabuf"
        ) {
            window.native_presented = window.native_presented.saturating_add(1);
        }
        if let Some((epoch, started)) = *awaiting
            && epoch == timing.source_epoch
        {
            let elapsed = finished.saturating_duration_since(started);
            if epoch == 1 {
                window.first_presentation.record(elapsed);
            } else {
                window.source_switch_presentation.record(elapsed);
            }
            *awaiting = None;
        }
    }

    #[cfg(feature = "remote-h264")]
    fn record_hardware_decode(&self, timings: super::h264::DecodeTimings) {
        if self.detailed {
            let mut window = self.lock_window();
            if let Some(decode) = timings.decode {
                window.hardware_decode.record(decode);
            }
            if let Some(download) = timings.download {
                window.hardware_download.record(download);
            }
            if let Some(direct_map) = timings.direct_map {
                window.hardware_direct_map.record(direct_map);
            }
            if let Some(conversion) = timings.conversion {
                window.hardware_conversion.record(conversion);
            }
            if let Some(copy) = timings.copy {
                window.hardware_rgb_copy.record(copy);
            }
            if let Some(work) = timings.rgb_work {
                window.hardware_rgb_work.record(work);
            }
        }
    }

    #[cfg(feature = "remote-h264")]
    fn record_native_submission(
        &self,
        elapsed: Duration,
        x11: bool,
        stages: Option<super::telemetry::NativeStageTimes>,
    ) {
        if self.detailed {
            let mut window = self.lock_window();
            window.native_preparation_and_submit.record(elapsed);
            if let Some(stages) = stages {
                if x11 {
                    window.native_x11_stages.record(stages);
                } else {
                    #[cfg(feature = "remote-wayland")]
                    window.native_wayland_stages.record(stages);
                }
            }
        }
    }

    fn record_cursor_position(&self, source_epoch: u64, now: Instant) {
        if !self.detailed {
            return;
        }
        let previous = self
            .last_cursor
            .lock()
            .unwrap()
            .replace((source_epoch, now));
        if let Some((previous_epoch, previous)) = previous
            && source_epoch == previous_epoch
        {
            self.lock_window()
                .cursor_interval
                .record(now.saturating_duration_since(previous));
        }
    }

    fn record_replaced(&self) {
        let mut window = self.lock_window();
        window.replaced = window.replaced.saturating_add(1);
    }

    fn record_drawn_at(&self, ready_at: Instant, draw_started: Instant, draw_finished: Instant) {
        let mut window = self.lock_window();
        let queue = draw_started.saturating_duration_since(ready_at);
        let completion = draw_finished.saturating_duration_since(draw_started);
        window.drawn = window.drawn.saturating_add(1);
        window.queue_elapsed = window.queue_elapsed.saturating_add(queue);
        window.draw_elapsed = window.draw_elapsed.saturating_add(completion);
        if self.detailed {
            window.presentation_queue.record(queue);
            window.presentation_completion.record(completion);
        }
    }

    fn record_acked(&self) {
        let mut window = self.lock_window();
        window.acked = window.acked.saturating_add(1);
    }

    fn record_send(&self, kind: MessageKind, timing: super::sender::SendTiming) {
        self.lock_window().sender.record(kind, timing);
    }

    fn record_decode_queue(
        &self,
        kind: MessageKind,
        timing: super::client_decode_queue::DequeueTiming,
    ) {
        if self.detailed {
            let mut window = self.lock_window();
            if kind == MessageKind::Frame {
                window.decode_queue_wait.record(timing.age);
            }
            window.decode_queue_records = window.decode_queue_records.max(timing.records);
            window.decode_queue_bytes = window.decode_queue_bytes.max(timing.bytes);
        }
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

#[derive(Clone, Copy)]
struct ViewerTelemetryContext {
    correlation_id: u128,
    backend: ViewerBackend,
    negotiated_codec: FrameCodec,
    limits: super::messages::FrameLimits,
    input_mode: &'static str,
}

fn negotiated_input_mode(hello: &ServerHello) -> &'static str {
    let stream = hello.stream;
    if stream.wayland_popup_pointer {
        "wayland-popup"
    } else if stream.wayland_text {
        "wayland-text"
    } else if stream.resumable_control {
        "wayland-resumable-control"
    } else if stream.wayland_control {
        "wayland-control"
    } else if stream.wayland_keyboard {
        "wayland-keyboard"
    } else if stream.wayland_pointer {
        "wayland-pointer"
    } else if hello.pointer_enabled && hello.keyboard_enabled {
        "x11-keyboard-pointer"
    } else if hello.pointer_enabled {
        "x11-pointer"
    } else {
        "view-only"
    }
}

impl ClientTelemetryReport {
    fn snapshot(
        &self,
        context: ViewerTelemetryContext,
        final_report: bool,
    ) -> super::engine::MetricsSnapshot {
        let window = &self.window;
        super::engine::MetricsSnapshot {
            session_id: context.correlation_id,
            window: self.elapsed,
            final_report,
            negotiated_codec: context.negotiated_codec,
            last_decoded_frame: window.last_frame,
            presented_frames: window.drawn,
            acked_frames: window.acked,
            native_presented_frames: window.native_presented,
            last_presentation_path: window.last_presentation_path,
            decode: window.decode_samples.snapshot(),
            hardware_download: window.hardware_download.snapshot(),
            presentation_queue: window.presentation_queue.snapshot(),
            presentation_completion: window.presentation_completion.snapshot(),
            native_preparation_and_submit: window.native_preparation_and_submit.snapshot(),
        }
    }

    fn json(&self, context: ViewerTelemetryContext, final_report: bool) -> serde_json::Value {
        let window = &self.window;
        #[cfg_attr(
            not(all(feature = "remote-h264", feature = "remote-wayland")),
            allow(unused_mut)
        )]
        let mut report = serde_json::json!({
            "type": "jwm_remote_viewer_stats", "schema_version": 1,
            "version": env!("CARGO_PKG_VERSION"),
            "commit": option_env!("JWM_BUILD_COMMIT"),
            "session_id": format!("{:032x}", context.correlation_id),
            "viewer_backend": context.backend.to_string(),
            "negotiated_codec": context.negotiated_codec.name(),
            "input_mode": context.input_mode,
            "frame_limits": {
                "max_width": context.limits.max_width,
                "max_height": context.limits.max_height,
                "max_pixels": context.limits.max_pixels,
            },
            "window_seconds": self.elapsed.as_secs_f64(),
            "final": final_report,
            "received": window.received, "decoded": window.decoded, "replaced": window.replaced,
            "drawn": window.drawn, "acked": window.acked, "last_frame": window.last_frame,
            "last_viewport_revision": window.last_viewport_revision,
            "last_presentation_path": window.last_presentation_path,
            "native_presented": window.native_presented,
            "hardware_rgb_work": window.hardware_rgb_work.json(),
            "x11_cpu_preparation": {
                "scheduled": window.x11_cpu_preparation.scheduled,
                "prepared": window.x11_cpu_preparation.prepared,
                "submitted": window.x11_cpu_preparation.submitted,
                "fallbacks": window.x11_cpu_preparation.fallbacks,
                "stale": window.x11_cpu_preparation.stale,
                "last_submitted_viewport_revision":
                    window.x11_cpu_preparation.last_submitted_viewport_revision,
            },
            "quantile_policy": super::telemetry::QUANTILE_POLICY,
            "sender_max_sampled_at_dequeue": {
                "control_records": window.sender.control_records,
                "bulk_records": window.sender.bulk_records,
                "control_bytes": window.sender.control_bytes,
                "bulk_bytes": window.sender.bulk_bytes,
            },
            "decode_queue_max_sampled_at_dequeue": {
                "records": window.decode_queue_records,
                "bytes": window.decode_queue_bytes,
            },
            "stages": {
                "decode_ms": window.decode_samples.json(),
                "presentation_queue_ms": window.presentation_queue.json(),
                "presentation_completion_ms": window.presentation_completion.json(),
                "x11_cpu_prepare_ms": window.x11_cpu_prepare.json(),
                "x11_presenter_submit_ms": window.x11_presenter_submit.json(),
                "native_preparation_and_submit_ms": window.native_preparation_and_submit.json(),
                "cursor_interval_ms": window.cursor_interval.json(),
                "hardware_decode_ms": window.hardware_decode.json(),
                "hardware_download_ms": window.hardware_download.json(),
                "hardware_direct_map_ms": window.hardware_direct_map.json(),
                "hardware_conversion_ms": window.hardware_conversion.json(),
                "hardware_rgb_copy_ms": window.hardware_rgb_copy.json(),
                "first_presentation_ms": window.first_presentation.json(),
                "source_switch_presentation_ms": window.source_switch_presentation.json(),
                "decode_queue_ms": window.decode_queue_wait.json(),
                "frame_header_wait_ms": window.frame_header_wait.json(),
                "frame_body_receive_ms": window.frame_body_receive.json(),
                "frame_authentication_ms": window.frame_authentication.json(),
                "sender_control_queue_ms": window.sender.control_wait.json(),
                "sender_input_queue_ms": window.sender.input_wait.json(),
                "sender_bulk_queue_ms": window.sender.bulk_wait.json(),
                "sender_control_write_ms": window.sender.control_write.json(),
                "sender_bulk_write_ms": window.sender.bulk_write.json(),
                "sender_oldest_queued_ms": window.sender.oldest_queued.json(),
            },
        });
        #[cfg(feature = "remote-h264")]
        if let Some(stages) = window.native_x11_stages.json() {
            report["native_x11_stages"] = stages;
        }
        #[cfg(all(feature = "remote-h264", feature = "remote-wayland"))]
        if let Some(stages) = window.native_wayland_stages.json() {
            report["native_wayland_stages"] = stages;
        }
        report
    }

    fn emit(self, final_report: bool) {
        let label = if final_report {
            "viewer final"
        } else {
            "viewer"
        };
        super::ssh_diagnostics::emit_best_effort(format_args!(
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
        ));
    }
}

struct ClientTelemetryReporter<'a> {
    telemetry: &'a ClientTelemetry,
    window_started: Instant,
    finished: bool,
    context: Option<ViewerTelemetryContext>,
    stats_json: bool,
    engine: Option<&'a ConnectToggle>,
}

impl<'a> ClientTelemetryReporter<'a> {
    fn new(
        telemetry: &'a ClientTelemetry,
        context: ViewerTelemetryContext,
        stats_json: bool,
        engine: Option<&'a ConnectToggle>,
    ) -> Self {
        let mut reporter = Self::new_at(telemetry, Instant::now());
        reporter.context = telemetry.detailed.then_some(context);
        reporter.stats_json = stats_json;
        reporter.engine = engine;
        reporter
    }

    fn new_at(telemetry: &'a ClientTelemetry, window_started: Instant) -> Self {
        Self {
            telemetry,
            window_started,
            finished: false,
            context: None,
            stats_json: false,
            engine: None,
        }
    }

    fn emit(&self, report: ClientTelemetryReport, final_report: bool) {
        if let Some(context) = self.context {
            if let Some(engine) = self.engine {
                engine.publish_metrics(report.snapshot(context, final_report));
            }
            if self.stats_json {
                super::ssh_diagnostics::emit_json_line(
                    &report.json(context, final_report).to_string(),
                );
                return;
            }
        }
        report.emit(final_report);
    }

    fn maybe_report(&mut self) {
        if self.finished {
            return;
        }
        if let Some(report) = self.maybe_report_at(Instant::now(), false) {
            self.emit(report, false);
        }
    }

    fn force_report(&mut self) {
        if self.finished {
            return;
        }
        if let Some(report) = self.maybe_report_at(Instant::now(), true) {
            self.emit(report, true);
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

pub(super) struct WakeSender {
    stream: UnixStream,
}

impl WakeSender {
    pub(super) fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    pub(super) fn signal(&self) -> io::Result<()> {
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
    viewer_fd: BorrowedFd<'_>,
    wake_fd: BorrowedFd<'_>,
    deadline: Instant,
) -> io::Result<WaitOutcome> {
    loop {
        let interests = PollFlags::POLLIN | PollFlags::POLLERR | PollFlags::POLLHUP;
        let mut descriptors = [
            PollFd::new(viewer_fd, interests),
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
                    return Ok(WaitOutcome::ViewerHangup);
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
    ViewerHangup,
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

    fn cancel(&mut self, writer: &mut impl RecordWriter, graceful_close: bool) -> RemoteResult<()> {
        if self.cancelled {
            return Ok(());
        }
        self.cancelled = true;
        let await_popup_ack = graceful_close
            && self
                .state
                .wayland_popup
                .lock()
                .unwrap()
                .close_deadline()
                .is_some();
        if !await_popup_ack {
            self.state.stopping.store(true, Ordering::Release);
        }
        let mut first_error = None;
        if graceful_close && !self.close_attempted {
            self.close_attempted = true;
            let close_result: RemoteResult<()> = (|| {
                writer.close()?;
                let mut popup = self.state.wayland_popup.lock().unwrap();
                if let Some(deadline) = popup.close_deadline() {
                    while !popup.close_confirmed() {
                        if !self.state.alive.load(Ordering::Acquire) {
                            return Err(invalid_data(
                                "popup closed without exact revocation acknowledgement",
                            )
                            .into());
                        }
                        let remaining = deadline
                            .checked_duration_since(Instant::now())
                            .filter(|left| !left.is_zero())
                            .ok_or_else(|| {
                                invalid_data("popup revocation acknowledgement timed out")
                            })?;
                        popup = self
                            .state
                            .popup_close_wake
                            .wait_timeout(popup, remaining)
                            .unwrap()
                            .0;
                    }
                }
                Ok(())
            })();
            if let Err(error) = close_result {
                first_error = Some(error);
            }
        }
        self.state.stopping.store(true, Ordering::Release);
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

#[derive(Debug, PartialEq, Eq)]
enum ReceiverExit {
    Completed,
    Panicked,
    TimedOut,
    ReaperFailed(String),
}

struct ReceiverThread {
    handle: Option<thread::JoinHandle<()>>,
}

impl ReceiverThread {
    fn spawn(task: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        Self::spawn_named("jwm-remote-video", task)
    }

    fn spawn_named(name: &str, task: impl FnOnce() + Send + 'static) -> io::Result<Self> {
        let handle = thread::Builder::new().name(name.into()).spawn(task)?;
        Ok(Self {
            handle: Some(handle),
        })
    }

    fn join_timeout(mut self, timeout: Duration) -> ReceiverExit {
        #[cfg(test)]
        if let Some(probe) = cleanup_probe::current() {
            probe.join_started();
        }
        let handle = self.handle.take().expect("receiver handle");
        let joined = match reap_worker(handle) {
            Ok(joined) => joined,
            Err(error) => return ReceiverExit::ReaperFailed(error.to_string()),
        };
        match joined.recv_timeout(timeout) {
            Ok(true) => ReceiverExit::Completed,
            Ok(false) => ReceiverExit::Panicked,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ReceiverExit::TimedOut,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ReceiverExit::ReaperFailed("receiver reaper disconnected".into())
            }
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

#[derive(Default)]
struct ClientPointer {
    repair: PointerRepair,
    scroll_enabled: bool,
    horizontal_enabled: bool,
    #[cfg(test)]
    scroll_observed: u64,
    enabled: bool,
    requested: Option<(super::wayland_pointer_wire::DrawnScope, Instant)>,
    presented: Option<(super::wayland_pointer_wire::DrawnScope, Instant)>,
    latest_decoded: Option<super::wayland_pointer_wire::DrawnScope>,
    // Exact actual NativeProof receipts, bounded independently of peer input.
    // The bool records a received Ready, including one retired without authority.
    presentation_receipts:
        std::collections::VecDeque<(super::wayland_pointer_wire::DrawnScope, bool)>,
    ready: Option<super::wayland_pointer_wire::DrawnScope>,
    consumed: Option<super::wayland_pointer_wire::DrawnScope>,
    epoch: Option<u64>,
    sequence: u64,
    ended: bool,
    terminal_seen: bool,
    newer_frame: bool,
    left_down: bool,
    click: Option<(super::wayland_pointer_wire::DrawnScope, i16, i16, Instant)>,
}
impl ClientPointer {
    fn before_frame(&mut self) {
        self.ready = None;
        self.click = None;
        self.newer_frame = true;
    }
    fn presented(
        &mut self,
        scope: super::wayland_pointer_wire::DrawnScope,
        now: Instant,
    ) -> Option<super::wayland_pointer_wire::PointerRequest> {
        if !self.enabled || self.ended {
            return None;
        }
        self.ready = None;
        self.click = None;
        self.newer_frame = self.latest_decoded.is_some_and(|decoded| {
            (decoded.source_epoch, decoded.geometry_epoch)
                != (scope.source_epoch, scope.geometry_epoch)
                || decoded.frame_sequence > scope.frame_sequence
        });
        if !self
            .presentation_receipts
            .iter()
            .any(|(seen, _)| *seen == scope)
        {
            if self.presentation_receipts.len() == 256 {
                self.presentation_receipts.pop_front();
            }
            self.presentation_receipts.push_back((scope, false));
        }
        self.presented = Some((scope, now));
        self.repair_resolve(now);
        if self.requested.is_none() {
            self.requested = Some((scope, now));
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            pointer_repair::observe_pointer_lifetime(now);
            Some(super::wayland_pointer_wire::PointerRequest { scope })
        } else {
            None
        }
    }
    fn receive(&mut self, value: super::wayland_pointer_wire::PointerStatus) -> RemoteResult<()> {
        use super::wayland_pointer_wire::PointerStatusKind as S;
        let (initial, _) = self
            .requested
            .ok_or_else(|| invalid_data("unsolicited pointer status"))?;
        if self.terminal_seen
            || (value.scope.source_epoch, value.scope.geometry_epoch)
                != (initial.source_epoch, initial.geometry_epoch)
        {
            return Err(invalid_data("pointer status scope mismatch or replay").into());
        }
        if let Some(epoch) = self.epoch
            && epoch != value.lease_epoch
        {
            return Err(invalid_data("pointer lease epoch changed").into());
        }
        if matches!(value.status, S::Denied | S::Revoked) {
            self.repair_check(Instant::now())?;
            #[cfg(test)]
            eprintln!("viewer pointer terminal host status: {value:?}");
            self.ended = true;
            self.repair_end();
            self.terminal_seen = true;
            self.ready = None;
            self.click = None;
            return Ok(());
        }
        self.repair_check(Instant::now())?;
        if let Some(result) = self.repair_expired_status(value) {
            return result;
        }
        self.repair_note_pointer_status(value);
        if !self.ended {
            self.repair_granted(value.lease_epoch)?;
            if value.status == S::Ready
                && self.repair_ordinary_ready(value.scope.into(), Instant::now())?
            {
                return Ok(());
            }
        }
        let Some((presented, at)) = self.presented else {
            return Err(invalid_data("pointer status before presentation").into());
        };
        if value.scope.frame_sequence > presented.frame_sequence {
            return Err(invalid_data("pointer Ready precedes actual presentation").into());
        }
        if self.ended {
            // Local revoke and the peer's already-sent Ready/Waiting cross on
            // the full-duplex stream. Until its terminal acknowledgment, drain
            // only this known lease's previously presented statuses. Never
            // reopen a gate, replace the epoch or accept a new grant here.
            if self.epoch == Some(value.lease_epoch) {
                return Ok(());
            }
            return Err(invalid_data("pointer status revived ended grant").into());
        }
        self.epoch = Some(value.lease_epoch);
        if value.scope != presented {
            let receipt = self
                .presentation_receipts
                .iter_mut()
                .find(|(scope, _)| *scope == value.scope)
                .ok_or_else(|| invalid_data("pointer status lacks exact presentation receipt"))?;
            if value.status == S::Ready {
                if receipt.1 {
                    return Err(invalid_data("pointer Ready replay").into());
                }
                receipt.1 = true;
            }
            return Ok(());
        } // exact stale receipt only; never changes current gate
        match value.status {
            S::Ready => {
                if self.consumed == Some(value.scope) || self.ready == Some(value.scope) {
                    return Err(invalid_data("pointer Ready replay").into());
                }
                if let Some((_, seen)) = self
                    .presentation_receipts
                    .iter_mut()
                    .find(|(scope, _)| *scope == value.scope)
                {
                    if *seen {
                        return Err(invalid_data("pointer Ready replay").into());
                    }
                    *seen = true;
                }
                if !self.newer_frame && at.elapsed() < super::wayland_pointer_wire::FRAME_MAX_AGE {
                    self.ready = Some(value.scope);
                }
            }
            S::Waiting => {
                self.ready = None;
                self.click = None;
            }
            _ => unreachable!(),
        }
        Ok(())
    }
    fn fresh(&self, now: Instant) -> bool {
        !self.ended
            && !self.repair_blocked()
            && !self.newer_frame
            && self.ready.is_some()
            && self.presented.is_some_and(|(scope, at)| {
                Some(scope) == self.ready
                    && now.saturating_duration_since(at)
                        < super::wayland_pointer_wire::FRAME_MAX_AGE
            })
            && self.requested.is_some_and(|(_, at)| {
                now.saturating_duration_since(at) < super::wayland_pointer_wire::LEASE_DURATION
            })
    }
    fn label(&self, now: Instant) -> &'static str {
        if self.ended {
            "Pointer: Revoked"
        } else if self.fresh(now) {
            "Pointer: Ready (one action)"
        } else {
            "Pointer: Waiting for fresh presented frame"
        }
    }
    fn expired(&self, now: Instant) -> bool {
        !self.ended
            && self.requested.is_some_and(|(_, at)| {
                now.saturating_duration_since(at) >= super::wayland_pointer_wire::LEASE_DURATION
            })
    }
    fn revoke(&mut self) -> Option<super::wayland_pointer_wire::PointerRevoke> {
        if self.ended {
            return None;
        }
        self.ended = true;
        self.repair_end();
        self.ready = None;
        self.click = None;
        self.left_down = false;
        self.epoch
            .map(|lease_epoch| super::wayland_pointer_wire::PointerRevoke { lease_epoch })
    }
    fn event(
        &mut self,
        event: &ViewerEvent,
        now: Instant,
    ) -> RemoteResult<Option<super::wayland_pointer_wire::PointerAction>> {
        self.repair_input(event, now);
        self.repair_check(now)?;
        use super::wayland_pointer_wire::PointerActionKind as A;
        use super::x11_viewer::WaylandPointerEventKind as E;
        if let ViewerEvent::WaylandPointerButtonCancel { pressed } = event {
            self.click = None;
            self.left_down = *pressed;
            return Ok(None);
        }
        if matches!(event, ViewerEvent::WaylandPointerCancel) {
            self.click = None;
            return Ok(None);
        }
        let ViewerEvent::WaylandPointer {
            frame_sequence,
            geometry_epoch,
            x,
            y,
            local_x,
            local_y,
            kind,
        } = *event
        else {
            return Ok(None);
        };
        // Track physical state even while no frame is authorized. It must not
        // turn into permission, or disappear when a later Ready arrives.
        if kind == E::LeftDown {
            self.left_down = true;
            self.click = None;
        } else if kind == E::LeftUp {
            self.left_down = false;
        }
        if matches!(
            kind,
            E::ScrollUp | E::ScrollDown | E::ScrollLeft | E::ScrollRight
        ) {
            #[cfg(test)]
            {
                self.scroll_observed += 1;
            }
            if !(if matches!(kind, E::ScrollLeft | E::ScrollRight) {
                self.horizontal_enabled
            } else {
                self.scroll_enabled
            }) || self.left_down
                || self.click.is_some()
                || self.repair_scroll_held()
            {
                self.click = None;
                return Ok(None);
            }
        }
        let Some((scope, _)) = self.presented else {
            return Ok(None);
        };
        if scope.frame_sequence != frame_sequence
            || scope.geometry_epoch != geometry_epoch
            || !self.fresh(now)
        {
            self.click = None;
            if kind == E::LeftUp {
                self.left_down = false;
            }
            return Ok(None);
        }
        let action = match kind {
            E::LeftDown => {
                self.left_down = true;
                self.click = Some((scope, local_x, local_y, now));
                return Ok(None);
            }
            E::LeftUp => {
                self.left_down = false;
                let Some((clicked, cx, cy, at)) = self.click.take() else {
                    return Ok(None);
                };
                if clicked != scope
                    || local_x.abs_diff(cx) > 3
                    || local_y.abs_diff(cy) > 3
                    || now.saturating_duration_since(at) > Duration::from_millis(250)
                {
                    return Ok(None);
                }
                A::ClickLeft
            }
            E::Motion if self.left_down => {
                if self.click.is_some_and(|(_, cx, cy, _)| {
                    local_x.abs_diff(cx) > 3 || local_y.abs_diff(cy) > 3
                }) {
                    self.click = None;
                }
                return Ok(None);
            }
            E::Motion => A::Motion,
            E::ScrollUp => A::ScrollUp,
            E::ScrollDown => A::ScrollDown,
            E::ScrollLeft => A::ScrollLeft,
            E::ScrollRight => A::ScrollRight,
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .filter(|v| *v <= super::wayland_pointer_wire::MAX_SEQUENCE)
            .ok_or_else(|| invalid_data("pointer sequence exhausted"))?;
        self.ready = None;
        self.consumed = Some(scope);
        self.click = None;
        Ok(Some(super::wayland_pointer_wire::PointerAction {
            lease_epoch: self.epoch.unwrap(),
            sequence: self.sequence,
            scope,
            kind: action,
            x,
            y,
        }))
    }
}

#[derive(Default)]
struct ClientKeyboard {
    digest: Option<[u8; 32]>,
    request: Option<(super::wayland_keyboard_wire::LeaseRequest, Instant)>,
    epoch: Option<u64>,
    sequence: u64,
    held: std::collections::BTreeSet<u16>,
    activated: bool,
    ended: bool,
    terminal_received: bool,
}
impl ClientKeyboard {
    #[cfg(feature = "remote-wayland")]
    fn enable(&mut self, digest: [u8; 32]) {
        self.digest = Some(digest);
    }
    fn configured(&self) -> bool {
        self.digest.is_some()
    }
    fn needs_request(&self) -> bool {
        self.configured() && self.request.is_none() && !self.ended
    }
    fn after_presentation_for_purpose(
        &mut self,
        scope: (u64, u64),
        sequence: u64,
        now: Instant,
        purpose: FramePurpose,
    ) -> Option<super::wayland_keyboard_wire::LeaseRequest> {
        if purpose != FramePurpose::NativeProof {
            return None;
        }
        self.after_presentation(scope, sequence, now)
    }
    fn after_presentation(
        &mut self,
        scope: (u64, u64),
        sequence: u64,
        now: Instant,
    ) -> Option<super::wayland_keyboard_wire::LeaseRequest> {
        if !self.needs_request() {
            return None;
        }
        let request = super::wayland_keyboard_wire::LeaseRequest {
            source_epoch: scope.0,
            geometry_epoch: scope.1,
            frame_sequence: sequence,
            keymap_digest: self.digest.unwrap(),
        };
        self.request = Some((request, now));
        Some(request)
    }
    fn receive(&mut self, result: super::wayland_keyboard_wire::LeaseStatus) -> RemoteResult<()> {
        use super::wayland_keyboard_wire::LeaseStatusKind;
        let (request, _) = self
            .request
            .ok_or_else(|| invalid_data("unsolicited keyboard lease status"))?;
        if self.terminal_received
            || (
                result.source_epoch,
                result.geometry_epoch,
                result.frame_sequence,
            ) != (
                request.source_epoch,
                request.geometry_epoch,
                request.frame_sequence,
            )
        {
            return Err(invalid_data("keyboard lease status scope mismatch or replay").into());
        }
        match result.status {
            LeaseStatusKind::Granted
                if !self.ended && self.epoch.is_none() && result.lease_epoch != 0 =>
            {
                self.check_timeout(Instant::now())?;
                self.epoch = Some(result.lease_epoch);
                eprintln!(
                    "jwm-remote: Wayland keyboard host approved; waiting for local focus validation, do not type yet"
                );
            }
            LeaseStatusKind::Denied if !self.ended && self.epoch.is_none() => {
                self.ended = true;
                self.terminal_received = true;
            }
            LeaseStatusKind::Revoked if self.epoch == Some(result.lease_epoch) => {
                self.ended = true;
                self.terminal_received = true;
            }
            _ => return Err(invalid_data("invalid keyboard lease status transition").into()),
        }
        if self.ended {
            self.held.clear();
            eprintln!("jwm-remote: Wayland keyboard lease ended; read-only viewing continues");
        }
        Ok(())
    }
    fn check_timeout(&self, now: Instant) -> RemoteResult<()> {
        if self.ended {
            return Ok(());
        }
        if let Some((_, started)) = self.request {
            let limit = if self.epoch.is_some() {
                super::wayland_keyboard_wire::LEASE_DURATION
            } else {
                Duration::from_secs(5)
            };
            if now.saturating_duration_since(started) >= limit {
                return Err(invalid_data("keyboard lease deadline expired; not renewed").into());
            }
        }
        Ok(())
    }
    fn check_scope(&self, scope: (u64, u64)) -> RemoteResult<()> {
        if !self.ended
            && let Some((request, _)) = self.request
            && scope != (request.source_epoch, request.geometry_epoch)
        {
            return Err(invalid_data("keyboard lease geometry/source changed").into());
        }
        Ok(())
    }
    fn key(
        &mut self,
        keycode: u8,
        pressed: bool,
    ) -> RemoteResult<Option<super::wayland_keyboard_wire::Key>> {
        if self.ended {
            return Ok(None);
        }
        if self.epoch.is_none() || !self.activated {
            if self.configured() {
                // Even balanced edges consumed while waiting must invalidate
                // consent. They cannot silently disappear and allow a later grant.
                self.ended = true;
                return Err(invalid_data(
                    "keyboard input before lease activation; authorization cancelled",
                )
                .into());
            }
            return Ok(None);
        }
        self.check_timeout(Instant::now())?;
        let code = keycode
            .checked_sub(8)
            .filter(|code| *code != 0)
            .ok_or_else(|| invalid_data("invalid XKB keyboard keycode"))?;
        #[cfg(feature = "remote-wayland")]
        if !crate::remote_keyboard_protocol::keyboard_key_allowed(u32::from(code)) {
            return Err(invalid_data("key is outside the native keyboard lease allowlist").into());
        }
        let code = u16::from(code);
        if (pressed && (self.held.len() >= 32 || !self.held.insert(code)))
            || (!pressed && !self.held.remove(&code))
        {
            return Err(invalid_data("keyboard lease key state mismatch").into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .filter(|seq| *seq <= 60000)
            .ok_or_else(|| invalid_data("keyboard lease sequence exhausted"))?;
        let (request, _) = self.request.unwrap();
        Ok(Some(super::wayland_keyboard_wire::Key {
            lease_epoch: self.epoch.unwrap(),
            sequence: self.sequence,
            source_epoch: request.source_epoch,
            geometry_epoch: request.geometry_epoch,
            frame_sequence: request.frame_sequence,
            keycode: code,
            pressed,
        }))
    }
    fn cancel_for_local_event(
        &mut self,
    ) -> RemoteResult<Option<super::wayland_keyboard_wire::KeyboardRevoke>> {
        // Snapshot before revoke mutates ended. A terminal denial is not an
        // outstanding grant, but a real pending request must still fail closed.
        let pending = self.request.is_some()
            && self.epoch.is_none()
            && !self.ended
            && !self.terminal_received;
        let record = self.revoke();
        if pending {
            return Err(invalid_data("keyboard lease cancelled while grant was pending").into());
        }
        Ok(record)
    }
    fn revoke(&mut self) -> Option<super::wayland_keyboard_wire::KeyboardRevoke> {
        if self.ended {
            return None;
        }
        self.ended = true;
        self.held.clear();
        self.epoch
            .map(|lease_epoch| super::wayland_keyboard_wire::KeyboardRevoke { lease_epoch })
    }
}

#[derive(Default)]
struct ClientResize {
    follow: Option<super::follow_source_size::FollowSourceSize>,
    requested: Option<super::window_size::WindowSize>,
    pending: Option<(super::window_resize::ResizeRequest, Instant)>,
}
impl ClientResize {
    fn after_presentation(
        &mut self,
        scope: (u64, u64),
        now: Instant,
    ) -> Option<super::window_resize::ResizeRequest> {
        let size = self.requested.take()?;
        let request = super::window_resize::ResizeRequest {
            request_id: 1,
            source_epoch: scope.0,
            geometry_epoch: scope.1,
            width: size.width,
            height: size.height,
        };
        self.pending = Some((request, now));
        Some(request)
    }
    fn check_timeout(&self, now: Instant) -> RemoteResult<()> {
        if self.pending.as_ref().is_some_and(|(_, start)| {
            now.saturating_duration_since(*start) >= Duration::from_secs(5)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "explicit source resize result timed out; not retried",
            )
            .into());
        }
        Ok(())
    }
    fn receive(&mut self, result: super::window_resize::ResizeResult) -> RemoteResult<()> {
        if let Some(follow) = self.follow.as_mut() {
            return follow.receive(result, Instant::now());
        }
        let (request, _) = self
            .pending
            .as_ref()
            .ok_or_else(|| invalid_data("unsolicited source resize result"))?;
        if (
            result.request_id,
            result.source_epoch,
            result.geometry_epoch,
        ) != (
            request.request_id,
            request.source_epoch,
            request.geometry_epoch,
        ) {
            return Err(invalid_data("source resize result scope mismatch").into());
        }
        self.pending = None;
        eprintln!(
            "jwm-remote: source resize {:?}: {}x{} (capture geometry {})",
            result.status, result.width, result.height, result.observed_geometry_epoch
        );
        Ok(())
    }
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone)]
struct ResumableTestDriver {
    commands: Arc<Mutex<std::collections::VecDeque<bool>>>,
    observations: Arc<Mutex<Vec<super::resumable_control::Record>>>,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static RESUMABLE_TEST_DRIVER: std::cell::RefCell<Option<ResumableTestDriver>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_resumable_control_test_driver<T>(
    commands: Arc<Mutex<std::collections::VecDeque<bool>>>,
    observations: Arc<Mutex<Vec<super::resumable_control::Record>>>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RESUMABLE_TEST_DRIVER.with(|slot| slot.borrow_mut().take());
        }
    }
    RESUMABLE_TEST_DRIVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(ResumableTestDriver {
            commands,
            observations,
        });
    });
    let _reset = Reset;
    run()
}

#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
#[derive(Clone)]
struct ResumableAckBarrier {
    reached: std::sync::mpsc::SyncSender<()>,
    release: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
}
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
thread_local! { static RESUMABLE_ACK_BARRIER: std::cell::RefCell<Option<ResumableAckBarrier>> = const { std::cell::RefCell::new(None) }; }
#[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
pub(crate) fn with_resumable_control_ack_barrier<T>(
    reached: std::sync::mpsc::SyncSender<()>,
    release: std::sync::mpsc::Receiver<()>,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            RESUMABLE_ACK_BARRIER.with(|slot| slot.borrow_mut().take());
        }
    }
    RESUMABLE_ACK_BARRIER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(ResumableAckBarrier {
            reached,
            release: Arc::new(Mutex::new(Some(release))),
        });
    });
    let _reset = Reset;
    run()
}

struct ReceiveState {
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    resumable_ack_barrier: Option<ResumableAckBarrier>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    resumable_test_driver: Option<ResumableTestDriver>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    scroll_decode_observer: Option<ScrollDecodeObserver>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    lossless_observer: Option<LosslessFrameObserver>,
    #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
    purpose_observer: Option<FramePurposeObserver>,
    wayland_text: Mutex<ClientText>,
    wayland_control: Mutex<ClientControl>,
    wayland_popup: Mutex<ClientPopup>,
    popup_close_wake: std::sync::Condvar,
    resize: Mutex<ClientResize>,
    wayland_keyboard: Mutex<ClientKeyboard>,
    wayland_pointer: Mutex<ClientPointer>,
    clipboard_restriction: Mutex<Option<RestrictionState>>,
    catalog: Mutex<Option<SourceCatalog>>,
    source_epoch: AtomicU64,
    session_epoch: u128,
    frame_limits: super::messages::FrameLimits,
    frame_codec: FrameCodec,
    #[cfg(feature = "remote-h264")]
    native_video: Option<native_video::Decode>,
    #[cfg(feature = "remote-h264")]
    vaapi_direct_map: bool,
    read_ahead: bool,
    scroll_copy: bool,
    static_lossless: bool,
    cursor_enabled: AtomicBool,
    cursor: Mutex<super::cursor::CursorInbox>,
    latest: Mutex<PresentationMailbox>,
    error: Mutex<Option<RemoteError>>,
    alive: AtomicBool,
    stopping: AtomicBool,
    telemetry: ClientTelemetry,
    decode_pool: SharedDecodeBufferPool,
}

impl ReceiveState {
    /// Source transitions rebuild all canvas/reference state while retaining
    /// only the immutable negotiated codec permission.
    fn new_tile_decoder(&self) -> RemoteResult<TileDecoder> {
        let mut decoder = TileDecoder::with_frame_limits(self.frame_limits)?;
        decoder.set_scroll_copy_enabled(self.scroll_copy);
        decoder.set_lossless_enabled(self.static_lossless);
        Ok(decoder)
    }

    fn new_video_decoder(&self) -> RemoteResult<SessionVideoDecoder> {
        Ok(self.new_tile_decoder()?.into())
    }

    fn new() -> Self {
        Self {
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            resumable_test_driver: RESUMABLE_TEST_DRIVER.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            resumable_ack_barrier: RESUMABLE_ACK_BARRIER.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            lossless_observer: LOSSLESS_FRAME_OBSERVER.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            scroll_decode_observer: SCROLL_DECODE_OBSERVER.with(|slot| slot.borrow().clone()),
            #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
            purpose_observer: FRAME_PURPOSE_OBSERVER.with(|slot| slot.borrow().clone()),
            wayland_text: Mutex::new(ClientText::default()),
            wayland_control: Mutex::new(ClientControl::default()),
            wayland_popup: Mutex::new(ClientPopup::default()),
            popup_close_wake: std::sync::Condvar::new(),
            resize: Mutex::new(ClientResize::default()),
            wayland_keyboard: Mutex::new(ClientKeyboard::default()),
            wayland_pointer: Mutex::new(ClientPointer::default()),
            clipboard_restriction: Mutex::new(None),
            catalog: Mutex::new(None),
            source_epoch: AtomicU64::new(1),
            session_epoch: 0,
            frame_limits: super::messages::FrameLimits::default(),
            frame_codec: FrameCodec::JpegTiles,
            #[cfg(feature = "remote-h264")]
            native_video: None,
            #[cfg(feature = "remote-h264")]
            vaapi_direct_map: false,
            read_ahead: false,
            scroll_copy: false,
            static_lossless: false,
            cursor_enabled: AtomicBool::new(false),
            cursor: Mutex::new(super::cursor::CursorInbox::default()),
            latest: Mutex::new(PresentationMailbox::default()),
            error: Mutex::new(None),
            alive: AtomicBool::new(true),
            stopping: AtomicBool::new(false),
            telemetry: ClientTelemetry::default(),
            decode_pool: new_decode_buffer_pool(),
        }
    }
}

fn accept_source_epoch(received: u64, current: u64) -> RemoteResult<bool> {
    if received > current {
        return Err(invalid_data("source payload precedes SourceChanged").into());
    }
    Ok(received == current)
}

fn receive_catalog(state: &ReceiveState, payload: &[u8]) -> RemoteResult<()> {
    let catalog = SourceCatalog::decode(payload)?;
    let mut slot = state.catalog.lock().unwrap();
    if slot.is_some() || catalog.current.epoch != 1 {
        return Err(invalid_data("unexpected source catalog").into());
    }
    *slot = Some(catalog);
    Ok(())
}

fn current_source_label(state: &ReceiveState) -> String {
    state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|catalog| {
            catalog
                .entries
                .iter()
                .find(|entry| entry.id == catalog.current.source)
                .map(|entry| entry.label.clone())
        })
        .unwrap_or_else(|| "source".into())
}

fn receive_cursor(state: &ReceiveState, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
    if !state.cursor_enabled.load(Ordering::Acquire) {
        return Err(invalid_data("independent cursor was not negotiated").into());
    }
    let (epoch, payload) = decode_source_payload(payload)?;
    let mut inbox = state.cursor.lock().unwrap();
    if !accept_source_epoch(epoch, state.source_epoch.load(Ordering::Acquire))? {
        return Ok(());
    }
    match kind {
        MessageKind::CursorShape => inbox.shape(super::cursor::CursorShape::decode(payload)?),
        MessageKind::CursorPosition => {
            inbox.position(super::cursor::CursorPosition::decode(payload)?)?;
            state
                .telemetry
                .record_cursor_position(epoch, Instant::now());
            Ok(())
        }
        _ => Err(invalid_data("unexpected cursor record").into()),
    }
}

fn take_cursor_for_epoch(
    state: &ReceiveState,
    epoch: u64,
) -> Option<(
    Arc<super::cursor::CursorShape>,
    super::cursor::CursorPosition,
)> {
    let mut cursor = state.cursor.lock().unwrap();
    if state.source_epoch.load(Ordering::Acquire) != epoch {
        return None;
    }
    cursor.take()
}

fn apply_pending_cursor(viewer: &mut Viewer, state: &ReceiveState, epoch: u64) -> RemoteResult<()> {
    let update = take_cursor_for_epoch(state, epoch);
    if let Some((shape, position)) = update {
        viewer.set_cursor_update(shape, position)?;
    }
    Ok(())
}

fn take_latest(state: &ReceiveState, epoch: u64) -> Option<QueuedFrame> {
    state.latest.lock().unwrap().take(epoch)
}

struct SessionVideoDecoder {
    jpeg: TileDecoder,
    #[cfg(feature = "remote-h264")]
    hardware: Option<(HardwareFrameHeader, super::h264::Decoder)>,
    #[cfg(feature = "remote-h264")]
    geometry_epoch: Option<u64>,
}

impl From<TileDecoder> for SessionVideoDecoder {
    fn from(jpeg: TileDecoder) -> Self {
        Self {
            jpeg,
            #[cfg(feature = "remote-h264")]
            hardware: None,
            #[cfg(feature = "remote-h264")]
            geometry_epoch: None,
        }
    }
}

impl SessionVideoDecoder {
    fn active_codec(&self, _state: &ReceiveState) -> FrameCodec {
        #[cfg(feature = "remote-h264")]
        if self.hardware.is_some() {
            return _state.frame_codec;
        }
        FrameCodec::JpegTiles
    }

    fn reset_source(&mut self, state: &ReceiveState) -> RemoteResult<()> {
        self.jpeg = state.new_tile_decoder()?;
        #[cfg(feature = "remote-h264")]
        {
            self.geometry_epoch = None;
        }
        Ok(())
    }

    #[cfg(not(feature = "remote-h264"))]
    fn decode_hardware_session(
        &mut self,
        _state: &ReceiveState,
        _payload: &[u8],
    ) -> RemoteResult<QueuedFrame> {
        Err(invalid_data("hardware video was not compiled in").into())
    }

    #[cfg(feature = "remote-h264")]
    fn decode_hardware_session(
        &mut self,
        state: &ReceiveState,
        payload: &[u8],
    ) -> RemoteResult<QueuedFrame> {
        let frame = match h264_wire::decode_video_payload(payload, state.frame_limits)? {
            VideoPayload::Jpeg(payload) => {
                if self.hardware.take().is_some() {
                    // Only a full JPEG keyframe can establish the canvas after
                    // a temporal stream. No earlier JPEG reference survives.
                    self.jpeg = state.new_tile_decoder()?;
                    eprintln!("jwm-remote: host switched to JPEG video");
                }
                decode_queued_frame(
                    &mut self.jpeg,
                    payload,
                    FramePurpose::Video,
                    &state.telemetry,
                    &state.decode_pool,
                )?
            }
            VideoPayload::Hardware {
                codec,
                header,
                annex_b,
            } => {
                if codec != state.frame_codec {
                    return Err(invalid_data(
                        "frame codec differs from the negotiated video profile",
                    )
                    .into());
                }
                self.decode_hardware(state, header, annex_b)?
            }
        };
        if self
            .geometry_epoch
            .is_some_and(|epoch| frame.frame.geometry_epoch() < epoch)
        {
            return Err(invalid_data("video geometry epoch moved backwards").into());
        }
        self.geometry_epoch = Some(frame.frame.geometry_epoch());
        Ok(frame)
    }

    #[cfg(feature = "remote-h264")]
    fn decode_hardware(
        &mut self,
        state: &ReceiveState,
        header: HardwareFrameHeader,
        annex_b: &[u8],
    ) -> RemoteResult<QueuedFrame> {
        if self
            .geometry_epoch
            .is_some_and(|epoch| header.geometry_epoch < epoch)
        {
            return Err(invalid_data("video geometry epoch moved backwards").into());
        }
        let decode_started = Instant::now();
        let dimensions = (
            u32::from(header.encoded_width),
            u32::from(header.encoded_height),
        );
        let (padded_w, padded_h) =
            h264_wire::h264_padded_dimensions(header.encoded_width, header.encoded_height)?;
        let padded = (u32::from(padded_w), u32::from(padded_h));
        let reset = self.geometry_epoch.is_none()
            || self.hardware.as_ref().is_none_or(|(previous, _)| {
                previous.geometry_epoch != header.geometry_epoch
                    || previous.source_width != header.source_width
                    || previous.source_height != header.source_height
                    || previous.encoded_width != header.encoded_width
                    || previous.encoded_height != header.encoded_height
            });
        if reset {
            if !h264_wire::hardware_has_idr(state.frame_codec, annex_b) {
                return Err(
                    invalid_data("new video frame geometry requires an IDR picture").into(),
                );
            }
            if let Some((previous, decoder)) = self
                .hardware
                .as_mut()
                .filter(|(_, decoder)| decoder.dimensions() == padded)
            {
                decoder.reset();
                *previous = header;
            } else {
                self.hardware = None;
                let decoder = match state
                    .native_video
                    .as_ref()
                    .and_then(|native| native.open(padded))
                {
                    Some(decoder) => decoder,
                    None => {
                        let mut decoder =
                            super::h264::Decoder::new(state.frame_codec, padded.0, padded.1)
                                .map_err(hardware_decoder_error)?;
                        if state.vaapi_direct_map {
                            decoder.enable_vaapi_map();
                        }
                        decoder
                    }
                };
                self.hardware = Some((header, decoder));
            }
        }
        let (_, decoder) = self
            .hardware
            .as_mut()
            .expect("hardware decoder initialized");
        if state
            .native_video
            .as_ref()
            .is_some_and(native_video::Decode::active)
        {
            let (pixels, timings) = decoder
                .decode_vaapi_surface(annex_b, state.telemetry.detailed)
                .map_err(hardware_decoder_error)?;
            state.telemetry.record_hardware_decode(timings);
            let ready_at = Instant::now();
            state.telemetry.record_decoded_at(decode_started, ready_at);
            return Ok(QueuedFrame {
                purpose: FramePurpose::Video,
                frame: FrameLease::Vaapi {
                    pixels,
                    info: FrameInfo {
                        session_epoch: state.session_epoch,
                        source_epoch: 0,
                        geometry_epoch: header.geometry_epoch,
                        sequence: header.sequence,
                        source_size: (header.source_width, header.source_height),
                        encoded_size: dimensions,
                    },
                },
                ready_at,
                decode_elapsed: ready_at.saturating_duration_since(decode_started),
            });
        }
        // Feed all authenticated access units in arrival order. The viewer
        // may skip presentation after decoding, so pictures used as future
        // H.264 references must not be skipped at this boundary.
        let rgb_bytes = dimensions.0 as usize * dimensions.1 as usize * 3;
        let (mut pixels, checkout) = super::frame::prepare_decode_buffer(
            &state.decode_pool,
            rgb_bytes,
            state.telemetry.detailed,
        )?;
        let timings = decoder
            .decode_into(annex_b, dimensions, &mut pixels, state.telemetry.detailed)
            .map_err(hardware_decoder_error)?
            .with_checkout(checkout);
        let rgb = image::RgbImage::from_raw(dimensions.0, dimensions.1, pixels)
            .ok_or_else(|| invalid_data("decoded hardware frame has invalid RGB dimensions"))?;
        state.telemetry.record_hardware_decode(timings);
        if reset {
            eprintln!(
                "jwm-remote: hardware {} decoding active ({}x{})",
                state.frame_codec.name(),
                dimensions.0,
                dimensions.1
            );
        }
        let ready_at = Instant::now();
        state.telemetry.record_decoded_at(decode_started, ready_at);
        Ok(QueuedFrame {
            frame: FrameLease::Cpu(
                RecyclableDecodedFrame::new(
                    header.sequence,
                    header.source_width,
                    header.source_height,
                    rgb,
                    Arc::clone(&state.decode_pool),
                )
                .with_geometry_epoch(header.geometry_epoch),
            ),
            ready_at,
            decode_elapsed: ready_at.saturating_duration_since(decode_started),
            purpose: FramePurpose::Video,
        })
    }
}

fn decode_popup_queued_frame(
    state: &ReceiveState,
    decoder: &mut SessionVideoDecoder,
    payload: &[u8],
    purpose: FramePurpose,
    epoch: u64,
) -> RemoteResult<QueuedFrame> {
    let enabled = state.wayland_popup.lock().unwrap().enabled;
    if enabled != (purpose == FramePurpose::PopupFamilyProof) {
        return Err(invalid_data("popup frame purpose was not negotiated").into());
    }
    if state.frame_codec.is_hardware() {
        if purpose != FramePurpose::Video {
            return Err(invalid_data("hardware session requires the video frame purpose").into());
        }
        return decoder.decode_hardware_session(state, payload);
    }
    let (metadata, payload) = if enabled {
        let prefix = super::wayland_popup_wire::METADATA_LEN;
        if payload.len() < prefix {
            return Err(invalid_data("short popup frame metadata").into());
        }
        (
            Some(super::wayland_popup_wire::Metadata::decode(
                &payload[..prefix],
            )?),
            &payload[prefix..],
        )
    } else {
        (None, payload)
    };
    let queued = decode_queued_frame(
        &mut decoder.jpeg,
        payload,
        purpose,
        &state.telemetry,
        &state.decode_pool,
    )?;
    if let Some(metadata) = metadata {
        state
            .wayland_popup
            .lock()
            .unwrap()
            .commit_decoded_or_drain(
                metadata,
                epoch,
                purpose,
                queued
                    .frame
                    .cpu()
                    .ok_or_else(|| invalid_data("popup frame requires CPU pixels"))?,
                Instant::now(),
            )?;
    }
    Ok(queued)
}

fn decode_queued_frame(
    decoder: &mut TileDecoder,
    payload: &[u8],
    purpose: FramePurpose,
    telemetry: &ClientTelemetry,
    pool: &SharedDecodeBufferPool,
) -> RemoteResult<QueuedFrame> {
    let decode_started = Instant::now();
    // Deltas are applied in arrival order into the decoder's own canvas, so a
    // frame the viewer later drops from its one-slot queue still contributes
    // its tiles. Only presentation is allowed to skip frames, never decoding.
    let frame = decoder.decode_into(payload, Arc::clone(pool))?;
    if decoder.last_frame_was_lossless() != (purpose == FramePurpose::VisualRepair) {
        return Err(
            invalid_data("PNG codec and authenticated visual-repair purpose must match").into(),
        );
    }
    let ready_at = Instant::now();
    telemetry.record_decoded_at(decode_started, ready_at);
    Ok(QueuedFrame {
        frame: FrameLease::Cpu(frame),
        ready_at,
        decode_elapsed: ready_at.saturating_duration_since(decode_started),
        purpose,
    })
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
        let error = result.err();
        self.publish_terminal(error);
    }

    fn publish_terminal(&mut self, error: Option<RemoteError>) {
        if self.finished {
            return;
        }
        // Mark first so even a poisoned diagnostic lock cannot recurse into
        // terminal publication while unwinding.
        self.finished = true;
        let mut slot = self
            .state
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.state.alive.load(Ordering::Acquire) {
            return;
        }
        if let Some(error) = error
            && !self.state.stopping.load(Ordering::Acquire)
        {
            *slot = Some(error);
        }
        self.state.alive.store(false, Ordering::Release);
        drop(slot);
        self.state.popup_close_wake.notify_all();
        let _ = self.wake.signal();
    }
}

impl Drop for ReceiveCompletion {
    fn drop(&mut self) {
        if !self.finished {
            self.publish_terminal(Some(
                io::Error::other("remote video thread panicked").into(),
            ));
        }
    }
}

fn receive_frames(
    mut reader: SessionReader<TcpStream>,
    state: Arc<ReceiveState>,
    last_sequence: u64,
    payload: Vec<u8>,
    wake: WakeSender,
    video_decoder: SessionVideoDecoder,
    clipboard: Option<ClipboardInbox>,
) {
    if state.read_ahead {
        receive_with_read_ahead(
            reader,
            state,
            last_sequence,
            payload,
            wake,
            video_decoder,
            clipboard,
        );
        return;
    }
    receive_frame_records(
        |payload, remaining| {
            reader.get_ref().set_read_timeout(Some(remaining))?;
            state
                .telemetry
                .read_record(&mut reader, payload)
                .map_err(receiver_read_error)
        },
        Arc::clone(&state),
        last_sequence,
        payload,
        wake,
        video_decoder,
        clipboard,
    );
}

struct StopReadAhead<'a> {
    socket: TcpStream,
    queue: &'a super::client_decode_queue::DecodeQueue,
}

impl Drop for StopReadAhead<'_> {
    fn drop(&mut self) {
        self.queue.close();
        let _ = self.socket.shutdown(Shutdown::Read);
    }
}

struct CloseDecodeQueue<'a>(&'a super::client_decode_queue::DecodeQueue);

impl Drop for CloseDecodeQueue<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn receive_with_read_ahead(
    mut reader: SessionReader<TcpStream>,
    state: Arc<ReceiveState>,
    last_sequence: u64,
    payload: Vec<u8>,
    wake: WakeSender,
    video_decoder: SessionVideoDecoder,
    clipboard: Option<ClipboardInbox>,
) {
    let queue = super::client_decode_queue::DecodeQueue::with_video_deadline(
        Instant::now() + VIDEO_FRAME_IDLE_TIMEOUT,
    );
    let mut setup_completion = ReceiveCompletion::new(Arc::clone(&state), wake);
    let result: RemoteResult<()> = thread::scope(|scope| {
        let cancel = StopReadAhead {
            socket: reader.get_ref().try_clone()?,
            queue: &queue,
        };
        // The deadline worker interrupts the socket independently of both
        // decoder progress and partial reads of an authenticated record.
        reader.get_ref().set_read_timeout(None)?;
        let deadline_socket = cancel.socket.try_clone()?;
        let deadline_wake = setup_completion.wake.try_clone()?;
        let reader_wake = setup_completion.wake.try_clone()?;
        let mut read_payload = payload;
        let queue_ref = &queue;
        let reader_state = &state;
        let deadline = thread::Builder::new()
            .name("jwm-remote-deadline".into())
            .spawn_scoped(scope, move || {
                if let Some(reason) = queue_ref.wait_for_expiry() {
                    let mut completion =
                        ReceiveCompletion::new(Arc::clone(reader_state), deadline_wake);
                    // Publish the cause before interrupting the read so the
                    // resulting EOF cannot become a reconnectable failure.
                    completion.finish(Err(reason.error().into()));
                    queue_ref.close();
                    let _ = deadline_socket.shutdown(Shutdown::Read);
                }
            })?;
        let producer = thread::Builder::new()
            .name("jwm-remote-reader".into())
            .spawn_scoped(scope, move || {
                let _queue_closer = CloseDecodeQueue(queue_ref);
                let mut completion = ReceiveCompletion::new(Arc::clone(reader_state), reader_wake);
                let result: RemoteResult<()> = (|| {
                    let mut wire_epoch = reader_state.source_epoch.load(Ordering::Acquire);
                    let mut retention = PayloadBufferRetention::default();
                    while reader_state.alive.load(Ordering::Acquire)
                        && !reader_state.stopping.load(Ordering::Acquire)
                    {
                        let kind = reader_state
                            .telemetry
                            .read_record(&mut reader, &mut read_payload)
                            .map_err(|error| {
                                if matches!(&error, ProtocolError::Io(_))
                                    && let Some(reason) = queue_ref.expired_at(Instant::now())
                                {
                                    return Box::new(reason.error()) as RemoteError;
                                }
                                receiver_read_error(error)
                            })?;
                        let authenticated_at = Instant::now();
                        queue_ref.record_authenticated(
                            kind,
                            authenticated_at,
                            VIDEO_FRAME_IDLE_TIMEOUT,
                        )?;
                        match kind {
                            MessageKind::Close | MessageKind::SessionClose => {
                                return Err(PeerClosed::decode(kind, &read_payload)?.into());
                            }
                            MessageKind::CursorShape | MessageKind::CursorPosition
                                if wire_epoch
                                    == reader_state.source_epoch.load(Ordering::Acquire)
                                    && queue_ref.can_dispatch_cursor() =>
                            {
                                receive_cursor(reader_state, kind, &read_payload)?;
                                completion.signal()?;
                                retention.observe(&mut read_payload);
                                continue;
                            }
                            MessageKind::SourceChanged => {
                                let changed = decode_source_changed(&read_payload)?;
                                if wire_epoch.checked_add(1) != Some(changed.epoch) {
                                    return Err(invalid_data("invalid source change order").into());
                                }
                                wire_epoch = changed.epoch;
                            }
                            _ => {}
                        }
                        queue_ref.push(kind, &mut read_payload, authenticated_at, || {
                            reader_state.stopping.load(Ordering::Acquire)
                                || !reader_state.alive.load(Ordering::Acquire)
                        })?;
                    }
                    Ok(())
                })();
                completion.finish(result);
            })?;
        let mut previous = None;
        receive_frame_records(
            |payload, remaining| {
                if let Some(kind) = previous.take() {
                    queue.handled(kind);
                }
                let (kind, timing) = queue.pop(payload, remaining)?;
                previous = Some(kind);
                state.telemetry.record_decode_queue(kind, timing);
                Ok(kind)
            },
            Arc::clone(&state),
            last_sequence,
            Vec::new(),
            setup_completion.wake.try_clone()?,
            video_decoder,
            clipboard,
        );
        drop(cancel);
        producer
            .join()
            .map_err(|_| io::Error::other("remote authenticated reader panicked"))?;
        deadline
            .join()
            .map_err(|_| io::Error::other("remote receive deadline worker panicked"))?;
        Ok(())
    });
    setup_completion.finish(result);
}

/// Record handling is shared by TCP and authenticated in-memory fixtures.
fn receive_frame_records(
    mut read_record: impl FnMut(&mut Vec<u8>, Duration) -> RemoteResult<MessageKind>,
    state: Arc<ReceiveState>,
    mut last_sequence: u64,
    mut payload: Vec<u8>,
    wake: WakeSender,
    video_decoder: impl Into<SessionVideoDecoder>,
    clipboard: Option<ClipboardInbox>,
) {
    let mut video_decoder = video_decoder.into();
    let mut completion = ReceiveCompletion::new(Arc::clone(&state), wake);
    let mut payload_retention = PayloadBufferRetention::default();
    let mut last_video_at = Instant::now();
    let result: RemoteResult<()> = (|| loop {
        // Independent cursor/clipboard activity must not keep dead video alive.
        let remaining = VIDEO_FRAME_IDLE_TIMEOUT
            .checked_sub(last_video_at.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "host stopped sending video frames")
            })?;
        let remaining = state
            .wayland_popup
            .lock()
            .unwrap()
            .close_deadline()
            .map_or(remaining, |deadline| {
                remaining.min(deadline.saturating_duration_since(Instant::now()))
            });
        if remaining.is_zero() {
            return Err(invalid_data("popup close acknowledgement timed out").into());
        }
        let kind = read_record(&mut payload, remaining)?;
        {
            let mut popup = state.wayland_popup.lock().unwrap();
            if popup.close_deadline().is_some() {
                let done = popup.drain_close_record(kind, &payload, Instant::now())?;
                drop(popup);
                state.popup_close_wake.notify_all();
                if done {
                    return Ok(());
                }
                continue;
            }
        }
        match kind {
            MessageKind::WaylandPopupStatus => {
                state
                    .wayland_popup
                    .lock()
                    .unwrap()
                    .status_or_drain(&payload, Instant::now())?;
                completion.signal()?;
            }
            MessageKind::WaylandControlActionCompleted => {
                state.wayland_control.lock().unwrap().repair_completed(
                    super::control_repair_wire::ActionReceipt::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandControlRepairOffer => {
                state.wayland_control.lock().unwrap().repair_offer(
                    super::control_repair_wire::Token::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandControlRepairStatus => {
                state.wayland_control.lock().unwrap().repair_status(
                    super::control_repair_wire::RepairStatus::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandResumableControl => {
                let record = super::resumable_control::Record::decode(&payload)?;
                // Test-only scheduling barrier: the real ACK has arrived, but
                // no lease lock or state transition is held while local input
                // is queued. This neither forges an ACK nor changes a deadline.
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if record.kind == super::resumable_control::Kind::Resumed
                    && let Some(barrier) = &state.resumable_ack_barrier
                    && let Some(release) = barrier.release.lock().unwrap().take()
                {
                    barrier
                        .reached
                        .try_send(())
                        .map_err(|_| invalid_data("resume ACK test barrier notification failed"))?;
                    release
                        .recv_timeout(Duration::from_secs(2))
                        .map_err(|_| invalid_data("resume ACK test barrier release timed out"))?;
                }
                let mut control = state.wayland_control.lock().unwrap();
                control.receive_resumable_ack(record, Instant::now())?;
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(driver) = &state.resumable_test_driver {
                    driver.observations.lock().unwrap().push(record);
                }
                completion.signal()?;
            }
            MessageKind::WaylandControlStatus => {
                state.wayland_control.lock().unwrap().receive(
                    super::wayland_control_wire::ControlStatus::decode(&payload)?,
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandTextResult => {
                state
                    .wayland_text
                    .lock()
                    .unwrap()
                    .receive(super::wayland_text_wire::TextResult::decode(&payload)?)?;
                completion.signal()?;
            }
            MessageKind::WaylandPointerActionCompleted => {
                state.wayland_pointer.lock().unwrap().repair_completed(
                    super::pointer_repair_phase::ActionReceipt::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandPointerRepairOffer => {
                state.wayland_pointer.lock().unwrap().repair_offer(
                    super::pointer_repair_phase::Token::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandPointerRepairStatus => {
                state.wayland_pointer.lock().unwrap().repair_status(
                    super::pointer_repair_wire::RepairStatus::decode(&payload)?,
                    Instant::now(),
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandPointerStatus => {
                state.wayland_pointer.lock().unwrap().receive(
                    super::wayland_pointer_wire::PointerStatus::decode(&payload)?,
                )?;
                completion.signal()?;
            }
            MessageKind::WaylandKeyboardStatus => {
                state
                    .wayland_keyboard
                    .lock()
                    .unwrap()
                    .receive(super::wayland_keyboard_wire::LeaseStatus::decode(&payload)?)?;
                completion.signal()?;
            }
            MessageKind::WindowResizeResult => {
                state
                    .resize
                    .lock()
                    .unwrap()
                    .receive(super::window_resize::ResizeResult::decode(&payload)?)?;
                completion.signal()?;
            }
            MessageKind::ClipboardRestricted => {
                let request = ClipboardRestriction::decode(&payload)?;
                state
                    .clipboard_restriction
                    .lock()
                    .unwrap()
                    .as_mut()
                    .ok_or_else(|| {
                        invalid_data("clipboard restriction acknowledgement without consent")
                    })?
                    .acknowledge(request)?;
                completion.signal()?;
            }
            MessageKind::SourceChanged => {
                let changed = decode_source_changed(&payload)?;
                {
                    let mut catalog = state.catalog.lock().unwrap();
                    let catalog = catalog
                        .as_mut()
                        .ok_or_else(|| invalid_data("source change before catalog"))?;
                    if changed.epoch
                        != catalog
                            .current
                            .epoch
                            .checked_add(1)
                            .ok_or_else(|| invalid_data("source epoch exhausted"))?
                        || !catalog
                            .entries
                            .iter()
                            .any(|entry| entry.id == changed.source)
                    {
                        return Err(invalid_data("invalid source change").into());
                    }
                    catalog.current = changed;
                }
                video_decoder.reset_source(&state)?;
                let stale = std::mem::take(&mut *state.latest.lock().unwrap());
                drop(stale);
                let mut cursor = state.cursor.lock().unwrap();
                *cursor = super::cursor::CursorInbox::default();
                if state.wayland_popup.lock().unwrap().enabled {
                    return Err(
                        invalid_data("popup source cannot change during the session").into(),
                    );
                }
                state.source_epoch.store(changed.epoch, Ordering::Release);
                drop(cursor);
                state
                    .telemetry
                    .expect_first_frame(changed.epoch, Instant::now());
                completion.signal()?;
            }
            MessageKind::Frame => {
                let (epoch, purpose, inner) = decode_frame_payload(&payload)?;
                if !accept_source_epoch(epoch, state.source_epoch.load(Ordering::Acquire))? {
                    continue;
                }
                // read_message_into authenticated the complete record before
                // returning Frame, so pool checkout starts only on trusted
                // video header and codec payload.
                state.telemetry.record_received();
                let mut queued =
                    decode_popup_queued_frame(&state, &mut video_decoder, inner, purpose, epoch)?;
                let publication_gate = state.read_ahead.then(|| state.error.lock().unwrap());
                if publication_gate.is_some()
                    && (!state.alive.load(Ordering::Acquire)
                        || state.stopping.load(Ordering::Acquire))
                {
                    return Ok(());
                }
                // Serialize the final publication with local revoke. Decode
                // can run outside this lock, but a close during decode must
                // never publish a new frame or mutate input proof state.
                let mut popup_publication = state.wayland_popup.lock().unwrap();
                if popup_publication.close_deadline().is_some() {
                    popup_publication.drain_close_record(
                        MessageKind::Frame,
                        &payload,
                        Instant::now(),
                    )?;
                    continue;
                }
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(observer) = &state.lossless_observer {
                    observer.lock().unwrap().push((
                        queued.frame.sequence(),
                        video_decoder.jpeg.last_frame_was_lossless(),
                    ));
                }
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if let Some(observer) = &state.purpose_observer {
                    let mut rows = observer.lock().unwrap();
                    assert!(rows.len() < 4096);
                    rows.push((queued.frame.sequence(), queued.purpose));
                }
                queued.frame = queued.frame.with_scope(state.session_epoch, epoch);
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
                state
                    .telemetry
                    .record_frame_scope(&queued.frame, video_decoder.active_codec(&state));
                #[cfg(all(test, feature = "remote-wayland", feature = "backend-wayland-udev"))]
                if state.frame_codec == FrameCodec::JpegTiles
                    && let Some(cpu) = queued.frame.cpu()
                {
                    observe_decoded_scroll(
                        &state.scroll_decode_observer,
                        inner,
                        epoch,
                        cpu,
                        video_decoder.jpeg.last_frame_was_lossless(),
                    );
                }
                last_sequence = queued.frame.sequence();
                last_video_at = Instant::now();
                {
                    let mut control = state.wayland_control.lock().unwrap();
                    control.note_decoded(
                        purpose,
                        super::wayland_control_wire::DrawnScope {
                            source_epoch: epoch,
                            geometry_epoch: queued.frame.geometry_epoch(),
                            frame_sequence: queued.frame.sequence(),
                        },
                    )?;
                    control.before_frame();
                }
                {
                    let mut pointer = state.wayland_pointer.lock().unwrap();
                    pointer.latest_decoded = Some(super::wayland_pointer_wire::DrawnScope {
                        source_epoch: epoch,
                        geometry_epoch: queued.frame.geometry_epoch(),
                        frame_sequence: queued.frame.sequence(),
                    });
                    pointer.repair_frame(
                        purpose,
                        super::pointer_repair_phase::Scope {
                            source: epoch,
                            geometry: queued.frame.geometry_epoch(),
                            sequence: queued.frame.sequence(),
                        },
                    )?;
                    pointer.before_frame();
                }
                let replaced = state.latest.lock().unwrap().publish(queued);
                drop(popup_publication);
                drop(publication_gate);
                for _ in replaced.iter().flatten() {
                    state.telemetry.record_replaced();
                }
                completion.signal()?;
                // A decoded RGB frame can own a large allocation. The latest
                // slot is unlocked before the stale frame reaches Drop.
                drop(replaced);
            }
            MessageKind::Close | MessageKind::SessionClose => {
                // A peer code/reason never certifies cleanup or permits retry.
                break Err(PeerClosed::decode(kind, &payload)?.into());
            }
            MessageKind::CursorShape | MessageKind::CursorPosition => {
                receive_cursor(&state, kind, &payload)?;
                completion.signal()?;
            }
            MessageKind::FrameAck => {
                return Err(invalid_data("host sent an unexpected frame acknowledgement").into());
            }
            MessageKind::Hello
            | MessageKind::HelloAck
            | MessageKind::Pointer
            | MessageKind::Key
            | MessageKind::Button
            | MessageKind::ReleaseAll
            | MessageKind::InputBatch
            | MessageKind::Viewport
            | MessageKind::SourceCatalog
            | MessageKind::SourceRequest
            | MessageKind::WindowResizeRequest
            | MessageKind::WaylandKeyboardRequest
            | MessageKind::WaylandKeyboardKey
            | MessageKind::WaylandKeyboardRevoke
            | MessageKind::WaylandPopupClick
            | MessageKind::WaylandPopupRevoke
            | MessageKind::WaylandPopupRevokeAck
            | MessageKind::WaylandPointerRequest
            | MessageKind::WaylandPointerAction
            | MessageKind::WaylandPointerRevoke
            | MessageKind::WaylandPointerRepairRequest
            | MessageKind::WaylandPointerRepairDecline
            | MessageKind::WaylandControlRepairRequest
            | MessageKind::WaylandControlRepairDecline
            | MessageKind::WaylandControlRequest
            | MessageKind::WaylandControlPrepare
            | MessageKind::WaylandControlAction
            | MessageKind::WaylandControlRevoke
            | MessageKind::WaylandTextCommit
            | MessageKind::ClipboardRestrict => {
                return Err(invalid_data(format!("unexpected host message: {kind:?}")).into());
            }
            MessageKind::Clipboard | MessageKind::ClipboardTransfer => match clipboard.as_ref() {
                Some(clipboard) => clipboard.deliver(kind, &payload)?,
                None => {
                    return Err(invalid_data("clipboard sharing was not negotiated").into());
                }
            },
            MessageKind::Heartbeat => {
                return Err(invalid_data("host sent an unexpected heartbeat").into());
            }
        }
        payload_retention.observe(&mut payload);
    })();

    completion.finish(result);
}

#[path = "client_connect.rs"]
mod connect;
use connect::connect_with_timeout;

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
pub(super) mod tests {
    pub(in crate::remote) fn emit_telemetry_for_diagnostic_regression() {
        for final_report in [false, true] {
            super::ClientTelemetryReport {
                window: super::ClientTelemetryWindow {
                    received: 1,
                    ..Default::default()
                },
                elapsed: std::time::Duration::from_secs(1),
            }
            .emit(final_report);
        }
    }

    #[test]
    fn visual_repair_never_grants_renews_or_revives_keyboard_lease() {
        use super::super::wayland_keyboard_wire::{LeaseStatus, LeaseStatusKind};
        use super::*;
        let now = Instant::now();
        let mut keyboard = ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        for purpose in [FramePurpose::Video, FramePurpose::VisualRepair] {
            assert!(
                keyboard
                    .after_presentation_for_purpose((1, 1), 0, now, purpose)
                    .is_none()
            );
            assert!(keyboard.request.is_none());
            assert!(keyboard.needs_request());
        }
        let request = keyboard
            .after_presentation_for_purpose((1, 1), 1, now, FramePurpose::NativeProof)
            .unwrap();
        keyboard
            .receive(LeaseStatus {
                lease_epoch: 9,
                source_epoch: 1,
                geometry_epoch: 1,
                frame_sequence: request.frame_sequence,
                status: LeaseStatusKind::Granted,
            })
            .unwrap();
        keyboard.activated = true;
        assert!(keyboard.key(38, true).unwrap().is_some());
        assert!(
            keyboard
                .after_presentation_for_purpose(
                    (1, 1),
                    2,
                    now + Duration::from_secs(20),
                    FramePurpose::VisualRepair
                )
                .is_none()
        );
        assert_eq!(keyboard.request.unwrap().1, now);
        assert!(
            keyboard.key(38, false).unwrap().is_some(),
            "existing held release survives repair"
        );
        assert!(
            keyboard.key(38, true).unwrap().is_some(),
            "existing valid lease continues typing"
        );
        assert!(keyboard.key(38, false).unwrap().is_some());
        assert!(
            keyboard
                .check_timeout(now + super::super::wayland_keyboard_wire::LEASE_DURATION)
                .is_err()
        );
        keyboard.cancel_for_local_event().unwrap();
        assert!(
            keyboard
                .after_presentation_for_purpose((1, 1), 3, now, FramePurpose::NativeProof)
                .is_none()
        );
        assert!(
            keyboard
                .after_presentation_for_purpose((1, 1), 4, now, FramePurpose::VisualRepair)
                .is_none()
        );
    }

    #[test]
    fn authenticated_purpose_survives_decode_and_rejects_png_proof_confusion() {
        use super::*;
        use crate::remote::{
            tile::{TileEncodeRequest, TileEncoder},
            x11_capture::CapturedFrame,
        };
        let base = Instant::now();
        let frame = CapturedFrame {
            geometry_epoch: 1,
            source_width: 32,
            source_height: 32,
            image: image::RgbImage::from_fn(32, 32, |x, y| {
                image::Rgb([x as u8 * 7, y as u8 * 7, 51])
            }),
        };
        let mut encoder = TileEncoder::new();
        encoder.set_lossless_enabled(true);
        encoder
            .plan_at(&frame, TileEncodeRequest::Keyframe, base)
            .unwrap();
        let mut jpeg = Vec::new();
        encoder.encode_into(&mut jpeg, 0, &frame, 60).unwrap();
        encoder.commit(&frame);
        encoder
            .plan_at(
                &frame,
                TileEncodeRequest::LosslessRepair,
                base + Duration::from_secs(3),
            )
            .unwrap();
        let mut png = Vec::new();
        encoder.encode_into(&mut png, 1, &frame, 60).unwrap();
        for purpose in [
            FramePurpose::Video,
            FramePurpose::NativeProof,
            FramePurpose::VisualRepair,
        ] {
            let mut decoder = TileDecoder::new();
            decoder.set_lossless_enabled(true);
            let telemetry = ClientTelemetry::default();
            let pool = new_decode_buffer_pool();
            let first = decode_queued_frame(
                &mut decoder,
                &jpeg,
                FramePurpose::NativeProof,
                &telemetry,
                &pool,
            )
            .unwrap();
            assert_eq!(first.purpose, FramePurpose::NativeProof);
            let result = decode_queued_frame(&mut decoder, &png, purpose, &telemetry, &pool);
            if purpose == FramePurpose::VisualRepair {
                assert_eq!(result.unwrap().purpose, purpose);
            } else {
                assert!(result.is_err());
            }
        }
        let mut decoder = TileDecoder::new();
        assert!(
            decode_queued_frame(
                &mut decoder,
                &jpeg,
                FramePurpose::VisualRepair,
                &ClientTelemetry::default(),
                &new_decode_buffer_pool()
            )
            .is_err()
        );
    }

    #[cfg(feature = "remote-h264")]
    #[test]
    fn h264_session_accepts_tagged_jpeg_and_rejects_stale_or_unscoped_video() {
        use super::*;
        use crate::remote::{
            tile::{TileEncodeRequest, TileEncoder},
            x11_capture::CapturedFrame,
        };

        let state = ReceiveState {
            frame_codec: FrameCodec::H264,
            ..ReceiveState::new()
        };
        let mut decoder = state.new_video_decoder().unwrap();
        let captured = CapturedFrame {
            geometry_epoch: 8,
            source_width: 32,
            source_height: 31,
            image: image::RgbImage::from_pixel(32, 31, image::Rgb([40, 90, 140])),
        };
        let mut jpeg_encoder = TileEncoder::new();
        jpeg_encoder
            .plan_at(&captured, TileEncodeRequest::Keyframe, Instant::now())
            .unwrap();
        let mut payload = Vec::new();
        jpeg_encoder
            .encode_into(&mut payload, 3, &captured, 85)
            .unwrap();
        h264_wire::tag_jpeg_frame(&mut payload).unwrap();
        let queued =
            decode_popup_queued_frame(&state, &mut decoder, &payload, FramePurpose::Video, 1)
                .unwrap();
        assert_eq!(queued.frame.sequence(), 3);
        assert_eq!(queued.frame.geometry_epoch(), 8);
        assert_eq!(queued.frame.dimensions(), (32, 31));
        assert!(
            decode_popup_queued_frame(&state, &mut decoder, &payload, FramePurpose::NativeProof, 1)
                .is_err()
        );

        let mut header = HardwareFrameHeader {
            sequence: 4,
            source_width: 32,
            source_height: 31,
            encoded_width: 32,
            encoded_height: 31,
            geometry_epoch: 7,
        };
        let idr = [0, 0, 1, 0x65, 0x80];
        h264_wire::encode_hardware_frame(
            &mut payload,
            FrameCodec::H264,
            header,
            &idr,
            state.frame_limits,
        )
        .unwrap();
        let result = decoder.decode_hardware_session(&state, &payload);
        assert_eq!(
            result.err().unwrap().to_string(),
            "video geometry epoch moved backwards"
        );

        header.geometry_epoch = 8;
        let inter = [0, 0, 1, 0x41, 0x80];
        h264_wire::encode_hardware_frame(
            &mut payload,
            FrameCodec::H264,
            header,
            &inter,
            state.frame_limits,
        )
        .unwrap();
        let result = decoder.decode_hardware_session(&state, &payload);
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("requires an IDR")
        );
        assert!(decoder.hardware.is_none());
    }

    #[test]
    fn h264_retry_requires_direct_failure_complete_cleanup_and_no_local_close() {
        use super::*;
        let hardware =
            || Box::new(HardwareDecodeFailure(io::Error::other("decode"))) as RemoteError;
        let is_retry = |cause| {
            hardware_decode_failure_clean(&ClientFailure {
                stage: ClientStage::Session,
                cause,
            })
        };
        assert!(is_retry(
            finalize_retry_intent(Err(hardware()), || Ok(true)).unwrap_err()
        ));
        assert!(!is_retry(
            finalize_retry_intent(Err(hardware()), || Ok(false)).unwrap_err()
        ));
        assert!(!is_retry(
            suppress_reconnect(Err(hardware()), true).unwrap_err()
        ));
        assert!(!is_retry(
            include_local_cleanup(Err(hardware()), Err(io::Error::other("join").into()))
                .unwrap_err()
        ));
    }

    fn pointer_fixture(now: std::time::Instant) -> super::ClientPointer {
        use super::super::wayland_pointer_wire::*;
        let mut pointer = super::ClientPointer {
            enabled: true,
            ..Default::default()
        };
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 0,
        };
        pointer.presented(scope, now).unwrap();
        pointer
            .receive(PointerStatus {
                lease_epoch: 7,
                scope,
                status: PointerStatusKind::Ready,
            })
            .unwrap();
        pointer
    }
    #[test]
    fn pointer_actual_receipt_precedes_ack_ready_and_survives_later_invalidation() {
        use super::super::wayland_pointer_wire::*;
        let now = std::time::Instant::now();
        for invalidate_before_ready in [false, true] {
            let mut pointer = pointer_fixture(now);
            let old = pointer.presented.unwrap().0;
            let next = DrawnScope {
                frame_sequence: 1,
                ..old
            };
            pointer.latest_decoded = Some(next);
            pointer.before_frame();
            // Actual feedback is saved before ACK is made visible to the host.
            assert!(pointer.presented(next, now).is_none());
            if invalidate_before_ready {
                pointer.latest_decoded = Some(DrawnScope {
                    frame_sequence: 2,
                    ..old
                });
                pointer.before_frame();
            }
            // Simulates the independent receiver replying immediately at ACK.
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope: next,
                    status: PointerStatusKind::Ready,
                })
                .unwrap();
            assert_eq!(pointer.fresh(now), !invalidate_before_ready);
            pointer.before_frame();
            assert!(!pointer.fresh(now));
            // Foreground must not call presented(next) again after ACK.
            assert!(
                pointer
                    .receive(PointerStatus {
                        lease_epoch: 7,
                        scope: next,
                        status: PointerStatusKind::Ready
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn pointer_late_exact_ready_retires_without_reviving_and_unknown_replays_fail() {
        use super::super::wayland_pointer_wire::*;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        let initial = pointer.presented.unwrap().0;
        let older = DrawnScope {
            frame_sequence: 1,
            ..initial
        };
        let latest = DrawnScope {
            frame_sequence: 3,
            ..initial
        };
        pointer.presented(older, now);
        pointer.latest_decoded = Some(latest);
        pointer.before_frame();
        // A receipt for N cannot clear an already-decoded N+2 invalidation.
        pointer.presented(older, now);
        assert!(pointer.newer_frame && !pointer.fresh(now));
        pointer.presented(latest, now);
        let ready = PointerStatus {
            lease_epoch: 7,
            scope: older,
            status: PointerStatusKind::Ready,
        };
        pointer.receive(ready).unwrap();
        assert!(pointer.ready.is_none());
        assert!(pointer.receive(ready).is_err());
        assert!(
            pointer
                .receive(PointerStatus {
                    scope: DrawnScope {
                        frame_sequence: 2,
                        ..initial
                    },
                    ..ready
                })
                .is_err()
        );
        assert!(
            pointer
                .receive(PointerStatus {
                    scope: DrawnScope {
                        frame_sequence: 4,
                        ..initial
                    },
                    ..ready
                })
                .is_err()
        );
        pointer
            .receive(PointerStatus {
                scope: latest,
                ..ready
            })
            .unwrap();
        assert!(pointer.fresh(now));
    }

    #[test]
    fn pointer_local_revoke_drains_only_known_inflight_status_without_reviving() {
        use super::super::wayland_pointer_wire::*;
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let now = std::time::Instant::now();
        for status in [PointerStatusKind::Ready, PointerStatusKind::Waiting] {
            let mut pointer = pointer_fixture(now);
            let scope = pointer.presented.unwrap().0;
            pointer.event(&pointer_event(E::LeftDown, 0), now).unwrap();
            assert!(pointer.click.is_some());
            assert_eq!(pointer.revoke().unwrap().lease_epoch, 7);
            assert!(!pointer.terminal_seen);
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope,
                    status,
                })
                .unwrap();
            assert!(pointer.ended);
            assert_eq!(pointer.epoch, Some(7));
            assert!(pointer.ready.is_none() && pointer.click.is_none());
            assert!(!pointer.left_down && !pointer.fresh(now));
            assert!(
                pointer
                    .event(&pointer_event(E::Motion, 0), now)
                    .unwrap()
                    .is_none()
            );
            assert!(pointer.presented(scope, now).is_none());
            assert!(pointer.revoke().is_none());
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope,
                    status: PointerStatusKind::Revoked,
                })
                .unwrap();
            assert!(pointer.terminal_seen);
            assert!(
                pointer
                    .receive(PointerStatus {
                        lease_epoch: 7,
                        scope,
                        status
                    })
                    .unwrap_err()
                    .to_string()
                    .contains("scope mismatch or replay")
            );
        }
    }
    #[test]
    fn pointer_local_revoke_still_rejects_unknown_or_unpresented_status() {
        use super::super::wayland_pointer_wire::*;
        let now = std::time::Instant::now();
        for case in 0..5 {
            let mut pointer = pointer_fixture(now);
            let mut scope = pointer.presented.unwrap().0;
            let mut epoch = 7;
            pointer.revoke().unwrap();
            let expected = match case {
                0 => {
                    epoch = 8;
                    "lease epoch changed"
                }
                1 => {
                    scope.source_epoch += 1;
                    "scope mismatch or replay"
                }
                2 => {
                    scope.geometry_epoch += 1;
                    "scope mismatch or replay"
                }
                3 => {
                    scope.frame_sequence += 1;
                    "precedes actual presentation"
                }
                _ => {
                    pointer.epoch = None;
                    "revived ended grant"
                }
            };
            let error = pointer
                .receive(PointerStatus {
                    lease_epoch: epoch,
                    scope,
                    status: PointerStatusKind::Ready,
                })
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{case}: {error}");
            assert!(pointer.ended && pointer.ready.is_none() && pointer.click.is_none());
            assert!(!pointer.fresh(now));
        }
    }
    fn pointer_event(
        kind: super::super::x11_viewer::WaylandPointerEventKind,
        x: i16,
    ) -> super::ViewerEvent {
        super::ViewerEvent::WaylandPointer {
            frame_sequence: 0,
            geometry_epoch: 2,
            x: 20,
            y: 30,
            local_x: x,
            local_y: 10,
            kind,
        }
    }
    #[test]
    fn horizontal_pointer_consent_is_independent_and_consumes_one_ticket() {
        use super::super::{
            wayland_pointer_wire::PointerActionKind as A, x11_viewer::WaylandPointerEventKind as E,
        };
        let now = std::time::Instant::now();
        for (event, expected) in [
            (E::ScrollLeft, A::ScrollLeft),
            (E::ScrollRight, A::ScrollRight),
        ] {
            let mut p = pointer_fixture(now);
            p.scroll_enabled = true;
            assert!(p.event(&pointer_event(event, 0), now).unwrap().is_none());
            p.scroll_enabled = false;
            p.horizontal_enabled = true;
            assert!(
                p.event(&pointer_event(E::ScrollUp, 0), now)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                p.event(&pointer_event(event, 0), now)
                    .unwrap()
                    .unwrap()
                    .kind,
                expected
            );
            assert!(p.event(&pointer_event(event, 0), now).unwrap().is_none());
            assert!(!p.fresh(now));
        }
    }
    #[test]
    fn scoped_scroll_requires_both_permissions_and_consumes_one_fresh_gate() {
        use super::super::{
            wayland_pointer_wire::PointerActionKind as A, x11_viewer::WaylandPointerEventKind as E,
        };
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        assert!(
            pointer
                .event(&pointer_event(E::ScrollUp, 0), now)
                .unwrap()
                .is_none()
        );
        assert_eq!(pointer.sequence, 0);
        pointer.scroll_enabled = true;
        let action = pointer
            .event(&pointer_event(E::ScrollUp, 0), now)
            .unwrap()
            .unwrap();
        assert_eq!(action.kind, A::ScrollUp);
        for _ in 0..3 {
            assert!(
                pointer
                    .event(&pointer_event(E::ScrollDown, 0), now)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(pointer.sequence, 1);
        assert!(
            pointer
                .event(&pointer_event(E::LeftUp, 0), now)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn scoped_scroll_during_left_hold_cancels_candidate_without_emitting() {
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        pointer.scroll_enabled = true;
        assert!(
            pointer
                .event(&pointer_event(E::LeftDown, 0), now)
                .unwrap()
                .is_none()
        );
        assert!(pointer.click.is_some());
        assert!(
            pointer
                .event(&pointer_event(E::ScrollDown, 0), now)
                .unwrap()
                .is_none()
        );
        assert!(pointer.click.is_none());
        assert!(
            pointer
                .event(&pointer_event(E::LeftUp, 0), now)
                .unwrap()
                .is_none()
        );
        assert_eq!(pointer.sequence, 0);
    }
    #[test]
    fn scoped_scroll_waiting_left_down_stays_held_across_ready() {
        use super::super::{
            wayland_pointer_wire::{PointerStatus, PointerStatusKind},
            x11_viewer::WaylandPointerEventKind as E,
        };
        let now = std::time::Instant::now();
        for no_presentation in [false, true] {
            let mut pointer = pointer_fixture(now);
            pointer.scroll_enabled = true;
            let scope = pointer.presented.unwrap().0;
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope,
                    status: PointerStatusKind::Waiting,
                })
                .unwrap();
            if no_presentation {
                pointer.presented = None;
            }
            assert!(
                pointer
                    .event(&pointer_event(E::LeftDown, 0), now)
                    .unwrap()
                    .is_none()
            );
            assert!(pointer.left_down);
            assert!(pointer.click.is_none());
            let scope = super::super::wayland_pointer_wire::DrawnScope {
                frame_sequence: scope.frame_sequence + 1,
                ..scope
            };
            pointer.presented(scope, now);
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope,
                    status: PointerStatusKind::Ready,
                })
                .unwrap();
            assert!(pointer.fresh(now));
            assert!(
                pointer
                    .event(&pointer_event(E::ScrollUp, 0), now)
                    .unwrap()
                    .is_none()
            );
            assert!(
                pointer
                    .event(&pointer_event(E::LeftUp, 0), now)
                    .unwrap()
                    .is_none()
            );
            assert!(!pointer.left_down);
            assert_eq!(pointer.sequence, 0);
        }
    }
    #[test]
    fn scroll_negotiation_rejects_unsolicited_missing_or_pointerless_grants() {
        assert!(super::validate_scroll_negotiation(true, false, false).is_ok());
        assert!(super::validate_scroll_negotiation(true, true, true).is_ok());
        for (pointer, request, grant) in [
            (true, false, true),
            (true, true, false),
            (false, true, true),
            (false, true, false),
        ] {
            assert!(super::validate_scroll_negotiation(pointer, request, grant).is_err());
        }
    }
    #[test]
    fn scoped_pointer_motion_consumes_exactly_one_presented_gate() {
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        assert!(
            pointer
                .event(&pointer_event(E::Motion, 10), now)
                .unwrap()
                .is_some()
        );
        assert!(
            pointer
                .event(&pointer_event(E::Motion, 11), now)
                .unwrap()
                .is_none()
        );
        assert!(!pointer.fresh(now));
        assert!(
            pointer
                .event(&pointer_event(E::LeftDown, 10), now)
                .unwrap()
                .is_none()
        );
        assert!(
            pointer
                .event(&pointer_event(E::LeftUp, 10), now)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn scoped_pointer_outside_release_cancels_click_but_restores_hover() {
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        pointer.event(&pointer_event(E::LeftDown, 10), now).unwrap();
        pointer
            .event(&super::ViewerEvent::WaylandPointerCancel, now)
            .unwrap();
        assert!(
            pointer
                .event(&pointer_event(E::Motion, 10), now)
                .unwrap()
                .is_none()
        );
        assert!(
            pointer
                .event(
                    &super::ViewerEvent::WaylandPointerButtonCancel { pressed: false },
                    now
                )
                .unwrap()
                .is_none()
        );
        assert!(pointer.click.is_none());
        assert!(!pointer.left_down);
        assert!(
            pointer
                .event(&pointer_event(E::Motion, 10), now)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn scoped_pointer_click_requires_same_frame_small_motion_and_short_hold() {
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        assert!(
            pointer
                .event(&pointer_event(E::LeftDown, 10), now)
                .unwrap()
                .is_none()
        );
        let click = pointer
            .event(
                &pointer_event(E::LeftUp, 12),
                now + std::time::Duration::from_millis(100),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            click.kind,
            super::super::wayland_pointer_wire::PointerActionKind::ClickLeft
        );
        for cancel in 0..4 {
            let mut pointer = pointer_fixture(now);
            pointer.event(&pointer_event(E::LeftDown, 10), now).unwrap();
            match cancel {
                0 => pointer.before_frame(),
                1 => {
                    pointer.event(&pointer_event(E::Motion, 20), now).unwrap();
                }
                2 => {
                    pointer
                        .event(&super::ViewerEvent::WaylandPointerCancel, now)
                        .unwrap();
                }
                _ => {
                    pointer.revoke();
                }
            }
            assert!(
                pointer
                    .event(
                        &pointer_event(E::LeftUp, 10),
                        now + std::time::Duration::from_millis(100)
                    )
                    .unwrap()
                    .is_none()
            );
        }
        let mut pointer = pointer_fixture(now);
        pointer.event(&pointer_event(E::LeftDown, 10), now).unwrap();
        assert!(
            pointer
                .event(
                    &pointer_event(E::LeftUp, 10),
                    now + std::time::Duration::from_millis(251)
                )
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn scoped_pointer_ready_never_refreshes_presentation_age_or_allows_future_frame() {
        use super::super::wayland_pointer_wire::*;
        let now = std::time::Instant::now();
        let mut pointer = pointer_fixture(now);
        assert!(!pointer.fresh(now + FRAME_MAX_AGE));
        let scope = DrawnScope {
            source_epoch: 1,
            geometry_epoch: 2,
            frame_sequence: 1,
        };
        assert!(
            pointer
                .receive(PointerStatus {
                    lease_epoch: 7,
                    scope,
                    status: PointerStatusKind::Ready
                })
                .is_err()
        );
        pointer.before_frame();
        pointer.presented(scope, now + std::time::Duration::from_millis(20));
        pointer
            .receive(PointerStatus {
                lease_epoch: 7,
                scope,
                status: PointerStatusKind::Ready,
            })
            .unwrap();
        assert!(pointer.fresh(now + std::time::Duration::from_millis(30)));
    }
    #[test]
    fn wayland_keyboard_balanced_pregrant_edges_permanently_cancel() {
        use super::super::wayland_keyboard_wire::{LeaseStatus, LeaseStatusKind};
        for requested in [false, true] {
            let mut keyboard = super::ClientKeyboard {
                digest: Some([7; 32]),
                ..Default::default()
            };
            let request = requested.then(|| {
                keyboard
                    .after_presentation((2, 3), 1, std::time::Instant::now())
                    .unwrap()
            });
            assert!(keyboard.key(38, true).is_err());
            assert!(keyboard.key(38, false).unwrap().is_none());
            assert!(keyboard.ended);
            assert!(keyboard.held.is_empty());
            assert!(!keyboard.needs_request());
            assert!(
                keyboard
                    .after_presentation((2, 3), 2, std::time::Instant::now())
                    .is_none()
            );
            if let Some(request) = request {
                assert!(
                    keyboard
                        .receive(LeaseStatus {
                            lease_epoch: 9,
                            source_epoch: request.source_epoch,
                            geometry_epoch: request.geometry_epoch,
                            frame_sequence: request.frame_sequence,
                            status: LeaseStatusKind::Granted,
                        })
                        .is_err()
                );
            }
        }
    }
    #[test]
    fn wayland_keyboard_grant_without_local_focus_activation_rejects_keys() {
        use super::super::wayland_keyboard_wire::{LeaseStatus, LeaseStatusKind};
        let mut keyboard = super::ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        keyboard
            .after_presentation((2, 3), 1, std::time::Instant::now())
            .unwrap();
        keyboard
            .receive(LeaseStatus {
                lease_epoch: 9,
                source_epoch: 2,
                geometry_epoch: 3,
                frame_sequence: 1,
                status: LeaseStatusKind::Granted,
            })
            .unwrap();
        assert!(!keyboard.activated);
        assert!(keyboard.key(38, true).is_err());
        assert!(keyboard.ended);
        assert!(keyboard.held.is_empty());
        assert!(keyboard.key(38, false).unwrap().is_none());
    }
    #[test]
    fn wayland_keyboard_status_requires_exact_drawn_scope_and_single_grant() {
        use super::super::wayland_keyboard_wire::{LeaseStatus, LeaseStatusKind};
        let mut keyboard = super::ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        let request = keyboard
            .after_presentation((2, 3), 0, std::time::Instant::now())
            .unwrap();
        assert!(
            keyboard
                .after_presentation((2, 3), 1, std::time::Instant::now())
                .is_none()
        );
        let granted = LeaseStatus {
            lease_epoch: 9,
            source_epoch: request.source_epoch,
            geometry_epoch: request.geometry_epoch,
            frame_sequence: request.frame_sequence,
            status: LeaseStatusKind::Granted,
        };
        assert!(
            keyboard
                .receive(LeaseStatus {
                    frame_sequence: 1,
                    ..granted
                })
                .is_err()
        );
        keyboard.receive(granted).unwrap();
        assert!(keyboard.receive(granted).is_err());
        assert!(keyboard.check_scope((2, 3)).is_ok());
        assert!(keyboard.check_scope((2, 4)).is_err());
        assert_eq!(keyboard.revoke().unwrap().lease_epoch, 9);
        assert!(keyboard.revoke().is_none());
        assert!(keyboard.key(38, true).unwrap().is_none());
    }
    #[test]
    fn wayland_keyboard_local_cancel_distinguishes_denied_pending_and_active() {
        use super::super::wayland_keyboard_wire::{LeaseStatus, LeaseStatusKind};
        let fresh = || super::ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        let request = |keyboard: &mut super::ClientKeyboard| {
            keyboard
                .after_presentation((1, 1), 0, std::time::Instant::now())
                .unwrap();
        };
        let granted = LeaseStatus {
            lease_epoch: 9,
            source_epoch: 1,
            geometry_epoch: 1,
            frame_sequence: 0,
            status: LeaseStatusKind::Granted,
        };
        let denied = LeaseStatus {
            lease_epoch: 0,
            status: LeaseStatusKind::Denied,
            ..granted
        };
        let mut keyboard = fresh();
        request(&mut keyboard);
        keyboard.receive(denied).unwrap();
        assert!(keyboard.cancel_for_local_event().unwrap().is_none());
        assert!(keyboard.key(38, true).unwrap().is_none());
        assert!(!keyboard.needs_request());
        assert!(keyboard.receive(granted).is_err());
        assert!(keyboard.receive(denied).is_err());

        let mut keyboard = fresh();
        request(&mut keyboard);
        assert!(keyboard.cancel_for_local_event().is_err());
        assert!(keyboard.ended);
        assert!(keyboard.receive(granted).is_err());
        assert!(keyboard.key(38, true).unwrap().is_none());
        assert!(!keyboard.needs_request());

        let mut keyboard = fresh();
        request(&mut keyboard);
        keyboard.receive(granted).unwrap();
        assert_eq!(
            keyboard
                .cancel_for_local_event()
                .unwrap()
                .unwrap()
                .lease_epoch,
            9
        );
        assert!(keyboard.cancel_for_local_event().unwrap().is_none());

        let mut keyboard = fresh();
        assert!(keyboard.cancel_for_local_event().unwrap().is_none());
        assert!(!keyboard.needs_request());
        assert!(
            keyboard
                .after_presentation((1, 1), 0, std::time::Instant::now())
                .is_none()
        );
    }
    #[test]
    fn wayland_keyboard_never_replays_pregrant_keys_and_deadlines_do_not_renew() {
        let now = std::time::Instant::now();
        let mut keyboard = super::ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        assert!(keyboard.key(38, true).is_err());
        assert!(keyboard.held.is_empty());
        assert!(keyboard.ended);
        let mut keyboard = super::ClientKeyboard {
            digest: Some([7; 32]),
            ..Default::default()
        };
        keyboard.after_presentation((1, 1), 0, now).unwrap();
        assert!(
            keyboard
                .check_timeout(now + std::time::Duration::from_secs(5))
                .is_err()
        );
        keyboard.epoch = Some(4);
        assert!(
            keyboard
                .check_timeout(now + super::super::wayland_keyboard_wire::LEASE_DURATION)
                .is_err()
        );
    }
    use super::*;
    use crate::remote::frame::{DecodedFrame, decode_pool_snapshot};
    use crate::remote::messages::decode_source_frame_ack;
    use crate::remote::protocol::{PSK_LEN, server_handshake};
    use crate::remote::tile::{TileEncodeRequest, TileEncoder};
    use crate::remote::x11_capture::CapturedFrame;
    use image::Rgb;
    use image::RgbImage;
    use std::io::Cursor;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;

    const TEST_PSK: [u8; PSK_LEN] = [0x5a; PSK_LEN];

    const CLOSE_CODES: [SessionCloseCode; 5] = [
        SessionCloseCode::Busy,
        SessionCloseCode::AdmissionDisabled,
        SessionCloseCode::CaptureUnavailable,
        SessionCloseCode::ClipboardUnavailable,
        SessionCloseCode::HostStopping,
    ];
    const CLOSE_PHASES: [ClientStage; 3] = [
        ClientStage::Negotiate,
        ClientStage::FirstFrame,
        ClientStage::Session,
    ];

    fn authenticated_terminal_at(
        phase: ClientStage,
        kind: MessageKind,
        payload: &[u8],
        receive_key: [u8; PSK_LEN],
    ) -> RemoteError {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        writer.write_message(kind, payload).unwrap();
        // A close ends the session, even if a peer continues sending records.
        writer
            .write_message(MessageKind::Frame, b"must not be decoded")
            .unwrap();
        authenticated_terminal_wire_at(phase, &writer.into_inner(), receive_key)
    }

    fn authenticated_terminal_wire_at(
        phase: ClientStage,
        wire: &[u8],
        receive_key: [u8; PSK_LEN],
    ) -> RemoteError {
        let mut reader = SessionReader::new(Cursor::new(wire), receive_key);
        let cause = match phase {
            ClientStage::Negotiate => {
                reader.set_max_payload_len(MAX_NEGOTIATION_PAYLOAD_LEN);
                read_server_hello(
                    &mut reader,
                    FrameCodec::JpegTiles,
                    ApplicationProfile::Standard,
                )
                .unwrap_err()
            }
            ClientStage::FirstFrame => {
                read_initial_frame_records(&mut reader, true, true, |_, _| {
                    panic!("a close must not be delivered as clipboard or cursor data")
                })
                .unwrap_err()
            }
            ClientStage::Session => {
                let state = Arc::new(ReceiveState::new());
                let (_wake_receiver, wake_sender) = wake_pair().unwrap();
                receive_frame_records(
                    |payload, _| {
                        reader
                            .read_message_into(payload)
                            .map_err(receiver_read_error)
                    },
                    Arc::clone(&state),
                    0,
                    Vec::new(),
                    wake_sender,
                    TileDecoder::new(),
                    None,
                );
                assert!(!state.alive.load(Ordering::Acquire));
                assert!(state.latest.lock().unwrap().is_none());
                assert_eq!(decode_pool_snapshot(&state.decode_pool).0, 0);
                state.error.lock().unwrap().take().expect("terminal cause")
            }
            _ => unreachable!(),
        };
        Box::new(ClientFailure {
            stage: phase,
            cause,
        })
    }

    /// Exercise genuine host runtime reports through both post-Hello readers.
    pub(in crate::remote) fn assert_runtime_terminal_record(
        wire: &[u8],
        key: [u8; PSK_LEN],
        expected_code: impl Into<Option<SessionCloseCode>>,
        expected_reason: &str,
    ) {
        let expected_code = expected_code.into();
        let code_name = match expected_code {
            Some(SessionCloseCode::CaptureUnavailable) => Some("capture_unavailable"),
            Some(SessionCloseCode::ClipboardUnavailable) => Some("clipboard_unavailable"),
            None => None,
            _ => panic!("expected a runtime capture, clipboard or legacy close"),
        };
        for phase in [ClientStage::FirstFrame, ClientStage::Session] {
            for cleanup_fails in [false, true] {
                let failure = authenticated_terminal_wire_at(phase, wire, key)
                    .downcast::<ClientFailure>()
                    .unwrap();
                let close = failure.cause.downcast_ref::<PeerClosed>().unwrap();
                assert_eq!(close.code, expected_code);
                assert_eq!(close.reason, expected_reason);
                let result = finish_session(
                    Err(failure.cause),
                    Ok(()),
                    ReceiverExit::Completed,
                    Ok(()),
                    if cleanup_fails {
                        Err(io::Error::other("PRIVATE /cleanup/path").into())
                    } else {
                        Ok(())
                    },
                );
                let cause = finalize_retry_intent(result, || {
                    panic!("runtime peer reports cannot acquire a retry certificate")
                })
                .unwrap_err();
                let error: RemoteError = Box::new(ClientFailure {
                    stage: phase,
                    cause,
                });
                let summary = classify_failure(error.as_ref());
                // Exact content-free JSON also excludes local and peer reason text.
                assert_eq!(
                    serde_json::to_value(&summary).unwrap(),
                    serde_json::json!({
                        "stage": if matches!(phase, ClientStage::FirstFrame) {
                            "first_frame"
                        } else {
                            "session"
                        },
                        "category": if cleanup_fails { "cleanup_failed" } else { "peer_closed" },
                        "io_kind": null,
                        "peer_close_code": code_name,
                        "cleanup": if cleanup_fails { "incomplete" } else { "unknown" },
                        "reconnect_certified": false,
                    })
                );
                let mut error = Some(error);
                let mut attempts = 0;
                reconnect_with(
                    5,
                    || {
                        attempts += 1;
                        Err(error.take().expect("runtime peer report must not retry"))
                    },
                    |_, _| panic!("runtime peer reports must not schedule reconnect"),
                )
                .unwrap_err();
                assert_eq!(attempts, 1);
            }
        }
    }

    #[test]
    fn structured_close_is_terminal_in_all_three_authenticated_phases() {
        for code in CLOSE_CODES {
            for phase in CLOSE_PHASES {
                let payload = SessionClose::new(
                    code,
                    "PRIVATE /home/alice/key host:48221 retry now; cleanup complete\n\u{1b}[31m",
                )
                .encode();
                let error =
                    authenticated_terminal_at(phase, MessageKind::SessionClose, &payload, TEST_PSK);
                let failure = error.downcast_ref::<ClientFailure>().unwrap();
                let close = failure.cause.downcast_ref::<PeerClosed>().unwrap();
                assert_eq!(close.code, Some(code));
                assert!(close.reason.contains("retry now; cleanup complete"));
                assert!(!close.reason.chars().any(char::is_control));
                let summary = classify_failure(error.as_ref());
                assert_eq!(summary.peer_close_code, Some(code));
                assert_eq!(summary.category, "peer_closed");
                assert_eq!(summary.cleanup, "unknown");
                assert!(!summary.reconnect_certified);
                let json = serde_json::to_string(&summary).unwrap();
                for private in [
                    "PRIVATE",
                    "/home/alice",
                    "host:48221",
                    "retry now",
                    "reason",
                ] {
                    assert!(!json.contains(private), "{json}");
                }
                let mut error = Some(error);
                let mut attempts = 0;
                reconnect_with(
                    5,
                    || {
                        attempts += 1;
                        Err(error.take().expect("peer close must not retry"))
                    },
                    |_, _| panic!("peer text and codes cannot authorize reconnect"),
                )
                .unwrap_err();
                assert_eq!(attempts, 1);
            }
        }
    }

    #[test]
    fn legacy_empty_and_forged_close_text_remain_terminal_without_a_code() {
        for phase in CLOSE_PHASES {
            for reason in [
                b"".as_slice(),
                b"host_stopping busy: retry; cleanup complete",
            ] {
                let error = authenticated_terminal_at(phase, MessageKind::Close, reason, TEST_PSK);
                let summary = classify_failure(error.as_ref());
                assert_eq!(summary.category, "peer_closed");
                assert_eq!(summary.peer_close_code, None);
                assert_eq!(summary.cleanup, "unknown");
                assert!(!summary.reconnect_certified);
                assert!(serde_json::to_value(summary).unwrap()["peer_close_code"].is_null());
            }
        }
    }

    #[test]
    fn only_an_authenticated_exact_legacy_close_refuses_a_hardware_offer() {
        let marker =
            b"unsupported remote codec, cursor or input mode; update jwm-remote on both machines together";
        let read_close = |codec, kind, payload: &[u8], receive_key| {
            let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
            writer.write_message(kind, payload).unwrap();
            let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), receive_key);
            reader.set_max_payload_len(MAX_NEGOTIATION_PAYLOAD_LEN);
            read_server_hello(&mut reader, codec, ApplicationProfile::Standard).unwrap_err()
        };

        for codec in [FrameCodec::H264, FrameCodec::Hevc444] {
            assert!(
                read_close(codec, MessageKind::Close, marker, TEST_PSK)
                    .is::<LegacyVideoProfileRefused>()
            );
        }

        let mut near_marker = marker.to_vec();
        near_marker.push(b'.');
        for (codec, payload) in [
            (FrameCodec::Hevc444, near_marker.as_slice()),
            (FrameCodec::JpegTiles, marker.as_slice()),
        ] {
            let close = read_close(codec, MessageKind::Close, payload, TEST_PSK)
                .downcast::<PeerClosed>()
                .unwrap();
            assert_eq!(close.code, None);
        }

        let structured =
            SessionClose::new(SessionCloseCode::Busy, std::str::from_utf8(marker).unwrap())
                .encode();
        let close = read_close(
            FrameCodec::Hevc444,
            MessageKind::SessionClose,
            &structured,
            TEST_PSK,
        )
        .downcast::<PeerClosed>()
        .unwrap();
        assert_eq!(close.code, Some(SessionCloseCode::Busy));

        let error = read_close(
            FrameCodec::Hevc444,
            MessageKind::Close,
            marker,
            [0xff; PSK_LEN],
        );
        assert!(matches!(
            error.downcast_ref::<ProtocolError>(),
            Some(ProtocolError::AuthenticationFailed)
        ));
    }

    #[test]
    fn malformed_and_unauthenticated_structured_close_never_become_peer_codes() {
        for phase in CLOSE_PHASES {
            for payload in [
                vec![],
                vec![2, 1, 0, 0],
                vec![1, 0, 0, 0],
                vec![1, 1, 0, 1],
                vec![1, 1, 0, 0, b'x'],
                vec![1, 1, 0, 1, 0xff],
            ] {
                let error =
                    authenticated_terminal_at(phase, MessageKind::SessionClose, &payload, TEST_PSK);
                let summary = classify_failure(error.as_ref());
                assert_eq!(summary.category, "io");
                assert_eq!(summary.io_kind, Some("invalid_data"));
                assert_eq!(summary.peer_close_code, None);
                assert!(!summary.reconnect_certified);
            }
            let payload = SessionClose::new(SessionCloseCode::Busy, "retry").encode();
            let error = authenticated_terminal_at(
                phase,
                MessageKind::SessionClose,
                &payload,
                [0xff; PSK_LEN],
            );
            let summary = classify_failure(error.as_ref());
            assert_eq!(summary.category, "authentication_failed");
            assert_eq!(summary.peer_close_code, None);
            assert!(!summary.reconnect_certified);
        }
    }

    #[test]
    fn structured_close_survives_cleanup_without_ever_certifying_reconnect() {
        for code in CLOSE_CODES {
            for cleanup_fails in [false, true] {
                let payload =
                    SessionClose::new(code, "PRIVATE host:48221 cleanup complete; retry").encode();
                let peer: RemoteError = PeerClosed::decode(MessageKind::SessionClose, &payload)
                    .unwrap()
                    .into();
                let result = finish_session(
                    Err(peer),
                    Ok(()),
                    ReceiverExit::Completed,
                    Ok(()),
                    if cleanup_fails {
                        Err(io::Error::other("PRIVATE /cleanup/path").into())
                    } else {
                        Ok(())
                    },
                );
                let error = finalize_retry_intent(result, || {
                    panic!("peer termination must never acquire a retry certificate")
                })
                .unwrap_err();
                let failure = ClientFailure {
                    stage: ClientStage::Session,
                    cause: error,
                };
                let summary = classify_failure(&failure);
                assert_eq!(summary.peer_close_code, Some(code));
                assert_eq!(
                    summary.category,
                    if cleanup_fails {
                        "cleanup_failed"
                    } else {
                        "peer_closed"
                    }
                );
                assert_eq!(
                    summary.cleanup,
                    if cleanup_fails {
                        "incomplete"
                    } else {
                        "unknown"
                    }
                );
                assert!(!summary.reconnect_certified);
                let json = serde_json::to_string(&summary).unwrap();
                for private in ["PRIVATE", "host:48221", "/cleanup/path", "retry"] {
                    assert!(!json.contains(private), "{json}");
                }
            }
        }
    }

    #[test]
    fn invalid_startup_size_fails_before_reading_key_or_display() {
        let options = ClientOptions {
            stats_json: false,
            control_duration: Default::default(),
            viewer_backend: crate::remote::viewer_backend::ViewerBackend::X11,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_pointer: false,
            wayland_control: false,
            wayland_gestures: false,
            resumable_control: false,
            wayland_seamless_input: false,
            wayland_popup_pointer: false,
            wayland_text: None,
            static_lossless: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            resize_source: None,
            follow_source_size: false,
            window_position: None,
            x11_parent_window: None,
            window_size: Some(super::super::window_size::WindowSize {
                width: 0,
                height: 1,
            }),
            frame_limits: super::super::messages::FrameLimits::default(),
            address: "invalid address".into(),
            peer_label: "test".into(),
            key: KeySource::File(std::path::PathBuf::from(
                "/nonexistent/window-size-test.key",
            )),
            display: Some("invalid display".into()),
            view_only: true,
            grab_input: false,
            seamless_input: None,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "F12".into(),
            scaling: super::super::scaling::ScalingPolicy::Auto,
        };
        assert!(
            run_client(options)
                .unwrap_err()
                .to_string()
                .contains("window size")
        );
    }

    #[test]
    fn raced_source_barrier_retains_first_future_frame_and_cursor() {
        let state = ReceiveState::new();
        // The viewer sampled epoch 1, then the receiver processed Changed(2)
        // and published the one keyframe whose ACK the host now waits for.
        state.source_epoch.store(2, Ordering::Release);
        let mut queued = queued_test_frame(41);
        queued.frame = queued.frame.with_scope(state.session_epoch, 2);
        state.latest.lock().unwrap().publish(queued);
        assert!(take_latest(&state, 1).is_none());
        assert!(!state.latest.lock().unwrap().is_none());
        assert_eq!(take_latest(&state, 2).unwrap().frame.sequence(), 41);
        state.cursor_enabled.store(true, Ordering::Release);
        let shape = crate::remote::cursor::CursorShape {
            id: 1,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![255; 4],
        };
        let position = crate::remote::cursor::CursorPosition {
            sequence: 0,
            geometry_epoch: 1,
            shape_id: 1,
            source_width: 640,
            source_height: 480,
            x: 1,
            y: 2,
            visible: true,
        };
        receive_cursor(
            &state,
            MessageKind::CursorShape,
            &crate::remote::source_wire::encode_source_payload(2, &shape.encode().unwrap())
                .unwrap(),
        )
        .unwrap();
        receive_cursor(
            &state,
            MessageKind::CursorPosition,
            &crate::remote::source_wire::encode_source_payload(2, &position.encode().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(take_cursor_for_epoch(&state, 1).is_none());
        assert_eq!(take_cursor_for_epoch(&state, 2).unwrap().1, position);
    }

    #[test]
    fn geometry_transition_release_keeps_last_presented_scope() {
        let key = [0x53; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        write_source_release(&mut writer, (8, 3), true).unwrap();
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), key);
        let (_, bytes) = reader.read_message().unwrap();
        assert_eq!(
            crate::remote::messages::decode_source_input(&bytes).unwrap(),
            (8, 3, vec![InputEvent::ReleaseAll])
        );
    }

    #[test]
    fn view_only_source_switch_has_no_input_fence_on_wire() {
        let key = [0x53; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        write_source_release(&mut writer, (1, 1), false).unwrap();
        writer
            .write_message(
                MessageKind::SourceRequest,
                &SourceRequest {
                    expected_epoch: 1,
                    target: crate::remote::source_session::SourceId(2),
                }
                .encode()
                .unwrap(),
            )
            .unwrap();
        write_source_release(&mut writer, (1, 1), false).unwrap();
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), key);
        assert_eq!(reader.read_message().unwrap().0, MessageKind::SourceRequest);
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn source_cycle_uses_only_catalog_ids_and_current_epoch() {
        use crate::remote::source_session::{SourceId, SourceVersion};
        use crate::remote::source_wire::SourceEntry;
        let mut catalog = SourceCatalog {
            current: SourceVersion {
                source: SourceId(9),
                epoch: 3,
            },
            entries: vec![
                SourceEntry {
                    id: SourceId(9),
                    label: "One".into(),
                },
                SourceEntry {
                    id: SourceId(42),
                    label: "Two".into(),
                },
            ],
        };
        assert_eq!(next_source(&catalog, 3), Some((SourceId(42), "Two".into())));
        assert_eq!(next_source(&catalog, 2), None);
        catalog.current.source = SourceId(42);
        assert_eq!(next_source(&catalog, 3), Some((SourceId(9), "One".into())));
        catalog.entries.truncate(1);
        assert_eq!(next_source(&catalog, 3), None);
    }

    #[test]
    fn source_epochs_reject_future_and_drop_stale_before_decode() {
        assert!(accept_source_epoch(3, 3).unwrap());
        assert!(!accept_source_epoch(2, 3).unwrap());
        assert!(accept_source_epoch(4, 3).is_err());
        let state = ReceiveState::new();
        state.cursor_enabled.store(true, Ordering::Release);
        state.source_epoch.store(3, Ordering::Release);
        // Invalid inner bytes must not enter the cursor codec/cache when stale.
        let stale = crate::remote::source_wire::encode_source_payload(2, b"invalid").unwrap();
        receive_cursor(&state, MessageKind::CursorShape, &stale).unwrap();
        receive_cursor(&state, MessageKind::CursorPosition, &stale).unwrap();
        assert!(state.cursor.lock().unwrap().take().is_none());
    }

    #[test]
    fn source_catalog_is_immutable_after_initial_acceptance() {
        use crate::remote::source_session::{SourceId, SourceVersion};
        use crate::remote::source_wire::SourceEntry;
        let state = ReceiveState::new();
        let catalog = SourceCatalog {
            current: SourceVersion {
                source: SourceId(1),
                epoch: 1,
            },
            entries: vec![SourceEntry {
                id: SourceId(1),
                label: "Desktop".into(),
            }],
        };
        let bytes = catalog.encode().unwrap();
        receive_catalog(&state, &bytes).unwrap();
        assert!(receive_catalog(&state, &bytes).is_err());
    }

    #[test]
    fn input_scope_tracks_presented_epoch_without_changing_inner_batch() {
        let key = [0x44; 32];
        let mut wire = SessionWriter::new(Vec::new(), key);
        let mut scoped = ScopedInputWriter {
            writer: &mut wire,
            scope: (7, 9),
        };
        write_input_batch(&mut scoped, &[InputEvent::ReleaseAll], &mut Vec::new()).unwrap();
        let mut reader = SessionReader::new(Cursor::new(wire.into_inner()), key);
        let (kind, bytes) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::InputBatch);
        assert_eq!(
            crate::remote::messages::decode_source_input(&bytes).unwrap(),
            (7, 9, vec![InputEvent::ReleaseAll])
        );
    }

    #[test]
    fn host_cannot_enlarge_any_offered_frame_limit() {
        use crate::remote::messages::FrameLimits;
        let offered = FrameLimits {
            max_width: 1024,
            max_height: 768,
            max_pixels: 500_000,
        };
        let mut hello = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: false,
            clipboard_files_enabled: false,
        };
        for selected in [
            offered,
            FrameLimits {
                max_width: 640,
                max_height: 480,
                max_pixels: 100_000,
            },
        ] {
            hello.stream.frame_limits = selected;
            assert!(validate_server_frame_limits(&hello, offered).is_ok());
        }
        for selected in [
            FrameLimits {
                max_width: 1025,
                ..offered
            },
            FrameLimits {
                max_height: 769,
                ..offered
            },
            FrameLimits {
                max_pixels: 500_001,
                ..offered
            },
            FrameLimits {
                max_pixels: 0,
                ..offered
            },
        ] {
            hello.stream.frame_limits = selected;
            assert!(validate_server_frame_limits(&hello, offered).is_err());
        }
    }

    #[test]
    fn host_file_limit_cannot_enlarge_or_enable_a_disabled_budget() {
        for (enabled, selected, offered, valid) in [
            (false, 0, 0, true),
            (false, 0, 100, true),
            (false, 1, 100, false),
            (true, 0, 100, false),
            (true, 1, 0, false),
            (true, 100, 50, false),
            (true, 50, 100, true),
            (true, 100, 100, true),
        ] {
            let hello = ServerHello {
                stream: super::super::messages::StreamCapabilities {
                    clipboard_file_limit: selected,
                    ..Default::default()
                },
                pointer_enabled: false,
                keyboard_enabled: false,
                clipboard_enabled: enabled,
                clipboard_files_enabled: enabled,
            };
            assert_eq!(validate_server_file_limit(&hello, offered).is_ok(), valid);
        }
    }

    #[test]
    fn server_clipboard_policy_cannot_expand_request() {
        for requested_content in [0, 1, 3, 7] {
            for requested_direction in 0..=3 {
                for content in [0, 1, 3, 7] {
                    for direction in 0..=3 {
                        let hello = ServerHello {
                            stream: super::super::messages::StreamCapabilities {
                                clipboard_content: content,
                                clipboard_directions: direction,
                                ..Default::default()
                            },
                            pointer_enabled: false,
                            keyboard_enabled: false,
                            clipboard_enabled: content != 0,
                            clipboard_files_enabled: content & 4 != 0,
                        };
                        assert_eq!(
                            validate_server_clipboard_policy(
                                &hello,
                                (requested_content, requested_direction)
                            )
                            .is_ok(),
                            content & !requested_content == 0
                                && direction & !requested_direction == 0
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn error_reports_keep_display_and_refusal_stages_distinct() {
        for (stage, expected) in [
            (ClientStage::Setup, "No remote desktop connection"),
            (ClientStage::Connect, "listener"),
            (ClientStage::Negotiate, "another viewer"),
            (ClientStage::FirstFrame, "receive/decode"),
            (
                ClientStage::LocalClipboard,
                "local clipboard setup or capture failed",
            ),
            (ClientStage::Display, "local viewer setup or drawing"),
            (ClientStage::FirstFrameAck, "was drawn"),
            (ClientStage::Session, "verify the host identity"),
        ] {
            let failure = ClientFailure {
                stage,
                cause: io::Error::new(io::ErrorKind::ConnectionRefused, "test cause").into(),
            };
            let message = failure.to_string();
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("test cause"));
            assert!(std::error::Error::source(&failure).is_some());
        }
    }

    fn text_record(text: &str) -> Outgoing {
        Outgoing {
            kind: MessageKind::Clipboard,
            payload: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn viewer_clipboard_budget_yields_and_preserves_queued_record_order() {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        let mut next = 0;
        let per_pass = MAX_VIEWER_CLIPBOARD_RECORDS_PER_PASS;
        for expected_end in [per_pass, 2 * per_pass] {
            let mut receive = || {
                next += 1;
                assert!(next <= expected_end + 1, "clipboard pass did not yield");
                Ok(Some(text_record(&format!("text-{next}"))))
            };
            assert!(forward_viewer_clipboard_from(&mut receive, &mut writer, false).unwrap());
            assert_eq!(next, expected_end);
        }
        let mut reader = SessionReader::new(std::io::Cursor::new(writer.into_inner()), TEST_PSK);
        for index in 1..=2 * per_pass {
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Clipboard);
            assert_eq!(payload, format!("text-{index}").as_bytes());
        }
    }

    #[test]
    fn viewer_clipboard_budget_covers_a_whole_transfer_window() {
        // A pass must be able to flush every chunk the worker may have in
        // flight, or a transfer would crawl at one pass per heartbeat.
        let chunks = (super::super::clipboard_sync::TRANSFER_WINDOW_BYTES
            / super::super::clipboard_sync::TRANSFER_CHUNK_BYTES as u64)
            as usize;
        assert!(MAX_VIEWER_CLIPBOARD_RECORDS_PER_PASS >= chunks + 2);
    }

    #[test]
    fn viewer_clipboard_reports_a_worker_exit_after_a_full_pass() {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        let error =
            forward_viewer_clipboard_from(|| Err(std::sync::mpsc::RecvError), &mut writer, false)
                .unwrap_err();
        assert!(error.is::<ClientFailure>());
        assert!(writer.into_inner().is_empty());
    }

    #[test]
    fn viewer_clipboard_closed_is_classified_after_queued_protocol_text() {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        let mut calls = 0;
        let error = forward_viewer_clipboard_from(
            || {
                calls += 1;
                match calls {
                    1 => Ok(Some(text_record("queued"))),
                    2 => Err(std::sync::mpsc::RecvError),
                    _ => panic!("closed receiver must not spin"),
                }
            },
            &mut writer,
            false,
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        let failure = error.downcast_ref::<ClientFailure>().unwrap();
        assert!(matches!(failure.stage, ClientStage::LocalClipboard));
        assert!(failure.to_string().contains("setup or capture failed"));
        let mut reader = SessionReader::new(std::io::Cursor::new(writer.into_inner()), TEST_PSK);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::Clipboard);
        assert_eq!(payload, b"queued");
    }

    #[test]
    fn viewer_clipboard_idle_and_normal_close_do_not_report_failure() {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        assert!(!forward_viewer_clipboard_from(|| Ok(None), &mut writer, false).unwrap());
        assert!(
            !forward_viewer_clipboard_from(
                || panic!("normal close must not poll clipboard"),
                &mut writer,
                true
            )
            .unwrap()
        );
        assert!(writer.into_inner().is_empty());
    }

    #[test]
    fn viewer_clipboard_write_failure_keeps_transport_error_classification() {
        let mut writer = SessionWriter::new(std::io::sink(), TEST_PSK);
        assert!(!forward_viewer_clipboard_from(|| Ok(None), &mut writer, false).unwrap());
        struct RejectWrite;
        impl std::io::Write for RejectWrite {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "transport write failed",
                ))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = SessionWriter::new(RejectWrite, TEST_PSK);
        let error =
            forward_viewer_clipboard_from(|| Ok(Some(text_record("text"))), &mut writer, false)
                .unwrap_err();
        assert!(!error.is::<ClientFailure>());
        assert!(error.to_string().contains("transport write failed"));
    }

    #[test]
    fn viewer_clipboard_failure_returns_through_existing_cleanup_before_propagation() {
        let source = include_str!("client.rs");
        let body = source
            .split("fn run_client_session(")
            .nth(1)
            .unwrap()
            .split("fn negotiate_session(")
            .next()
            .unwrap();
        assert!(
            body.find("viewer_loop(").unwrap()
                < body.find("let cancel_result = cancel.cancel(").unwrap()
        );
        assert!(
            body.find("let cancel_result = cancel.cancel(").unwrap()
                < body.find("receiver.join_timeout(").unwrap()
        );
        assert!(
            body.find("receiver.join_timeout(").unwrap() < body.find("finish_session(").unwrap()
        );
    }

    #[test]
    fn a_vanished_host_is_described_rather_than_quoted_from_std() {
        let eof: RemoteError = Box::new(ProtocolError::Io(io::Error::from(
            io::ErrorKind::UnexpectedEof,
        )));
        let shown = describe_cause(&eof);
        assert!(shown.contains("without a goodbye"), "{shown}");
        let other: RemoteError = Box::new(ProtocolError::AuthenticationFailed);
        assert_eq!(describe_cause(&other), "remote peer authentication failed");
    }

    #[test]
    fn a_negotiation_hang_up_names_its_likely_cause() {
        let eof = || ProtocolError::Io(io::Error::from(io::ErrorKind::UnexpectedEof));
        let during_auth = explain_negotiation_eof(eof(), false).to_string();
        assert!(
            during_auth.contains("key files most likely differ"),
            "{during_auth}"
        );
        let after_auth = explain_negotiation_eof(eof(), true).to_string();
        assert!(after_auth.contains("accepted the key"), "{after_auth}");
        assert!(!after_auth.contains("fill whole buffer"));
        // Anything other than a hang-up keeps its own, more specific message.
        let other = explain_negotiation_eof(ProtocolError::AuthenticationFailed, false);
        assert_eq!(other.to_string(), "remote peer authentication failed");
    }

    #[test]
    fn heartbeats_renew_static_held_input_before_host_safety_release() {
        assert!(HEARTBEAT_INTERVAL.saturating_mul(2) < Duration::from_millis(600));
    }

    #[test]
    fn cursor_before_first_frame_is_bounded_and_requires_negotiation() {
        let shape = super::super::cursor::CursorShape {
            id: 1,
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![255; 4],
        };
        let position = super::super::cursor::CursorPosition {
            sequence: 0,
            geometry_epoch: 1,
            shape_id: 1,
            source_width: 640,
            source_height: 480,
            x: 10,
            y: 20,
            visible: true,
        };
        let state = ReceiveState::new();
        assert!(
            receive_cursor(
                &state,
                MessageKind::CursorShape,
                &crate::remote::source_wire::encode_source_payload(1, &shape.encode().unwrap())
                    .unwrap()
            )
            .is_err()
        );
        state.cursor_enabled.store(true, Ordering::Release);
        let key = [0x5b; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        writer
            .write_message(
                MessageKind::CursorShape,
                &crate::remote::source_wire::encode_source_payload(1, &shape.encode().unwrap())
                    .unwrap(),
            )
            .unwrap();
        writer
            .write_message(
                MessageKind::CursorPosition,
                &crate::remote::source_wire::encode_source_payload(1, &position.encode().unwrap())
                    .unwrap(),
            )
            .unwrap();
        writer.write_message(MessageKind::Frame, b"first").unwrap();
        let wire = writer.into_inner();
        let mut reader = SessionReader::new(std::io::Cursor::new(wire.clone()), key);
        let payload = read_initial_frame_records(&mut reader, false, true, |kind, payload| {
            receive_cursor(&state, kind, payload)
        })
        .unwrap();
        assert_eq!(payload, b"first");
        assert_eq!(state.cursor.lock().unwrap().take().unwrap().1, position);
        let mut reader = SessionReader::new(std::io::Cursor::new(wire), key);
        assert!(read_initial_frame_records(&mut reader, false, false, |_, _| Ok(())).is_err());
    }

    #[test]
    fn hello_ack_cannot_enable_unrequested_clipboard_or_input() {
        let mut hello = ServerHello {
            stream: Default::default(),
            pointer_enabled: false,
            keyboard_enabled: false,
            clipboard_enabled: true,
            clipboard_files_enabled: false,
        };
        assert!(validate_server_capabilities(&hello, false, false, false).is_err());
        assert!(validate_server_capabilities(&hello, false, true, false).is_ok());
        hello.clipboard_files_enabled = true;
        assert!(validate_server_capabilities(&hello, false, true, false).is_err());
        assert!(validate_server_capabilities(&hello, false, true, true).is_ok());
        hello.clipboard_enabled = false;
        hello.clipboard_files_enabled = false;
        hello.pointer_enabled = true;
        assert!(validate_server_capabilities(&hello, false, false, false).is_err());
        assert!(validate_server_capabilities(&hello, true, false, false).is_ok());
    }

    #[test]
    fn negotiated_clipboard_may_precede_the_first_video_record() {
        let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
        writer
            .write_message(MessageKind::Clipboard, b"synthetic fixture")
            .unwrap();
        writer
            .write_message(MessageKind::ClipboardTransfer, b"\x03\0\0\0\x01")
            .unwrap();
        writer
            .write_message(MessageKind::Frame, b"synthetic frame header")
            .unwrap();
        let bytes = writer.into_inner();
        let mut reader = SessionReader::new(Cursor::new(bytes.clone()), TEST_PSK);
        let mut copied = Vec::new();
        let frame = read_initial_frame_records(&mut reader, true, false, |kind, payload| {
            copied.push((kind, payload.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            copied,
            [
                (MessageKind::Clipboard, b"synthetic fixture".to_vec()),
                (MessageKind::ClipboardTransfer, b"\x03\0\0\0\x01".to_vec()),
            ]
        );
        assert_eq!(frame, b"synthetic frame header");
        let mut reader = SessionReader::new(Cursor::new(bytes), TEST_PSK);
        assert!(
            read_initial_frame_records(&mut reader, false, false, |_, _| {
                panic!("unnegotiated clipboard must never reach the setter")
            })
            .is_err()
        );
    }

    #[test]
    fn clipboard_traffic_cannot_extend_the_first_frame_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let sender = thread::spawn(move || {
            let mut writer = SessionWriter::new(sender, TEST_PSK);
            for _ in 0..100 {
                if writer
                    .write_message(MessageKind::Clipboard, b"fixture")
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut reader = SessionReader::new(receiver, TEST_PSK);
        let started = Instant::now();
        assert!(
            read_initial_frame(
                &mut reader,
                true,
                false,
                Duration::from_millis(40),
                |_, _| Ok(())
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        sender.join().unwrap();
    }

    /// One self-contained tile keyframe, as the host would put on the wire.
    fn tile_keyframe_payload(sequence: u64, frame: &CapturedFrame, quality: u8) -> Vec<u8> {
        let mut encoder = TileEncoder::new();
        let mut payload = Vec::new();
        let plan = encoder
            .plan(frame, TileEncodeRequest::Keyframe)
            .expect("a keyframe always plans");
        assert!(plan.emit && plan.keyframe);
        encoder
            .encode_into(&mut payload, sequence, frame, quality)
            .expect("a keyframe always encodes");
        payload
    }

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
            purpose: FramePurpose::Video,
            frame: FrameLease::Cpu(decoded_test_frame(sequence)),
            ready_at: Instant::now(),
            decode_elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn ready_x11_preparation_cannot_overtake_a_new_geometry_or_source() {
        let (jobs, _requests) = mpsc::sync_channel(1);
        let (replies, ready) = mpsc::sync_channel(1);
        let mut worker = X11PreparationWorker {
            jobs,
            ready,
            thread: ReceiverThread::spawn(|| {}).unwrap(),
            scratch: Vec::new(),
            pending_scope: None,
            obsolete: false,
        };
        let mut state = ReceiveState::new();
        state.telemetry.detailed = true;
        let queued = |seq, source, geometry| QueuedFrame {
            purpose: FramePurpose::Video,
            frame: FrameLease::Cpu(
                decoded_test_frame(seq)
                    .with_source_epoch(source)
                    .with_geometry_epoch(geometry),
            ),
            ready_at: Instant::now(),
            decode_elapsed: Duration::ZERO,
        };
        for (new_source, new_geometry) in [(1, 2), (2, 1)] {
            state.source_epoch.store(new_source, Ordering::Release);
            worker.pending_scope = Some((1, 1));
            worker.obsolete = false;
            replies
                .send(X11PreparationJob {
                    queued: queued(10, 1, 1),
                    output: PreparedX11Rgb {
                        plan: super::super::x11_viewer::CpuPreparation::new(0, (1, 1), (1, 1))
                            .unwrap(),
                        pixels: vec![0; 4],
                        packed: true,
                    },
                    work_elapsed: Duration::from_millis(1),
                })
                .unwrap_or_else(|_| panic!("preparation receiver was dropped"));
            state
                .latest
                .lock()
                .unwrap()
                .publish(queued(11, new_source, new_geometry));

            let (next, prepared) = worker
                .next_frame(&state, new_source, true)
                .unwrap()
                .unwrap();
            assert_eq!(next.frame.sequence(), 11);
            assert!(prepared.is_none());
            assert!(!worker.busy());
            assert!(
                worker
                    .next_frame(&state, new_source, true)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(state.telemetry.lock_window().x11_cpu_preparation.stale, 2);
        worker.close().unwrap();
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
    fn scoped_pointer_events_never_enter_legacy_input_batches() {
        use super::super::x11_viewer::WaylandPointerEventKind as E;
        let (batches, outcome) = collect_input_batches(
            [
                pointer_event(E::Motion, 10),
                pointer_event(E::LeftDown, 10),
                pointer_event(E::LeftUp, 10),
                ViewerEvent::WaylandPointerCancel,
                ViewerEvent::WaylandPointerButtonCancel { pressed: false },
                ViewerEvent::WaylandPointerRevoke,
            ],
            true,
            true,
        );
        assert!(batches.is_empty());
        assert!(!outcome.wrote_input);
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
    fn escape_shortcut_release_all_partitions_pointer_runs() {
        let (batches, _) = collect_input_batches(
            [
                ViewerEvent::Key {
                    keycode: 38,
                    pressed: true,
                },
                pointer(20),
                pointer(21),
                // The escape shortcut releases the grab, which the
                // viewer represents upstream as this ReleaseAll edge.
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

    #[derive(Default)]
    struct FailingInputWriter {
        fail_write: bool,
        writes: usize,
        flushes: usize,
    }

    impl RecordWriter for FailingInputWriter {
        fn write_message(&mut self, _: MessageKind, _: &[u8]) -> RemoteResult<()> {
            self.writes += 1;
            if self.fail_write {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "input write failed").into())
            } else {
                Ok(())
            }
        }
        fn flush(&mut self) -> RemoteResult<()> {
            self.flushes += 1;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "input flush failed").into())
        }
    }

    #[test]
    fn viewer_close_intent_survives_input_write_and_flush_failures() {
        for (full_batch, fail_write) in [(false, true), (true, true), (false, false)] {
            let mut events = if full_batch {
                vec![
                    ViewerEvent::Key {
                        keycode: 38,
                        pressed: true
                    };
                    MAX_INPUT_BATCH_EVENTS + 1
                ]
            } else {
                vec![]
            };
            events.extend([ViewerEvent::ReleaseAll, ViewerEvent::Close]);
            let mut writer = FailingInputWriter {
                fail_write,
                ..Default::default()
            };
            let error = dispatch_viewer_input(
                events,
                true,
                true,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut writer,
            )
            .unwrap_err();
            let cleanup = error.downcast_ref::<SessionCleanupError>().unwrap();
            assert!(cleanup.primary.as_ref().unwrap().is::<UserClosed>());
            assert_eq!(cleanup.failures.len(), 1);
            assert_eq!(
                cleanup.failures[0]
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
            assert_eq!(writer.writes, 1);
            assert_eq!(writer.flushes, usize::from(!fail_write));
        }
    }

    #[test]
    fn release_all_without_close_is_not_user_close() {
        let mut writer = FailingInputWriter {
            fail_write: true,
            ..Default::default()
        };
        let error = dispatch_viewer_input(
            vec![ViewerEvent::ReleaseAll],
            true,
            true,
            &mut Vec::new(),
            &mut Vec::new(),
            &mut writer,
        )
        .unwrap_err();
        assert!(error.is::<io::Error>());
        assert!(!error.is::<UserClosed>());
        assert!(!error.is::<SessionCleanupError>());
    }

    #[test]
    fn viewer_close_flushes_release_and_view_only_close_needs_no_write() {
        for input_enabled in [false, true] {
            let mut writer = SessionWriter::new(Vec::new(), TEST_PSK);
            let outcome = dispatch_viewer_input(
                vec![ViewerEvent::ReleaseAll, ViewerEvent::Close],
                input_enabled,
                input_enabled,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut writer,
            )
            .unwrap();
            assert!(outcome.close);
            assert_eq!(outcome.wrote_release_all, input_enabled);
            assert_eq!(writer.into_inner().is_empty(), !input_enabled);
        }
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
    fn bounded_event_polls_preserve_edges_wheel_and_close_release_order() {
        use super::super::pacing::EventPollBudget;
        let mut events: std::collections::VecDeque<_> = (0..254).map(pointer).collect();
        events.extend([
            ViewerEvent::Key {
                keycode: 38,
                pressed: true,
            },
            ViewerEvent::Button {
                button: 1,
                pressed: true,
            },
            pointer(254),
            pointer(255),
            ViewerEvent::Button {
                button: 4,
                pressed: true,
            },
            ViewerEvent::Button {
                button: 4,
                pressed: false,
            },
            pointer(256),
            ViewerEvent::Button {
                button: 1,
                pressed: false,
            },
            ViewerEvent::Key {
                keycode: 38,
                pressed: false,
            },
            ViewerEvent::ReleaseAll,
            ViewerEvent::Close,
            pointer(999),
        ]);
        let mut emitted = Vec::new();
        let mut saw_close = false;
        while !events.is_empty() && !saw_close {
            let mut budget = EventPollBudget::new();
            let mut polled = Vec::new();
            while budget.take() {
                let Some(event) = events.pop_front() else {
                    break;
                };
                polled.push(event);
            }
            let (batches, outcome) = collect_input_batches(polled, true, true);
            emitted.extend(batches.into_iter().flatten());
            saw_close = outcome.close;
            if saw_close {
                assert!(outcome.wrote_release_all);
            }
        }
        assert!(saw_close);
        assert_eq!(
            emitted,
            [
                input_pointer(253),
                InputEvent::Key {
                    keycode: 38,
                    pressed: true
                },
                InputEvent::Button {
                    button: 1,
                    pressed: true
                },
                input_pointer(255),
                InputEvent::Button {
                    button: 4,
                    pressed: true
                },
                InputEvent::Button {
                    button: 4,
                    pressed: false
                },
                input_pointer(256),
                InputEvent::Button {
                    button: 1,
                    pressed: false
                },
                InputEvent::Key {
                    keycode: 38,
                    pressed: false
                },
                InputEvent::ReleaseAll,
            ]
        );
    }

    #[test]
    fn recovered_latest_frame_supersedes_failed_pending_frames_without_acking_them() {
        let telemetry = ClientTelemetry::default();
        let now = Instant::now();
        let mut pending = (40..=42)
            .map(|sequence| PendingFrameTiming {
                purpose: FramePurpose::Video,
                source_size: (1, 1),
                viewport_revision: 0,
                source_epoch: 1,
                geometry_epoch: 0,
                sequence,
                ready_at: now,
                decode_elapsed: Duration::ZERO,
                draw_started: now,
                presentation_path: "rgb",
            })
            .collect();
        let key = [0x31; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        acknowledge_presentation(&mut writer, 42, &mut pending, &telemetry).unwrap();
        assert!(pending.is_empty());
        let window = telemetry.take_window();
        assert_eq!((window.drawn, window.acked), (1, 1));
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), key);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::FrameAck);
        assert_eq!(decode_source_frame_ack(&payload).unwrap().1, 42);
        assert!(reader.read_message().is_err());
    }

    #[test]
    fn asynchronous_completion_acks_only_the_expected_submitted_frame() {
        let telemetry = ClientTelemetry {
            detailed: true,
            ..ClientTelemetry::default()
        };
        let now = Instant::now();
        let mut pending = std::collections::VecDeque::from([PendingFrameTiming {
            purpose: FramePurpose::Video,
            source_size: (1, 1),
            viewport_revision: 0,
            source_epoch: 7,
            geometry_epoch: 0,
            sequence: 42,
            ready_at: now,
            decode_elapsed: Duration::ZERO,
            draw_started: now,
            presentation_path: "x11-vaapi-dmabuf-dri3",
        }]);
        let mut writer = SessionWriter::new(Vec::new(), [0x31; 32]);
        assert_eq!(telemetry.lock_window().native_presented, 0);
        acknowledge_presentation(&mut writer, 42, &mut pending, &telemetry).unwrap();
        assert!(pending.is_empty());
        assert_eq!(telemetry.window.lock().unwrap().drawn, 1);
        assert_eq!(telemetry.lock_window().native_presented, 1);
        assert!(acknowledge_presentation(&mut writer, 42, &mut pending, &telemetry).is_err());
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), [0x31; 32]);
        let (kind, payload) = reader.read_message().unwrap();
        assert_eq!(kind, MessageKind::FrameAck);
        assert_eq!(decode_source_frame_ack(&payload).unwrap(), (7, 42));
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
        assert_eq!(decode_source_frame_ack(&payload).unwrap().1, 9);
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
            purpose: FramePurpose::Video,
            frame: FrameLease::Cpu(decoded_test_frame_in_pool(20, Arc::clone(&pool))),
            ready_at: Instant::now(),
            decode_elapsed: Duration::ZERO,
        };
        let mut writer = SessionWriter::new(Vec::new(), key);
        draw_and_ack(&mut writer, queued, &telemetry, |_| Ok(false)).unwrap();
        assert!(writer.into_inner().is_empty());
        assert_eq!(decode_pool_snapshot(&pool).0, 1);

        let queued = QueuedFrame {
            purpose: FramePurpose::Video,
            frame: FrameLease::Cpu(decoded_test_frame_in_pool(21, Arc::clone(&pool))),
            ready_at: Instant::now(),
            decode_elapsed: Duration::ZERO,
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
    fn frame_read_telemetry_counts_only_authenticated_video_records() {
        let key = [0x29; 32];
        let mut writer = SessionWriter::new(Vec::new(), key);
        writer.write_message(MessageKind::Heartbeat, &[]).unwrap();
        writer.write_message(MessageKind::Frame, b"first").unwrap();
        writer
            .write_message(MessageKind::Frame, b"invalid")
            .unwrap();
        let mut wire = writer.into_inner();
        *wire.last_mut().unwrap() ^= 1;

        let telemetry = ClientTelemetry {
            detailed: true,
            ..Default::default()
        };
        let mut reader = SessionReader::new(Cursor::new(&wire), key);
        let mut payload = Vec::new();
        assert_eq!(
            telemetry.read_record(&mut reader, &mut payload).unwrap(),
            MessageKind::Heartbeat
        );
        assert!(telemetry.take_window().is_empty());
        assert_eq!(
            telemetry.read_record(&mut reader, &mut payload).unwrap(),
            MessageKind::Frame
        );
        assert_eq!(payload, b"first");
        let window = telemetry.take_window();
        assert!(!window.is_empty());
        assert_eq!(window.frame_header_wait.observations, 1);
        assert_eq!(window.frame_body_receive.observations, 1);
        assert_eq!(window.frame_authentication.observations, 1);
        assert!(matches!(
            telemetry.read_record(&mut reader, &mut payload),
            Err(ProtocolError::AuthenticationFailed)
        ));
        assert!(payload.is_empty());
        assert!(telemetry.take_window().is_empty());

        let undetailed = ClientTelemetry::default();
        let mut reader = SessionReader::new(Cursor::new(&wire), key);
        undetailed.read_record(&mut reader, &mut payload).unwrap();
        undetailed.read_record(&mut reader, &mut payload).unwrap();
        assert!(undetailed.take_window().is_empty());
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

    #[cfg(feature = "remote-h264")]
    #[test]
    fn hardware_rgb_materialization_keeps_its_own_telemetry_window() {
        use crate::remote::{
            frame::{DecodeBufferAllocation, DecodeBufferCheckout},
            h264::{DecodeTimings, DownloadTarget, HardwareRgbWork, RgbCopy, RgbInput},
        };

        let telemetry = ClientTelemetry {
            detailed: true,
            ..Default::default()
        };
        let started = Instant::now();
        let mut reporter = ClientTelemetryReporter::new_at(&telemetry, started);
        telemetry.record_hardware_decode(DecodeTimings {
            decode: Some(Duration::from_millis(2)),
            ..Default::default()
        });
        for (allocation, capacity_added_bytes, input, copy) in [
            (
                DecodeBufferAllocation::Fresh,
                36,
                RgbInput::Download(DownloadTarget::Populated),
                RgbCopy::FullFrame(36),
            ),
            (
                DecodeBufferAllocation::Reused,
                0,
                RgbInput::DirectMap(18),
                RgbCopy::Banded(36),
            ),
            (
                DecodeBufferAllocation::Grown,
                18,
                RgbInput::MapFallback(DownloadTarget::Reused),
                RgbCopy::Banded(36),
            ),
        ] {
            telemetry.record_hardware_decode(
                DecodeTimings {
                    download: matches!(input, RgbInput::Download(_) | RgbInput::MapFallback(_))
                        .then_some(Duration::from_millis(3)),
                    direct_map: matches!(input, RgbInput::DirectMap(_))
                        .then_some(Duration::from_millis(4)),
                    rgb_work: Some(HardwareRgbWork {
                        input,
                        copy,
                        checkout: None,
                    }),
                    ..Default::default()
                }
                .with_checkout(Some(DecodeBufferCheckout {
                    allocation,
                    requested_bytes: 36,
                    capacity_added_bytes,
                })),
            );
        }

        let report = reporter.maybe_report_at(started, true).unwrap();
        let record = report.json(
            ViewerTelemetryContext {
                correlation_id: 1,
                backend: ViewerBackend::X11,
                negotiated_codec: FrameCodec::H264,
                limits: Default::default(),
                input_mode: "view-only",
            },
            true,
        );
        assert_eq!(record["decoded"], 0);
        assert_eq!(record["stages"]["hardware_decode_ms"]["observations"], 1);
        assert_eq!(record["stages"]["hardware_rgb_copy_ms"]["observations"], 0);
        assert_eq!(record["stages"]["hardware_download_ms"]["observations"], 2);
        assert_eq!(
            record["stages"]["hardware_direct_map_ms"]["observations"],
            1
        );
        assert_eq!(
            record["hardware_rgb_work"],
            serde_json::json!({
                "pool": {
                    "fitting_checkouts": 1, "fresh_reserves": 1, "growth_reserves": 1,
                    "requested_bytes": 108, "added_capacity_bytes": 54,
                },
                "download": {
                    "empty_targets_populated": 1, "retained_targets_reused": 1,
                    "populated_targets_replaced": 0,
                },
                "vaapi_map": {
                    "mapped_frames": 1, "staged_bytes": 18, "fallback_frames": 1,
                },
                "copy": {
                    "full_frame_copies": 1, "full_frame_bytes": 36,
                    "banded_copies": 2, "banded_bytes": 72,
                },
            })
        );
        assert!(reporter.maybe_report_at(started, true).is_none());
    }

    #[test]
    fn authenticated_frame_is_received_even_when_jpeg_decode_fails() {
        let telemetry = ClientTelemetry::default();
        telemetry.record_received();
        assert!(
            decode_queued_frame(
                &mut TileDecoder::new(),
                b"not a frame",
                FramePurpose::Video,
                &telemetry,
                &new_decode_buffer_pool()
            )
            .is_err()
        );

        let window = telemetry.take_window();
        assert_eq!(window.received, 1);
        assert_eq!(window.decoded, 0);
        assert_eq!(window.decode_elapsed, Duration::ZERO);
    }

    #[test]
    fn same_pixel_proof_preserves_the_pending_repair_until_new_damage_or_scope() {
        use super::super::frame::TileSnapshot;

        let image = image::RgbImage::new(16, 16);
        let tiles = TileSnapshot::update(&image, None, &[1], 16);
        let pool = new_decode_buffer_pool();
        let queued = |sequence, geometry, purpose, snapshot| QueuedFrame {
            purpose,
            frame: FrameLease::Cpu(
                RecyclableDecodedFrame::from_tiles(sequence, 16, 16, snapshot, Arc::clone(&pool))
                    .with_geometry_epoch(geometry),
            ),
            ready_at: Instant::now(),
            decode_elapsed: Duration::ZERO,
        };
        let mut mailbox = PresentationMailbox::default();
        mailbox.publish(queued(1, 1, FramePurpose::VisualRepair, tiles.clone()));
        mailbox.publish(queued(2, 1, FramePurpose::NativeProof, tiles.clone()));
        assert_eq!(mailbox.take(1).unwrap().frame.sequence(), 1);
        assert_eq!(mailbox.take(1).unwrap().frame.sequence(), 2);

        mailbox.publish(queued(3, 1, FramePurpose::VisualRepair, tiles.clone()));
        mailbox.publish(queued(4, 1, FramePurpose::NativeProof, tiles.clone()));
        let changed = TileSnapshot::update(&image, Some(&tiles), &[1], 16);
        let [old_proof, old_repair] = mailbox.publish(queued(5, 1, FramePurpose::Video, changed));
        assert_eq!(old_proof.unwrap().frame.sequence(), 4);
        assert_eq!(old_repair.unwrap().frame.sequence(), 3);
        assert_eq!(mailbox.take(1).unwrap().frame.sequence(), 5);

        mailbox.publish(queued(6, 1, FramePurpose::VisualRepair, tiles.clone()));
        mailbox.publish(queued(7, 2, FramePurpose::NativeProof, tiles));
        assert_eq!(mailbox.take(1).unwrap().frame.sequence(), 7);
    }

    #[test]
    fn read_ahead_source_change_rejects_delta_using_the_old_canvas() {
        use crate::remote::source_session::{SourceId, SourceVersion};
        use crate::remote::source_wire::{
            SourceEntry, encode_frame_payload, encode_source_changed,
        };

        let timeout = Duration::from_secs(5);
        let old_source = SourceVersion {
            source: SourceId(1),
            epoch: 1,
        };
        let new_source = SourceVersion {
            source: SourceId(2),
            epoch: 2,
        };
        let catalog = SourceCatalog {
            current: old_source,
            entries: [old_source, new_source]
                .into_iter()
                .map(|version| SourceEntry {
                    id: version.source,
                    label: format!("Source {}", version.epoch),
                })
                .collect(),
        }
        .encode()
        .unwrap();

        let old_color = Rgb([24, 48, 96]);
        let patch_color = Rgb([190, 60, 40]);
        let new_color = Rgb([60, 170, 40]);
        let old_frame = CapturedFrame {
            geometry_epoch: 7,
            image: RgbImage::from_pixel(32, 32, old_color),
            source_width: 32,
            source_height: 32,
        };
        let mut encoder = TileEncoder::new();
        let plan = encoder
            .plan(&old_frame, TileEncodeRequest::Keyframe)
            .unwrap();
        assert!(plan.emit && plan.keyframe);
        let mut keyframe = Vec::new();
        encoder
            .encode_into(&mut keyframe, 0, &old_frame, 100)
            .unwrap();
        encoder.commit(&old_frame);
        let delta_frame = CapturedFrame {
            image: RgbImage::from_fn(32, 32, |x, y| {
                if x < 16 && y < 16 {
                    patch_color
                } else {
                    old_color
                }
            }),
            ..old_frame
        };
        let plan = encoder
            .plan(&delta_frame, TileEncodeRequest::Delta)
            .unwrap();
        assert!(plan.emit && !plan.keyframe);
        assert_eq!(plan.dirty_tiles, 1);
        let mut old_delta = Vec::new();
        encoder
            .encode_into(&mut old_delta, 1, &delta_frame, 100)
            .unwrap();
        let new_keyframe = tile_keyframe_payload(
            1,
            &CapturedFrame {
                image: RgbImage::from_pixel(32, 32, new_color),
                ..old_frame
            },
            100,
        );

        // The same delta is valid within the old source. Both new-source
        // records keep the dimensions and inner geometry epoch, so only
        // processing SourceChanged can invalidate the old decoder reference.
        for (source, encoded, colors) in [
            (old_source, &old_delta, Some([patch_color, old_color])),
            (new_source, &old_delta, None),
            (new_source, &new_keyframe, Some([new_color, new_color])),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut viewer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut host, _) = listener.accept().unwrap();
            for socket in [&host, &viewer] {
                socket.set_read_timeout(Some(timeout)).unwrap();
                socket.set_write_timeout(Some(timeout)).unwrap();
            }
            let (mut writer, mut reader) = thread::scope(|scope| {
                let host = scope.spawn(move || {
                    let (_, send_key) = server_handshake(&mut host, &TEST_PSK, [0x27; 32])
                        .unwrap()
                        .into_server();
                    SessionWriter::new(host, send_key)
                });
                let (receive_key, _) = client_handshake(&mut viewer, &TEST_PSK, [0x19; 32])
                    .unwrap()
                    .into_client();
                (
                    host.join().unwrap(),
                    SessionReader::new(viewer, receive_key),
                )
            });
            writer
                .write_message(MessageKind::SourceCatalog, &catalog)
                .unwrap();
            writer
                .write_message(
                    MessageKind::Frame,
                    &encode_frame_payload(old_source.epoch, FramePurpose::Video, &keyframe)
                        .unwrap(),
                )
                .unwrap();
            writer.flush().unwrap();

            let mut state = ReceiveState::new();
            state.read_ahead = true;
            state.telemetry.detailed = true;
            let state = Arc::new(state);
            let payload =
                read_initial_frame(&mut reader, false, false, timeout, |kind, payload| {
                    assert_eq!(kind, MessageKind::SourceCatalog);
                    receive_catalog(&state, payload)
                })
                .unwrap();
            let mut decoder = state.new_video_decoder().unwrap();
            let (epoch, purpose, inner) = decode_frame_payload(&payload).unwrap();
            let first =
                decode_popup_queued_frame(&state, &mut decoder, inner, purpose, epoch).unwrap();
            assert_eq!(
                (epoch, first.frame.sequence(), first.frame.geometry_epoch()),
                (1, 0, 7)
            );

            if source != old_source {
                writer
                    .write_message(
                        MessageKind::SourceChanged,
                        &encode_source_changed(source).unwrap(),
                    )
                    .unwrap();
            }
            writer
                .write_message(
                    MessageKind::Frame,
                    &encode_frame_payload(source.epoch, FramePurpose::Video, encoded).unwrap(),
                )
                .unwrap();
            writer.flush().unwrap();

            let (mut wake_receiver, wake_sender) = wake_pair().unwrap();
            let cancel =
                SessionCancel::new(reader.get_ref().try_clone().unwrap(), Arc::clone(&state));
            let receiver_state = Arc::clone(&state);
            let receiver = ReceiverThread::spawn(move || {
                receive_frames(
                    reader,
                    receiver_state,
                    0,
                    payload,
                    wake_sender,
                    decoder,
                    None,
                );
            })
            .unwrap();
            let deadline = Instant::now() + timeout;
            let (published, terminal) = loop {
                let published = take_latest(&state, source.epoch);
                let terminal = !state.alive.load(Ordering::Acquire);
                if published.is_some() || terminal {
                    break (published, terminal);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero()
                    || !super::super::x11_capture::wait_readable(wake_receiver.as_fd(), remaining)
                        .unwrap()
                {
                    break (None, false);
                }
                wake_receiver.drain().unwrap();
            };
            // Keep the writer open until the decoder has decided. EOF at the
            // read-ahead producer could otherwise preempt the queued records.
            drop(cancel);
            drop(writer);
            assert_eq!(receiver.join_timeout(timeout), ReceiverExit::Completed);
            assert_eq!(state.source_epoch.load(Ordering::Acquire), source.epoch);
            let telemetry = state.telemetry.take_window();
            assert_eq!(telemetry.decode_queue_wait.observations, 1);
            assert_eq!(telemetry.frame_header_wait.observations, 1);
            assert_eq!(telemetry.frame_body_receive.observations, 1);
            assert_eq!(telemetry.frame_authentication.observations, 1);
            let error = state.error.lock().unwrap().take();
            if let Some(colors) = colors {
                assert!(
                    !terminal && error.is_none(),
                    "unexpected decoder error: {error:?}"
                );
                let frame = published.expect("valid frame was not published").frame;
                assert_eq!(
                    (frame.sequence(), frame.geometry_epoch(), frame.dimensions()),
                    (1, 7, (32, 32))
                );
                let image = frame.cpu().unwrap().image();
                for ((x, y), expected) in [(8, 8), (24, 24)].into_iter().zip(colors) {
                    let actual = image.get_pixel(x, y);
                    assert!(
                        actual
                            .0
                            .into_iter()
                            .zip(expected.0)
                            .all(|(a, b)| a.abs_diff(b) <= 5),
                        "pixel ({x}, {y}) expected {expected:?}, got {actual:?}"
                    );
                }
            } else {
                assert!(
                    published.is_none() && terminal,
                    "old reference crossed SourceChanged"
                );
                let error = error.expect("decoder did not reject the old reference");
                assert_eq!(
                    error.to_string(),
                    "tile delta frame does not match the current source geometry"
                );
                assert!(state.latest.lock().unwrap().is_none());
            }
        }
    }

    #[test]
    fn receiver_sequence_error_drops_tiles_without_materializing_or_publishing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        let mut writer = SessionWriter::new(sender, TEST_PSK);
        let payload = tile_keyframe_payload(
            2,
            &CapturedFrame {
                geometry_epoch: 0,
                image: RgbImage::from_pixel(32, 18, Rgb([20, 40, 80])),
                source_width: 32,
                source_height: 18,
            },
            80,
        );
        writer
            .write_message(
                MessageKind::Frame,
                &crate::remote::source_wire::encode_frame_payload(1, FramePurpose::Video, &payload)
                    .unwrap(),
            )
            .unwrap();
        writer.flush().unwrap();

        let state = Arc::new(ReceiveState::new());
        let (_wake_receiver, wake_sender) = wake_pair().unwrap();
        receive_frames(
            SessionReader::new(receiver, TEST_PSK),
            Arc::clone(&state),
            0,
            Vec::new(),
            wake_sender,
            TileDecoder::new().into(),
            None,
        );

        assert!(!state.alive.load(Ordering::Acquire));
        assert!(state.latest.lock().unwrap().is_none());
        assert!(
            state
                .error
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| error.to_string().contains("expected 1, got 2"))
        );
        // Rejected tile snapshots never materialize a full RGB frame.
        assert_eq!(decode_pool_snapshot(&state.decode_pool).0, 0);
    }

    #[test]
    fn receiver_authentication_failure_never_checks_out_a_decode_buffer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (receiver, _) = listener.accept().unwrap();
        let mut writer = SessionWriter::new(sender, [0x91; PSK_LEN]);
        let payload = tile_keyframe_payload(
            1,
            &CapturedFrame {
                geometry_epoch: 0,
                image: RgbImage::from_pixel(32, 18, Rgb([20, 40, 80])),
                source_width: 32,
                source_height: 18,
            },
            80,
        );
        writer
            .write_message(
                MessageKind::Frame,
                &crate::remote::source_wire::encode_frame_payload(1, FramePurpose::Video, &payload)
                    .unwrap(),
            )
            .unwrap();
        writer.flush().unwrap();

        let state = Arc::new(ReceiveState::new());
        let (_wake_receiver, wake_sender) = wake_pair().unwrap();
        receive_frames(
            SessionReader::new(receiver, TEST_PSK),
            Arc::clone(&state),
            0,
            Vec::new(),
            wake_sender,
            TileDecoder::new().into(),
            None,
        );

        assert!(!state.alive.load(Ordering::Acquire));
        assert!(state.latest.lock().unwrap().is_none());
        assert_eq!(decode_pool_snapshot(&state.decode_pool).0, 0);
        assert_eq!(state.telemetry.take_window().received, 0);
        assert!(matches!(
            state
                .error
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .downcast_ref::<ProtocolError>(),
            Some(ProtocolError::AuthenticationFailed)
        ));
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
            WaitOutcome::ViewerHangup
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
            state
                .error
                .lock()
                .unwrap()
                .as_ref()
                .map(ToString::to_string),
            Some("remote video thread panicked".to_string())
        );
    }

    fn certified_disconnect() -> RemoteError {
        let loss = receiver_read_error(ProtocolError::Io(io::ErrorKind::UnexpectedEof.into()));
        let cause =
            finish_session(Err(loss), Ok(()), ReceiverExit::Completed, Ok(()), Ok(())).unwrap_err();
        Box::new(ClientFailure {
            stage: ClientStage::Session,
            cause,
        })
    }

    #[test]
    fn late_close_escape_or_pending_events_after_cleanup_cancel_retry() {
        let loss = || {
            Err(receiver_read_error(ProtocolError::Io(
                io::ErrorKind::UnexpectedEof.into(),
            )))
        };
        for late_intent in [
            Ok(false),
            Err(io::Error::other("local polling failed").into()),
        ] {
            let cleaned = finish_session(loss(), Ok(()), ReceiverExit::Completed, Ok(()), Ok(()))
                .unwrap_err();
            assert!(cleaned.is::<CleanTransportDisconnect>());
            // Close/escape arrives only after workers have joined.
            let result = finalize_retry_intent(Err(cleaned), || late_intent);
            let cause = result.unwrap_err();
            assert!(!cause.is::<CleanTransportDisconnect>());
            let mut error = Some(Box::new(ClientFailure {
                stage: ClientStage::Session,
                cause,
            }) as RemoteError);
            reconnect_with(
                5,
                || Err(error.take().expect("late close must not retry")),
                |_, _| panic!("late close must not wait"),
            )
            .unwrap_err();
        }
        let cleaned = finish_session(loss(), Ok(()), ReceiverExit::Completed, Ok(()), Ok(()));
        assert!(
            finalize_retry_intent(cleaned, || Ok(true))
                .unwrap_err()
                .is::<CleanTransportDisconnect>()
        );
        assert!(
            finalize_retry_intent(Ok(()), || panic!("normal close must not poll for retry"))
                .is_ok()
        );
    }

    #[test]
    fn reconnect_budget_and_backoff_are_bounded_without_reset() {
        for budget in 0..=5 {
            let mut calls = 0;
            let mut waits = Vec::new();
            reconnect_with(
                budget,
                || {
                    calls += 1;
                    Err(certified_disconnect())
                },
                |attempt, delay| waits.push((attempt, delay.as_secs())),
            )
            .unwrap_err();
            assert_eq!(calls, budget + 1);
            assert_eq!(
                waits,
                (1..=budget)
                    .map(|i| (i, [1, 2, 4, 8, 8][usize::from(i - 1)]))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn reconnect_stops_on_normal_close_or_uncertified_failure() {
        for terminal in [
            None,
            Some(Box::new(UserClosed) as RemoteError),
            Some(Box::new(PeerClosed::legacy(b"stop"))),
            Some(Box::new(io::Error::from(io::ErrorKind::BrokenPipe))),
        ] {
            let mut terminal = Some(terminal);
            let mut calls = 0;
            let mut waits = 0;
            let result = reconnect_with(
                5,
                || {
                    calls += 1;
                    if calls == 1 {
                        return Err(certified_disconnect());
                    }
                    match terminal.take().unwrap() {
                        None => Ok(()),
                        Some(cause) => Err(Box::new(ClientFailure {
                            stage: ClientStage::Session,
                            cause,
                        })),
                    }
                },
                |_, _| waits += 1,
            );
            assert_eq!(calls, 2);
            assert_eq!(waits, 1);
            let _ = result;
        }
    }

    #[test]
    fn only_reader_originated_allowlisted_errors_can_be_certified() {
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::TimedOut,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidData,
            io::ErrorKind::Other,
        ] {
            let allowed = matches!(
                kind,
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
            );
            let marked = receiver_read_error(ProtocolError::Io(kind.into()));
            assert_eq!(marked.is::<ReceiverTransportLoss>(), allowed);
            let result =
                finish_session(Err(marked), Ok(()), ReceiverExit::Completed, Ok(()), Ok(()))
                    .unwrap_err();
            assert_eq!(result.is::<CleanTransportDisconnect>(), allowed);
            // An identical error from setup, decode, X11 or sender is not marked.
            let result = finish_session(
                Err(Box::new(ProtocolError::Io(kind.into()))),
                Ok(()),
                ReceiverExit::Completed,
                Ok(()),
                Ok(()),
            )
            .unwrap_err();
            assert!(!result.is::<CleanTransportDisconnect>());
        }
        assert!(
            !receiver_read_error(ProtocolError::AuthenticationFailed).is::<ReceiverTransportLoss>()
        );
        // Even a reader-origin marker alone is not authorization to retry.
        let mut calls = 0;
        reconnect_with(
            5,
            || {
                calls += 1;
                Err(Box::new(ClientFailure {
                    stage: ClientStage::Session,
                    cause: receiver_read_error(ProtocolError::Io(
                        io::ErrorKind::UnexpectedEof.into(),
                    )),
                }))
            },
            |_, _| panic!("uncertified loss cannot retry"),
        )
        .unwrap_err();
        assert_eq!(calls, 1);
    }

    #[test]
    fn user_escape_close_and_each_cleanup_failure_block_certification() {
        let loss = || {
            Err(receiver_read_error(ProtocolError::Io(
                io::ErrorKind::UnexpectedEof.into(),
            )))
        };
        let failed = || Err(io::Error::other("incomplete").into());
        let suppressed = suppress_reconnect(loss(), true);
        let error = finish_session(suppressed, Ok(()), ReceiverExit::Completed, Ok(()), Ok(()))
            .unwrap_err();
        assert!(error.is::<ProtocolError>());
        assert!(suppress_reconnect(Ok(()), true).is_ok());
        for index in 0..4 {
            let error = finish_session(
                loss(),
                if index == 0 { failed() } else { Ok(()) },
                if index == 1 {
                    ReceiverExit::TimedOut
                } else {
                    ReceiverExit::Completed
                },
                if index == 2 { failed() } else { Ok(()) },
                if index == 3 { failed() } else { Ok(()) },
            )
            .unwrap_err();
            assert!(error.is::<SessionCleanupError>());
            assert!(!error.is::<CleanTransportDisconnect>());
        }
    }

    #[test]
    fn local_release_error_still_runs_all_remaining_cleanup_in_order() {
        let mut steps = Vec::new();
        let (local, remaining) =
            run_remaining_cleanup(Err(io::Error::other("local release failed").into()), || {
                steps.push("cancel");
                let cancel: RemoteResult<()> = Err(io::Error::other("cancel failed").into());
                steps.push("receiver");
                let receiver = ReceiverExit::Completed;
                steps.push("sender");
                let sender: RemoteResult<()> = Ok(());
                steps.push("clipboard");
                let clipboard: RemoteResult<()> = Ok(());
                (cancel, receiver, sender, clipboard)
            });
        assert_eq!(steps, ["cancel", "receiver", "sender", "clipboard"]);
        let result = finish_session(Ok(()), remaining.0, remaining.1, remaining.2, remaining.3);
        let error = include_local_cleanup(result, local).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SessionCleanupError>()
                .unwrap()
                .failures
                .len(),
            2
        );
    }

    #[test]
    fn local_release_failure_preserves_worker_errors_and_forbids_retry() {
        let failure =
            || -> RemoteResult<()> { Err(io::Error::other("local release failed").into()) };
        let loss = || -> RemoteResult<()> {
            Err(receiver_read_error(ProtocolError::Io(
                io::ErrorKind::UnexpectedEof.into(),
            )))
        };
        let clean = finish_session(loss(), Ok(()), ReceiverExit::Completed, Ok(()), Ok(()));
        let result = include_local_cleanup(clean, failure());
        let error = finalize_retry_intent(result, || {
            panic!("failed local release must not get retry certificate")
        })
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SessionCleanupError>()
                .unwrap()
                .failures
                .len(),
            1
        );
        let workers = finish_session(
            loss(),
            Err(io::Error::other("cancel").into()),
            ReceiverExit::TimedOut,
            Err(io::Error::other("sender").into()),
            Err(io::Error::other("clipboard").into()),
        );
        let error = include_local_cleanup(workers, failure()).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<SessionCleanupError>()
                .unwrap()
                .failures
                .len(),
            5
        );
        let error = include_local_cleanup(Ok(()), failure()).unwrap_err();
        assert!(
            error
                .downcast_ref::<SessionCleanupError>()
                .unwrap()
                .primary
                .is_none()
        );
        assert!(include_local_cleanup(Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn receiver_eof_with_idle_sender_has_clean_teardown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let control = socket.try_clone().unwrap();
        let mut reader = SessionReader::new(socket.try_clone().unwrap(), TEST_PSK);
        let mut sender = Sender::spawn(
            SessionWriter::new(socket, TEST_PSK),
            false,
            false,
            |_, _| {},
        )
        .unwrap();
        let peer = thread::spawn(move || {
            let mut writer = SessionWriter::new(peer, TEST_PSK);
            writer.write_message(MessageKind::Heartbeat, &[]).unwrap();
            writer.flush().unwrap();
        });
        // A successfully authenticated record precedes the unexpected EOF.
        assert_eq!(reader.read_message().unwrap().0, MessageKind::Heartbeat);
        peer.join().unwrap();
        let (error_tx, error_rx) = mpsc::channel();
        let receiver = ReceiverThread::spawn(move || {
            error_tx.send(reader.read_message().unwrap_err()).unwrap();
        })
        .unwrap();
        let error = error_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(&error, ProtocolError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof));
        let state = Arc::new(ReceiveState::new());
        let mut cancel = SessionCancel::new(control, state);
        let cancelled = cancel.cancel(&mut sender, false);
        let receiver = receiver.join_timeout(Duration::from_secs(2));
        let sender = sender.shutdown_timeout(Duration::from_secs(2));
        let result = finish_session(
            Err(receiver_read_error(error)),
            cancelled,
            receiver,
            sender,
            Ok(()),
        );
        let error = result.unwrap_err();
        assert!(error.is::<CleanTransportDisconnect>());
        assert!(!error.is::<SessionCleanupError>());
    }

    #[test]
    fn only_closed_sender_cleanup_has_readonly_test_provenance() {
        let peer = || -> RemoteError {
            PeerClosed::decode(
                MessageKind::SessionClose,
                &SessionClose::new(SessionCloseCode::CaptureUnavailable, "capture unavailable")
                    .encode(),
            )
            .unwrap()
            .into()
        };
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Other,
        ] {
            for extra in 0..4 {
                let error = finish_session(
                    Err(peer()),
                    if extra == 1 {
                        Err(io::Error::other("cancel").into())
                    } else {
                        Ok(())
                    },
                    if extra == 2 {
                        ReceiverExit::TimedOut
                    } else {
                        ReceiverExit::Completed
                    },
                    Err(Box::new(
                        super::super::sender::SenderShutdownTransportError {
                            kind,
                            message: "sender".into(),
                        },
                    )),
                    if extra == 3 {
                        Err(io::Error::other("clipboard").into())
                    } else {
                        Ok(())
                    },
                )
                .unwrap_err();
                let summary = classify_failure(error.as_ref());
                assert_eq!(summary.category, "cleanup_failed");
                assert_eq!(summary.cleanup, "incomplete");
                assert_eq!(
                    summary.peer_close_code,
                    Some(SessionCloseCode::CaptureUnavailable)
                );
                assert!(!summary.reconnect_certified);
                assert_eq!(
                    summary.cleanup_sender_transport_only,
                    extra == 0
                        && matches!(
                            kind,
                            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                        )
                );
                assert!(
                    !serde_json::to_string(&summary)
                        .unwrap()
                        .contains("cleanup_sender_transport_only")
                );
            }
        }
        let inner = finish_session(
            Err(peer()),
            Err(io::Error::other("cancel").into()),
            ReceiverExit::Completed,
            Ok(()),
            Ok(()),
        )
        .unwrap_err();
        let outer = finish_session(
            Err(inner),
            Ok(()),
            ReceiverExit::Completed,
            Err(Box::new(
                super::super::sender::SenderShutdownTransportError {
                    kind: io::ErrorKind::BrokenPipe,
                    message: "sender".into(),
                },
            )),
            Ok(()),
        )
        .unwrap_err();
        assert!(!classify_failure(outer.as_ref()).cleanup_sender_transport_only);
        let direct = finish_session(
            Err(peer()),
            Ok(()),
            ReceiverExit::Completed,
            Err(Box::new(
                super::super::sender::SenderShutdownTransportError {
                    kind: io::ErrorKind::BrokenPipe,
                    message: "worker".into(),
                },
            )),
            Ok(()),
        )
        .unwrap_err();
        let nested = SessionCleanupError {
            sender_transport_only: false,
            primary: Some(direct),
            failures: vec![io::Error::other("unknown cleanup").into()],
        };
        let wrapped = io::Error::other(nested);
        assert!(!classify_failure(&wrapped).cleanup_sender_transport_only);

        let no_primary = finish_session(
            Ok(()),
            Ok(()),
            ReceiverExit::Completed,
            Err(Box::new(
                super::super::sender::SenderShutdownTransportError {
                    kind: io::ErrorKind::BrokenPipe,
                    message: "sender".into(),
                },
            )),
            Ok(()),
        )
        .unwrap_err();
        assert!(!classify_failure(no_primary.as_ref()).cleanup_sender_transport_only);
        let non_worker = finish_session(
            Err(peer()),
            Ok(()),
            ReceiverExit::Completed,
            Err(io::Error::from(io::ErrorKind::BrokenPipe).into()),
            Ok(()),
        )
        .unwrap_err();
        assert!(!classify_failure(non_worker.as_ref()).cleanup_sender_transport_only);
    }

    #[test]
    fn terminal_types_and_cleanup_failures_are_preserved() {
        let auth: RemoteError = Box::new(ProtocolError::AuthenticationFailed);
        let error =
            finish_session(Err(auth), Ok(()), ReceiverExit::Completed, Ok(()), Ok(())).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ProtocolError>(),
            Some(ProtocolError::AuthenticationFailed)
        ));
        let peer: RemoteError = Box::new(PeerClosed::legacy(&[]));
        let error =
            finish_session(Err(peer), Ok(()), ReceiverExit::Completed, Ok(()), Ok(())).unwrap_err();
        assert!(error.is::<PeerClosed>());
        let transport: RemoteError = Box::new(ProtocolError::Io(io::Error::from(
            io::ErrorKind::UnexpectedEof,
        )));
        let error = finish_session(
            Err(transport),
            Err(io::Error::other("cancel failed").into()),
            ReceiverExit::TimedOut,
            Err(io::Error::other("sender failed").into()),
            Err(io::Error::other("clipboard cleanup incomplete").into()),
        )
        .unwrap_err();
        let cleanup = error.downcast_ref::<SessionCleanupError>().unwrap();
        assert_eq!(cleanup.failures.len(), 4);
        assert!(matches!(
            cleanup
                .primary
                .as_ref()
                .unwrap()
                .downcast_ref::<ProtocolError>(),
            Some(ProtocolError::Io(_))
        ));
        assert!(
            cleanup.failures[1]
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::TimedOut)
        );
        assert!(error.to_string().contains("without a goodbye"));
        assert!(error.to_string().contains("detached"));
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
        // Run the real server handshake, not a hand-written invalid transcript.
        // Deliver the v3 seven-byte magic/version prefix immediately, then drip
        // the valid public nonce. No Noise ciphertext or private key is fabricated.
        struct SlowHandshake {
            stream: TcpStream,
            written: usize,
        }
        impl Read for SlowHandshake {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.stream.read(bytes)
            }
        }
        impl Write for SlowHandshake {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.is_empty() {
                    return Ok(0);
                }
                let count = if self.written < 7 {
                    bytes.len().min(7 - self.written)
                } else {
                    thread::sleep(Duration::from_millis(20));
                    1
                };
                let sent = self.stream.write(&bytes[..count])?;
                self.written += sent;
                Ok(sent)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.stream.flush()
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut stream = SlowHandshake { stream, written: 0 };
            let result = server_handshake(&mut stream, &TEST_PSK, [0x24; 32]);
            assert!(
                matches!(result, Err(ProtocolError::Io(_))),
                "client deadline must close the real handshake"
            );
            assert!(stream.written >= 7);
        });

        let stream = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        let result = negotiate_session(
            false,
            false,
            false,
            false,
            false,
            false,
            InputMode::X11Keycodes,
            false,
            Default::default(),
            false,
            false,
            false,
            stream,
            &TEST_PSK,
            false,
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            None,
            false,
            Default::default(),
            FrameCodec::JpegTiles,
            Duration::from_millis(80),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("slow handshake unexpectedly succeeded"),
        };
        assert!(matches!(
            classify_failure(error.as_ref()).io_kind,
            Some("connection_aborted" | "timed_out" | "would_block" | "connection_reset")
        ));
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "not an early malformed-flight rejection"
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }

    #[test]
    fn negotiation_deadline_also_covers_a_slow_hello_ack() {
        const BUDGET: Duration = Duration::from_secs(2);
        const DRIP: Duration = Duration::from_millis(100);
        let (phase_tx, phase_rx) = std::sync::mpsc::channel();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let _watchdog = TcpStreamDeadline::arm(&stream, Duration::from_secs(5)).unwrap();
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
                        stream: Default::default(),
                        pointer_enabled: false,
                        keyboard_enabled: false,
                        clipboard_enabled: false,
                        clipboard_files_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            let bytes = encoded.into_inner();
            assert!(DRIP * (bytes.len() as u32) > BUDGET * 2);
            let mut sent = 0;
            for byte in bytes {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                sent += 1;
                if sent == 1 {
                    phase_tx.send(()).unwrap();
                }
                thread::sleep(DRIP);
            }
        });

        let stream = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        let result = negotiate_session(
            false,
            false,
            false,
            false,
            false,
            false,
            InputMode::X11Keycodes,
            false,
            Default::default(),
            false,
            false,
            false,
            stream,
            &TEST_PSK,
            false,
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            None,
            false,
            Default::default(),
            FrameCodec::JpegTiles,
            BUDGET,
        );
        let elapsed = started.elapsed();
        // The first actual encrypted ACK byte was written only after a valid
        // authenticated Hello. A failure in Noise setup cannot pass this test.
        phase_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("server reached and began the real HelloAck drip");
        server.join().unwrap();
        assert!(result.is_err());
        assert!(elapsed >= BUDGET - Duration::from_millis(100));
        assert!(
            elapsed < BUDGET + Duration::from_secs(1),
            "per-byte progress extended the phase deadline: {elapsed:?}"
        );
    }

    #[test]
    fn refusal_reasons_are_escaped_and_bounded_for_the_terminal() {
        assert_eq!(
            refusal_reason_for_display(b"another viewer is already connected to this host"),
            "another viewer is already connected to this host"
        );
        let shown = refusal_reason_for_display(b"\x1b[2Jjwm-remote: connected\n\x1b]52;c;aGk=\x07");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.contains("\\u{1b}[2J"));
        assert!(shown.contains("\\n"));
        let long = refusal_reason_for_display(&[b'x'; 64 * 1024]);
        assert!(long.len() <= MAX_REFUSAL_REASON_BYTES + '…'.len_utf8());
        assert!(long.ends_with('…'));
    }

    #[test]
    fn an_oversized_refusal_is_rejected_before_it_is_buffered() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x3a; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, _) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            // The client may hang up as soon as it reads the length.
            let _ = writer.write_message(MessageKind::Close, &vec![b'x'; 64 * 1024]);
            let _ = writer.flush();
        });
        let stream = TcpStream::connect(address).unwrap();
        let result = negotiate_session(
            false,
            false,
            false,
            false,
            false,
            false,
            InputMode::X11Keycodes,
            false,
            Default::default(),
            false,
            false,
            false,
            stream,
            &TEST_PSK,
            false,
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            None,
            false,
            Default::default(),
            FrameCodec::JpegTiles,
            Duration::from_secs(5),
        );
        server.join().unwrap();
        let error = result.err().expect("a 64 KiB refusal must not be accepted");
        assert!(
            error.to_string().contains("too large") || error.to_string().contains("payload"),
            "{error}"
        );
    }

    #[test]
    fn explicit_resize_negotiation_never_accepts_an_unrequested_grant() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x41; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            assert!(!ClientHello::decode(&payload).unwrap().stream.window_resize);
            writer
                .write_message(
                    MessageKind::HelloAck,
                    &ServerHello {
                        stream: super::super::messages::StreamCapabilities {
                            window_resize: true,
                            ..Default::default()
                        },
                        pointer_enabled: false,
                        keyboard_enabled: false,
                        clipboard_enabled: false,
                        clipboard_files_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            writer.flush().unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        let result = negotiate_session(
            false,
            false,
            false,
            false,
            false,
            false,
            InputMode::X11Keycodes,
            false,
            Default::default(),
            false,
            false,
            false,
            stream,
            &TEST_PSK,
            false,
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            None,
            false,
            Default::default(),
            FrameCodec::JpegTiles,
            Duration::from_secs(5),
        );
        server.join().unwrap();
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("unrequested native resize")
        );
    }

    #[test]
    fn authenticated_ack_cannot_enable_unrequested_clipboard() {
        // This regression calls only APIs already present in the baseline.
        // It can be transplanted unchanged to show the old execution path
        // accepts an unsolicited grant; no X11 or real clipboard is touched.
        for request_clipboard in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let keys = server_handshake(&mut stream, &TEST_PSK, [0x39; 32]).unwrap();
                let reader_stream = stream.try_clone().unwrap();
                let (receive_key, send_key) = keys.into_server();
                let mut reader = SessionReader::new(reader_stream, receive_key);
                let mut writer = SessionWriter::new(stream, send_key);
                let (kind, payload) = reader.read_message().unwrap();
                assert_eq!(kind, MessageKind::Hello);
                let hello = ClientHello::decode(&payload).unwrap();
                assert_eq!(hello.request_clipboard, request_clipboard);
                writer
                    .write_message(
                        MessageKind::HelloAck,
                        &ServerHello {
                            stream: super::super::messages::StreamCapabilities {
                                clipboard_content: 3,
                                clipboard_directions: 3,
                                ..Default::default()
                            },
                            pointer_enabled: false,
                            keyboard_enabled: false,
                            clipboard_enabled: true,
                            clipboard_files_enabled: false,
                        }
                        .encode(),
                    )
                    .unwrap();
                writer.flush().unwrap();
            });
            let stream = TcpStream::connect(address).unwrap();
            let result = negotiate_session(
                false,
                false,
                false,
                false,
                false,
                false,
                InputMode::X11Keycodes,
                false,
                Default::default(),
                false,
                false,
                false,
                stream,
                &TEST_PSK,
                false,
                request_clipboard,
                0,
                ClipboardMode::All,
                ClipboardDirection::Both,
                &KeymapTable::empty(),
                None,
                false,
                Default::default(),
                FrameCodec::JpegTiles,
                Duration::from_secs(5),
            );
            server.join().unwrap();
            if request_clipboard {
                assert!(result.is_ok(), "explicit clipboard consent must still work");
            } else {
                assert!(
                    result.is_err(),
                    "an authenticated host cannot opt in local clipboard sharing"
                );
                let error = match result {
                    Err(error) => error.to_string(),
                    Ok(_) => unreachable!(),
                };
                assert!(error.contains("without client consent"), "{error}");
            }
        }
    }

    #[test]
    fn successful_negotiation_cancels_its_absolute_deadline() {
        const BUDGET: Duration = Duration::from_secs(2);
        let offered = super::super::messages::FrameLimits {
            max_width: 1920,
            max_height: 1080,
            max_pixels: 1_000_000,
        };
        let selected = super::super::messages::FrameLimits {
            max_width: 1280,
            max_height: 720,
            max_pixels: 500_000,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let _watchdog = TcpStreamDeadline::arm(&stream, Duration::from_secs(6)).unwrap();
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x33; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            let requested = ClientHello::decode(&payload).unwrap();
            assert_eq!(requested.stream.frame_limits, offered);
            assert!(requested.stream.window_resize);
            assert_eq!(requested.initial_viewport, Some((1280, 720)));
            assert_eq!(
                requested.stream.cursor,
                super::super::messages::CursorMode::Independent
            );
            writer
                .write_message(
                    MessageKind::HelloAck,
                    &ServerHello {
                        stream: super::super::messages::StreamCapabilities {
                            frame_limits: selected,
                            window_resize: true,
                            ..Default::default()
                        },
                        pointer_enabled: false,
                        keyboard_enabled: false,
                        clipboard_enabled: false,
                        clipboard_files_enabled: false,
                    }
                    .encode(),
                )
                .unwrap();
            writer.flush().unwrap();
            thread::sleep(BUDGET + Duration::from_millis(200));
            writer.write_message(MessageKind::Close, &[]).unwrap();
            writer.flush().unwrap();
        });

        let stream = TcpStream::connect(address).unwrap();
        let started = Instant::now();
        let (mut reader, _writer, hello, _, _) = negotiate_session(
            true,
            false,
            false,
            false,
            false,
            false,
            InputMode::X11Keycodes,
            false,
            Default::default(),
            false,
            false,
            false,
            stream,
            &TEST_PSK,
            false,
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            Some((1280, 720)),
            true,
            offered,
            FrameCodec::JpegTiles,
            BUDGET,
        )
        .unwrap();
        assert_eq!(hello.stream.frame_limits, selected);
        assert!(hello.stream.window_resize);
        assert!(!hello.pointer_enabled);
        assert!(!hello.keyboard_enabled);
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let (kind, payload) = reader.read_message().unwrap();
        assert!(
            started.elapsed() > BUDGET,
            "connection must survive its original phase deadline"
        );
        assert_eq!(kind, MessageKind::Close);
        assert!(payload.is_empty());
        server.join().unwrap();
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    fn certificate() -> RemoteError {
        Box::new(CleanTransportDisconnect(ProtocolError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "private peer",
        ))))
    }
    fn session(cause: RemoteError) -> RemoteError {
        Box::new(ClientFailure {
            stage: ClientStage::Session,
            cause,
        })
    }

    #[test]
    fn certificate_requires_exact_retry_shape() {
        let direct = session(certificate());
        let summary = classify_failure(direct.as_ref());
        assert_eq!(summary.stage, Some("session"));
        assert_eq!(summary.category, "transport_loss");
        assert_eq!(summary.io_kind, Some("unexpected_eof"));
        assert_eq!(summary.cleanup, "complete");
        assert!(summary.reconnect_certified);
        for error in [
            certificate(),
            session(session(certificate())),
            Box::new(io::Error::other(session(certificate()))) as RemoteError,
        ] {
            let summary = classify_failure(error.as_ref());
            assert!(!summary.reconnect_certified);
            assert_eq!(summary.cleanup, "unknown");
        }
    }

    #[test]
    fn cleanup_failure_overrides_nested_certificate() {
        let error = session(Box::new(SessionCleanupError {
            #[cfg(test)]
            sender_transport_only: false,
            primary: Some(session(certificate())),
            failures: vec![io::Error::other("private cleanup path").into()],
        }));
        let summary = classify_failure(error.as_ref());
        assert_eq!(summary.cleanup, "incomplete");
        assert_eq!(summary.category, "cleanup_failed");
        assert!(!summary.reconnect_certified);
        let wrapped = io::Error::other(error);
        assert_eq!(classify_failure(&wrapped).cleanup, "incomplete");
    }

    #[test]
    fn markers_and_ordinary_io_are_distinct() {
        let receiver =
            ReceiverTransportLoss(ProtocolError::Io(io::ErrorKind::UnexpectedEof.into()));
        let summary = classify_failure(&receiver);
        assert_eq!(summary.category, "transport_loss");
        assert_eq!(summary.cleanup, "unknown");
        assert!(!summary.reconnect_certified);
        assert_eq!(
            classify_failure(&io::Error::from(io::ErrorKind::UnexpectedEof)).category,
            "io"
        );
        assert_eq!(
            classify_failure(&PeerClosed::legacy(b"PRIVATE\npeer")).category,
            "peer_closed"
        );
        assert_eq!(classify_failure(&UserClosed).category, "user_closed");
        assert_eq!(
            classify_failure(&ProtocolError::AuthenticationFailed).category,
            "authentication_failed"
        );
    }

    #[cfg(feature = "remote-wayland")]
    #[test]
    fn wayland_presentation_timeouts_have_a_static_terminal_category() {
        use super::super::wayland_viewer::PresentationTimeout;

        for timeout in [PresentationTimeout::Current, PresentationTimeout::Obsolete] {
            let error = session(Box::new(timeout));
            let summary = classify_failure(error.as_ref());
            assert_eq!(summary.stage, Some("session"));
            assert_eq!(summary.category, "wayland_presentation_timeout");
            assert_eq!(summary.cleanup, "unknown");
            assert!(!summary.reconnect_certified);
        }
    }

    #[test]
    fn classifier_never_formats_unknown_errors_and_bounds_cycles() {
        #[derive(Debug)]
        struct Unformattable;
        impl std::fmt::Display for Unformattable {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("diagnostics must not format errors");
            }
        }
        impl std::error::Error for Unformattable {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self)
            }
        }
        let summary = classify_failure(&Unformattable);
        assert_eq!(summary.category, "unknown");
        assert_eq!(summary.cleanup, "unknown");
    }
    #[test]
    fn protocol_variants_have_precise_static_categories() {
        for (error, category) in [
            (
                ProtocolError::InvalidPskLength {
                    expected: 32,
                    actual: 999,
                },
                "invalid_psk_length",
            ),
            (ProtocolError::AuthenticationFailed, "authentication_failed"),
            (
                ProtocolError::UnknownMessageKind(255),
                "unknown_message_kind",
            ),
            (
                ProtocolError::PayloadTooLarge { len: 999, max: 1 },
                "payload_too_large",
            ),
            (
                ProtocolError::SequenceMismatch {
                    expected: 1,
                    received: 999,
                },
                "sequence_mismatch",
            ),
            (ProtocolError::SequenceExhausted, "sequence_exhausted"),
            (ProtocolError::MalformedPadding, "malformed_padding"),
            (
                ProtocolError::UnsupportedTransportVersion {
                    received: 999,
                    expected: super::super::protocol::TRANSPORT_VERSION,
                },
                "unsupported_transport_version",
            ),
            (ProtocolError::BadTransportMagic, "bad_transport_magic"),
        ] {
            let summary = classify_failure(&error);
            assert_eq!(summary.category, category);
            assert_eq!(summary.io_kind, None);
            assert_eq!(summary.cleanup, "unknown");
            assert!(!summary.reconnect_certified);
            assert!(!serde_json::to_string(&summary).unwrap().contains("999"));
        }
    }
    #[test]
    fn clipboard_restriction_pending_or_confirmed_prevents_broader_reconnect() {
        let state = ReceiveState::new();
        *state.clipboard_restriction.lock().unwrap() = Some(RestrictionState::new(7));
        assert!(!clipboard_was_restricted(&state, 7));
        let request = state
            .clipboard_restriction
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_next(Instant::now())
            .unwrap()
            .unwrap();
        assert!(clipboard_was_restricted(&state, 7));
        let loss = || -> RemoteResult<()> {
            Err(Box::new(ReceiverTransportLoss(ProtocolError::Io(
                io::Error::new(io::ErrorKind::UnexpectedEof, "lost"),
            ))))
        };
        assert!(
            !suppress_reconnect(loss(), clipboard_was_restricted(&state, 7))
                .unwrap_err()
                .is::<ReceiverTransportLoss>()
        );
        {
            let mut restriction = state.clipboard_restriction.lock().unwrap();
            let restriction = restriction.as_mut().unwrap();
            restriction.local_complete(request).unwrap();
            restriction.acknowledge(request).unwrap();
            assert_eq!(restriction.take_confirmed(), Some(request));
        }
        assert!(clipboard_was_restricted(&state, 7));
    }

    #[test]
    fn managed_clipboard_policy_drops_stale_progress_and_clears_on_close_or_retry() {
        use super::super::engine::{ClipboardProgress, TransferKind};

        let (toggle, control) = ConnectToggle::local();
        let (_wake, send) = wake_pair().unwrap();
        assert!(toggle.begin_viewing(&send).unwrap());
        let mut snapshot = ClipboardSnapshot::negotiated(7, 1, 1024);
        let files = ClipboardProgress {
            direction: ClipboardDirection::ToHost,
            kind: TransferKind::Files,
            bytes_processed: 512,
            total_bytes: 1024,
        };
        assert!(snapshot.update_transfer(Some(files)));
        toggle.publish_clipboard(Some(snapshot));
        toggle.invalidate_sources();
        assert_eq!(control.snapshot().clipboard, Some(snapshot));
        snapshot.narrow(3);
        assert_eq!(snapshot.mode, Some(ClipboardMode::TextImages));
        assert_eq!(snapshot.direction, Some(ClipboardDirection::ToHost));
        assert_eq!(snapshot.file_limit_bytes, 0);
        assert_eq!(snapshot.transfer, None);
        assert!(!snapshot.update_transfer(Some(files)));
        snapshot.narrow(7);
        assert_eq!(snapshot.mode, Some(ClipboardMode::TextImages));
        let incoming = ClipboardProgress {
            direction: ClipboardDirection::FromHost,
            kind: TransferKind::Image,
            ..files
        };
        assert!(!snapshot.update_transfer(Some(incoming)));
        toggle.publish_clipboard(Some(snapshot));
        assert!(toggle.begin_backoff());
        assert_eq!(control.snapshot().clipboard, None);
        assert!(toggle.begin_attempt());
        assert!(toggle.begin_viewing(&send).unwrap());
        snapshot.narrow(0);
        assert_eq!(snapshot, ClipboardSnapshot::default());
        toggle.publish_clipboard(Some(snapshot));
        assert_eq!(control.snapshot().clipboard, Some(snapshot));
        control.close();
        toggle.publish_clipboard(Some(snapshot));
        assert_eq!(control.snapshot().clipboard, None);
    }
}

#[cfg(test)]
mod explicit_resize_tests {
    use super::*;
    use crate::remote::window_resize::{ResizeResult, ResizeStatus};
    #[test]
    fn explicit_resize_once_scoped_to_presentation_and_timeout() {
        let now = Instant::now();
        let mut state = ClientResize {
            follow: None,
            requested: Some(super::super::window_size::WindowSize {
                width: 480,
                height: 320,
            }),
            pending: None,
        };
        let req = state.after_presentation((3, 7), now).unwrap();
        assert_eq!((req.source_epoch, req.geometry_epoch), (3, 7));
        assert!(state.after_presentation((3, 8), now).is_none());
        assert!(state.check_timeout(now + Duration::from_secs(5)).is_err());
        let mut result = ResizeResult {
            request_id: req.request_id,
            source_epoch: 4,
            geometry_epoch: 7,
            observed_geometry_epoch: 8,
            width: 480,
            height: 320,
            status: ResizeStatus::Applied,
        };
        assert!(state.receive(result).is_err());
        assert!(state.pending.is_some());
        result.source_epoch = 3;
        state.receive(result).unwrap();
        assert!(state.pending.is_none());
        assert!(state.receive(result).is_err());
    }
}

#[cfg(test)]
mod native_viewer_permission_tests {
    use super::*;
    #[test]
    fn native_wayland_viewer_permissions_are_checked_before_any_io() {
        let mut options = ClientOptions {
            stats_json: false,
            control_duration: Default::default(),
            viewer_backend: crate::remote::viewer_backend::ViewerBackend::Wayland,
            viewer_wayland_display: None,
            wayland_keyboard: false,
            wayland_pointer: false,
            wayland_control: false,
            wayland_gestures: false,
            resumable_control: false,
            wayland_seamless_input: false,
            wayland_popup_pointer: false,
            wayland_text: None,
            static_lossless: false,
            wayland_scroll: false,
            wayland_horizontal_scroll: false,
            resize_source: None,
            follow_source_size: false,
            window_position: None,
            x11_parent_window: None,
            window_size: None,
            frame_limits: super::super::messages::FrameLimits::default(),
            address: "invalid address".into(),
            peer_label: "test".into(),
            key: KeySource::File(std::path::PathBuf::from(
                "/nonexistent/window-size-test.key",
            )),
            display: None,
            view_only: true,
            grab_input: false,
            seamless_input: None,
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "F12".into(),
            scaling: super::super::scaling::ScalingPolicy::Auto,
        };
        assert_eq!(
            validate_viewer_backend(&options).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        // No display, socket or key file may be opened for an unsupported
        // seamless policy, including callers of the concrete viewer factory.
        for (backend, grab, view_only) in [
            (ViewerBackend::Wayland, true, false),
            (ViewerBackend::X11, false, false),
            (ViewerBackend::X11, true, true),
        ] {
            let mut seamless = options.clone();
            seamless.viewer_backend = backend;
            seamless.grab_input = grab;
            seamless.view_only = view_only;
            seamless.seamless_input = Some(SeamlessInput::Hover);
            let expected = "seamless input requires X11, --grab-input and input control";
            assert_eq!(
                validate_viewer_backend(&seamless).unwrap_err().to_string(),
                expected
            );
            let error = super::super::viewer_backend::Viewer::connect(
                &seamless, 800, 600, true, true, None,
            )
            .err()
            .expect("unsupported seamless input must fail before display I/O");
            assert_eq!(error.to_string(), expected);
        }
        let mut native_seamless = options.clone();
        native_seamless.view_only = false;
        native_seamless.wayland_control = true;
        native_seamless.resumable_control = true;
        native_seamless.wayland_seamless_input = true;
        assert_eq!(
            validate_viewer_backend(&native_seamless).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in 0..7 {
            let mut invalid = native_seamless.clone();
            match mutation {
                0 => invalid.resumable_control = false,
                1 => invalid.wayland_control = false,
                2 => invalid.viewer_backend = ViewerBackend::X11,
                3 => invalid.view_only = true,
                4 => invalid.clipboard = true,
                5 => invalid.grab_input = true,
                _ => invalid.static_lossless = true,
            }
            assert!(
                validate_viewer_backend(&invalid)
                    .unwrap_err()
                    .to_string()
                    .starts_with("native seamless input requires")
            );
        }
        assert_eq!(
            run_client_reconnecting(native_seamless, 1)
                .unwrap_err()
                .to_string(),
            "automatic reconnect is unavailable for source resize, scoped input leases or the Wayland viewer"
        );
        let mut popup = options.clone();
        popup.view_only = false;
        popup.wayland_popup_pointer = true;
        assert_eq!(
            validate_viewer_backend(&popup).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in 0..12 {
            let mut changed = popup.clone();
            match mutation {
                0 => changed.view_only = true,
                1 => changed.wayland_keyboard = true,
                2 => changed.wayland_pointer = true,
                3 => changed.wayland_control = true,
                4 => changed.wayland_scroll = true,
                5 => changed.wayland_horizontal_scroll = true,
                6 => changed.static_lossless = true,
                7 => changed.clipboard = true,
                8 => changed.grab_input = true,
                9 => changed.follow_source_size = true,
                10 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 32,
                        height: 24,
                    })
                }
                _ => changed.viewer_backend = ViewerBackend::X11,
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        for mutation in 0..8 {
            let mut changed = options.clone();
            match mutation {
                0 => changed.view_only = false,
                1 => changed.wayland_keyboard = true,
                2 => changed.wayland_pointer = true,
                3 => changed.clipboard = true,
                4 => changed.grab_input = true,
                5 => changed.display = Some(":not-to-be-opened".into()),
                6 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
                _ => {
                    changed.window_position =
                        Some(super::super::window_position::WindowPosition { x: 0, y: 0 })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        let mut bundle = options.clone();
        bundle.view_only = false;
        bundle.wayland_control = true;
        assert_eq!(
            validate_viewer_backend(&bundle).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        let mut scrolling_bundle = bundle.clone();
        scrolling_bundle.wayland_scroll = true;
        assert_eq!(
            validate_viewer_backend(&scrolling_bundle).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in (0..11).filter(|v| *v != 4) {
            let mut changed = bundle.clone();
            match mutation {
                0 => changed.viewer_backend = ViewerBackend::X11,
                1 => changed.view_only = true,
                2 => changed.wayland_keyboard = true,
                3 => changed.wayland_pointer = true,
                5 => changed.clipboard = true,
                6 => changed.grab_input = true,
                7 => changed.display = Some(":never-open".into()),
                8 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
                9 => changed.follow_source_size = true,
                _ => {
                    changed.window_position =
                        Some(super::super::window_position::WindowPosition { x: 0, y: 0 })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        assert!(run_client_reconnecting(bundle, 1).is_err());
        let mut bundle = options.clone();
        bundle.view_only = false;
        bundle.wayland_control = true;
        assert_eq!(
            validate_viewer_backend(&bundle).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        let mut scrolling_bundle = bundle.clone();
        scrolling_bundle.wayland_scroll = true;
        assert_eq!(
            validate_viewer_backend(&scrolling_bundle).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in (0..11).filter(|v| *v != 4) {
            let mut changed = bundle.clone();
            match mutation {
                0 => changed.viewer_backend = ViewerBackend::X11,
                1 => changed.view_only = true,
                2 => changed.wayland_keyboard = true,
                3 => changed.wayland_pointer = true,
                5 => changed.clipboard = true,
                6 => changed.grab_input = true,
                7 => changed.display = Some(":never-open".into()),
                8 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
                9 => changed.follow_source_size = true,
                _ => {
                    changed.window_position =
                        Some(super::super::window_position::WindowPosition { x: 0, y: 0 })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        assert!(run_client_reconnecting(bundle, 1).is_err());
        // Native keyboard is a distinct scoped lease, not legacy input.
        let mut keyboard = options.clone();
        keyboard.view_only = false;
        keyboard.wayland_keyboard = true;
        assert_eq!(
            validate_viewer_backend(&keyboard).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in 0..10 {
            let mut changed = keyboard.clone();
            match mutation {
                0 => changed.view_only = true,
                1 => changed.wayland_keyboard = false,
                2 => changed.wayland_pointer = true,
                3 => changed.wayland_scroll = true,
                4 => changed.clipboard = true,
                5 => changed.grab_input = true,
                6 => changed.display = Some(":never-open".into()),
                7 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
                8 => changed.follow_source_size = true,
                _ => {
                    changed.window_position =
                        Some(super::super::window_position::WindowPosition { x: 0, y: 0 })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        assert!(run_client_reconnecting(keyboard, 1).is_err());
        let mut pointer = options.clone();
        pointer.view_only = false;
        pointer.wayland_pointer = true;
        assert_eq!(
            validate_viewer_backend(&pointer).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        for mutation in 0..10 {
            let mut changed = pointer.clone();
            match mutation {
                0 => changed.view_only = true,
                1 => changed.wayland_pointer = false,
                2 => changed.wayland_keyboard = true,
                3 => {
                    changed.wayland_scroll = true;
                    changed.wayland_pointer = false;
                }
                4 => changed.clipboard = true,
                5 => changed.grab_input = true,
                6 => changed.display = Some(":never-open".into()),
                7 => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
                8 => changed.follow_source_size = true,
                _ => {
                    changed.window_position =
                        Some(super::super::window_position::WindowPosition { x: 0, y: 0 })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
        pointer.wayland_scroll = true;
        assert_eq!(
            validate_viewer_backend(&pointer).is_ok(),
            cfg!(feature = "remote-wayland")
        );
        assert!(run_client_reconnecting(pointer, 1).is_err());

        for mode in 0..4 {
            let mut restricted = options.clone();
            restricted.viewer_backend = ViewerBackend::X11;
            match mode {
                0 => restricted.viewer_backend = ViewerBackend::Wayland,
                1 => restricted.wayland_keyboard = true,
                2 => restricted.wayland_pointer = true,
                _ => {
                    restricted.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
            }
            let error = run_client_reconnecting(restricted, 1).unwrap_err();
            assert_eq!(
                error.to_string(),
                "automatic reconnect is unavailable for source resize, scoped input leases or the Wayland viewer"
            );
        }
        options.viewer_backend = ViewerBackend::X11;
        options.viewer_wayland_display = Some("wayland-test".into());
        assert!(validate_viewer_backend(&options).is_err());
        options.viewer_wayland_display = None;
        options.follow_source_size = true;
        assert!(validate_viewer_backend(&options).is_ok());
        assert!(run_client_reconnecting(options.clone(), 1).is_err());
        for mutation in 0..7 {
            let mut changed = options.clone();
            match mutation {
                0 => changed.viewer_backend = ViewerBackend::Wayland,
                1 => changed.view_only = false,
                2 => changed.clipboard = true,
                3 => changed.grab_input = true,
                4 => changed.wayland_keyboard = true,
                5 => changed.wayland_pointer = true,
                _ => {
                    changed.resize_source = Some(super::super::window_size::WindowSize {
                        width: 10,
                        height: 10,
                    })
                }
            }
            assert!(validate_viewer_backend(&changed).is_err());
        }
    }
}

#[cfg(test)]
mod follow_viewport_policy_tests {
    use super::*;
    #[derive(Default)]
    struct Records(Vec<(MessageKind, Vec<u8>)>);
    impl RecordWriter for Records {
        fn write_message(&mut self, kind: MessageKind, payload: &[u8]) -> RemoteResult<()> {
            self.0.push((kind, payload.to_vec()));
            Ok(())
        }
        fn flush(&mut self) -> RemoteResult<()> {
            Ok(())
        }
    }
    #[test]
    fn follow_hello_and_entire_session_have_no_viewport_geometry_writer() {
        let mut follow = super::super::follow_source_size::FollowSourceSize::default();
        let policy = ViewportPolicy::for_follow(true);
        assert_eq!(policy.initial((400, 300)), None);
        let mut records = Records::default();
        let mut previous = None;
        for size in [(400, 300), (480, 320), (580, 400), (640, 400), (400, 300)] {
            follow.viewport(size);
            assert!(!policy.report(&mut records, &mut previous, size).unwrap());
        }
        follow.stop("resize Stale");
        for size in [(720, 460), (800, 600)] {
            follow.viewport(size);
            assert!(!policy.report(&mut records, &mut previous, size).unwrap());
        }
        assert!(records.0.is_empty());
        assert_eq!(previous, None);
    }
    #[test]
    fn ordinary_and_one_shot_sessions_keep_viewport_fit_updates() {
        // One-shot resize does not set the follow opt-in.
        let policy = ViewportPolicy::for_follow(false);
        assert_eq!(policy.initial((400, 300)), Some((400, 300)));
        let mut records = Records::default();
        let mut previous = None;
        assert!(
            policy
                .report(&mut records, &mut previous, (400, 300))
                .unwrap()
        );
        assert!(
            !policy
                .report(&mut records, &mut previous, (400, 300))
                .unwrap()
        );
        assert!(
            policy
                .report(&mut records, &mut previous, (640, 400))
                .unwrap()
        );
        assert_eq!(
            records.0,
            vec![
                (MessageKind::Viewport, encode_viewport(400, 300).to_vec()),
                (MessageKind::Viewport, encode_viewport(640, 400).to_vec())
            ]
        );
    }
}

#[cfg(test)]
#[path = "scroll_copy_transport_tests.rs"]
mod scroll_copy_transport_tests;

#[cfg(test)]
mod popup_hello_policy_tests {
    use super::*;

    #[test]
    fn popup_session_label_precedes_legacy_mode_without_claiming_authorization() {
        for pointer in [false, true] {
            for keyboard in [false, true] {
                assert_eq!(
                    negotiated_session_label(true, pointer, keyboard),
                    "popup pointer negotiated; awaiting presented-frame authorization"
                );
                assert_eq!(
                    negotiated_session_label(false, pointer, keyboard),
                    session_mode(pointer, keyboard)
                );
            }
        }
    }

    #[test]
    fn popup_production_frame_offer_roundtrips_without_legacy_codec_or_clipboard() {
        for popup in [false, true] {
            let mut hello = ClientHello {
                stream: super::super::messages::StreamCapabilities {
                    wayland_popup_pointer: popup,
                    ..requested_frame_stream(popup, Default::default(), FrameCodec::JpegTiles)
                },
                initial_viewport: Some((320, 240)),
                request_input: false,
                request_clipboard: false,
                request_clipboard_files: false,
                keymap: KeymapTable::empty(),
            };
            let decoded = ClientHello::decode(&hello.encode()).unwrap();
            assert_eq!(decoded.stream.wayland_popup_pointer, popup);
            assert_eq!(decoded.stream.scroll_copy, !popup);
            assert_eq!(decoded.stream.clipboard_content, 0);
            assert_eq!(decoded.stream.clipboard_directions, 0);
            assert!(!decoded.request_input && !decoded.request_clipboard);
            if popup {
                // The previous production offer failed exactly this check.
                hello.stream.scroll_copy = true;
                assert!(ClientHello::decode(&hello.encode()).is_err());
                hello.stream.scroll_copy = false;
                hello.stream.clipboard_content = 1;
                hello.stream.clipboard_directions = 1;
                hello.request_clipboard = true;
                assert!(ClientHello::decode(&hello.encode()).is_err());
            }
        }
    }
}
