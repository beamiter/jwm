//! Built-in microphone recorder.
//!
//! PCM WAV can be captured directly through ALSA. Compressed formats use an
//! ffmpeg worker while retaining ALSA as the Linux capture source.

#[cfg(feature = "media-audio")]
use alsa::pcm::{Access, Format, HwParams, PCM};
#[cfg(feature = "media-audio")]
use alsa::{Direction, ValueOr};
#[cfg(feature = "media-audio")]
use std::fs::{File, OpenOptions};
#[cfg(feature = "media-audio")]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(feature = "media-audio")]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(feature = "media-audio")]
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "media-audio")]
use std::sync::mpsc;
#[cfg(feature = "media-audio")]
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct AudioWorker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<(), String>>,
}

/// A recorder thread that missed its initialization deadline.
///
/// It has been told to stop but is never joined on the caller's thread: the
/// capture device can block inside its open call indefinitely (a sound-server
/// ALSA plugin waiting on a wedged daemon), and `start()` runs on the event
/// thread, so joining it there would turn the deadline into a compositor
/// freeze. `refresh()` reaps it once it returns, and no new recorder starts
/// meanwhile so two workers never contend for the capture device.
struct AbandonedWorker {
    handle: JoinHandle<Result<(), String>>,
    output_path: PathBuf,
}

/// A recorder told to stop whose container is still being finalized.
///
/// The key toggle stops this way: the direct recorder rewrites its WAV header
/// and syncs the file, and ffmpeg gets a grace period to flush its trailer,
/// so joining at once would stall the event thread for that long (ffmpeg's
/// is `FFMPEG_STOP_GRACE`). The frame tick collects the
/// outcome through [`AudioRecordingState::poll_finalized`] once the thread
/// has returned.
struct FinalizingWorker {
    handle: JoinHandle<Result<(), String>>,
    output_path: Option<String>,
}

/// A recording whose finalization finished off the event thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedRecording {
    /// Where the recording was written, as `output_path` said when it
    /// stopped.
    pub output_path: Option<String>,
    /// The recorder's own outcome: an error means the file is unfinalized.
    pub outcome: Result<(), String>,
}

/// Size of the RIFF/WAVE header the direct recorder writes before capturing;
/// an output no larger than this holds no audio.
const WAV_HEADER_LEN: u64 = 44;

/// What the recorder thread sends once it is capturing: the negotiated
/// sample rate and channel count, or why it could not start.
#[cfg(feature = "media-audio")]
type ReadyMessage = Result<(u32, u16), String>;

/// How long `start()` waits for the recorder to report that it is capturing.
#[cfg(feature = "media-audio")]
const READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Map a joined recorder thread to the outcome callers report.
fn join_outcome(handle: JoinHandle<Result<(), String>>) -> Result<(), String> {
    handle
        .join()
        .unwrap_or_else(|_| Err("audio recorder thread panicked".into()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioBackend {
    Direct,
    Ffmpeg,
}

impl AudioBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Ffmpeg => "ffmpeg",
        }
    }
}

fn select_backend(configured: &str, format: &str) -> Result<AudioBackend, String> {
    match configured {
        "direct" if format == "wav" => Ok(AudioBackend::Direct),
        "direct" => Err(format!(
            "direct audio backend only supports wav, not {format}"
        )),
        "ffmpeg" => Ok(AudioBackend::Ffmpeg),
        "auto" | "" if format == "wav" => Ok(AudioBackend::Direct),
        "auto" | "" => Ok(AudioBackend::Ffmpeg),
        value => Err(format!(
            "unknown audio recording backend '{value}' (expected auto, direct, or ffmpeg)"
        )),
    }
}

fn output_format(path: &Path) -> Result<&str, String> {
    let format = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    match format {
        "wav" | "flac" | "opus" | "mp3" => Ok(format),
        _ => Err("audio recording path must end in .wav, .flac, .opus, or .mp3".into()),
    }
}

#[cfg(feature = "media-audio")]
const MAX_FFMPEG_LOG_DETAIL_BYTES: usize = 64 * 1024;
#[cfg(feature = "media-audio")]
const FFMPEG_STOP_GRACE: Duration = Duration::from_secs(2);

/// Own an ffmpeg child across every fallible polling path.
///
/// `std::process::Child` does not terminate or reap on drop. Without this
/// guard, an unexpected `try_wait`/wait error could return from the recorder
/// worker while ffmpeg kept capturing with nobody left to stop it.
#[cfg(feature = "media-audio")]
struct FfmpegChild {
    child: Child,
}

#[cfg(feature = "media-audio")]
impl FfmpegChild {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn as_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

#[cfg(feature = "media-audio")]
impl Drop for FfmpegChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "media-audio")]
struct FfmpegLog {
    path: std::path::PathBuf,
}

#[cfg(feature = "media-audio")]
impl FfmpegLog {
    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(feature = "media-audio")]
