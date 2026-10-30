//! One configured standard-model Realtime session kept open while Eva is
//! idle, so activation skips TCP, TLS, the WebSocket upgrade and session
//! configuration. Nothing is uploaded before activation: no audio, desktop
//! context, tools or response request, so an idle warm session has no token
//! cost. It carries no execution authority; the activation path still obtains
//! its permit and execution context from JWM exactly as for a cold connection.
use crate::{
    config::Config,
    errors::{self, Recovery},
    providers::openai_realtime::{Realtime, connect_configured},
};
use serde_json::{Value, json};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, watch};

/// Realtime sessions expire after an hour; rotate well before that.
const ROTATE_AFTER: Duration = Duration::from_secs(50 * 60);
const EXPIRY_MARGIN: Duration = Duration::from_secs(5 * 60);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const MIN_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(300);
const PERMANENT_RETRY: Duration = Duration::from_secs(600);
const TAKE_TIMEOUT: Duration = Duration::from_millis(250);

type ConnectFuture = Pin<Box<dyn Future<Output = io::Result<(Realtime, Option<u64>)>> + Send>>;
type Connector = Arc<dyn Fn(Config) -> ConnectFuture + Send + Sync>;

pub struct Warm {
    requests: mpsc::Sender<oneshot::Sender<Realtime>>,
    enabled: watch::Sender<bool>,
    status: Arc<Mutex<Value>>,
    task: tokio::task::JoinHandle<()>,
}
impl Warm {
    pub fn spawn(c: Config) -> Self {
        Self::spawn_with(
            c,
            Arc::new(|c: Config| {
                Box::pin(async move {
                    let key = c.api_key().await?;
                    connect_configured(&c, &key).await
                })
            }),
        )
    }
    fn spawn_with(c: Config, connector: Connector) -> Self {
        let (requests, request_rx) = mpsc::channel(4);
        let (enabled, enabled_rx) = watch::channel(false);
        let status = Arc::new(Mutex::new(json!({"state":"disabled"})));
        let task = tokio::spawn(run(c, connector, enabled_rx, request_rx, status.clone()));
        Self {
            requests,
            enabled,
            status,
            task,
        }
    }
    /// Enabled only while no voice session or dry-run owns the standard model.
    pub fn set_enabled(&self, enabled: bool) {
        if *self.enabled.borrow() != enabled {
            let _ = self.enabled.send(enabled);
        }
    }
    pub fn status(&self) -> Value {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    /// A configured session if one is ready within a short bound; otherwise
    /// `None` and the caller connects cold. This never waits for a connection.
    pub async fn take(&self) -> Option<Realtime> {
        if !*self.enabled.borrow() {
            return None;
        }
        let (reply, rx) = oneshot::channel();
        self.requests.try_send(reply).ok()?;
        tokio::time::timeout(TAKE_TIMEOUT, rx).await.ok()?.ok()
    }
    pub fn stop(&self) {
        self.task.abort();
    }
}
impl Drop for Warm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn set(status: &Arc<Mutex<Value>>, value: Value) {
    *status.lock().unwrap_or_else(|error| error.into_inner()) = value;
}

/// Rotate before the server expiry when it is known, never later than the
/// fixed limit. An expiry already in the past is treated as unknown.
fn rotation_deadline(connected_at: Instant, expires_at: Option<u64>, now_unix: u64) -> Instant {
    let fixed = connected_at + ROTATE_AFTER;
    match expires_at {
        Some(expires_at) if expires_at > now_unix => {
            let remaining =
                Duration::from_secs(expires_at - now_unix).saturating_sub(EXPIRY_MARGIN);
            fixed.min(connected_at + remaining)
        }
        _ => fixed,
    }
}

async fn run(
    c: Config,
    connector: Connector,
    mut enabled: watch::Receiver<bool>,
    mut requests: mpsc::Receiver<oneshot::Sender<Realtime>>,
    status: Arc<Mutex<Value>>,
) {
    let mut retry = MIN_RETRY;
    loop {
        // Mark the current flag as seen so `changed()` below only wakes for a
        // later toggle, even if the flag was set before this task first ran.
        while !*enabled.borrow_and_update() {
            set(&status, json!({"state":"disabled"}));
            if enabled.changed().await.is_err() {
                return;
            }
        }
        set(&status, json!({"state":"connecting"}));
        let connect = connector(c.clone());
        let outcome = tokio::select! {
            value = connect => value,
            _ = enabled.changed() => continue,
        };
        match outcome {
            Ok((ws, expires_at)) => {
                retry = MIN_RETRY;
                let connected_at = Instant::now();
                crate::telemetry::event(
                    "warm_ready",
                    &json!({"model":c.openai.model,"result":if expires_at.is_some(){"expiry_known"}else{"expiry_unknown"}}),
                );
                let rotate_at =
                    rotation_deadline(connected_at, expires_at, crate::store::wall_ms() / 1000);
                serve(
                    &c,
                    ws,
                    connected_at,
                    rotate_at,
                    &mut enabled,
                    &mut requests,
                    &status,
                )
                .await;
            }
            Err(error) => {
                let code = errors::session_code(&error);
                let wait = if errors::recovery(&error) == Recovery::Permanent {
                    PERMANENT_RETRY
                } else {
                    retry
                };
                retry = (retry * 2).min(MAX_RETRY);
                crate::telemetry::event(
                    "warm_connect_failed",
                    &json!({"model":c.openai.model,"result":code}),
                );
                set(
                    &status,
                    json!({"state":"waiting","error_code":code,"retry_in_ms":wait.as_millis() as u64}),
                );
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {},
                    _ = enabled.changed() => {},
                }
            }
        }
    }
}

