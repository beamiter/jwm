//! An in-process session controller for native viewers.
//!
//! The remote worker retains ownership of its viewer, authenticated transport
//! and input cleanup. UI callers keep a bounded state snapshot and submit
//! commands through the same wake/cancellation path as the CLI.

use super::client::{ClientOptions, FailureDiagnostic, classify_failure, reconnect_client};
use super::clipboard_policy::{ClipboardDirection, ClipboardMode};
pub use super::clipboard_sync::TransferKind;
use super::connect_toggle::{ConnectToggle, ControlState};
use super::messages::FrameCodec;
use super::source_session::SourceId;
use super::source_wire::SourceCatalog;
pub use super::telemetry::{StageTiming, TimingDistribution};
use super::viewer_backend::ViewerBackend;
use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Disconnect,
    ReleaseInput,
    CycleSource,
    SelectSource {
        session_id: u128,
        source_epoch: u64,
        target: SourceId,
    },
    RequestInput {
        session_id: u128,
        source_epoch: u64,
        target: SourceId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSources {
    /// Authenticated connection identity. Old selections expire on reconnect.
    pub session_id: u128,
    pub catalog: SourceCatalog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ViewerFrameScope {
    pub source_epoch: u64,
    pub geometry_epoch: u64,
    pub sequence: u64,
    pub codec: &'static str,
    pub source_dimensions: (u16, u16),
    pub encoded_dimensions: (u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricsSnapshot {
    pub session_id: u128,
    pub window: Duration,
    pub final_report: bool,
    pub negotiated_codec: FrameCodec,
    pub last_decoded_frame: Option<ViewerFrameScope>,
    pub presented_frames: u64,
    pub acked_frames: u64,
    pub native_presented_frames: u64,
    pub last_presentation_path: Option<&'static str>,
    pub decode: StageTiming,
    pub hardware_download: StageTiming,
    pub presentation_queue: StageTiming,
    pub presentation_completion: StageTiming,
    pub native_preparation_and_submit: StageTiming,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    WaitingForPresentation,
    Viewing,
    WaitingToRetry,
    Closing,
    Closed,
    Failed(FailureDiagnostic),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaylandInputState {
    Available,
    Pending,
    Active,
    Releasing,
    Released,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WaylandGestureState {
    pub ready_frame: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardProgress {
    /// Active transfer direction, relative to the host.
    pub direction: ClipboardDirection,
    pub kind: TransferKind,
    /// Sender flow-credit bytes or received bytes for this transfer.
    pub bytes_processed: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub mode: Option<ClipboardMode>,
    pub direction: Option<ClipboardDirection>,
    pub file_limit_bytes: u64,
    pub transfer: Option<ClipboardProgress>,
}

impl ClipboardSnapshot {
    pub(super) fn negotiated(content: u8, directions: u8, file_limit_bytes: u64) -> Self {
        use ClipboardDirection::{Both, FromHost, ToHost};
        use ClipboardMode::{All, Text, TextImages};

        let mode = match content {
            1 => Text,
            3 => TextImages,
            7 if file_limit_bytes != 0 => All,
            _ => return Self::default(),
        };
        let direction = match directions {
            1 => ToHost,
            2 => FromHost,
            3 => Both,
            _ => return Self::default(),
        };
        Self {
            mode: Some(mode),
            direction: Some(direction),
            file_limit_bytes: if mode == All { file_limit_bytes } else { 0 },
            transfer: None,
        }
    }

    pub(super) fn narrow(&mut self, content: u8) {
        let mut next = Self::negotiated(
            content & self.mode.map_or(0, ClipboardMode::mask),
            self.direction.map_or(0, ClipboardDirection::mask),
            self.file_limit_bytes,
        );
        next.update_transfer(self.transfer);
        *self = next;
    }

    pub(super) fn update_transfer(&mut self, progress: Option<ClipboardProgress>) -> bool {
        let next = progress.filter(|progress| {
            let kind = match progress.kind {
                TransferKind::Image => 2,
                TransferKind::Files => 4,
            };
            self.mode.is_some_and(|mode| mode.mask() & kind != 0)
                && progress.direction != ClipboardDirection::Both
                && self
                    .direction
                    .is_some_and(|direction| direction.mask() & progress.direction.mask() != 0)
                && progress.bytes_processed <= progress.total_bytes
        });
        let changed = self.transfer != next;
        self.transfer = next;
        changed
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub sources: Option<SessionSources>,
    pub metrics: Option<MetricsSnapshot>,
    /// The effective clipboard policy after the current connection has negotiated it.
    pub clipboard: Option<ClipboardSnapshot>,
    /// Present when the host granted input and the viewer uses a local grab.
    pub input_grabbed: Option<bool>,
    pub wayland_input: Option<WaylandInputState>,
    pub wayland_source_control: bool,
    /// Present for negotiated gesture sessions, with the latest available start proof.
    pub wayland_gestures: Option<WaylandGestureState>,
}

pub struct RemoteSession {
    control: Arc<ControlState>,
    terminal: Arc<Mutex<Option<SessionState>>>,
    release_supported: bool,
    source_selection_supported: bool,
}

impl RemoteSession {
    /// Start a remote worker without occupying the caller's event loop.
    pub fn connect(
        options: ClientOptions,
        codec: FrameCodec,
        reconnect_attempts: u8,
    ) -> io::Result<Self> {
        Self::start(options, codec, reconnect_attempts, false)
    }

    /// Reauthenticate after an eligible transport loss in the same X11 viewer window.
    pub fn connect_retaining_x11_window(
        options: ClientOptions,
        codec: FrameCodec,
        reconnect_attempts: u8,
    ) -> io::Result<Self> {
        if options.viewer_backend != ViewerBackend::X11 || !options.view_only || options.clipboard {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed reconnect requires an X11 view-only session with clipboard off",
            ));
        }
        if !(1..=5).contains(&reconnect_attempts) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "retained-window reconnect attempts must be 1..=5",
            ));
        }
        Self::start(options, codec, reconnect_attempts, true)
    }

    fn start(
        options: ClientOptions,
        codec: FrameCodec,
        reconnect_attempts: u8,
        retain_x11_window: bool,
    ) -> io::Result<Self> {
        let wayland = options.viewer_backend == ViewerBackend::Wayland;
        let wayland_control = wayland
            && options.wayland_control
            && !options.resumable_control
            && !options.wayland_seamless_input
            && !options.static_lossless;
        if wayland
            && !(super::capabilities::NATIVE_WAYLAND
                && (options.view_only || wayland_control)
                && reconnect_attempts == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "managed Wayland requires a Wayland build, view-only or one-shot control, and no automatic retry",
            ));
        }
        let release_supported =
            wayland_control || (!wayland && options.grab_input && !options.view_only);
        let source_selection_supported = !wayland || options.view_only;
        let (toggle, control) = ConnectToggle::local();
        let terminal = Arc::new(Mutex::new(None));
        let worker_terminal = Arc::clone(&terminal);
        let worker_control = Arc::clone(&control);
        thread::Builder::new()
            .name("jwm-remote-session-engine".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reconnect_client(
                        options,
                        reconnect_attempts,
                        codec,
                        Some(&toggle),
                        retain_x11_window,
                    )
                }))
                .unwrap_or_else(|_| Err(io::Error::other("remote session worker panicked").into()));
                let status = match result {
                    Ok(()) => SessionState::Closed,
                    Err(error) => SessionState::Failed(classify_failure(error.as_ref())),
                };
                worker_control.close();
                *worker_terminal.lock().unwrap() = Some(status);
            })?;
        Ok(Self {
            control,
            terminal,
            release_supported,
            source_selection_supported,
        })
    }

    pub fn send(&self, command: SessionCommand) -> io::Result<()> {
        if !self.source_selection_supported && matches!(command, SessionCommand::CycleSource) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "scoped Wayland control has a fixed source",
            ));
        }
        match command {
            SessionCommand::Disconnect => self.control.close(),
            SessionCommand::ReleaseInput if self.release_supported => {
                return self.control.release_input();
            }
            SessionCommand::ReleaseInput => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this session has no local input grab",
                ));
            }
            SessionCommand::CycleSource => self.control.next_source(),
            SessionCommand::SelectSource {
                session_id,
                source_epoch,
                target,
            } => return self.control.select_source(session_id, source_epoch, target),
            SessionCommand::RequestInput {
                session_id,
                source_epoch,
                target,
            } => {
                return self.control.request_input(session_id, source_epoch, target);
            }
        }
        Ok(())
    }

    /// Catalog for the currently presented source, cleared during switching and retries.
    pub fn sources(&self) -> Option<SessionSources> {
        self.control.sources()
    }

    /// Latest completed local timing window. A new connection starts a fresh window.
    pub fn metrics(&self) -> Option<MetricsSnapshot> {
        self.control.metrics()
    }

    /// Current state replaces earlier snapshots, including after retries.
    pub fn state(&self) -> SessionState {
        self.terminal
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.control.state())
    }

    pub fn snapshot(&self) -> SessionSnapshot {
        let terminal = self.terminal.lock().unwrap();
        let mut snapshot = self.control.snapshot();
        if let Some(state) = terminal.as_ref() {
            snapshot.state = state.clone();
        }
        snapshot
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        self.control.close();
    }
}
