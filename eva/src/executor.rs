use crate::{
    config::Config,
    store::{Store, wall_ms},
};
use jwm_eva_core::{
    actions::FrozenPlan,
    ledger::{OperationRecord, OperationState, UndoPlan, UndoStep},
    protocol::{Action, AssistantPermit, ExpectedContext, GuardedRequest},
};
use serde_json::{Value, json};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct DispatchLifetime(Arc<AtomicBool>);
impl Drop for DispatchLifetime {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub async fn query(name: &str, args: Value) -> io::Result<Value> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || {
        let client = jwm_ipc_client::JwmIpc::new();
        match client.query_with_args(&name, args.clone()) {
            Err(e)
                if [
                    io::ErrorKind::TimedOut,
                    io::ErrorKind::WouldBlock,
                    io::ErrorKind::ConnectionReset,
                    io::ErrorKind::ConnectionRefused,
                    io::ErrorKind::NotFound,
                ]
                .contains(&e.kind()) =>
            {
                client.query_with_args(&name, args)
            }
            result => result,
        }
    })
    .await
    .map_err(io::Error::other)?
}
pub async fn command(name: &str, args: Value) -> io::Result<Value> {
    let name = name.to_string();
    tokio::task::spawn_blocking(move || jwm_ipc_client::JwmIpc::new().command(&name, args))
        .await
        .map_err(io::Error::other)?
}
pub fn prepare_applications(plan: &mut FrozenPlan, c: &Config) -> io::Result<()> {
    for a in &mut plan.actions {
        if a.name == "launch_application" {
            let id = a.arguments["application_id"]
                .as_str()
                .ok_or_else(|| io::Error::other("invalid_application_id"))?;
            let argv = c
                .applications
                .get(id)
                .ok_or_else(|| io::Error::other("application_not_registered"))?;
            a.arguments["argv"] = json!(argv);
        }
    }
    // A new invocation must obtain a fresh confirmation even for identical
    // actions in the same voice session; a previous approval cannot carry over.
    static NEXT_PLAN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sequence = NEXT_PLAN
        .fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |v| v.checked_add(1),
        )
        .map_err(|_| io::Error::other("plan_identity_exhausted"))?;
    plan.plan_hash = jwm_eva_core::protocol::digest(
        &json!({"plan_nonce":{"pid":std::process::id(),"monotonic_ns":crate::context::monotonic_ns()?,"sequence":sequence},"wm_instance_id":plan.wm_instance_id,"actions":plan.actions}),
    );
    Ok(())
}
pub async fn execute(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
) -> io::Result<Value> {
    execute_with_step_budget(
        plan,
        permit,
        confirmation,
        store,
        c,
        std::time::Duration::from_secs(22),
        None,
        None,
    )
    .await
}
pub async fn execute_with_progress(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    progress: &tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>,
) -> io::Result<Value> {
    execute_with_step_budget(
        plan,
        permit,
        confirmation,
        store,
        c,
        std::time::Duration::from_secs(22),
        Some(progress),
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_observed(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    progress: Option<&tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>>,
    owner: Option<&crate::companion::OperationOwner>,
) -> io::Result<Value> {
    execute_with_step_budget(
        plan,
        permit,
        confirmation,
        store,
        c,
        std::time::Duration::from_secs(22),
        progress,
        owner,
    )
    .await
}
/// Execute against the original planning snapshot. This is a non-authorizing
/// consistency guard; it never modifies actions, consent or target membership.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_observed_with_context(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    progress: Option<&tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>>,
    owner: Option<&crate::companion::OperationOwner>,
    captured_context: &Value,
) -> io::Result<Value> {
    execute_with_step_budget_and_context(
        plan,
        permit,
        confirmation,
        store,
        c,
        std::time::Duration::from_secs(22),
        progress,
        owner,
        Some(captured_context),
    )
    .await
}

#[derive(Clone, Debug, PartialEq)]
struct CloseWindowSnapshot {
    reference: String,
    monitor: String,
    tags: u64,
    native: Value,
    pid: Value,
    visible: bool,
    minimized: bool,
    sticky: bool,
    swallowed: bool,
}
#[derive(Clone, Debug, PartialEq)]
struct CloseContextSnapshot {
    instance: String,
    permit: Value,
    selected: Value,
    monitors: std::collections::BTreeMap<String, (u64, bool)>,
    windows: std::collections::BTreeMap<String, CloseWindowSnapshot>,
}
impl CloseContextSnapshot {
    fn capture(context: &Value) -> Result<Self, &'static str> {
        jwm_eva_core::protocol::validate_assistant_features(
            context,
            &[jwm_eva_core::protocol::DIRECTED_CLOSE_WINDOW_FEATURE],
        )
        .map_err(|_| "unsupported_action: directed_close_window_v1 unavailable")?;
        if context["windows_complete"] != true {
            return Err("unsupported_action: complete close snapshot unavailable");
        }
        if context["session_unlocked"] != true {
            return Err("session_locked");
        }
        let instance = context["wm_instance_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("close_snapshot_invalid")?
            .to_owned();
        let mut monitors = std::collections::BTreeMap::new();
        for row in context["monitors"]
            .as_array()
            .filter(|rows| rows.len() <= 128)
            .ok_or("close_snapshot_invalid")?
        {
            let token = row["monitor_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("close_snapshot_invalid")?;
            let tags = row["visible_tag_mask"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                .ok_or("close_snapshot_invalid")?;
            let locked = row["locked"].as_bool().ok_or("close_snapshot_invalid")?;
            if monitors.insert(token.to_owned(), (tags, locked)).is_some() {
                return Err("close_snapshot_ambiguous");
            }
        }
        let mut windows = std::collections::BTreeMap::new();
        let mut refs = std::collections::BTreeSet::new();
        for row in context["windows"]
            .as_array()
            .filter(|rows| rows.len() <= 128)
            .ok_or("close_snapshot_invalid")?
        {
            let token = row["target_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("close_snapshot_invalid")?;
            let reference = row["window_ref"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("close_snapshot_invalid")?;
            let monitor = row["monitor_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("close_snapshot_invalid")?;
            let tags = row["tag_mask"]
                .as_u64()
                .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                .ok_or("close_snapshot_invalid")?;
            if row["closed"] == true || row["live"] == false || !monitors.contains_key(monitor) {
                return Err("target_window_gone");
            }
            let get = |key| row[key].as_bool().ok_or("close_snapshot_invalid");
            let entry = CloseWindowSnapshot {
                reference: reference.into(),
                monitor: monitor.into(),
                tags,
                native: row["id"].clone(),
                pid: row["pid"].clone(),
                visible: get("visible")?,
                minimized: get("minimized")?,
                sticky: get("sticky")?,
                swallowed: get("swallowed")?,
            };
            if windows.insert(token.to_owned(), entry).is_some()
                || !refs.insert(reference.to_owned())
            {
                return Err("close_snapshot_ambiguous");
            }
        }
        Ok(Self {
            instance,
            permit: context["assistant_permit"].clone(),
            selected: context["selected_monitor_token"].clone(),
            monitors,
            windows,
        })
    }
    fn validates_targets(
        &self,
        plan: &FrozenPlan,
        permit: &AssistantPermit,
    ) -> Result<(), &'static str> {
        if self.instance != plan.wm_instance_id || self.permit != json!(permit) {
            return Err("assistant_permit_mismatch");
        }
        let mut seen = std::collections::BTreeSet::new();
        for action in plan
            .actions
            .iter()
            .filter(|action| action.name == "killclient")
        {
            let token = action.arguments["window_token"]
                .as_str()
                .ok_or("close_snapshot_invalid")?;
            let window = self.windows.get(token).ok_or("target_window_gone")?;
            if !seen.insert(token) {
                return Err("duplicate_close_target");
            }
            if self
                .monitors
                .get(&window.monitor)
                .is_none_or(|(_, locked)| *locked)
            {
                return Err("session_locked");
            }
            let expected = &action.arguments["expected_state"];
            if expected["window_token"] != token
                || expected["monitor_token"] != window.monitor
                || expected["tag_mask"].as_u64() != Some(window.tags)
                || action
                    .arguments
                    .get("monitor_token")
                    .is_some_and(|v| v.as_str() != Some(window.monitor.as_str()))
                || action.arguments.get("tag_mask").is_some_and(|v| {
                    v.as_u64()
                        .is_none_or(|mask| mask == 0 || mask & !window.tags != 0)
                })
            {
                return Err("stale_context: close source changed");
            }
        }
        if !seen.is_empty() && !plan.requires_confirmation {
            return Err("confirmation_required");
        }
        Ok(())
    }
}

fn close_preflight_check(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    captured: Option<&Value>,
    fresh: &Value,
) -> Result<(), &'static str> {
    let count = plan
        .actions
        .iter()
        .filter(|a| a.name == "killclient")
        .count();
    if count == 0 {
        return Ok(());
    }
    let current = CloseContextSnapshot::capture(fresh)?;
    current.validates_targets(plan, permit)?;
    if let Some(captured) = captured {
        let original = CloseContextSnapshot::capture(captured)?;
        original.validates_targets(plan, permit)?;
        if original != current {
            return Err("stale_context: batch close snapshot changed");
        }
    } else if count > 1 {
        return Err("unsupported_action: original batch close snapshot required");
    }
    Ok(())
}
fn close_preflight_failure(error: &str, total: usize) -> Value {
    let (code, detail) = error.split_once(':').unwrap_or((error, ""));
    json!({"status":"failed","error":code.trim(),"detail":detail.trim(),"completed_steps":0,"unexecuted_steps":total,"steps":[],"dispatched":false})
}

fn emit_progress(
    sender: Option<&tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>>,
    value: Option<crate::progress::ExecutionProgress>,
) {
    if let Some(sender) = sender {
        sender.send_replace(value);
    }
}
#[allow(clippy::too_many_arguments)]
async fn execute_with_step_budget(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    step_budget: std::time::Duration,
    progress: Option<&tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>>,
    owner: Option<&crate::companion::OperationOwner>,
) -> io::Result<Value> {
    execute_with_step_budget_and_context(
        plan,
        permit,
        confirmation,
        store,
        c,
        step_budget,
        progress,
        owner,
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn execute_with_step_budget_and_context(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    step_budget: std::time::Duration,
    progress: Option<&tokio::sync::watch::Sender<Option<crate::progress::ExecutionProgress>>>,
    owner: Option<&crate::companion::OperationOwner>,
    captured_context: Option<&Value>,
) -> io::Result<Value> {
    use crate::progress::{ExecutionProgress, Stage};
    let dispatch_cancelled = Arc::new(AtomicBool::new(false));
    let _lifetime = DispatchLifetime(dispatch_cancelled.clone());
    let mut steps = Vec::new();
    let mut undo = Vec::new();
    let mut ledger_error = None;
    // The first budget includes waiting for the shared persistent store too.
    let mut dispatch_deadline = std::time::Instant::now() + step_budget;
    if plan
        .actions
        .iter()
        .any(|action| action.name == "killclient")
    {
        // Entire close target set is checked before any plan side effect or
        // operation intent, including when another action precedes the closes.
        let fresh = query("get_execution_context", json!({})).await?;
        if let Err(error) = close_preflight_check(plan, permit, captured_context, &fresh) {
            return Ok(close_preflight_failure(error, plan.actions.len()));
        }
        if std::time::Instant::now() >= dispatch_deadline {
            return Ok(close_preflight_failure(
                "execution_deadline_expired",
                plan.actions.len(),
            ));
        }
    }
    let previous_mutating_plan_hash = crate::store::access(&store, |storage| {
        Ok(storage.ledger.last_mutating_plan_hash.clone())
    })
    .await?;
    for (index, action) in plan.actions.iter().enumerate() {
        let completed = steps
            .iter()
            .filter(|step| crate::progress::verified_step(step))
            .count();
        emit_progress(
            progress,
            ExecutionProgress::at(plan, index, completed, Stage::Preparing),
        );
        if index > 0 {
            dispatch_deadline = std::time::Instant::now() + step_budget;
        }
        if std::time::Instant::now() >= dispatch_deadline {
            steps.push(json!({"name":action.name,"status":"cancelled","dispatched":false,"error":"execution_deadline_expired"}));
            break;
        }
        let before = match query("get_execution_context", json!({})).await {
            Ok(context) => context,
            Err(error) => {
                steps.push(json!({"name":action.name,"status":"failed","dispatched":false,"error":"ipc_context_unavailable","detail":error.to_string()}));
                break;
            }
        };
        if before["wm_instance_id"] != plan.wm_instance_id
            || before["assistant_permit"] != json!(permit)
        {
            steps.push(json!({"name":action.name,"status":"cancelled","dispatched":false,"error":"assistant_permit_mismatch"}));
            break;
        }
        if std::time::Instant::now() >= dispatch_deadline {
            steps.push(json!({"name":action.name,"status":"cancelled","dispatched":false,"error":"execution_deadline_expired"}));
            break;
        }
        let prefix = plan.plan_hash.chars().take(8).collect::<String>();
        let id = format!(
            "{}-{}-{index}-{prefix}",
            permit.assistant_session_id,
            wall_ms()
        );
        let mut request = GuardedRequest {
            protocol_version: 1,
            wm_instance_id: plan.wm_instance_id.clone(),
            assistant_permit: permit.clone(),
            operation_id: id.clone(),
            request_digest: String::new(),
            confirmation_token: confirmation.clone(),
            expected_context: ExpectedContext {
                session_unlocked: true,
                monitor_token: action.arguments["monitor_token"]
                    .as_str()
                    .map(str::to_owned),
                focus_token: None,
                visible_tag_mask: None,
            },
            action: action.clone(),
        };
        if action.arguments.get("window_token").is_some()
            && !matches!(
                action.name.as_str(),
                "move_window_to_tag"
                    | "move_window_to_tags"
                    | "focus_window"
                    | "focus_agent_session"
                    | "move_window_to_monitor"
                    | "set_window_minimized"
                    | "set_window_maximized"
                    | "set_window_floating"
                    | "set_window_pip"
                    | "set_window_sticky"
                    | "query_window"
                    | "killclient"
                    | "close_window"
            )
        {
            request.expected_context.focus_token =
                action.arguments["window_token"].as_str().map(str::to_owned);
        }
        request.seal();
        let lifecycle_evidence = if matches!(action.name.as_str(), "quit" | "restart") {
            match crate::lifecycle::prepare(&id, &before) {
                Ok(evidence) => Some(evidence),
                Err(error) => {
                    steps.push(json!({"name":action.name,"status":"failed","dispatched":false,"error":"wm_process_identity_mismatch","detail":error.to_string()}));
                    break;
                }
            }
        } else {
            None
        };
        let intent = {
            let (id, plan_hash, wm_instance_id, action, lifecycle_evidence, cancelled) = (
                id.clone(),
                plan.plan_hash.clone(),
                plan.wm_instance_id.clone(),
                action.clone(),
                lifecycle_evidence.clone(),
                dispatch_cancelled.clone(),
            );
            crate::store::access(&store, move |storage| {
                if cancelled.load(Ordering::Acquire) {
                    crate::lifecycle::forget(&id);
                    return Err(io::Error::other("cancelled"));
                }
                storage
                .ledger
                .record_intent(OperationRecord {
                    operation_id: id.clone(),
                    plan_hash,
                    wm_instance_id,
                    action,
                    state: OperationState::Dispatched,
                    result: lifecycle_evidence.as_ref().map(
                        |evidence| json!({"status":"dispatched","verified":{"lifecycle":evidence}}),
                    ),
                    created_ms: wall_ms(),
                })
                .map_err(io::Error::other)
                .and_then(|()| storage.save())
            })
            .await
        };
        if let Err(error) = intent {
            crate::lifecycle::forget(&id);
            let detail = error.to_string();
            ledger_error = Some(detail.clone());
            let failure = json!({"status":"failed","error":"ledger_write_failed_before_dispatch","dispatched":false});
            // No desktop request has been sent. Even if a partial disk write
            // retained the intent, recovery must never infer it is replayable.
            let (failed_id, failed_result) = (id.clone(), failure.clone());
            let _ = crate::store::access(&store, move |storage| {
                storage
                    .ledger
                    .record_result(&failed_id, OperationState::Failed, failed_result)
                    .map_err(io::Error::other)
            })
            .await;
            steps.push(json!({"name":action.name,"operation_id":id,"status":"failed","dispatched":false,"ledger_error":detail,"result":failure}));
            break;
        }
        if let Some(owner) = owner {
            owner.register(
                &id,
                &plan.wm_instance_id,
                &jwm_eva_core::protocol::digest(&json!(action)),
                &request.request_digest,
            );
        }
        if std::time::Instant::now() >= dispatch_deadline {
            crate::lifecycle::forget(&id);
            let failure = json!({"status":"cancelled","error":"execution_deadline_expired","dispatched":false});
            let (saved_id, saved_failure) = (id.clone(), failure.clone());
            let saved = crate::store::access(&store, move |storage| {
                storage
                    .ledger
                    .record_result(&saved_id, OperationState::Cancelled, saved_failure)
                    .map_err(io::Error::other)
                    .and_then(|()| storage.save())?;
                publish_settlement(storage, &saved_id);
                Ok(())
            })
            .await;
            if let Err(error) = saved {
                ledger_error = Some(error.to_string());
            }
            steps.push(json!({"name":action.name,"operation_id":id,"status":"cancelled","dispatched":false,"result":failure}));
            break;
        }
        emit_progress(
            progress,
            ExecutionProgress::at(plan, index, completed, Stage::Executing),
        );
        crate::telemetry::event(
            "action_dispatched",
            &json!({"generation":permit.generation,"plan_hash":plan.plan_hash,"operation_id":id,"action":action.name,"model":c.openai.model,"phase":"desktop"}),
        );
        let (result, dispatched) = if let Some(evidence) = lifecycle_evidence {
            let id = id.clone();
            let instance = plan.wm_instance_id.clone();
            let name = action.name.clone();
            let record_action = action.clone();
            let storage = store.clone();
            let wire = json!(request);
            let cancelled = dispatch_cancelled.clone();
            // This one already-admitted operation may settle after the voice
            // actor closes. It never owns the remainder of the desktop plan.
            let current = tokio::spawn(async move {
                let (mut result, dispatched) =
                    dispatch_once_cancellable(wire, &id, &instance, dispatch_deadline, cancelled)
                        .await;
                if !result.is_object() {
                    result = json!({"status":"unknown"});
                }
                if !result["verified"].is_object() {
                    result["verified"] = json!({});
                }
                result["verified"]["lifecycle"] = evidence;
                if dispatched {
                    result = crate::lifecycle::verify(
                        &id,
                        &name,
                        result,
                        std::time::Duration::from_millis(1500),
                    )
                    .await;
                } else {
                    crate::lifecycle::forget(&id);
                }
                persist_settlement(storage, id, record_action, Value::Null, result, dispatched)
                    .await
            });
            match current.await {
                Ok(result) => result,
                Err(_) => (
                    json!({"status":"unknown","error":"lifecycle_verification_interrupted"}),
                    true,
                ),
            }
        } else {
            let id = id.clone();
            let instance = plan.wm_instance_id.clone();
            let wire = json!(request);
            let storage = store.clone();
            let action = action.clone();
            let before = before.clone();
            let cancelled = dispatch_cancelled.clone();
            // Barge-in drops the plan future. Retain only this already admitted
            // operation so a late IPC receipt settles its ledger intent; this
            // task has neither the remaining steps nor authority to replay it.
            let current = tokio::spawn(async move {
                let (result, dispatched) =
                    dispatch_once_cancellable(wire, &id, &instance, dispatch_deadline, cancelled)
                        .await;
                persist_settlement(storage, id, action, before, result, dispatched).await
            });
            match current.await {
                Ok(result) => result,
                Err(_) => (
                    json!({"status":"unknown","error":"operation_settlement_interrupted"}),
                    true,
                ),
            }
        };
        emit_progress(
            progress,
            ExecutionProgress::at(plan, index, completed, Stage::Verifying),
        );
        let result = if matches!(
            action.name.as_str(),
            "set_volume" | "adjust_volume" | "set_brightness" | "adjust_brightness"
        ) && dispatched
        {
            settle_level_control(result, &id, &plan.wm_instance_id, dispatch_deadline).await
        } else {
            result
        };
        let status = result["status"].as_str().unwrap_or("unknown").to_owned();
        crate::telemetry::event(
            if status == "succeeded" {
                "action_verified"
            } else {
                "action_result"
            },
            &json!({"generation":permit.generation,"plan_hash":plan.plan_hash,"operation_id":id,"action":action.name,"model":c.openai.model,"result":status}),
        );
        steps.push(json!({"name":action.name,"operation_id":id,"status":status,"dispatched":dispatched,"result":result}));
        let operation_state = match status.as_str() {
            "succeeded" => OperationState::Succeeded,
            "failed" => OperationState::Failed,
            "accepted" | "queued" | "in_flight" => OperationState::Accepted,
            "cancelled" => OperationState::Cancelled,
            _ => OperationState::Unknown,
        };
        let persisted = {
            let (saved_id, saved_result) = (
                id.clone(),
                crate::result_projection::for_ledger(&result, action, &before),
            );
            crate::store::access(&store, move |storage| {
                storage
                    .ledger
                    .record_result(&saved_id, operation_state, saved_result)
                    .map_err(io::Error::other)
                    .and_then(|()| storage.save())?;
                publish_settlement(storage, &saved_id);
                Ok(())
            })
            .await
        };
        if let Err(error) = persisted {
            let detail = error.to_string();
            ledger_error = Some(detail.clone());
            steps.last_mut().unwrap()["ledger_error"] = json!(detail);
            break;
        }
        if status != "succeeded" || std::time::Instant::now() >= dispatch_deadline {
            break;
        }
        // A WM lifecycle boundary invalidates every remaining frozen permit
        // and object. Never continue the old plan against a replacement WM.
        if matches!(action.name.as_str(), "quit" | "restart") {
            break;
        }
        // This snapshot supplies compensation data, not proof of the action:
        // JWM's confirmed receipt above remains true if the read-back fails.
        match query("get_execution_context", json!({})).await {
            Ok(after) => {
                if c.undo.enabled
                    && let Some(step) = inverse(action, &before, &after)
                {
                    undo.push(step);
                }
            }
            Err(error) => {
                steps.last_mut().unwrap()["undo_available"] = json!(false);
                steps.last_mut().unwrap()["undo_error"] = json!(error.to_string());
            }
        }
        if std::time::Instant::now() >= dispatch_deadline {
            break;
        }
    }
    if !undo.is_empty() && c.undo.enabled && ledger_error.is_none() {
        let (plan_hash, instance, max_age, max_plans, cancelled) = (
            plan.plan_hash.clone(),
            plan.wm_instance_id.clone(),
            c.undo.max_age_seconds,
            c.undo.max_plans,
            dispatch_cancelled.clone(),
        );
        let saved = crate::store::access(&store, move |storage| {
            if cancelled.load(Ordering::Acquire)
                || storage.ledger.last_mutating_plan_hash.as_deref() != Some(plan_hash.as_str())
            {
                return Ok(());
            }
            storage.ledger.remember_undo(
                UndoPlan {
                    plan_hash,
                    previous_mutating_plan_hash,
                    wm_instance_id: instance,
                    created_ms: wall_ms(),
                    steps: undo,
                },
                wall_ms(),
            );
            storage.ledger.undo.retain(|plan| {
                wall_ms().saturating_sub(plan.created_ms) <= max_age.saturating_mul(1000)
            });
            while storage.ledger.undo.len() > max_plans {
                storage.ledger.undo.remove(0);
            }
            storage.save()
        })
        .await;
        if let Err(error) = saved {
            ledger_error = Some(error.to_string());
        }
    }
    let report = execution_report(plan.actions.len(), steps, ledger_error);
    emit_progress(progress, ExecutionProgress::finished(plan, &report));
    Ok(report)
}
#[cfg(test)]
async fn dispatch_once(
    request: Value,
    id: &str,
    instance: &str,
    deadline: std::time::Instant,
) -> (Value, bool) {
    dispatch_once_cancellable(
        request,
        id,
        instance,
        deadline,
        Arc::new(AtomicBool::new(false)),
    )
    .await
}
async fn dispatch_once_cancellable(
    request: Value,
    id: &str,
    instance: &str,
    deadline: std::time::Instant,
    cancelled: Arc<AtomicBool>,
) -> (Value, bool) {
    // Check again inside the blocking worker: a delayed executor or fsync must
    // not turn a stale intention into a newly dispatched desktop mutation.
    let reply = tokio::task::spawn_blocking(move || {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "execution_cancelled",
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "execution_deadline_expired",
            ));
        }
        jwm_ipc_client::JwmIpc::new().command("guarded_execute", request)
    })
    .await
    .map_err(io::Error::other)
    .and_then(|reply| reply);
    match reply {
        Err(error)
            if error.kind() == io::ErrorKind::Interrupted
                && ["execution_deadline_expired", "execution_cancelled"]
                    .contains(&error.to_string().as_str()) =>
        {
            (
                json!({"status":"cancelled","error":error.to_string(),"dispatched":false}),
                false,
            )
        }
        Ok(result) => (result, true),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            (json!({"status":"failed","error":error.to_string()}), false)
        }
        Err(_) => {
            let result = match query(
                "get_execution_status",
                json!({"operation_id":id,"wm_instance_id":instance}),
            )
            .await
            {
                Ok(result)
                    if result["status"]
                        .as_str()
                        .is_some_and(|status| status != "not_found") =>
                {
                    result
                }
                _ => json!({"status":"unknown","error":"ipc_timeout"}),
            };
            (result, true)
        }
    }
}
/// Wait only for the submitted operation's receipt. Queries can settle an
/// accepted control, but never re-dispatch it or infer success from an OSD.
async fn settle_level_control(
    mut result: Value,
    id: &str,
    instance: &str,
    dispatch_deadline: std::time::Instant,
) -> Value {
    let deadline =
        dispatch_deadline.min(std::time::Instant::now() + std::time::Duration::from_secs(2));
    loop {
        if result["operation_id"].as_str() != Some(id) {
            return json!({"operation_id":id,"status":"unknown","error":"control_receipt_mismatch"});
        }
        if !matches!(
            result["status"].as_str(),
            Some("accepted" | "queued" | "in_flight")
        ) {
            return result;
        }
        let observed = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            query(
                "get_execution_status",
                json!({"operation_id":id,"wm_instance_id":instance}),
            ),
        )
        .await;
        match observed {
            Ok(Ok(receipt)) => result = receipt,
            _ => {
                result["status"] = json!("unknown");
                result["error"] = json!("control_readback_unavailable");
                return result;
            }
        }
        if matches!(
            result["status"].as_str(),
            Some("accepted" | "queued" | "in_flight")
        ) {
            if std::time::Instant::now() >= deadline {
                result["status"] = json!("unknown");
                result["error"] = json!("control_readback_timeout");
                return result;
            }
            tokio::time::sleep_until(tokio::time::Instant::from_std(
                deadline.min(std::time::Instant::now() + std::time::Duration::from_millis(40)),
            ))
            .await;
        }
    }
}
pub(crate) async fn persist_settlement(
    storage: Arc<Mutex<Store>>,
    id: String,
    action: Action,
    before: Value,
    mut result: Value,
    dispatched: bool,
) -> (Value, bool) {
    // Carry only the verified same-instance source monitor from this actual
    // dispatch's before snapshot. It is local evidence, never model input.
    if action.name == "move_window_to_monitor"
        && result["status"] == "succeeded"
        && result["verified"]["window_token"] == action.arguments["window_token"]
        && let Some(window) = before["windows"].as_array().and_then(|windows| {
            let mut matching = windows
                .iter()
                .filter(|w| w["target_token"] == action.arguments["window_token"]);
            let first = matching.next()?;
            matching.next().is_none().then_some(first)
        })
        && before["monitors"].as_array().is_some_and(|monitors| {
            monitors
                .iter()
                .filter(|m| m["monitor_token"] == window["monitor_token"])
                .count()
                == 1
        })
    {
        result["verified"]["previous_monitor_token"] = window["monitor_token"].clone();
    }
    crate::store::access(&storage, move |store| {
        let current = store.ledger.operations.get(&id);
        if current.is_some_and(|record| {
            matches!(
                record.state,
                OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
            )
        }) && let Some(known) = current.and_then(|record| record.result.clone())
        {
            return Ok((known, dispatched));
        }
        let saved = store
            .ledger
            .record_result(
                &id,
                operation_state(&result),
                crate::result_projection::for_ledger(&result, &action, &before),
            )
            .map_err(io::Error::other)
            .and_then(|()| store.save());
        if let Err(error) = saved {
            result["ledger_error"] = json!(error.to_string());
        } else {
            publish_settlement(store, &id);
        }
        Ok((result, dispatched))
    })
    .await
    .unwrap_or_else(|_| {
        (
            json!({"status":"unknown","error":"ledger_write_failed"}),
            dispatched,
        )
    })
}
fn publish_settlement(store: &Store, id: &str) {
    if !crate::companion::operation_tracked(id) {
        return;
    }
    let Some(record) = store.ledger.operations.get(id) else {
        return;
    };
    let Some(result) = record.result.as_ref() else {
        return;
    };
    let outcome = stored_outcome(record);
    crate::companion::operation_settled(crate::companion::OperationSettlement {
        id,
        plan: &record.plan_hash,
        instance: &record.wm_instance_id,
        action_digest: &jwm_eva_core::protocol::digest(&json!(record.action)),
        result_digest: &jwm_eva_core::protocol::digest(result),
        outcome,
    });
}

fn stored_outcome(record: &OperationRecord) -> companion_voice::dialogue::Outcome {
    use companion_voice::dialogue::Outcome;
    let result = record.result.as_ref().unwrap_or(&Value::Null);
    let playback_verified = result["verified"]["playback_verified"]
        .as_bool()
        .or_else(|| result["verified"]["read_back"]["playback_verified"].as_bool());
    match record.state {
        OperationState::Succeeded
            if result["status"] == "succeeded" && playback_verified != Some(false) =>
        {
            Outcome::Succeeded
        }
        OperationState::Failed if result["status"] == "failed" => Outcome::Failed,
        OperationState::Cancelled if result["status"] == "cancelled" => Outcome::Cancelled,
        _ => Outcome::Unknown,
    }
}

fn operation_state(result: &Value) -> OperationState {
    match result["status"].as_str().unwrap_or("unknown") {
        "succeeded" => OperationState::Succeeded,
        "failed" => OperationState::Failed,
        "accepted" | "queued" | "in_flight" => OperationState::Accepted,
        "cancelled" => OperationState::Cancelled,
        _ => OperationState::Unknown,
    }
}
fn execution_report(total: usize, steps: Vec<Value>, ledger_error: Option<String>) -> Value {
    let completed = steps
        .iter()
        .filter(|step| step["status"] == "succeeded")
        .count();
    let submitted = steps
        .iter()
        .filter(|step| step["dispatched"] != false)
        .count();
    let status = if completed == total {
        "succeeded"
    } else if completed > 0 {
        "partial"
    } else {
        steps
            .last()
            .and_then(|step| step["status"].as_str())
            .unwrap_or("failed")
    };
    let mut result = json!({"status":status,"completed_steps":completed,"steps":steps,"unexecuted_steps":total.saturating_sub(submitted)});
    if let Some(error) = ledger_error {
        result["ledger_error"] = json!(error);
    }
    result
}
fn find<'a>(context: &'a Value, field: &str, token: &Value, key: &str) -> Option<&'a Value> {
    context[field].as_array()?.iter().find(|v| v[key] == *token)
}
fn inverse(a: &Action, before: &Value, after: &Value) -> Option<UndoStep> {
    if before["wm_instance_id"] != after["wm_instance_id"] {
        return None;
    }
    let args = &a.arguments;
    let (inv, projection) = match a.name.as_str() {
        "switch_tag" | "switch_tags" | "toggle_tags" | "set_visible_tags" => {
            let b = find(before, "monitors", &args["monitor_token"], "monitor_token")?;
            let p = find(after, "monitors", &args["monitor_token"], "monitor_token")?;
            (
                Action {
                    name: "set_visible_tags".into(),
                    arguments: json!({"monitor_token":args["monitor_token"],"tag_mask":b["visible_tag_mask"].as_u64()?}),
                },
                json!({"monitor_token":args["monitor_token"],"visible_tag_mask":p["visible_tag_mask"].as_u64()?}),
            )
        }
        "set_layout" | "set_layout_at" => {
            let tag = args["tag_mask"].as_u64()?;
            let match_ws = |v: &Value| {
                v["workspaces"]
                    .as_array()?
                    .iter()
                    .find(|w| {
                        w["monitor_token"] == args["monitor_token"]
                            && w["tag_mask"].as_u64() == Some(tag)
                    })
                    .and_then(|w| w["layout"].as_str())
                    .map(str::to_owned)
            };
            (
                Action {
                    name: "set_layout".into(),
                    arguments: json!({"monitor_token":args["monitor_token"],"tag_mask":tag,"layout":match_ws(before)?}),
                },
                json!({"monitor_token":args["monitor_token"],"tag_mask":tag,"layout":match_ws(after)?}),
            )
        }
        "move_window_to_tag" | "move_window_to_tags" | "toggle_window_tags" => {
            let b = find(before, "windows", &args["window_token"], "target_token")?;
            let p = find(after, "windows", &args["window_token"], "target_token")?;
            (
                Action {
                    name: "move_window_to_tags".into(),
                    arguments: json!({"window_token":args["window_token"],"tag_mask":b["tag_mask"].as_u64()?}),
                },
                json!({"window_token":args["window_token"],"tag_mask":p["tag_mask"].as_u64()?,"monitor_token":p["monitor_token"].as_str()?}),
            )
        }
        "move_window_to_monitor" => {
            let b = find(before, "windows", &args["window_token"], "target_token")?;
            let p = find(after, "windows", &args["window_token"], "target_token")?;
            (
                Action {
                    name: a.name.clone(),
                    arguments: json!({"window_token":args["window_token"],"monitor_token":b["monitor_token"].as_str()?,"restore_tag_mask":b["tag_mask"].as_u64()?}),
                },
                json!({"window_token":args["window_token"],"tag_mask":p["tag_mask"].as_u64()?,"monitor_token":p["monitor_token"].as_str()?}),
            )
        }
        "focus_window" => {
            let previous = before["focus_token"].as_str()?;
            find(after, "windows", &json!(previous), "target_token")?;
            (
                Action {
                    name: "focus_window".into(),
                    arguments: json!({"window_token":previous}),
                },
                json!({"focus_token":after["focus_token"],"selected_monitor_token":after["selected_monitor_token"]}),
            )
        }
        "focus_monitor" => (
            Action {
                name: "focus_monitor".into(),
                arguments: json!({"monitor_token":before["selected_monitor_token"].as_str()?}),
            },
            json!({"focus_token":after["focus_token"],"selected_monitor_token":after["selected_monitor_token"]}),
        ),
        name => {
            let (adapter, field, argument) = match name {
                "set_window_minimized" => ("set_window_minimized", "minimized", "minimized"),
                "set_window_maximized" | "togglemaximize" => {
                    ("set_window_maximized", "maximized", "enabled")
                }
                "set_window_floating" | "togglefloating" => {
                    ("set_window_floating", "floating", "enabled")
                }
                "set_window_sticky" | "togglesticky" => ("set_window_sticky", "sticky", "enabled"),
                "set_window_pip" | "togglepip" => ("set_window_pip", "pip", "enabled"),
                _ => return None,
            };
            let b = find(before, "windows", &args["window_token"], "target_token")?;
            let p = find(after, "windows", &args["window_token"], "target_token")?;
            // A boolean setter cannot reconstruct a single-axis maximize.
            // Older servers without per-axis evidence are equally ambiguous.
            let maximize_axes = if adapter == "set_window_maximized" {
                let (before_h, before_v) = (
                    b["maximized_horz"].as_bool()?,
                    b["maximized_vert"].as_bool()?,
                );
                let (after_h, after_v) = (
                    p["maximized_horz"].as_bool()?,
                    p["maximized_vert"].as_bool()?,
                );
                if before_h != before_v || after_h != after_v {
                    return None;
                }
                Some((after_h, after_v))
            } else {
                None
            };
            let mut arguments = json!({"window_token":args["window_token"]});
            arguments[argument] = json!(b[field].as_bool()?);
            let mut projection = json!({"window_token":args["window_token"],"tag_mask":p["tag_mask"].as_u64()?,"monitor_token":p["monitor_token"].as_str()?});
            projection[field] = json!(p[field].as_bool()?);
            if let Some((horz, vert)) = maximize_axes {
                projection["maximized_horz"] = json!(horz);
                projection["maximized_vert"] = json!(vert);
            }
            (
                Action {
                    name: adapter.into(),
                    arguments,
                },
                projection,
            )
        }
    };
    Some(UndoStep {
        inverse: inv,
        after: projection,
    })
}

