//! Separate processes isolate ORT's process-global loader without mutating
//! environment variables inside a multithreaded test runner.
use jwm_eva::{
    audio::{Frame, local_voice::LocalVoiceDetector, voice_gate::UploadGate},
    config::Config,
};
use std::{process::Command, time::Instant};

#[test]
fn special_model_file_probe() {
    let Some(path) = std::env::var_os("JWM_EVA_TEST_SPECIAL_MODEL") else {
        return;
    };
    let mut c = Config::default();
    c.audio.silero_model_path = Some(path.into());
    let detector = LocalVoiceDetector::new(&c);
    assert_eq!(detector.status.active, "rnnoise");
    #[cfg(feature = "silero")]
    assert_eq!(
        detector.status.degraded_reason,
        Some("silero_model_invalid_file")
    );
    c.audio.local_voice_fallback = "none".into();
    c.audio.local_voice_detection = false;
    assert_eq!(LocalVoiceDetector::new(&c).status.active, "disabled");
    let gate = UploadGate::new(&c);
    assert!(!gate.enabled);
    assert_eq!(gate.statistics()["voice_backend"], "disabled");
    // Standby may explicitly disable its gate while the global setting is on.
    c.audio.local_voice_detection = true;
    let gate = UploadGate::with_enabled(&c, false);
    assert_eq!(gate.statistics()["voice_backend"], "disabled");
}

#[test]
fn non_regular_model_files_fail_promptly_and_disabled_gates_never_probe_them() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, time::Duration};
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("model.fifo");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a valid NUL-terminated pathname in this test's own dir.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "special_model_file_probe", "--nocapture"])
        .env("JWM_EVA_TEST_SPECIAL_MODEL", fifo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().unwrap().is_some() {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("model FIFO blocked initialization/disabled gate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

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
