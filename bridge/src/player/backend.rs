//! A bounded worker owns mpv and its private IPC socket. No shell, shared
//! filesystem socket, libmpv linkage, or compositor-thread I/O is involved.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::cli::{MAX_FILES, MAX_INPUT_BYTES, Mode, Open};
use super::eva;
use super::model::State;
use std::collections::VecDeque;
type EvaReceipts = Arc<Mutex<VecDeque<Value>>>;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_LIMIT: usize = 1024 * 1024;
const QUEUE_CAPACITY: usize = 32;
pub type SharedState = Arc<Mutex<State>>;

#[derive(Debug)]
pub enum Action {
    Open(Open),
    OpenEva { open: Open, request: Value },
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek(i64),
    SetPosition { track: String, position: i64 },
    Volume(f64),
    Rate(f64),
    Loop(String),
    Shuffle(bool),
    Fullscreen(bool),
    Raise,
    Quit,
}

struct Request {
    action: Action,
    reply: oneshot::Sender<Result<(), String>>,
    deadline: Instant,
}

#[derive(Clone)]
pub struct Controller {
    tx: mpsc::SyncSender<Request>,
    pub state: SharedState,
    pub eva_receipts: EvaReceipts,
}

pub struct Worker {
    rx: mpsc::Receiver<Request>,
    state: SharedState,
    eva_receipts: EvaReceipts,
}

impl Controller {
    pub fn new() -> (Self, Worker) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(State::default()));
        let eva_receipts = Arc::new(Mutex::new(VecDeque::new()));
        (
            Self {
                tx,
                state: state.clone(),
                eva_receipts: eva_receipts.clone(),
            },
            Worker {
                rx,
                state,
                eva_receipts,
            },
        )
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub async fn submit(&self, action: Action) -> zbus::fdo::Result<()> {
        let (reply, result) = oneshot::channel();
        self.tx
            .try_send(Request {
                action,
                reply,
                deadline: Instant::now() + REQUEST_TIMEOUT,
            })
            .map_err(|error| {
                zbus::fdo::Error::Failed(match error {
                    mpsc::TrySendError::Full(_) => "player command queue is full".into(),
                    mpsc::TrySendError::Disconnected(_) => "player has exited".into(),
                })
            })?;
        tokio::time::timeout(REQUEST_TIMEOUT, result)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("player command timed out".into()))?
            .map_err(|_| zbus::fdo::Error::Failed("player has exited".into()))?
            .map_err(zbus::fdo::Error::Failed)
    }
}

impl Worker {
    pub(super) fn spawn_inner(
        self,
        initial: Open,
        headless: bool,
    ) -> oneshot::Receiver<Result<(), String>> {
        self.spawn_eva(initial, headless, None)
    }
    pub(super) fn spawn_eva(
        self,
        initial: Open,
        headless: bool,
        request: Option<Value>,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (done, result) = oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("jwm-player-ipc".into())
            .spawn(move || {
                let result = self.run(initial, headless, request);
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.alive = false;
                state.loaded = false;
                if let Err(error) = &result {
                    state.failure(error.clone());
                }
                drop(state);
                let _ = done.send(result);
            });
        result
    }

    fn run(&self, initial: Open, headless: bool, request: Option<Value>) -> Result<(), String> {
        let mut mpv = Mpv::spawn(self.state.clone(), initial.mode, headless)?;
        mpv.eva_receipts = self.eva_receipts.clone();
        for (id, property) in [
            "pause",
            "idle-active",
            "time-pos",
            "duration",
            "seekable",
            "volume",
            "mute",
            "fullscreen",
            "speed",
            "media-title",
            "metadata",
            "path",
            "playlist-pos",
            "playlist",
            "loop-file",
            "loop-playlist",
            "shuffle",
            "play-direction",
        ]
        .iter()
        .enumerate()
        {
            mpv.command(json!(["observe_property", id + 1, property]))?;
        }
        if let Some(request) = request {
            mpv.open_eva(initial, request, Instant::now() + REQUEST_TIMEOUT);
        } else {
            mpv.open(initial, Instant::now() + REQUEST_TIMEOUT)?;
        }
        let mut requested_quit = false;
        loop {
            // One request per iteration keeps streaming property events and
            // child exit handling live even under a sustained command burst.
            match self.rx.try_recv() {
                Ok(request) if request.reply.is_closed() || Instant::now() >= request.deadline => {
                    let _ = request
                        .reply
                        .send(Err("player request expired before execution".into()));
                }
                Ok(request) => {
                    let quit = matches!(request.action, Action::Quit);
                    let result = mpv.action(request.action, request.deadline);
                    let _ = request.reply.send(result);
                    if quit {
                        requested_quit = true;
                        break;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    requested_quit = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match mpv.next_message() {
                Ok(Some(message)) => mpv.event(&message),
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(format!("mpv IPC failed: {error}")),
            }
            if mpv.refresh_position {
                mpv.refresh_position = false;
                mpv.publish_seek_position();
            }
            if let Some(status) = mpv.child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("mpv exited with {status}"))
                };
            }
        }
        if requested_quit {
            return Ok(());
        }
        // EOF is also how a decoder crash arrives. Preserve its failure
        // status instead of silently treating every closed socket as Quit.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if let Some(status) = mpv.child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("mpv exited with {status}"))
                };
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err("mpv disconnected unexpectedly".into())
    }
}

