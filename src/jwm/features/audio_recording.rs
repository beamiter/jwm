//! Built-in microphone recorder.
//!
//! PCM WAV can be captured directly through ALSA. Compressed formats use an
//! ffmpeg worker while retaining ALSA as the Linux capture source.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct AudioWorker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<(), String>>,
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
            .finish()
    }
}

impl AudioRecordingState {
    pub fn start(
        &mut self,
        output_path: &Path,
        device: &str,
        sample_rate: u32,
        channels: u16,
        configured_backend: &str,
        bitrate: &str,
    ) -> Result<(), String> {
        self.refresh();
        if self.active {
            return Err("audio recording is already active".into());
        }
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
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_device = device_name.clone();
        let thread_format = format.to_string();
        let thread_bitrate = bitrate.to_string();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("jwm-audio-recorder".into())
            .spawn(move || match backend {
                AudioBackend::Direct => capture_to_wav(
                    &path,
                    &thread_device,
                    sample_rate,
                    channels,
                    thread_stop,
                    ready_tx,
                ),
                AudioBackend::Ffmpeg => capture_with_ffmpeg(
                    &path,
                    &thread_device,
                    sample_rate,
                    channels,
                    &thread_format,
                    &thread_bitrate,
                    thread_stop,
                    ready_tx,
                ),
            })
            .map_err(|error| error.to_string())?;

        let (actual_rate, actual_channels) = match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(format)) => format,
            Ok(Err(error)) => {
                let _ = handle.join();
                return Err(error);
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = handle.join();
                return Err(format!("audio recorder did not initialize: {error}"));
            }
        };

        self.active = true;
        self.output_path = Some(output_path.to_string_lossy().into_owned());
        self.started_at = Some(Instant::now());
        self.sample_rate = actual_rate;
        self.channels = actual_channels;
        self.device = device_name;
        self.backend = backend.as_str().to_string();
        self.format = format.to_string();
        self.last_error = None;
        self.worker = Some(AudioWorker { stop, handle });
        Ok(())
    }

    /// Stop and finalize the active container. Safe to call more than once.
    pub fn stop(&mut self) -> Result<(), String> {
        let Some(worker) = self.worker.take() else {
            self.active = false;
            return Ok(());
        };
        worker.stop.store(true, Ordering::Release);
        let result = worker
            .handle
            .join()
            .map_err(|_| "audio recorder thread panicked".to_string())?;
        self.active = false;
        if let Err(error) = &result {
            self.last_error = Some(error.clone());
        }
        result
    }

    /// Observe a device/file error which ended the worker unexpectedly.
    pub fn refresh(&mut self) {
        if !self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.handle.is_finished())
        {
            return;
        }
        let worker = self.worker.take().expect("worker checked above");
        self.active = false;
        match worker.handle.join() {
            Ok(Err(error)) => self.last_error = Some(error),
            Err(_) => self.last_error = Some("audio recorder thread panicked".into()),
            Ok(Ok(())) => {}
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at
            .map_or(Duration::ZERO, |start| start.elapsed())
    }
}

#[allow(clippy::too_many_arguments)]
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

    let log_path = format!("/tmp/jwm-audio-recording-ffmpeg-{}.log", std::process::id());
    let stderr = match File::create(&log_path) {
        Ok(file) => file,
        Err(error) => {
            let error = error.to_string();
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let mut child = match Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let error = format!("cannot start ffmpeg audio recorder: {error}");
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };

    thread::sleep(Duration::from_millis(150));
    if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
        let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(path);
        let error = format!(
            "ffmpeg audio recorder exited during startup ({status}): {}",
            detail.trim()
        );
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
    let _ = ready.send(Ok((rate, channels)));

    loop {
        if stop.load(Ordering::Acquire) {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(b"q\n");
                let _ = stdin.flush();
            }
            let status = child.wait().map_err(|error| error.to_string())?;
            return if status.success() {
                let _ = std::fs::remove_file(&log_path);
                Ok(())
            } else {
                let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
                Err(format!(
                    "ffmpeg audio recorder exited with {status}: {}",
                    detail.trim()
                ))
            };
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let detail = std::fs::read_to_string(&log_path).unwrap_or_default();
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
        let _ = ready.send(Ok((rate, channels)));

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
        let _ = ready.send(Err(error.clone()));
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() <= 44) {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

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
