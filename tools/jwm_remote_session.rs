//! Local, versioned control for terminal adapters. The viewer owns all pixels.
use super::{DEFAULT_CLIPBOARD_FILE_LIMIT, normalize_address, parse_scaling, resolve_key_path};
use clap::Args;
use jwm::remote::{
    RemoteResult,
    client::ClientOptions,
    clipboard_files::parse_size,
    clipboard_policy::{ClipboardDirection, ClipboardMode},
    engine::{
        ClipboardSnapshot, RemoteSession, SessionCommand, SessionSnapshot, SessionState,
        TransferKind,
    },
    key::KeySource,
    messages::FrameCodec,
    source_session::SourceId,
    viewer_backend::{SeamlessInput, ViewerBackend},
    window_size::WindowSize,
    x11_capture::ScalingPolicy,
    x11_viewer::DEFAULT_ESCAPE_KEY,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

const VERSION: u32 = 1;
const MAX_COMMAND_BYTES: usize = 512;
const UPDATE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Args, Debug)]
pub(super) struct SessionArgs {
    /// Hostname or IP, with optional port.
    #[arg(value_parser = normalize_address)]
    address: String,
    /// Private session key file. The standard JWM key path is the default.
    #[arg(long)]
    key_file: Option<PathBuf>,
    /// Request keyboard and pointer control. Wayland uses a one-shot lease per authorized window.
    #[arg(long)]
    control: bool,
    /// Share the clipboard with the X11 host when it also grants clipboard access.
    #[arg(long)]
    clipboard: bool,
    /// Narrow shared content to text, text-images, or all.
    #[arg(long, default_value = "all", requires = "clipboard")]
    clipboard_mode: ClipboardMode,
    /// Choose a host-relative direction: both, to-host, or from-host.
    #[arg(long, default_value = "both", requires = "clipboard")]
    clipboard_direction: ClipboardDirection,
    /// Largest shared file copy, such as 512M or 2G. Zero disables files.
    #[arg(long, default_value = DEFAULT_CLIPBOARD_FILE_LIMIT, requires = "clipboard", value_name = "SIZE", value_parser = parse_clipboard_file_limit)]
    clipboard_file_limit: u64,
    /// Base X keysym for release and clipboard restriction shortcuts.
    #[arg(long, default_value = DEFAULT_ESCAPE_KEY, value_name = "KEYSYM")]
    escape_key: String,
    /// Request separately authorized Wayland vertical scroll with control.
    #[arg(long, requires = "control")]
    wayland_scroll: bool,
    /// Request separately authorized Wayland horizontal scroll with control.
    #[arg(long, requires = "control")]
    wayland_horizontal_scroll: bool,
    /// Require a host grant for modifier clicks and in-window left-button dragging.
    #[arg(long, requires = "control")]
    wayland_gestures: bool,
    /// Use JPEG for this session.
    #[arg(long)]
    jpeg: bool,
    /// Additional transport retries in the same X11 view-only window with clipboard off.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=5))]
    reconnect_attempts: u8,
    /// Write detailed viewer timing windows as JSON to stderr.
    #[arg(long)]
    stats_json: bool,
    /// Native viewer backend.
    #[arg(long, default_value_t = ViewerBackend::X11)]
    viewer_backend: ViewerBackend,
    /// X11 viewer display, defaulting to DISPLAY.
    #[arg(long)]
    display: Option<String>,
    /// Native viewer Wayland socket.
    #[arg(long)]
    viewer_wayland_display: Option<String>,
    /// Initial local viewer size.
    #[arg(long)]
    window_size: Option<WindowSize>,
    /// XEmbed container window ID (decimal) owned by the local application.
    #[arg(long, conflicts_with = "window_size")]
    x11_parent_window: Option<std::num::NonZeroU32>,
    /// Scaling preference.
    #[arg(long, default_value = "auto", value_parser = parse_scaling)]
    scaling: ScalingPolicy,
}

