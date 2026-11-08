//! Remote desktop viewer/client orchestration.

use super::clipboard_policy::{self, ClipboardDirection, ClipboardMode};
use super::clipboard_sync::{
    ClipboardInbox, ClipboardOutbox, ClipboardSync, Outgoing, SyncOptions, SyncStatus,
};
use super::close_wire::{SessionClose, SessionCloseCode};
use super::deadline::TcpStreamDeadline;
use super::frame::{RecyclableDecodedFrame, SharedDecodeBufferPool, new_decode_buffer_pool};
use super::key::KeySource;
use super::messages::{
    ClientHello, MAX_INPUT_BATCH_EVENTS, ServerHello, encode_input_batch_into,
    encode_source_frame_ack, encode_viewport,
};
use super::protocol::{
    MAX_PAYLOAD_LEN, MessageKind, PayloadBufferRetention, ProtocolError, SessionReader,
    SessionWriter, client_handshake,
};
use super::sender::{RecordWriter, Sender, reap_worker};
use super::source_wire::{
    SourceCatalog, SourceRequest, decode_source_changed, decode_source_payload,
};
use super::tile::TileDecoder;
use super::usability::{ClientStage, failure_hint, session_mode};
use super::x11_input::InputEvent;
use super::x11_keymap::{KeymapTable, read_display};
use super::x11_viewer::{Viewer, ViewerEvent, validate_escape_key};
use super::{RemoteError, RemoteResult, VIDEO_FRAME_IDLE_TIMEOUT};
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::io::Write as _;
use std::io::{self, Read as _};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
    /// Explicit local client-area startup size; None preserves automatic sizing.
    pub window_size: Option<super::window_size::WindowSize>,
    /// Explicit local best-effort startup position; coordinates are bounded by i16.
    pub window_position: Option<super::window_position::WindowPosition>,
    pub frame_limits: super::messages::FrameLimits,
    pub address: String,
    /// What to call the host in messages and the window title: the address
    /// as typed, or the SSH destination when the address is a local tunnel.
    pub peer_label: String,
    pub key: KeySource,
    pub display: Option<String>,
    pub view_only: bool,
    pub grab_input: bool,
    pub clipboard: bool,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_direction: ClipboardDirection,
    /// Largest file copy shared through the clipboard; 0 shares no files.
    pub clipboard_file_limit: u64,
    /// Base X keysym of the Ctrl+Super local grab-release shortcut.
    pub escape_key: String,
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
    };
    let mut current = Some(error);
    for _ in 0..32 {
        let Some(error) = current else { break };
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
                result.category = "peer_closed";
            }
        } else if error.is::<UserClosed>() {
            if result.category == "unknown" {
                result.category = "user_closed";
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
    let mut stage = ClientStage::Setup;
    let result = run_client_session(options, &mut stage);
    match result {
        Ok(()) => {
            eprintln!("jwm-remote: session ended");
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
    if attempts > 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconnect attempts must be 0..=5",
        )
        .into());
    }
    reconnect_with(
        attempts,
        || run_client(options.clone()),
        |attempt, delay| {
            eprintln!(
                "jwm-remote: transport lost; reconnect attempt {attempt}/{attempts} in {} seconds (Ctrl+C to stop)",
                delay.as_secs()
            );
            thread::sleep(delay);
        },
    )
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
                Err(error) => error,
            }
        } else {
            error
        }
    })
}

