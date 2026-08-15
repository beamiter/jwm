//! Shared recording encoder selection for all compositor backends.

use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Return whether both the local ALSA device and ffmpeg's ALSA input are
/// available. Screen recording falls back to video-only when this is false.
#[cfg(feature = "media-audio")]
pub fn recording_audio_available(device: &str) -> bool {
    let alsa_available = alsa::pcm::PCM::new(device, alsa::Direction::Capture, true).is_ok();
    if !alsa_available {
        return false;
    }
    ffmpeg_has_alsa_demuxer()
}

/// Whether this ffmpeg build can read from ALSA. The answer cannot change while
/// jwm runs, and the probe costs a full ffmpeg process spawn on the compositor
/// thread, so it is resolved once. Before this was cached, every recording
/// start paid it as a visible hitch at the moment the hotkey was pressed.
#[cfg(feature = "media-audio")]
fn ffmpeg_has_alsa_demuxer() -> bool {
    static HAS_ALSA: OnceLock<bool> = OnceLock::new();
    *HAS_ALSA.get_or_init(|| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-demuxers"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| line.split_whitespace().any(|field| field == "alsa"))
            })
    })
}

/// Audio capture is not compiled in; screen recording stays video-only.
#[cfg(not(feature = "media-audio"))]
pub fn recording_audio_available(_device: &str) -> bool {
    false
}

/// Add a timestamped ALSA input after the raw-video input.
pub fn append_recording_audio_input(args: &mut Vec<String>, device: &str) {
    args.extend(
        [
            "-thread_queue_size",
            "1024",
            "-use_wallclock_as_timestamps",
            "1",
            "-f",
            "alsa",
            "-i",
            device,
        ]
        .into_iter()
        .map(str::to_string),
    );
}

/// Map and encode the synchronized microphone track. `aresample=async=1`
/// compensates small drift between the GPU frame clock and the audio clock.
pub fn append_recording_audio_output(args: &mut Vec<String>, bitrate: &str) {
    args.extend(
        [
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:a",
            "aac",
            "-b:a",
            bitrate,
            "-af",
            "aresample=async=1:first_pts=0",
            "-shortest",
        ]
        .into_iter()
        .map(str::to_string),
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingEncoder {
    Nvenc,
    Vaapi,
    Software,
}

impl RecordingEncoder {
    pub fn codec_name(self, software_codec: &'static str) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::Vaapi => "h264_vaapi",
            Self::Software => software_codec,
        }
    }
}

/// Honor an explicit encoder choice, or probe NVENC → VAAPI → software for
/// `auto`. Probing is deliberately backend-agnostic: all compositors feed
/// ffmpeg the same CPU-side RGBA frames.
///
/// The `auto` probe runs up to two real encodes in child ffmpeg processes,
/// which initialize the NVENC/VAAPI driver and take a few hundred milliseconds
/// each — on the compositor thread, at the instant the recording hotkey is
/// pressed. Hardware does not appear or vanish mid-session, so the answer is
/// memoized per configured value and only the first recording pays for it.
pub fn select_recording_encoder(configured: &str) -> RecordingEncoder {
    static CACHE: OnceLock<Mutex<Option<(String, RecordingEncoder)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((cached_for, encoder)) = guard.as_ref()
        && cached_for == configured
    {
        return *encoder;
    }
    let encoder = probe_recording_encoder(configured);
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((configured.to_string(), encoder));
    }
    encoder
}

fn probe_recording_encoder(configured: &str) -> RecordingEncoder {
    match configured {
        "nvenc" => RecordingEncoder::Nvenc,
        "vaapi" => RecordingEncoder::Vaapi,
        "software" => RecordingEncoder::Software,
        _ if probe(&[
            "-f",
            "lavfi",
            "-i",
            "nullsrc=s=64x64",
            "-frames:v",
            "1",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ]) =>
        {
            RecordingEncoder::Nvenc
        }
        _ if std::path::Path::new("/dev/dri/renderD128").exists()
            && probe(&[
                "-vaapi_device",
                "/dev/dri/renderD128",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=s=64x64",
                "-frames:v",
                "1",
                "-c:v",
                "h264_vaapi",
                "-f",
                "null",
                "-",
            ]) =>
        {
            RecordingEncoder::Vaapi
        }
        _ => RecordingEncoder::Software,
    }
}