fn parse_clipboard_file_limit(value: &str) -> Result<u64, String> {
    let bytes = value.trim();
    if !bytes.is_empty() && bytes.bytes().all(|digit| digit.is_ascii_digit()) {
        return bytes.parse().map_err(|_| format!("{value:?} is too large"));
    }
    parse_size(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    session_id: String,
    source_epoch: String,
    source_id: u32,
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum SessionRequest {
    Disconnect { request: u32 },
    ReleaseInput { request: u32 },
    SelectSource { request: u32, selection: Selection },
    RequestInput { request: u32, selection: Selection },
}

fn apply(session: &RemoteSession, request: SessionRequest) -> Value {
    let (id, command) = match request {
        SessionRequest::Disconnect { request } => (request, Ok(SessionCommand::Disconnect)),
        SessionRequest::ReleaseInput { request } => (request, Ok(SessionCommand::ReleaseInput)),
        SessionRequest::SelectSource { request, selection } => (request, select_command(selection)),
        SessionRequest::RequestInput { request, selection } => (
            request,
            parse_selection(selection).map(|(session_id, source_epoch, target)| {
                SessionCommand::RequestInput {
                    session_id,
                    source_epoch,
                    target,
                }
            }),
        ),
    };
    let result = command.and_then(|command| session.send(command));
    let error = result.err().map(|error| match error.kind() {
        io::ErrorKind::InvalidInput => "invalid_or_stale_selection",
        io::ErrorKind::WouldBlock => "not_ready",
        io::ErrorKind::PermissionDenied => "input_not_granted",
        io::ErrorKind::Unsupported => "not_supported",
        _ => "session_error",
    });
    json!({"protocol": VERSION, "event": "command_result", "request": id,
           "accepted": error.is_none(), "error": error})
}

fn select_command(selection: Selection) -> io::Result<SessionCommand> {
    let (session_id, source_epoch, target) = parse_selection(selection)?;
    Ok(SessionCommand::SelectSource {
        session_id,
        source_epoch,
        target,
    })
}

fn parse_selection(selection: Selection) -> io::Result<(u128, u64, SourceId)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid source identity");
    if selection.session_id.len() != 32
        || !selection
            .session_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || selection.source_id == 0
        || selection.source_epoch.is_empty()
        || !selection
            .source_epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid());
    }
    let session_id = u128::from_str_radix(&selection.session_id, 16).map_err(|_| invalid())?;
    let source_epoch = selection
        .source_epoch
        .parse::<u64>()
        .map_err(|_| invalid())?;
    if source_epoch == 0 {
        return Err(invalid());
    }
    Ok((session_id, source_epoch, SourceId(selection.source_id)))
}

fn snapshot_event(snapshot: &SessionSnapshot) -> Value {
    let (state, failure) = match &snapshot.state {
        SessionState::Connecting => ("connecting", None),
        SessionState::WaitingForPresentation => ("waiting_for_presentation", None),
        SessionState::Viewing => ("viewing", None),
        SessionState::WaitingToRetry => ("waiting_to_retry", None),
        SessionState::Closing => ("closing", None),
        SessionState::Closed => ("closed", None),
        SessionState::Failed(failure) => ("failed", Some(failure)),
    };
    let catalog = snapshot.sources.as_ref().map(|sources| {
        json!({
            "session_id": format!("{:032x}", sources.session_id),
            "source_epoch": sources.catalog.current.epoch.to_string(),
            "source_id": sources.catalog.current.source.0,
            "entries": sources.catalog.entries.iter().map(|entry| json!({
                "source_id": entry.id.0, "label": entry.label,
            })).collect::<Vec<_>>(),
        })
    });
    let metrics = snapshot.metrics.map(|metrics| {
        json!({
            "session_id": format!("{:032x}", metrics.session_id),
            "window_ms": metrics.window.as_secs_f64() * 1000.0,
            "final": metrics.final_report,
            "negotiated_codec": metrics.negotiated_codec.name(),
            "last_decoded_frame": metrics.last_decoded_frame,
            "presented_frames": metrics.presented_frames,
            "acked_frames": metrics.acked_frames,
            "native_presented_frames": metrics.native_presented_frames,
            "last_presentation_path": metrics.last_presentation_path,
            "decode_ms": metrics.decode,
            "hardware_download_ms": metrics.hardware_download,
            "presentation_queue_ms": metrics.presentation_queue,
            "presentation_completion_ms": metrics.presentation_completion,
            "native_preparation_and_submit_ms": metrics.native_preparation_and_submit,
        })
    });
    json!({
        "protocol": VERSION, "event": "snapshot", "state": state, "failure": failure,
        "catalog": catalog, "metrics": metrics, "input_grabbed": snapshot.input_grabbed,
        "wayland_input": snapshot.wayland_input,
        "wayland_source_control": snapshot.wayland_source_control,
        "wayland_gestures": snapshot.wayland_gestures,
        "clipboard": snapshot.clipboard.as_ref().map(clipboard_event),
    })
}