/// Compensate the most recent reversible plan. The initial check catches
/// manual edits, and each inverse carries that same post-state into JWM's
/// atomic guard so an edit between this query and dispatch is also rejected.
pub async fn undo(
    permit: &AssistantPermit,
    store: Arc<Mutex<Store>>,
    c: &Config,
) -> io::Result<Value> {
    undo_observed(permit, store, c, None).await
}
pub(crate) async fn undo_observed(
    permit: &AssistantPermit,
    store: Arc<Mutex<Store>>,
    c: &Config,
    owner: Option<&crate::companion::OperationOwner>,
) -> io::Result<Value> {
    if !c.undo.enabled {
        return Err(io::Error::other("undo_disabled"));
    }
    let context = query("get_execution_context", json!({})).await?;
    let instance = context["wm_instance_id"]
        .as_str()
        .ok_or_else(|| io::Error::other("missing_wm_instance_id"))?;
    if context["assistant_permit"] != serde_json::to_value(permit)? {
        return Err(io::Error::other("assistant_permit_mismatch"));
    }
    let (original, mut actions) = {
        let ledger = crate::store::ledger(&store).await?;
        let original = ledger.latest_undo_plan().map_err(io::Error::other)?.clone();
        if wall_ms().saturating_sub(original.created_ms)
            > c.undo.max_age_seconds.saturating_mul(1000)
        {
            return Err(io::Error::other("undo_expired"));
        }
        let current = original
            .steps
            .iter()
            .map(|step| undo_projection(&context, &step.after))
            .collect::<io::Result<Vec<_>>>()?;
        let actions = ledger
            .undo_candidate(instance, wall_ms(), &current)
            .map_err(io::Error::other)?;
        (original, actions)
    };
    for (action, step) in actions.iter_mut().zip(original.steps.iter().rev()) {
        action.arguments["expected_state"] = step.after.clone();
    }
    let plan = FrozenPlan {
        plan_hash: jwm_eva_core::protocol::digest(
            &json!({"undo_of":original.plan_hash,"wm_instance_id":instance,"actions":actions}),
        ),
        wm_instance_id: instance.into(),
        disposition: jwm_eva_core::actions::Disposition::Execute,
        actions,
        question: None,
        reason: Some("撤销最近一次已核验的操作".into()),
        requires_confirmation: false,
    };
    let mut undo_config = c.clone();
    undo_config.undo.enabled = false;
    let owner = owner.map(|owner| owner.with_plan(&plan.plan_hash));
    let mut result = execute_observed(
        &plan,
        permit,
        None,
        store.clone(),
        &undo_config,
        None,
        owner.as_ref(),
    )
    .await?;
    // A compound plan can also contain irreversible effects. Compensation
    // reports only the reversible changes that were actually restored.
    result["undo_scope"] = json!("reversible_changes");
    let completed = result["steps"].as_array().map_or(0, |steps| {
        steps
            .iter()
            .filter(|step| step["status"] == "succeeded")
            .count()
    });
    let uncertain = result["steps"].as_array().is_some_and(|steps| {
        steps.iter().any(|step| {
            matches!(
                step["status"].as_str(),
                Some("unknown" | "accepted" | "in_flight")
            ) || (step["status"] == "failed" && step["dispatched"] != false)
        })
    });
    {
        let saved = persist_undo_cleanup(
            &store,
            original,
            plan.plan_hash.clone(),
            completed,
            uncertain,
        )
        .await;
        if let Err(error) = saved {
            result["ledger_error"] = json!(error.to_string());
        }
    }
    Ok(result)
}

