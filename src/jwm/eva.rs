//! Main-loop authority for Eva: incarnation-safe targets, expiring permits and
//! at-most-once dispatch. Network/model work never enters this module.
use crate::backend::api::Backend;
use crate::config::CONFIG;
use crate::core::layout::LayoutEnum;
use crate::core::models::{ClientKey, MonitorKey};
use crate::jwm::WMArgEnum;
use crate::{Jwm, ipc::IpcResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slotmap::Key;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const LEASE: Duration = Duration::from_secs(3);
const RETENTION: Duration = Duration::from_secs(600);
const MAX_OPERATIONS: usize = 1024;
// The transport already bounds the complete JSON frame to one MiB. Keep
// authority checks at that boundary as well: a valid typed config batch or a
// locally expanded application argv can be much larger than a short utterance.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssistantPermit {
    pub assistant_session_id: String,
    pub generation: u64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedContext {
    session_unlocked: bool,
    #[serde(default)]
    monitor_token: Option<String>,
    #[serde(default)]
    focus_token: Option<String>,
    #[serde(default)]
    visible_tag_mask: Option<u32>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Action {
    name: String,
    arguments: Value,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuardedRequest {
    protocol_version: u32,
    wm_instance_id: String,
    assistant_permit: AssistantPermit,
    operation_id: String,
    request_digest: String,
    expected_context: ExpectedContext,
    action: Action,
    #[serde(default)]
    confirmation_token: Option<String>,
}
struct Operation {
    digest: String,
    /// Retain the complete canonical request as well: a supplied digest alone
    /// never permits a caller to reuse an ID with changed arguments.
    request: Value,
    result: Value,
    expires: Instant,
    terminal: bool,
    control_sequence: Option<(String, u64)>,
    background_keepalive: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    clipboard_receipt: Option<crate::backend::clipboard_offer::ClipboardReceipt>,
}
struct Confirmation {
    expiry_reported: bool,
    token: String,
    plan_hash: String,
    targets: Value,
    expires: Instant,
    approved: bool,
    used: Vec<bool>,
    restore_snapshot: Option<super::session::SessionSnapshot>,
    restore_windows: Vec<String>,
    restore_monitors: Vec<String>,
    display_items: Vec<Value>,
}
pub(crate) struct EvaRuntime {
    instance: String,
    generation: u64,
    confirmation_generation: u64,
    confirmation_layout_signature: Option<u64>,
    activated_monotonic_ns: Option<u64>,
    activation_monitor: Option<MonitorKey>,
    presentation_monitor: Option<MonitorKey>,
    error_visible_until: Option<Instant>,
    focus_history: VecDeque<(u64, Option<String>)>,
    permit: Option<AssistantPermit>,
    lease_until: Option<Instant>,
    state: String,
    text: String,
    level: f64,
    microphone_active: bool,
    undo_available: bool,
    input_caption: Option<Value>,
    pub(crate) ui_action_grabs: Vec<(crate::backend::common_define::Mods, u32)>,
    confirmation: Option<Confirmation>,
    media_observation: Option<Value>,
    media_players: HashMap<String, Value>,
    operations: HashMap<String, Operation>,
    pub(crate) ui_grab_active: bool,
    pub(crate) ui_confirmation_grab_active: bool,
    pub(crate) confirmation_detail_page: Option<usize>,
    pub(crate) confirmation_seen_pages: Vec<usize>,
    pub(crate) confirmation_ui: crate::backend::compositor_common::eva_overlay::EvaOverlayState,
}
impl Default for EvaRuntime {
    fn default() -> Self {
        Self {
            instance: format!("wm-{:x}-{:x}", super::wm_session_id(), std::process::id()),
            generation: 0,
            confirmation_generation: 0,
            confirmation_layout_signature: None,
            activated_monotonic_ns: None,
            activation_monitor: None,
            presentation_monitor: None,
            error_visible_until: None,
            focus_history: VecDeque::new(),
            permit: None,
            lease_until: None,
            state: "idle".into(),
            text: String::new(),
            level: 0.,
            microphone_active: false,
            undo_available: false,
            input_caption: None,
            ui_action_grabs: Vec::new(),
            confirmation: None,
            media_observation: None,
            media_players: HashMap::new(),
            operations: HashMap::new(),
            ui_grab_active: false,
            ui_confirmation_grab_active: false,
            confirmation_detail_page: None,
            confirmation_seen_pages: Vec::new(),
            confirmation_ui: Default::default(),
        }
    }
}
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // CLOCK_MONOTONIC is shared by local processes and unaffected by wall-clock jumps.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}
fn process_start_ticks() -> Option<String> {
    static START: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    START
        .get_or_init(|| {
            std::fs::read_to_string("/proc/self/stat")
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(|value| value.to_string())
                })
        })
        .clone()
}
fn bounded_identifier(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)
}
fn failure(code: &str) -> IpcResponse {
    IpcResponse::err(code)
}
fn canonical_layout(layout: &LayoutEnum) -> &str {
    match layout.0 {
        "centeredmaster" => "centered_master",
        "threecol" => "three_col",
        other => other,
    }
}
fn parse_layout(name: &str) -> Option<&'static LayoutEnum> {
    LayoutEnum::from_name(match name {
        "centered_master" => "centeredmaster",
        "three_col" => "threecol",
        other => other,
    })
}