fn clipboard_event(clipboard: &ClipboardSnapshot) -> Value {
    let transfer = clipboard.transfer.as_ref().map(|progress| {
        json!({
            "direction": progress.direction,
            "kind": match progress.kind {
                TransferKind::Image => "image",
                TransferKind::Files => "files",
            },
            "bytes_processed": progress.bytes_processed.to_string(),
            "total_bytes": progress.total_bytes.to_string(),
        })
    });
    json!({
        "mode": clipboard.mode,
        "direction": clipboard.direction,
        "file_limit_bytes": clipboard.file_limit_bytes.to_string(),
        "transfer": transfer,
    })
}

fn emit(out: &mut impl Write, event: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &event)?;
    out.write_all(b"\n")?;
    out.flush()
}

enum Incoming {
    Request(SessionRequest),
    Malformed,
}

fn receive_commands(
    mut input: impl BufRead,
    commands: mpsc::SyncSender<Incoming>,
    session: Arc<RemoteSession>,
) {
    let mut line = Vec::with_capacity(MAX_COMMAND_BYTES);
    loop {
        let message = match read_request(&mut input, &mut line) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(_) => {
                let _ = commands.try_send(Incoming::Malformed);
                break;
            }
        };
        // A stalled controller cannot make the helper queue machine input or
        // source selection indefinitely. Disconnect through ordinary cleanup.
        if commands.try_send(message).is_err() {
            break;
        }
    }
    let _ = session.send(SessionCommand::Disconnect);
}

fn read_request(input: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<Incoming>> {
    line.clear();
    let length = input
        .by_ref()
        .take((MAX_COMMAND_BYTES + 1) as u64)
        .read_until(b'\n', line)?;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_COMMAND_BYTES || !line.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid session record length",
        ));
    }
    Ok(Some(
        serde_json::from_slice(line)
            .map(Incoming::Request)
            .unwrap_or(Incoming::Malformed),
    ))
}

fn drive(
    session: &RemoteSession,
    mut output: impl Write,
    commands: mpsc::Receiver<Incoming>,
) -> RemoteResult<()> {
    let mut last = None;
    let mut requests_open = true;
    loop {
        let snapshot = session.snapshot();
        let closed = matches!(
            snapshot.state,
            SessionState::Closed | SessionState::Failed(_)
        );
        let failure = match &snapshot.state {
            SessionState::Failed(failure) => Some(match failure.category {
                "wayland_release_confirmation_unavailable" => {
                    "update the host's jwm-remote to use managed Wayland control"
                }
                "managed_wayland_gestures_unavailable" => {
                    "update the host's jwm-remote to use managed Wayland gestures"
                }
                "wayland_gestures_permission_required" => {
                    "the host needs explicit --allow-wayland-gestures permission"
                }
                "native_wayland_gestures_unavailable" => {
                    "the host compositor needs Wayland source control version 6"
                }
                "wayland_presentation_discarded" => {
                    "the local Wayland compositor discarded the remote frame"
                }
                _ => "managed remote session failed",
            }),
            _ => None,
        };
        if last.as_ref() != Some(&snapshot) {
            emit(&mut output, snapshot_event(&snapshot))?;
            last = Some(snapshot);
        }
        if closed {
            if let Some(failure) = failure {
                return Err(io::Error::other(failure).into());
            }
            return Ok(());
        }
        if requests_open {
            match commands.recv_timeout(UPDATE_INTERVAL) {
                Ok(Incoming::Request(request)) => emit(&mut output, apply(session, request))?,
                Ok(Incoming::Malformed) => emit(
                    &mut output,
                    json!({
                        "protocol": VERSION, "event": "command_result", "request": null,
                        "accepted": false, "error": "malformed_command",
                    }),
                )?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => requests_open = false,
            }
        } else {
            std::thread::sleep(UPDATE_INTERVAL);
        }
    }
}