impl Drop for FfmpegLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(feature = "media-audio")]
fn create_ffmpeg_log() -> std::io::Result<(FfmpegLog, File)> {
    for _ in 0..16 {
        let nonce = rand::random::<u64>();
        let path = std::env::temp_dir().join(format!(
            "jwm-audio-recording-ffmpeg-{}-{nonce:016x}.log",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                let log = FfmpegLog { path };
                // A restrictive umask may remove owner bits. Restore the
                // exact private mode so the parent can reopen the log tail.
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                return Ok((log, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique ffmpeg log file",
    ))
}

#[cfg(feature = "media-audio")]
fn read_ffmpeg_log_tail(path: &Path) -> String {
    read_ffmpeg_log_tail_with_limit(path, MAX_FFMPEG_LOG_DETAIL_BYTES)
}

#[cfg(feature = "media-audio")]
fn read_ffmpeg_log_tail_with_limit(path: &Path, limit: usize) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let start = length.saturating_sub(limit as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity(limit.min(length as usize));
    if file.take(limit as u64).read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let detail = String::from_utf8_lossy(&bytes).trim().to_string();
    if start > 0 && !detail.is_empty() {
        format!("[earlier ffmpeg output omitted] {detail}")
    } else {
        detail
    }
}

#[cfg(feature = "media-audio")]
fn wait_for_child_with_grace(child: &mut Child, grace: Duration) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(20)),
        );
    }

    // The child may exit between the final try_wait and kill. InvalidInput in
    // that race still permits wait() to reap the already-finished process.
    if let Err(error) = child.kill()
        && error.kind() != std::io::ErrorKind::InvalidInput
    {
        return Err(error);
    }
    child.wait()
}

/// Runtime state for the built-in audio recorder.
#[derive(Default)]
pub struct AudioRecordingState {
    pub active: bool,
    pub output_path: Option<String>,
    pub started_at: Option<Instant>,
    pub sample_rate: u32,
    pub channels: u16,
    pub device: String,
    pub backend: String,
    pub format: String,
    pub last_error: Option<String>,
    worker: Option<AudioWorker>,
    /// Outcome of a worker that ended on its own, held for the next `stop()`.
    exited: Option<Result<(), String>>,
    abandoned: Option<AbandonedWorker>,
    /// A recorder `begin_stop()` asked to stop, until its thread returns.
    finalizing: Option<FinalizingWorker>,
}

impl std::fmt::Debug for AudioRecordingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRecordingState")
            .field("active", &self.active)
            .field("output_path", &self.output_path)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("device", &self.device)
            .field("backend", &self.backend)
            .field("format", &self.format)
            .field("last_error", &self.last_error)
            .field("exit_pending", &self.exited.is_some())
            .field("abandoned_worker", &self.abandoned.is_some())
            .field("finalizing", &self.finalizing.is_some())
            .finish()
    }
}