impl Jwm {
    fn eva_record_focus(&mut self) {
        let token = self
            .get_selected_client_key()
            .map(|k| self.eva_window_token(k));
        if self
            .eva
            .focus_history
            .back()
            .is_none_or(|(_, old)| *old != token)
        {
            if self.eva.focus_history.len() >= 128 {
                self.eva.focus_history.pop_front();
            }
            self.eva.focus_history.push_back((monotonic_ns(), token));
        }
    }
    fn eva_presentable_monitor(
        &self,
        backend: &dyn Backend,
        preferred: Option<MonitorKey>,
    ) -> Option<MonitorKey> {
        preferred
            .into_iter()
            .chain(self.state.sel_mon)
            .chain(self.state.monitor_order.iter().copied())
            .find(|key| {
                self.state.monitors.get(*key).is_some_and(|m| {
                    let g = &m.geometry;
                    g.w_w > 0
                        && g.w_h > 0
                        && backend.compositor_eva_viewport_visible([
                            g.w_x as f32,
                            g.w_y as f32,
                            g.w_w as f32,
                            g.w_h as f32,
                        ])
                })
            })
    }
    pub(crate) fn eva_active(&self) -> bool {
        self.eva.permit.is_some()
            && self.eva.lease_until.is_some_and(|t| Instant::now() < t)
            && !self.features.system_ui.is_locked()
            && self.features.monitor_lock.is_empty()
            && self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn eva_cancel(&mut self, reason: &str) {
        let had_permit = self.eva.permit.take();
        self.eva.lease_until = None;
        self.eva.activated_monotonic_ns = None;
        self.eva.confirmation = None;
        self.eva.confirmation_detail_page = None;
        self.eva.confirmation_seen_pages.clear();
        self.eva.confirmation_ui = Default::default();
        self.eva.confirmation_layout_signature = None;
        self.eva.error_visible_until = None;
        self.eva.state = "idle".into();
        self.eva.text.clear();
        self.eva.level = 0.;
        self.eva.microphone_active = false;
        self.eva.undo_available = false;
        self.eva.input_caption = None;
        if let Some(permit) = had_permit {
            self.broadcast_ipc_event(
                "assistant/cancelled",
                json!({"reason":reason,"assistant_permit":permit}),
            );
        }
    }
    pub(crate) fn eva_next_wakeup(&self, now: Instant) -> Option<Duration> {
        self.eva
            .lease_until
            .into_iter()
            .chain(self.eva.confirmation.as_ref().map(|c| c.expires))
            .chain(self.eva.error_visible_until)
            .chain(
                self.eva
                    .operations
                    .values()
                    .filter(|op| !op.terminal && op.result["status"] == "accepted")
                    .map(|op| op.expires - RETENTION + Duration::from_secs(20)),
            )
            .chain(
                self.eva
                    .operations
                    .values()
                    .filter(|op| {
                        !op.terminal
                            && op.clipboard_receipt.is_some()
                            && op.result["status"] == "accepted"
                    })
                    .map(|_| now + Duration::from_millis(20)),
            )
            .map(|at| at.saturating_duration_since(now))
            .min()
    }
    pub(crate) fn eva_tick(&mut self, backend: &mut dyn Backend) {
        self.eva_record_focus();
        if self.eva.permit.is_some() {
            let monitor = self.eva_presentable_monitor(
                backend,
                self.eva
                    .presentation_monitor
                    .or(self.eva.activation_monitor),
            );
            if !backend.compositor_supports_eva_overlay() || monitor.is_none() {
                self.eva_cancel("overlay_unavailable");
            } else {
                self.eva.presentation_monitor = monitor;
            }
        }
        if self.eva.permit.is_some() && backend.compositor_session_locked() {
            self.eva_cancel("session_locked");
        }
        if self.eva.permit.is_some() && !self.eva_active() {
            let lost_lease = !self.features.system_ui.is_locked()
                && self.features.monitor_lock.is_empty()
                && self.running.load(std::sync::atomic::Ordering::SeqCst);
            self.eva_cancel(if lost_lease {
                "lease_expired"
            } else {
                "session_locked_or_exiting"
            });
            if lost_lease {
                self.eva.state = "error".into();
                self.eva.text = "连接已中断，麦克风状态待确认".into();
                self.eva.error_visible_until = Some(Instant::now() + Duration::from_secs(2));
            }
        }
        if self
            .eva
            .confirmation
            .as_ref()
            .is_some_and(|c| c.expires <= Instant::now() && !c.expiry_reported)
        {
            // Keep the frozen scope as an expired tombstone: the same
            // daemon heartbeat must not silently issue a fresh deadline.
            if let Some(c) = self.eva.confirmation.as_mut() {
                c.expiry_reported = true;
                c.approved = false;
            }
            self.eva.confirmation_detail_page = None;
            self.eva.confirmation_seen_pages.clear();
            self.eva.confirmation_ui = Default::default();
            self.eva.confirmation_layout_signature = None;
            if self.eva.state == "confirming" {
                self.eva.state = "error".into();
                self.eva.text = "确认已超时，请重新发起操作".into();
                self.eva.microphone_active = false;
            }
            self.broadcast_ipc_event("assistant/confirmation_expired", json!({}));
        }
        if self
            .eva
            .error_visible_until
            .is_some_and(|at| at <= Instant::now())
        {
            self.eva.error_visible_until = None;
        }
        let session_locked =
            self.features.system_ui.is_locked() || backend.compositor_session_locked();
        let mut clipboard_finished = Vec::new();
        for (id, op) in &mut self.eva.operations {
            if op.terminal {
                continue;
            }
            if session_locked && op.request["action"]["name"] == "lock_screen" {
                op.result["status"] = json!("succeeded");
                op.result["verified"]["completion"] = json!("session_lock_observed");
                op.terminal = true;
                op.expires = Instant::now() + RETENTION;
                clipboard_finished.push((id.clone(), op.result.clone()));
                continue;
            }
            let Some(receipt) = op.clipboard_receipt.as_ref() else {
                continue;
            };
            use crate::backend::clipboard_offer::ClipboardReceiptStatus;
            let succeeded = match receipt.status() {
                ClipboardReceiptStatus::Pending => continue,
                ClipboardReceiptStatus::Owned => true,
                ClipboardReceiptStatus::Failed => false,
            };
            op.result["status"] = json!(if succeeded { "succeeded" } else { "failed" });
            op.result["verified"]["completion"] = json!(if succeeded {
                "native_clipboard_ownership_observed"
            } else {
                "native_clipboard_offer_failed"
            });
            op.terminal = true;
            op.expires = Instant::now() + RETENTION;
            clipboard_finished.push((id.clone(), op.result.clone()));
        }
        for (id, result) in clipboard_finished {
            self.broadcast_ipc_event(
                "assistant/execution",
                json!({"operation_id":id,"result":result}),
            );
        }
        for op in self.eva.operations.values_mut() {
            if !op.terminal
                && op.result["status"] == "accepted"
                && Instant::now() >= op.expires - RETENTION + Duration::from_secs(20)
            {
                op.result["status"] = json!("unknown");
                op.result["error"] = json!("completion_deadline_elapsed");
                // Keep the dispatch tombstone until WM exit; unknown is never
                // converted into permission to replay a side effect.
            }
        }
        self.eva
            .operations
            .retain(|_, op| !op.terminal || op.expires > Instant::now());
        self.eva_sync_ui(backend);
    }
    pub(crate) fn eva_toggle(
        &mut self,
        backend: &mut dyn Backend,
        _arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let response = self.eva_control(backend, &json!({"action":"toggle"}));
        if !response.success {
            return Err(response
                .error
                .unwrap_or_else(|| "Eva activation failed".into())
                .into());
        }
        // The user service starts asynchronously. Its startup cannot extend or
        // recreate this permit; if the three-second lease elapses the next
        // activation must come from the user again.
        #[cfg(not(test))]
        if self.eva_active() {
            let mut command = std::process::Command::new("systemctl");
            command
                .args(["--user", "start", "--no-block", "jwm-eva.service"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match command.spawn() {
                Ok(child) => self.supervise_transient_child(child),
                Err(error) => {
                    self.eva_cancel("daemon_start_failed");
                    self.eva.state = "error".into();
                    self.eva.text = "无法启动 Eva，请检查用户服务安装".into();
                    self.eva.error_visible_until = Some(Instant::now() + Duration::from_secs(2));
                    self.eva_sync_ui(backend);
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
    /// Called only by trusted local key/pointer routing. The model and IPC
    /// status publisher cannot manufacture approval with `confirmed: true`.
    /// Page visits belong to one concrete readable layout. Output/font changes
    /// invalidate visits, while status/caption updates and heartbeat keep them.
    /// This is also checked immediately before trusted confirmation input.
    pub(crate) fn eva_refresh_confirmation_layout(&mut self) {
        let snapshot = self.eva_overlay_snapshot();
        if !snapshot["confirmation"].is_object() || snapshot["confirmation"]["approved"] == true {
            self.eva.confirmation_layout_signature = None;
            self.eva.confirmation_ui.update(&snapshot, Instant::now());
            return;
        }
        self.eva.confirmation_ui.update(&snapshot, Instant::now());
        let signature = self.eva.confirmation_ui.confirmation_layout_signature();
        if signature != self.eva.confirmation_layout_signature {
            self.eva.confirmation_layout_signature = signature;
            self.eva.confirmation_detail_page = None;
            self.eva.confirmation_seen_pages.clear();
            let snapshot = self.eva_overlay_snapshot();
            self.eva.confirmation_ui.update(&snapshot, Instant::now());
        }
    }
    pub(crate) fn eva_confirm(&mut self) -> bool {
        if !self.eva_active() || self.eva.state != "confirming" {
            return false;
        }
        self.eva_refresh_confirmation_layout();
        if !self.eva.confirmation_ui.confirmation_reviewable() || !self.eva_active() {
            return false;
        }
        let permit = self.eva.permit.clone();
        let Some(c) = self
            .eva
            .confirmation
            .as_mut()
            .filter(|c| !c.approved && c.expires > Instant::now())
        else {
            return false;
        };
        c.approved = true;
        let event = json!({"confirmation_token":c.token,"plan_hash":c.plan_hash,"total_actions":c.used.len(),"assistant_permit":permit});
        self.broadcast_ipc_event("assistant/confirmed", event);
        true
    }
    pub(crate) fn eva_overlay_snapshot(&self) -> Value {
        let confirmation = self
            .eva
            .confirmation
            .as_ref()
            .filter(|c| self.eva.state == "confirming" && c.expires > Instant::now())
            .map(|c| json!({"plan_hash":c.plan_hash,"total_actions":c.used.len(),"display_items":c.display_items,"details_page":self.eva.confirmation_detail_page,"details_seen_pages":self.eva.confirmation_seen_pages,"approved":c.approved,"token":if c.approved{Some(&c.token)}else{None},"restore_window_count":c.restore_windows.len(),"remaining_ms":c.expires.saturating_duration_since(Instant::now()).as_millis() as u64}));
        let monitor = self
            .eva
            .presentation_monitor
            .filter(|m| self.state.monitors.contains_key(*m))
            .or_else(|| {
                self.eva
                    .activation_monitor
                    .filter(|m| self.state.monitors.contains_key(*m))
            })
            .or(self.state.sel_mon)
            .or_else(|| self.state.monitor_order.first().copied());
        let rect = monitor.and_then(|m| self.state.monitors.get(m)).map(|m| {
            [
                m.geometry.w_x,
                m.geometry.w_y,
                m.geometry.w_w,
                m.geometry.w_h,
            ]
        });
        let text = if let Some(c) =
            self.eva.confirmation.as_ref().filter(|c| {
                c.restore_snapshot.is_some() && !c.approved && c.expires > Instant::now()
            }) {
            let labels = c
                .restore_windows
                .iter()
                .filter_map(|token| self.eva_resolve_window(token))
                .filter_map(|key| self.state.clients.get(key))
                .take(3)
                .map(|c| {
                    c.name
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(24)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("、");
            format!(
                "{} · 恢复 {} 个匹配窗口：{}",
                self.eva.text,
                c.restore_windows.len(),
                labels
            )
        } else {
            self.eva.text.clone()
        };
        json!({"active":self.eva_active(),"visible":self.eva_active() || self.eva.error_visible_until.is_some_and(|t| t > Instant::now()),"monitor_rect":rect,"state":self.eva.state,"text":text,"level":self.eva.level,"microphone_active":if self.eva.state=="error"&&self.eva.error_visible_until.is_some(){None}else{Some(self.eva.microphone_active)},
            "undo_available":self.eva.undo_available,"input_caption":self.eva.input_caption,"confirmation":confirmation,"assistant_permit":self.eva.permit,"wm_instance_id":self.eva.instance,"activated_monotonic_ns":self.eva.activated_monotonic_ns,
            "lease_remaining_ms":self.eva.lease_until.map(|t|t.saturating_duration_since(Instant::now()).as_millis() as u64).unwrap_or(0)})
    }
    pub(crate) fn eva_control(&mut self, backend: &mut dyn Backend, args: &Value) -> IpcResponse {
        let Some(action) = args.get("action").and_then(Value::as_str) else {
            return failure("plan_invalid: missing assistant action");
        };
        if !matches!(action, "toggle" | "activate" | "cancel") {
            return failure("plan_invalid: unsupported assistant action");
        }
        if action == "cancel" || (action == "toggle" && self.eva_active()) {
            self.eva_cancel("user_cancelled");
        } else {
            if !backend.compositor_supports_eva_overlay()
                || self
                    .eva_presentable_monitor(backend, self.state.sel_mon)
                    .is_none()
            {
                return failure("assistant_overlay_unavailable");
            }
            if self.features.system_ui.is_locked()
                || !self.features.monitor_lock.is_empty()
                || backend.compositor_session_locked()
            {
                return failure("session_locked");
            }
            self.eva_cancel("replaced");
            let Some(generation) = self.eva.generation.checked_add(1) else {
                return failure("generation_exhausted");
            };
            self.eva.generation = generation;
            self.eva.activated_monotonic_ns = Some(monotonic_ns());
            let presentation = self.eva_presentable_monitor(backend, self.state.sel_mon);
            self.eva.activation_monitor = self
                .state
                .sel_mon
                .filter(|m| self.state.monitors.contains_key(*m))
                .or(presentation);
            self.eva.presentation_monitor = presentation;
            self.eva.permit = Some(AssistantPermit {
                assistant_session_id: format!("{}-{generation}", self.eva.instance),
                generation,
            });
            self.eva.lease_until = Some(Instant::now() + LEASE);
            self.eva.state = "connecting".into();
            self.eva.text = "正在连接 Eva…".into();
            self.broadcast_ipc_event("assistant/activated", self.eva_overlay_snapshot());
        }
        self.eva_sync_ui(backend);
        IpcResponse::ok(Some(self.eva_overlay_snapshot()))
    }
    pub(crate) fn eva_set_status(
        &mut self,
        backend: &mut dyn Backend,
        args: &Value,
    ) -> IpcResponse {
        if !self.eva_active() {
            self.eva_tick(backend);
            return failure("assistant_permit_expired");
        }
        let permit = args
            .get("assistant_permit")
            .cloned()
            .and_then(|v| serde_json::from_value::<AssistantPermit>(v).ok());
        if permit != self.eva.permit {
            return failure("assistant_permit_mismatch");
        }
        let state = args
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or(&self.eva.state);
        if !matches!(
            state,
            "connecting"
                | "listening"
                | "planning"
                | "confirming"
                | "executing"
                | "responding"
                | "follow_up"
                | "followup"
                | "closing"
                | "error"
                | "idle"
        ) {
            return failure("plan_invalid: invalid assistant state");
        }
        if state == "idle" || state == "closing" {
            self.eva_cancel("daemon_closed");
            self.eva_sync_ui(backend);
            return IpcResponse::ok(Some(self.eva_overlay_snapshot()));
        }
        self.eva.microphone_active = match args.get("microphone_active") {
            Some(Value::Bool(active)) => *active,
            None => false,
            _ => return failure("plan_invalid: microphone_active must be boolean"),
        };
        self.eva.undo_available = match args.get("undo_available") {
            Some(Value::Bool(value)) => *value,
            None => false,
            _ => return failure("plan_invalid: undo_available must be boolean"),
        };
        self.eva.input_caption = match args.get("input_caption") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let Some(item) = value["item_id"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 256)
                else {
                    return failure("plan_invalid: caption item");
                };
                let Some(turn) = value["turn_epoch"].as_u64() else {
                    return failure("plan_invalid: caption turn");
                };
                let Some(text) = value["text"].as_str() else {
                    return failure("plan_invalid: caption text");
                };
                let text:String=text.chars().filter(|c| !c.is_control() && !matches!(*c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')).take(160).collect();
                Some(json!({"item_id":item,"turn_epoch":turn,"text":text}))
            }
        };
        self.eva.state = state.into();
        if let Some(text) = args.get("text").and_then(Value::as_str) {
            self.eva.text = text.chars().filter(|c| !c.is_control()).take(240).collect();
        }
        if let Some(level) = args.get("level").and_then(Value::as_f64) {
            self.eva.level = level.clamp(0., 1.);
        }
        if let Some(request) = args.get("confirmation") {
            let Some(hash) = request
                .get("plan_hash")
                .and_then(Value::as_str)
                .filter(|s| bounded_identifier(s))
            else {
                return failure("plan_invalid: confirmation hash");
            };
            let targets = request.get("targets").cloned().unwrap_or(Value::Null);
            if targets.to_string().len() > MAX_REQUEST_BYTES {
                return failure("plan_invalid: confirmation too large");
            }
            let Some(actions) = targets
                .as_array()
                .filter(|actions| !actions.is_empty() && actions.len() <= 8)
            else {
                return failure("plan_invalid: confirmation must contain one to eight actions");
            };
            for action in actions {
                let Ok(action) =
                    serde_json::from_value::<jwm_eva_core::protocol::Action>(action.clone())
                else {
                    return failure("plan_invalid: malformed confirmation action");
                };
                if let Err(error) = jwm_eva_core::actions::validate_execution_action(&action) {
                    return failure(&format!("plan_invalid: confirmation action: {error}"));
                }
            }
            let timeout_ms = match request.get("timeout_ms") {
                None => 15_000,
                Some(value) => match value
                    .as_u64()
                    .filter(|value| (1_000..=600_000).contains(value))
                {
                    Some(value) => value,
                    None => {
                        return failure(
                            "plan_invalid: confirmation timeout must be 1000..600000 ms",
                        );
                    }
                },
            };
            if self
                .eva
                .confirmation
                .as_ref()
                .is_none_or(|c| c.plan_hash != hash || c.targets != targets)
            {
                self.eva.confirmation_generation =
                    self.eva.confirmation_generation.saturating_add(1);
                let restore_snapshot = if targets
                    .as_array()
                    .is_some_and(|actions| actions.iter().any(|a| a["name"] == "restore_session"))
                {
                    match Self::eva_load_restore_snapshot() {
                        Ok(snapshot) => Some(snapshot),
                        Err(error) => return failure(&error),
                    }
                } else {
                    None
                };
                let mut restore_windows = restore_snapshot
                    .as_ref()
                    .map(|snapshot| {
                        self.eva_restore_matching_keys(snapshot)
                            .into_iter()
                            .map(|w| self.eva_window_token(w))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                restore_windows.sort();
                let mut restore_monitors = self
                    .state
                    .monitors
                    .keys()
                    .map(|m| self.eva_monitor_token(m))
                    .collect::<Vec<_>>();
                restore_monitors.sort();
                let display_items = self.eva_confirmation_items(&targets, &restore_windows);
                self.eva.confirmation_detail_page = None;
                self.eva.confirmation_seen_pages.clear();
                self.eva.confirmation_ui = Default::default();
                self.eva.confirmation_layout_signature = None;
                self.eva.confirmation = Some(Confirmation {
                    expiry_reported: false,
                    token: format!(
                        "confirm-{}-{}-{}",
                        self.eva.generation,
                        self.eva.confirmation_generation,
                        super::wm_session_id()
                    ),
                    plan_hash: hash.into(),
                    used: vec![false; targets.as_array().map_or(0, Vec::len)],
                    targets,
                    expires: Instant::now() + Duration::from_millis(timeout_ms),
                    approved: false,
                    restore_snapshot,
                    restore_windows,
                    restore_monitors,
                    display_items,
                });
            }
        }
        if self.eva.state == "confirming"
            && self
                .eva
                .confirmation
                .as_ref()
                .is_some_and(|c| c.expires <= Instant::now())
        {
            self.eva.state = "error".into();
            self.eva.text = "确认已超时，请重新发起操作".into();
            self.eva.microphone_active = false;
        }
        self.eva.lease_until = Some(Instant::now() + LEASE);
        self.eva_sync_ui(backend);
        IpcResponse::ok(Some(self.eva_overlay_snapshot()))
    }
    /// Freeze human-reviewable identities and values from the same local
    /// targets that the confirmation token authorizes. Provider prose is never
    /// an authority for this list. JSON escaping keeps newlines/control text
    /// inside values distinguishable from labels; only the separate title may
    /// be shortened by the renderer.
    fn eva_confirmation_items(&self, targets: &Value, restore_windows: &[String]) -> Vec<Value> {
        let specs = jwm_eva_core::actions::semantic_actions();
        let window_keys = self
            .state
            .clients
            .keys()
            .map(|key| (self.eva_window_token(key), key))
            .collect::<HashMap<_, _>>();
        let mut first_restore = None;
        targets
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, action)| {
                let name = action["name"].as_str().unwrap_or("");
                let description = specs
                    .iter()
                    .find(|spec| spec.canonical_name == name)
                    .map(|spec| spec.description_zh.as_str())
                    .unwrap_or(name);
                let mut identities = Vec::new();
                let mut title = String::new();
                let mut values = serde_json::Map::new();
                if let Some(args) = action["arguments"].as_object() {
                    for (key, value) in args {
                        if key.ends_with("window_token") {
                            if let Some(window) = value
                                .as_str()
                                .and_then(|token| window_keys.get(token).copied())
                            {
                                identities.push(format!("{key}=w{:x}", window.data().as_ffi()));
                                if title.is_empty() {
                                    title = self.state.clients[window]
                                        .name
                                        .chars()
                                        .filter(|c| !c.is_control())
                                        .take(80)
                                        .collect();
                                }
                            } else {
                                // Retain an unresolved identity visibly; execution
                                // will reject the stale token rather than retarget.
                                identities.push(format!("{key}={value}"));
                            }
                        } else if key.ends_with("monitor_token") {
                            if let Some(monitor) = value
                                .as_str()
                                .and_then(|token| self.eva_resolve_monitor(token))
                            {
                                identities.push(format!("{key}=m{:x}", monitor.data().as_ffi()));
                            } else {
                                identities.push(format!("{key}={value}"));
                            }
                        } else if key == "tag_mask" {
                            let tags = value.as_u64().map(|mask| {
                                (0..32)
                                    .filter(|bit| mask & (1u64 << bit) != 0)
                                    .map(|bit| bit + 1)
                                    .collect::<Vec<_>>()
                            });
                            values.insert("tag_numbers".into(), json!(tags));
                        } else if !matches!(key.as_str(), "argv" | "expected_state") {
                            // argv is resolved from the local application allowlist;
                            // application_id is the user's complete launch choice.
                            values.insert(key.clone(), value.clone());
                        }
                    }
                }
                if name == "restore_session" {
                    if let Some(first) = first_restore {
                        // Every restore in this frozen plan uses the same
                        // snapshot and exact incarnation set. Refer to that
                        // complete list instead of duplicating up to 16K
                        // identities eight times in one response frame.
                        values.insert("restore_scope_same_as_action".into(), json!(first));
                    } else {
                        first_restore = Some(index + 1);
                        let mut title_bytes = 32 * 1024usize;
                        let scope = restore_windows
                            .iter()
                            .map(|token| {
                                window_keys
                                    .get(token)
                                    .copied()
                                    .map(|window| {
                                        let mut item =
                                            json!({"ref":format!("w{:x}", window.data().as_ffi())});
                                        let title = self.state.clients[window]
                                            .name
                                            .chars()
                                            .filter(|c| !c.is_control())
                                            .take(28)
                                            .scan((), |_, ch| {
                                                if ch.len_utf8() > title_bytes {
                                                    return None;
                                                }
                                                title_bytes -= ch.len_utf8();
                                                Some(ch)
                                            })
                                            .collect::<String>();
                                        if !title.is_empty() {
                                            item["title"] = json!(title);
                                        }
                                        item
                                    })
                                    .unwrap_or_else(|| json!({"ref":token}))
                            })
                            .collect::<Vec<_>>();
                        values.insert("restore_existing_windows".into(), json!(scope));
                    }
                }
                let mut critical = identities.join(" ");
                if !critical.is_empty() {
                    critical.push_str(" · ");
                }
                critical.push_str(&format!("{name} · {description}"));
                if !values.is_empty() {
                    critical.push_str(&format!(" · {}", Value::Object(values)));
                }
                json!({"critical":critical,"title":title})
            })
            .collect()
    }
    fn eva_window_token(&self, key: ClientKey) -> String {
        format!("{}:w:{:x}", self.eva.instance, key.data().as_ffi())
    }
    fn eva_monitor_token(&self, key: MonitorKey) -> String {
        format!("{}:m:{:x}", self.eva.instance, key.data().as_ffi())
    }
    fn eva_resolve_window(&self, token: &str) -> Option<ClientKey> {
        self.state
            .clients
            .keys()
            .find(|&k| self.eva_window_token(k) == token)
    }
    fn eva_resolve_monitor(&self, token: &str) -> Option<MonitorKey> {
        self.state
            .monitors
            .keys()
            .find(|&k| self.eva_monitor_token(k) == token)
    }
    pub(crate) fn eva_execution_context(&mut self) -> Value {
        self.eva_record_focus();
        let monitors: Vec<Value> = self.state.monitor_order.iter().filter_map(|&key|self.state.monitors.get(key).map(|m|json!({
            "monitor_token":self.eva_monitor_token(key),"monitor_ref":format!("m{:x}",key.data().as_ffi()),"num":m.num,"tab_tokens":self.tab_group_clients(key).iter().map(|w|self.eva_window_token(*w)).collect::<Vec<_>>(),"visible_tag_mask":m.get_active_tags(),"layout":canonical_layout(&m.lt),"locked":self.monitor_key_is_locked(key)
        }))).collect();
        let windows: Vec<Value> = self.state.clients.iter().filter(|(_,c)|!c.state.is_dock && !c.state.is_desktop).map(|(key,c)|json!({
            "target_token":self.eva_window_token(key),"window_ref":format!("w{:x}",key.data().as_ffi()),"id":c.win.raw().to_string(),"title":c.name.chars().take(160).collect::<String>(),"class":c.class.chars().take(80).collect::<String>(),"monitor_token":c.mon.map(|m|self.eva_monitor_token(m)),"tag_mask":c.state.tags,"focused":self.get_selected_client_key()==Some(key),"minimized":c.state.is_hidden,"floating":c.state.is_floating,"maximized":c.state.is_maximized_vert&&c.state.is_maximized_horz,"maximized_horz":c.state.is_maximized_horz,"maximized_vert":c.state.is_maximized_vert,"sticky":c.state.is_sticky,"pip":c.state.is_pip
        })).collect();
        let mut workspaces = Vec::new();
        for &key in &self.state.monitor_order {
            if let Some(m) = self.state.monitors.get(key) {
                for index in 0..CONFIG.load().tags_length().min(32) {
                    let layout = m
                        .pertag
                        .as_ref()
                        .and_then(|p| p.lts.get(index + 1))
                        .unwrap_or(&m.lt);
                    workspaces.push(json!({"monitor_token":self.eva_monitor_token(key),"tag_number":index+1,"tag_mask":1u32<<index,"layout":canonical_layout(layout)}));
                }
            }
        }
        json!({"wm_pid":std::process::id(),"wm_start_ticks":process_start_ticks(),"observed_monotonic_ns":monotonic_ns(),"focus_history":self.eva.focus_history.iter().map(|(at,token)|json!({"at_monotonic_ns":at,"focus_token":token})).collect::<Vec<_>>(),"protocol_version":1,"wm_instance_id":self.eva.instance,"assistant_permit":self.eva.permit,"session_unlocked":!self.features.system_ui.is_locked()&&self.features.monitor_lock.is_empty(),"activation_monitor_token":self.eva.activation_monitor.filter(|_|self.eva_active()).map(|m|self.eva_monitor_token(m)),"selected_monitor_token":self.state.sel_mon.map(|m|self.eva_monitor_token(m)),"focus_token":self.get_selected_client_key().map(|k|self.eva_window_token(k)),"tag_count":CONFIG.load().tags_length().min(32),"monitors":monitors,"windows":windows,"workspaces":workspaces})
    }
    pub(crate) fn eva_command_schema(&self) -> Value {
        let mut actions = serde_json::to_value(jwm_eva_core::actions::full_registry())
            .unwrap_or_else(|_| json!([]));
        if let Some(specs) = actions.as_array_mut() {
            for spec in specs {
                if spec["canonical_name"] == "unlock_monitor" {
                    spec["support_status"] = json!("policy_requires_local_auth");
                    spec["unavailable_reason"] = json!(
                        "Unlocking a monitor requires the trusted local authentication prompt; an assistant permit cannot unlock it"
                    );
                    continue;
                }
                let reason = match spec["canonical_name"].as_str() {
                    Some("monitor_layout")
                        if !matches!(self.runtime_backend.as_str(), "x11rb" | "xcb") =>
                    {
                        Some(
                            "monitor_layout requires the X11 RandR backend; the selected backend is not X11",
                        )
                    }
                    Some("monitor_layout") if self.state.monitors.len() < 2 => {
                        Some("monitor_layout requires at least two current outputs")
                    }
                    Some("start_audio_recording" | "toggle_audio_recording")
                        if !cfg!(feature = "media-audio") =>
                    {
                        Some("this JWM build does not include the media-audio capture feature")
                    }
                    _ => None,
                };
                if let Some(reason) = reason {
                    spec["support_status"] = json!("backend_unavailable");
                    spec["unavailable_reason"] = json!(reason);
                    spec["observed_backend"] = json!(self.runtime_backend);
                }
            }
        }
        json!({"protocol_version":1,"guarded_execution":true,"context_tokens":true,"actions":actions})
    }
    pub(crate) fn eva_execution_status(
        &mut self,
        backend: &mut dyn Backend,
        args: &Value,
    ) -> IpcResponse {
        self.eva_tick(backend);
        let pending = args["operation_id"].as_str().and_then(|id| {
            self.eva
                .operations
                .get(id)
                .filter(|op| {
                    !op.terminal
                        && matches!(op.result["status"].as_str(), Some("accepted" | "unknown"))
                })
                .map(|op| {
                    (
                        id.to_string(),
                        op.request["action"].clone(),
                        op.result["verified"].clone(),
                    )
                })
        });
        if let Some((id, action, verified)) = pending {
            let name = action["name"].as_str().unwrap_or("");
            let audio = matches!(
                name,
                "toggle_audio_recording" | "start_audio_recording" | "stop_audio_recording"
            );
            if name == "benchmark" {
                if let Some(report) = self
                    .handle_ipc_query("benchmark_report", &json!({}), backend)
                    .data
                {
                    if verified.get("after").is_none_or(Value::is_null) {
                        if let Some(op) = self.eva.operations.get_mut(&id) {
                            op.result["status"] = json!("succeeded");
                            op.result["verified"]["report"] = report;
                            op.terminal = true;
                            op.expires = Instant::now() + RETENTION;
                        }
                    }
                }
            }
            if matches!(name, "waterlily_case" | "waterlily_palette") {
                if let Some(state) = self
                    .handle_ipc_query("get_waterlily_status", &json!({}), backend)
                    .data
                {
                    let field = if name == "waterlily_case" {
                        "active_case"
                    } else {
                        "active_palette"
                    };
                    let requested = if name == "waterlily_case" {
                        "requested_case"
                    } else {
                        "requested_palette"
                    };
                    let target = verified.get("after").and_then(|value| value.get(requested));
                    if state["active"] == true
                        && target
                            .is_some_and(|target| target.is_string() && state[field] == *target)
                    {
                        if let Some(op) = self.eva.operations.get_mut(&id) {
                            op.result["status"] = json!("succeeded");
                            op.result["verified"]["effect"] = state;
                            op.terminal = true;
                            op.expires = Instant::now() + RETENTION;
                        }
                    }
                }
            }
            if audio
                || matches!(
                    name,
                    "toggle_recording" | "start_recording" | "stop_recording"
                )
            {
                let query = if audio {
                    "get_audio_recording_status"
                } else {
                    "get_recording_status"
                };
                if let Some(state) = self.handle_ipc_query(query, &json!({}), backend).data {
                    let desired = action["arguments"]["enabled"]
                        .as_bool()
                        .unwrap_or(name.starts_with("start_"));
                    let expected_path = action["arguments"]
                        .get("path")
                        .or_else(|| verified.get("reply").and_then(|r| r.get("output_path")))
                        .or_else(|| verified.get("after").and_then(|r| r.get("output_path")));
                    let same_path = expected_path
                        .is_none_or(|path| path.is_null() || state["output_path"] == *path);
                    let complete = if desired {
                        state["active"] == true && state["output_path"].is_string()
                    } else if audio {
                        state["active"] == false
                            && state["finalizing"] == false
                            && (state["output_exists"] == true || state["output_path"].is_null())
                    } else {
                        state["active"] == false
                            && state["exporting"] == false
                            && (state["finalized"] == true || state["output_path"].is_null())
                    };
                    if same_path && complete {
                        if let Some(op) = self.eva.operations.get_mut(&id) {
                            op.result["status"] = json!("succeeded");
                            op.result["verified"]["recording"] = state;
                            op.terminal = true;
                            op.expires = Instant::now() + RETENTION;
                        }
                    } else if same_path && state["last_error"].is_string() {
                        if let Some(op) = self.eva.operations.get_mut(&id) {
                            op.result["status"] = json!("failed");
                            op.result["error"] = state["last_error"].clone();
                            op.terminal = true;
                            op.expires = Instant::now() + RETENTION;
                        }
                    }
                }
            }
        }
        if args
            .get("wm_instance_id")
            .and_then(Value::as_str)
            .is_some_and(|id| id != self.eva.instance)
        {
            return failure("wm_instance_mismatch");
        }
        match args
            .get("operation_id")
            .and_then(Value::as_str)
            .and_then(|id| self.eva.operations.get(id))
        {
            Some(op) => IpcResponse::ok(Some(op.result.clone())),
            None => IpcResponse::ok(Some(json!({"status":"unknown"}))),
        }
    }
    pub(crate) fn eva_guarded_execute(
        &mut self,
        backend: &mut dyn Backend,
        args: &Value,
    ) -> IpcResponse {
        if args.to_string().len() > MAX_REQUEST_BYTES {
            return failure("plan_invalid: request too large");
        }
        let req: GuardedRequest = match serde_json::from_value(args.clone()) {
            Ok(r) => r,
            Err(_) => return failure("plan_invalid: malformed guarded request"),
        };
        if req.protocol_version != 1 || req.wm_instance_id != self.eva.instance {
            return failure("version_incompatible_or_wm_instance_mismatch");
        }
        if !bounded_identifier(&req.operation_id) || !bounded_identifier(&req.request_digest) {
            return failure("plan_invalid: invalid operation identity");
        }
        let mut digest_payload = args.clone();
        digest_payload
            .as_object_mut()
            .expect("request object")
            .remove("request_digest");
        if jwm_eva_core::protocol::digest(&digest_payload) != req.request_digest {
            return failure("request_digest_mismatch");
        }
        if let Some(old) = self.eva.operations.get(&req.operation_id) {
            return if old.digest == req.request_digest && old.request == *args {
                IpcResponse::ok(Some(old.result.clone()))
            } else {
                failure("operation_id_conflict")
            };
        }
        self.eva_tick(backend);
        if !self.eva_active() || self.eva.permit.as_ref() != Some(&req.assistant_permit) {
            return failure("assistant_permit_expired");
        }
        if !req.expected_context.session_unlocked {
            return failure("session_locked");
        }
        let expected = &req.expected_context;
        if let Some(token) = &expected.focus_token {
            if self
                .get_selected_client_key()
                .map(|k| self.eva_window_token(k))
                .as_ref()
                != Some(token)
            {
                return failure("stale_context: focus changed");
            }
        }
        if let Some(token) = &expected.monitor_token {
            let Some(m) = self.eva_resolve_monitor(token) else {
                return failure("target_gone: monitor");
            };
            if self.monitor_key_is_locked(m) {
                return failure("session_locked");
            }
            if expected
                .visible_tag_mask
                .is_some_and(|mask| self.state.monitors[m].get_active_tags() != mask)
            {
                return failure("stale_context: visible tags changed");
            }
        } else if expected.visible_tag_mask.is_some() {
            return failure("plan_invalid: tag condition requires monitor");
        }
        if self.eva.operations.len() >= MAX_OPERATIONS {
            return failure("execution_capacity_exhausted");
        }
        if let Err(error) =
            jwm_eva_core::actions::validate_execution_action(&jwm_eva_core::protocol::Action {
                name: req.action.name.clone(),
                arguments: req.action.arguments.clone(),
            })
        {
            return failure(&format!("plan_invalid: {error}"));
        }
        if let Err(error) = self.eva_check_undo_state(&req.action) {
            return failure(&error);
        }
        let sensitive = jwm_eva_core::actions::semantic_actions()
            .iter()
            .find(|spec| spec.canonical_name == req.action.name)
            .is_some_and(|spec| spec.risk_class == "confirmation");
        let confirmation_index = if sensitive {
            self.eva
                .confirmation
                .as_ref()
                .filter(|c| {
                    c.approved
                        && c.expires > Instant::now()
                        && req.confirmation_token.as_deref() == Some(c.token.as_str())
                })
                .and_then(|c| {
                    c.targets.as_array().and_then(|actions| {
                        actions
                            .iter()
                            .enumerate()
                            .find(|(i, a)| {
                                !c.used.get(*i).copied().unwrap_or(true)
                                    && **a
                                        == serde_json::to_value(&req.action).unwrap_or(Value::Null)
                            })
                            .map(|(i, _)| i)
                    })
                })
        } else {
            None
        };
        if sensitive && confirmation_index.is_none() {
            return failure("confirmation_required");
        }
        if req.action.name == "restore_session" {
            let Some(c) = self.eva.confirmation.as_ref() else {
                return failure("confirmation_required");
            };
            let Some(snapshot) = c.restore_snapshot.as_ref() else {
                return failure("confirmation_required");
            };
            let mut windows = self
                .eva_restore_matching_keys(snapshot)
                .into_iter()
                .map(|w| self.eva_window_token(w))
                .collect::<Vec<_>>();
            windows.sort();
            let mut monitors = self
                .state
                .monitors
                .keys()
                .map(|m| self.eva_monitor_token(m))
                .collect::<Vec<_>>();
            monitors.sort();
            if windows != c.restore_windows || monitors != c.restore_monitors {
                return failure("stale_context: restore scope changed after confirmation");
            }
        }
        // Reserve BEFORE executing: failures and panics must not cause replay.
        self.eva.operations.insert(
            req.operation_id.clone(),
            Operation {
                digest: req.request_digest.clone(),
                request: args.clone(),
                result: json!({"operation_id":req.operation_id,"status":"in_flight"}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: None,
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        if let Some(index) = confirmation_index {
            if let Some(c) = self.eva.confirmation.as_mut() {
                c.used[index] = true;
            }
        }
        let result = self.eva_dispatch(
            backend,
            &req.action,
            &req.expected_context,
            &req.operation_id,
        );
        let result = match result {
            Ok((status, verified)) => {
                json!({"operation_id":req.operation_id,"status":status,"verified":verified})
            }
            Err(error) => json!({"operation_id":req.operation_id,"status":"failed","error":error}),
        };
        if let Some(op) = self.eva.operations.get_mut(&req.operation_id) {
            op.terminal = result["status"] != "accepted" && result["status"] != "unknown";
            op.result = result.clone();
        }
        IpcResponse::ok(Some(result))
    }
    fn eva_check_undo_state(&self, action: &Action) -> Result<(), String> {
        let Some(expected) = action.arguments.get("expected_state") else {
            return Ok(());
        };
        let fields = expected
            .as_object()
            .ok_or("undo_conflict: invalid expected state")?;
        let args = &action.arguments;
        let wt = expected
            .get("window_token")
            .or_else(|| args.get("window_token"))
            .and_then(Value::as_str);
        let mt = expected
            .get("monitor_token")
            .or_else(|| args.get("monitor_token"))
            .and_then(Value::as_str);
        let w = wt.and_then(|t| self.eva_resolve_window(t));
        let m = mt.and_then(|t| self.eva_resolve_monitor(t));
        let mut actual = serde_json::Map::new();
        actual.insert(
            "focus_token".into(),
            json!(
                self.get_selected_client_key()
                    .map(|window| self.eva_window_token(window))
            ),
        );
        actual.insert(
            "selected_monitor_token".into(),
            json!(
                self.state
                    .sel_mon
                    .map(|monitor| self.eva_monitor_token(monitor))
            ),
        );
        if let Some(w) = w {
            let c = &self.state.clients[w];
            actual.insert("window_token".into(), json!(self.eva_window_token(w)));
            actual.insert("tag_mask".into(), json!(c.state.tags));
            actual.insert("minimized".into(), json!(c.state.is_hidden));
            actual.insert(
                "maximized".into(),
                json!(c.state.is_maximized_vert && c.state.is_maximized_horz),
            );
            actual.insert("floating".into(), json!(c.state.is_floating));
            actual.insert("sticky".into(), json!(c.state.is_sticky));
            actual.insert("maximized_horz".into(), json!(c.state.is_maximized_horz));
            actual.insert("maximized_vert".into(), json!(c.state.is_maximized_vert));
            actual.insert("pip".into(), json!(c.state.is_pip));
            if let Some(mon) = c.mon {
                actual.insert("monitor_token".into(), json!(self.eva_monitor_token(mon)));
            }
        }
        if let Some(m) = m {
            let mon = &self.state.monitors[m];
            actual.insert("monitor_token".into(), json!(self.eva_monitor_token(m)));
            actual.insert("visible_tag_mask".into(), json!(mon.get_active_tags()));
            if w.is_none() {
                if let Some(mask) = expected
                    .get("tag_mask")
                    .or_else(|| args.get("tag_mask"))
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|v| v.count_ones() == 1)
                {
                    let slot = mask.trailing_zeros() as usize + 1;
                    let layout = mon
                        .pertag
                        .as_ref()
                        .and_then(|p| p.lts.get(slot))
                        .unwrap_or(&mon.lt);
                    actual.insert("tag_mask".into(), json!(mask));
                    actual.insert("layout".into(), json!(canonical_layout(layout)));
                }
            }
        }
        if fields
            .iter()
            .all(|(key, value)| actual.get(key) == Some(value))
        {
            Ok(())
        } else {
            Err("undo_conflict: desktop state changed since original action".into())
        }
    }

    fn eva_panel_visible(&self, name: &str) -> Option<bool> {
        let ui = &self.features.system_ui;
        let kind = match name {
            "app_launcher" => "launcher",
            "window_switcher" => "window_switcher",
            "control_center" => "control_center",
            "calendar" => "calendar",
            "notification_center" => "notification_center",
            "audio_output_picker" => "audio_output_picker",
            "audio_input_picker" => "audio_input_picker",
            "bluetooth_picker" => "bluetooth_picker",
            "wifi_picker" => "wifi_picker",
            "wallpaper_picker" => "wallpaper_picker",
            "layout_picker" => "layout_picker",
            "clipboard_picker" => "clipboard_picker",
            "session_menu" => "session_menu",
            "music_player" => "music_player",
            "video_player" => "video_player",
            "monitor_layout" => "monitor_layout",
            "restore_preview" => return Some(ui.is_restore_preview()),
            _ => return None,
        };
        Some(
            ui.panel_kind() == Some(kind)
                || name == "control_center" && self.features.system_ui_return_to_hub,
        )
    }
    fn eva_toggle_state(&self, backend: &dyn Backend, name: &str) -> Option<bool> {
        match name {
            "togglebar" => self.query_focused_show_bar(backend)["show_bar"].as_bool(),
            "toggle_dnd" => Some(self.do_not_disturb),
            "toggle_idle_inhibit" => Some(self.idle_inhibited),
            "toggle_night_light" => Some(self.night_light_active()),
            "toggle_wifi" => self
                .features
                .connectivity
                .network
                .as_ref()
                .map(|n| n.wifi_enabled),
            "toggle_bluetooth" => self
                .features
                .connectivity
                .bluetooth
                .present
                .then_some(self.features.connectivity.bluetooth.powered),
            "togglecompositor" => Some(backend.has_compositor()),
            "togglepartialdamage" => Some(backend.has_partial_damage()),
            "toggle_waterlily" => backend.compositor_waterlily_status().map(|s| s.enabled),
            "toggle_overview" => Some(self.features.overview.active),
            "toggle_tags_overview" => Some(self.features.system_ui.is_tags_overview()),
            "toggle_expose" => Some(self.features.expose_active),
            "toggle_peek" => Some(self.features.peek_active),
            "toggle_magnifier" => Some(self.features.magnifier.enabled),
            "toggle_annotation" => Some(self.features.annotation_active),
            "toggle_shortcut_passthrough" => Some(self.shortcut_passthrough_owner.is_some()),
            "toggle_recording" => Some(self.features.recording.active),
            "toggle_audio_recording" => Some(self.features.audio_recording.active),
            "toggle_spawn_placement" => self
                .get_selected_client_key()
                .and_then(|w| self.client_spawn_placement_pinned(w)),
            "toggle_desktop_shell_mode" => {
                let c = CONFIG.load().desktop_shell_config();
                Some(
                    ![c.left, c.right, c.bottom]
                        .contains(&xbar_core::edge_shell::RevealMode::Hover),
                )
            }
            "scrolling_toggle_attach_mode" => self
                .state
                .sel_mon
                .and_then(|m| self.scrolling_state_for_monitor(m))
                .map(|s| s.attach_new_windows_to_focused_column),
            "togglefloating" => self
                .get_selected_client_key()
                .map(|w| self.state.clients[w].state.is_floating),
            "togglesticky" => self
                .get_selected_client_key()
                .map(|w| self.state.clients[w].state.is_sticky),
            "togglepip" => self
                .get_selected_client_key()
                .map(|w| self.state.clients[w].state.is_pip),
            "togglemaximize" => self.get_selected_client_key().map(|w| {
                let c = &self.state.clients[w];
                c.state.is_maximized_vert && c.state.is_maximized_horz
            }),
            _ => None,
        }
    }
    fn eva_legacy_postcondition(
        &self,
        name: &str,
        args: &Value,
        overview_before: usize,
        overview_count: usize,
        backend: &dyn Backend,
    ) -> bool {
        if let Some(target) = args.get("enabled").and_then(Value::as_bool) {
            if let Some(visible) = self.eva_panel_visible(name) {
                return visible == target;
            }
        }
        let m = self.state.sel_mon.and_then(|m| self.state.monitors.get(m));
        match name {
            "save_session" => self.saved_session_matches_live(backend),
            "lock_screen" => {
                self.features.system_ui.is_locked() || backend.compositor_session_locked()
            }
            "lock_monitor" => args["monitor_token"]
                .as_str()
                .and_then(|token| self.eva_resolve_monitor(token))
                .is_some_and(|monitor| self.monitor_key_is_locked(monitor)),

            "set_recording_region" => {
                let parsed = args["x"]
                    .as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .zip(args["y"].as_i64().and_then(|n| i32::try_from(n).ok()))
                    .zip(args["width"].as_i64().and_then(|n| i32::try_from(n).ok()))
                    .zip(args["height"].as_i64().and_then(|n| i32::try_from(n).ok()))
                    .map(|(((x, y), w), h)| crate::core::types::Rect::new(x, y, w, h));
                self.features.recording.active
                    && parsed
                        .and_then(|r| self.normalize_initial_recording_region(r).ok())
                        .is_some_and(|r| self.features.recording.region == Some(r))
            }
            "clear_clipboard" => self.features.clipboard.is_empty(),
            "clear_notifications" => self.features.notifications.is_empty(),
            "close_notification" => args["id"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .is_some_and(|id| self.features.notifications.get(id).is_none()),
            "set_config" => args["key"].as_str().is_some_and(|key| {
                let cfg = CONFIG.load();
                cfg.with_validated_value(key, &args["value"])
                    .ok()
                    .and_then(|expected| expected.value_at(key))
                    .is_some_and(|expected| cfg.value_at(key) == Some(expected))
            }),
            "set_config_batch" => args["changes"].as_array().is_some_and(|changes| {
                changes.iter().all(|change| {
                    change["key"].as_str().is_some_and(|key| {
                        let cfg = CONFIG.load();
                        cfg.with_validated_value(key, &change["value"])
                            .ok()
                            .and_then(|expected| expected.value_at(key))
                            .is_some_and(|expected| cfg.value_at(key) == Some(expected))
                    })
                })
            }),
            "reload_config" => self.query_config_status()["reload"]["last_success"] == true,
            "stop_audio_recording" => {
                !self.features.audio_recording.active
                    && !self.features.audio_recording.is_finalizing()
                    && self.features.audio_recording.last_error.is_none()
            }
            "start_audio_recording" => {
                self.features.audio_recording.active
                    && self.features.audio_recording.output_path.as_deref() == args["path"].as_str()
            }
            "start_recording" => {
                self.features.recording.active
                    && self.features.recording.output_path.as_deref() == args["path"].as_str()
            }
            "stop_recording" => {
                !self.features.recording.active
                    && self.features.recording.finalized
                    && self.features.gif_exports.is_empty()
            }
            "setmfact" => m
                .zip(args["value"].as_f64())
                .is_some_and(|(m, n)| (f64::from(m.layout.m_fact) - n).abs() < 0.0001),
            "setnmaster" => m
                .zip(args["value"].as_u64())
                .is_some_and(|(m, n)| u64::from(m.layout.n_master) == n),
            "setgaps" => m
                .zip(args["value"].as_i64())
                .is_some_and(|(m, n)| i64::from(m.layout.gap) == n),
            "reset_layout_size" => self.state.sel_mon.is_some_and(|m| {
                self.layout_resize_scope(m).is_some_and(|scope| {
                    !self.state.monitors[m].layout_resizes.contains_key(&scope)
                })
            }),
            "focus_none" => self.get_selected_client_key().is_none(),
            "refocus" => args["window_token"].as_str().is_some_and(|token| {
                self.get_selected_client_key()
                    .map(|w| self.eva_window_token(w))
                    .as_deref()
                    == Some(token)
            }),
            "cycle_overview" => {
                overview_count > 0
                    && self.features.overview.active
                    && self.features.overview.index
                        == if args["value"].as_i64().unwrap_or(1) > 0 {
                            (overview_before + 1) % overview_count
                        } else {
                            (overview_before + overview_count - 1) % overview_count
                        }
            }
            _ => false,
        }
    }

    fn eva_dispatch(
        &mut self,
        backend: &mut dyn Backend,
        action: &Action,
        _expected: &ExpectedContext,
        operation_id: &str,
    ) -> Result<(&'static str, Value), String> {
        let args = &action.arguments;
        if !args.is_object() {
            return Err("plan_invalid: action arguments must be object".into());
        }
        let monitor = || {
            args.get("monitor_token")
                .and_then(Value::as_str)
                .and_then(|t| self.eva_resolve_monitor(t))
                .ok_or_else(|| "target_gone: monitor".to_string())
        };
        let window = || {
            args.get("window_token")
                .or_else(|| args.get("target_token"))
                .and_then(Value::as_str)
                .and_then(|t| self.eva_resolve_window(t))
                .ok_or_else(|| "target_gone: window".to_string())
        };
        let tag_mask = || -> Result<u32, String> {
            let mask = if let Some(n) = args.get("tag_number").and_then(Value::as_u64) {
                if n == 0 || n > 32 {
                    return Err("plan_invalid: tag range".into());
                }
                1u32 << (n - 1)
            } else {
                args.get("tag_mask")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| "plan_invalid: tag_mask".to_string())?
            };
            if mask == 0 || mask & !CONFIG.load().tagmask() != 0 {
                Err("plan_invalid: tag outside configured range".into())
            } else {
                Ok(mask)
            }
        };
        match action.name.as_str() {
            "switch_tag" | "switch_tags" => {
                let m = monitor()?;
                let mask = tag_mask()?;
                self.view_on_monitor(backend, m, mask)
                    .map_err(|e| e.to_string())?;
                if self.state.monitors[m].get_active_tags() != mask {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"monitor_token":self.eva_monitor_token(m),"visible_tag_mask":mask}),
                ))
            }
            "toggle_tags" => {
                let m = monitor()?;
                let mask = tag_mask()?;
                let target = self.state.monitors[m].get_active_tags() ^ mask;
                if target == 0 {
                    return Err("plan_invalid: no visible tag".into());
                }
                self.view_on_monitor(backend, m, target)
                    .map_err(|e| e.to_string())?;
                Ok((
                    "succeeded",
                    json!({"monitor_token":self.eva_monitor_token(m),"visible_tag_mask":self.state.monitors[m].get_active_tags()}),
                ))
            }
            "set_layout" | "set_layout_at" => {
                let m = monitor()?;
                let mask = tag_mask()?;
                if mask.count_ones() != 1 {
                    return Err("plan_invalid: layout requires one tag".into());
                }
                let layout = args
                    .get("layout")
                    .and_then(Value::as_str)
                    .and_then(parse_layout)
                    .ok_or_else(|| "plan_invalid: layout".to_string())?
                    .clone();
                let slot = mask.trailing_zeros() as usize + 1;
                let active = self.state.monitors[m].get_active_tags() == mask;
                if active {
                    self.apply_layout_change(backend, m, |this, key| {
                        this.set_new_layout(key, &std::rc::Rc::new(layout.clone()), slot);
                        Ok(std::rc::Rc::new(layout.clone()))
                    })
                    .map_err(|e| e.to_string())?;
                } else {
                    let p = self.state.monitors[m]
                        .pertag
                        .as_mut()
                        .ok_or_else(|| "unsupported_action: no per-tag layouts".to_string())?;
                    if slot >= p.lts.len() {
                        return Err("target_gone: tag".into());
                    }
                    p.prev_lts[slot] = p.lts[slot].clone();
                    p.lts[slot] = std::rc::Rc::new(layout.clone());
                    self.mark_layout_dirty();
                    self.broadcast_monitor_bar_ipc(backend, m);
                }
                let actual = self.state.monitors[m]
                    .pertag
                    .as_ref()
                    .and_then(|p| p.lts.get(slot))
                    .unwrap_or(&self.state.monitors[m].lt);
                if **actual != layout {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"monitor_token":self.eva_monitor_token(m),"tag_mask":mask,"layout":canonical_layout(&layout)}),
                ))
            }
            "toggle_window_tags" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let mask = tag_mask()?;
                let target = self.state.clients[w].state.tags ^ mask;
                if target == 0 {
                    return Err("plan_invalid: window needs at least one tag".into());
                }
                self.move_client_to_tag(backend, w, target)
                    .map_err(|e| e.to_string())?;
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"tag_mask":self.state.clients[w].state.tags}),
                ))
            }
            "move_window_to_tag" | "move_window_to_tags" => {
                let w = window()?;
                let mask = tag_mask()?;
                self.eva_mutable_window(w)?;
                if let Some(token) = args.get("monitor_token").and_then(Value::as_str) {
                    if self.state.clients[w].mon != self.eva_resolve_monitor(token) {
                        return Err("stale_context: window monitor changed".into());
                    }
                }
                self.move_client_to_tag(backend, w, mask)
                    .map_err(|e| e.to_string())?;
                if self.state.clients[w].state.tags != mask {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"tag_mask":mask}),
                ))
            }
            "focus_tab" => {
                let m = monitor()?;
                let index = args["tab_index"]
                    .as_u64()
                    .ok_or("plan_invalid: tab index")? as usize;
                let target = self
                    .tab_group_clients(m)
                    .get(index)
                    .copied()
                    .ok_or("target_gone: tab")?;
                if args["expected_window_token"].as_str()
                    != Some(self.eva_window_token(target).as_str())
                {
                    return Err("stale_context: tab group changed".into());
                }
                self.eva_mutable_window(target)?;
                self.focus_tab(
                    backend,
                    &WMArgEnum::StringVec(vec![
                        self.state.monitors[m].num.to_string(),
                        index.to_string(),
                    ]),
                )
                .map_err(|e| e.to_string())?;
                if self.get_selected_client_key() != Some(target) {
                    return Err("postcondition_failed: focused tab".into());
                }
                Ok((
                    "succeeded",
                    json!({"focus_token":self.eva_window_token(target),"monitor_token":self.eva_monitor_token(m),"tab_index":index}),
                ))
            }
            "set_window_factor" | "adjust_scrolling_column_width" => {
                let w = window()?;
                let m = monitor()?;
                let mask = tag_mask()?;
                self.eva_mutable_window(w)?;
                if self.get_selected_client_key() != Some(w)
                    || self.state.sel_mon != Some(m)
                    || self.state.monitors[m].get_active_tags() != mask
                {
                    return Err("stale_context: factor source changed".into());
                }
                if action.name == "set_window_factor" {
                    if self.is_scrolling_layout() {
                        return Err("unsupported_action: factor requires a non-scrolling layout; use adjust_scrolling_column_width".into());
                    }
                    let target = args["factor"].as_f64().ok_or("plan_invalid: factor")? as f32;
                    let current = self.state.clients[w].state.client_fact;
                    if (target - current).abs() > 0.0001 {
                        self.setcfact(backend, &WMArgEnum::Float(target - current))
                            .map_err(|e| e.to_string())?;
                    }
                    if (self.state.clients[w].state.client_fact - target).abs() > 0.0001 {
                        return Err("postcondition_failed: factor".into());
                    }
                    Ok((
                        "succeeded",
                        json!({"window_token":self.eva_window_token(w),"factor":self.state.clients[w].state.client_fact}),
                    ))
                } else {
                    if !self.is_scrolling_layout() {
                        return Err("unsupported_action: not a scrolling layout".into());
                    }
                    let delta = args["delta"].as_f64().ok_or("plan_invalid: delta")? as f32;
                    let state = self
                        .scrolling_state_for_monitor(m)
                        .ok_or("target_gone: column")?;
                    let index = state
                        .columns
                        .iter()
                        .position(|col| col.contains(&w))
                        .ok_or("target_gone: column")?;
                    let before = state.column_width_factors.get(index).copied().unwrap_or(1.);
                    let target = if delta.abs() < 0.0001 {
                        1.
                    } else {
                        (before + delta).clamp(0.25, 2.5)
                    };
                    self.scrolling_set_column_width(backend, &WMArgEnum::Float(delta))
                        .map_err(|e| e.to_string())?;
                    let actual = self
                        .scrolling_state_for_monitor(m)
                        .and_then(|s| s.column_width_factors.get(index))
                        .copied()
                        .ok_or("target_gone: column")?;
                    if (actual - target).abs() > 0.0001 {
                        return Err("postcondition_failed: column width".into());
                    }
                    Ok((
                        "succeeded",
                        json!({"window_token":self.eva_window_token(w),"monitor_token":self.eva_monitor_token(m),"column_width_factor":actual}),
                    ))
                }
            }
            "focus_window" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let id = self.state.clients[w].win.raw();
                self.focus_window(backend, &WMArgEnum::UInt64(id))
                    .map_err(|e| e.to_string())?;
                if self.get_selected_client_key() != Some(w) {
                    return Err("postcondition_failed".into());
                }
                Ok(("succeeded", json!({"focus_token":self.eva_window_token(w)})))
            }
            "focus_monitor" => {
                let m = monitor()?;
                let target = self.state.monitors[m].sel;
                self.switch_monitor_focusing(backend, Some(m), target)
                    .map_err(|e| e.to_string())?;
                if self.state.sel_mon != Some(m) {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"monitor_token":self.eva_monitor_token(m)}),
                ))
            }
            "move_window_to_monitor" => {
                let w = window()?;
                let m = monitor()?;
                let restore_tags = args
                    .get("restore_tag_mask")
                    .map(|value| {
                        value
                            .as_u64()
                            .and_then(|value| u32::try_from(value).ok())
                            .filter(|mask| *mask != 0 && mask & !CONFIG.load().tagmask() == 0)
                            .ok_or_else(|| {
                                "plan_invalid: restore tags outside configured range".to_string()
                            })
                    })
                    .transpose()?;
                self.eva_mutable_window(w)?;
                if self.monitor_key_is_locked(m) {
                    return Err("session_locked".into());
                }
                self.sendmon(backend, Some(w), Some(m));
                if let Some(tags) = restore_tags {
                    self.move_client_to_tag(backend, w, tags)
                        .map_err(|error| error.to_string())?;
                }
                if self.state.clients[w].mon != Some(m)
                    || restore_tags.is_some_and(|tags| self.state.clients[w].state.tags != tags)
                {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"monitor_token":self.eva_monitor_token(m),"tag_mask":self.state.clients[w].state.tags}),
                ))
            }
            "set_window_minimized" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let enabled = args
                    .get("minimized")
                    .or_else(|| args.get("enabled"))
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "plan_invalid: minimized".to_string())?;
                self.set_client_minimized(backend, w, enabled)
                    .map_err(|e| e.to_string())?;
                if self.state.clients[w].state.is_hidden != enabled {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"minimized":enabled}),
                ))
            }
            "set_window_maximized" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let enabled = args
                    .get("maximized")
                    .or_else(|| args.get("enabled"))
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "plan_invalid: maximized".to_string())?;
                let axes = if enabled {
                    crate::backend::api::MaximizeAxes::BOTH
                } else {
                    crate::backend::api::MaximizeAxes::NONE
                };
                self.set_client_maximized(
                    backend,
                    w,
                    axes,
                    crate::core::maximize::MaximizeOrigin::User,
                )
                .map_err(|e| e.to_string())?;
                let c = &self.state.clients[w];
                if c.state.is_maximized_vert != enabled || c.state.is_maximized_horz != enabled {
                    return Err("postcondition_failed".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"maximized":enabled}),
                ))
            }
            "set_window_floating" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let enabled = args
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or("plan_invalid: enabled")?;
                if self.get_selected_client_key() != Some(w) {
                    return Err("stale_context: floating target no longer focused".into());
                }
                if self.state.clients[w].state.is_floating != enabled {
                    self.togglefloating(backend, &WMArgEnum::Int(0))
                        .map_err(|e| e.to_string())?;
                }
                if self.state.clients[w].state.is_floating != enabled {
                    return Err("postcondition_failed: floating".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"floating":enabled}),
                ))
            }
            "set_window_pip" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let enabled = args
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or("plan_invalid: enabled")?;
                self.set_client_pip(backend, w, enabled)
                    .map_err(|e| e.to_string())?;
                if self.state.clients[w].state.is_pip != enabled {
                    return Err("postcondition_failed: pip".into());
                }
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"pip":enabled}),
                ))
            }
            "set_window_sticky" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                let enabled = args
                    .get("sticky")
                    .or_else(|| args.get("enabled"))
                    .and_then(Value::as_bool)
                    .ok_or_else(|| "plan_invalid: sticky".to_string())?;
                self.set_client_sticky(backend, w, enabled);
                Ok((
                    "succeeded",
                    json!({"window_token":self.eva_window_token(w),"sticky":self.state.clients[w].state.is_sticky}),
                ))
            }
            "close_window" => {
                let w = window()?;
                self.eva_mutable_window(w)?;
                // killclient is focus-dependent, never refocus implicitly.
                if self.get_selected_client_key() != Some(w) {
                    return Err("stale_context: close target not focused".into());
                }
                self.killclient(backend, &WMArgEnum::Int(0))
                    .map_err(|e| e.to_string())?;
                Ok((
                    "accepted",
                    json!({"window_token":self.eva_window_token(w),"completion":"window disappearance"}),
                ))
            }
            "restore_session" => {
                let snapshot = self
                    .eva
                    .confirmation
                    .as_ref()
                    .and_then(|c| c.restore_snapshot.clone())
                    .ok_or("confirmation_required")?;
                let verified = self.eva_restore_frozen_snapshot(backend, &snapshot);
                let status = match verified["status"].as_str() {
                    Some("succeeded") => "succeeded",
                    Some("partial") => "partial",
                    _ => "failed",
                };
                Ok((status, verified))
            }
            name @ ("media_control" | "media_play_pause" | "media_next" | "media_previous"
            | "media_stop") => {
                let command = match name {
                    "media_control" => args["action"]
                        .as_str()
                        .ok_or("plan_invalid: media action")?,
                    "media_play_pause" => "play_pause",
                    "media_next" => "next",
                    "media_previous" => "previous",
                    _ => "stop",
                };
                let state = self
                    .features
                    .media
                    .get()
                    .ok_or("target_gone: no media player")?;
                let player = state.player.clone();
                let row = self.eva.media_players.get(&player).ok_or(
                    "unsupported_action: update the MPRIS bridge to verify player identity",
                )?;
                let owner = row["bus_owner"]
                    .as_str()
                    .ok_or("target_gone: media owner")?
                    .to_owned();
                let pid = row["process_id"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or("target_gone: media PID")?;
                let track = row["track_id"].as_str().filter(|track| {
                    track.starts_with('/') && *track != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                });
                if command == "seek" && track.is_none() {
                    return Err(
                        "unsupported_action: player has no stable track ID for absolute seek"
                            .into(),
                    );
                }
                let before = json!({"player":player,"status":state.status.as_str(),"track_id":row["track_id"]});
                let target =
                    json!({"player":player,"bus_owner":owner,"process_id":pid,"track_id":track});
                let payload = json!({"action":command,"target_player":player,"target_bus_owner":owner,
                    "target_pid":pid,"target_track_id":track,"position_us":args["position_us"],
                    "assistant_operation_id":operation_id,"wm_instance_id":self.eva.instance});
                self.broadcast_ipc_event("media/command", payload);
                Ok((
                    "accepted",
                    json!({"before":before,"media_target":target,"media_action":command,
                    "completion":"awaiting_postcondition"}),
                ))
            }
            "unlock_monitor" => Err(
                "policy_requires_local_auth: use the trusted local authentication prompt".into(),
            ),
            "clipboard_copy" => {
                use crate::backend::clipboard_offer::{CapturedClipboard, ClipboardReceiptStatus};
                use crate::jwm::features::ClipboardEntry;
                let index = args["index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("plan_invalid: clipboard index")?;
                let entry = self
                    .features
                    .clipboard
                    .get(index)
                    .cloned()
                    .ok_or("target_gone: clipboard entry")?;
                let offer = match &entry {
                    ClipboardEntry::Text { text, .. } => CapturedClipboard::Text(text.clone()),
                    ClipboardEntry::Png { bytes, .. } => CapturedClipboard::Png(bytes.clone()),
                };
                let receipt = backend
                    .offer_clipboard_with_receipt(offer)
                    .ok_or("unsupported_action: native clipboard ownership receipts unavailable")?;
                let status = match receipt.status() {
                    ClipboardReceiptStatus::Pending => "accepted",
                    ClipboardReceiptStatus::Owned => "succeeded",
                    ClipboardReceiptStatus::Failed => "failed",
                };
                if status != "failed" {
                    match entry {
                        ClipboardEntry::Text { text, .. } => {
                            self.record_clipboard(&text);
                        }
                        ClipboardEntry::Png { bytes, .. } => {
                            self.record_clipboard_png(&bytes);
                        }
                    }
                }
                if let Some(op) = self.eva.operations.get_mut(operation_id) {
                    op.clipboard_receipt = Some(receipt);
                }
                Ok((
                    status,
                    json!({"completion":match status {"succeeded"=>"native_clipboard_ownership_observed","failed"=>"native_clipboard_offer_failed",_=>"awaiting_native_clipboard_ownership"}}),
                ))
            }
            "launch_application" => {
                let argv: Vec<String> = args["argv"]
                    .as_array()
                    .ok_or("plan_invalid: argv")?
                    .iter()
                    .map(|v| v.as_str().map(str::to_string).ok_or("plan_invalid: argv"))
                    .collect::<Result<_, _>>()?;
                if argv.is_empty()
                    || argv.len() > 32
                    || argv
                        .iter()
                        .any(|s| s.is_empty() || s.len() > 4096 || s.contains('\0'))
                {
                    return Err("plan_invalid: argv".into());
                }
                let mut command = std::process::Command::new(&argv[0]);
                command.args(&argv[1..]);
                Self::setup_smithay_child_env(&mut command, backend);
                Self::apply_child_pre_exec(&mut command);
                command
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let child = command
                    .spawn()
                    .map_err(|e| format!("application_spawn_failed: {e}"))?;
                let pid = child.id();
                self.supervise_transient_child(child);
                Ok((
                    "succeeded",
                    json!({"application_id":args["application_id"],"pid":pid,"completion":"process_spawned"}),
                ))
            }
            "query_window" => {
                let w = window()?;
                let id = self.state.clients[w].win.raw();
                let response = self.handle_ipc_query("get_window", &json!({"id":id}), backend);
                if !response.success {
                    return Err(response.error.unwrap_or_default());
                }
                Ok(("succeeded", response.data.unwrap_or(Value::Null)))
            }
            "query_state" => {
                let name = args
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "plan_invalid: query name".to_string())?;
                if !crate::ipc::is_supported_query(name) || name.starts_with("get_execution") {
                    return Err("unsupported_action".into());
                }
                let response =
                    self.handle_ipc_query(name, args.get("args").unwrap_or(&json!({})), backend);
                if !response.success {
                    return Err(response.error.unwrap_or_default());
                }
                Ok(("succeeded", response.data.unwrap_or(Value::Null)))
            }
            other => {
                // The reviewed registry is an allowlist; internal bridge writes
                // cannot enter this path. Scope tokens are verified here, then
                // removed rather than passed to a legacy handler that ignores them.
                let spec = jwm_eva_core::actions::semantic_actions()
                    .into_iter()
                    .find(|s| s.canonical_name == other)
                    .ok_or("unsupported_action")?;
                if spec.role == "bridge_internal"
                    || !crate::ipc::ipc_capabilities()
                        .commands
                        .iter()
                        .any(|name| name == other)
                {
                    return Err("unsupported_action".into());
                }
                if let Some(token) = args.get("window_token").and_then(Value::as_str) {
                    let w = self
                        .eva_resolve_window(token)
                        .ok_or("target_gone: window")?;
                    self.eva_mutable_window(w)?;
                    if self.get_selected_client_key() != Some(w) {
                        return Err("stale_context: legacy target no longer selected".into());
                    }
                }
                if let Some(token) = args.get("monitor_token").and_then(Value::as_str) {
                    let m = self
                        .eva_resolve_monitor(token)
                        .ok_or("target_gone: monitor")?;
                    if self.state.sel_mon != Some(m) {
                        return Err("stale_context: legacy monitor no longer selected".into());
                    }
                    if let Some(mask) = args.get("tag_mask").and_then(Value::as_u64) {
                        if self.state.monitors[m].get_active_tags() != mask as u32 {
                            return Err("stale_context: workspace changed".into());
                        }
                    }
                }
                if matches!(other, "setmfact" | "setcfact") && self.is_scrolling_layout() {
                    return Err(
                        "unsupported_action: use explicit scrolling column actions in this layout"
                            .into(),
                    );
                }
                if other.starts_with("scrolling_")
                    && other != "scrolling_toggle_attach_mode"
                    && !self.is_scrolling_layout()
                {
                    return Err("unsupported_action: not a scrolling layout".into());
                }
                if other == "cycle_overview"
                    && (!self.features.overview.active || self.features.overview.clients.is_empty())
                {
                    return Err("unsupported_action: overview is not active".into());
                }
                let desired = args.get("enabled").and_then(Value::as_bool);
                if let Some(target) = desired {
                    if let Some(visible) = self.eva_panel_visible(other) {
                        if visible == target {
                            return Ok((
                                "succeeded",
                                json!({"enabled":target,"kind":self.features.system_ui.panel_kind()}),
                            ));
                        }
                        if !target {
                            self.close_system_ui(backend);
                            return if self.eva_panel_visible(other) == Some(false) {
                                Ok((
                                    "succeeded",
                                    json!({"enabled":false,"kind":self.features.system_ui.panel_kind()}),
                                ))
                            } else {
                                Err("postcondition_failed: panel still visible".into())
                            };
                        }
                    } else {
                        let current = self
                            .eva_toggle_state(backend, other)
                            .ok_or("unsupported_action: state is unavailable")?;
                        if current == target {
                            let finalizing = !target
                                && match other {
                                    "toggle_audio_recording" => {
                                        self.features.audio_recording.is_finalizing()
                                    }
                                    "toggle_recording" => {
                                        self.features.recording.output_path.is_some()
                                            && !self.features.recording.finalized
                                    }
                                    _ => false,
                                };
                            if finalizing {
                                let query = if other == "toggle_audio_recording" {
                                    "get_audio_recording_status"
                                } else {
                                    "get_recording_status"
                                };
                                let state = self.handle_ipc_query(query, &json!({}), backend).data;
                                return Ok((
                                    "accepted",
                                    json!({"verification_query":query,"after":state,"completion":"awaiting_file_finalization"}),
                                ));
                            }
                            return Ok(("succeeded", json!({"enabled":target})));
                        }
                    }
                }
                let mut legacy = args.clone();
                let fields = legacy.as_object_mut().ok_or("plan_invalid")?;
                fields.remove("window_token");
                fields.remove("monitor_token");
                fields.remove("tag_mask");
                fields.remove("expected_state");
                fields.remove("enabled");
                if other == "setmfact" {
                    if let Some(n) = fields.get("value").and_then(Value::as_f64) {
                        fields.insert("value".into(), json!(n + 1.));
                    }
                }
                if other == "setgaps" {
                    if let (Some(n), Some(m)) = (
                        fields.get("value").and_then(Value::as_i64),
                        self.state.sel_mon,
                    ) {
                        fields.insert(
                            "value".into(),
                            json!(n - i64::from(self.state.monitors[m].layout.gap)),
                        );
                    }
                }
                if matches!(other, "lock_monitor" | "unlock_monitor") {
                    fields.insert(
                        "value".into(),
                        json!(
                            self.state
                                .sel_mon
                                .map(|m| self.state.monitors[m].num)
                                .unwrap_or(-1)
                        ),
                    );
                }
                if other == "benchmark" {
                    fields.insert("action".into(), json!("start"));
                }
                if other == "clipboard_copy" && fields.contains_key("text") {
                    return Err("plan_invalid: clipboard_copy requires history index".into());
                }
                let overview_index = self.features.overview.index;
                let overview_count = self.features.overview.clients.len();
                let query = spec
                    .completion_contract
                    .strip_prefix("query:")
                    .filter(|q| crate::ipc::is_supported_query(q));
                let before = query.and_then(|q| self.handle_ipc_query(q, &json!({}), backend).data);
                let response = self.handle_ipc_command(backend, other, &legacy);
                if !response.success {
                    return Err(response.error.unwrap_or_default());
                }
                let after = query.and_then(|q| self.handle_ipc_query(q, &json!({}), backend).data);
                let asynchronous = matches!(
                    other,
                    "killclient"
                        | "toggle_wifi"
                        | "toggle_bluetooth"
                        | "set_mic_mute"
                        | "set_audio_device"
                        | "set_power_profile"
                        | "media_play_pause"
                        | "media_next"
                        | "media_previous"
                        | "media_stop"
                        | "media_control"
                        | "open_media"
                        | "take_screenshot"
                        | "take_screenshot_fullscreen"
                        | "toggle_recording"
                        | "toggle_audio_recording"
                        | "stop_recording"
                        | "restore_session"
                        | "benchmark"
                        | "waterlily_case"
                        | "waterlily_palette"
                        | "quit"
                        | "restart"
                        | "lock_screen"
                );
                let desired_satisfied = desired.is_some_and(|target| {
                    self.eva_toggle_state(backend, other) == Some(target)
                        || self.eva_panel_visible(other) == Some(target)
                });
                let changed = before != after
                    && query.is_some_and(|q| {
                        !matches!(q, "get_status" | "get_st" | "get_config_status")
                    });
                let exact_completed = self.eva_legacy_postcondition(
                    other,
                    args,
                    overview_index,
                    overview_count,
                    backend,
                );
                let completed = !asynchronous
                    && if desired.is_some() {
                        desired_satisfied
                    } else {
                        exact_completed || changed
                    };
                Ok((
                    if completed { "succeeded" } else { "accepted" },
                    json!({"reply":response.data,"verification_query":query,"before":before,"after":after,"completion":if completed{"postcondition_observed"}else{"awaiting_postcondition"}}),
                ))
            }
        }
    }
    pub(crate) fn eva_capture_completed(&mut self, path: &str, succeeded: bool) {
        let mut finished = Vec::new();
        for (id, op) in &mut self.eva.operations {
            if !op.terminal
                && op.request["action"]["name"] == "take_screenshot_fullscreen"
                && op.result["verified"]["reply"]["path"].as_str() == Some(path)
            {
                op.result["status"] = json!(if succeeded { "succeeded" } else { "failed" });
                op.result["verified"]["path"] = json!(path);
                op.terminal = true;
                op.expires = Instant::now() + RETENTION;
                finished.push((id.clone(), op.result.clone()));
            }
        }
        for (id, result) in finished {
            self.broadcast_ipc_event(
                "assistant/execution",
                json!({"operation_id":id,"result":result}),
            );
        }
    }

    pub(crate) fn eva_note_media_launch_sequence(
        &mut self,
        id: u64,
        keepalive: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    ) {
        for op in self.eva.operations.values_mut().filter(|op| {
            op.result["status"] == "in_flight" && op.request["action"]["name"] == "open_media"
        }) {
            op.control_sequence = Some(("media_launch".into(), id));
            op.background_keepalive = Some(keepalive.clone());
        }
    }
    pub(crate) fn eva_media_launch_prepared(
        &mut self,
        sequence: u64,
        resource_uri: &str,
    ) -> Result<Option<Value>, String> {
        let active = self.eva_active();
        let permit = serde_json::to_value(&self.eva.permit).unwrap_or(Value::Null);
        if self.eva.operations.values().any(|op| {
            !op.terminal
                && op
                    .control_sequence
                    .as_ref()
                    .is_some_and(|(domain, id)| domain == "media_launch" && *id == sequence)
                && (!active || op.request["assistant_permit"] != permit)
        }) {
            return Err("assistant_permit_expired_before_player_spawn".into());
        }
        let previous_count = self
            .eva
            .media_observation
            .as_ref()
            .and_then(|row| row["queued_urls"].as_array())
            .map(|urls| {
                urls.iter()
                    .filter(|url| url.as_str() == Some(resource_uri))
                    .count()
            });
        let target = self.eva.media_players.get("jwm");
        let target_owner = target
            .and_then(|row| row["bus_owner"].as_str())
            .map(str::to_owned);
        let target_pid = target
            .and_then(|row| row["process_id"].as_u64())
            .and_then(|n| u32::try_from(n).ok());
        let instance = self.eva.instance.clone();
        let mut request = None;
        for (id, op) in self.eva.operations.iter_mut().filter(|(_, op)| {
            !op.terminal
                && op
                    .control_sequence
                    .as_ref()
                    .is_some_and(|(domain, id)| domain == "media_launch" && *id == sequence)
        }) {
            op.result["verified"]["expected_resource_uri"] = json!(resource_uri);
            let metadata = json!({"operation_id":id,"wm_instance_id":instance,"sequence":sequence,"resource_uri":resource_uri,
                "enqueue":op.request["action"]["arguments"]["enqueue"],"target_player":"jwm",
                "target_bus_owner":target_owner,"target_pid":target_pid});
            op.result["verified"]["media_request"] = metadata.clone();
            request = Some(metadata);

            op.result["verified"]["media_before_count"] = json!(previous_count);
            op.result["verified"]["media_before_owner"] = self
                .eva
                .media_observation
                .as_ref()
                .map(|row| row["bus_owner"].clone())
                .unwrap_or(Value::Null);
            op.result["verified"]["media_before_pid"] = self
                .eva
                .media_observation
                .as_ref()
                .map(|row| row["process_id"].clone())
                .unwrap_or(Value::Null);
            op.result["verified"]["media_before_absent"] = json!(
                self.eva
                    .media_observation
                    .as_ref()
                    .is_some_and(|row| row["player_absent"] == true)
            );
        }
        Ok(request)
    }
    /// The bridge reads these fields from one unique MPRIS bus owner. A
    /// spawned helper or a transport command acknowledgement alone cannot
    /// prove that the requested resource is playing or really queued in mpv.
    pub(crate) fn eva_media_observed(&mut self, observation: &Value) {
        self.eva.media_players.clear();
        for row in observation["player_details"]
            .as_array()
            .into_iter()
            .flatten()
            .take(64)
            .chain(std::iter::once(observation))
        {
            if let (Some(player), Some(owner), Some(pid)) = (
                row["player"].as_str(),
                row["bus_owner"].as_str(),
                row["process_id"].as_u64(),
            ) {
                if !player.is_empty()
                    && player.len() <= 256
                    && owner.starts_with(':')
                    && owner.len() <= 256
                    && pid > 0
                {
                    self.eva.media_players.insert(player.into(), row.clone());
                }
            }
        }
        let row = observation["player_details"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["player"] == "jwm"))
            .or_else(|| (observation["player"] == "jwm").then_some(observation));
        let trusted = row.filter(|row| {
            row["bus_owner"]
                .as_str()
                .is_some_and(|owner| owner.starts_with(':'))
                && row["process_id"].as_u64().is_some_and(|pid| pid > 0)
        });
        self.eva.media_observation = trusted.map(|row| json!({"bus_owner":row["bus_owner"],"process_id":row["process_id"],"queued_urls":row["queued_urls"]}))
            .or_else(||observation["players"].as_array().filter(|players|players.iter().all(|p|p.as_str().is_some_and(|p|p!="jwm")))
                .map(|_|json!({"player_absent":true,"queued_urls":[]})));
    }
    pub(crate) fn eva_media_open_completed(&mut self, completion: &Value) {
        if completion["wm_instance_id"].as_str() != Some(self.eva.instance.as_str()) {
            return;
        }
        let Some(id) = completion["operation_id"].as_str() else {
            return;
        };
        let Some(op) = self
            .eva
            .operations
            .get_mut(id)
            .filter(|op| !op.terminal && op.request["action"]["name"] == "open_media")
        else {
            return;
        };
        let request = &op.result["verified"]["media_request"];
        if !request.is_object()
            || [
                "operation_id",
                "wm_instance_id",
                "sequence",
                "resource_uri",
                "enqueue",
                "target_player",
                "target_bus_owner",
                "target_pid",
            ]
            .iter()
            .any(|key| completion[*key] != request[*key])
            || completion["player"] != "jwm"
            || !completion["bus_owner"]
                .as_str()
                .is_some_and(|s| s.starts_with(':'))
            || !completion["process_id"].as_u64().is_some_and(|pid| pid > 0)
        {
            return;
        }
        if (request["target_bus_owner"].is_string()
            && completion["bus_owner"] != request["target_bus_owner"])
            || (request["target_pid"].is_number()
                && completion["process_id"] != request["target_pid"])
        {
            return;
        }
        let enqueue = request["enqueue"] == true;
        let verified = completion["verified"] == true
            && completion["mpv_command_ack"] == true
            && completion["player_request_id"]
                .as_u64()
                .is_some_and(|n| n > 0)
            && completion["entry_id"].as_u64().is_some_and(|n| n > 0)
            && (enqueue
                || (completion["track_url"] == request["resource_uri"]
                    && completion["status"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("playing"))));
        let gone = completion["verified"] == false && completion["error"] == "target_gone";
        op.result["status"] = json!(if verified {
            "succeeded"
        } else if gone {
            "failed"
        } else {
            "unknown"
        });
        op.result["verified"]["read_back"] = completion.clone();
        op.result["verified"]["completion"] = json!(if verified {
            if enqueue {
                "resource_queued_observed"
            } else {
                "resource_playing_observed"
            }
        } else {
            "media_state_unverified"
        });
        op.result["verified"]["playback_verified"] = json!(verified && !enqueue);
        op.terminal = verified || gone;
        if op.terminal {
            op.expires = Instant::now() + RETENTION;
        }
        let result = op.result.clone();
        self.broadcast_ipc_event(
            "assistant/execution",
            json!({"operation_id":id,"result":result}),
        );
    }
    pub(crate) fn eva_media_command_completed(&mut self, completion: &Value) {
        if completion["wm_instance_id"].as_str() != Some(self.eva.instance.as_str()) {
            return;
        }
        let Some(id) = completion["operation_id"].as_str() else {
            return;
        };
        let Some(op) = self
            .eva
            .operations
            .get_mut(id)
            .filter(|op| !op.terminal && op.result["verified"]["media_action"].is_string())
        else {
            return;
        };
        let target = &op.result["verified"]["media_target"];
        let command = op.result["verified"]["media_action"]
            .as_str()
            .unwrap_or_default();
        if completion["player"] != target["player"]
            || completion["bus_owner"] != target["bus_owner"]
            || completion["process_id"] != target["process_id"]
            || completion["action"].as_str() != Some(command)
        {
            return;
        }
        let status = completion["status"].as_str().unwrap_or_default();
        let proof = match command {
            "play" => status.eq_ignore_ascii_case("playing"),
            "pause" => status.eq_ignore_ascii_case("paused"),
            "stop" => status.eq_ignore_ascii_case("stopped"),
            "play_pause" => completion["before_status"].as_str().is_some_and(|before| {
                if before.eq_ignore_ascii_case("playing") {
                    status.eq_ignore_ascii_case("paused")
                } else if before.eq_ignore_ascii_case("paused")
                    || before.eq_ignore_ascii_case("stopped")
                {
                    status.eq_ignore_ascii_case("playing")
                } else {
                    false
                }
            }),
            "next" | "previous" => completion["before_track_id"]
                .as_str()
                .zip(completion["track_id"].as_str())
                .is_some_and(|(before, after)| {
                    before.starts_with('/')
                        && before != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                        && after.starts_with('/')
                        && before != after
                        && after != "/org/mpris/MediaPlayer2/TrackList/NoTrack"
                }),
            "seek" => {
                completion["track_id"] == target["track_id"]
                    && completion["position_us"]
                        .as_i64()
                        .zip(op.request["action"]["arguments"]["position_us"].as_i64())
                        .is_some_and(|(actual, requested)| actual.abs_diff(requested) <= 1_000_000)
            }
            _ => false,
        };
        let verified = completion["verified"] == true && proof;
        let gone = completion["verified"] == false && completion["error"] == "target_gone";
        op.result["status"] = json!(if verified {
            "succeeded"
        } else if gone {
            "failed"
        } else {
            "unknown"
        });
        op.result["verified"]["read_back"] = completion.clone();
        op.result["verified"]["completion"] = json!(if verified {
            "targeted_media_state_observed"
        } else {
            "media_state_unverified"
        });
        op.terminal = verified || gone;
        if op.terminal {
            op.expires = Instant::now() + RETENTION;
        }
        let result = op.result.clone();
        self.broadcast_ipc_event(
            "assistant/execution",
            json!({"operation_id":id,"result":result}),
        );
    }
    pub(crate) fn eva_note_radio_sequence(
        &mut self,
        id: u64,
        keepalive: std::sync::Arc<dyn std::any::Any + Send + Sync>,
    ) {
        for op in self
            .eva
            .operations
            .values_mut()
            .filter(|op| op.result["status"] == "in_flight")
        {
            if matches!(
                op.request["action"]["name"].as_str(),
                Some("toggle_wifi" | "toggle_bluetooth")
            ) {
                op.control_sequence = Some(("radio".into(), id));
                op.background_keepalive = Some(keepalive.clone());
            }
        }
    }
    pub(crate) fn eva_radio_completion(
        &mut self,
        id: u64,
        state: &crate::jwm::features::connectivity::ConnectivityState,
    ) {
        let mut finished = Vec::new();
        for (op_id, op) in &mut self.eva.operations {
            if op
                .control_sequence
                .as_ref()
                .is_none_or(|(d, s)| d != "radio" || *s != id)
                || op.terminal
            {
                continue;
            }
            let desired = op.request["action"]["arguments"]["enabled"].as_bool();
            let actual = match op.request["action"]["name"].as_str() {
                Some("toggle_wifi") => state.network.as_ref().map(|n| n.wifi_enabled),
                Some("toggle_bluetooth") => {
                    state.bluetooth.present.then_some(state.bluetooth.powered)
                }
                _ => None,
            };
            let status = match actual.zip(desired) {
                Some((a, d)) if a == d => "succeeded",
                Some(_) => "failed",
                None => "unknown",
            };
            op.result["status"] = json!(status);
            op.result["verified"]["enabled"] = json!(actual);
            op.terminal = status != "unknown";
            if op.terminal {
                op.expires = Instant::now() + RETENTION;
            }
            finished.push((op_id.clone(), op.result.clone()));
        }
        for (id, result) in finished {
            self.broadcast_ipc_event(
                "assistant/execution",
                json!({"operation_id":id,"result":result}),
            );
        }
    }

    pub(crate) fn eva_mic_target(&self, seq: u64) -> Option<bool> {
        self.eva
            .operations
            .values()
            .find(|op| {
                op.control_sequence
                    .as_ref()
                    .is_some_and(|(d, s)| d == "mic" && *s == seq)
            })
            .and_then(|op| op.request["action"]["arguments"]["muted"].as_bool())
    }
    pub(crate) fn eva_note_control_sequence(&mut self, domain: &str, seq: u64) {
        for op in self
            .eva
            .operations
            .values_mut()
            .filter(|op| op.result["status"] == "in_flight")
        {
            op.control_sequence = Some((domain.into(), seq));
        }
    }
    pub(crate) fn eva_control_completion(
        &mut self,
        domain: &str,
        seq: u64,
        took: Option<bool>,
        evidence: Value,
    ) {
        let mut finished = Vec::new();
        for (id, op) in &mut self.eva.operations {
            if op
                .control_sequence
                .as_ref()
                .is_some_and(|(d, s)| d == domain && *s == seq)
                && !op.terminal
            {
                let media_pending = domain == "media_launch" && took == Some(true);
                op.result["status"] = json!(match took {
                    Some(true) if media_pending => "accepted",
                    Some(true) => "succeeded",
                    Some(false) => "failed",
                    None => "unknown",
                });
                op.result["verified"]["read_back"] = evidence.clone();
                op.terminal = took.is_some() && !media_pending;
                if op.terminal {
                    op.expires = Instant::now() + RETENTION;
                }
                finished.push((id.clone(), op.result.clone()));
            }
        }
        for (id, result) in finished {
            self.broadcast_ipc_event(
                "assistant/execution",
                json!({"operation_id":id,"result":result}),
            );
        }
    }

    /// Observe only authoritative completion reports. In particular the mic's
    /// optimistic OSD publish is excluded while its feedback estimate exists.
    pub(crate) fn eva_observe_event(&mut self, event: &str, payload: &Value) {
        self.eva_record_focus();
        if event.starts_with("assistant/") {
            return;
        }
        let mic_confirmed = self.features.control_feedback.mic_shown().is_none();
        let event_window_token = payload["id"].as_u64().and_then(|id| {
            self.state
                .clients
                .iter()
                .find(|(_, c)| c.win.raw() == id)
                .map(|(k, _)| self.eva_window_token(k))
        });
        let mut finished = Vec::new();
        for (id, op) in &mut self.eva.operations {
            if op.terminal || op.result["status"] != "accepted" {
                continue;
            }
            let action = &op.request["action"];
            let args = &action["arguments"];
            let name = action["name"].as_str().unwrap_or("");
            let matched = match (name, event) {
                ("killclient" | "close_window", "window/close") => {
                    event_window_token.as_deref() == args["window_token"].as_str()
                }
                ("set_mic_mute", "audio/mic") if op.control_sequence.is_none() => {
                    mic_confirmed && payload["muted"] == args["muted"]
                }
                ("set_power_profile", "power/profile") if op.control_sequence.is_none() => {
                    payload["active"] == args["profile"]
                }
                ("set_audio_device", "audio/devices") if op.control_sequence.is_none() => {
                    args["direction"]
                        .as_str()
                        .and_then(|d| payload[d].as_array())
                        .is_some_and(|devices| {
                            devices
                                .iter()
                                .any(|d| d["default"] == true && d["id"] == args["id"])
                        })
                }
                (
                    "media_stop" | "media_play_pause" | "media_next" | "media_previous"
                    | "media_control",
                    "media/status",
                ) => false,
                _ => false,
            };
            if matched {
                op.result["status"] = json!("succeeded");
                op.result["verified"]["completion_event"] = json!(event);
                op.result["verified"]["completion_payload"] = payload.clone();
                op.terminal = true;
                op.expires = Instant::now() + RETENTION;
                finished.push((id.clone(), op.result.clone()));
            }
        }
        for (id, result) in finished {
            self.broadcast_ipc_event(
                "assistant/execution",
                json!({"operation_id":id,"result":result}),
            );
        }
    }

    fn eva_mutable_window(&self, w: ClientKey) -> Result<(), String> {
        let c = &self.state.clients[w];
        if c.state.is_dock || c.state.is_desktop {
            return Err("unsupported_action: protected surface".into());
        }
        if self.client_is_on_locked_monitor(w) {
            return Err("session_locked".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::api::CompositorWorkspaceEffects;
    use crate::backend::common_define::WindowId;
    use crate::core::models::WMClient;
    use crate::jwm::monitor::test_support::{DisplaySpyBackend, output};

    fn setup() -> (Jwm, DisplaySpyBackend) {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        backend.eva_overlay_supported = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        jwm.eva.generation = 1;
        jwm.eva.permit = Some(AssistantPermit {
            assistant_session_id: "test-session".into(),
            generation: 1,
        });
        jwm.eva.lease_until = Some(Instant::now() + LEASE);
        (jwm, backend)
    }
    fn request(jwm: &Jwm, id: &str, action: Value) -> Value {
        let mut value = json!({"protocol_version":1,"wm_instance_id":jwm.eva.instance,"assistant_permit":jwm.eva.permit,"operation_id":id,"request_digest":"","expected_context":{"session_unlocked":true},"action":action});
        let mut payload = value.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        value["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        value
    }
    #[test]
    fn activation_requires_visible_overlay_and_never_issues_permit_headlessly() {
        let mut backend = DisplaySpyBackend::new(vec![output(1, 0, 0, 1920, 1080)]);
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        if !backend.compositor_supports_eva_overlay() {
            assert!(
                !jwm.eva_control(&mut backend, &json!({"action":"activate"}))
                    .success
            );
            assert!(jwm.eva.permit.is_none());
        }
        backend.eva_overlay_supported = true;
        let activated = jwm.eva_control(&mut backend, &json!({"action":"activate"}));
        assert!(activated.success);
        let issued_at = activated.data.unwrap()["activated_monotonic_ns"]
            .as_u64()
            .unwrap();
        assert!(issued_at > 0 && issued_at <= monotonic_ns());
        let heartbeat = json!({"assistant_permit":jwm.eva.permit,"state":"listening"});
        let refreshed = jwm.eva_set_status(&mut backend, &heartbeat);
        assert_eq!(refreshed.data.unwrap()["activated_monotonic_ns"], issued_at);
        jwm.eva_cancel("test");
        assert!(jwm.eva_overlay_snapshot()["activated_monotonic_ns"].is_null());
    }
    #[test]
    fn activation_scope_is_fixed_while_indicator_moves_and_dark_outputs_revoke_authority() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        backend.eva_overlay_supported = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        let first = jwm.state.monitor_order[0];
        let second = jwm.state.monitor_order[1];
        jwm.state.sel_mon = Some(first);
        backend.eva_visible_outputs = Some(vec![2]);
        assert!(
            jwm.eva_control(&mut backend, &json!({"action":"activate"}))
                .success
        );
        assert_eq!(jwm.eva.activation_monitor, Some(first));
        assert_eq!(jwm.eva.presentation_monitor, Some(second));
        assert_eq!(
            jwm.eva_execution_context()["activation_monitor_token"],
            jwm.eva_monitor_token(first)
        );
        backend.eva_visible_outputs = Some(vec![1]);
        jwm.eva_tick(&mut backend);
        assert_eq!(jwm.eva.activation_monitor, Some(first));
        assert_eq!(jwm.eva.presentation_monitor, Some(first));
        let frozen_scope = jwm.eva_monitor_token(first);
        let old_request = request(
            &jwm,
            "removed-activation",
            json!({"name":"switch_tag","arguments":{"monitor_token":frozen_scope,"tag_mask":2}}),
        );
        jwm.state.monitors.remove(first);
        jwm.state.monitor_order.retain(|m| *m != first);
        jwm.state.sel_mon = Some(second);
        backend.eva_visible_outputs = Some(vec![2]);
        jwm.eva_tick(&mut backend);
        assert_eq!(jwm.eva.presentation_monitor, Some(second));
        assert_eq!(jwm.eva.activation_monitor, Some(first));
        assert_eq!(
            jwm.eva_execution_context()["activation_monitor_token"],
            frozen_scope,
            "presentation migration must retain an invalid original scope token"
        );
        let refused = jwm.eva_guarded_execute(&mut backend, &old_request);
        assert_eq!(refused.data.as_ref().unwrap()["status"], "failed");
        assert!(
            refused.data.as_ref().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("target_gone")
        );
        assert_eq!(jwm.state.monitors[second].get_active_tags(), 1);
        let req = request(
            &jwm,
            "dark-output",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(second),"tag_mask":2}}),
        );
        backend.eva_visible_outputs = Some(Vec::new());
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert!(!jwm.eva_active());
        assert_eq!(jwm.state.monitors[second].get_active_tags(), 1);
        assert!(
            !jwm.eva_control(&mut backend, &json!({"action":"activate"}))
                .success
        );
    }
    #[test]
    fn internal_writes_and_extra_fields_are_not_guarded_actions() {
        let (mut jwm, mut backend) = setup();
        for action in [
            json!({"name":"set_media_status","arguments":{"status":"playing"}}),
            json!({"name":"focus_monitor","arguments":{"monitor_token":jwm.eva_monitor_token(jwm.state.sel_mon.unwrap()),"shell":"evil"}}),
        ] {
            let req = request(&jwm, "invalid", action);
            assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
            assert!(!jwm.eva.operations.contains_key("invalid"));
        }
    }
    #[test]
    fn directed_window_move_keeps_identity_when_focus_changes() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let mut a = WMClient::new(WindowId::from_raw(900));
        a.mon = Some(m);
        a.state.tags = 1;
        let mut b = a.clone();
        b.win = WindowId::from_raw(901);
        let ka = jwm.insert_client(a);
        let kb = jwm.insert_client(b);
        jwm.attach_to_monitor(ka, m);
        jwm.attach_to_monitor(kb, m);
        let req = request(
            &jwm,
            "target",
            json!({"name":"move_window_to_tag","arguments":{"window_token":jwm.eva_window_token(ka),"tag_mask":2}}),
        );
        jwm.state.monitors[m].sel = Some(kb);
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(result.success, "{result:?}");
        assert_eq!(result.data.unwrap()["status"], "succeeded");
        assert_eq!(jwm.state.clients[ka].state.tags, 2);
        assert_eq!(jwm.state.clients[kb].state.tags, 1);
    }
    #[test]
    fn explicit_focus_condition_rejects_changed_selection() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let mut a = WMClient::new(WindowId::from_raw(910));
        a.mon = Some(m);
        a.state.tags = 1;
        let ka = jwm.insert_client(a);
        jwm.attach_to_monitor(ka, m);
        let mut req = request(
            &jwm,
            "focus-guard",
            json!({"name":"move_window_to_tag","arguments":{"window_token":jwm.eva_window_token(ka),"tag_mask":2}}),
        );
        req["expected_context"]["focus_token"] = json!(jwm.eva_window_token(ka));
        let mut payload = req.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        req["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        jwm.state.monitors[m].sel = None;
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert_eq!(jwm.state.clients[ka].state.tags, 1);
    }
    #[test]
    fn losing_overlay_revokes_before_late_action() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let req = request(
            &jwm,
            "no-overlay",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(m),"tag_mask":2}}),
        );
        backend.eva_overlay_supported = false;
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
        assert!(jwm.eva.permit.is_none());
    }
    #[test]
    fn model_confirmation_never_authorizes_and_local_confirmation_is_single_use() {
        let (mut jwm, mut backend) = setup();
        let action = json!({"name":"clear_clipboard","arguments":{}});
        let status = json!({"assistant_permit":jwm.eva.permit,"state":"confirming","confirmation":{"plan_hash":"hash-one","targets":[action]},"confirmed":true});
        assert!(jwm.eva_set_status(&mut backend, &status).success);
        let mut req = request(&jwm, "requires-local", action.clone());
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert!(jwm.eva_confirm());
        let token = jwm.eva.confirmation.as_ref().unwrap().token.clone();
        req["confirmation_token"] = json!(token);
        let mut payload = req.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        req["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        assert!(jwm.eva_guarded_execute(&mut backend, &req).success);
        let mut second = req.clone();
        second["operation_id"] = json!("second");
        let mut payload = second.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        second["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        assert!(!jwm.eva_guarded_execute(&mut backend, &second).success);
        let mut status2 = status;
        status2["confirmation"]["plan_hash"] = json!("hash-two");
        assert!(jwm.eva_set_status(&mut backend, &status2).success);
        assert_ne!(token, jwm.eva.confirmation.as_ref().unwrap().token);
        assert!(!jwm.eva.confirmation.as_ref().unwrap().approved);
    }
    #[test]
    fn confirmation_keeps_complete_large_values_and_requires_reviewed_detail_pages() {
        let (mut jwm, mut backend) = setup();
        let value = "\\\"配置".repeat(1000);
        // Eight legal 4000-character values exceed the former 8 KiB status
        // limit and 32 KiB guarded request limit after JSON escaping.
        let changes = (0..8)
            .map(|index| json!({"key":format!("review-key-{index}"),"value":value}))
            .collect::<Vec<_>>();
        let action = json!({"name":"set_config_batch","arguments":{"changes":changes}});
        let status = json!({"assistant_permit":jwm.eva.permit,"state":"confirming","confirmation":{"plan_hash":"large-plan","targets":[action],"timeout_ms":120000}});
        assert!(status.to_string().len() > 32768);
        let result = jwm.eva_set_status(&mut backend, &status);
        assert!(result.success, "{result:?}");
        let snapshot = result.data.unwrap();
        assert!(snapshot["confirmation"].get("targets").is_none());
        assert_eq!(snapshot["confirmation"]["total_actions"], 1);
        let critical = snapshot["confirmation"]["display_items"][0]["critical"]
            .as_str()
            .unwrap();
        for change in &changes {
            assert!(
                critical
                    .contains(&serde_json::to_string(change["value"].as_str().unwrap()).unwrap())
            );
            assert!(critical.contains(change["key"].as_str().unwrap()));
        }
        assert!(snapshot.to_string().len() < MAX_REQUEST_BYTES);
        assert!(!jwm.eva_confirm());
        assert!(
            jwm.eva_active(),
            "initial detail layout must not block the main loop for the entire permit lease"
        );
        let pages = jwm.eva.confirmation_ui.confirmation_page_count_cached();
        assert!(pages > 1);
        // Real review is accompanied by daemon heartbeats. Refresh between
        // distinct local input events, without allowing a slow single layout
        // call to hide behind a longer authority lease.
        assert!(jwm.eva_set_status(&mut backend, &status).success);
        jwm.eva.confirmation_detail_page = Some(pages - 1);
        jwm.eva.confirmation_seen_pages = (0..pages).collect();
        assert!(jwm.eva_confirm());
        let expiry = jwm.eva.confirmation.as_ref().unwrap().expires;
        let token = jwm.eva.confirmation.as_ref().unwrap().token.clone();
        let mut same = status.clone();
        same["confirmation"]["timeout_ms"] = json!(600000);
        assert!(jwm.eva_set_status(&mut backend, &same).success);
        assert_eq!(jwm.eva.confirmation.as_ref().unwrap().expires, expiry);
        let mut req = request(&jwm, "large-guarded", action);
        // Missing confirmation must be the reason to reject, not request size.
        let denied = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(
            denied
                .error
                .as_deref()
                .unwrap_or("")
                .contains("confirmation"),
            "{denied:?}"
        );
        req["action"]["arguments"]["changes"][0]["value"] = json!("changed");
        same["confirmation"]["targets"][0] = req["action"].clone();
        assert!(jwm.eva_set_status(&mut backend, &same).success);
        assert_ne!(jwm.eva.confirmation.as_ref().unwrap().token, token);
        assert!(jwm.eva.confirmation_seen_pages.is_empty());
        assert!(jwm.eva.confirmation_detail_page.is_none());
        let mut invalid = same.clone();
        invalid["confirmation"]["timeout_ms"] = json!(600001);
        assert!(!jwm.eva_set_status(&mut backend, &invalid).success);
        let expired_at = Instant::now() - Duration::from_secs(1);
        let token = jwm.eva.confirmation.as_ref().unwrap().token.clone();
        jwm.eva.confirmation.as_mut().unwrap().expires = expired_at;
        jwm.eva_tick(&mut backend);
        assert!(jwm.eva.confirmation.as_ref().unwrap().expiry_reported);
        assert!(jwm.eva_overlay_snapshot()["confirmation"].is_null());
        assert!(jwm.eva_set_status(&mut backend, &same).success);
        assert_eq!(
            jwm.eva.confirmation.as_ref().unwrap().expires,
            expired_at,
            "an expired same-scope heartbeat cannot restart the TTL"
        );
        assert_eq!(jwm.eva.confirmation.as_ref().unwrap().token, token);
        assert!(!jwm.eva_confirm());
        assert_eq!(jwm.eva_overlay_snapshot()["state"], "error");
    }
    #[test]
    fn confirmation_review_pages_are_bound_to_layout_but_not_late_captions() {
        struct RestoreConfig(std::sync::Arc<crate::config::Config>);
        impl Drop for RestoreConfig {
            fn drop(&mut self) {
                CONFIG.store(self.0.clone());
            }
        }
        let _restore = RestoreConfig(CONFIG.load_full());
        let (mut jwm, mut backend) = setup();
        let monitor = jwm.state.sel_mon.unwrap();
        jwm.state.monitors[monitor].geometry.w_h = 240;
        let status = json!({"assistant_permit":jwm.eva.permit,"state":"confirming",
            "confirmation":{"plan_hash":"layout-bound-review","timeout_ms":120000,
            "targets":[{"name":"set_config","arguments":{"key":"review-key","value":"abcde".repeat(400)}}]}});
        assert!(jwm.eva_set_status(&mut backend, &status).success);
        let expires = jwm.eva.confirmation.as_ref().unwrap().expires;
        let signature = jwm.eva.confirmation_layout_signature;
        assert!(signature.is_some());
        let pages = jwm.eva.confirmation_ui.confirmation_page_count_cached();
        assert!(pages > 1);
        jwm.eva.confirmation_seen_pages = (0..pages).collect();
        jwm.eva.confirmation_detail_page = Some(pages - 1);
        let mut caption = status.clone();
        caption["input_caption"] = json!({"item_id":"asr","turn_epoch":1,"text":"迟到识别"});
        assert!(jwm.eva_set_status(&mut backend, &caption).success);
        assert_eq!(jwm.eva.confirmation_layout_signature, signature);
        assert_eq!(
            jwm.eva.confirmation_seen_pages.len(),
            pages,
            "caption is outside the review layout signature"
        );
        jwm.state.monitors[monitor].geometry.w_w = 1000;
        jwm.state.monitors[monitor].geometry.w_h = 600;
        jwm.eva_refresh_confirmation_layout();
        assert_ne!(jwm.eva.confirmation_layout_signature, signature);
        assert!(jwm.eva.confirmation_seen_pages.is_empty());
        assert!(jwm.eva.confirmation_detail_page.is_none());
        assert!(!jwm.eva_confirm());
        let repaged = jwm.eva.confirmation_ui.confirmation_page_count_cached();
        assert!(
            repaged < pages,
            "this resize must reproduce old seen-indices falsely covering fewer new pages"
        );
        let pages = repaged;
        jwm.eva.confirmation_seen_pages = (0..pages).collect();
        jwm.eva.confirmation_detail_page = Some(0);
        // Fonts change through configuration reload, rather than the scalar
        // runtime setter; exercise that real path without touching user files.
        struct FontFixture(std::path::PathBuf);
        impl Drop for FontFixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = std::env::temp_dir().join(format!(
            "jwm-eva-font-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _fixture = FontFixture(dir.clone());
        let path = dir.join("font.toml");
        CONFIG.load().save_to_file(&path).unwrap();
        let mut tree: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        tree["appearance"]["system_ui_font"] = toml::Value::String("Sans 18".into());
        std::fs::write(&path, toml::to_string(&tree).unwrap()).unwrap();
        CONFIG.store(std::sync::Arc::new(
            crate::config::Config::load_from_file(&path).unwrap(),
        ));
        jwm.eva_refresh_confirmation_layout();
        assert!(
            jwm.eva.confirmation_seen_pages.is_empty(),
            "font changes invalidate previous page visits"
        );
        assert!(!jwm.eva_confirm());
        assert_eq!(
            jwm.eva.confirmation.as_ref().unwrap().expires,
            expires,
            "relayout never renews the confirmation TTL"
        );
    }
    #[test]
    fn confirmation_freezes_all_local_target_rows_and_hides_them_after_review() {
        let (mut jwm, mut backend) = setup();
        let mon = jwm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(1717));
        client.mon = Some(mon);
        client.name = "Frozen local title".into();
        let window = jwm.insert_client(client);
        let action = json!({"name":"move_window_to_tag","arguments":{"window_token":jwm.eva_window_token(window),"tag_mask":4}});
        let status = json!({"assistant_permit":jwm.eva.permit,"state":"confirming","confirmation":{"plan_hash":"eight-targets","targets":vec![action;8]}});
        let result = jwm.eva_set_status(&mut backend, &status);
        assert!(result.success, "{result:?}");
        let initial = jwm.eva_overlay_snapshot()["confirmation"].clone();
        assert_eq!(initial["display_items"].as_array().unwrap().len(), 8);
        let first = initial["display_items"][0]["critical"].as_str().unwrap();
        assert!(first.contains(&format!("w{:x}", window.data().as_ffi())));
        assert!(first.contains("\"tag_numbers\":[3]"));
        jwm.state.clients[window].name = "Renamed later".into();
        assert_eq!(
            jwm.eva_overlay_snapshot()["confirmation"]["display_items"],
            initial["display_items"]
        );
        let update = json!({"assistant_permit":jwm.eva.permit,"state":"executing"});
        assert!(jwm.eva_set_status(&mut backend, &update).success);
        assert!(jwm.eva_overlay_snapshot()["confirmation"].is_null());
        assert!(jwm.eva.confirmation.is_some());
        assert!(!jwm.eva_confirm());
        jwm.eva_cancel("test");
        assert!(jwm.eva.confirmation_seen_pages.is_empty());
    }
    #[test]
    fn asynchronous_control_uses_its_sequence_not_optimistic_or_stale_events() {
        let (mut jwm, _backend) = setup();
        let req = request(
            &jwm,
            "mic",
            json!({"name":"set_mic_mute","arguments":{"muted":true}}),
        );
        jwm.eva.operations.insert(
            "mic".into(),
            Operation {
                digest: req["request_digest"].as_str().unwrap().into(),
                request: req,
                result: json!({"status":"accepted","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: Some(("mic".into(), 7)),
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        jwm.eva_observe_event("audio/mic", &json!({"muted":true}));
        assert_eq!(jwm.eva.operations["mic"].result["status"], "accepted");
        jwm.eva_control_completion("mic", 6, Some(true), json!({"muted":true}));
        assert_eq!(jwm.eva.operations["mic"].result["status"], "accepted");
        jwm.eva_control_completion("mic", 7, Some(false), json!({"muted":false}));
        assert_eq!(jwm.eva.operations["mic"].result["status"], "failed");
    }
    #[test]
    fn media_launch_requires_correlated_native_mpv_receipts_not_spawn_or_old_poll() {
        let (mut jwm, _) = setup();
        let uri = "file:///tmp/requested%20media.ogg";
        let action = json!({"name":"open_media","arguments":{"path":"/tmp/requested media.ogg","mode":"auto","enqueue":false}});
        let req = request(&jwm, "media", action.clone());
        jwm.eva.operations.insert(
            "media".into(),
            Operation {
                digest: "media-digest".into(),
                request: req,
                result: json!({"status":"accepted","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: Some(("media_launch".into(), 42)),
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        let metadata = jwm.eva_media_launch_prepared(42, uri).unwrap().unwrap();
        assert!(jwm.eva.operations["media"].result["verified"]["media_before_count"].is_null());
        jwm.eva_control_completion(
            "media_launch",
            42,
            Some(true),
            json!({"pid":123,"completion":"player_process_spawned","playback_verified":false}),
        );
        let observed = json!({"player":"jwm","bus_owner":":1.77","process_id":999,"track_id":"/org/jwm/Player/track_7","track_url":uri,"status":"Playing","queued_urls":[uri,uri]});
        jwm.eva_media_observed(&observed);
        assert_eq!(
            jwm.eva.operations["media"].result["status"], "accepted",
            "even an exact resource already Playing in an old poll is not this operation's receipt"
        );
        let mut completion = metadata;
        for (key,value) in json!({"player":"jwm","bus_owner":":1.77","process_id":999,"player_request_id":1,"entry_id":7,
            "verified":true,"mpv_command_ack":true,"status":"Playing","track_url":uri}).as_object().unwrap(){completion[key]=value.clone();}
        let mut wrong = completion.clone();
        wrong["wm_instance_id"] = json!("old-wm");
        jwm.eva_media_open_completed(&wrong);
        wrong = completion.clone();
        wrong["sequence"] = json!(43);
        jwm.eva_media_open_completed(&wrong);
        assert_eq!(jwm.eva.operations["media"].result["status"], "accepted");
        wrong = completion.clone();
        wrong["mpv_command_ack"] = json!(false);
        jwm.eva_media_open_completed(&wrong);
        assert_eq!(jwm.eva.operations["media"].result["status"], "unknown");
        jwm.eva_media_open_completed(&completion);
        assert_eq!(jwm.eva.operations["media"].result["status"], "succeeded");
        assert_eq!(
            jwm.eva.operations["media"].result["verified"]["playback_verified"],
            true
        );
        let mut enqueue = action;
        enqueue["arguments"]["enqueue"] = json!(true);
        let req = request(&jwm, "enqueue", enqueue);
        jwm.eva.operations.insert(
            "enqueue".into(),
            Operation {
                digest: "enqueue-digest".into(),
                request: req,
                result: json!({"status":"accepted","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: Some(("media_launch".into(), 43)),
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        let metadata = jwm.eva_media_launch_prepared(43, uri).unwrap().unwrap();
        assert_eq!(metadata["target_bus_owner"], ":1.77");
        let mut later_poll = observed;
        later_poll["queued_urls"] = json!([uri, uri, uri]);
        jwm.eva_media_observed(&later_poll);
        jwm.eva_media_open_completed(&completion);
        assert_eq!(
            jwm.eva.operations["enqueue"].result["status"], "accepted",
            "queue growth or a previous operation's receipt cannot settle this enqueue"
        );
        completion = metadata;
        for (key,value) in json!({"player":"jwm","bus_owner":":1.77","process_id":999,"player_request_id":2,"entry_id":8,
            "verified":true,"mpv_command_ack":true,"status":"Paused","track_url":"file:///tmp/other.ogg"}).as_object().unwrap(){completion[key]=value.clone();}
        wrong = completion.clone();
        wrong["bus_owner"] = json!(":1.88");
        jwm.eva_media_open_completed(&wrong);
        assert_eq!(jwm.eva.operations["enqueue"].result["status"], "accepted");
        wrong = completion.clone();
        wrong["entry_id"] = json!(0);
        jwm.eva_media_open_completed(&wrong);
        assert_eq!(jwm.eva.operations["enqueue"].result["status"], "unknown");
        jwm.eva_media_open_completed(&completion);
        assert_eq!(jwm.eva.operations["enqueue"].result["status"], "succeeded");
        assert_eq!(
            jwm.eva.operations["enqueue"].result["verified"]["completion"],
            "resource_queued_observed"
        );
        assert_eq!(
            jwm.eva.operations["enqueue"].result["verified"]["playback_verified"],
            false
        );
        let req = request(
            &jwm,
            "cancelled-preparation",
            jwm.eva.operations["enqueue"].request["action"].clone(),
        );
        jwm.eva.operations.insert(
            "cancelled-preparation".into(),
            Operation {
                digest: "cancelled-preparation".into(),
                request: req,
                result: json!({"status":"accepted","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: Some(("media_launch".into(), 44)),
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        jwm.eva_cancel("test");
        assert_eq!(
            jwm.eva_media_launch_prepared(44, uri).unwrap_err(),
            "assistant_permit_expired_before_player_spawn"
        );
    }
    #[test]
    fn unix_socket_cancel_from_another_process_revokes_old_request() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        let (mut jwm, mut backend) = setup();
        let dir = std::env::temp_dir().join(format!(
            "jwm-eva-protocol-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("ipc.sock");
        jwm.ipc_server =
            Some(crate::ipc_server::IpcServer::new_at_test_path(socket.clone()).unwrap());
        let script = r#"
import socket,json,hashlib,sys
path=sys.argv[1]
def call(value):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(2);s.connect(path);s.sendall((json.dumps(value,separators=(',',':'))+'\n').encode());data=b''
        while not data.endswith(b'\n'): data+=s.recv(65536)
        return json.loads(data)
assert call({'command':'assistant_control','args':{'action':'activate'}})['success']
ctx=call({'query':'get_execution_context'})['data']
assert ctx['assistant_permit'] is not None,ctx
request={'protocol_version':1,'wm_instance_id':ctx['wm_instance_id'],'assistant_permit':ctx['assistant_permit'],'operation_id':'cross-process','expected_context':{'session_unlocked':True},'action':{'name':'switch_tag','arguments':{'monitor_token':ctx['selected_monitor_token'],'tag_mask':2}}}
request['request_digest']=hashlib.sha256(json.dumps(request,sort_keys=True,separators=(',',':')).encode()).hexdigest()
assert call({'command':'assistant_control','args':{'action':'cancel'}})['success']
answer=call({'command':'guarded_execute','args':request})
assert not answer['success'],answer
assert answer['error']=='assistant_permit_expired',answer
"#;
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&socket)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            jwm.process_ipc(&mut backend);
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("cross-process IPC test timed out")
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            jwm.state.monitors[jwm.state.sel_mon.unwrap()].get_active_tags(),
            1
        );
        assert!(!jwm.eva_active());
        drop(jwm);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn directed_workspace_change_preserves_selected_monitor() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        backend.eva_overlay_supported = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        assert!(
            jwm.eva_control(&mut backend, &json!({"action":"activate"}))
                .success
        );
        let selected = jwm.state.sel_mon;
        let other = jwm
            .state
            .monitor_order
            .iter()
            .copied()
            .find(|m| Some(*m) != selected)
            .unwrap();
        let req = request(
            &jwm,
            "other-monitor",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(other),"tag_mask":4}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "succeeded"
        );
        assert_eq!(jwm.state.sel_mon, selected);
        assert_eq!(jwm.state.monitors[other].get_active_tags(), 4);
    }
    #[test]
    fn undo_poststate_check_is_atomic_and_conflicts_do_not_modify_target() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let token = jwm.eva_monitor_token(m);
        let req = request(
            &jwm,
            "undo",
            json!({"name":"switch_tag","arguments":{"monitor_token":token,"tag_mask":2,"expected_state":{"monitor_token":token,"visible_tag_mask":4}}}),
        );
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(!result.success);
        assert!(result.error.unwrap().starts_with("undo_conflict"));
        assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
        for (name, expected) in [
            ("wrong-focus", json!({"focus_token":"removed-focus"})),
            (
                "wrong-monitor",
                json!({"selected_monitor_token":"removed-monitor"}),
            ),
        ] {
            let req = request(
                &jwm,
                name,
                json!({"name":"switch_tag","arguments":{"monitor_token":token,"tag_mask":2,"expected_state":expected}}),
            );
            let result = jwm.eva_guarded_execute(&mut backend, &req);
            assert!(!result.success, "{result:?}");
            assert!(result.error.unwrap().starts_with("undo_conflict"));
            assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
        }
        let req = request(
            &jwm,
            "correct-focus-monitor",
            json!({"name":"switch_tag","arguments":{"monitor_token":token,"tag_mask":2,"expected_state":{"focus_token":null,"selected_monitor_token":token}}}),
        );
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(result.success, "{result:?}");
        assert_eq!(result.data.unwrap()["status"], "succeeded");
    }
    #[test]
    fn monitor_move_compensation_restores_tags_and_rejects_bad_masks_before_moving() {
        let mut backend = DisplaySpyBackend::new(vec![
            output(1, 0, 0, 1920, 1080),
            output(2, 1920, 0, 1920, 1080),
        ]);
        backend.eva_overlay_supported = true;
        let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
        assert!(
            jwm.eva_control(&mut backend, &json!({"action":"activate"}))
                .success
        );
        let first = jwm.state.monitor_order[0];
        let second = jwm.state.monitor_order[1];
        let mut client = WMClient::new(WindowId::from_raw(1818));
        client.mon = Some(second);
        client.state.tags = 1;
        let window = jwm.insert_client(client);
        jwm.attach_to_monitor(window, second);
        let action = |mask| json!({"name":"move_window_to_monitor","arguments":{"window_token":jwm.eva_window_token(window),"monitor_token":jwm.eva_monitor_token(first),"restore_tag_mask":mask}});
        let invalid = request(&jwm, "invalid-monitor-undo-tags", action(1u32 << 31));
        let valid = request(&jwm, "monitor-undo-tags", action(4));
        let result = jwm.eva_guarded_execute(&mut backend, &invalid);
        assert_eq!(result.data.unwrap()["status"], "failed");
        assert_eq!(jwm.state.clients[window].mon, Some(second));
        assert_eq!(jwm.state.clients[window].state.tags, 1);
        let result = jwm.eva_guarded_execute(&mut backend, &valid);
        assert_eq!(result.data.unwrap()["status"], "succeeded");
        assert_eq!(jwm.state.clients[window].mon, Some(first));
        assert_eq!(jwm.state.clients[window].state.tags, 4);
    }
    #[test]
    fn radio_readback_completes_even_when_the_optimistic_cache_already_matches() {
        use crate::jwm::features::connectivity::{
            BackgroundJob, BluetoothState, ConnectivityState,
        };
        let (mut jwm, _) = setup();
        let state = ConnectivityState {
            network: None,
            bluetooth: BluetoothState {
                present: true,
                powered: true,
            },
        };
        let job = BackgroundJob::completed_for_test(state.clone());
        let req = request(
            &jwm,
            "radio",
            json!({"name":"toggle_bluetooth","arguments":{"enabled":true}}),
        );
        jwm.eva.operations.insert(
            "radio".into(),
            Operation {
                digest: req["request_digest"].as_str().unwrap().into(),
                request: req,
                result: json!({"status":"in_flight","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: None,
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        jwm.eva_note_radio_sequence(job.eva_identity(), job.eva_keepalive());
        jwm.eva.operations.get_mut("radio").unwrap().result["status"] = json!("accepted");
        jwm.features.connectivity = state;
        jwm.features.connectivity_poll = Some(job);
        jwm.poll_connectivity_job();
        assert_eq!(jwm.eva.operations["radio"].result["status"], "succeeded");
    }
    #[test]
    fn enabled_global_state_is_idempotent_instead_of_blindly_toggling() {
        let (mut jwm, mut backend) = setup();
        for (id, enabled) in [
            ("on-one", true),
            ("on-again", true),
            ("off", false),
            ("off-again", false),
        ] {
            let req = request(
                &jwm,
                id,
                json!({"name":"toggle_dnd","arguments":{"enabled":enabled}}),
            );
            assert_eq!(
                jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
                "succeeded"
            );
            assert_eq!(jwm.do_not_disturb, enabled);
        }
        jwm.features
            .media
            .update(crate::jwm::features::media::parse_state_args(
                &json!({"player":"jwm","status":"Paused"}),
            ));
        jwm.eva_media_observed(
            &json!({"player":"jwm","status":"Paused","bus_owner":":1.7","process_id":777}),
        );
        let instance = jwm.eva.instance.clone();
        for id in ["pause-already", "pause-again"] {
            let req = request(
                &jwm,
                id,
                json!({"name":"media_control","arguments":{"action":"pause","position_us":0}}),
            );
            assert_eq!(
                jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
                "accepted"
            );
            assert_eq!(jwm.features.media.get().unwrap().status.as_str(), "paused");
            jwm.eva_observe_event("media/status", &json!({"player":"jwm","status":"paused"}));
            assert_eq!(
                jwm.eva.operations[id].result["status"], "accepted",
                "an in-flight ordinary poll must not settle the setter"
            );
            jwm.eva_media_command_completed(&json!({"wm_instance_id":instance,"operation_id":id,"action":"pause","process_id":777,"player":"jwm","bus_owner":":1.7","status":"Paused","verified":true}));
            assert_eq!(jwm.eva.operations[id].result["status"], "succeeded");
        }
        let req = request(
            &jwm,
            "explicit-play",
            json!({"name":"media_control","arguments":{"action":"play","position_us":0}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "accepted"
        );
        jwm.eva_observe_event(
            "media/status",
            &json!({"player":"other-player","status":"playing"}),
        );
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "accepted"
        );
        jwm.eva_observe_event("media/status", &json!({"player":"jwm","status":"paused"}));
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "accepted"
        );
        jwm.eva_observe_event("media/status", &json!({"player":"jwm","status":"playing"}));
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "accepted"
        );
        jwm.eva_media_command_completed(&json!({"wm_instance_id":"previous-wm","operation_id":"explicit-play","action":"play","process_id":777,"player":"jwm","bus_owner":":1.7","status":"Playing","verified":true}));
        jwm.eva_media_command_completed(&json!({"wm_instance_id":instance,"operation_id":"explicit-play","action":"play","process_id":777,"player":"jwm","bus_owner":":1.8","status":"Playing","verified":true}));
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "accepted"
        );
        jwm.eva_media_command_completed(&json!({"wm_instance_id":instance,"operation_id":"explicit-play","action":"play","process_id":777,"player":"jwm","bus_owner":":1.7","verified":false,"error":"unknown"}));
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "unknown"
        );
        jwm.eva_media_command_completed(&json!({"wm_instance_id":instance,"operation_id":"explicit-play","action":"play","process_id":777,"player":"jwm","bus_owner":":1.7","status":"Playing","verified":true}));
        assert_eq!(
            jwm.eva.operations["explicit-play"].result["status"],
            "succeeded"
        );
    }
    #[test]
    fn every_media_transport_freezes_owner_and_requires_operation_specific_readback() {
        let (mut jwm, mut backend) = setup();
        jwm.features
            .media
            .update(crate::jwm::features::media::parse_state_args(
                &json!({"player":"p","status":"Playing"}),
            ));
        jwm.eva_media_observed(&json!({"player":"p","bus_owner":":1.7","process_id":777,
            "track_id":"/track/old","status":"Playing"}));
        let instance = jwm.eva.instance.clone();
        for (id, name, args, command, readback) in [
            (
                "stop",
                "media_stop",
                json!({}),
                "stop",
                json!({"status":"Stopped"}),
            ),
            (
                "next",
                "media_next",
                json!({}),
                "next",
                json!({"before_track_id":"/track/old","track_id":"/track/new"}),
            ),
            (
                "previous",
                "media_previous",
                json!({}),
                "previous",
                json!({"before_track_id":"/track/old","track_id":"/track/earlier"}),
            ),
            (
                "toggle",
                "media_play_pause",
                json!({}),
                "play_pause",
                json!({"before_status":"Playing","status":"Paused"}),
            ),
            (
                "seek",
                "media_control",
                json!({"action":"seek","position_us":42_000_000}),
                "seek",
                json!({"track_id":"/track/old","position_us":42_000_000}),
            ),
        ] {
            let req = request(&jwm, id, json!({"name":name,"arguments":args}));
            let receipt = jwm.eva_guarded_execute(&mut backend, &req);
            assert_eq!(receipt.data.unwrap()["status"], "accepted", "{id}");
            jwm.eva_observe_event(
                "media/status",
                &json!({"player":"p","status":"stopped","title":"new","position_us":42_000_000}),
            );
            assert_eq!(jwm.eva.operations[id].result["status"], "accepted");
            let mut completion = json!({"wm_instance_id":instance,"operation_id":id,"action":command,
                "player":"p","bus_owner":":1.7","process_id":777,"verified":true});
            for (k, v) in readback.as_object().unwrap() {
                completion[k] = v.clone();
            }
            let mut wrong = completion.clone();
            wrong["process_id"] = json!(778);
            jwm.eva_media_command_completed(&wrong);
            assert_eq!(jwm.eva.operations[id].result["status"], "accepted");
            if command == "seek" {
                wrong = completion.clone();
                wrong["position_us"] = json!(42); // microseconds, not seconds
                jwm.eva_media_command_completed(&wrong);
                assert_eq!(jwm.eva.operations[id].result["status"], "unknown");
                wrong = completion.clone();
                wrong["track_id"] = json!("/track/other");
                jwm.eva_media_command_completed(&wrong);
                assert_eq!(jwm.eva.operations[id].result["status"], "unknown");
            }
            jwm.eva_media_command_completed(&completion);
            assert_eq!(jwm.eva.operations[id].result["status"], "succeeded", "{id}");
        }
    }
    #[test]
    fn clipboard_completion_requires_the_distinct_native_ownership_receipt() {
        let (mut jwm, mut backend) = setup();
        jwm.record_clipboard("private history text");
        for id in ["first", "second", "failure"] {
            let req = request(
                &jwm,
                id,
                json!({"name":"clipboard_copy","arguments":{"index":0}}),
            );
            let receipt = jwm.eva_guarded_execute(&mut backend, &req);
            assert_eq!(receipt.data.unwrap()["status"], "accepted");
        }
        assert_eq!(backend.clipboard_receipts.len(), 3);
        backend.clipboard_receipts[0].complete(true);
        backend.clipboard_receipts[2].complete(false);
        jwm.eva_tick(&mut backend);
        assert_eq!(jwm.eva.operations["first"].result["status"], "succeeded");
        assert_eq!(
            jwm.eva.operations["second"].result["status"], "accepted",
            "old ownership acknowledgement cannot settle a new offer"
        );
        assert_eq!(jwm.eva.operations["failure"].result["status"], "failed");
        assert!(
            !jwm.eva.operations["first"]
                .result
                .to_string()
                .contains("private history text")
        );
        backend.clipboard_receipts[1].complete(true);
        jwm.eva_tick(&mut backend);
        assert_eq!(jwm.eva.operations["second"].result["status"], "succeeded");
        assert_eq!(
            backend.clipboard_receipts.len(),
            3,
            "polling never resubmits an offer"
        );
        let req = request(
            &jwm,
            "stalled",
            json!({"name":"clipboard_copy","arguments":{"index":0}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "accepted"
        );
        jwm.eva.operations.get_mut("stalled").unwrap().expires =
            Instant::now() - Duration::from_secs(1);
        jwm.eva_tick(&mut backend);
        assert_eq!(jwm.eva.operations["stalled"].result["status"], "unknown");
        jwm.eva_cancel("test");
        assert_eq!(
            jwm.eva_next_wakeup(Instant::now()),
            None,
            "an unresponsive clipboard worker must not keep Idle polling forever"
        );
        backend.clipboard_receipts[3].complete(true);
        let late = jwm.eva_execution_status(
            &mut backend,
            &json!({"wm_instance_id":jwm.eva.instance,"operation_id":"stalled"}),
        );
        assert_eq!(
            late.data.unwrap()["status"],
            "succeeded",
            "a read-only historical query still settles late ownership after cancel"
        );
        assert_eq!(backend.clipboard_receipts.len(), 4);
    }
    #[test]
    fn session_lock_receipt_settles_after_permit_revocation_and_unlock_requires_local_auth() {
        let (mut jwm, mut backend) = setup();
        let req = request(&jwm, "lock", json!({"name":"lock_screen","arguments":{}}));
        jwm.eva.operations.insert(
            "lock".into(),
            Operation {
                digest: req["request_digest"].as_str().unwrap().into(),
                request: req,
                result: json!({"status":"accepted","verified":{}}),
                expires: Instant::now() + RETENTION,
                terminal: false,
                control_sequence: None,
                background_keepalive: None,
                clipboard_receipt: None,
            },
        );
        jwm.features.system_ui = crate::jwm::features::SystemUiState::lock();
        jwm.eva_tick(&mut backend);
        assert!(!jwm.eva_active());
        assert_eq!(jwm.eva.operations["lock"].result["status"], "succeeded");
        let schema = jwm.eva_command_schema();
        let unlock = schema["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["canonical_name"] == "unlock_monitor")
            .unwrap();
        assert_eq!(unlock["support_status"], "policy_requires_local_auth");
    }
    #[test]
    fn caption_and_undo_are_bounded_ui_data_and_do_not_grant_execution_authority() {
        let (mut jwm, mut backend) = setup();
        let response = jwm.eva_set_status(
            &mut backend,
            &json!({"assistant_permit":jwm.eva.permit,
            "state":"listening","undo_available":true,
            "input_caption":{"item_id":"item-1","turn_epoch":1,"text":"切到第三个tag\u{202e}\n"}}),
        );
        assert!(response.success);
        let snapshot = jwm.eva_overlay_snapshot();
        assert_eq!(snapshot["undo_available"], true);
        assert_eq!(snapshot["input_caption"]["text"], "切到第三个tag");
        assert!(jwm.eva.confirmation.is_none());
        assert!(
            jwm.eva_set_status(
                &mut backend,
                &json!({"assistant_permit":jwm.eva.permit,"state":"planning"})
            )
            .success
        );
        assert!(jwm.eva_overlay_snapshot()["input_caption"].is_null());
        assert_eq!(jwm.eva_overlay_snapshot()["undo_available"], false);
    }
    #[test]
    fn setting_the_same_recording_region_has_an_exact_normalized_postcondition() {
        let (mut jwm, mut backend) = setup();
        let region = crate::core::types::Rect::new(10, 20, 640, 480);
        jwm.features.recording.active = true;
        jwm.features.recording.region = Some(region);
        let req = request(
            &jwm,
            "same-region",
            json!({"name":"set_recording_region",
            "arguments":{"x":10,"y":20,"width":640,"height":480}}),
        );
        let receipt = jwm.eva_guarded_execute(&mut backend, &req);
        assert_eq!(receipt.data.unwrap()["status"], "succeeded");
        assert_eq!(jwm.features.recording.region, Some(region));
    }
    #[test]
    fn opening_an_already_visible_panel_preserves_it_and_false_closes_only_that_panel() {
        let (mut jwm, mut backend) = setup();
        jwm.features.system_ui =
            crate::jwm::features::SystemUiState::calendar(chrono::Local::now().naive_local());
        let req = request(
            &jwm,
            "calendar-open",
            json!({"name":"calendar","arguments":{"enabled":true}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "succeeded"
        );
        assert!(jwm.features.system_ui.is_calendar());
        let req = request(
            &jwm,
            "unrelated-close",
            json!({"name":"control_center","arguments":{"enabled":false}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "succeeded"
        );
        assert!(jwm.features.system_ui.is_calendar());
        let req = request(
            &jwm,
            "calendar-close",
            json!({"name":"calendar","arguments":{"enabled":false}}),
        );
        assert_eq!(
            jwm.eva_guarded_execute(&mut backend, &req).data.unwrap()["status"],
            "succeeded"
        );
        assert!(!jwm.features.system_ui.is_calendar());
    }
    #[test]
    fn confirmed_restore_uses_frozen_snapshot_and_verifies_actual_poststate() {
        let (mut jwm, mut backend) = setup();
        let mon = jwm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(1200));
        client.class = "RestoreTest".into();
        client.instance = "restore-test".into();
        client.mon = Some(mon);
        client.state.tags = 1;
        client.geometry.w = 400;
        client.geometry.h = 300;
        client.state.client_fact = 1.0;
        let key = jwm.insert_client(client);
        jwm.attach_to_monitor(key, mon);
        let snapshot =
            super::super::session::capture_snapshot(&jwm.state, CONFIG.load().status_bar_name());
        let action = json!({"name":"restore_session","arguments":{}});
        let mut monitors = jwm
            .state
            .monitors
            .keys()
            .map(|m| jwm.eva_monitor_token(m))
            .collect::<Vec<_>>();
        monitors.sort();
        jwm.eva.confirmation = Some(Confirmation {
            expiry_reported: false,
            token: "restore-local".into(),
            plan_hash: "frozen".into(),
            targets: json!([action]),
            expires: Instant::now() + Duration::from_secs(15),
            approved: true,
            used: vec![false],
            restore_snapshot: Some(snapshot),
            restore_windows: vec![jwm.eva_window_token(key)],
            restore_monitors: monitors,
            display_items: Vec::new(),
        });
        jwm.state.clients[key].state.tags = 2;
        jwm.state.clients[key].state.client_fact = 2.;
        let mut req = request(&jwm, "restore", action);
        req["confirmation_token"] = json!("restore-local");
        let mut payload = req.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        req["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(result.success, "{result:?}");
        assert_eq!(result.data.unwrap()["status"], "succeeded");
        assert_eq!(jwm.state.clients[key].state.tags, 1);
        assert_eq!(jwm.state.clients[key].state.client_fact, 1.);
    }
    #[test]
    fn bulk_restore_confirmation_rejects_replacement_window_incarnation() {
        let (mut jwm, mut backend) = setup();
        let mon = jwm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(1300));
        client.class = "RestoreTest".into();
        client.instance = "restore-test".into();
        client.mon = Some(mon);
        client.state.tags = 1;
        let key = jwm.insert_client(client.clone());
        jwm.attach_to_monitor(key, mon);
        let snapshot =
            super::super::session::capture_snapshot(&jwm.state, CONFIG.load().status_bar_name());
        let token = jwm.eva_window_token(key);
        let action = json!({"name":"restore_session","arguments":{}});
        let mut monitors = jwm
            .state
            .monitors
            .keys()
            .map(|m| jwm.eva_monitor_token(m))
            .collect::<Vec<_>>();
        monitors.sort();
        jwm.eva.confirmation = Some(Confirmation {
            expiry_reported: false,
            token: "restore-local".into(),
            plan_hash: "frozen".into(),
            targets: json!([action]),
            expires: Instant::now() + Duration::from_secs(15),
            approved: true,
            used: vec![false],
            restore_snapshot: Some(snapshot),
            restore_windows: vec![token],
            restore_monitors: monitors,
            display_items: Vec::new(),
        });
        jwm.state.clients.remove(key);
        let replacement = jwm.insert_client(client);
        jwm.attach_to_monitor(replacement, mon);
        jwm.state.clients[replacement].state.tags = 2;
        let mut req = request(&jwm, "restore-replaced", action);
        req["confirmation_token"] = json!("restore-local");
        let mut payload = req.clone();
        payload.as_object_mut().unwrap().remove("request_digest");
        req["request_digest"] = json!(jwm_eva_core::protocol::digest(&payload));
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap()
                .starts_with("stale_context: restore scope")
        );
        assert_eq!(jwm.state.clients[replacement].state.tags, 2);
    }
    #[test]
    fn short_references_never_alias_a_replacement_window_between_turns() {
        let (mut jwm, _) = setup();
        let mon = jwm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(1400));
        client.mon = Some(mon);
        client.state.tags = 1;
        let old = jwm.insert_client(client.clone());
        jwm.attach_to_monitor(old, mon);
        let before = jwm.eva_execution_context();
        let old_ref = before["windows"][0]["window_ref"]
            .as_str()
            .unwrap()
            .to_string();
        let mon_ref = before["monitors"][0]["monitor_ref"].clone();
        jwm.state.clients.remove(old);
        let replacement = jwm.insert_client(client);
        jwm.attach_to_monitor(replacement, mon);
        let after = jwm.eva_execution_context();
        let new_ref = after["windows"][0]["window_ref"].as_str().unwrap();
        assert_ne!(old_ref, new_ref);
        assert!(
            after["windows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|w| w["window_ref"] != old_ref)
        );
        assert_eq!(after["monitors"][0]["monitor_ref"], mon_ref);
        assert!(new_ref.len() <= 32);
    }
    #[test]
    fn cancelled_permit_and_old_heartbeat_cannot_reactivate() {
        let (mut jwm, mut backend) = setup();
        let permit = jwm.eva.permit.clone();
        let m = jwm.state.sel_mon.unwrap();
        let old = jwm.state.monitors[m].get_active_tags();
        let req = request(
            &jwm,
            "cancel",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(m),"tag_mask":2}}),
        );
        jwm.eva_cancel("test");
        assert!(
            !jwm.eva_set_status(
                &mut backend,
                &json!({"assistant_permit":permit,"state":"listening"})
            )
            .success
        );
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert_eq!(jwm.state.monitors[m].get_active_tags(), old);
        assert!(!jwm.eva_active());
    }
    #[test]
    fn retry_after_cancel_returns_receipt_and_never_replays() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let req = request(
            &jwm,
            "once",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(m),"tag_mask":2}}),
        );
        let first = jwm.eva_guarded_execute(&mut backend, &req);
        assert!(first.success);
        assert_eq!(first.data.as_ref().unwrap()["status"], "succeeded");
        jwm.eva_cancel("test");
        jwm.view(&mut backend, &WMArgEnum::UInt(1)).unwrap();
        let second = jwm.eva_guarded_execute(&mut backend, &req);
        assert_eq!(first.data, second.data);
        assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
        let mut conflict = req;
        conflict["action"]["arguments"]["tag_mask"] = json!(3);
        assert!(!jwm.eva_guarded_execute(&mut backend, &conflict).success);
    }
    #[test]
    fn recycled_native_window_id_does_not_retarget_old_token() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        let mut client = WMClient::new(WindowId::from_raw(7654));
        client.mon = Some(m);
        client.state.tags = 1;
        let key = jwm.insert_client(client.clone());
        jwm.attach_to_monitor(key, m);
        let token = jwm.eva_window_token(key);
        let req = request(
            &jwm,
            "old-window",
            json!({"name":"move_window_to_tag","arguments":{"window_token":token,"tag_mask":2}}),
        );
        jwm.state.clients.remove(key);
        let new = jwm.insert_client(client);
        jwm.attach_to_monitor(new, m);
        assert_ne!(token, jwm.eva_window_token(new));
        let result = jwm.eva_guarded_execute(&mut backend, &req);
        assert_eq!(result.data.unwrap()["status"], "failed");
        assert_eq!(jwm.state.clients[new].state.tags, 1);
    }
    #[test]
    fn capacity_is_reserved_before_any_side_effect() {
        let (mut jwm, mut backend) = setup();
        let m = jwm.state.sel_mon.unwrap();
        for n in 0..MAX_OPERATIONS {
            jwm.eva.operations.insert(
                format!("old-{n}"),
                Operation {
                    digest: "a".into(),
                    request: Value::Null,
                    result: Value::Null,
                    expires: Instant::now() + RETENTION,
                    terminal: true,
                    control_sequence: None,
                    background_keepalive: None,
                    clipboard_receipt: None,
                },
            );
        }
        let req = request(
            &jwm,
            "overflow",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(m),"tag_mask":2}}),
        );
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
    }
    #[test]
    fn lease_expiry_revokes_before_dispatch() {
        let (mut jwm, mut backend) = setup();
        jwm.eva.lease_until = Some(Instant::now() - Duration::from_millis(1));
        let m = jwm.state.sel_mon.unwrap();
        let req = request(
            &jwm,
            "expired",
            json!({"name":"switch_tag","arguments":{"monitor_token":jwm.eva_monitor_token(m),"tag_mask":2}}),
        );
        assert!(!jwm.eva_guarded_execute(&mut backend, &req).success);
        assert!(jwm.eva.permit.is_none());
        assert_eq!(jwm.state.monitors[m].get_active_tags(), 1);
    }
}
