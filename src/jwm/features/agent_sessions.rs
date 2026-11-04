//! Live terminal-owned session associations. JWM never owns a PTY or resumes a
//! provider. A reveal is completed only by a correlated terminal receipt.
use crate::{Jwm, backend::api::Backend, jwm::types::WMArgEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const MAX_SESSIONS: usize = 1000;
const MAX_REQUESTS: usize = 64;
const LEASE: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSession {
    pub session_id: String,
    pub project_id: String,
    pub provider: String,
    pub host_id: String,
    pub host_incarnation: String,
    pub pane_id: String,
    pub window_token: String,
    pub title: String,
    pub status: String,
    pub can_reveal: bool,
    pub revision: u64,
}
impl AgentSession {
    fn validate(&self) -> Result<(), String> {
        for value in [
            &self.session_id,
            &self.project_id,
            &self.host_id,
            &self.host_incarnation,
            &self.pane_id,
            &self.window_token,
        ] {
            if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                return Err("invalid_agent_session_identity".into());
            }
        }
        if !["anvil", "forge", "ember", "frost"].contains(&self.provider.as_str())
            || ![
                "running",
                "waiting",
                "succeeded",
                "failed",
                "unknown",
                "disconnected",
            ]
            .contains(&self.status.as_str())
            || self.title.len() > 512
            || self.title.chars().any(char::is_control)
            || self.revision == 0
        {
            return Err("invalid_agent_session_metadata".into());
        }
        Ok(())
    }
}
#[derive(Debug)]
struct Entry {
    session: AgentSession,
    seen: Instant,
}
#[derive(Clone, Debug, Serialize)]
struct Reveal {
    operation_id: String,
    session_id: String,
    host_id: String,
    host_incarnation: String,
    pane_id: String,
    window_token: String,
    revision: u64,
    status: String,
    #[serde(skip)]
    started: Instant,
}
#[derive(Debug, Default)]
pub(crate) struct AgentSessions {
    entries: BTreeMap<String, Entry>,
    requests: BTreeMap<String, Reveal>,
    revision: u64,
    sequence: u64,
    notices: BTreeMap<u32, AgentSession>,
}
impl AgentSessions {
    fn register(&mut self, session: AgentSession, now: Instant) -> Result<(), String> {
        session.validate()?;
        if let Some(old) = self.entries.get(&session.session_id) {
            if old.session.host_id != session.host_id
                || old.session.host_incarnation != session.host_incarnation
            {
                if now.saturating_duration_since(old.seen) <= LEASE {
                    return Err("agent_session_has_live_owner".into());
                }
            } else if session.revision < old.session.revision
                || (session.revision == old.session.revision && session != old.session)
            {
                return Err("stale_agent_session_revision".into());
            }
        } else if self.entries.len() >= MAX_SESSIONS {
            self.entries
                .retain(|_, entry| now.saturating_duration_since(entry.seen) <= LEASE);
            if self.entries.len() >= MAX_SESSIONS {
                return Err("agent_session_registry_full".into());
            }
        }
        let changed = self.entries.get(&session.session_id).is_none_or(|old| {
            old.session != session || now.saturating_duration_since(old.seen) > LEASE
        });
        if changed {
            for request in self
                .requests
                .values_mut()
                .filter(|r| r.session_id == session.session_id && r.status == "queued")
            {
                request.status = "superseded".into();
            }
        }
        self.entries
            .insert(session.session_id.clone(), Entry { session, seen: now });
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        Ok(())
    }
    fn retire_destroyed_association(&mut self, id: &str, windows: &Value) -> bool {
        let Some(windows) = windows.as_array() else {
            return false;
        };
        let gone = self.entries.get(id).is_some_and(|entry| {
            !windows
                .iter()
                .any(|window| window["target_token"] == entry.session.window_token)
        });
        if gone {
            self.entries.remove(id);
            for request in self
                .requests
                .values_mut()
                .filter(|r| r.session_id == id && matches!(r.status.as_str(), "queued" | "claimed"))
            {
                request.status = "superseded".into();
            }
        }
        gone
    }
    fn active(&self, id: &str, revision: u64, now: Instant) -> Result<AgentSession, String> {
        let entry = self.entries.get(id).ok_or("agent_session_gone")?;
        if entry.session.revision != revision {
            return Err("stale_agent_session_revision".into());
        }
        if now.saturating_duration_since(entry.seen) > LEASE
            || !entry.session.can_reveal
            || entry.session.status == "disconnected"
        {
            return Err("agent_session_disconnected_or_unsupported".into());
        }
        Ok(entry.session.clone())
    }
    fn settle(&mut self, now: Instant) {
        for request in self.requests.values_mut() {
            if matches!(request.status.as_str(), "queued" | "claimed")
                && now.saturating_duration_since(request.started) > REQUEST_TIMEOUT
            {
                request.status = "unknown".into();
            }
        }
    }
    fn queue(&mut self, session: &AgentSession, wm: &str, now: Instant) -> Result<Value, String> {
        self.settle(now);
        if let Some(request) = self.requests.values().find(|r| {
            r.session_id == session.session_id
                && r.host_id == session.host_id
                && r.host_incarnation == session.host_incarnation
                && r.pane_id == session.pane_id
                && r.window_token == session.window_token
                && r.revision == session.revision
                && matches!(r.status.as_str(), "queued" | "claimed")
        }) {
            return Ok(json!(request));
        }
        self.requests.retain(|_, request| {
            now.saturating_duration_since(request.started) < Duration::from_secs(60)
        });
        if self.requests.len() >= MAX_REQUESTS {
            return Err("agent_reveal_queue_full".into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("agent_reveal_sequence_exhausted")?;
        let operation_id = format!("{wm}:agent:{}", self.sequence);
        let request = Reveal {
            operation_id: operation_id.clone(),
            session_id: session.session_id.clone(),
            host_id: session.host_id.clone(),
            host_incarnation: session.host_incarnation.clone(),
            pane_id: session.pane_id.clone(),
            window_token: session.window_token.clone(),
            revision: session.revision,
            status: "queued".into(),
            started: now,
        };
        let result = json!(request);
        self.requests.insert(operation_id, request);
        Ok(result)
    }
    fn claim(&mut self, args: &Value, now: Instant) -> Result<Value, String> {
        self.settle(now);
        let id = args["operation_id"]
            .as_str()
            .ok_or("missing_operation_id")?;
        let request = self.requests.get(id).ok_or("agent_reveal_gone")?.clone();
        let current = self.active(&request.session_id, request.revision, now)?;
        for (field, expected) in [
            ("session_id", &request.session_id),
            ("host_id", &request.host_id),
            ("host_incarnation", &request.host_incarnation),
            ("pane_id", &request.pane_id),
            ("window_token", &request.window_token),
        ] {
            if args[field].as_str() != Some(expected.as_str()) {
                return Err("agent_reveal_identity_mismatch".into());
            }
        }
        if args["revision"] != request.revision
            || current.host_id != request.host_id
            || current.host_incarnation != request.host_incarnation
            || current.pane_id != request.pane_id
            || current.window_token != request.window_token
        {
            return Err("agent_reveal_association_changed".into());
        }
        let mut result = json!(request);
        if request.status == "claimed" {
            result["already_claimed"] = json!(true);
            return Ok(result);
        }
        if request.status != "queued" {
            return Err("agent_reveal_already_settled_or_unknown".into());
        }
        self.requests.get_mut(id).unwrap().status = "claimed".into();
        result["status"] = json!("claimed");
        result["already_claimed"] = json!(false);
        Ok(result)
    }
    fn receipt(&mut self, args: &Value, now: Instant) -> Result<Value, String> {
        self.settle(now);
        let request = self
            .requests
            .get(
                args["operation_id"]
                    .as_str()
                    .ok_or("missing_operation_id")?,
            )
            .ok_or("agent_reveal_gone")?
            .clone();
        for (key, value) in [
            ("host_id", &request.host_id),
            ("host_incarnation", &request.host_incarnation),
            ("session_id", &request.session_id),
            ("pane_id", &request.pane_id),
            ("window_token", &request.window_token),
        ] {
            if args[key].as_str() != Some(value.as_str()) {
                return Err("agent_reveal_identity_mismatch".into());
            }
        }
        let active = self.active(&request.session_id, request.revision, now)?;
        if active.host_id != request.host_id
            || active.host_incarnation != request.host_incarnation
            || active.pane_id != request.pane_id
            || active.window_token != request.window_token
        {
            return Err("agent_reveal_association_changed".into());
        }
        if args["revision"].as_u64() != Some(request.revision) {
            return Err("stale_agent_session_revision".into());
        }
        let status = args["status"]
            .as_str()
            .filter(|s| ["revealed", "failed"].contains(s))
            .ok_or("invalid_agent_reveal_status")?;
        if request.status == status {
            return Ok(json!(request));
        }
        if request.status != "claimed" {
            return Err("agent_reveal_not_claimed_or_already_settled".into());
        }
        let request = self.requests.get_mut(&request.operation_id).unwrap();
        request.status = status.into();
        Ok(json!(request))
    }
    pub(crate) fn activation_status(&mut self, operation: &str) -> Option<Value> {
        self.settle(Instant::now());
        self.requests.get(operation).map(|request| json!(request))
    }
    pub(crate) fn cancel_activation(&mut self, operation: &str) {
        if let Some(request) = self.requests.get_mut(operation)
            && request.status == "queued"
        {
            request.status = "cancelled".into();
        }
    }
    /// A live provider can anchor to its registered native window. Multiple
    /// sessions in one host remain one window association; competing hosts or
    /// expired registrations cannot identify a pane or own an anchor.
    pub(crate) fn window_anchor(&self, token: &str, now: Instant) -> Option<String> {
        let owners = self
            .entries
            .values()
            .filter(|entry| {
                entry.session.window_token == token
                    && entry.session.can_reveal
                    && entry.session.status != "disconnected"
                    && now.saturating_duration_since(entry.seen) <= LEASE
            })
            .map(|entry| (&entry.session.host_id, &entry.session.host_incarnation))
            .collect::<std::collections::BTreeSet<_>>();
        if owners.len() != 1 {
            return None;
        }
        let (host, incarnation) = owners.into_iter().next()?;
        Some(jwm_eva_core::protocol::digest(
            &json!({"window_token":token,"host_id":host,"host_incarnation":incarnation}),
        ))
    }
    fn summary(&self, windows: &std::collections::HashSet<String>, now: Instant) -> Value {
        let (mut running, mut waiting, mut unknown, mut disconnected, mut completed, mut failed) =
            (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
        for entry in self.entries.values() {
            if !windows.contains(&entry.session.window_token)
                || now.saturating_duration_since(entry.seen) > LEASE
            {
                disconnected += 1;
                continue;
            }
            match entry.session.status.as_str() {
                "running" => running += 1,
                "waiting" => waiting += 1,
                "disconnected" => disconnected += 1,
                "succeeded" => completed += 1,
                "failed" => failed += 1,
                _ => unknown += 1,
            }
        }
        json!({"protocol_version":1,"revision":self.revision,"running":running,"waiting":waiting,"unknown":unknown,"total":running+waiting+unknown+completed+failed,"completed":completed,"failed":failed,"disconnected":disconnected,"available":true})
    }
    pub(crate) fn snapshot(&self, context: &Value) -> Value {
        let now = Instant::now();
        let sessions = self
            .entries
            .values()
            .map(|entry| {
                let mut value = json!(entry.session);
                let digest=jwm_eva_core::protocol::digest(&json!({"session_id":entry.session.session_id,"host_incarnation":entry.session.host_incarnation}));
                value["session_ref"]=json!(format!("s{}",&digest[..31]));
                value["association_token"] = json!(jwm_eva_core::protocol::digest(&json!({"session_id":entry.session.session_id,"host_incarnation":entry.session.host_incarnation,"pane_id":entry.session.pane_id,"window_token":entry.session.window_token,"revision":entry.session.revision})));
                let window = context["windows"].as_array().and_then(|windows| {
                    windows
                        .iter()
                        .find(|w| w["target_token"] == entry.session.window_token)
                });
                if let Some(window) = window {
                    value["monitor_token"] = window["monitor_token"].clone();
                    value["tag_mask"] = window["tag_mask"].clone();
                    value["monitor"]=context["monitors"].as_array().and_then(|monitors|monitors.iter().find(|m|m["monitor_token"]==window["monitor_token"])).map(|m|m["num"].clone()).unwrap_or(Value::Null);
                }
                if window.is_none() || now.saturating_duration_since(entry.seen) > LEASE {
                    value["status"] = json!("disconnected");
                    value["can_reveal"] = json!(false);
                }
                value
            })
            .collect::<Vec<_>>();
        json!({"protocol_version":1,"wm_instance_id":context["wm_instance_id"],"revision":self.revision,"sessions":sessions,"provider_owns_lifecycle":true})
    }
}
impl Jwm {
    pub(crate) fn agent_summary(&self) -> Value {
        let windows = self
            .state
            .clients
            .keys()
            .map(|key| self.eva_window_token(key))
            .collect();
        self.features
            .agent_sessions
            .summary(&windows, Instant::now())
    }
    pub(crate) fn open_agent_session_picker(
        &mut self,
        backend: &mut dyn Backend,
    ) -> Result<(), String> {
        if self.features.system_ui.is_locked() || !self.features.monitor_lock.is_empty() {
            return Err("session_locked".into());
        }
        if !self.features.system_ui.is_launcher() {
            self.app_launcher(backend, &WMArgEnum::Int(0))
                .map_err(|error| error.to_string())?;
        }
        let snapshot = self.agent_sessions_snapshot();
        self.features.system_ui.set_agent_sessions(&snapshot);
        if let super::system_ui::SystemUiState::Launcher { query, .. } =
            &mut self.features.system_ui
        {
            query.clear();
        }
        for ch in "sessions:".chars() {
            self.features.system_ui.push_char(ch);
        }
        self.mark_system_ui_dirty();
        Ok(())
    }

    fn reconcile_agent_notice(
        &mut self,
        backend: &mut dyn Backend,
        previous: Option<&AgentSession>,
        current: &AgentSession,
    ) {
        let live: std::collections::HashSet<_> = self
            .features
            .notifications
            .recent()
            .filter(|record| record.app == "JWM Agents")
            .map(|record| record.id)
            .collect();
        self.features
            .agent_sessions
            .notices
            .retain(|id, _| live.contains(id));
        let mut existing = self
            .features
            .agent_sessions
            .notices
            .iter()
            .find(|(_, session)| session.session_id == current.session_id)
            .map(|(id, _)| *id);
        if let Some(id) = existing
            && self
                .features
                .agent_sessions
                .notices
                .get(&id)
                .is_some_and(|old| {
                    old.host_incarnation != current.host_incarnation
                        || old.window_token != current.window_token
                        || old.pane_id != current.pane_id
                })
        {
            self.features.agent_sessions.notices.remove(&id);
            self.close_notification(id, super::notifications::CloseReason::Requested);
            existing = None;
        }
        if current.status != "waiting" {
            if let Some(id) = existing {
                self.features.agent_sessions.notices.remove(&id);
                self.close_notification(id, super::notifications::CloseReason::Requested);
            }
            return;
        }
        let transitioned = previous.is_some_and(|old| {
            old.status != "waiting"
                || old.host_incarnation != current.host_incarnation
                || old.window_token != current.window_token
                || old.pane_id != current.pane_id
        });
        if !transitioned {
            if let Some(id) = existing {
                self.features
                    .agent_sessions
                    .notices
                    .insert(id, current.clone());
            }
            return;
        }
        let request = super::notifications::NotificationRequest {
            app: "JWM Agents".into(),
            summary: "Agent waiting for you".into(),
            body: format!("{} · {}", current.provider, current.title),
            urgency: 0,
            replaces_id: existing.unwrap_or(0),
            actions: vec![super::notifications::NotificationAction {
                key: "agent_return".into(),
                label: "Return to session".into(),
            }],
        };
        let id = self.post_notification(backend, &request, 4000);
        self.features
            .agent_sessions
            .notices
            .insert(id, current.clone());
        while self.features.agent_sessions.notices.len() > super::notifications::MAX_HISTORY {
            if let Some(id) = self.features.agent_sessions.notices.keys().next().copied() {
                self.features.agent_sessions.notices.remove(&id);
            }
        }
    }

    pub(crate) fn invoke_agent_or_notification_action(
        &mut self,
        backend: &mut dyn Backend,
        id: u32,
        action: &str,
    ) -> bool {
        let target = (action == "agent_return"
            && self
                .features
                .notifications
                .get(id)
                .is_some_and(|record| record.app == "JWM Agents"))
        .then(|| self.features.agent_sessions.notices.get(&id).cloned())
        .flatten();
        if target.is_some()
            && (self.features.system_ui.is_locked() || !self.features.monitor_lock.is_empty())
        {
            return false;
        }
        if !self.invoke_notification_action(id, action) {
            return false;
        }
        self.features.agent_sessions.notices.remove(&id);
        if let Some(target) = target {
            self.close_system_ui(backend);
            let current = self.features.agent_sessions.active(
                &target.session_id,
                target.revision,
                Instant::now(),
            );
            let result = current.and_then(|current| {
                if current.host_incarnation != target.host_incarnation
                    || current.window_token != target.window_token
                    || current.pane_id != target.pane_id
                {
                    return Err("agent_session_association_changed".into());
                }
                self.activate_agent_session(backend, &target.session_id, target.revision)
            });
            if let Err(error) = result {
                self.push_system_toast(
                    backend,
                    crate::backend::api::ToastNotification {
                        title: "Agent session unavailable".into(),
                        body: error,
                        ..Default::default()
                    },
                );
            }
        }
        true
    }
    pub(crate) fn agent_sessions_snapshot(&mut self) -> Value {
        let context = self.eva_execution_context();
        self.features.agent_sessions.snapshot(&context)
    }
    fn publish_agent_sessions(&mut self) -> Value {
        let snapshot = self.agent_sessions_snapshot();
        if self.features.system_ui.is_launcher() {
            self.features.system_ui.set_agent_sessions(&snapshot);
            self.mark_system_ui_dirty();
        }
        self.broadcast_ipc_event("agent/sessions", snapshot.clone());
        snapshot
    }
    pub(crate) fn activate_agent_session(
        &mut self,
        backend: &mut dyn Backend,
        id: &str,
        revision: u64,
    ) -> Result<Value, String> {
        if self.features.system_ui.is_locked() || !self.features.monitor_lock.is_empty() {
            return Err("session_locked".into());
        }
        let now = Instant::now();
        let session = self.features.agent_sessions.active(id, revision, now)?;
        let context = self.eva_execution_context();
        let window = context["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|w| w["target_token"] == session.window_token)
            })
            .ok_or("agent_window_gone")?;
        let raw = window["id"]
            .as_str()
            .and_then(|id| id.parse::<u64>().ok())
            .ok_or("agent_window_gone")?;
        // Check queue capacity before changing focus. No network, model call or
        // process creation is performed by this command on the event loop.
        self.features.agent_sessions.settle(now);
        if self.features.agent_sessions.requests.len() >= MAX_REQUESTS
            && !self
                .features
                .agent_sessions
                .requests
                .values()
                .any(|r| r.session_id == id && matches!(r.status.as_str(), "queued" | "claimed"))
        {
            return Err("agent_reveal_queue_full".into());
        }
        self.focus_window(backend, &WMArgEnum::UInt64(raw))
            .map_err(|e| e.to_string())?;
        let current = self.eva_execution_context();
        if current["focus_token"] != session.window_token {
            return Err("agent_focus_postcondition_failed".into());
        }
        let result = self.features.agent_sessions.queue(
            &session,
            context["wm_instance_id"]
                .as_str()
                .ok_or("missing_wm_instance_id")?,
            now,
        )?;
        self.broadcast_ipc_event("agent/activation", result.clone());
        Ok(result)
    }
    pub(crate) fn agent_session_command(
        &mut self,
        backend: &mut dyn Backend,
        name: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let context = self.eva_execution_context();
        if args["wm_instance_id"] != context["wm_instance_id"] {
            return Err("stale_wm_instance".into());
        }
        match name {
            "register_agent_session" => {
                let session: AgentSession = serde_json::from_value(args["session"].clone())
                    .map_err(|_| "invalid_agent_session")?;
                if !context["windows"].as_array().is_some_and(|windows| {
                    windows
                        .iter()
                        .any(|w| w["target_token"] == session.window_token)
                }) {
                    return Err("agent_window_gone".into());
                }
                // A destroyed WM lifetime is authoritative absence of that
                // window association. It is not evidence that an old PTY or
                // provider process has retired; those remain terminal-owned.
                self.features
                    .agent_sessions
                    .retire_destroyed_association(&session.session_id, &context["windows"]);
                let previous = self
                    .features
                    .agent_sessions
                    .entries
                    .get(&session.session_id)
                    .map(|entry| entry.session.clone());
                self.features
                    .agent_sessions
                    .register(session.clone(), Instant::now())?;
                self.reconcile_agent_notice(backend, previous.as_ref(), &session);
                Ok(self.publish_agent_sessions())
            }
            "unregister_agent_session" => {
                let id = args["session_id"].as_str().ok_or("missing_session_id")?;
                if let Some(entry) = self.features.agent_sessions.entries.get(id) {
                    if args["host_id"].as_str() != Some(&entry.session.host_id)
                        || args["host_incarnation"].as_str()
                            != Some(&entry.session.host_incarnation)
                    {
                        return Err("agent_session_owner_mismatch".into());
                    }
                }
                if self.features.agent_sessions.entries.remove(id).is_some() {
                    self.features.agent_sessions.revision =
                        self.features.agent_sessions.revision.saturating_add(1);
                }
                Ok(self.publish_agent_sessions())
            }
            "activate_agent_session" => self.activate_agent_session(
                backend,
                args["session_id"].as_str().ok_or("missing_session_id")?,
                args["revision"]
                    .as_u64()
                    .ok_or("missing_session_revision")?,
            ),
            "claim_agent_session_reveal" => {
                if self.features.system_ui.is_locked() || !self.features.monitor_lock.is_empty() {
                    return Err("session_locked".into());
                }
                if !context["windows"].as_array().is_some_and(|windows| {
                    windows
                        .iter()
                        .any(|window| window["target_token"] == args["window_token"])
                }) {
                    return Err("agent_window_gone".into());
                }
                self.features.agent_sessions.claim(args, Instant::now())
            }
            "agent_session_revealed" => {
                if self.features.system_ui.is_locked() || !self.features.monitor_lock.is_empty() {
                    return Err("session_locked".into());
                }
                if !context["windows"].as_array().is_some_and(|windows| {
                    windows
                        .iter()
                        .any(|w| w["target_token"] == args["window_token"])
                }) {
                    return Err("agent_window_gone".into());
                }
                let result = self.features.agent_sessions.receipt(args, Instant::now())?;
                self.broadcast_ipc_event("agent/activation", result.clone());
                Ok(result)
            }
            _ => Err("unsupported_agent_session_command".into()),
        }
    }
    pub(crate) fn agent_session_requests(&mut self, args: &Value) -> Value {
        self.features.agent_sessions.settle(Instant::now());
        let requests = self
            .features
            .agent_sessions
            .requests
            .values()
            .filter(|r| {
                args["host_id"] == r.host_id && args["host_incarnation"] == r.host_incarnation
            })
            .map(|r| json!(r))
            .collect::<Vec<_>>();
        json!({"requests":requests})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> AgentSession {
        AgentSession {
            session_id: "codex:one".into(),
            project_id: "project".into(),
            provider: "forge".into(),
            host_id: "host".into(),
            host_incarnation: "boot:one".into(),
            pane_id: "pane:one".into(),
            window_token: "wm:w:1000001".into(),
            title: "same project".into(),
            status: "waiting".into(),
            can_reveal: true,
            revision: 1,
        }
    }
    #[test]
    fn live_owner_revision_and_lease_prevent_reused_targets() {
        let now = Instant::now();
        let mut registry = AgentSessions::default();
        let original = session();
        registry.register(original.clone(), now).unwrap();
        let mut other = original.clone();
        other.host_incarnation = "boot:two".into();
        assert!(registry.register(other.clone(), now).is_err());
        assert!(
            registry
                .active(
                    &original.session_id,
                    1,
                    now + LEASE + Duration::from_secs(1)
                )
                .is_err()
        );
        registry
            .register(other, now + LEASE + Duration::from_secs(1))
            .unwrap();
        let mut changed = original;
        changed.pane_id = "pane:two".into();
        assert!(
            registry
                .register(changed, now + LEASE + Duration::from_secs(1))
                .is_err()
        );
    }
    #[test]
    fn summary_only_counts_current_window_lifetimes_and_distinguishes_unknown_results() {
        let now = Instant::now();
        let mut registry = AgentSessions::default();
        let mut entry = session();
        entry.status = "running".into();
        registry.register(entry.clone(), now).unwrap();
        let windows = std::collections::HashSet::from([entry.window_token.clone()]);
        assert_eq!(registry.summary(&windows, now)["running"], 1);
        assert_eq!(
            registry.summary(&std::collections::HashSet::new(), now)["running"],
            0
        );
        entry.status = "succeeded".into();
        entry.revision += 1;
        registry.register(entry, now).unwrap();
        let summary = registry.summary(&windows, now);
        assert_eq!(summary["unknown"], 0);
        assert_eq!(summary["completed"], 1);
        assert_eq!(
            registry.summary(&windows, now + LEASE + Duration::from_secs(1))["disconnected"],
            1
        );
    }

    #[test]
    fn waiting_transition_reuses_one_notification_and_click_uses_live_registered_identity() {
        use crate::{
            backend::common_define::WindowId,
            core::models::WMClient,
            jwm::monitor::test_support::{DisplaySpyBackend, output},
        };
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 800, 600)]);
        let mut wm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let monitor = wm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(450));
        client.mon = Some(monitor);
        client.state.tags = 1;
        let key = wm.insert_client(client);
        wm.attach_to_monitor(key, monitor);
        let mut current = session();
        current.window_token = wm.eva_window_token(key);
        current.status = "running".into();
        let args = |wm: &Jwm, current: &AgentSession| json!({"wm_instance_id":wm.eva_overlay_snapshot()["wm_instance_id"],"session":current});
        let registration = args(&wm, &current);
        wm.agent_session_command(&mut backend, "register_agent_session", &registration)
            .unwrap();
        assert!(wm.features.agent_sessions.notices.is_empty());
        current.status = "waiting".into();
        current.revision += 1;
        let registration = args(&wm, &current);
        wm.agent_session_command(&mut backend, "register_agent_session", &registration)
            .unwrap();
        let id = *wm.features.agent_sessions.notices.keys().next().unwrap();
        wm.agent_session_command(&mut backend, "register_agent_session", &registration)
            .unwrap();
        assert_eq!(wm.features.agent_sessions.notices.len(), 1);
        assert!(wm.invoke_agent_or_notification_action(&mut backend, id, "agent_return"));
        assert_eq!(wm.features.agent_sessions.requests.len(), 1);
        assert!(!wm.invoke_agent_or_notification_action(&mut backend, id, "agent_return"));
    }
    #[test]
    fn companion_anchor_requires_one_live_native_owner_and_cannot_follow_recycled_tokens() {
        let now = Instant::now();
        let mut registry = AgentSessions::default();
        let first = session();
        registry.register(first.clone(), now).unwrap();
        assert!(registry.window_anchor(&first.window_token, now).is_some());
        assert!(
            registry
                .window_anchor("replacement-window-lifetime", now)
                .is_none()
        );
        assert!(
            registry
                .window_anchor(&first.window_token, now + LEASE + Duration::from_millis(1))
                .is_none()
        );
        let mut second = first.clone();
        second.session_id = "second-session".into();
        second.pane_id = "another-pane".into();
        registry.register(second.clone(), now).unwrap();
        assert!(registry.window_anchor(&first.window_token, now).is_some());
        second.host_id = "competing-host".into();
        second.session_id = "competing-session".into();
        registry.register(second, now).unwrap();
        assert!(registry.window_anchor(&first.window_token, now).is_none());
    }
    #[test]
    fn activation_requires_exact_current_receipt_and_timeout_is_unknown() {
        let now = Instant::now();
        let mut registry = AgentSessions::default();
        let original = session();
        registry.register(original.clone(), now).unwrap();
        let request = registry.queue(&original, "wm", now).unwrap();
        assert_eq!(
            registry.queue(&original, "wm", now).unwrap()["operation_id"],
            request["operation_id"]
        );
        assert_eq!(
            registry.claim(&request, now).unwrap()["already_claimed"],
            false
        );
        assert_eq!(
            registry.claim(&request, now).unwrap()["already_claimed"],
            true
        );
        let mut receipt = request;
        receipt["status"] = json!("revealed");
        receipt["pane_id"] = json!("wrong-pane");
        assert!(registry.receipt(&receipt, now).is_err());
        receipt["pane_id"] = json!(original.pane_id);
        assert_eq!(
            registry.receipt(&receipt, now).unwrap()["status"],
            "revealed"
        );
        assert_eq!(
            registry.receipt(&receipt, now).unwrap()["status"],
            "revealed"
        );
        let next = registry.queue(&original, "wm", now).unwrap();
        registry.claim(&next, now).unwrap();
        let mut late = next.clone();
        late["status"] = json!("revealed");
        assert!(
            registry
                .receipt(&late, now + REQUEST_TIMEOUT + Duration::from_millis(1))
                .is_err()
        );
        assert_eq!(
            registry.requests[next["operation_id"].as_str().unwrap()].status,
            "unknown"
        );
    }
}