impl AudioRecordingState {
    /// Without the `media-audio` feature the capture engines are not
    /// compiled; starting reports that plainly instead of failing deeper in.
    #[cfg(not(feature = "media-audio"))]
    #[allow(clippy::unused_self, clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        _output_path: &Path,
        _device: &str,
        _sample_rate: u32,
        _channels: u16,
        _configured_backend: &str,
        _bitrate: &str,
    ) -> Result<(), String> {
        Err(
            "audio capture is not compiled into this jwm binary (rebuild with \
             --features media-audio)"
                .into(),
        )
    }

    #[cfg(feature = "media-audio")]
    pub fn start(
        &mut self,
        output_path: &Path,
        device: &str,
        sample_rate: u32,
        channels: u16,
        configured_backend: &str,
        bitrate: &str,
    ) -> Result<(), String> {
        self.check_can_start()?;
        if !output_path.is_absolute() {
            return Err("audio recording output path must be absolute".into());
        }
        let format = output_format(output_path)?;
        let backend = select_backend(configured_backend, format)?;
        if output_path.exists() {
            return Err(format!(
                "audio recording output already exists: {}",
                output_path.display()
            ));
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }

        let path = output_path.to_path_buf();
        let device_name = if device.trim().is_empty() {
            "default".to_string()
        } else {
            device.to_string()
        };
        let thread_device = device_name.clone();
        let thread_format = format.to_string();
        let thread_bitrate = bitrate.to_string();
        let (worker, (actual_rate, actual_channels)) = self.spawn_worker(
            output_path,
            READY_TIMEOUT,
            move |stop, ready| match backend {
                AudioBackend::Direct => {
                    capture_to_wav(&path, &thread_device, sample_rate, channels, stop, ready)
                }
                AudioBackend::Ffmpeg => capture_with_ffmpeg(
                    &path,
                    &thread_device,
                    sample_rate,
                    channels,
                    &thread_format,
                    &thread_bitrate,
                    stop,
                    ready,
                ),
            },
        )?;

        self.active = true;
        self.output_path = Some(output_path.to_string_lossy().into_owned());
        self.started_at = Some(Instant::now());
        self.sample_rate = actual_rate;
        self.channels = actual_channels;
        self.device = device_name;
        self.backend = backend.as_str().to_string();
        self.format = format.to_string();
        self.last_error = None;
        // Starting anew acknowledges a session whose worker died unobserved,
        // as an explicit IPC start always has.
        self.exited = None;
        self.worker = Some(worker);
        Ok(())
    }

    /// Refuse a second recorder while an earlier one may still hold the
    /// capture device.
    #[cfg(feature = "media-audio")]
    fn check_can_start(&mut self) -> Result<(), String> {
        self.refresh();
        // Refused even once its thread returned: the outcome is still waiting
        // for `poll_finalized()`, and the next stop would park a new worker
        // over it. The frame tick collects it within one maintenance tick.
        if self.finalizing.is_some() {
            return Err(
                "the previous audio recording is still being finalized; try again in a moment"
                    .into(),
            );
        }
        if self.abandoned.is_some() {
            return Err(
                "the previous audio recorder is still stuck opening the capture \
                        device; try again once it responds"
                    .into(),
            );
        }
        if self.worker.is_some() {
            return Err("audio recording is already active".into());
        }
        Ok(())
    }

    /// Spawn the recorder thread and wait up to `timeout` for its ready
    /// message.
    ///
    /// The worker is joined only when it is known to be returning. One that
    /// misses the deadline is told to stop and parked in `abandoned` instead,
    /// because it may never come back from opening the device.
    #[cfg(feature = "media-audio")]
    fn spawn_worker<F>(
        &mut self,
        output_path: &Path,
        timeout: Duration,
        body: F,
    ) -> Result<(AudioWorker, (u32, u16)), String>
    where
        F: FnOnce(Arc<AtomicBool>, mpsc::SyncSender<ReadyMessage>) -> Result<(), String>
            + Send
            + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("jwm-audio-recorder".into())
            .spawn(move || body(thread_stop, ready_tx))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(format)) => Ok((AudioWorker { stop, handle }, format)),
            // The worker reported its own failure and is returning.
            Ok(Err(error)) => {
                let _ = handle.join();
                Err(error)
            }
            // The worker dropped its sender, so it has already returned; its
            // result names the reason better than the closed channel does.
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(match join_outcome(handle) {
                Ok(()) => "audio recorder exited during initialization".into(),
                Err(error) => error,
            }),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Release);
                self.abandoned = Some(AbandonedWorker {
                    handle,
                    output_path: output_path.to_path_buf(),
                });
                Err(format!(
                    "audio recorder did not initialize within {timeout:?}"
                ))
            }
        }
    }

    /// Stop and finalize the active container, waiting for the recorder to
    /// return. Safe to call more than once.
    ///
    /// A worker that already ended on its own is reported here: its latched
    /// error becomes this call's error, so the stop the user asked for says
    /// that the recording failed. A recording [`Self::begin_stop`] left
    /// finalizing is waited for too, so shutdown and the screen recorder's
    /// microphone hand-off never leave a half-written file or a held device.
    pub fn stop(&mut self) -> Result<(), String> {
        if let Some(finalizing) = self.finalizing.take() {
            self.active = false;
            let outcome = join_outcome(finalizing.handle);
            if let Err(error) = &outcome {
                self.last_error = Some(error.clone());
            }
            return outcome;
        }
        if let Some(outcome) = self.exited.take() {
            self.active = false;
            return outcome;
        }
        let Some(worker) = self.worker.take() else {
            self.active = false;
            return Ok(());
        };
        worker.stop.store(true, Ordering::Release);
        // The thread is gone either way once joined, panicked or not.
        self.active = false;
        let result = join_outcome(worker.handle);
        if let Err(error) = &result {
            self.last_error = Some(error.clone());
        }
        result
    }

    /// Ask the recorder to stop without waiting for it to finalize.
    ///
    /// The microphone session ends here — `active` clears so the MIC chip,
    /// the idle inhibit and `has_active_feature` drop at once — while the
    /// thread flushes its container. Returns the outcome at once when there
    /// is nothing left to wait for (no recorder, or one that already ended on
    /// its own); `None` means the recorder is finalizing and
    /// [`Self::poll_finalized`] reports it once it returns.
    pub fn begin_stop(&mut self) -> Option<Result<(), String>> {
        if let Some(outcome) = self.exited.take() {
            self.active = false;
            return Some(outcome);
        }
        let Some(worker) = self.worker.take() else {
            self.active = false;
            return Some(Ok(()));
        };
        worker.stop.store(true, Ordering::Release);
        self.active = false;
        self.finalizing = Some(FinalizingWorker {
            handle: worker.handle,
            output_path: self.output_path.clone(),
        });
        None
    }

    /// Whether a recording [`Self::begin_stop`] ended is still finalizing.
    #[must_use]
    pub fn is_finalizing(&self) -> bool {
        self.finalizing.is_some()
    }

    /// The outcome of a finalizing recording, once its thread has returned.
    /// Never blocks: an unfinished recorder reads as `None` and is asked
    /// again on the next tick.
    pub fn poll_finalized(&mut self) -> Option<FinalizedRecording> {
        if !self
            .finalizing
            .as_ref()
            .is_some_and(|finalizing| finalizing.handle.is_finished())
        {
            return None;
        }
        let finalizing = self.finalizing.take()?;
        let outcome = join_outcome(finalizing.handle);
        if let Err(error) = &outcome {
            self.last_error = Some(error.clone());
        }
        Some(FinalizedRecording {
            output_path: finalizing.output_path,
            outcome,
        })
    }

    /// Observe a device/file error which ended the worker unexpectedly, and
    /// reap a recorder abandoned during initialization once it has returned.
    ///
    /// A worker that ended on its own leaves the session open: `active` stays
    /// true and the outcome is latched for the next `stop()`. The user still
    /// believes the microphone is recording, so the next toggle has to take
    /// the stop path (clearing the MIC chip and reporting the failure) rather
    /// than read "inactive" and open the microphone again. Returns true while
    /// such an exit awaits `stop()`, so a periodic caller can finalize it.
    pub fn refresh(&mut self) -> bool {
        self.reap_abandoned();
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.handle.is_finished())
            && let Some(worker) = self.worker.take()
        {
            let outcome = join_outcome(worker.handle);
            if let Err(error) = &outcome {
                log::warn!("[audio-recording] recorder stopped on its own: {error}");
                self.last_error = Some(error.clone());
            }
            self.exited = Some(outcome);
        }
        self.exited.is_some()
    }

    fn reap_abandoned(&mut self) {
        if !self
            .abandoned
            .as_ref()
            .is_some_and(|abandoned| abandoned.handle.is_finished())
        {
            return;
        }
        let Some(abandoned) = self.abandoned.take() else {
            return;
        };
        if let Err(error) = join_outcome(abandoned.handle) {
            log::debug!("[audio-recording] abandoned recorder exited: {error}");
        }
        // A device that finally opened after start() gave up leaves at most a
        // header behind: start() already reported that recording as failed.
        if std::fs::metadata(&abandoned.output_path)
            .is_ok_and(|metadata| metadata.len() <= WAV_HEADER_LEN)
        {
            let _ = std::fs::remove_file(&abandoned.output_path);
        }
    }

    /// An active recording whose recorder thread runs `body` with the stop
    /// flag, for policy tests that drive the stop and tick paths without a
    /// capture device. `body` stands in for the capture loop: it returns
    /// when asked to stop (or on its own, to model a device that vanished).
    #[cfg(test)]
    pub(crate) fn recording_for_test(
        output_path: &str,
        body: impl FnOnce(Arc<AtomicBool>) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || body(thread_stop));
        let mut state = Self::default();
        state.active = true;
        state.output_path = Some(output_path.to_string());
        state.started_at = Some(Instant::now());
        state.worker = Some(AudioWorker { stop, handle });
        state
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at
            .map_or(Duration::ZERO, |start| start.elapsed())
    }
}

