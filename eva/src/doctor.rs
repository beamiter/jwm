//! Explicit provider diagnostics using the very same settings as normal and
//! wake sessions. A check owns only a WebSocket: no microphone, IPC, tool call,
//! conversation item, or inference request is created.
use crate::{
    config::Config,
    providers::openai_realtime::{Realtime, ready_custom, session_settings},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Standard,
    Wake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connect,
    SessionCreated,
    Configure,
    SessionUpdated,
    Ready,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCheck {
    pub role: Role,
    pub model: String,
    pub attempted: bool,
    pub ready: bool,
    pub phase: Phase,
    pub code: &'static str,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveReport {
    pub ready: bool,
    pub checks: Vec<ModelCheck>,
    pub microphone_opened: bool,
    pub audio_uploaded: bool,
    pub inference_requested: bool,
    pub desktop_execution: bool,
}
impl LiveReport {
    fn from_checks(checks: Vec<ModelCheck>) -> Self {
        Self {
            ready: checks.iter().all(|check| check.ready),
            checks,
            microphone_opened: false,
            audio_uploaded: false,
            inference_requested: false,
            desktop_execution: false,
        }
    }
    pub fn credential_unavailable(c: &Config) -> Self {
        Self::from_checks(vec![
            unattempted(Role::Standard, &c.openai.model, "credential_unavailable"),
            unattempted(Role::Wake, &c.wake_word.model, "credential_unavailable"),
        ])
    }
}
fn unattempted(role: Role, model: &str, code: &'static str) -> ModelCheck {
    ModelCheck {
        role,
        model: model.into(),
        attempted: false,
        ready: false,
        phase: Phase::Connect,
        code,
        elapsed_ms: 0,
    }
}

pub async fn check_live(c: &Config, key: &str) -> LiveReport {
    let (_cancel, receiver) = watch::channel(false);
    check_live_with_cancel(c, key, receiver).await
}

pub async fn check_live_with_cancel(
    c: &Config,
    key: &str,
    mut cancel: watch::Receiver<bool>,
) -> LiveReport {
    if c.validate().is_err() {
        return LiveReport::from_checks(vec![
            unattempted(Role::Standard, &c.openai.model, "configuration_invalid"),
            unattempted(Role::Wake, &c.wake_word.model, "configuration_invalid"),
        ]);
    }
    let standard = check_model(c, key, Role::Standard, session_settings(c), &mut cancel).await;
    let mini = c.wake_config();
    let wake = check_model(
        &mini,
        key,
        Role::Wake,
        crate::wake::session_settings(&mini),
        &mut cancel,
    )
    .await;
    LiveReport::from_checks(vec![standard, wake])
}

async fn cancellation(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn check_model(
    c: &Config,
    key: &str,
    role: Role,
    expected: Value,
    cancel: &mut watch::Receiver<bool>,
) -> ModelCheck {
    if *cancel.borrow() {
        return unattempted(role, &c.openai.model, "cancelled");
    }
    let start = Instant::now();
    let mut phase = Phase::Connect;
    let deadline = Duration::from_millis(c.openai.connect_timeout_ms.clamp(100, 30_000));
    let result = {
        let attempt = async {
            let mut provider = Realtime::connect(c, key).await?;
            handshake(&mut provider, &c.openai.model, &expected, &mut phase).await
        };
        bounded_probe(attempt, cancel, deadline).await
    };
    ModelCheck {
        role,
        model: c.openai.model.clone(),
        attempted: true,
        ready: result.is_ok(),
        phase,
        code: result
            .as_ref()
            .map_or_else(|error| public_code(error), |_| "ready"),
        elapsed_ms: start.elapsed().as_millis().min(u64::MAX as u128) as u64,
    }
}

async fn bounded_probe(
    attempt: impl std::future::Future<Output = io::Result<()>>,
    cancel: &mut watch::Receiver<bool>,
    deadline: Duration,
) -> io::Result<()> {
    tokio::pin!(attempt);
    tokio::select! {biased;
        _=cancellation(cancel)=>Err(io::Error::other("doctor_cancelled")),
        result=tokio::time::timeout(deadline,&mut attempt)=>result.unwrap_or_else(|_|Err(io::Error::other("doctor_timeout"))),
    }
}

fn public_code(error: &io::Error) -> &'static str {
    match error.to_string().as_str() {
        "doctor_cancelled" => "cancelled",
        "doctor_timeout" => "timeout",
        "doctor_lifecycle_invalid" => "lifecycle_invalid",
        "doctor_model_mismatch" => "model_mismatch",
        "doctor_configuration_mismatch" => "configuration_mismatch",
        "doctor_unexpected_event" => "unexpected_event",
        _ => crate::errors::describe(error).0,
    }
}

fn matches_model(actual: &Value, requested: &str) -> bool {
    let Some(actual) = actual.as_str() else {
        return false;
    };
    if actual == requested {
        return true;
    }
    // Official aliases may resolve to a dated snapshot. A standard alias must
    // never pass merely because "gpt-realtime-mini" shares its prefix.
    actual
        .strip_prefix(requested)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .is_some_and(|date| {
            let bytes = date.as_bytes();
            bytes.len() == 10
                && bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes
                    .iter()
                    .enumerate()
                    .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
        })
}

async fn handshake(
    provider: &mut Realtime,
    model: &str,
    expected: &Value,
    phase: &mut Phase,
) -> io::Result<()> {
    *phase = Phase::SessionCreated;
    let created = provider.next().await?;
    if created["type"] != "session.created" {
        return Err(io::Error::other("doctor_lifecycle_invalid"));
    }
    if !matches_model(&created["session"]["model"], model) {
        return Err(io::Error::other("doctor_model_mismatch"));
    }
    *phase = Phase::Configure;
    provider.configure_custom(expected.clone()).await?;
    *phase = Phase::SessionUpdated;
    loop {
        let event = provider.next().await?;
        match event["type"].as_str() {
            Some("session.updated") => {
                if !matches_model(&event["session"]["model"], model) {
                    return Err(io::Error::other("doctor_model_mismatch"));
                }
                if !ready_custom(&event, expected) {
                    return Err(io::Error::other("doctor_configuration_mismatch"));
                }
                *phase = Phase::Ready;
                return Ok(());
            }
            Some("rate_limits.updated") => {}
            _ => return Err(io::Error::other("doctor_unexpected_event")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    async fn socket_pair(
        created: Value,
        updated: Option<Value>,
    ) -> (Realtime, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    created.to_string().into(),
                ))
                .await
                .unwrap();
            let mut received = vec![];
            while let Some(Ok(message)) = socket.next().await {
                let tokio_tungstenite::tungstenite::Message::Text(message) = message else {
                    continue;
                };
                let event: Value = serde_json::from_str(&message).unwrap();
                assert_eq!(
                    event["type"], "session.update",
                    "diagnostics may only configure the session"
                );
                assert!(
                    received.is_empty(),
                    "one diagnostic sends exactly one settings update"
                );
                received.push(event);
                if let Some(updated) = updated.as_ref() {
                    socket
                        .send(tokio_tungstenite::tungstenite::Message::Text(
                            updated.to_string().into(),
                        ))
                        .await
                        .unwrap();
                }
            }
            received
        });
        (Realtime::connect_loopback(address).await.unwrap(), server)
    }
    #[tokio::test]
    async fn exact_selected_standard_and_wake_settings_handshake_without_audio_or_inference() {
        let mut config = Config::default();
        config.openai.voice = "cedar".into();
        config.audio.noise_reduction = "far_field".into();
        config.turn_detection.eagerness = "low".into();
        config.openai.input_transcription = true;
        for role in [Role::Standard, Role::Wake] {
            let selected = if role == Role::Wake {
                config.wake_config()
            } else {
                config.clone()
            };
            let expected = if role == Role::Wake {
                crate::wake::session_settings(&selected)
            } else {
                session_settings(&selected)
            };
            let mut effective = expected.clone();
            effective["model"] = serde_json::json!(selected.openai.model);
            let (mut socket,server)=socket_pair(serde_json::json!({"type":"session.created","session":{"model":selected.openai.model}}),Some(serde_json::json!({"type":"session.updated","session":effective}))).await;
            let mut phase = Phase::Connect;
            handshake(&mut socket, &selected.openai.model, &expected, &mut phase)
                .await
                .unwrap();
            assert_eq!(phase, Phase::Ready);
            drop(socket);
            let sent = server.await.unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0]["session"], expected);
            if role == Role::Wake {
                assert_eq!(expected["tools"], serde_json::json!([]));
                assert_eq!(expected["audio"]["input"]["transcription"], Value::Null);
                assert_eq!(
                    expected["audio"]["input"]["turn_detection"]["type"],
                    "server_vad"
                );
            }
        }
    }
    #[tokio::test]
    async fn mismatched_configuration_model_and_lifecycle_are_rejected() {
        let config = Config::default();
        let expected = session_settings(&config);
        for (created, mut effective, code) in [
            (
                serde_json::json!({"type":"session.updated"}),
                expected.clone(),
                "lifecycle_invalid",
            ),
            (
                serde_json::json!({"type":"session.created","session":{"model":"gpt-realtime-mini"}}),
                expected.clone(),
                "model_mismatch",
            ),
            (
                serde_json::json!({"type":"session.created","session":{"model":config.openai.model}}),
                expected.clone(),
                "configuration_mismatch",
            ),
        ] {
            effective["model"] = serde_json::json!(config.openai.model);
            effective["audio"]["output"]["voice"] = serde_json::json!("cedar");
            let (mut socket, server) = socket_pair(
                created,
                Some(serde_json::json!({"type":"session.updated","session":effective})),
            )
            .await;
            let error = handshake(
                &mut socket,
                &config.openai.model,
                &expected,
                &mut Phase::Connect,
            )
            .await
            .unwrap_err();
            assert_eq!(public_code(&error), code);
            drop(socket);
            server.await.unwrap();
        }
        assert!(!matches_model(
            &serde_json::json!("gpt-realtime-mini"),
            "gpt-realtime"
        ));
        assert!(matches_model(
            &serde_json::json!("gpt-realtime-2025-08-28"),
            "gpt-realtime"
        ));
    }
    #[tokio::test]
    async fn cancelled_or_timed_out_handshake_never_becomes_ready_or_requests_a_response() {
        let config = Config::default();
        let expected = session_settings(&config);
        let (mut socket, server) = socket_pair(
            serde_json::json!({"type":"session.created","session":{"model":config.openai.model}}),
            None,
        )
        .await;
        let mut phase = Phase::Connect;
        let (_timeout_cancel, mut timeout_receiver) = watch::channel(false);
        let timed_out = bounded_probe(
            handshake(&mut socket, &config.openai.model, &expected, &mut phase),
            &mut timeout_receiver,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert_eq!(public_code(&timed_out), "timeout");
        assert!(matches!(
            phase,
            Phase::SessionCreated | Phase::Configure | Phase::SessionUpdated
        ));
        drop(socket);
        assert!(server.await.unwrap().len() <= 1);
        let (cancel, receiver) = watch::channel(true);
        let report = check_live_with_cancel(&config, "private-key-never-sent", receiver).await;
        drop(cancel);
        assert!(!report.ready);
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.code == "cancelled" && !check.attempted)
        );
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("private-key")
        );
        let missing = LiveReport::credential_unavailable(&config);
        assert_eq!(missing.checks.len(), 2);
        assert!(!missing.ready);
        assert!(missing.checks.iter().all(|check| !check.attempted));
        for error in [
            io::Error::other("auth_failed: private-key"),
            io::Error::other("private remote body"),
        ] {
            assert!(!public_code(&error).contains("private"));
        }
    }
    #[tokio::test]
    async fn cancellation_after_configuration_closes_the_probe_before_any_ready_result() {
        let config = Config::default();
        let expected = session_settings(&config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (configured, observed) = tokio::sync::oneshot::channel();
        let model = config.openai.model.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::json!({"type":"session.created","session":{"model":model}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            let message = ws.next().await.unwrap().unwrap();
            let event: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(event["type"], "session.update");
            configured.send(()).unwrap();
            while let Some(Ok(message)) = ws.next().await {
                assert!(
                    !message.is_text(),
                    "cancelled diagnostics cannot submit model/audio work"
                );
            }
        });
        let mut socket = Realtime::connect_loopback(address).await.unwrap();
        let mut phase = Phase::Connect;
        let (cancel, mut receiver) = watch::channel(false);
        let cancelling = tokio::spawn(async move {
            observed.await.unwrap();
            cancel.send(true).unwrap();
        });
        let result = bounded_probe(
            handshake(&mut socket, &config.openai.model, &expected, &mut phase),
            &mut receiver,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(public_code(&result.unwrap_err()), "cancelled");
        assert_eq!(phase, Phase::SessionUpdated);
        drop(socket);
        cancelling.await.unwrap();
        server.await.unwrap();
    }
}