async fn persist_undo_cleanup(
    store: &Arc<Mutex<Store>>,
    original: UndoPlan,
    compensation_hash: String,
    completed: usize,
    uncertain: bool,
) -> io::Result<()> {
    crate::store::access(store, move |s| {
        // A cancelled caller can leave this bookkeeping queued behind disk
        // I/O. It must never rewind a newer mutation admitted in the meantime.
        if !s
            .ledger
            .last_mutating_plan_hash
            .as_ref()
            .is_some_and(|hash| hash == &original.plan_hash || hash == &compensation_hash)
        {
            return Ok(());
        }
        if let Some(latest) = s
            .ledger
            .undo
            .last_mut()
            .filter(|p| p.plan_hash == original.plan_hash)
        {
            if uncertain {
                latest.steps.clear();
            } else {
                latest
                    .steps
                    .truncate(latest.steps.len().saturating_sub(completed));
            }
            if latest.steps.is_empty() {
                s.ledger.undo.pop();
            }
        }
        // Successful compensation exposes exactly the previous mutation
        // frontier. A partial known refusal may retry only the uncompleted
        // portion; an uncertain compensation cannot expose an older plan.
        if !uncertain {
            s.ledger.last_mutating_plan_hash = if completed == original.steps.len() {
                original.previous_mutating_plan_hash.clone().or_else(|| {
                    // Legacy compensation records have no trustworthy
                    // predecessor. Refuse to skip across an older plan.
                    (!s.ledger.undo.is_empty()).then(|| compensation_hash.clone())
                })
            } else {
                Some(original.plan_hash.clone())
            };
        }
        s.save()
    })
    .await
}

