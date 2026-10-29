//! Opt-in live provider contract check. Offline CI ignores this test and must
//! not describe that ignored result as voice or API acceptance.
use jwm_eva::{
    config::Config,
    providers::openai_realtime::{Realtime, ready_with},
};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires explicit JWM_EVA_RUN_LIVE=1 and OPENAI_API_KEY; opens a real API session"]
async fn realtime_configuration_handshake() {
    assert_eq!(
        std::env::var("JWM_EVA_RUN_LIVE").as_deref(),
        Ok("1"),
        "live API checks require explicit opt-in"
    );
    let key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY is required; missing credentials are a failed gate, not a pass");
    assert!(!key.trim().is_empty(), "OPENAI_API_KEY must not be empty");
    let config = Config::default();
    let mut provider = Realtime::connect(&config, &key)
        .await
        .expect("live Realtime connection failed");
    let created = tokio::time::timeout(Duration::from_secs(8), provider.next())
        .await
        .expect("session.created timeout")
        .expect("provider read failed");
    assert_eq!(
        created["type"], "session.created",
        "expected the official session lifecycle"
    );
    provider
        .configure(&config)
        .await
        .expect("session update failed");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let event = provider.next().await.expect("provider read failed");
            assert_ne!(
                event["type"], "error",
                "live session configuration was rejected"
            );
            if event["type"] == "session.updated" {
                assert!(
                    ready_with(&event, &config),
                    "effective PCM/VAD/session configuration differs from the local contract"
                );
                break;
            }
        }
    })
    .await
    .expect("session.updated timeout");
    // No audio, desktop commands, or inference responses are submitted here.
}