fn run_client_session(options: ClientOptions, stage: &mut ClientStage) -> RemoteResult<()> {
    if let Some(size) = options.window_size {
        size.validate()?;
    }
    validate_escape_key(&options.escape_key)?;
    let key = options.key.load()?;
    let request_input = !options.view_only;
    let keymap = if request_input {
        read_display(options.display.as_deref())?
    } else {
        KeymapTable::empty()
    };
    let initial_viewport = super::x11_viewer::initial_viewport_with_position(
        options.display.as_deref(),
        options.window_size,
        options.window_position,
    )?;
    let request_cursor_plane =
        super::x11_viewer::supports_cursor_plane(options.display.as_deref())?;
    *stage = ClientStage::Connect;
    eprintln!("jwm-remote: connecting to {}", options.peer_label);
    let stream = connect_with_timeout(&options.address, CONNECT_TIMEOUT)?;
    *stage = ClientStage::Negotiate;
    eprintln!("jwm-remote: connection opened; authenticating and negotiating permissions");
    let (mut reader, writer, hello) = negotiate_session(
        stream,
        &key,
        request_input,
        options.clipboard,
        options.clipboard_file_limit,
        options.clipboard_mode,
        options.clipboard_direction,
        &keymap,
        Some(initial_viewport),
        request_cursor_plane,
        options.frame_limits,
        HANDSHAKE_TIMEOUT,
    )?;
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
    let (mut clipboard_sync, clipboard_inbox, clipboard_outbox) = if hello.clipboard_enabled {
        *stage = ClientStage::LocalClipboard;
        let clipboard_wake = wake_sender.try_clone()?;
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
                let _ = clipboard_wake.signal();
            })),
        ) {
            Ok((sync, inbox, outbox)) => {
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
    initial_state.frame_limits = hello.stream.frame_limits;
    let state = Arc::new(initial_state);
    state.cursor_enabled.store(
        hello.stream.cursor == super::messages::CursorMode::Independent,
        Ordering::Release,
    );
    let mut telemetry_reporter = ClientTelemetryReporter::new(&state.telemetry);
    let payload = read_initial_frame(
        &mut reader,
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
    )?;
    state.telemetry.record_received();
    // SessionReader exposes this payload only after its record MAC succeeds;
    // no decoded-image allocation is checked out before authentication.
    let mut tile_decoder = TileDecoder::with_frame_limits(hello.stream.frame_limits)?;
    let (initial_epoch, initial_payload) = decode_source_payload(&payload)?;
    if initial_epoch != 1 || state.catalog.lock().unwrap().is_none() {
        return Err(invalid_data("initial frame missing source catalog or initial epoch").into());
    }
    let mut first_frame = decode_queued_frame(
        &mut tile_decoder,
        initial_payload,
        &state.telemetry,
        &state.decode_pool,
    )?;
    first_frame.frame = first_frame.frame.with_source_epoch(initial_epoch);
    reader
        .get_ref()
        .set_read_timeout(Some(VIDEO_FRAME_IDLE_TIMEOUT))?;
    let initial_width = u16::try_from(first_frame.frame.image().width())
        .map_err(|_| invalid_data("initial frame is wider than an X11 window"))?;
    let initial_height = u16::try_from(first_frame.frame.image().height())
        .map_err(|_| invalid_data("initial frame is taller than an X11 window"))?;
    let last_sequence = first_frame.frame.sequence();
    *stage = ClientStage::Display;
    let mut viewer = Viewer::connect_with_window_geometry(
        options.display.as_deref(),
        initial_width,
        initial_height,
        options.window_size,
        options.window_position,
        hello.pointer_enabled,
        hello.keyboard_enabled,
        hello.keyboard_enabled.then_some(keymap),
        options.grab_input && (hello.pointer_enabled || hello.keyboard_enabled),
        &options.escape_key,
        &options.peer_label,
    )?;
    viewer.set_clipboard_status(hello.clipboard_enabled);
    viewer.set_clipboard_policy(clipboard_policy::label(
        hello.stream.clipboard_content,
        hello.stream.clipboard_directions,
    ));
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
        move |kind| {
            if kind == MessageKind::FrameAck {
                sender_state.telemetry.record_acked();
            }
        },
    )?;
    let mut pending_presentations = std::collections::VecDeque::new();
    if !submit_frame(&mut viewer, first_frame, &mut pending_presentations)? {
        return Ok(());
    }

    *stage = ClientStage::Session;
    eprintln!(
        "jwm-remote: connected; {}; clipboard {}",
        session_mode(
            hello.pointer_enabled && request_input,
            hello.keyboard_enabled && request_input
        ),
        clipboard_policy::label(
            hello.stream.clipboard_content,
            hello.stream.clipboard_directions
        )
    );
    if options.grab_input && (hello.pointer_enabled || hello.keyboard_enabled) {
        eprintln!(
            "jwm-remote: click the viewer to control; Ctrl+Super+{} to release input",
            options.escape_key
        );
    }

    let mut cancel = SessionCancel::new(control, Arc::clone(&state));
    let receive_state = Arc::clone(&state);
    let receiver = ReceiverThread::spawn(move || {
        // Move the authenticated first-frame allocation into the receiver
        // thread. It remains thread-local while avoiding a second large
        // allocation for the next frame.
        receive_frames(
            reader,
            receive_state,
            last_sequence,
            payload,
            wake_sender,
            tile_decoder,
            clipboard_inbox,
        );
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
        clipboard_outbox.as_ref(),
        clipboard_status.as_deref(),
        &mut pending_presentations,
    );
    let graceful_close = matches!(
        &loop_result,
        Ok(ViewerLoopExit {
            close_release_flushed: true
        })
    );
    let cancel_result = cancel.cancel(&mut writer, graceful_close);
    let receiver_exit = receiver.join_timeout(RECEIVER_JOIN_TIMEOUT);
    let sender_result = writer.shutdown_timeout(RECEIVER_JOIN_TIMEOUT);
    let clipboard_result = clipboard_sync
        .as_mut()
        .map_or(Ok(()), |sync| sync.shutdown_timeout(RECEIVER_JOIN_TIMEOUT));
    telemetry_reporter.force_report();
    let result = finish_session(
        suppress_reconnect(
            loop_result.map(|_| ()),
            viewer.user_close_requested() || viewer.user_released_grab(),
        ),
        cancel_result,
        receiver_exit,
        sender_result,
        clipboard_result,
    );
    finalize_retry_intent(result, || viewer.reconnect_allowed_after_cleanup())
}

/// The old window remains visible while workers join. Consume local intent
/// that arrived during that interval before returning a retry certificate.
fn finalize_retry_intent(
    result: RemoteResult<()>,
    check: impl FnOnce() -> RemoteResult<bool>,
) -> RemoteResult<()> {
    match result {
        Err(error) => match error.downcast::<CleanTransportDisconnect>() {
            Ok(transport) => match check() {
                Ok(true) => Err(transport),
                Ok(false) => Err(Box::new(transport.0)),
                Err(error) => Err(Box::new(SessionCleanupError {
                    primary: Some(transport),
                    failures: vec![error],
                })),
            },
            Err(error) => Err(error),
        },
        Ok(()) => Ok(()),
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

fn finish_session(
    foreground: RemoteResult<()>,
    cancel: RemoteResult<()>,
    receiver: ReceiverExit,
    sender: RemoteResult<()>,
    clipboard: RemoteResult<()>,
) -> RemoteResult<()> {
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
            primary: foreground.err(),
            failures,
        }))
    }
}