fn undo_projection(context: &Value, template: &Value) -> io::Result<Value> {
    let error = || io::Error::other("undo_target_gone");
    let fields = template.as_object().ok_or_else(error)?;
    let source =
        if fields.contains_key("focus_token") || fields.contains_key("selected_monitor_token") {
            context
        } else if let Some(token) = fields.get("window_token") {
            find(context, "windows", token, "target_token").ok_or_else(error)?
        } else if fields.contains_key("layout") {
            context["workspaces"]
                .as_array()
                .and_then(|items| {
                    items.iter().find(|w| {
                        w["monitor_token"] == template["monitor_token"]
                            && w["tag_mask"] == template["tag_mask"]
                    })
                })
                .ok_or_else(error)?
        } else {
            find(
                context,
                "monitors",
                &template["monitor_token"],
                "monitor_token",
            )
            .ok_or_else(error)?
        };
    let mut projection = serde_json::Map::new();
    for key in fields.keys() {
        let source_key = if key == "window_token" {
            "target_token"
        } else {
            key
        };
        projection.insert(
            key.clone(),
            source.get(source_key).cloned().ok_or_else(error)?,
        );
    }
    Ok(Value::Object(projection))
}

/// Rotate over every unresolved tombstone, including old unknown results.
/// Limiting one pass never turns the same first sixteen records into a wall
/// that hides later receipts. This selects read-only status queries only.
fn reconciliation_batch(
    ledger: &jwm_eva_core::ledger::Ledger,
    cursor: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let ids = ledger
        .operations
        .iter()
        .filter(|(_, record)| {
            matches!(
                record.state,
                OperationState::Dispatched | OperationState::Accepted | OperationState::Unknown
            )
        })
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    let start = cursor.map_or(0, |cursor| ids.partition_point(|id| *id <= cursor));
    ids[start..]
        .iter()
        .chain(ids[..start].iter())
        .take(limit.min(ids.len()))
        .map(|id| (*id).to_owned())
        .collect()
}

