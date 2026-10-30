//! Bounded desktop observations and utterance-time focus binding.
//! Socket receipt times are diagnostics; only JWM's monotonic focus history
//! can establish which window was current when the microphone captured speech.
use jwm_ipc_client::{JwmIpc, SubscriptionEvent};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, mpsc};

#[derive(Debug, Clone)]
pub struct UtteranceBinding {
    pub connection_epoch: u64,
    pub onset_monotonic_ns: u64,
    pub wm_instance_id: String,
}
#[derive(Debug, Clone)]
pub struct Observation {
    pub received_at: Instant,
    pub focus_token: Option<String>,
    pub monitor_token: Option<String>,
    pub observed_monotonic_ns: Option<u64>,
}
#[derive(Debug, Default)]
pub struct ContextStore {
    pub connection_epoch: u64,
    pub snapshot: Option<Value>,
    pub observations: VecDeque<Observation>,
    connected: bool,
    /// First successful observation of this connection epoch, independent of
    /// the bounded focus-change deque's later eviction.
    connected_since: Option<Instant>,
}
impl ContextStore {
    pub fn invalidate(&mut self) {
        self.connection_epoch = self.connection_epoch.wrapping_add(1);
        self.snapshot = None;
        self.observations.clear();
        self.connected = false;
        self.connected_since = None;
    }
    pub fn observe(&mut self, context: Value, received_at: Instant) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|old| old["wm_instance_id"] != context["wm_instance_id"])
        {
            self.invalidate();
        }
        if self.connected_since.is_none() {
            self.connected_since = Some(received_at);
        }
        let focus = context["focus_token"].as_str().map(str::to_owned);
        let monitor = context["selected_monitor_token"]
            .as_str()
            .map(str::to_owned);
        if self
            .observations
            .back()
            .is_none_or(|old| old.focus_token != focus || old.monitor_token != monitor)
        {
            self.observations.push_back(Observation {
                received_at,
                focus_token: focus,
                monitor_token: monitor,
                observed_monotonic_ns: context["observed_monotonic_ns"].as_u64(),
            });
        }
        while self.observations.len() > 128 {
            self.observations.pop_front();
        }
        self.snapshot = Some(context);
        self.connected = true;
    }
    pub fn bind_onset(&self, captured: Instant) -> io::Result<UtteranceBinding> {
        if !self.connected {
            return Err(io::Error::other("context_not_connected"));
        }
        if self
            .connected_since
            .is_none_or(|baseline| captured < baseline)
        {
            return Err(io::Error::other("context_reconnected"));
        }
        let context = self
            .snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("context_not_ready"))?;
        let onset_monotonic_ns = instant_to_monotonic_ns(captured)?;
        Ok(UtteranceBinding {
            connection_epoch: self.connection_epoch,
            onset_monotonic_ns,
            wm_instance_id: context["wm_instance_id"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing_wm_instance_id"))?
                .into(),
        })
    }
    /// A wake handoff must retain the output observed before speech, rather
    /// than whichever output is selected after cloud wake detection finishes.
    pub fn monitor_at(&self, captured: Instant) -> io::Result<String> {
        self.bind_onset(captured)?;
        monitor_at_onset(
            self.snapshot
                .as_ref()
                .ok_or_else(|| io::Error::other("context_not_ready"))?,
            instant_to_monotonic_ns(captured)?,
        )
    }

    pub fn binding_current(&self, binding: &UtteranceBinding) -> bool {
        self.connected
            && self.connection_epoch == binding.connection_epoch
            && self.snapshot.as_ref().is_some_and(|context| {
                context["wm_instance_id"].as_str() == Some(binding.wm_instance_id.as_str())
                    && context["session_unlocked"] == true
            })
    }
    pub fn validate_commit(
        &self,
        binding: &UtteranceBinding,
        committed_ns: u64,
        mut latest: Value,
    ) -> io::Result<Value> {
        planning_monitor_scope(&mut latest)?;
        if !self.connected
            || self.connection_epoch != binding.connection_epoch
            || latest["wm_instance_id"].as_str() != Some(&binding.wm_instance_id)
        {
            return Err(io::Error::other("context_reconnected"));
        }
        if latest["session_unlocked"] != true {
            return Err(io::Error::other("session_locked"));
        }
        let focus = match focus_during_utterance(&latest, binding.onset_monotonic_ns, committed_ns)
        {
            Ok(focus) => focus,
            Err(error) if error.to_string() == "speech_has_no_focused_window" => {
                latest["focus_token"] = Value::Null;
                latest["speech_focus_ref"] = Value::Null;
                if let Some(windows) = latest["windows"].as_array_mut() {
                    for window in windows {
                        window["focused"] = json!(false);
                    }
                }
                return Ok(latest);
            }
            Err(error) => return Err(error),
        };
        let window = latest["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|w| w["target_token"].as_str() == Some(focus.as_str()))
            })
            .cloned()
            .ok_or_else(|| io::Error::other("speech_target_gone"))?;
        latest["focus_token"] = json!(focus);
        latest["speech_focus_ref"] = window["window_ref"].clone();
        if latest
            .get("activation_monitor_token")
            .is_none_or(Value::is_null)
        {
            latest["selected_monitor_token"] = window["monitor_token"].clone();
        }
        if let Some(windows) = latest["windows"].as_array_mut() {
            for w in windows {
                w["focused"] = json!(w["target_token"] == focus);
            }
        }
        Ok(latest)
    }
}

