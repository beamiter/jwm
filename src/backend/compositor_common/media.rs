//! Shared recording encoder selection for all compositor backends.

use std::process::{Command, Stdio};

/// Return whether both the local ALSA device and ffmpeg's ALSA input are
/// available. Screen recording falls back to video-only when this is false.
pub fn recording_audio_available(device: &str) -> bool {
    let alsa_available = alsa::pcm::PCM::new(device, alsa::Direction::Capture, true).is_ok();
    if !alsa_available {
        return false;
    }
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
pub fn select_recording_encoder(configured: &str) -> RecordingEncoder {
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

#[cfg(test)]
mod tests {
    use super::*;

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
