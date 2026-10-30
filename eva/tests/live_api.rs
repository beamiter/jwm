//! Explicitly opt-in cloud configuration checks. Offline CI must not describe
//! an ignored test as API, microphone, or voice acceptance.
use jwm_eva::{config::Config, doctor};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "requires explicit JWM_EVA_RUN_LIVE=1 and OPENAI_API_KEY; connects both actual configured models"]
async fn realtime_configuration_handshake() {
    assert_eq!(
        std::env::var("JWM_EVA_RUN_LIVE").as_deref(),
        Ok("1"),
        "live API checks require explicit opt-in"
    );
    let key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY is required; missing credentials are a failed gate, not a pass");
    assert!(!key.trim().is_empty(), "OPENAI_API_KEY must not be empty");
    let path = std::env::var_os("JWM_EVA_LIVE_CONFIG").map(PathBuf::from);
    let config = Config::load(path.as_deref())
        .expect("selected live test configuration must load and validate");
    // The same helper powers doctor --live. Mini uses the actual isolated
    // wake settings, including no tools/ASR and manual server VAD responses.
    let report = doctor::check_live(&config, key.trim()).await;
    assert!(!report.microphone_opened);
    assert!(!report.audio_uploaded);
    assert!(!report.inference_requested);
    assert!(!report.desktop_execution);
    assert_eq!(report.checks.len(), 2);
    assert!(
        report.ready,
        "configured Realtime handshake failed: {}",
        serde_json::to_string(&report).unwrap()
    );
}