/// Preserve the visible output selected at native activation. It can differ
/// from JWM's selected monitor (for example when that output is not presentable).
/// Missing/null is the compatibility path for older servers, never a reason
/// to replace a supplied but stale activation token with a different monitor.
pub fn planning_monitor_scope(context: &mut Value) -> io::Result<()> {
    let Some(token) = context
        .get("activation_monitor_token")
        .filter(|token| !token.is_null())
    else {
        return Ok(());
    };
    let token = token
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| io::Error::other("stale_context: invalid activation monitor token"))?;
    if !context["monitors"].as_array().is_some_and(|monitors| {
        monitors
            .iter()
            .any(|monitor| monitor["monitor_token"].as_str() == Some(token))
    }) {
        return Err(io::Error::other("target_gone: activation monitor"));
    }
    let token = token.to_owned();
    context["selected_monitor_token"] = json!(token);
    Ok(())
}

/// Returns a token only if the server's retained history covers the whole
/// utterance and contains no focus transition within it. A 1ms boundary band
/// handles clock-read scheduling without pretending timestamps are exact.
pub fn focus_during_utterance(
    context: &Value,
    onset_ns: u64,
    commit_ns: u64,
) -> io::Result<String> {
    if onset_ns > commit_ns {
        return Err(io::Error::other("invalid_utterance_interval"));
    }
    let observed = context["observed_monotonic_ns"]
        .as_u64()
        .ok_or_else(|| io::Error::other("focus_history_unavailable"))?;
    if observed < commit_ns {
        return Err(io::Error::other("focus_history_does_not_cover_commit"));
    }
    let history = context["focus_history"]
        .as_array()
        .ok_or_else(|| io::Error::other("focus_history_unavailable"))?;
    let mut previous_time = None;
    let mut focus = None;
    for entry in history {
        let at = entry["at_monotonic_ns"]
            .as_u64()
            .ok_or_else(|| io::Error::other("focus_history_invalid"))?;
        if previous_time.is_some_and(|previous| at < previous) {
            return Err(io::Error::other("focus_history_unordered"));
        }
        previous_time = Some(at);
        if at <= onset_ns {
            focus = Some(entry["focus_token"].as_str().map(str::to_owned));
        }
        // Any transition near either edge is ambiguous, including A→B→A.
        if at > onset_ns.saturating_sub(1_000_000) && at <= commit_ns.saturating_add(1_000_000) {
            return Err(io::Error::other("focus_changed_during_speech"));
        }
    }
    focus
        .ok_or_else(|| io::Error::other("focus_history_unavailable"))?
        .ok_or_else(|| io::Error::other("speech_has_no_focused_window"))
}