/// The recorder process: `program` with `args`, stdin piped, stdout
/// discarded, stderr to `stderr`.
///
/// It is started on the recorder thread, which inherits the SIGCHLD block
/// JWM keeps for the event loop's signalfd, and std would hand the recorder
/// that mask; the child starts with SIGCHLD unblocked.
#[cfg(feature = "media-audio")]
fn recorder_command(program: &str, args: &[String], stderr: Stdio) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr);
    crate::external_command::unblock_sigchld_in_child(&mut command);
    command
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "media-audio")]
fn capture_with_ffmpeg(
    path: &Path,
    device: &str,
    requested_rate: u32,
    requested_channels: u16,
    format: &str,
    bitrate: &str,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<(u32, u16), String>>,
) -> Result<(), String> {
    let rate = requested_rate.clamp(8_000, 192_000);
    let channels = requested_channels.clamp(1, 2);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostats".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-n".to_string(),
        "-f".to_string(),
        "alsa".to_string(),
        "-i".to_string(),
        device.to_string(),
        "-ar".to_string(),
        rate.to_string(),
        "-ac".to_string(),
        channels.to_string(),
    ];
    match format {
        "wav" => args.extend(["-c:a".into(), "pcm_s16le".into()]),
        "flac" => args.extend(["-c:a".into(), "flac".into()]),
        "opus" => args.extend([
            "-c:a".into(),
            "libopus".into(),
            "-b:a".into(),
            bitrate.into(),
        ]),
        "mp3" => args.extend([
            "-c:a".into(),
            "libmp3lame".into(),
            "-b:a".into(),
            bitrate.into(),
        ]),
        _ => return Err(format!("unsupported ffmpeg audio format: {format}")),
    }
    args.push(path.to_string_lossy().into_owned());

    let (log, stderr) = match create_ffmpeg_log() {
        Ok(log) => log,
        Err(error) => {
            let error = error.to_string();
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let child = match recorder_command("ffmpeg", &args, Stdio::from(stderr)).spawn() {
        Ok(child) => child,
        Err(error) => {
            let error = format!("cannot start ffmpeg audio recorder: {error}");
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let mut child = FfmpegChild::new(child);

    thread::sleep(Duration::from_millis(150));
    if let Some(status) = child
        .as_mut()
        .try_wait()
        .map_err(|error| error.to_string())?
    {
        let detail = read_ffmpeg_log_tail(log.path());
        let _ = std::fs::remove_file(path);
        let error = format!(
            "ffmpeg audio recorder exited during startup ({status}): {}",
            detail.trim()
        );
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
    if !claim_ready(&stop, &ready, (rate, channels)) {
        drop(child);
        let _ = std::fs::remove_file(path);
        return Err("ffmpeg audio recorder started after start() gave up on it".into());
    }

    loop {
        if stop.load(Ordering::Acquire) {
            if let Some(mut stdin) = child.as_mut().stdin.take() {
                let _ = stdin.write_all(b"q\n");
                let _ = stdin.flush();
            }
            let status = wait_for_child_with_grace(child.as_mut(), FFMPEG_STOP_GRACE)
                .map_err(|error| error.to_string())?;
            return if status.success() {
                Ok(())
            } else {
                let detail = read_ffmpeg_log_tail(log.path());
                Err(format!(
                    "ffmpeg audio recorder exited with {status}: {}",
                    detail.trim()
                ))
            };
        }
        if let Some(status) = child
            .as_mut()
            .try_wait()
            .map_err(|error| error.to_string())?
        {
            let detail = read_ffmpeg_log_tail(log.path());
            return Err(format!(
                "ffmpeg audio recorder stopped unexpectedly ({status}): {}",
                detail.trim()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

impl Drop for AudioRecordingState {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Hand the negotiated format to `start()` unless it already gave up on this
/// worker.
///
/// `start()` sets `stop` before abandoning a worker that missed its deadline
/// and drops the receiver when it returns. Either signal means the user was
/// told this recording never started, so a late worker must release the
/// device and remove its output instead of capturing.
#[cfg(feature = "media-audio")]
fn claim_ready(
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<ReadyMessage>,
    format: (u32, u16),
) -> bool {
    !stop.load(Ordering::Acquire) && ready.try_send(Ok(format)).is_ok()
}

#[cfg(feature = "media-audio")]
fn capture_to_wav(
    path: &Path,
    device: &str,
    requested_rate: u32,
    requested_channels: u16,
    stop: Arc<AtomicBool>,
    ready: mpsc::SyncSender<Result<(u32, u16), String>>,
) -> Result<(), String> {
    let result = (|| {
        let pcm = PCM::new(device, Direction::Capture, true)
            .map_err(|error| format!("cannot open audio input '{device}': {error}"))?;
        let (rate, channels) = {
            let params = HwParams::any(&pcm).map_err(|error| error.to_string())?;
            params
                .set_access(Access::RWInterleaved)
                .map_err(|error| error.to_string())?;
            params
                .set_format(Format::s16())
                .map_err(|error| error.to_string())?;
            params
                .set_channels(u32::from(requested_channels.clamp(1, 2)))
                .map_err(|error| error.to_string())?;
            params
                .set_rate(requested_rate.clamp(8_000, 192_000), ValueOr::Nearest)
                .map_err(|error| error.to_string())?;
            params
                .set_period_size(1024, ValueOr::Nearest)
                .map_err(|error| error.to_string())?;
            pcm.hw_params(&params).map_err(|error| error.to_string())?;
            (
                params.get_rate().map_err(|error| error.to_string())?,
                params.get_channels().map_err(|error| error.to_string())? as u16,
            )
        };

        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        write_wav_header(&mut file, rate, channels, 0).map_err(|error| error.to_string())?;
        let io = pcm.io_i16().map_err(|error| error.to_string())?;
        pcm.start().map_err(|error| error.to_string())?;
        if !claim_ready(&stop, &ready, (rate, channels)) {
            return Err("audio recorder opened the device after start() gave up on it".into());
        }

        let mut samples = vec![0i16; 1024 * usize::from(channels)];
        let mut bytes = vec![0u8; samples.len() * 2];
        let mut data_len = 0u32;
        let capture_result = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            match io.readi(&mut samples) {
                Ok(frames) => {
                    let count = frames * usize::from(channels);
                    for (sample, output) in samples[..count]
                        .iter()
                        .zip(bytes[..count * 2].chunks_exact_mut(2))
                    {
                        output.copy_from_slice(&sample.to_le_bytes());
                    }
                    if let Err(error) = file.write_all(&bytes[..count * 2]) {
                        break Err(error.to_string());
                    }
                    data_len = data_len.saturating_add((count * 2) as u32);
                }
                Err(error) if error.errno() == libc::EAGAIN => {
                    if let Err(error) = pcm.wait(Some(100)) {
                        break Err(error.to_string());
                    }
                }
                Err(error) => {
                    if let Err(error) = pcm.try_recover(error, true) {
                        break Err(format!("audio capture failed: {error}"));
                    }
                }
            }
        };
        let _ = pcm.drop();
        file.seek(SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        write_wav_header(&mut file, rate, channels, data_len).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        capture_result
    })();

    if let Err(error) = &result {
        // Never block on start(): it may have stopped reading already, and a
        // worker it abandoned must still finish its cleanup below.
        let _ = ready.try_send(Err(error.clone()));
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= WAV_HEADER_LEN) {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

#[cfg(feature = "media-audio")]
fn write_wav_header(
    file: &mut File,
    rate: u32,
    channels: u16,
    data_len: u32,
) -> std::io::Result<()> {
    let bits_per_sample = 16u16;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = rate * u32::from(block_align);
    file.write_all(b"RIFF")?;
    file.write_all(&data_len.saturating_add(36).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits_per_sample.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "media-audio")]
    #[test]
    fn wav_header_contains_negotiated_format_and_size() {
        let path =
            std::path::PathBuf::from(format!("/tmp/jwm-wav-header-{}.wav", std::process::id()));
        let mut file = File::create(&path).unwrap();
        write_wav_header(&mut file, 48_000, 1, 960).unwrap();
        drop(file);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 960);
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn ffmpeg_log_is_private_temporary_and_reports_only_its_tail() {
        let (log, mut file) = create_ffmpeg_log().unwrap();
        let path = log.path().to_path_buf();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        file.write_all(b"old output that must be omitted\nfinal error\n")
            .unwrap();
        file.flush().unwrap();

        let detail = read_ffmpeg_log_tail_with_limit(&path, 12);
        assert!(detail.starts_with("[earlier ffmpeg output omitted]"));
        assert!(detail.ends_with("final error"));

        drop(file);
        drop(log);
        assert!(!path.exists());
    }

    /// Regression: the recorder is started on a thread that inherits the
    /// SIGCHLD block JWM keeps for its signalfd, and std handed ffmpeg that
    /// mask. The recorder command unblocks it.
    #[cfg(feature = "media-audio")]
    #[test]
    fn the_recorder_starts_with_sigchld_unblocked() {
        use crate::external_command::test_support::{SigchldBlockedOnThisThread, SigchldProbe};

        let _blocked = SigchldBlockedOnThisThread::new();
        let probe = SigchldProbe::new("recorder");
        let args = ["-c".to_owned(), probe.script()];

        let child = recorder_command("sh", &args, Stdio::null())
            .spawn()
            .expect("spawn the probe recorder");
        let status = FfmpegChild::new(child)
            .as_mut()
            .wait()
            .expect("reap the probe recorder");

        assert!(status.success());
        assert!(!probe.child_blocked_sigchld());
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn child_wait_has_a_hard_grace_deadline() {
        let mut child = Command::new("sh")
            .args(["-c", "exec sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();

        let status = wait_for_child_with_grace(&mut child, Duration::from_millis(20)).unwrap();

        assert!(!status.success());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn ffmpeg_child_guard_terminates_and_reaps_an_early_return() {
        let child = Command::new("sh")
            .args(["-c", "exec sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let started = Instant::now();

        drop(FfmpegChild::new(child));

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "guard did not terminate the abandoned child promptly"
        );
        let mut status = 0;
        let wait_result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(wait_result, -1, "abandoned ffmpeg child remained waitable");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "abandoned ffmpeg child was not reaped"
        );
    }

    /// Spin (never sleep) until a recorder thread has returned, so the state
    /// under test observes it through `is_finished()` like the event loop.
    fn wait_until_finished(handle: &JoinHandle<Result<(), String>>) {
        while !handle.is_finished() {
            std::thread::yield_now();
        }
    }

    #[cfg(feature = "media-audio")]
    fn unique_output_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jwm-audio-{tag}-{}-{:016x}.wav",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    fn state_with_worker(handle: JoinHandle<Result<(), String>>) -> AudioRecordingState {
        let mut state = AudioRecordingState::default();
        state.active = true;
        state.output_path = Some("/tmp/jwm-recording.wav".into());
        state.worker = Some(AudioWorker {
            stop: Arc::new(AtomicBool::new(false)),
            handle,
        });
        state
    }

    #[test]
    fn a_recorder_that_died_on_its_own_keeps_the_session_open_until_stop() {
        // A USB microphone unplugged mid-recording: the worker returns its
        // capture error without anyone asking it to stop.
        let handle = std::thread::spawn(|| Err("audio capture failed: No such device".to_string()));
        wait_until_finished(&handle);
        let mut state = state_with_worker(handle);

        assert!(state.refresh(), "the unexpected exit must be reported");
        // toggle_audio_recording branches on `active` right after refresh():
        // it has to take the stop path, never reopen the microphone.
        assert!(state.active);
        assert_eq!(
            state.last_error.as_deref(),
            Some("audio capture failed: No such device")
        );
        assert!(state.refresh(), "the exit stays latched until stop()");
        assert_eq!(
            state.stop(),
            Err("audio capture failed: No such device".to_string())
        );
        assert!(!state.active);
        assert!(!state.refresh());
        assert_eq!(state.stop(), Ok(()), "stop stays idempotent");
    }

    /// A recorder thread that finalizes only once it was asked to stop and
    /// the test releases it — the stand-in for a WAV header rewrite or an
    /// ffmpeg trailer flush still in progress.
    fn finalizing_recorder(
        outcome: Result<(), String>,
    ) -> (AudioRecordingState, std::sync::mpsc::Sender<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            // The long bound only keeps a regression from hanging the suite.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            if !thread_stop.load(Ordering::Acquire) {
                return Err("finalized without being asked to stop".to_string());
            }
            outcome
        });
        let mut state = AudioRecordingState::default();
        state.active = true;
        state.output_path = Some("/tmp/jwm-recording.wav".into());
        state.worker = Some(AudioWorker { stop, handle });
        (state, release_tx)
    }

    #[test]
    fn a_key_stop_ends_the_session_at_once_and_finalizes_off_the_event_thread() {
        let (mut state, release) = finalizing_recorder(Ok(()));

        assert_eq!(state.begin_stop(), None, "the recorder is still finalizing");
        // The microphone session is over for the chip, idle and the effect
        // queries, while the file is still being written.
        assert!(!state.active);
        assert!(state.is_finalizing());
        assert_eq!(state.poll_finalized(), None, "never joins a running thread");
        assert!(state.is_finalizing());

        release.send(()).unwrap();
        if let Some(finalizing) = state.finalizing.as_ref() {
            wait_until_finished(&finalizing.handle);
        }
        assert_eq!(
            state.poll_finalized(),
            Some(FinalizedRecording {
                output_path: Some("/tmp/jwm-recording.wav".into()),
                outcome: Ok(()),
            })
        );
        assert!(!state.is_finalizing());
        assert_eq!(state.poll_finalized(), None, "reported once");
        assert_eq!(state.stop(), Ok(()), "stop stays idempotent");
    }

    #[test]
    fn a_finalization_that_failed_is_reported_by_the_poll() {
        let (mut state, release) =
            finalizing_recorder(Err("ffmpeg audio recorder exited with 1".to_string()));
        assert_eq!(state.begin_stop(), None);
        release.send(()).unwrap();
        if let Some(finalizing) = state.finalizing.as_ref() {
            wait_until_finished(&finalizing.handle);
        }

        let finalized = state.poll_finalized().expect("the recorder returned");
        assert_eq!(
            finalized.outcome,
            Err("ffmpeg audio recorder exited with 1".to_string())
        );
        assert_eq!(
            state.last_error.as_deref(),
            Some("ffmpeg audio recorder exited with 1")
        );
    }

    #[test]
    fn a_blocking_stop_waits_for_a_recording_left_finalizing() {
        // Shutdown, the screen recorder's microphone hand-off and IPC
        // `stop_audio_recording` still need the file finished (and the
        // device released) before they go on.
        let (mut state, release) = finalizing_recorder(Ok(()));
        assert_eq!(state.begin_stop(), None);
        release.send(()).unwrap();

        assert_eq!(state.stop(), Ok(()));
        assert!(!state.is_finalizing());
        assert_eq!(
            state.poll_finalized(),
            None,
            "the blocking stop reported it"
        );
    }

    #[test]
    fn begin_stop_answers_at_once_when_nothing_is_left_to_finalize() {
        let mut idle = AudioRecordingState::default();
        assert_eq!(idle.begin_stop(), Some(Ok(())));
        assert!(!idle.is_finalizing());

        // A recorder that died on its own already returned: its latched
        // error is the answer, with nothing to wait for.
        let handle = std::thread::spawn(|| Err("audio capture failed: No such device".to_string()));
        wait_until_finished(&handle);
        let mut state = state_with_worker(handle);
        assert!(state.refresh());
        assert_eq!(
            state.begin_stop(),
            Some(Err("audio capture failed: No such device".to_string()))
        );
        assert!(!state.active);
        assert!(!state.is_finalizing());
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn a_new_recording_waits_for_the_previous_one_to_finalize() {
        let (mut state, release) = finalizing_recorder(Ok(()));
        assert_eq!(state.begin_stop(), None);

        // The previous recorder may still hold the capture device.
        let refused = state.check_can_start().unwrap_err();
        assert!(refused.contains("still being finalized"), "{refused}");

        release.send(()).unwrap();
        if let Some(finalizing) = state.finalizing.as_ref() {
            wait_until_finished(&finalizing.handle);
        }
        assert!(state.poll_finalized().is_some());
        assert_eq!(state.check_can_start(), Ok(()));
    }

    #[test]
    fn a_panicked_recorder_still_ends_the_session_on_stop() {
        // resume_unwind skips the panic hook, keeping the test output clean.
        let handle = std::thread::spawn(|| -> Result<(), String> {
            std::panic::resume_unwind(Box::new("injected recorder panic"))
        });
        wait_until_finished(&handle);
        let mut state = state_with_worker(handle);

        assert_eq!(
            state.stop(),
            Err("audio recorder thread panicked".to_string())
        );
        assert!(!state.active, "a joined, panicked worker is not recording");
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn starting_after_an_unobserved_exit_is_still_allowed() {
        let handle =
            std::thread::spawn(|| Err("ffmpeg audio recorder stopped unexpectedly".to_string()));
        wait_until_finished(&handle);
        let mut state = state_with_worker(handle);

        // An explicit IPC start acknowledges the dead session, as before.
        assert_eq!(state.check_can_start(), Ok(()));
        assert!(state.exited.is_some());
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn a_recorder_stuck_initializing_is_parked_instead_of_joined() {
        let mut state = AudioRecordingState::default();
        let path = unique_output_path("stuck-init");
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker_path = path.clone();
        let started = state.spawn_worker(&path, Duration::from_millis(10), move |stop, _ready| {
            // Stands in for PCM::new blocked inside a wedged sound-server
            // plugin; the long bound only keeps a regression from hanging.
            let _ = release_rx.recv_timeout(Duration::from_secs(30));
            if !stop.load(Ordering::Acquire) {
                return Err("start() gave up without requesting a stop".into());
            }
            // The device finally opened: a header-only file is left behind.
            std::fs::write(&worker_path, [0u8; WAV_HEADER_LEN as usize])
                .map_err(|error| error.to_string())
        });

        let error = match started {
            Ok(_) => panic!("a worker that never became ready was adopted"),
            Err(error) => error,
        };
        assert!(error.contains("did not initialize"), "{error}");
        let abandoned = state
            .abandoned
            .as_ref()
            .expect("the stuck worker is parked for refresh() to reap");
        assert!(
            !abandoned.handle.is_finished(),
            "start() must return without waiting for the stuck worker"
        );
        assert!(!state.active);

        // No second recorder may race the stuck one for the capture device.
        let refused = state.check_can_start().unwrap_err();
        assert!(refused.contains("still stuck"), "{refused}");

        release_tx.send(()).unwrap();
        if let Some(abandoned) = state.abandoned.as_ref() {
            wait_until_finished(&abandoned.handle);
        }
        assert_eq!(state.check_can_start(), Ok(()));
        assert!(state.abandoned.is_none(), "refresh() reaped the worker");
        assert!(!path.exists(), "the late header-only output was removed");
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn a_worker_that_returns_without_reporting_surfaces_its_own_error() {
        let mut state = AudioRecordingState::default();
        let path = unique_output_path("silent-exit");
        let started = state.spawn_worker(&path, Duration::from_secs(30), |_stop, ready| {
            drop(ready);
            Err("cannot open audio input 'default': Device or resource busy".into())
        });

        assert_eq!(
            started.err().as_deref(),
            Some("cannot open audio input 'default': Device or resource busy")
        );
        assert!(state.abandoned.is_none(), "a returned worker is joined");
    }

    #[cfg(feature = "media-audio")]
    #[test]
    fn a_late_worker_does_not_claim_a_start_that_gave_up() {
        let stop = AtomicBool::new(false);

        let (ready, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        assert!(!claim_ready(&stop, &ready, (48_000, 1)), "start() returned");

        let (ready, receiver) = mpsc::sync_channel(1);
        stop.store(true, Ordering::Release);
        assert!(!claim_ready(&stop, &ready, (48_000, 1)), "stop requested");
        assert!(receiver.try_recv().is_err());

        stop.store(false, Ordering::Release);
        assert!(claim_ready(&stop, &ready, (48_000, 1)));
        assert_eq!(receiver.try_recv().unwrap(), Ok((48_000, 1)));
    }

    #[test]
    fn auto_backend_keeps_wav_direct_and_compresses_other_formats() {
        assert_eq!(select_backend("auto", "wav").unwrap(), AudioBackend::Direct);
        assert_eq!(
            select_backend("auto", "flac").unwrap(),
            AudioBackend::Ffmpeg
        );
        assert_eq!(
            select_backend("auto", "opus").unwrap(),
            AudioBackend::Ffmpeg
        );
        assert!(select_backend("direct", "mp3").is_err());
    }

    #[test]
    fn supported_output_formats_are_explicit() {
        for extension in ["wav", "flac", "opus", "mp3"] {
            let path = std::path::PathBuf::from(format!("/tmp/note.{extension}"));
            assert_eq!(output_format(&path).unwrap(), extension);
        }
        assert!(output_format(Path::new("/tmp/note.aac")).is_err());
    }
}
