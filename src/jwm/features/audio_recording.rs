//! Built-in microphone recorder.
//!
//! Audio is captured directly through ALSA and written as PCM WAV.  No
//! `arecord`, ffmpeg, or desktop recording application is spawned.

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct AudioWorker {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<(), String>>,
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
    ) -> Result<(), String> {
        self.refresh();
        if self.active {
            return Err("audio recording is already active".into());
        }
        if !output_path.is_absolute() {
            return Err("audio recording output path must be absolute".into());
        }
        if output_path.extension().and_then(|value| value.to_str()) != Some("wav") {
            return Err("audio recording output path must end in .wav".into());
        }
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
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("jwm-audio-recorder".into())
            .spawn(move || {
                capture_to_wav(
                    &path,
                    &thread_device,
                    sample_rate,
                    channels,
                    thread_stop,
                    ready_tx,
                )
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
        self.last_error = None;
        self.worker = Some(AudioWorker { stop, handle });
        Ok(())
    }

    /// Stop and finalize the WAV header. Safe to call more than once.
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
}