/// Own the idle socket: answer pings, keep it alive, rotate before expiry,
/// and hand it over exactly once. Idle server events carry no authority and
/// are discarded; an error event or a closed socket ends this session.
async fn serve(
    c: &Config,
    mut ws: Realtime,
    connected_at: Instant,
    rotate_at: Instant,
    enabled: &mut watch::Receiver<bool>,
    requests: &mut mpsc::Receiver<oneshot::Sender<Realtime>>,
    status: &Arc<Mutex<Value>>,
) {
    let mut ping = tokio::time::interval_at(
        tokio::time::Instant::from_std(connected_at + PING_INTERVAL),
        PING_INTERVAL,
    );
    loop {
        set(
            status,
            json!({"state":"ready","age_ms":connected_at.elapsed().as_millis() as u64}),
        );
        tokio::select! {
            event = ws.next() => {
                if let Err(error) = event {
                    crate::telemetry::event("warm_lost",&json!({"model":c.openai.model,"result":errors::session_code(&error)}));
                    return;
                }
            }
            _ = ping.tick() => {
                if ws.ping().await.is_err() {
                    crate::telemetry::event("warm_lost",&json!({"model":c.openai.model,"result":"realtime_write_failed"}));
                    return;
                }
            }
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(rotate_at)) => {
                crate::telemetry::event("warm_rotated",&json!({"model":c.openai.model}));
                return;
            }
            changed = enabled.changed() => {
                if changed.is_err() || !*enabled.borrow() {
                    return;
                }
            }
            request = requests.recv() => {
                let Some(reply) = request else { return };
                if reply.is_closed() {
                    continue;
                }
                match reply.send(ws) {
                    Ok(()) => {
                        crate::telemetry::event("warm_taken",&json!({"model":c.openai.model,"result":"handed_over"}));
                        return;
                    }
                    Err(returned) => ws = returned,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::openai_realtime::complete_handshake;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A loopback "Realtime" server: creates sessions, confirms configuration,
    /// then idles (optionally closing right after configuration).
    async fn serve_sessions(
        listener: tokio::net::TcpListener,
        connections: Arc<AtomicUsize>,
        close_after_configuration: bool,
    ) {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            connections.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                let created = json!({"type":"session.created","session":{"expires_at":crate::store::wall_ms()/1000+3600}});
                if ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        created.to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                while let Some(Ok(message)) = ws.next().await {
                    let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                        continue;
                    };
                    let event: Value = serde_json::from_str(&text).unwrap();
                    if event["type"] == "session.update" {
                        let updated = json!({"type":"session.updated","session":event["session"]});
                        if ws
                            .send(tokio_tungstenite::tungstenite::Message::Text(
                                updated.to_string().into(),
                            ))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if close_after_configuration {
                            return;
                        }
                    }
                }
            });
        }
    }
    async fn loopback(close_after_configuration: bool) -> (Connector, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        tokio::spawn(serve_sessions(
            listener,
            connections.clone(),
            close_after_configuration,
        ));
        let connector: Connector = Arc::new(move |c: Config| {
            Box::pin(async move {
                let mut ws = Realtime::connect_loopback(address).await?;
                let expires_at = complete_handshake(&mut ws, &c).await?;
                Ok((ws, expires_at))
            })
        });
        (connector, connections)
    }
    async fn wait_state(warm: &Warm, state: &str) {
        for _ in 0..300 {
            if warm.status()["state"] == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("warm state {state} not reached: {}", warm.status());
    }
    #[tokio::test]
    async fn warm_session_is_configured_handed_over_once_and_reconnected() {
        let (connector, connections) = loopback(false).await;
        let warm = Warm::spawn_with(Config::default(), connector);
        assert!(
            warm.take().await.is_none(),
            "disabled warm never hands over"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(connections.load(Ordering::SeqCst), 0);
        warm.set_enabled(true);
        wait_state(&warm, "ready").await;
        assert!(warm.status()["age_ms"].is_u64());
        let ws = warm.take().await.expect("ready session is handed over");
        assert!(ws.is_configured());
        // The keeper reconnects for the next activation on its own.
        wait_state(&warm, "ready").await;
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        warm.set_enabled(false);
        wait_state(&warm, "disabled").await;
        assert!(warm.take().await.is_none());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(connections.load(Ordering::SeqCst), 2);
        drop(ws);
    }
    #[tokio::test]
    async fn closed_idle_session_is_replaced_without_waiting_for_a_take() {
        let (connector, connections) = loopback(true).await;
        let warm = Warm::spawn_with(Config::default(), connector);
        warm.set_enabled(true);
        for _ in 0..300 {
            if connections.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            connections.load(Ordering::SeqCst) >= 2,
            "a dropped idle session must be replaced"
        );
        warm.stop();
    }
    #[tokio::test]
    async fn failed_connections_back_off_and_report_the_error_code() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let connector: Connector = Arc::new(move |_c: Config| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(io::Error::other("auth_failed")) })
        });
        let warm = Warm::spawn_with(Config::default(), connector);
        warm.set_enabled(true);
        wait_state(&warm, "waiting").await;
        let status = warm.status();
        assert_eq!(status["error_code"], "auth_failed");
        assert_eq!(status["retry_in_ms"], PERMANENT_RETRY.as_millis() as u64);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no tight retry loop");
        assert!(warm.take().await.is_none());
    }
    #[test]
    fn rotation_prefers_the_earlier_of_server_expiry_and_fixed_limit() {
        let now = Instant::now();
        assert_eq!(rotation_deadline(now, None, 1_000), now + ROTATE_AFTER);
        assert_eq!(
            rotation_deadline(now, Some(1_000 + 20 * 60), 1_000),
            now + Duration::from_secs(15 * 60)
        );
        assert_eq!(
            rotation_deadline(now, Some(1_000 + 2 * 3600), 1_000),
            now + ROTATE_AFTER
        );
        assert_eq!(rotation_deadline(now, Some(500), 1_000), now + ROTATE_AFTER);
    }
}
