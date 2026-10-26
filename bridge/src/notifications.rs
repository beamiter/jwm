//! `org.freedesktop.Notifications` served on top of jwm's native toasts.
//!
//! The compositor owns the notification history, the toast cards, and the
//! identifiers; this interface only translates. Everything that decides *what*
//! jwm is told lives in pure functions below so the mapping (urgency hints,
//! expiry, default action) is unit tested without a session bus.

use std::collections::HashMap;

use serde_json::Value;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};
use zvariant::OwnedValue;

use crate::jwm_ipc::JwmIpc;

pub const PATH: &str = "/org/freedesktop/Notifications";
pub const NAME: &str = "org.freedesktop.Notifications";

/// jwm clamps toast timeouts to 800..30000 ms. A sender asking for a
/// notification that never expires gets the longest card jwm will draw; the
/// record stays in the notification center regardless.
const NEVER_EXPIRES_MS: u32 = 30_000;

pub struct Notifications {
    ipc: JwmIpc,
}

impl Notifications {
    #[must_use]
    pub fn new(ipc: JwmIpc) -> Self {
        Self { ipc }
    }
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    /// Post or replace a notification, returning jwm's identifier for it.
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        let args = notify_args(
            &app_name,
            replaces_id,
            &summary,
            &body,
            &actions,
            urgency_from_hints(&hints),
            expire_timeout,
        );
        let ipc = self.ipc.clone();
        let data = tokio::task::spawn_blocking(move || ipc.command("notify", args))
            .await
            .map_err(|error| zbus::fdo::Error::Failed(format!("bridge task failed: {error}")))?
            .map_err(|error| zbus::fdo::Error::Failed(format!("jwm notify failed: {error}")))?;

        let id = data.get("id").and_then(Value::as_u64).unwrap_or(0) as u32;
        log::debug!("Notify from {app_name:?} -> id {id}");
        Ok(id)
    }

    /// Close one notification. jwm broadcasts the close, which the event pump
    /// turns back into the `NotificationClosed` signal, so senders see exactly
    /// one close per notification however it was dismissed.
    async fn close_notification(&self, id: u32) -> zbus::fdo::Result<()> {
        let ipc = self.ipc.clone();
        let args = serde_json::json!({ "id": id, "reason": 3 });
        tokio::task::spawn_blocking(move || ipc.command("close_notification", args))
            .await
            .map_err(|error| zbus::fdo::Error::Failed(format!("bridge task failed: {error}")))?
            .map_err(|error| zbus::fdo::Error::Failed(format!("jwm close failed: {error}")))?;
        Ok(())
    }

    /// What this server supports. `body` and `actions` are real; `persistence`
    /// reflects the notification center keeping a bounded history.
    fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "actions".to_string(),
            "persistence".to_string(),
        ]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "jwm".to_string(),
            "jwm".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.2".to_string(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}

/// Urgency hint, defaulting to normal. The specification types it as a byte,
/// but senders have been seen using wider integers, so accept both.
fn urgency_from_hints(hints: &HashMap<String, OwnedValue>) -> u8 {
    let Some(value) = hints.get("urgency") else {
        return 1;
    };
    let level = u8::try_from(value)
        .map(u64::from)
        .or_else(|_| u32::try_from(value).map(u64::from))
        .or_else(|_| u64::try_from(value))
        .unwrap_or(1);
    level.min(2) as u8
}

/// The key jwm falls back to when it is too old to read the whole list.
///
/// `actions` is a flat `[key, label, key, label, ...]` list, and the whole of
/// it is now forwarded — the panel draws every button and lets the user pick.
/// This field stays for one reason: the bridge is installed separately from
/// the compositor, so a new bridge talking to an older jwm is a real
/// deployment, and that jwm understands only a single key. It applies the old
/// rule: the reserved `default`, else a lone action, else nothing.
fn default_action(actions: &[String]) -> Option<String> {
    let keys: Vec<&String> = actions.iter().step_by(2).collect();
    if let Some(explicit) = keys.iter().find(|key| key.as_str() == "default") {
        return Some((*explicit).clone());
    }
    match keys.as_slice() {
        [only] => Some((*only).clone()),
        _ => None,
    }
}

/// Translate the expiry the sender asked for into jwm's toast timeout.
/// `-1` means "server default" (jwm picks), `0` means "never expire".
fn toast_timeout_ms(expire_timeout: i32) -> u32 {
    match expire_timeout {
        0 => NEVER_EXPIRES_MS,
        timeout if timeout < 0 => 0,
        timeout => timeout as u32,
    }
}

