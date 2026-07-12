//! Shared recording encoder selection for all compositor backends.

use std::process::{Command, Stdio};

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
