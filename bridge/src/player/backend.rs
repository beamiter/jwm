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
use super::model::State;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_LIMIT: usize = 1024 * 1024;
const QUEUE_CAPACITY: usize = 32;
pub type SharedState = Arc<Mutex<State>>;

#[derive(Debug)]
pub enum Action {
    Open(Open),
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
}

pub struct Worker {
    rx: mpsc::Receiver<Request>,
    state: SharedState,
}

impl Controller {
    pub fn new() -> (Self, Worker) {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(State::default()));
        (
            Self {
                tx,
                state: state.clone(),
            },
            Worker { rx, state },
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
        let (done, result) = oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("jwm-player-ipc".into())
            .spawn(move || {
                let result = self.run(initial, headless);
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

    fn run(&self, initial: Open, headless: bool) -> Result<(), String> {
        let mut mpv = Mpv::spawn(self.state.clone(), initial.mode, headless)?;
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
        ]
        .iter()
        .enumerate()
        {
            mpv.command(json!(["observe_property", id + 1, property]))?;
        }
        mpv.open(initial, Instant::now() + REQUEST_TIMEOUT)?;
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
                if self.snapshot().can_next() {
                    self.command(json!(["playlist-next", "weak"])).map(|_| ())
                } else {
                    Ok(())
                }
            }
            Action::Previous => {
                if self.snapshot().can_previous() {
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
                if !rate.is_finite() || !(0.25..=4.0).contains(&rate) {
                    return Err("rate must be between 0.25 and 4".into());
                }
                self.command(json!(["set_property", "speed", rate]))
                    .map(|_| ())
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
        };
        assert_eq!(
            mpv.next_message().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
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
        controller.submit(Action::Quit).await.unwrap();
        tokio::time::timeout(Duration::from_secs(4), done)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!controller.snapshot().alive);
        let _ = std::fs::remove_dir_all(directory);
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