struct Mpv {
    socket: UnixStream,
    child: Child,
    buffer: Vec<u8>,
    request_id: u64,
    state: SharedState,
    resume_index: usize,
    refresh_position: bool,
    seek_pending: bool,
    _ui: Option<super::ui::Script>,
    eva_receipts: EvaReceipts,
    last_open_command_id: Option<u64>,
}

impl Mpv {
    fn spawn(state: SharedState, mode: Mode, headless: bool) -> Result<Self, String> {
        let (socket, child_socket) = UnixStream::pair().map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_millis(25)))
            .map_err(|e| e.to_string())?;
        socket
            .set_write_timeout(Some(COMMAND_TIMEOUT))
            .map_err(|e| e.to_string())?;
        let child_fd: OwnedFd = child_socket.into();
        let ui =
            super::ui::Script::create().map_err(|e| format!("cannot prepare player UI: {e}"))?;
        let mut command = Command::new("mpv");
        command.env_remove(eva::ENV);
        command.args([
            "--no-config",
            "--no-terminal",
            "--input-terminal=no",
            "--idle=yes",
            "--input-ipc-client=fd://0",
            "--force-window=yes",
            "--keep-open=no",
            "--title=JWM Player${?media-title: — ${media-title}}",
            "--wayland-app-id=jwm-player",
            "--x11-name=jwm-player",
            "--volume-max=100",
            "--ytdl=no",
            "--load-scripts=no",
            "--access-references=no",
            "--load-unsafe-playlists=no",
            "--osc=no",
            "--osd-bar=no",
            "--border=no",
            "--hwdec=auto-safe",
            "--sub-auto=fuzzy",
            "--input-default-bindings=yes",
            "--input-vo-keyboard=yes",
        ]);
        command.arg(format!("--script={}", ui.path().display()));
        // Leave audio-display at mpv's version-specific default: old mpv
        // calls it `attachment`, newer mpv `embedded-first`. Both show art.
        command.arg(if mode == Mode::Music {
            "--geometry=560x420"
        } else {
            "--autofit=80%x80%"
        });
        if headless {
            command.args(["--ao=null", "--vo=null", "--force-window=no"]);
        }
        let child = command
            .stdin(Stdio::from(child_fd))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                format!("cannot start mpv: {error}. Install the mpv package to enable playback.")
            })?;
        Ok(Self {
            socket,
            child,
            buffer: Vec::new(),
            request_id: 0,
            state,
            resume_index: 0,
            refresh_position: false,
            seek_pending: false,
            _ui: Some(ui),
            eva_receipts: Arc::new(Mutex::new(VecDeque::new())),
            last_open_command_id: None,
        })
    }

    fn snapshot(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn event(&mut self, message: &Value) {
        match message["event"].as_str() {
            Some("seek") => self.seek_pending = true,
            Some("playback-restart") if self.seek_pending => {
                self.seek_pending = false;
                self.refresh_position = true;
            }
            Some("start-file" | "end-file") => {
                self.seek_pending = false;
                self.refresh_position = false;
            }
            _ => {}
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .event(message);
    }

    fn next_message(&mut self) -> std::io::Result<Option<Value>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=end).collect();
                if line.len() == 1 {
                    continue;
                }
                // mpv documents that tags can contain invalid UTF-8. Replace
                // those bytes without dropping the entire state connection.
                let text = String::from_utf8_lossy(&line);
                let message = serde_json::from_str(&text)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                return Ok(Some(message));
            }
            if self.buffer.len() >= FRAME_LIMIT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "mpv JSON frame exceeds 1 MiB",
                ));
            }
            let mut chunk = [0u8; 8192];
            let capacity = chunk.len().min(FRAME_LIMIT - self.buffer.len());
            match self.socket.read(&mut chunk[..capacity]) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "mpv closed IPC",
                    ));
                }
                Ok(length) => self.buffer.extend_from_slice(&chunk[..length]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn send(&mut self, command: Value) -> Result<u64, String> {
        self.request_id += 1;
        let mut frame =
            serde_json::to_vec(&json!({"command": command, "request_id": self.request_id}))
                .map_err(|e| e.to_string())?;
        frame.push(b'\n');
        self.socket
            .write_all(&frame)
            .map_err(|e| format!("cannot write mpv command: {e}"))?;
        Ok(self.request_id)
    }

    fn command(&mut self, command: Value) -> Result<Value, String> {
        let id = self.send(command)?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(message) = self.next_message().map_err(|e| e.to_string())? {
                if message["request_id"].as_u64() == Some(id) {
                    if message["error"] == "success" {
                        return Ok(message["data"].clone());
                    }
                    return Err(format!(
                        "mpv: {}",
                        message["error"].as_str().unwrap_or("command failed")
                    ));
                }
                self.event(&message);
            }
        }
        Err("mpv command timed out".into())
    }

    fn get(&mut self, property: &str) -> Result<Value, String> {
        let result = self.command(json!(["get_property", property]))?;
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .property(property, &result);
        Ok(result)
    }

    fn publish_seek_position(&mut self) {
        let track = self.snapshot().track_id;
        if let Ok(position) = self.command(json!(["get_property", "time-pos"])) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.track_id == track
                && state.loaded
                && super::model::microseconds(&position).is_some()
            {
                state.property("time-pos", &position);
                state.seek_serial = state.seek_serial.wrapping_add(1);
            }
        }
    }

    fn open(&mut self, open: Open, deadline: Instant) -> Result<(), String> {
        self.last_open_command_id = None;
        if open.files.is_empty() {
            return self.raise();
        }
        let count = if open.enqueue {
            self.get("playlist-count")?.as_u64().unwrap_or(0) as usize
        } else {
            0
        };
        if count.saturating_add(open.files.len()) > MAX_FILES {
            return Err(format!(
                "playlist limit is {MAX_FILES} files; replace the playlist to open more"
            ));
        }
        let existing_bytes = if open.enqueue {
            self.get("playlist")?
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["filename"].as_str())
                .map(str::len)
                .sum::<usize>()
        } else {
            0
        };
        if existing_bytes.saturating_add(open.files.iter().map(String::len).sum::<usize>())
            > MAX_INPUT_BYTES
        {
            return Err(
                "playlist paths exceed the 128 KiB limit; replace the playlist to open more".into(),
            );
        }
        let idle = self.get("idle-active")?.as_bool().unwrap_or(true);
        // Local normalization happens at every API boundary, not in a shell.
        // loadfile with just replace/append works before and after mpv 0.38.
        for (index, file) in open.files.iter().enumerate() {
            if Instant::now() >= deadline {
                return Err(
                    "opening the playlist timed out; some earlier files may already be queued"
                        .into(),
                );
            }
            let flag = if !open.enqueue && index == 0 {
                "replace"
            } else {
                "append"
            };
            self.command(json!(["loadfile", file, flag]))?;
            self.last_open_command_id = Some(self.request_id);
        }
        if !open.enqueue {
            self.resume_index = 0;
        }
        if open.enqueue && idle {
            self.command(json!(["set_property", "playlist-pos", count]))?;
            self.resume_index = count;
        }
        if !open.enqueue || idle {
            self.command(json!(["set_property", "pause", false]))?;
        }
        Ok(())
    }

    fn open_eva(&mut self, open: Open, request: Value, deadline: Instant) {
        if self
            .eva_receipts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|old| eva::same_operation(old, &request))
        {
            return;
        }
        self.last_open_command_id = None;
        let mut entry_id = 0;
        let result = (|| -> Result<Value, String> {
            eva::check_open(&request, &open)?;
            let before = self.get("playlist")?;
            let before_ids = before
                .as_array()
                .ok_or("mpv playlist unavailable")?
                .iter()
                .filter_map(|entry| entry["id"].as_u64())
                .collect::<std::collections::BTreeSet<_>>();
            let before_count = before.as_array().unwrap().len();
            let enqueue = open.enqueue;
            self.open(open, deadline)?;
            let after = self.get("playlist")?;
            let entries = after.as_array().ok_or("mpv playlist unavailable")?;
            let matching = entries
                .iter()
                .filter(|entry| {
                    entry["id"]
                        .as_u64()
                        .is_some_and(|id| id > 0 && !before_ids.contains(&id))
                        && entry["filename"].as_str().is_some_and(|path| {
                            super::cli::file_uri(path) == request["resource_uri"]
                        })
                })
                .collect::<Vec<_>>();
            if matching.len() != 1 || entries.len() != if enqueue { before_count + 1 } else { 1 } {
                return Err("ambiguous mpv playlist update".into());
            }
            entry_id = matching[0]["id"].as_u64().unwrap();
            if enqueue {
                return Ok(json!({"completion":"resource_queued_observed"}));
            }
            loop {
                let path_ready = match self.get("path") {
                    Ok(_) => true,
                    // loadfile's acknowledgement precedes file-loaded. A
                    // first player has no path until that asynchronous event.
                    Err(error) if error == "mpv: property unavailable" => false,
                    Err(error) => return Err(error),
                };
                self.get("pause")?;
                self.get("idle-active")?;
                let state = self.snapshot();
                if path_ready
                    && state.track_id == Some(entry_id)
                    && state.status() == "Playing"
                    && super::cli::file_uri(&state.path) == request["resource_uri"]
                {
                    return Ok(
                        json!({"status":state.status(),"track_url":super::cli::file_uri(&state.path),"track_id":state.track_path(),"completion":"resource_playing_observed"}),
                    );
                }
                if Instant::now() >= deadline {
                    return Err("mpv open postcondition timed out".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        let mut receipt = eva::receipt(
            &request,
            self.last_open_command_id.unwrap_or(0),
            entry_id,
            result.is_ok(),
            result.as_ref().err().map(|_| "unknown"),
        );
        if let Ok(extra) = result {
            for (key, value) in extra.as_object().unwrap() {
                receipt[key] = value.clone();
            }
        }
        let mut receipts = self
            .eva_receipts
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while receipts.len() >= eva::RECEIPT_CAPACITY {
            receipts.pop_front();
        }
        receipts.push_back(receipt);
    }

    fn raise(&mut self) -> Result<(), String> {
        // Wayland may refuse to unminimize a surface without an activation
        // token. This hint is best effort; media commands still succeed.
        let _ = self.command(json!(["set_property", "window-minimized", false]));
        self.command(json!(["show-text", "JWM Player", 1200]))
            .map(|_| ())
    }

    fn play(&mut self) -> Result<(), String> {
        let idle = self.get("idle-active")?.as_bool().unwrap_or(true);
        if idle {
            let count = self.get("playlist-count")?.as_u64().unwrap_or(0) as usize;
            if count == 0 {
                return Ok(());
            }
            self.command(json!([
                "set_property",
                "playlist-pos",
                self.resume_index.min(count - 1)
            ]))?;
        }
        self.command(json!(["set_property", "pause", false]))
            .map(|_| ())
    }

    fn action(&mut self, action: Action, deadline: Instant) -> Result<(), String> {
        match action {
            Action::Open(open) => self.open(open, deadline),
            Action::OpenEva { open, request } => {
                self.open_eva(open, request, deadline);
                Ok(())
            }
            Action::Play => self.play(),
            Action::Pause => self
                .command(json!(["set_property", "pause", true]))
                .map(|_| ()),
            Action::Toggle => {
                if self.get("idle-active")?.as_bool().unwrap_or(true) {
                    self.play()
                } else {
                    self.command(json!(["cycle", "pause"])).map(|_| ())
                }
            }
            Action::Stop => {
                if let Some(pos) = self.get("playlist-pos")?.as_u64() {
                    self.resume_index = pos as usize;
                }
                self.command(json!(["set_property", "playlist-pos", -1]))
                    .map(|_| ())
            }
            Action::Next => {
                let state = self.snapshot();
                if state.can_next() && state.playlist_pos == Some(state.playlist.len() - 1) {
                    self.command(json!(["set_property", "playlist-pos", 0]))
                        .map(|_| ())
                } else if state.can_next() {
                    self.command(json!(["playlist-next", "weak"])).map(|_| ())
                } else {
                    Ok(())
                }
            }
            Action::Previous => {
                let state = self.snapshot();
                if state.can_previous() && state.playlist_pos == Some(0) {
                    self.command(json!([
                        "set_property",
                        "playlist-pos",
                        state.playlist.len() - 1
                    ]))
                    .map(|_| ())
                } else if state.can_previous() {
                    self.command(json!(["playlist-prev", "weak"])).map(|_| ())
                } else {
                    Ok(())
                }
            }
            Action::Seek(offset) => {
                if !self.snapshot().seekable {
                    return Ok(());
                }
                self.command(json!(["seek", offset as f64 / 1_000_000.0, "relative"]))
                    .map(|_| ())
            }
            Action::SetPosition { track, position } => {
                let state = self.snapshot();
                if !state.seekable
                    || position < 0
                    || track != state.track_path()
                    || state
                        .duration_us
                        .is_some_and(|duration| position > duration)
                {
                    return Ok(());
                }
                self.command(json!([
                    "seek",
                    position as f64 / 1_000_000.0,
                    "absolute+exact"
                ]))
                .map(|_| ())
            }
            Action::Volume(volume) => {
                if !volume.is_finite() {
                    return Err("volume must be finite".into());
                }
                self.command(json!([
                    "set_property",
                    "volume",
                    volume.clamp(0.0, 1.0) * 100.0
                ]))
                .map(|_| ())
            }
            Action::Rate(rate) => {
                if !rate.is_finite() || !(0.25..=4.0).contains(&rate.abs()) {
                    return Err("rate magnitude must be between 0.25 and 4".into());
                }
                let state = self.snapshot();
                if rate < 0.0 && state.loaded && !state.seekable {
                    return Err("reverse playback requires a seekable file".into());
                }
                self.command(json!([
                    "set_property",
                    "play-direction",
                    if rate < 0.0 { "backward" } else { "forward" }
                ]))?;
                self.command(json!(["set_property", "speed", rate.abs()]))
                    .map(|_| ())
            }
            Action::Shuffle(enabled) => {
                let already = self.get("shuffle")?.as_bool().unwrap_or(false);
                if enabled != already {
                    self.command(json!([if enabled {
                        "playlist-shuffle"
                    } else {
                        "playlist-unshuffle"
                    }]))?;
                    self.command(json!(["set_property", "shuffle", enabled]))?;
                }
                Ok(())
            }
            Action::Loop(status) => {
                let (file, playlist) = match status.as_str() {
                    "None" => ("no", "no"),
                    "Track" => ("inf", "no"),
                    "Playlist" => ("no", "inf"),
                    _ => return Err("LoopStatus must be None, Track, or Playlist".into()),
                };
                self.command(json!(["set_property", "loop-file", file]))?;
                self.command(json!(["set_property", "loop-playlist", playlist]))
                    .map(|_| ())
            }
            Action::Fullscreen(fullscreen) => self
                .command(json!(["set_property", "fullscreen", fullscreen]))
                .map(|_| ()),
            Action::Raise => self.raise(),
            Action::Quit => self.send(json!(["quit"])).map(|_| ()),
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        let _ = self.send(json!(["quit"]));
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return,
            }
        }
        // A process stuck in uninterruptible kernel I/O cannot be reaped
        // synchronously. The helper is exiting and init will reap it later;
        // do not turn player shutdown into an unbounded waitpid.
        log::warn!("mpv did not exit after SIGKILL; leaving final reap to the process supervisor");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragmented_events_and_unrelated_replies_do_not_confuse_command_reply() {
        use std::io::{BufRead, BufReader};
        let (socket, peer) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        socket.set_write_timeout(Some(COMMAND_TIMEOUT)).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let mut mpv = Mpv {
            socket,
            child: Command::new("true").spawn().unwrap(),
            buffer: Vec::new(),
            request_id: 0,
            state,
            resume_index: 0,
            refresh_position: false,
            seek_pending: false,
            _ui: None,
            eva_receipts: Arc::new(Mutex::new(VecDeque::new())),
            last_open_command_id: None,
        };
        let filename = "/tmp/音乐 \"q\"\n${title}.flac";
        let server = std::thread::spawn(move || {
            let mut peer = BufReader::new(peer);
            let mut line = String::new();
            peer.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["command"][1], filename);
            peer.get_mut().write_all(b"{\"event\":\"property-").unwrap();
            std::thread::sleep(Duration::from_millis(30));
            peer.get_mut().write_all(b"change\",\"name\":\"pause\",\"data\":true}\n{\"request_id\":999,\"error\":\"success\"}\n").unwrap();
            writeln!(
                peer.get_mut(),
                "{}",
                json!({"request_id":request["request_id"],"error":"success","data":17})
            )
            .unwrap();
        });
        assert_eq!(
            mpv.command(json!(["loadfile", filename, "replace"]))
                .unwrap(),
            17
        );
        assert!(mpv.snapshot().paused);
        server.join().unwrap();
    }

    #[test]
    fn oversized_unterminated_ipc_frame_is_rejected() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let mut mpv = Mpv {
            socket,
            child: Command::new("true").spawn().unwrap(),
            buffer: vec![b'x'; FRAME_LIMIT],
            request_id: 0,
            state: Arc::new(Mutex::new(State::default())),
            resume_index: 0,
            refresh_position: false,
            seek_pending: false,
            _ui: None,
            eva_receipts: Arc::new(Mutex::new(VecDeque::new())),
            last_open_command_id: None,
        };
        assert_eq!(
            mpv.next_message().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn unknown_native_open_is_recorded_once_without_replaying_loadfile() {
        use std::io::{BufRead, BufReader};
        let (socket, peer) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut mpv = Mpv {
            socket,
            child: Command::new("true").spawn().unwrap(),
            buffer: Vec::new(),
            request_id: 0,
            state: Arc::new(Mutex::new(State::default())),
            resume_index: 0,
            refresh_position: false,
            seek_pending: false,
            _ui: None,
            eva_receipts: Arc::new(Mutex::new(VecDeque::new())),
            last_open_command_id: None,
        };
        let server = std::thread::spawn(move || {
            let mut peer = BufReader::new(peer);
            let mut loads = 0;
            loop {
                let mut line = String::new();
                if peer.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let command = &request["command"];
                let data = match command[1].as_str() {
                    Some("playlist") => json!([]),
                    Some("playlist-count") => json!(0),
                    Some("idle-active") => json!(false),
                    _ => Value::Null,
                };
                if command[0] == "loadfile" {
                    loads += 1;
                    // The mutation was received, then its reply disappeared.
                    break;
                }
                writeln!(
                    peer.get_mut(),
                    "{}",
                    json!({"request_id":request["request_id"],"error":"success","data":data})
                )
                .unwrap();
            }
            loads
        });
        let open = Open {
            files: vec!["/tmp/one.wav".into()],
            enqueue: true,
            mode: Mode::Music,
        };
        let request=eva::parse(r#"{"operation_id":"a","wm_instance_id":"wm","sequence":1,"resource_uri":"file:///tmp/one.wav","enqueue":true,"target_player":"jwm","target_bus_owner":null,"target_pid":null}"#).unwrap();
        mpv.open_eva(
            open.clone(),
            request.clone(),
            Instant::now() + REQUEST_TIMEOUT,
        );
        let receipt = mpv.eva_receipts.lock().unwrap()[0].clone();
        assert_eq!(receipt["verified"], false);
        assert_eq!(receipt["error"], "unknown");
        assert_eq!(receipt["mpv_command_ack"], false);
        mpv.open_eva(open, request, Instant::now() + REQUEST_TIMEOUT);
        assert_eq!(mpv.eva_receipts.lock().unwrap().len(), 1);
        // Close the fake IPC before joining; Mpv's drop may attempt Quit but
        // must never issue another resource load.
        let _ = mpv.socket.shutdown(std::net::Shutdown::Both);
        assert_eq!(server.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn stopped_worker_and_full_queue_fail_without_blocking() {
        let (controller, worker) = Controller::new();
        drop(worker);
        assert!(controller.submit(Action::Play).await.is_err());
        let (controller, _worker) = Controller::new();
        for _ in 0..QUEUE_CAPACITY {
            let (reply, _result) = oneshot::channel();
            controller
                .tx
                .try_send(Request {
                    action: Action::Play,
                    reply,
                    deadline: Instant::now(),
                })
                .unwrap();
        }
        assert!(
            controller
                .submit(Action::Play)
                .await
                .unwrap_err()
                .to_string()
                .contains("queue is full")
        );
    }

    // An opt-in integration gate: RUN_JWM_MPV_TESTS=1 enables the real decoder
    // with null audio/video sinks, so tests never play sound or open a window.
    #[tokio::test]
    async fn real_mpv_load_pause_seek_stop_resume_and_video() {
        if std::env::var_os("RUN_JWM_MPV_TESTS").is_none() {
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("jwm-player-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let wave = directory.join("歌曲 \"quoted\" ${literal}.wav");
        let video = directory.join("video.mp4");
        for (path, source, video_mode) in [
            (&wave, "sine=frequency=440:duration=4", false),
            (&video, "testsrc=size=64x64:rate=10:duration=4", true),
        ] {
            let mut command = Command::new("ffmpeg");
            command.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                source,
            ]);
            if video_mode {
                command.args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p"]);
            }
            assert!(command.arg(path).status().unwrap().success());
        }
        let (controller, worker) = Controller::new();
        let done = worker.spawn_inner(
            Open {
                files: vec![
                    wave.to_str().unwrap().into(),
                    video.to_str().unwrap().into(),
                ],
                enqueue: false,
                mode: Mode::Auto,
            },
            true,
        );
        wait_for(&controller, |s| {
            s.status() == "Playing" && s.position_us > 50_000 && s.playlist.len() == 2
        })
        .await;
        controller.submit(Action::Pause).await.unwrap();
        wait_for(&controller, |s| s.status() == "Paused").await;
        let track = controller.snapshot().track_path();
        controller
            .submit(Action::SetPosition {
                track,
                position: 1_500_000,
            })
            .await
            .unwrap();
        // A paused audio-only clock includes the sink's buffered samples.
        // Allow its documented/device-dependent latency while proving a
        // large seek actually occurred, rather than asserting a fake clock.
        wait_for(&controller, |s| {
            s.seek_serial > 0 && (1_200_000..=1_700_000).contains(&s.position_us)
        })
        .await;
        let before_stale_seek = controller.snapshot().seek_serial;
        controller
            .submit(Action::SetPosition {
                track: "/org/jwm/Player/track_9999".into(),
                position: 3_500_000,
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.snapshot().seek_serial, before_stale_seek);
        assert!(controller.snapshot().position_us < 2_000_000);
        controller.submit(Action::Volume(0.42)).await.unwrap();
        wait_for(&controller, |s| (s.volume - 0.42).abs() < 0.001).await;
        controller
            .submit(Action::Loop("Playlist".into()))
            .await
            .unwrap();
        wait_for(&controller, |s| s.loop_status() == "Playlist").await;
        controller.submit(Action::Stop).await.unwrap();
        wait_for(&controller, |s| s.status() == "Stopped").await;
        assert_eq!(controller.snapshot().playlist.len(), 2);
        controller.submit(Action::Play).await.unwrap();
        wait_for(&controller, |s| s.status() == "Playing").await;
        controller.submit(Action::Next).await.unwrap();
        wait_for(&controller, |s| {
            s.path.ends_with("video.mp4") && s.status() == "Playing" && s.position_us > 50_000
        })
        .await;
        controller.submit(Action::Next).await.unwrap();
        wait_for(&controller, |s| {
            s.path.ends_with(".wav") && s.status() == "Playing"
        })
        .await;
        controller.submit(Action::Previous).await.unwrap();
        wait_for(&controller, |s| {
            s.path.ends_with("video.mp4") && s.status() == "Playing"
        })
        .await;
        controller
            .submit(Action::Loop("Track".into()))
            .await
            .unwrap();
        wait_for(&controller, |s| s.loop_status() == "Track").await;
        controller
            .submit(Action::SetPosition {
                track: controller.snapshot().track_path(),
                position: 3_400_000,
            })
            .await
            .unwrap();
        wait_for(&controller, |s| s.position_us > 3_100_000).await;
        wait_for(&controller, |s| {
            s.position_us < 1_000_000 && s.status() == "Playing"
        })
        .await;
        controller.submit(Action::Pause).await.unwrap();
        wait_for(&controller, |s| s.status() == "Paused").await;
        controller
            .submit(Action::SetPosition {
                track: controller.snapshot().track_path(),
                position: 2_500_000,
            })
            .await
            .unwrap();
        wait_for(&controller, |s| s.position_us > 2_200_000).await;
        controller.submit(Action::Rate(-1.0)).await.unwrap();
        wait_for(&controller, |s| s.playback_rate() == -1.0).await;
        controller.submit(Action::Play).await.unwrap();
        wait_for(&controller, |s| {
            s.status() == "Playing" && s.position_us < 1_900_000
        })
        .await;
        controller.submit(Action::Rate(1.0)).await.unwrap();
        wait_for(&controller, |s| s.playback_rate() == 1.0).await;
        controller.submit(Action::Shuffle(true)).await.unwrap();
        wait_for(&controller, |s| s.shuffle).await;
        controller.submit(Action::Shuffle(false)).await.unwrap();
        wait_for(&controller, |s| !s.shuffle).await;
        controller.submit(Action::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(4), done)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!controller.snapshot().alive);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn real_mpv_common_audio_containers_and_wave_encodings() {
        if std::env::var_os("RUN_JWM_MPV_TESTS").is_none() {
            return;
        }
        let directory =
            std::env::temp_dir().join(format!("jwm-audio-formats-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut files = Vec::new();
        for (name, codec, options) in [
            ("pcm16.wav", "pcm_s16le", vec![]),
            ("pcm24.wav", "pcm_s24le", vec![]),
            ("pcm32.wav", "pcm_s32le", vec![]),
            ("float32.wav", "pcm_f32le", vec![]),
            ("rf64.wav", "pcm_s16le", vec!["-rf64", "always"]),
            ("wave64.w64", "pcm_s16le", vec![]),
            ("lossless.flac", "flac", vec![]),
            ("music.mp3", "libmp3lame", vec![]),
            ("music.mp2", "mp2", vec![]),
            ("music.aac", "aac", vec![]),
            ("music.m4a", "aac", vec![]),
            ("apple-lossless.m4a", "alac", vec![]),
            ("music.ogg", "libvorbis", vec![]),
            ("music.opus", "libopus", vec![]),
            ("music.wv", "wavpack", vec![]),
            ("music.wma", "wmav2", vec![]),
            ("music.aiff", "pcm_s16be", vec![]),
            ("music.au", "pcm_mulaw", vec![]),
            ("music.caf", "pcm_s16le", vec![]),
            ("music.tta", "tta", vec![]),
            ("music.ac3", "ac3", vec![]),
            ("music.eac3", "eac3", vec![]),
            ("music.mka", "flac", vec![]),
        ] {
            let path = directory.join(name);
            let output = Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=2",
                    "-c:a",
                    codec,
                ])
                .args(options)
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "generate {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            files.push(path);
        }
        // Content probing must also work for a filename with no recognized suffix.
        for name in ["recording", "recording.unknown"] {
            let path = directory.join(name);
            std::fs::copy(&files[0], &path).unwrap();
            files.push(path);
        }
        let (controller, worker) = Controller::new();
        let done = worker.spawn_inner(
            Open {
                files: vec![],
                enqueue: false,
                mode: Mode::Music,
            },
            true,
        );
        for path in &files {
            let path = path.to_str().unwrap();
            controller
                .submit(Action::Open(Open {
                    files: vec![path.to_owned()],
                    enqueue: false,
                    mode: Mode::Music,
                }))
                .await
                .unwrap();
            wait_for(&controller, |state| {
                state.path == path
                    && state.status() == "Playing"
                    && state.position_us > 0
                    && state.duration_us.is_some_and(|duration| duration > 0)
            })
            .await;
        }
        controller.submit(Action::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(4), done)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn real_mpv_crash_reaps_child_and_removes_ui() {
        if std::env::var_os("RUN_JWM_MPV_TESTS").is_none() {
            return;
        }
        let mut mpv =
            Mpv::spawn(Arc::new(Mutex::new(State::default())), Mode::Video, true).unwrap();
        mpv.command(json!(["get_property", "mpv-version"])).unwrap();
        let script = mpv._ui.as_ref().unwrap().path().to_owned();
        assert!(script.exists());
        mpv.child.kill().unwrap();
        let status = mpv.child.wait().unwrap();
        assert!(!status.success());
        assert!(mpv.command(json!(["get_property", "pause"])).is_err());
        let started = Instant::now();
        drop(mpv);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!script.exists());
    }

    async fn wait_for(controller: &Controller, predicate: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            let state = controller.snapshot();
            if predicate(&state) {
                return;
            }
            assert!(state.alive, "player exited: {:?}", state.error);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("state transition timed out: {:?}", controller.snapshot());
    }
}