fn negotiate_session(
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
    negotiation_timeout: Duration,
) -> RemoteResult<(
    SessionReader<TcpStream>,
    SessionWriter<TcpStream>,
    ServerHello,
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
    let (receive_key, send_key) = session_keys.into_client();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    // The acknowledgement and a refusal are both tiny; frames come later.
    reader.set_max_payload_len(MAX_NEGOTIATION_PAYLOAD_LEN);
    let mut writer = SessionWriter::new(stream, send_key);
    writer.write_message(
        MessageKind::Hello,
        &ClientHello {
            stream: super::messages::StreamCapabilities {
                frame_limits,
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
                ..Default::default()
            },
            initial_viewport,
            request_input,
            request_clipboard,
            request_clipboard_files,
            keymap: keymap.clone(),
        }
        .encode(),
    )?;
    writer.flush()?;
    let hello = read_server_hello(&mut reader)?;
    validate_server_frame_limits(&hello, frame_limits)?;
    if hello.stream.cursor == super::messages::CursorMode::Independent && !request_cursor_plane {
        return Err(invalid_data("host enabled independent cursor without viewer support").into());
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
    Ok((reader, writer, hello))
}

/// Read only authenticated records, accepting explicit terminal responses before
/// HelloAck as well as after negotiation. The phase remains in ClientFailure.
fn read_server_hello<R: io::Read>(reader: &mut SessionReader<R>) -> RemoteResult<ServerHello> {
    let (kind, payload) = reader
        .read_message()
        .map_err(|error| explain_negotiation_eof(error, true))?;
    match kind {
        MessageKind::Close | MessageKind::SessionClose => {
            let mut close = PeerClosed::decode(kind, &payload)?;
            if close.reason.trim().is_empty() {
                close.reason = "host declined the session without a reason".to_owned();
            }
            Err(close.into())
        }
        MessageKind::HelloAck => ServerHello::decode(&payload),
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
    pending_presentations: &mut std::collections::VecDeque<PendingFrameTiming>,
) -> RemoteResult<ViewerLoopExit> {
    let mut clipboard_status_seen = 0;
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
    let mut input_batch = Vec::with_capacity(MAX_INPUT_BATCH_EVENTS);
    let mut input_payload = Vec::new();
    // Tell the host how much this window can actually show. Without it the
    // host encodes to a fixed width forever, so a small window receives detail
    // it immediately throws away.
    let mut reported_viewport = None;
    let mut source_epoch = 1;
    let mut presented_scope = (1, 1);
    let mut switching = false;
    let mut requested_epoch = None;
    let source_cycle_available = state
        .catalog
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|catalog| catalog.entries.len() > 1);
    loop {
        wake.drain()?;
        let received_epoch = state.source_epoch.load(Ordering::Acquire);
        if received_epoch != source_epoch {
            source_epoch = received_epoch;
            requested_epoch = None;
            switching = true;
            pending_presentations.clear();
            viewer.invalidate_source(current_source_label(state))?;
            write_source_release(writer, presented_scope, pointer_enabled || keyboard_enabled)?;
        }
        let mut events = viewer.poll_events().map_err(|error| {
            if viewer.user_close_requested() {
                user_close_cleanup_failure(error)
            } else {
                error
            }
        })?;
        if events.contains(&ViewerEvent::CycleSource)
            && !events.contains(&ViewerEvent::Close)
            && !switching
        {
            let next = state
                .catalog
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|catalog| next_source(catalog, source_epoch));
            if let Some((target, label)) = next {
                write_source_release(writer, presented_scope, pointer_enabled || keyboard_enabled)?;
                writer.write_message(
                    MessageKind::SourceRequest,
                    &SourceRequest {
                        expected_epoch: source_epoch,
                        target,
                    }
                    .encode()?,
                )?;
                pending_presentations.clear();
                viewer.invalidate_source(label)?;
                requested_epoch = Some(source_epoch);
                switching = true;
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
        if reported_viewport != Some(viewport) && !outcome.close {
            let (width, height) = viewport;
            writer.write_message(MessageKind::Viewport, &encode_viewport(width, height))?;
            reported_viewport = Some(viewport);
            wrote = true;
        }
        if let Some(clipboard) = clipboard {
            // Closing the viewer wins over a simultaneous local worker exit.
            wrote |= forward_viewer_clipboard_from(|| clipboard.try_next(), writer, outcome.close)?;
        }
        if let Some((version, line)) =
            clipboard_status.and_then(|status| status.changed_since(clipboard_status_seen))
        {
            clipboard_status_seen = version;
            viewer.set_clipboard_activity(line);
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
            if let Some(timing) = pending_presentations
                .iter()
                .find(|timing| timing.sequence == sequence)
            {
                if timing.source_epoch != source_epoch {
                    continue;
                }
                presented_scope = (timing.source_epoch, timing.geometry_epoch);
            }
            acknowledge_presentation(writer, sequence, pending_presentations, &state.telemetry)?;
            viewer.source_presented(presented_scope.1);
            viewer.enable_source_cycle(source_cycle_available);
            switching = false;
        }
        if viewer.can_submit_frame() {
            if let Some(frame) = take_latest(state, source_epoch) {
                if frame.frame.source_epoch() == source_epoch
                    && requested_epoch != Some(source_epoch)
                {
                    if viewer.prepare_frame_geometry(frame.frame.geometry_epoch())? {
                        write_source_release(
                            writer,
                            presented_scope,
                            pointer_enabled || keyboard_enabled,
                        )?;
                    }
                    submit_frame(viewer, frame, pending_presentations)?;
                }
            } else {
                viewer.refresh_pending_redraw()?;
            }
        }

        // A source barrier may race the loop-top epoch snapshot. Never wait
        // after leaving its first keyframe/cursor queued for the next pass.
        if state.source_epoch.load(Ordering::Acquire) != source_epoch {
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
            ViewerEvent::CycleSource => continue,
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
    source_epoch: u64,
    geometry_epoch: u64,
    sequence: u64,
    ready_at: Instant,
    draw_started: Instant,
}

fn submit_frame(
    viewer: &mut Viewer,
    queued: QueuedFrame,
    pending: &mut std::collections::VecDeque<PendingFrameTiming>,
) -> RemoteResult<bool> {
    let timing = PendingFrameTiming {
        source_epoch: queued.frame.source_epoch(),
        geometry_epoch: queued.frame.geometry_epoch(),
        sequence: queued.frame.sequence(),
        ready_at: queued.ready_at,
        draw_started: Instant::now(),
    };
    if !viewer.draw_recyclable(queued.frame)? {
        return Ok(false);
    }
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
    telemetry.record_drawn_at(timing.ready_at, timing.draw_started, Instant::now());
    writer.write_message(
        MessageKind::FrameAck,
        &encode_source_frame_ack(timing.source_epoch, sequence)?,
    )?;
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
    let draw_result = draw(queued.frame);
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
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

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

    fn cancel(&mut self, writer: &mut impl RecordWriter, graceful_close: bool) -> RemoteResult<()> {
        if self.cancelled {
            return Ok(());
        }
        self.cancelled = true;
        self.state.stopping.store(true, Ordering::Release);

        let mut first_error = None;
        if graceful_close && !self.close_attempted {
            self.close_attempted = true;
            let close_result: RemoteResult<()> = (|| {
                writer.close()?;
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
        let handle = thread::Builder::new()
            .name("jwm-remote-video".into())
            .spawn(task)?;
        Ok(Self {
            handle: Some(handle),
        })
    }

    fn join_timeout(mut self, timeout: Duration) -> ReceiverExit {
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

struct ReceiveState {
    catalog: Mutex<Option<SourceCatalog>>,
    source_epoch: AtomicU64,
    frame_limits: super::messages::FrameLimits,
    cursor_enabled: AtomicBool,
    cursor: Mutex<super::cursor::CursorInbox>,
    latest: Mutex<Option<QueuedFrame>>,
    error: Mutex<Option<RemoteError>>,
    alive: AtomicBool,
    stopping: AtomicBool,
    telemetry: ClientTelemetry,
    decode_pool: SharedDecodeBufferPool,
}

impl ReceiveState {
    fn new() -> Self {
        Self {
            catalog: Mutex::new(None),
            source_epoch: AtomicU64::new(1),
            frame_limits: super::messages::FrameLimits::default(),
            cursor_enabled: AtomicBool::new(false),
            cursor: Mutex::new(super::cursor::CursorInbox::default()),
            latest: Mutex::new(None),
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
    if !accept_source_epoch(epoch, state.source_epoch.load(Ordering::Acquire))? {
        return Ok(());
    }
    let mut inbox = state.cursor.lock().unwrap();
    match kind {
        MessageKind::CursorShape => inbox.shape(super::cursor::CursorShape::decode(payload)?),
        MessageKind::CursorPosition => {
            inbox.position(super::cursor::CursorPosition::decode(payload)?)
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
    let mut latest = state.latest.lock().unwrap();
    if latest
        .as_ref()
        .is_some_and(|queued| queued.frame.source_epoch() > epoch)
    {
        return None;
    }
    latest
        .take()
        .filter(|queued| queued.frame.source_epoch() == epoch)
}

fn replace_latest<T>(latest: &Mutex<Option<T>>, next: T) -> Option<T> {
    let mut latest = latest.lock().unwrap();
    latest.replace(next)
}

fn decode_queued_frame(
    decoder: &mut TileDecoder,
    payload: &[u8],
    telemetry: &ClientTelemetry,
    pool: &SharedDecodeBufferPool,
) -> RemoteResult<QueuedFrame> {
    let decode_started = Instant::now();
    // Deltas are applied in arrival order into the decoder's own canvas, so a
    // frame the viewer later drops from its one-slot queue still contributes
    // its tiles. Only presentation is allowed to skip frames, never decoding.
    let frame = decoder.decode_into(payload, Arc::clone(pool))?;
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
    tile_decoder: TileDecoder,
    clipboard: Option<ClipboardInbox>,
) {
    receive_frame_records(
        |payload, remaining| {
            reader.get_ref().set_read_timeout(Some(remaining))?;
            reader
                .read_message_into(payload)
                .map_err(receiver_read_error)
        },
        state,
        last_sequence,
        payload,
        wake,
        tile_decoder,
        clipboard,
    );
}

/// Record handling is shared by TCP and authenticated in-memory fixtures.
fn receive_frame_records(
    mut read_record: impl FnMut(&mut Vec<u8>, Duration) -> RemoteResult<MessageKind>,
    state: Arc<ReceiveState>,
    mut last_sequence: u64,
    mut payload: Vec<u8>,
    wake: WakeSender,
    mut tile_decoder: TileDecoder,
    clipboard: Option<ClipboardInbox>,
) {
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
        let kind = read_record(&mut payload, remaining)?;
        match kind {
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
                tile_decoder = TileDecoder::with_frame_limits(state.frame_limits)?;
                *state.latest.lock().unwrap() = None;
                *state.cursor.lock().unwrap() = super::cursor::CursorInbox::default();
                state.source_epoch.store(changed.epoch, Ordering::Release);
                completion.signal()?;
            }
            MessageKind::Frame => {
                let (epoch, inner) = decode_source_payload(&payload)?;
                if !accept_source_epoch(epoch, state.source_epoch.load(Ordering::Acquire))? {
                    continue;
                }
                // read_message_into authenticated the complete record before
                // returning Frame, so pool checkout starts only on trusted
                // header/JPEG bytes.
                state.telemetry.record_received();
                let mut queued = decode_queued_frame(
                    &mut tile_decoder,
                    inner,
                    &state.telemetry,
                    &state.decode_pool,
                )?;
                queued.frame = queued.frame.with_source_epoch(epoch);
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
                last_video_at = Instant::now();
                let replaced = replace_latest(&state.latest, queued);
                if replaced.is_some() {
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
            | MessageKind::SourceRequest => {
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

/// Connect with an explicit bound on the TCP handshake.
///
/// Every later phase of a session is absolutely bounded, but a bare
/// `TcpStream::connect` is not: a black-holed address burns the kernel's whole
/// SYN retry budget (~130 s on Linux) against a documented 5 s negotiation
/// budget. Each resolved address gets its own attempt so a stale AAAA record
/// cannot consume the entire allowance.
fn connect_with_timeout(address: &str, timeout: Duration) -> RemoteResult<TcpStream> {
    let resolved = address
        .to_socket_addrs()
        .map_err(|error| invalid_data(format!("cannot resolve {address}: {error}")))?
        .collect::<Vec<_>>();
    let Some((last, rest)) = resolved.split_last() else {
        return Err(invalid_data(format!("{address} resolved to no addresses")).into());
    };
    let per_address = timeout
        .checked_div(resolved.len() as u32)
        .unwrap_or(timeout)
        .max(Duration::from_secs(1));
    for candidate in rest {
        if let Ok(stream) = TcpStream::connect_timeout(candidate, per_address) {
            return Ok(stream);
        }
    }
    TcpStream::connect_timeout(last, per_address).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot reach {address} within {timeout:?}: {error}"),
        )
        .into()
    })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
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
        let mut reader = SessionReader::new(Cursor::new(writer.into_inner()), receive_key);
        let cause = match phase {
            ClientStage::Negotiate => {
                reader.set_max_payload_len(MAX_NEGOTIATION_PAYLOAD_LEN);
                read_server_hello(&mut reader).unwrap_err()
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
            window_position: None,
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
            clipboard: false,
            clipboard_mode: Default::default(),
            clipboard_direction: Default::default(),
            clipboard_file_limit: 0,
            escape_key: "F12".into(),
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
        queued.frame = queued.frame.with_source_epoch(2);
        *state.latest.lock().unwrap() = Some(queued);
        assert!(take_latest(&state, 1).is_none());
        assert!(state.latest.lock().unwrap().is_some());
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
            body.find("let loop_result = viewer_loop(").unwrap()
                < body.find("let cancel_result = cancel.cancel(").unwrap()
        );
        assert!(
            body.find("let cancel_result = cancel.cancel(").unwrap()
                < body
                    .find("let receiver_exit = receiver.join_timeout(")
                    .unwrap()
        );
        assert!(
            body.find("let receiver_exit = receiver.join_timeout(")
                .unwrap()
                < body.find("finish_session(").unwrap()
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
                source_epoch: 1,
                geometry_epoch: 0,
                sequence,
                ready_at: now,
                draw_started: now,
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
        let telemetry = ClientTelemetry::default();
        let now = Instant::now();
        let mut pending = std::collections::VecDeque::from([PendingFrameTiming {
            source_epoch: 7,
            geometry_epoch: 0,
            sequence: 42,
            ready_at: now,
            draw_started: now,
        }]);
        let mut writer = SessionWriter::new(Vec::new(), [0x31; 32]);
        acknowledge_presentation(&mut writer, 42, &mut pending, &telemetry).unwrap();
        assert!(pending.is_empty());
        assert_eq!(telemetry.window.lock().unwrap().drawn, 1);
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
            decode_queued_frame(
                &mut TileDecoder::new(),
                b"not a frame",
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
                &crate::remote::source_wire::encode_source_payload(1, &payload).unwrap(),
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
            TileDecoder::new(),
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
                &crate::remote::source_wire::encode_source_payload(1, &payload).unwrap(),
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
            TileDecoder::new(),
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
    fn receiver_eof_with_idle_sender_has_clean_teardown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let control = socket.try_clone().unwrap();
        let mut reader = SessionReader::new(socket.try_clone().unwrap(), TEST_PSK);
        let mut sender =
            Sender::spawn(SessionWriter::new(socket, TEST_PSK), false, |_| {}).unwrap();
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
        let result = negotiate_session(
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
            Duration::from_millis(80),
        );
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
                        stream: Default::default(),
                        pointer_enabled: false,
                        keyboard_enabled: false,
                        clipboard_enabled: false,
                        clipboard_files_enabled: false,
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
            false,
            0,
            ClipboardMode::All,
            ClipboardDirection::Both,
            &KeymapTable::empty(),
            None,
            false,
            Default::default(),
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
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
            let keys = server_handshake(&mut stream, &TEST_PSK, [0x33; 32]).unwrap();
            let reader_stream = stream.try_clone().unwrap();
            let (receive_key, send_key) = keys.into_server();
            let mut reader = SessionReader::new(reader_stream, receive_key);
            let mut writer = SessionWriter::new(stream, send_key);
            let (kind, payload) = reader.read_message().unwrap();
            assert_eq!(kind, MessageKind::Hello);
            let requested = ClientHello::decode(&payload).unwrap();
            assert_eq!(requested.stream.frame_limits, offered);
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
            thread::sleep(Duration::from_millis(160));
            writer.write_message(MessageKind::Close, &[]).unwrap();
            writer.flush().unwrap();
        });

        let stream = TcpStream::connect(address).unwrap();
        let (mut reader, _writer, hello) = negotiate_session(
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
            Duration::from_millis(80),
        )
        .unwrap();
        assert_eq!(hello.stream.frame_limits, selected);
        assert!(!hello.pointer_enabled);
        assert!(!hello.keyboard_enabled);
        let (kind, payload) = reader.read_message().unwrap();
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
                    expected: 2,
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
}