pub fn monitor_at_onset(context: &Value, onset: u64) -> io::Result<String> {
    if context["observed_monotonic_ns"]
        .as_u64()
        .is_none_or(|observed| observed < onset)
    {
        return Err(io::Error::other("speech_monitor_unavailable"));
    }
    let history = context["selected_monitor_history"]
        .as_array()
        .ok_or_else(|| io::Error::other("speech_monitor_unavailable"))?;
    let mut previous = None;
    let mut selected = None;
    for entry in history {
        let at = entry["at_monotonic_ns"]
            .as_u64()
            .ok_or_else(|| io::Error::other("speech_monitor_unavailable"))?;
        if previous.is_some_and(|old| at < old) {
            return Err(io::Error::other("speech_monitor_unavailable"));
        }
        previous = Some(at);
        if at <= onset {
            selected = entry["monitor_token"].as_str().map(str::to_owned);
        }
        if at > onset.saturating_sub(1_000_000) && at <= onset.saturating_add(1_000_000) {
            return Err(io::Error::other("speech_monitor_ambiguous"));
        }
    }
    selected.ok_or_else(|| io::Error::other("speech_monitor_unavailable"))
}

pub fn monotonic_ns() -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64))
}
fn instant_to_monotonic_ns(instant: Instant) -> io::Result<u64> {
    let now = Instant::now();
    let mono = monotonic_ns()?;
    if instant > now {
        return Err(io::Error::other("future_audio_timestamp"));
    }
    Ok(mono.saturating_sub(now.duration_since(instant).as_nanos().min(u64::MAX as u128) as u64))
}

