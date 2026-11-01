//! Separate processes isolate ORT's process-global loader without mutating
//! environment variables inside a multithreaded test runner.
use jwm_eva::{
    audio::{Frame, local_voice::LocalVoiceDetector, voice_gate::UploadGate},
    config::Config,
};
use std::{process::Command, time::Instant};

#[test]
fn backend_readiness_probe() {
    let Ok(expected) = std::env::var("JWM_EVA_TEST_VAD_PROBE") else {
        return;
    };
    let mut c = Config::default();
    let detector = LocalVoiceDetector::new(&c);
    assert_eq!(detector.status.active, "rnnoise");
    assert_eq!(detector.status.degraded_reason, Some(expected.as_str()));
    c.audio.local_voice_fallback = "none".into();
    let mut gate = UploadGate::new(&c);
    assert_eq!(gate.statistics()["voice_backend"], "unavailable");
    assert!(
        gate.admit(
            Frame {
                captured: Instant::now(),
                timestamp_verified: true,
                pcm: vec![0; 960],
                level: 1.0
            },
            false
        )
        .is_err()
    );
    assert_eq!(gate.uploaded_frames, 0);
}

#[test]
fn missing_runtime_never_panics_or_silently_uploads() {
    let reason = if cfg!(feature = "silero") {
        "silero_runtime_unavailable"
    } else {
        "silero_feature_unavailable"
    };
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "backend_readiness_probe", "--nocapture"])
        .env("JWM_EVA_TEST_VAD_PROBE", reason)
        .env("ORT_DYLIB_PATH", "/nonexistent/eva-test-onnxruntime.so")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg(feature = "silero")]
fn incompatible_runtime_never_panics_or_silently_uploads() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "backend_readiness_probe", "--nocapture"])
        .env("JWM_EVA_TEST_VAD_PROBE", "silero_runtime_incompatible")
        .env("ORT_DYLIB_PATH", "libc.so.6")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg(feature = "silero")]
fn corrupt_model_is_refused_before_native_inference() {
    use std::io::Write;
    let mut model = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = include_bytes!("../models/silero-v5.1.2/silero_vad.onnx").to_vec();
    bytes[100] ^= 1;
    model.write_all(&bytes).unwrap();
    let mut c = Config::default();
    c.audio.silero_model_path = Some(model.path().to_owned());
    let detector = LocalVoiceDetector::new(&c);
    assert_eq!(detector.status.active, "rnnoise");
    assert_eq!(
        detector.status.degraded_reason,
        Some("silero_model_checksum_mismatch")
    );
}