fn probe(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Quiet ffmpeg down before anything else. By default it prints a progress line
/// per encoded frame to stderr, and both backends point stderr at a log file in
/// `/tmp` — which is tmpfs, i.e. RAM — so an hour of recording silently spends
/// tens of megabytes of memory on a line nobody reads. Warnings and errors are
/// kept, because they are the only diagnostic when a recording comes out empty.
pub fn append_recording_log_args(args: &mut Vec<String>) {
    args.extend(["-hide_banner", "-loglevel", "warning", "-nostats"].map(str::to_string));
}

/// Threads the software encoder may use. libx264 defaults to one thread per
/// core plus change, so at 1080p30 it can saturate the machine and leave the
/// compositor — which needs only a few milliseconds per frame — waiting on the
/// run queue behind it. Recording is a background job by definition; capping it
/// at half the cores keeps the desktop responsive and still comfortably encodes
/// 1080p30 in real time.
pub fn recording_encoder_threads() -> usize {
    std::thread::available_parallelism()
        .map(|cores| (cores.get() / 2).max(1))
        .unwrap_or(1)
}

/// Encoder-side pacing for the software path: a fast preset so a CPU encode can
/// keep up with the capture rate in the first place, plus the thread cap above.
/// Hardware encoders do the work on the GPU and need neither.
pub fn append_software_encoder_pacing(args: &mut Vec<String>) {
    args.extend(
        [
            "-preset".to_string(),
            "veryfast".to_string(),
            "-threads".to_string(),
            recording_encoder_threads().to_string(),
        ]
        .into_iter(),
    );
}

/// Start ffmpeg one nice level below the desktop so the kernel prefers the
/// compositor whenever both are runnable. `pre_exec` runs in the forked child
/// before `execvp`, so it only ever changes the encoder's priority.
///
/// # Safety
/// `libc::nice` is async-signal-safe and touches no allocator state, which is
/// the requirement `pre_exec` imposes on its closure.
pub fn deprioritize_encoder(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            // A failed nice() is not a reason to abandon the recording.
            libc::nice(10);
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_args_silence_the_per_frame_progress_line() {
        // ffmpeg's default stats write one line per encoded frame into the log
        // file both backends open under /tmp, which is tmpfs.
        let mut args = Vec::new();
        append_recording_log_args(&mut args);
        assert!(args.iter().any(|arg| arg == "-nostats"));
        assert!(args.windows(2).any(|pair| pair == ["-loglevel", "warning"]));
    }

    #[test]
    fn software_encoding_is_capped_so_it_cannot_starve_the_compositor() {
        let mut args = Vec::new();
        append_software_encoder_pacing(&mut args);
        assert!(args.windows(2).any(|pair| pair == ["-preset", "veryfast"]));
        let threads = args
            .iter()
            .position(|arg| arg == "-threads")
            .and_then(|index| args.get(index + 1))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("thread cap is passed to ffmpeg");
        assert_eq!(threads, recording_encoder_threads());
        assert!(threads >= 1);
        let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get());
        assert!(
            threads <= cores.max(1),
            "the encoder must never be given more threads than the machine has"
        );
    }

    #[test]
    fn an_explicit_encoder_choice_is_honored_without_probing() {
        // Explicit choices must not spawn probe processes, so these are safe to
        // assert in a unit test on a machine with no ffmpeg at all.
        assert_eq!(
            select_recording_encoder("software"),
            RecordingEncoder::Software
        );
        assert_eq!(select_recording_encoder("nvenc"), RecordingEncoder::Nvenc);
        assert_eq!(select_recording_encoder("vaapi"), RecordingEncoder::Vaapi);
        // Repeating a choice goes through the memo path and must agree.
        assert_eq!(
            select_recording_encoder("software"),
            RecordingEncoder::Software
        );
    }

    #[test]
    fn synchronized_audio_args_map_second_input_and_bound_duration() {
        let mut args = Vec::new();
        append_recording_audio_input(&mut args, "hw:1,0");
        append_recording_audio_output(&mut args, "160k");
        assert!(args.windows(2).any(|pair| pair == ["-f", "alsa"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "1:a:0"]));
        assert!(args.windows(2).any(|pair| pair == ["-b:a", "160k"]));
        assert!(
            args.iter()
                .any(|arg| arg == "aresample=async=1:first_pts=0")
        );
        assert!(args.iter().any(|arg| arg == "-shortest"));
    }
}
