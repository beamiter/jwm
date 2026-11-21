//! Toolkit-independent controller for the JWM native viewer.
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    num::NonZeroU32,
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_EVENT_BYTES: u64 = 64 * 1024;
pub(crate) const TICK: Duration = Duration::from_millis(100);
pub(crate) const CLIPBOARD_FILE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Default)]
pub(crate) struct ConnectionOptions {
    pub(crate) address: String,
    pub(crate) key_file: String,
    pub(crate) backend: Viewer,
    pub(crate) scaling: Scaling,
    pub(crate) control: bool,
    pub(crate) reconnect_automatically: bool,
    pub(crate) wayland_gestures: bool,
    pub(crate) clipboard: Option<ClipboardOptions>,
    pub(crate) x11_embed: Option<X11EmbedTarget>,
}

impl ConnectionOptions {
    pub(crate) fn validate_reconnect(&self) -> Result<(), &'static str> {
        if self.reconnect_automatically
            && !self
                .backend
                .allows_reconnect(self.control, self.clipboard.is_some())
        {
            return Err("Automatic reconnect requires view-only X11 with clipboard sharing off.");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ClipboardMode {
    Text,
    TextImages,
    All,
}

impl ClipboardMode {
    pub(crate) const CHOICES: [Self; 3] = [Self::Text, Self::TextImages, Self::All];

    fn as_arg(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::TextImages => "text-images",
            Self::All => "all",
        }
    }
}

impl std::fmt::Display for ClipboardMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "Text",
            Self::TextImages => "Text and images",
            Self::All => "Text, images and files",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ClipboardDirection {
    Both,
    ToHost,
    FromHost,
}

impl ClipboardDirection {
    pub(crate) const CHOICES: [Self; 3] = [Self::FromHost, Self::ToHost, Self::Both];

    fn as_arg(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::ToHost => "to-host",
            Self::FromHost => "from-host",
        }
    }
}

impl std::fmt::Display for ClipboardDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FromHost => "From host",
            Self::ToHost => "To host",
            Self::Both => "Both ways",
        })
    }
}

// Keep the full u64 range across JSON IPC even on a JavaScript reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct ClipboardBytes(u64);

impl TryFrom<String> for ClipboardBytes {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Clipboard byte counts must be decimal strings.");
        }
        value
            .parse()
            .map(Self)
            .map_err(|_| "Clipboard byte count exceeds the supported range.")
    }
}