#[cfg(test)]
mod restart_association_tests {
    use super::*;
    #[test]
    fn a_destroyed_window_allows_immediate_new_host_without_retiring_its_provider() {
        let now = Instant::now();
        let mut registry = AgentSessions::default();
        let mut session = AgentSession {
            session_id: "stable-session".into(),
            project_id: "project".into(),
            provider: "forge".into(),
            host_id: "old-host".into(),
            host_incarnation: "old-incarnation".into(),
            pane_id: "old-pane".into(),
            window_token: "wm:w:old-generation".into(),
            title: "same title".into(),
            status: "unknown".into(),
            can_reveal: true,
            revision: 1,
        };
        registry.register(session.clone(), now).unwrap();
        let request = registry.queue(&session, "wm", now).unwrap();
        assert!(!registry.retire_destroyed_association(&session.session_id, &Value::Null));
        assert!(!registry.retire_destroyed_association(
            &session.session_id,
            &json!([{"target_token":session.window_token}])
        ));
        session.host_id = "new-host".into();
        session.host_incarnation = "new-incarnation".into();
        session.window_token = "wm:w:new-generation".into();
        assert!(
            registry.register(session.clone(), now).is_err(),
            "a live old window remains bound"
        );
        assert!(registry.retire_destroyed_association(
            &session.session_id,
            &json!([{"target_token":session.window_token,"id":"same-reused-raw-id"}])
        ));
        registry.register(session, now).unwrap();
        assert_eq!(
            registry.requests[request["operation_id"].as_str().unwrap()].status,
            "superseded"
        );
    }
}