/// Reconcile historical receipts even after the voice epoch has been cancelled.
/// This is read-only and can never restore a session or replay an operation.
pub async fn reconcile(store: Arc<Mutex<Store>>) -> io::Result<()> {
    static CURSOR: Mutex<Option<String>> = Mutex::new(None);
    let snapshot_store = store.clone();
    // Both a busy Store mutex and its durable filesystem operations can wait
    // arbitrarily long under host I/O pressure. Never occupy an async worker
    // while taking the snapshot or writing a reconciled receipt.
    let pending = tokio::task::spawn_blocking(move || {
        let mut cursor = CURSOR.lock().unwrap();
        let s = snapshot_store.lock().unwrap();
        let ids = reconciliation_batch(&s.ledger, cursor.as_deref(), 16);
        *cursor = ids.last().cloned();
        ids.into_iter()
            .map(|id| {
                let record = &s.ledger.operations[&id];
                (
                    id,
                    record.wm_instance_id.clone(),
                    record.action.name.clone(),
                    record.result.clone(),
                )
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|_| io::Error::other("reconciliation_snapshot_failed"))?;
    for (id, instance, action, previous) in pending {
        let receipt = if matches!(action.as_str(), "quit" | "restart") {
            let mut previous = previous.unwrap_or_else(|| json!({"status":"unknown"}));
            if previous["status"] == "dispatched" {
                previous["status"] = json!("unknown");
                previous["error"] = json!("dispatch_receipt_missing");
                Ok(previous)
            } else {
                Ok(crate::lifecycle::verify(
                    &id,
                    &action,
                    previous,
                    std::time::Duration::from_millis(200),
                )
                .await)
            }
        } else {
            query(
                "get_execution_status",
                json!({"operation_id":id,"wm_instance_id":instance}),
            )
            .await
        };
        let (state, value) = match receipt {
            Ok(v)
                if !matches!(action.as_str(), "quit" | "restart")
                    && v["status"] != "unknown"
                    && v["operation_id"].as_str() != Some(id.as_str()) =>
            {
                (
                    OperationState::Unknown,
                    json!({"status":"unknown","error":"historical_receipt_identity_mismatch"}),
                )
            }
            Ok(v) => {
                let st = match v["status"].as_str().unwrap_or("") {
                    "succeeded" => OperationState::Succeeded,
                    "failed" => OperationState::Failed,
                    "cancelled" => OperationState::Cancelled,
                    "accepted" | "in_flight" | "queued" => OperationState::Accepted,
                    _ => OperationState::Unknown,
                };
                (st, v)
            }
            Err(_) => (
                OperationState::Unknown,
                json!({"status":"unknown","error":"historical_receipt_unavailable"}),
            ),
        };
        let settlement_store = store.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut s = settlement_store.lock().unwrap();
            if s.ledger.operations.get(&id).is_some_and(|record| {
                matches!(
                    record.state,
                    OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
                )
            }) {
                return Ok(());
            }
            let projected = if let Some(record) = s.ledger.operations.get(&id) {
                crate::result_projection::for_ledger(
                    &value,
                    &record.action,
                    record.result.as_ref().unwrap_or(&Value::Null),
                )
            } else {
                return Ok(());
            };
            if s.ledger
                .operations
                .get(&id)
                .is_some_and(|r| r.state != state || r.result.as_ref() != Some(&projected))
            {
                s.ledger
                    .record_result(&id, state, projected)
                    .map_err(io::Error::other)?;
                s.save()?;
                publish_settlement(&s, &id);
            }
            Ok(())
        })
        .await
        .map_err(|_| io::Error::other("reconciliation_settlement_failed"))??;
    }
    Ok(())
}

#[cfg(test)]
mod undo_tests {
    use super::*;
    #[test]
    fn shared_receipts_require_confirmed_status_and_playback_evidence() {
        use companion_voice::dialogue::Outcome;
        let mut record = OperationRecord {
            operation_id: "op".into(),
            plan_hash: "plan".into(),
            wm_instance_id: "wm".into(),
            action: Action {
                name: "open_media".into(),
                arguments: json!({}),
            },
            state: OperationState::Succeeded,
            result: Some(json!({"status":"accepted"})),
            created_ms: 1,
        };
        assert_eq!(stored_outcome(&record), Outcome::Unknown);
        record.result = Some(json!({"status":"succeeded","verified":{"playback_verified":false}}));
        assert_eq!(stored_outcome(&record), Outcome::Unknown);
        record.result = Some(
            json!({"status":"succeeded","verified":{"playback_verified":true,"read_back":{"playback_verified":false}}}),
        );
        assert_eq!(stored_outcome(&record), Outcome::Succeeded);
        record.state = OperationState::Failed;
        record.result = Some(json!({"status":"failed"}));
        assert_eq!(stored_outcome(&record), Outcome::Failed);
    }
    #[tokio::test]
    async fn late_undo_bookkeeping_cannot_rewind_a_newer_or_already_cleared_frontier() {
        let dir = [std::path::PathBuf::from("/dev/shm"), std::env::temp_dir()]
            .into_iter()
            .find_map(|base| {
                let p = base.join(format!(
                    "eva-undo-bookkeeping-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                crate::store::private_dir(&p).ok().map(|()| p)
            })
            .unwrap();
        let store = Arc::new(Mutex::new(Store::open_at(dir.clone()).unwrap()));
        let original = UndoPlan {
            plan_hash: "original".into(),
            previous_mutating_plan_hash: Some("previous".into()),
            wm_instance_id: "wm".into(),
            created_ms: wall_ms(),
            steps: vec![UndoStep {
                inverse: Action {
                    name: "set_layout".into(),
                    arguments: json!({}),
                },
                after: json!({}),
            }],
        };
        for frontier in [Some("newer"), None, Some("compensation")] {
            {
                let mut storage = store.lock().unwrap();
                storage.ledger.undo = vec![original.clone()];
                storage.ledger.last_mutating_plan_hash = frontier.map(str::to_owned);
            }
            persist_undo_cleanup(&store, original.clone(), "compensation".into(), 1, false)
                .await
                .unwrap();
            let storage = store.lock().unwrap();
            if frontier == Some("compensation") {
                assert!(storage.ledger.undo.is_empty());
                assert_eq!(
                    storage.ledger.last_mutating_plan_hash.as_deref(),
                    Some("previous")
                );
            } else {
                assert_eq!(storage.ledger.undo.len(), 1);
                assert_eq!(storage.ledger.last_mutating_plan_hash.as_deref(), frontier);
            }
        }
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn reconciliation_rotates_beyond_sixteen_and_keeps_old_unknown_receipts_eligible() {
        use jwm_eva_core::ledger::Ledger;
        let mut ledger = Ledger::default();
        for number in 0..40 {
            let id = format!("op-{number:02}");
            ledger.operations.insert(
                id.clone(),
                OperationRecord {
                    operation_id: id,
                    plan_hash: "old-plan".into(),
                    wm_instance_id: "old-wm".into(),
                    action: Action {
                        name: "stop_recording".into(),
                        arguments: json!({}),
                    },
                    state: if number == 39 {
                        OperationState::Succeeded
                    } else {
                        OperationState::Unknown
                    },
                    result: Some(json!({"status":"unknown"})),
                    created_ms: 1,
                },
            );
        }
        let first = reconciliation_batch(&ledger, None, 16);
        let second = reconciliation_batch(&ledger, first.last().map(String::as_str), 16);
        let third = reconciliation_batch(&ledger, second.last().map(String::as_str), 16);
        assert_eq!(first.len(), 16);
        assert_eq!(second.len(), 16);
        assert_eq!(third.len(), 16);
        let visited = first
            .iter()
            .chain(&second)
            .chain(&third)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            visited.len(),
            39,
            "every old unresolved operation must get a status query"
        );
        assert!(
            !visited.contains("op-39"),
            "terminal operations need no polling"
        );
        assert!(second.iter().all(|id| !first.contains(id)));
        assert!(reconciliation_batch(&ledger, None, 0).is_empty());
    }
    #[tokio::test]
    async fn expired_dispatch_worker_never_opens_a_desktop_request() {
        let (result, dispatched) = dispatch_once(
            json!({}),
            "expired",
            "old-instance",
            std::time::Instant::now() - std::time::Duration::from_millis(1),
        )
        .await;
        assert!(!dispatched);
        assert_eq!(result["status"], "cancelled");
        assert_eq!(result["error"], "execution_deadline_expired");
    }
    #[tokio::test]
    async fn dropped_plan_lifetime_fences_queued_dispatch_without_desktop_request() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let lifetime = DispatchLifetime(cancelled.clone());
        drop(lifetime);
        let (result, dispatched) = dispatch_once_cancellable(
            json!({}),
            "cancelled",
            "old-instance",
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            cancelled,
        )
        .await;
        assert!(!dispatched);
        assert_eq!(result["status"], "cancelled");
        assert_eq!(result["error"], "execution_cancelled");
    }
    #[test]
    fn inverse_covers_declared_initial_actions_and_freezes_related_state() {
        use jwm_eva_core::actions::validate_execution_action;
        let before = json!({"wm_instance_id":"wm","selected_monitor_token":"m1","focus_token":"w0",
            "monitors":[{"monitor_token":"m1","visible_tag_mask":3},{"monitor_token":"m2","visible_tag_mask":4}],
            "windows":[{"target_token":"w0","monitor_token":"m1","tag_mask":3},{"target_token":"w1","monitor_token":"m1","tag_mask":3,"minimized":false,"maximized":false,"maximized_horz":false,"maximized_vert":false,"floating":false,"sticky":false,"pip":false}],
            "workspaces":[{"monitor_token":"m1","tag_mask":2,"layout":"tile"}]});
        let after = json!({"wm_instance_id":"wm","selected_monitor_token":"m2","focus_token":"w1",
            "monitors":[{"monitor_token":"m1","visible_tag_mask":6},{"monitor_token":"m2","visible_tag_mask":4}],
            "windows":[{"target_token":"w0","monitor_token":"m1","tag_mask":3},{"target_token":"w1","monitor_token":"m2","tag_mask":4,"minimized":true,"maximized":true,"maximized_horz":true,"maximized_vert":true,"floating":true,"sticky":true,"pip":true}],
            "workspaces":[{"monitor_token":"m1","tag_mask":2,"layout":"grid"}]});
        for (name, arguments) in [
            ("switch_tag", json!({"monitor_token":"m1","tag_mask":2})),
            ("switch_tags", json!({"monitor_token":"m1","tag_mask":6})),
            ("toggle_tags", json!({"monitor_token":"m1","tag_mask":5})),
            (
                "set_layout",
                json!({"monitor_token":"m1","tag_mask":2,"layout":"grid"}),
            ),
            (
                "set_layout_at",
                json!({"monitor_token":"m1","tag_mask":2,"layout":"grid"}),
            ),
            (
                "move_window_to_tag",
                json!({"window_token":"w1","tag_mask":4}),
            ),
            (
                "move_window_to_tags",
                json!({"window_token":"w1","tag_mask":4}),
            ),
            (
                "toggle_window_tags",
                json!({"window_token":"w1","tag_mask":7}),
            ),
            (
                "move_window_to_monitor",
                json!({"window_token":"w1","monitor_token":"m2"}),
            ),
            ("focus_window", json!({"window_token":"w1"})),
            ("focus_monitor", json!({"monitor_token":"m2"})),
            (
                "set_window_minimized",
                json!({"window_token":"w1","minimized":true}),
            ),
            (
                "set_window_maximized",
                json!({"window_token":"w1","enabled":true}),
            ),
            (
                "set_window_floating",
                json!({"window_token":"w1","enabled":true}),
            ),
            (
                "set_window_sticky",
                json!({"window_token":"w1","enabled":true}),
            ),
            (
                "set_window_pip",
                json!({"window_token":"w1","enabled":true}),
            ),
            (
                "togglefloating",
                json!({"window_token":"w1","monitor_token":"m1","tag_mask":3,"enabled":true}),
            ),
        ] {
            let action = Action {
                name: name.into(),
                arguments,
            };
            let step = inverse(&action, &before, &after)
                .unwrap_or_else(|| panic!("missing inverse: {name}"));
            let mut guarded = step.inverse.clone();
            guarded.arguments["expected_state"] = step.after.clone();
            validate_execution_action(&guarded)
                .unwrap_or_else(|error| panic!("{name}: {error}: {guarded:?}"));
            assert_eq!(
                undo_projection(&after, &step.after).unwrap(),
                step.after,
                "{name}"
            );
            if name == "move_window_to_monitor" {
                assert_eq!(step.inverse.arguments["monitor_token"], "m1");
                assert_eq!(step.inverse.arguments["restore_tag_mask"], 3);
            }
        }
        let max_action = Action {
            name: "set_window_maximized".into(),
            arguments: json!({"window_token":"w1","enabled":true}),
        };
        let mut single_axis = before.clone();
        single_axis["windows"][1]["maximized_horz"] = json!(true);
        single_axis["windows"][1]["maximized"] = json!(true);
        assert!(
            inverse(&max_action, &single_axis, &after).is_none(),
            "one boolean cannot restore a single-axis prestate"
        );
        let guarded = inverse(&max_action, &before, &after).unwrap();
        let mut manual_axis_change = after.clone();
        manual_axis_change["windows"][1]["maximized_horz"] = json!(false);
        assert_ne!(
            undo_projection(&manual_axis_change, &guarded.after).unwrap(),
            guarded.after,
            "single-axis manual changes are compensation conflicts"
        );
        let focus = Action {
            name: "focus_window".into(),
            arguments: json!({"window_token":"w1"}),
        };
        let mut no_old_window = after.clone();
        no_old_window["windows"] = json!([]);
        assert!(inverse(&focus, &before, &no_old_window).is_none());
        let mut replaced = after.clone();
        replaced["wm_instance_id"] = json!("replacement");
        assert!(inverse(&focus, &before, &replaced).is_none());
    }
    #[test]
    fn identical_plan_invocations_require_fresh_confirmation_identity() {
        let make = || FrozenPlan {
            plan_hash: "old".into(),
            wm_instance_id: "wm".into(),
            disposition: jwm_eva_core::actions::Disposition::Execute,
            actions: vec![Action {
                name: "set_config".into(),
                arguments: json!({"key":"gaps","value":10}),
            }],
            question: None,
            reason: None,
            requires_confirmation: true,
        };
        let (mut first, mut second) = (make(), make());
        prepare_applications(&mut first, &Config::default()).unwrap();
        prepare_applications(&mut second, &Config::default()).unwrap();
        assert_ne!(first.plan_hash, second.plan_hash);
    }
    #[test]
    fn undo_projection_uses_instance_and_relevant_fields_only() {
        let ctx = json!({"windows":[{"target_token":"old-instance:w1","tag_mask":4,"monitor_token":"m1","title":"unrelated new title"}]});
        let after = json!({"window_token":"old-instance:w1","tag_mask":4,"monitor_token":"m1"});
        assert_eq!(undo_projection(&ctx, &after).unwrap(), after);
        assert!(
            undo_projection(
                &ctx,
                &json!({"window_token":"new-instance:w1","tag_mask":4,"monitor_token":"m1"})
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod close_preflight_tests {
    use super::*;
    fn fixture() -> (FrozenPlan, AssistantPermit, Value) {
        let permit = AssistantPermit {
            assistant_session_id: "session".into(),
            generation: 7,
        };
        let windows = (1..=2).map(|n| json!({"window_ref":format!("w{n}"),"target_token":format!("t{n}"),"id":format!("native{n}"),"pid":n,"monitor_token":"m1","tag_mask":1,"visible":true,"minimized":false,"sticky":false,"swallowed":false})).collect::<Vec<_>>();
        let context = json!({"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"wm_instance_id":"wm","assistant_permit":permit,"session_unlocked":true,"windows_complete":true,"selected_monitor_token":"m1","monitors":[{"monitor_token":"m1","visible_tag_mask":1,"locked":false}],"windows":windows});
        let actions = (1..=2).map(|n| { let token=format!("t{n}"); Action {name:"killclient".into(),arguments:json!({"window_token":token,"expected_state":{"window_token":token,"monitor_token":"m1","tag_mask":1}})} }).collect();
        let plan = FrozenPlan {
            plan_hash: "hash".into(),
            wm_instance_id: "wm".into(),
            disposition: jwm_eva_core::actions::Disposition::Execute,
            actions,
            question: None,
            reason: None,
            requires_confirmation: true,
        };
        (plan, permit, context)
    }
    #[test]
    fn entire_batch_is_checked_before_first_effect_and_focus_is_irrelevant() {
        let (plan, permit, context) = fixture();
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &context),
            Ok(())
        );
        let mut focus = context.clone();
        focus["focus_token"] = json!("t2");
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &focus),
            Ok(())
        );
        for field in ["target_token", "monitor_token", "tag_mask", "id", "visible"] {
            let mut fresh = context.clone();
            fresh["windows"][1][field] = match field {
                "tag_mask" => json!(2),
                "visible" => json!(false),
                _ => json!("changed"),
            };
            assert!(
                close_preflight_check(&plan, &permit, Some(&context), &fresh).is_err(),
                "{field}"
            );
        }
        let mut fresh = context.clone();
        fresh["windows"].as_array_mut().unwrap().pop();
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &fresh),
            Err("target_window_gone")
        );
        let result = close_preflight_failure("target_window_gone", plan.actions.len());
        assert_eq!(result["completed_steps"], 0);
        assert_eq!(result["dispatched"], false);
    }
    #[test]
    fn missing_capability_original_snapshot_and_confirmation_fail_closed() {
        let (mut plan, permit, context) = fixture();
        assert!(
            close_preflight_check(&plan, &permit, None, &context)
                .unwrap_err()
                .starts_with("unsupported_action")
        );
        for key in ["assistant_protocol", "windows_complete"] {
            let mut old = context.clone();
            old.as_object_mut().unwrap().remove(key);
            assert!(close_preflight_check(&plan, &permit, Some(&context), &old).is_err());
        }
        plan.requires_confirmation = false;
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &context),
            Err("confirmation_required")
        );
        plan.requires_confirmation = true;
        plan.actions[1] = plan.actions[0].clone();
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &context),
            Err("duplicate_close_target")
        );
    }
    #[test]
    fn complete_snapshot_and_lease_identity_cannot_drift() {
        let (plan, permit, context) = fixture();
        for key in [
            "wm_instance_id",
            "assistant_permit",
            "selected_monitor_token",
        ] {
            let mut fresh = context.clone();
            fresh[key] = json!("changed");
            assert!(close_preflight_check(&plan, &permit, Some(&context), &fresh).is_err());
        }
        let mut fresh = context.clone();
        let mut extra = fresh["windows"][0].clone();
        extra["target_token"] = json!("t3");
        extra["window_ref"] = json!("w3");
        fresh["windows"].as_array_mut().unwrap().push(extra);
        assert!(close_preflight_check(&plan, &permit, Some(&context), &fresh).is_err());
        let mut fresh = context.clone();
        fresh["monitors"][0]["locked"] = json!(true);
        assert_eq!(
            close_preflight_check(&plan, &permit, Some(&context), &fresh),
            Err("session_locked")
        );
    }
}