impl From<ClipboardBytes> for String {
    fn from(value: ClipboardBytes) -> Self {
        value.0.to_string()
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ClipboardOptions {
    mode: ClipboardMode,
    direction: ClipboardDirection,
    file_limit_bytes: ClipboardBytes,
}

impl ClipboardOptions {
    pub(crate) fn new(
        mode: ClipboardMode,
        direction: ClipboardDirection,
        file_limit_bytes: u64,
    ) -> Self {
        Self {
            mode,
            direction,
            file_limit_bytes: ClipboardBytes(file_limit_bytes),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct ClipboardStatus {
    mode: Option<ClipboardMode>,
    direction: Option<ClipboardDirection>,
    file_limit_bytes: ClipboardBytes,
    transfer: Option<ClipboardTransfer>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
struct ClipboardTransfer {
    direction: ClipboardTransferDirection,
    kind: ClipboardTransferKind,
    bytes_processed: ClipboardBytes,
    total_bytes: ClipboardBytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ClipboardTransferDirection {
    ToHost,
    FromHost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClipboardTransferKind {
    Image,
    Files,
}

#[derive(Clone)]
pub(crate) struct X11EmbedTarget {
    pub(crate) display: String,
    pub(crate) parent: NonZeroU32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Viewer {
    #[default]
    X11,
    Wayland,
}

impl Viewer {
    pub(crate) const CHOICES: [Self; 2] = [Self::X11, Self::Wayland];

    pub(crate) fn allows_reconnect(self, control: bool, clipboard: bool) -> bool {
        self == Self::X11 && !control && !clipboard
    }
}

impl std::fmt::Display for Viewer {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::X11 => "X11",
            Self::Wayland => "Wayland",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Scaling {
    #[default]
    Auto,
    Gpu,
    Cpu,
}

impl Scaling {
    pub(crate) const CHOICES: [Self; 3] = [Self::Auto, Self::Gpu, Self::Cpu];
}

impl std::fmt::Display for Scaling {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Auto => "Auto",
            Self::Gpu => "GPU",
            Self::Cpu => "CPU",
        })
    }
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
struct Catalog {
    session_id: String,
    source_epoch: String,
    source_id: u32,
    entries: Vec<Source>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
struct Source {
    source_id: u32,
    label: String,
}

#[derive(Deserialize)]
struct Snapshot {
    state: String,
    failure: Option<Failure>,
    catalog: Option<Catalog>,
    metrics: Option<Metrics>,
    input_grabbed: Option<bool>,
    wayland_input: Option<WaylandInput>,
    #[serde(default)]
    wayland_source_control: bool,
    wayland_gestures: Option<WaylandGestures>,
    #[serde(default)]
    clipboard: Option<ClipboardStatus>,
}

#[derive(Deserialize)]
struct WaylandGestures {
    ready_frame: Option<u64>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WaylandInput {
    Available,
    Pending,
    Active,
    Releasing,
    Released,
    Unavailable,
}

#[derive(Deserialize)]
struct Failure {
    stage: Option<String>,
    category: String,
}

#[derive(Deserialize)]
struct Metrics {
    session_id: String,
    window_ms: f64,
    presented_frames: u64,
    last_decoded_frame: Option<Frame>,
    decode_ms: Timing,
    presentation_completion_ms: Timing,
    last_presentation_path: Option<String>,
}

#[derive(Deserialize)]
struct Frame {
    source_epoch: u64,
    codec: String,
    encoded_dimensions: [u32; 2],
}

#[derive(Deserialize)]
struct Timing {
    mean: Option<f64>,
}

#[derive(Deserialize)]
struct Message {
    protocol: u32,
    #[serde(flatten)]
    event: Event,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Snapshot(Box<Snapshot>),
    CommandResult {
        accepted: bool,
        error: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Request {
    ReleaseInput { request: u32 },
    SelectSource { request: u32, selection: Selection },
    RequestInput { request: u32, selection: Selection },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    session_id: String,
    source_epoch: String,
    source_id: u32,
}

#[derive(Default)]
struct Updates {
    latest: Option<Box<Snapshot>>,
    message: Option<String>,
    complete: bool,
    ending: bool,
}

struct Adapter {
    commands: mpsc::SyncSender<Request>,
    updates: Arc<Mutex<Updates>>,
}

impl Drop for Adapter {
    fn drop(&mut self) {
        self.updates.lock().unwrap().ending = true;
    }
}

impl Adapter {
    fn start(options: ConnectionOptions) -> io::Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(4);
        let updates = Arc::new(Mutex::new(Updates::default()));
        let worker = Arc::clone(&updates);
        std::thread::Builder::new()
            .name("jwm-remote-controller".into())
            .spawn(move || {
                let result = run_helper(options, receiver, Arc::clone(&worker));
                let mut update = worker.lock().unwrap();
                if let Err(error) = result {
                    update.message = Some(if error.kind() == io::ErrorKind::NotFound {
                        "jwm-remote was not found on PATH.".into()
                    } else {
                        format!("Could not run jwm-remote: {error}")
                    });
                }
                update.complete = true;
            })?;
        Ok(Self { commands, updates })
    }
}

fn helper_command(options: &ConnectionOptions) -> Command {
    let mut command = Command::new("jwm-remote");
    command.args([
        "session",
        "--viewer-backend",
        match options.backend {
            Viewer::X11 => "x11",
            Viewer::Wayland => "wayland",
        },
        "--scaling",
        match options.scaling {
            Scaling::Auto => "auto",
            Scaling::Gpu => "gpu",
            Scaling::Cpu => "cpu",
        },
    ]);
    if !options.key_file.is_empty() {
        command.arg("--key-file").arg(&options.key_file);
    }
    if options.control {
        command.arg("--control");
    }
    if options.reconnect_automatically {
        command.args(["--reconnect-attempts", "3"]);
    }
    if options.wayland_gestures {
        command.arg("--wayland-gestures");
    }
    if let Some(clipboard) = &options.clipboard {
        let file_limit = if clipboard.mode == ClipboardMode::All {
            clipboard.file_limit_bytes.0
        } else {
            0
        };
        command.args([
            "--clipboard",
            "--clipboard-mode",
            clipboard.mode.as_arg(),
            "--clipboard-direction",
            clipboard.direction.as_arg(),
            "--clipboard-file-limit",
        ]);
        command.arg(file_limit.to_string());
    }
    if let Some(target) = &options.x11_embed {
        command.args(["--display", &target.display, "--x11-parent-window"]);
        command.arg(target.parent.to_string());
    }
    command.arg("--").arg(&options.address);
    command
}

fn run_helper(
    options: ConnectionOptions,
    receiver: mpsc::Receiver<Request>,
    updates: Arc<Mutex<Updates>>,
) -> io::Result<()> {
    let mut child = helper_command(&options)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take();
    let output = child.stdout.take().unwrap();
    let reader_updates = Arc::clone(&updates);
    let reader = std::thread::Builder::new()
        .name("jwm-remote-controller-events".into())
        .spawn(move || {
            if read_helper(BufReader::new(output), &reader_updates).is_err() {
                let mut view = reader_updates.lock().unwrap();
                view.message =
                    Some("The installed jwm-remote uses an incompatible session protocol.".into());
                view.ending = true;
            }
        });
    let reader = match reader {
        Ok(reader) => reader,
        Err(error) => {
            drop(input.take());
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let mut deadline = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if updates.lock().unwrap().ending {
            drop(input.take());
        }
        if input.is_none() {
            let end = *deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(8));
            if Instant::now() >= end {
                let _ = child.kill();
                break child.wait()?;
            }
            std::thread::sleep(TICK);
            continue;
        }
        match receiver.recv_timeout(TICK) {
            Ok(request) => {
                let pipe = input.as_mut().unwrap();
                let sent = serde_json::to_writer(&mut *pipe, &request)
                    .map_err(io::Error::other)
                    .and_then(|()| pipe.write_all(b"\n"))
                    .and_then(|()| pipe.flush());
                if sent.is_err() {
                    drop(input.take());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(input.take());
            }
        }
    };
    let _ = reader.join();
    let mut view = updates.lock().unwrap();
    if !status.success() && !view.ending && view.message.is_none() {
        view.message = Some("The jwm-remote helper exited before the session closed.".into());
    }
    Ok(())
}

fn read_helper(mut output: impl BufRead, updates: &Mutex<Updates>) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let size = output
            .by_ref()
            .take(MAX_EVENT_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if size == 0 {
            return Ok(());
        }
        if size as u64 > MAX_EVENT_BYTES || !line.ends_with(b"\n") {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let message: Message = serde_json::from_slice(&line)?;
        if message.protocol != 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut update = updates.lock().unwrap();
        match message.event {
            Event::Snapshot(snapshot) => {
                if let Some(failure) = &snapshot.failure {
                    update.message = Some(match failure.category.as_str() {
                        "wayland_release_confirmation_unavailable" => {
                            "Update jwm-remote on the host to use managed Wayland control.".into()
                        }
                        "managed_wayland_gestures_unavailable" => {
                            "Update jwm-remote on the host to use managed Wayland gestures.".into()
                        }
                        "wayland_gestures_permission_required" => {
                            "The host needs explicit --allow-wayland-gestures permission.".into()
                        }
                        "native_wayland_gestures_unavailable" => {
                            "The host compositor needs Wayland source control version 6.".into()
                        }
                        category => {
                            let stage = failure.stage.as_deref().unwrap_or("session");
                            format!(
                                "Remote session failed during {stage}: {}.",
                                category.replace('_', " ")
                            )
                        }
                    });
                }
                update.latest = Some(snapshot);
            }
            Event::CommandResult {
                accepted: false,
                error,
            } => {
                let message = match error.as_deref() {
                    Some("invalid_or_stale_selection") => {
                        "The source list changed. Select a current source."
                    }
                    Some("not_ready") => "Wait for the current source to finish presenting.",
                    Some("input_not_granted" | "not_supported") => "Remote input is unavailable.",
                    _ => "The viewer rejected the command.",
                };
                update.message = Some(message.into());
            }
            Event::CommandResult { .. } => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SourceChoice {
    pub(crate) selection: Selection,
    pub(crate) label: String,
}

impl std::fmt::Display for SourceChoice {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(&self.label)
    }
}

#[derive(Default)]
pub(crate) struct RemoteDesktop {
    adapter: Option<Adapter>,
    status: Option<&'static str>,
    feedback: String,
    metrics: String,
    clipboard_requested: bool,
    clipboard: Option<ClipboardStatus>,
    release_available: bool,
    request_available: bool,
    closing: bool,
    request_id: u32,
    catalog: Option<Catalog>,
    sources: Vec<SourceChoice>,
}

impl RemoteDesktop {
    pub(crate) fn active(&self) -> bool {
        self.adapter.is_some()
    }

    pub(crate) fn closing(&self) -> bool {
        self.closing
    }

    pub(crate) fn status(&self) -> &'static str {
        self.status.unwrap_or("Disconnected")
    }

    pub(crate) fn feedback(&self) -> &str {
        &self.feedback
    }

    pub(crate) fn metrics(&self) -> &str {
        &self.metrics
    }

    pub(crate) fn clipboard_requested(&self) -> bool {
        self.clipboard_requested
    }

    pub(crate) fn clipboard(&self) -> Option<&ClipboardStatus> {
        self.clipboard.as_ref()
    }

    pub(crate) fn clipboard_summary(&self) -> String {
        use std::fmt::Write as _;

        let Some(clipboard) = self.clipboard() else {
            return if self.clipboard_requested() && self.active() && !self.closing {
                "Clipboard status unavailable".into()
            } else {
                String::new()
            };
        };
        let (Some(mode), Some(direction)) = (clipboard.mode, clipboard.direction) else {
            return "Clipboard off".into();
        };
        let mut summary = format!("Clipboard · {mode} · {direction}");
        if mode == ClipboardMode::All {
            match clipboard.file_limit_bytes.0 {
                CLIPBOARD_FILE_LIMIT_BYTES => summary.push_str(" · File limit 2 GiB"),
                bytes => {
                    let _ = write!(summary, " · File limit {bytes} bytes");
                }
            }
        }
        if let Some(transfer) = &clipboard.transfer {
            let verb = match transfer.direction {
                ClipboardTransferDirection::ToHost => "Sending",
                ClipboardTransferDirection::FromHost => "Receiving",
            };
            let kind = match transfer.kind {
                ClipboardTransferKind::Image => "image",
                ClipboardTransferKind::Files => "files",
            };
            let _ = write!(
                summary,
                "\n{verb} {kind} · {} / {} bytes processed",
                transfer.bytes_processed.0, transfer.total_bytes.0,
            );
        }
        summary
    }

    pub(crate) fn can_release_input(&self) -> bool {
        self.release_available && !self.closing
    }

    pub(crate) fn can_request_input(&self) -> bool {
        self.request_available && !self.closing
    }

    pub(crate) fn sources(&self) -> &[SourceChoice] {
        &self.sources
    }

    pub(crate) fn current_source(&self) -> Option<&SourceChoice> {
        let current = self.catalog.as_ref()?.source_id;
        self.sources
            .iter()
            .find(|source| source.selection.source_id == current)
    }

    pub(crate) fn connect(&mut self, mut options: ConnectionOptions) {
        if self.active() {
            return;
        }
        options.address = options.address.trim().to_owned();
        if options.address.is_empty() {
            self.feedback = "Enter a host name or address.".into();
            return;
        }
        if options.wayland_gestures && (options.backend != Viewer::Wayland || !options.control) {
            self.feedback =
                "Gestures require the Wayland viewer and keyboard and pointer control.".into();
            return;
        }
        if options.clipboard.is_some() && options.backend != Viewer::X11 {
            self.feedback = "Clipboard sharing requires the X11 viewer.".into();
            return;
        }
        if let Err(message) = options.validate_reconnect() {
            self.feedback = message.into();
            return;
        }
        self.feedback.clear();
        self.metrics.clear();
        self.clipboard = None;
        let clipboard_requested = options.clipboard.is_some();
        match Adapter::start(options) {
            Ok(adapter) => {
                self.adapter = Some(adapter);
                self.clipboard_requested = clipboard_requested;
                self.status = Some("Connecting…");
            }
            Err(error) => self.feedback = error.to_string(),
        }
    }

    pub(crate) fn select_source(&mut self, source: &SourceChoice) {
        if self.closing || !self.sources.contains(source) || self.current_source() == Some(source) {
            return;
        }
        let request = self.next_request();
        self.send(Request::SelectSource {
            request,
            selection: source.selection.clone(),
        });
    }

    pub(crate) fn release_input(&mut self) {
        if self.can_release_input() {
            let request = self.next_request();
            self.send(Request::ReleaseInput { request });
        }
    }

    pub(crate) fn request_input(&mut self, selection: &Selection) {
        if self.can_request_input()
            && self
                .current_source()
                .is_some_and(|source| &source.selection == selection)
        {
            let request = self.next_request();
            self.send(Request::RequestInput {
                request,
                selection: selection.clone(),
            });
        }
    }

    pub(crate) fn disconnect(&mut self) {
        if let Some(adapter) = self.adapter.as_ref() {
            adapter.updates.lock().unwrap().ending = true;
            self.closing = true;
            self.clipboard = None;
            self.feedback.clear();
            self.status = Some("Disconnecting…");
        }
    }

    fn next_request(&mut self) -> u32 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    fn send(&mut self, request: Request) {
        if let Some(adapter) = self.adapter.as_ref() {
            self.feedback.clear();
            if adapter.commands.try_send(request).is_err() {
                self.feedback = "The viewer is busy or closing.".into();
            }
        }
    }

    pub(crate) fn refresh(&mut self) {
        let (latest, message, complete) = {
            let Some(adapter) = self.adapter.as_ref() else {
                return;
            };
            let mut updates = adapter.updates.lock().unwrap();
            (
                updates.latest.take(),
                updates.message.take(),
                updates.complete,
            )
        };
        if let Some(snapshot) = latest.filter(|_| !self.closing) {
            self.release_available = snapshot.state == "viewing"
                && (snapshot.input_grabbed == Some(true)
                    || snapshot.wayland_input == Some(WaylandInput::Active));
            self.request_available = snapshot.state == "viewing"
                && snapshot.wayland_input == Some(WaylandInput::Available)
                && snapshot.wayland_source_control;
            self.status = Some(
                match (
                    snapshot.state.as_str(),
                    snapshot.input_grabbed,
                    snapshot.wayland_input,
                ) {
                    ("connecting", _, _) => "Connecting…",
                    ("waiting_for_presentation", _, _) => "Waiting for the selected source…",
                    ("viewing", _, Some(WaylandInput::Pending)) => {
                        "Waiting for Wayland input authorization…"
                    }
                    ("viewing", _, Some(WaylandInput::Available)) => {
                        "Viewing only · Request input for this source"
                    }
                    ("viewing", _, Some(WaylandInput::Active))
                        if snapshot
                            .wayland_gestures
                            .as_ref()
                            .is_some_and(|gestures| gestures.ready_frame.is_some()) =>
                    {
                        "Wayland gestures active · Ready to drag"
                    }
                    ("viewing", _, Some(WaylandInput::Active))
                        if snapshot.wayland_gestures.is_some() =>
                    {
                        "Wayland gestures active"
                    }
                    ("viewing", _, Some(WaylandInput::Active))
                        if snapshot.wayland_source_control =>
                    {
                        "Wayland input authorized"
                    }
                    ("viewing", _, Some(WaylandInput::Active)) => {
                        "Wayland input authorized · Fixed source"
                    }
                    ("viewing", _, Some(WaylandInput::Releasing)) => "Releasing Wayland input…",
                    ("viewing", _, Some(WaylandInput::Released)) => "Input released · Viewing only",
                    ("viewing", _, Some(WaylandInput::Unavailable)) => {
                        "Wayland input unavailable · Viewing only"
                    }
                    ("viewing", Some(true), None) => {
                        "Input captured · Move out of the image to release"
                    }
                    ("viewing", Some(false), None) => "Click the viewer to take control",
                    ("viewing", None, None) => "Viewing only",
                    ("waiting_to_retry", _, _) => "Waiting to reconnect…",
                    ("closing", _, _) => "Disconnecting…",
                    ("failed", _, _) => "Connection failed",
                    _ => "Disconnected",
                },
            );
            if self.catalog != snapshot.catalog {
                self.sources.clear();
                if let Some(catalog) = &snapshot.catalog {
                    self.sources.extend(
                        catalog
                            .entries
                            .iter()
                            .filter(|source| {
                                snapshot.wayland_input.is_none()
                                    || snapshot.wayland_source_control
                                    || source.source_id == catalog.source_id
                            })
                            .map(|source| SourceChoice {
                                selection: Selection {
                                    session_id: catalog.session_id.clone(),
                                    source_epoch: catalog.source_epoch.clone(),
                                    source_id: source.source_id,
                                },
                                label: source.label.clone(),
                            }),
                    );
                }
                self.catalog = snapshot.catalog;
            }
            self.metrics = format_metrics(snapshot.metrics, self.catalog.as_ref());
            self.clipboard = if self.clipboard_requested
                && matches!(
                    snapshot.state.as_str(),
                    "viewing" | "waiting_for_presentation"
                ) {
                snapshot.clipboard
            } else {
                None
            };
        }
        if let Some(message) = message.filter(|_| !self.closing) {
            self.feedback = message;
        }
        if complete {
            self.status = Some(if self.closing || self.feedback.is_empty() {
                "Disconnected"
            } else {
                "Connection failed"
            });
            self.adapter = None;
            self.catalog = None;
            self.sources.clear();
            self.metrics.clear();
            self.clipboard_requested = false;
            self.clipboard = None;
            self.release_available = false;
            self.request_available = false;
            self.closing = false;
        }
    }
}

fn format_metrics(metrics: Option<Metrics>, catalog: Option<&Catalog>) -> String {
    let Some((metrics, catalog)) = metrics.zip(catalog) else {
        return String::new();
    };
    let Some(frame) = metrics.last_decoded_frame.filter(|frame| {
        metrics.session_id == catalog.session_id
            && Some(frame.source_epoch) == catalog.source_epoch.parse().ok()
    }) else {
        return String::new();
    };
    let fps = metrics.presented_frames as f64 * 1000.0 / metrics.window_ms.max(1.0);
    let timing = |value: Option<f64>| value.map_or("—".into(), |ms| format!("{ms:.1} ms"));
    format!(
        "{} · {}×{} · {:.1} fps\nDecode {} · Display {}\n{}",
        frame.codec,
        frame.encoded_dimensions[0],
        frame.encoded_dimensions[1],
        fps,
        timing(metrics.decode_ms.mean),
        timing(metrics.presentation_completion_ms.mean),
        metrics.last_presentation_path.as_deref().unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn argv(options: &ConnectionOptions) -> Vec<String> {
        helper_command(options)
            .get_args()
            .map(|item| item.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn managed_reconnect_is_explicit_and_requires_view_only_x11() {
        let mut options = ConnectionOptions {
            address: "127.0.0.1".into(),
            ..ConnectionOptions::default()
        };
        assert!(!argv(&options)
            .iter()
            .any(|arg| arg == "--reconnect-attempts"));
        options.reconnect_automatically = true;
        options.x11_embed = Some(X11EmbedTarget {
            display: ":9".into(),
            parent: NonZeroU32::new(42).unwrap(),
        });
        assert_eq!(options.validate_reconnect(), Ok(()));
        assert_eq!(
            &argv(&options)[5..],
            [
                "--reconnect-attempts",
                "3",
                "--display",
                ":9",
                "--x11-parent-window",
                "42",
                "--",
                "127.0.0.1"
            ]
        );
        for (backend, control, clipboard) in [
            (Viewer::X11, true, false),
            (Viewer::X11, false, true),
            (Viewer::Wayland, false, false),
        ] {
            let mut session = RemoteDesktop::default();
            session.connect(ConnectionOptions {
                backend,
                control,
                clipboard: clipboard.then(|| {
                    ClipboardOptions::new(ClipboardMode::Text, ClipboardDirection::FromHost, 0)
                }),
                x11_embed: None,
                ..options.clone()
            });
            assert!(!session.active());
            assert_eq!(
                session.feedback(),
                "Automatic reconnect requires view-only X11 with clipboard sharing off."
            );
        }
    }

    #[test]
    fn clipboard_cli_is_explicit_and_preserves_the_selected_byte_limit() {
        let mut options = ConnectionOptions {
            address: "127.0.0.1".into(),
            ..ConnectionOptions::default()
        };
        assert_eq!(
            argv(&options),
            [
                "session",
                "--viewer-backend",
                "x11",
                "--scaling",
                "auto",
                "--",
                "127.0.0.1"
            ]
        );
        options.clipboard = Some(
            serde_json::from_value(json!({
                "mode":"all", "direction":"to-host", "fileLimitBytes":"9007199254740993"
            }))
            .unwrap(),
        );
        assert_eq!(
            &argv(&options)[5..],
            [
                "--clipboard",
                "--clipboard-mode",
                "all",
                "--clipboard-direction",
                "to-host",
                "--clipboard-file-limit",
                "9007199254740993",
                "--",
                "127.0.0.1"
            ]
        );
        options.clipboard = Some(ClipboardOptions::new(
            ClipboardMode::TextImages,
            ClipboardDirection::ToHost,
            CLIPBOARD_FILE_LIMIT_BYTES,
        ));
        assert_eq!(
            &argv(&options)[5..],
            [
                "--clipboard",
                "--clipboard-mode",
                "text-images",
                "--clipboard-direction",
                "to-host",
                "--clipboard-file-limit",
                "0",
                "--",
                "127.0.0.1"
            ]
        );
    }

    fn snapshot(session: &mut RemoteDesktop, updates: &Mutex<Updates>, event: Value) {
        let mut line = serde_json::to_vec(&event).unwrap();
        line.push(b'\n');
        read_helper(&line[..], updates).unwrap();
        session.refresh();
    }

    #[test]
    fn clipboard_snapshot_legacy_effective_progress_and_cleanup() {
        let (commands, _receiver) = mpsc::sync_channel(4);
        let updates = Arc::new(Mutex::new(Updates::default()));
        let mut session = RemoteDesktop {
            adapter: Some(Adapter {
                commands,
                updates: Arc::clone(&updates),
            }),
            clipboard_requested: true,
            ..RemoteDesktop::default()
        };
        snapshot(
            &mut session,
            &updates,
            json!({"protocol":1,"event":"snapshot","state":"viewing"}),
        );
        assert_eq!(session.status(), "Viewing only");
        assert_eq!(session.clipboard(), None);
        assert_eq!(session.clipboard_summary(), "Clipboard status unavailable");
        let current = json!({
            "mode":"all","direction":"from-host","file_limit_bytes":"18446744073709551615",
            "transfer":{"direction":"from-host","kind":"files",
                        "bytes_processed":"9007199254740993","total_bytes":"9007199254741000"}
        });
        let catalog = json!({
            "session_id":"0123456789abcdef0123456789abcdef", "source_epoch":"1", "source_id":1,
            "entries":[{"source_id":1,"label":"Root"}]
        });
        let metrics = json!({
            "session_id":"0123456789abcdef0123456789abcdef", "window_ms":5000,
            "presented_frames":140,
            "last_decoded_frame":{"source_epoch":1,"codec":"JPEG","encoded_dimensions":[800,600]},
            "decode_ms":{"mean":1.0}, "presentation_completion_ms":{"mean":2.0},
            "last_presentation_path":"rgb"
        });
        snapshot(
            &mut session,
            &updates,
            json!({"protocol":1,"event":"snapshot","state":"viewing","clipboard":current,
                   "catalog":catalog,"metrics":metrics}),
        );
        assert_eq!(session.sources().len(), 1);
        assert!(!session.metrics().is_empty());
        assert_eq!(serde_json::to_value(session.clipboard()).unwrap(), current);
        assert_eq!(
            session.clipboard_summary(),
            "Clipboard · Text, images and files · From host · File limit 18446744073709551615 bytes\nReceiving files · 9007199254740993 / 9007199254741000 bytes processed"
        );
        snapshot(
            &mut session,
            &updates,
            json!({"protocol":1,"event":"snapshot","state":"waiting_to_retry","clipboard":current}),
        );
        assert!(session.active());
        assert_eq!(session.status(), "Waiting to reconnect…");
        assert!(!session.can_release_input());
        assert!(!session.can_request_input());
        assert!(session.sources().is_empty());
        assert!(session.metrics().is_empty());
        assert_eq!(session.clipboard(), None);
        snapshot(
            &mut session,
            &updates,
            json!({"protocol":1,"event":"snapshot","state":"viewing","clipboard":current}),
        );
        assert!(session.clipboard().is_some());
        session.disconnect();
        assert_eq!(session.clipboard(), None);
        assert!(session.clipboard_summary().is_empty());
        snapshot(
            &mut session,
            &updates,
            json!({"protocol":1,"event":"snapshot","state":"viewing","clipboard":current}),
        );
        assert_eq!(session.clipboard(), None);
        updates.lock().unwrap().complete = true;
        session.refresh();
        assert!(!session.clipboard_requested());
        assert_eq!(session.clipboard(), None);
    }
}