pub struct ContextMonitor {
    pub store: Arc<Mutex<ContextStore>>,
    notify: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl ContextMonitor {
    pub fn start() -> Self {
        let store = Arc::new(Mutex::new(ContextStore::default()));
        let notify = Arc::new(Notify::new());
        let (tx, mut rx) = mpsc::channel(128);
        jwm_ipc_client::subscribe_with_lifecycle(
            JwmIpc::new(),
            &["window", "monitor", "workspace", "tag", "layout"],
            tx,
        );
        let cache = store.clone();
        let ready = notify.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    SubscriptionEvent::Disconnected { .. } => cache.lock().unwrap().invalidate(),
                    SubscriptionEvent::Connected { received_at }
                    | SubscriptionEvent::Message { received_at, .. } => {
                        match crate::executor::query("get_execution_context", json!({})).await {
                            Ok(context) => {
                                cache.lock().unwrap().observe(context, received_at);
                                ready.notify_waiters();
                            }
                            Err(_) => cache.lock().unwrap().invalidate(),
                        }
                    }
                }
            }
            cache.lock().unwrap().invalidate();
        });
        Self {
            store,
            notify,
            task,
        }
    }
    pub async fn wait_ready(&self, timeout: Duration) -> io::Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            if let Some(context) = self.store.lock().unwrap().snapshot.clone() {
                return Ok(context);
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| io::Error::other("context_timeout"))?;
        }
    }
    pub fn bind_onset(&self, captured: Instant) -> io::Result<UtteranceBinding> {
        self.store.lock().unwrap().bind_onset(captured)
    }
    pub async fn monitor_at(&self, captured: Instant) -> io::Result<String> {
        let binding = self.bind_onset(captured)?;
        let context = crate::executor::query("get_execution_context", json!({})).await?;
        if !self.binding_current(&binding)
            || context["wm_instance_id"].as_str() != Some(binding.wm_instance_id.as_str())
            || context["session_unlocked"] != true
        {
            return Err(io::Error::other("context_reconnected"));
        }
        monitor_at_onset(&context, binding.onset_monotonic_ns)
    }

    pub fn binding_current(&self, binding: &UtteranceBinding) -> bool {
        self.store.lock().unwrap().binding_current(binding)
    }
    pub async fn verify_commit(
        &self,
        binding: &UtteranceBinding,
        committed: Instant,
    ) -> io::Result<Value> {
        let committed_ns = instant_to_monotonic_ns(committed)?;
        let context = crate::executor::query("get_execution_context", json!({})).await?;
        self.store
            .lock()
            .unwrap()
            .validate_commit(binding, committed_ns, context)
    }
}
impl Drop for ContextMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(history: Value) -> Value {
        json!({"observed_monotonic_ns":100_000_000,"focus_history":history})
    }
    #[test]
    fn wake_monitor_scope_uses_native_history_even_when_event_receipt_is_late() {
        let context = json!({"observed_monotonic_ns":120_000_000,"selected_monitor_history":[
            {"at_monotonic_ns":1,"monitor_token":"old"},
            {"at_monotonic_ns":95_000_000,"monitor_token":"actual-at-onset"},
            {"at_monotonic_ns":110_000_000,"monitor_token":"later"}
        ]});
        assert_eq!(
            monitor_at_onset(&context, 100_000_000).unwrap(),
            "actual-at-onset"
        );
        assert!(monitor_at_onset(&context, 95_000_000).is_err());
        assert!(monitor_at_onset(&context, 130_000_000).is_err());
        let missing = json!({"observed_monotonic_ns":120_000_000,"selected_monitor_token":"guess"});
        assert!(monitor_at_onset(&missing, 100_000_000).is_err());
    }
    #[test]
    fn delayed_receipt_does_not_rebind_speech_to_a_later_window() {
        let ctx = snapshot(
            json!([{"at_monotonic_ns":1,"focus_token":"original"},{"at_monotonic_ns":80_000_000,"focus_token":"later"}]),
        );
        assert_eq!(
            focus_during_utterance(&ctx, 10_000_000, 50_000_000).unwrap(),
            "original"
        );
    }
    #[test]
    fn switching_away_and_back_during_speech_is_ambiguous() {
        let ctx = snapshot(
            json!([{"at_monotonic_ns":1,"focus_token":"a"},{"at_monotonic_ns":20_000_000,"focus_token":"b"},{"at_monotonic_ns":30_000_000,"focus_token":"a"}]),
        );
        assert!(focus_during_utterance(&ctx, 10_000_000, 50_000_000).is_err());
    }
    #[test]
    fn missing_or_truncated_history_is_not_a_guess() {
        assert!(focus_during_utterance(&json!({}), 10, 50).is_err());
        assert!(
            focus_during_utterance(
                &snapshot(json!([{"at_monotonic_ns":20_000_000,"focus_token":"a"}])),
                10_000_000,
                50_000_000
            )
            .is_err()
        );
    }
    #[test]
    fn reconnect_invalidates_all_old_refs() {
        let mut cache = ContextStore::default();
        cache.observe(
            json!({"wm_instance_id":"one","focus_token":"a"}),
            Instant::now(),
        );
        let epoch = cache.connection_epoch;
        cache.invalidate();
        assert!(cache.snapshot.is_none());
        assert_ne!(cache.connection_epoch, epoch);
        assert!(cache.bind_onset(Instant::now()).is_err());
    }
    #[test]
    fn late_binding_rejects_capture_from_before_reconnection() {
        let now = Instant::now();
        let first_baseline = now - Duration::from_secs(2);
        let prefix = first_baseline + Duration::from_millis(100);
        let mut cache = ContextStore::default();
        cache.observe(
            json!({"wm_instance_id":"same-wm","focus_token":"a"}),
            first_baseline,
        );
        assert!(cache.bind_onset(prefix).is_ok());
        cache.invalidate();
        let new_baseline = now - Duration::from_millis(500);
        cache.observe(
            json!({"wm_instance_id":"same-wm","focus_token":"a"}),
            new_baseline,
        );
        assert_eq!(
            cache.bind_onset(prefix).unwrap_err().to_string(),
            "context_reconnected"
        );
        assert!(
            cache
                .bind_onset(new_baseline + Duration::from_millis(10))
                .is_ok()
        );
    }
    #[test]
    fn a_valid_source_prefix_does_not_expire_with_the_focus_deque_front() {
        let baseline = Instant::now() - Duration::from_secs(2);
        let mut cache = ContextStore::default();
        cache.observe(
            json!({"wm_instance_id":"wm","focus_token":"first"}),
            baseline,
        );
        let prefix = baseline + Duration::from_millis(1);
        for index in 1..=140 {
            cache.observe(
                json!({"wm_instance_id":"wm","focus_token":format!("focus-{index}")}),
                baseline + Duration::from_millis(index),
            );
        }
        assert_eq!(cache.observations.len(), 128);
        assert!(cache.observations.front().unwrap().received_at > prefix);
        assert_eq!(cache.connected_since, Some(baseline));
        assert!(
            cache.bind_onset(prefix).is_ok(),
            "focus deque eviction must not invent a newer connection epoch"
        );
    }
    fn activation_context() -> Value {
        json!({"wm_instance_id":"wm","session_unlocked":true,"selected_monitor_token":"selected-output","activation_monitor_token":"visible-output",
            "monitors":[{"monitor_token":"selected-output"},{"monitor_token":"visible-output"}],
            "windows":[{"target_token":"focus","window_ref":"w1","monitor_token":"selected-output","focused":true}],
            "observed_monotonic_ns":100_000_000,"focus_history":[{"at_monotonic_ns":1,"focus_token":"focus"}]})
    }
    #[test]
    fn activation_output_is_the_planning_scope_even_when_focus_is_on_another_monitor() {
        let latest = activation_context();
        let mut cache = ContextStore::default();
        cache.observe(latest.clone(), Instant::now() - Duration::from_secs(1));
        let binding = UtteranceBinding {
            connection_epoch: cache.connection_epoch,
            onset_monotonic_ns: 10_000_000,
            wm_instance_id: "wm".into(),
        };
        let verified = cache.validate_commit(&binding, 50_000_000, latest).unwrap();
        assert_eq!(verified["selected_monitor_token"], "visible-output");
        assert_eq!(verified["focus_token"], "focus");
        assert_eq!(verified["speech_focus_ref"], "w1");
    }
    #[test]
    fn missing_activation_output_is_rejected_and_old_servers_keep_window_scope() {
        let mut context = activation_context();
        context["activation_monitor_token"] = json!("removed-output");
        assert_eq!(
            planning_monitor_scope(&mut context)
                .unwrap_err()
                .to_string(),
            "target_gone: activation monitor"
        );
        assert_eq!(
            context["selected_monitor_token"], "selected-output",
            "failure must not guess or mutate another scope"
        );
        for absent in [false, true] {
            let mut latest = activation_context();
            if absent {
                latest
                    .as_object_mut()
                    .unwrap()
                    .remove("activation_monitor_token");
            } else {
                latest["activation_monitor_token"] = Value::Null;
            }
            latest["selected_monitor_token"] = json!("visible-output");
            planning_monitor_scope(&mut latest).unwrap();
            let mut cache = ContextStore::default();
            cache.observe(latest.clone(), Instant::now() - Duration::from_secs(1));
            let binding = UtteranceBinding {
                connection_epoch: cache.connection_epoch,
                onset_monotonic_ns: 10_000_000,
                wm_instance_id: "wm".into(),
            };
            let verified = cache.validate_commit(&binding, 50_000_000, latest).unwrap();
            assert_eq!(
                verified["selected_monitor_token"], "selected-output",
                "legacy servers retain original window-derived scope"
            );
        }
    }
}