/// Build the `notify` IPC arguments. Senders may leave both text fields empty;
/// jwm requires at least one, so the application name stands in.
fn notify_args(
    app_name: &str,
    replaces_id: u32,
    summary: &str,
    body: &str,
    actions: &[String],
    urgency: u8,
    expire_timeout: i32,
) -> Value {
    let title = if summary.trim().is_empty() && body.trim().is_empty() {
        if app_name.trim().is_empty() {
            "Notification"
        } else {
            app_name
        }
    } else {
        summary
    };
    serde_json::json!({
        "app": app_name,
        "title": title,
        "body": body,
        "urgency": urgency,
        "replaces_id": replaces_id,
        "timeout_ms": toast_timeout_ms(expire_timeout),
        // The whole list, in the order the sender offered it — the
        // specification requires that to be the display order.
        "actions": actions,
        // Ignored by any jwm that understands `actions`; see above.
        "default_action": default_action(actions),
    })
}

/// Turn jwm's `notification/*` events back into D-Bus signals.
///
/// jwm is the source of truth for closes: a toast that expired, a row the user
/// dismissed in the notification center, and a `CloseNotification` call all
/// arrive here the same way.
pub async fn pump_signals(connection: Connection, mut events: tokio::sync::mpsc::Receiver<Value>) {
    while let Some(event) = events.recv().await {
        let kind = event.get("event").and_then(Value::as_str).unwrap_or("");
        let payload = event.get("payload").cloned().unwrap_or(Value::Null);
        let Some(id) = payload.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let id = id as u32;

        let interface = match connection
            .object_server()
            .interface::<_, Notifications>(PATH)
            .await
        {
            Ok(interface) => interface,
            Err(error) => {
                log::warn!("cannot reach the served interface: {error}");
                continue;
            }
        };
        let emitter = interface.signal_emitter();

        let result = match kind {
            "notification/closed" => {
                let reason = payload
                    .get("reason")
                    .and_then(Value::as_u64)
                    .unwrap_or(4)
                    .min(u64::from(u32::MAX)) as u32;
                Notifications::notification_closed(emitter, id, reason).await
            }
            "notification/action" => {
                let action = payload
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_string();
                Notifications::action_invoked(emitter, id, action).await
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            log::warn!("failed to emit signal for {kind}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(keys: &[&str]) -> Vec<String> {
        keys.iter()
            .flat_map(|key| [(*key).to_string(), format!("{key} label")])
            .collect()
    }

    #[test]
    fn urgency_defaults_to_normal_without_a_hint() {
        assert_eq!(urgency_from_hints(&HashMap::new()), 1);
    }

    #[test]
    fn urgency_reads_the_byte_hint() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), OwnedValue::from(2u8));
        assert_eq!(urgency_from_hints(&hints), 2);
    }

    #[test]
    fn urgency_accepts_wider_integers_and_clamps() {
        let mut hints = HashMap::new();
        hints.insert("urgency".to_string(), OwnedValue::from(9u32));
        assert_eq!(urgency_from_hints(&hints), 2);
    }

    #[test]
    fn urgency_falls_back_when_the_hint_is_not_a_number() {
        let mut hints = HashMap::new();
        hints.insert(
            "urgency".to_string(),
            OwnedValue::from(zvariant::Str::from("critical")),
        );
        assert_eq!(urgency_from_hints(&hints), 1);
    }

    #[test]
    fn the_reserved_default_key_wins() {
        let list = actions(&["open", "default", "dismiss"]);
        assert_eq!(default_action(&list).as_deref(), Some("default"));
    }

    #[test]
    fn a_lone_action_is_unambiguous() {
        assert_eq!(default_action(&actions(&["open"])).as_deref(), Some("open"));
    }

    #[test]
    fn several_actions_without_a_default_choose_none() {
        assert_eq!(default_action(&actions(&["open", "dismiss"])), None);
    }

    #[test]
    fn no_actions_choose_none() {
        assert_eq!(default_action(&[]), None);
    }

    #[test]
    fn expiry_maps_onto_the_toast_timeout() {
        assert_eq!(toast_timeout_ms(-1), 0);
        assert_eq!(toast_timeout_ms(0), NEVER_EXPIRES_MS);
        assert_eq!(toast_timeout_ms(5_000), 5_000);
    }

    #[test]
    fn args_carry_every_field_jwm_needs() {
        let args = notify_args(
            "Thunderbird",
            7,
            "New mail",
            "from Ada",
            &actions(&["default"]),
            2,
            5_000,
        );
        assert_eq!(args["app"], "Thunderbird");
        assert_eq!(args["title"], "New mail");
        assert_eq!(args["body"], "from Ada");
        assert_eq!(args["urgency"], 2);
        assert_eq!(args["replaces_id"], 7);
        assert_eq!(args["timeout_ms"], 5_000);
        assert_eq!(args["default_action"], "default");
    }

    #[test]
    fn an_empty_notification_falls_back_to_the_application_name() {
        let args = notify_args("Backups", 0, "  ", "", &[], 1, -1);
        assert_eq!(args["title"], "Backups");
    }

    #[test]
    fn a_nameless_empty_notification_still_has_a_title() {
        let args = notify_args("", 0, "", "", &[], 1, -1);
        assert_eq!(args["title"], "Notification");
    }

    #[test]
    fn no_default_action_serializes_as_null() {
        let args = notify_args("app", 0, "s", "b", &actions(&["a", "b"]), 1, -1);
        assert!(args["default_action"].is_null());
    }

    #[test]
    fn the_whole_action_list_is_forwarded_in_order() {
        // jwm draws a button per action, so the flat list has to arrive
        // intact — `default_action` alone was throwing every other one away.
        let list = actions(&["open", "later"]);
        let args = notify_args("app", 0, "s", "b", &list, 1, -1);
        assert_eq!(
            args["actions"],
            serde_json::json!(["open", "open label", "later", "later label"])
        );
        // A sender that offered none sends none, not null.
        let args = notify_args("app", 0, "s", "b", &[], 1, -1);
        assert_eq!(args["actions"], serde_json::json!([]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discovery_replies_while_a_notification_waits_for_jwm() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        struct PrivateBus(Child);
        impl Drop for PrivateBus {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = match Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                assert!(
                    std::env::var_os("JWM_REQUIRE_DBUS_DAEMON").is_none(),
                    "dbus-daemon required: {error}"
                );
                return;
            }
        };
        let mut bus = PrivateBus(child);
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim();
        assert!(!address.is_empty(), "private bus did not start");
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Directory(std::env::temp_dir().join(format!(
                "jwm-notify-discovery-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        std::fs::create_dir(&directory.0).unwrap();
        let socket = directory.0.join("ipc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready, received) = tokio::sync::oneshot::channel();
        let (release, wait_for_release) = std::sync::mpsc::channel();
        let backend = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("notification IPC did not arrive: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = String::new();
            BufReader::new(&stream).read_line(&mut request).unwrap();
            assert!(request.contains("\"notify\""));
            let _ = ready.send(());
            let _ = wait_for_release.recv_timeout(Duration::from_secs(3));
            writeln!(stream, "{{\"success\":true,\"data\":{{\"id\":42}}}}").unwrap();
        });
        let _server = zbus::connection::Builder::address(address)
            .unwrap()
            .name(NAME)
            .unwrap()
            .serve_at(PATH, Notifications::new(JwmIpc::with_socket(socket)))
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address)
            .unwrap()
            .build()
            .await
            .unwrap();
        let notify_client = client.clone();
        let notification = tokio::spawn(async move {
            notify_client
                .call_method(
                    Some(NAME),
                    PATH,
                    Some(NAME),
                    "Notify",
                    &(
                        "test-app",
                        0u32,
                        "",
                        "discovery regression",
                        "",
                        Vec::<String>::new(),
                        HashMap::<String, OwnedValue>::new(),
                        1000i32,
                    ),
                )
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        // Chromium/Feishu can block startup on these discovery methods. They
        // must not wait behind a pending compositor command or subscription.
        let reply = tokio::time::timeout(
            Duration::from_secs(1),
            client.call_method(Some(NAME), PATH, Some(NAME), "GetCapabilities", &()),
        )
        .await
        .unwrap()
        .unwrap();
        let capabilities: Vec<String> = reply.body().deserialize().unwrap();
        assert_eq!(capabilities, ["body", "actions", "persistence"]);
        let info = tokio::time::timeout(
            Duration::from_secs(1),
            client.call_method(Some(NAME), PATH, Some(NAME), "GetServerInformation", &()),
        )
        .await
        .unwrap()
        .unwrap();
        let (name, _, _, spec): (String, String, String, String) =
            info.body().deserialize().unwrap();
        assert_eq!((name.as_str(), spec.as_str()), ("jwm", "1.2"));
        release.send(()).unwrap();
        let reply = notification.await.unwrap();
        assert_eq!(reply.body().deserialize::<u32>().unwrap(), 42);
        backend.await.unwrap();
    }
}