pub(super) fn run(args: SessionArgs) -> RemoteResult<()> {
    let wayland_control = args.control && args.viewer_backend == ViewerBackend::Wayland;
    let x11_control = args.control && args.viewer_backend == ViewerBackend::X11;
    if args.x11_parent_window.is_some() && args.viewer_backend != ViewerBackend::X11 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "XEmbed requires the X11 viewer",
        )
        .into());
    }
    if args.clipboard && args.viewer_backend != ViewerBackend::X11 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Clipboard sharing requires the X11 viewer",
        )
        .into());
    }
    if args.reconnect_attempts != 0
        && (args.viewer_backend != ViewerBackend::X11 || args.control || args.clipboard)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed reconnect requires an X11 view-only session with clipboard off",
        )
        .into());
    }
    if args.wayland_gestures && !wayland_control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Wayland gestures require the native viewer with control enabled",
        )
        .into());
    }
    if (args.wayland_scroll || args.wayland_horizontal_scroll) && !wayland_control {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Wayland scroll requires the native viewer with control enabled",
        )
        .into());
    }
    let options = ClientOptions {
        stats_json: args.stats_json,
        control_duration: Default::default(),
        viewer_backend: args.viewer_backend,
        viewer_wayland_display: args.viewer_wayland_display,
        address: args.address.clone(),
        peer_label: args.address,
        key: KeySource::File(resolve_key_path(args.key_file)?),
        display: args.display,
        window_size: args.window_size,
        window_position: None,
        x11_parent_window: args.x11_parent_window,
        resize_source: None,
        follow_source_size: false,
        wayland_keyboard: false,
        wayland_pointer: false,
        wayland_control,
        wayland_gestures: args.wayland_gestures,
        resumable_control: false,
        wayland_seamless_input: false,
        wayland_text: None,
        wayland_popup_pointer: false,
        static_lossless: false,
        wayland_scroll: args.wayland_scroll,
        wayland_horizontal_scroll: args.wayland_horizontal_scroll,
        view_only: !args.control,
        grab_input: x11_control,
        seamless_input: x11_control.then_some(SeamlessInput::Click),
        clipboard: args.clipboard,
        clipboard_mode: args.clipboard_mode,
        clipboard_direction: args.clipboard_direction,
        clipboard_file_limit: args.clipboard_file_limit,
        frame_limits: Default::default(),
        escape_key: args.escape_key,
        scaling: args.scaling,
    };
    let codec = if args.jpeg || !cfg!(feature = "remote-h264") {
        FrameCodec::JpegTiles
    } else {
        FrameCodec::Hevc444
    };
    let session = Arc::new(if args.reconnect_attempts == 0 {
        RemoteSession::connect(options, codec, 0)?
    } else {
        RemoteSession::connect_retaining_x11_window(options, codec, args.reconnect_attempts)?
    });
    let (sender, receiver) = mpsc::sync_channel(8);
    let input_session = Arc::clone(&session);
    std::thread::Builder::new()
        .name("jwm-remote-session-stdin".into())
        .spawn(move || receive_commands(io::stdin().lock(), sender, input_session))?;
    let result = drive(&session, io::stdout().lock(), receiver);
    if result.is_err() {
        let _ = session.send(SessionCommand::Disconnect);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !matches!(
            session.state(),
            SessionState::Closed | SessionState::Failed(_)
        ) && Instant::now() < deadline
        {
            std::thread::sleep(UPDATE_INTERVAL);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_session_raw_stats_json_is_opt_in() {
        use clap::Parser;

        for enabled in [false, true] {
            let mut argv = vec!["jwm-remote", "session", "127.0.0.1:3053"];
            if enabled {
                argv.push("--stats-json");
            }
            let crate::Command::Session(args) = crate::Cli::try_parse_from(argv).unwrap().command
            else {
                panic!("managed session command expected");
            };
            assert_eq!(args.stats_json, enabled);
        }
    }

    #[test]
    fn managed_reconnect_is_opt_in_and_bounded() {
        use clap::Parser;

        let base = ["jwm-remote", "session", "127.0.0.1:3053"];
        let crate::Command::Session(args) = crate::Cli::try_parse_from(base).unwrap().command
        else {
            panic!("managed session command expected");
        };
        assert_eq!(args.reconnect_attempts, 0);
        for (value, expected) in [("0", 0), ("1", 1), ("5", 5)] {
            let crate::Command::Session(args) =
                crate::Cli::try_parse_from(base.into_iter().chain(["--reconnect-attempts", value]))
                    .unwrap()
                    .command
            else {
                panic!("managed session command expected");
            };
            assert_eq!(args.reconnect_attempts, expected);
        }
        assert!(
            crate::Cli::try_parse_from(base.into_iter().chain(["--reconnect-attempts", "6"]))
                .is_err()
        );
        for ineligible in [
            &["--control"][..],
            &["--clipboard"],
            &["--viewer-backend", "wayland"],
        ] {
            let argv = base
                .into_iter()
                .chain(["--reconnect-attempts", "1"])
                .chain(ineligible.iter().copied());
            let crate::Command::Session(args) = crate::Cli::try_parse_from(argv).unwrap().command
            else {
                panic!("managed session command expected");
            };
            let error = run(args).unwrap_err();
            assert_eq!(
                error.to_string(),
                "managed reconnect requires an X11 view-only session with clipboard off"
            );
        }
    }

    #[test]
    fn managed_clipboard_selectors_require_explicit_consent() {
        use clap::Parser;

        let base = ["jwm-remote", "session", "127.0.0.1:3053"];
        let crate::Command::Session(args) = crate::Cli::try_parse_from(base).unwrap().command
        else {
            panic!("managed session command expected");
        };
        assert!(!args.clipboard);

        let selectors = [
            ["--clipboard-mode", "text-images"],
            ["--clipboard-direction", "to-host"],
            ["--clipboard-file-limit", "32M"],
        ];
        for selector in selectors {
            assert!(crate::Cli::try_parse_from(base.into_iter().chain(selector)).is_err());
        }

        let crate::Command::Session(args) = crate::Cli::try_parse_from(
            base.into_iter()
                .chain(["--clipboard", "--escape-key", "Pause"])
                .chain(selectors.into_iter().flatten()),
        )
        .unwrap()
        .command
        else {
            panic!("managed session command expected");
        };
        assert!(args.clipboard);
        assert_eq!(args.clipboard_mode, ClipboardMode::TextImages);
        assert_eq!(args.clipboard_direction, ClipboardDirection::ToHost);
        assert_eq!(args.clipboard_file_limit, 32 * 1024 * 1024);
        assert_eq!(args.escape_key, "Pause");
        assert_eq!(
            parse_clipboard_file_limit("9007199254740993"),
            Ok((1 << 53) + 1)
        );
        assert!(parse_clipboard_file_limit("18446744073709551616").is_err());
    }

    #[test]
    fn managed_clipboard_snapshot_preserves_byte_counts_and_host_direction() {
        use jwm::remote::engine::ClipboardProgress;

        let clipboard = ClipboardSnapshot {
            mode: Some(ClipboardMode::All),
            direction: Some(ClipboardDirection::Both),
            file_limit_bytes: u64::MAX,
            transfer: Some(ClipboardProgress {
                direction: ClipboardDirection::FromHost,
                kind: TransferKind::Files,
                bytes_processed: u64::MAX - 1,
                total_bytes: u64::MAX,
            }),
        };
        assert_eq!(
            clipboard_event(&clipboard),
            json!({
                "mode": "all",
                "direction": "both",
                "file_limit_bytes": "18446744073709551615",
                "transfer": {
                    "direction": "from-host",
                    "kind": "files",
                    "bytes_processed": "18446744073709551614",
                    "total_bytes": "18446744073709551615",
                },
            }),
        );
    }

    #[test]
    fn wire_selection_preserves_session_and_epoch_at_integer_limits() {
        let payload = br#"{"command":"select_source","request":9,"selection":{"session_id":"ffffffffffffffffffffffffffffffff","source_epoch":"18446744073709551615","source_id":32}}"#;
        let request = serde_json::from_slice::<SessionRequest>(payload).unwrap();
        let SessionRequest::SelectSource { request, selection } = request else {
            panic!()
        };
        assert_eq!(request, 9);
        assert_eq!(
            select_command(selection).unwrap(),
            SessionCommand::SelectSource {
                session_id: u128::MAX,
                source_epoch: u64::MAX,
                target: SourceId(32),
            }
        );
        let bad = br#"{"command":"select_source","request":9,"selection":{"session_id":"ffffffffffffffffffffffffffffffff","source_epoch":18446744073709551615,"source_id":32}}"#;
        assert!(serde_json::from_slice::<SessionRequest>(bad).is_err());
    }

    #[test]
    fn session_reader_caps_growth_and_requires_line_termination() {
        let oversized = vec![b'a'; 8192];
        let mut input = oversized.as_slice();
        let mut buffer = Vec::new();
        assert!(read_request(&mut input, &mut buffer).is_err());
        assert_eq!(buffer.len(), MAX_COMMAND_BYTES + 1);
        assert_eq!(input.len(), oversized.len() - MAX_COMMAND_BYTES - 1);
        assert!(
            read_request(
                &mut br#"{"command":"disconnect","request":1}"#.as_slice(),
                &mut buffer
            )
            .is_err()
        );
        assert!(matches!(
            read_request(
                &mut b"{\"command\":\"disconnect\",\"request\":1}\n".as_slice(),
                &mut buffer
            ),
            Ok(Some(Incoming::Request(SessionRequest::Disconnect {
                request: 1
            })))
        ));
    }
}
