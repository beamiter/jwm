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
    sync::{Arc, Mutex},
};

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
    )
    .await
}
async fn execute_with_step_budget(
    plan: &FrozenPlan,
    permit: &AssistantPermit,
    confirmation: Option<String>,
    store: Arc<Mutex<Store>>,
    c: &Config,
    step_budget: std::time::Duration,
) -> io::Result<Value> {
    let mut steps = Vec::new();
    let mut undo = Vec::new();
    let mut ledger_error = None;
    // The first budget includes waiting for the shared persistent store too.
    let mut dispatch_deadline = std::time::Instant::now() + step_budget;
    let previous_mutating_plan_hash = store.lock().unwrap().ledger.last_mutating_plan_hash.clone();
    for (index, action) in plan.actions.iter().enumerate() {
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
                    | "move_window_to_monitor"
                    | "set_window_minimized"
                    | "set_window_maximized"
                    | "set_window_floating"
                    | "set_window_pip"
                    | "set_window_sticky"
                    | "query_window"
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
            let mut storage = store.lock().unwrap();
            storage
                .ledger
                .record_intent(OperationRecord {
                    operation_id: id.clone(),
                    plan_hash: plan.plan_hash.clone(),
                    wm_instance_id: plan.wm_instance_id.clone(),
                    action: action.clone(),
                    state: OperationState::Dispatched,
                    result: lifecycle_evidence.as_ref().map(
                        |evidence| json!({"status":"dispatched","verified":{"lifecycle":evidence}}),
                    ),
                    created_ms: wall_ms(),
                })
                .map_err(io::Error::other)
                .and_then(|()| storage.save())
        };
        if let Err(error) = intent {
            crate::lifecycle::forget(&id);
            let detail = error.to_string();
            ledger_error = Some(detail.clone());
            let failure = json!({"status":"failed","error":"ledger_write_failed_before_dispatch","dispatched":false});
            // No desktop request has been sent. Even if a partial disk write
            // retained the intent, recovery must never infer it is replayable.
            let _ = store.lock().unwrap().ledger.record_result(
                &id,
                OperationState::Failed,
                failure.clone(),
            );
            steps.push(json!({"name":action.name,"operation_id":id,"status":"failed","dispatched":false,"ledger_error":detail,"result":failure}));
            break;
        }
        if std::time::Instant::now() >= dispatch_deadline {
            crate::lifecycle::forget(&id);
            let failure = json!({"status":"cancelled","error":"execution_deadline_expired","dispatched":false});
            let mut storage = store.lock().unwrap();
            let saved = storage
                .ledger
                .record_result(&id, OperationState::Cancelled, failure.clone())
                .map_err(io::Error::other)
                .and_then(|()| storage.save());
            if let Err(error) = saved {
                ledger_error = Some(error.to_string());
            }
            steps.push(json!({"name":action.name,"operation_id":id,"status":"cancelled","dispatched":false,"result":failure}));
            break;
        }
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
            // This one already-admitted operation may settle after the voice
            // actor closes. It never owns the remainder of the desktop plan.
            let current = tokio::spawn(async move {
                let (mut result, dispatched) =
                    dispatch_once(wire, &id, &instance, dispatch_deadline).await;
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
                let mut store = storage.lock().unwrap();
                let current = store.ledger.operations.get(&id);
                if current.is_some_and(|record| {
                    matches!(
                        record.state,
                        OperationState::Succeeded
                            | OperationState::Failed
                            | OperationState::Cancelled
                    )
                }) && let Some(known) = current.and_then(|record| record.result.clone())
                {
                    return (known, dispatched);
                }
                let status = operation_state(&result);
                let saved = store
                    .ledger
                    .record_result(
                        &id,
                        status,
                        crate::result_projection::for_ledger(&result, &record_action, &Value::Null),
                    )
                    .map_err(io::Error::other)
                    .and_then(|()| store.save());
                if let Err(error) = saved {
                    result["ledger_error"] = json!(error.to_string());
                }
                (result, dispatched)
            });
            match current.await {
                Ok(result) => result,
                Err(_) => (
                    json!({"status":"unknown","error":"lifecycle_verification_interrupted"}),
                    true,
                ),
            }
        } else {
            dispatch_once(json!(request), &id, &plan.wm_instance_id, dispatch_deadline).await
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
            let mut storage = store.lock().unwrap();
            storage
                .ledger
                .record_result(
                    &id,
                    operation_state,
                    crate::result_projection::for_ledger(&result, action, &before),
                )
                .map_err(io::Error::other)
                .and_then(|()| storage.save())
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
        let mut storage = store.lock().unwrap();
        storage.ledger.remember_undo(
            UndoPlan {
                plan_hash: plan.plan_hash.clone(),
                previous_mutating_plan_hash,
                wm_instance_id: plan.wm_instance_id.clone(),
                created_ms: wall_ms(),
                steps: undo,
            },
            wall_ms(),
        );
        storage.ledger.undo.retain(|plan| {
            wall_ms().saturating_sub(plan.created_ms) <= c.undo.max_age_seconds.saturating_mul(1000)
        });
        while storage.ledger.undo.len() > c.undo.max_plans {
            storage.ledger.undo.remove(0);
        }
        if let Err(error) = storage.save() {
            ledger_error = Some(error.to_string());
        }
    }
    Ok(execution_report(plan.actions.len(), steps, ledger_error))
}
async fn dispatch_once(
    request: Value,
    id: &str,
    instance: &str,
    deadline: std::time::Instant,
) -> (Value, bool) {
    // Check again inside the blocking worker: a delayed executor or fsync must
    // not turn a stale intention into a newly dispatched desktop mutation.
    let reply = tokio::task::spawn_blocking(move || {
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
                && error.to_string() == "execution_deadline_expired" =>
        {
            (
                json!({"status":"cancelled","error":"execution_deadline_expired","dispatched":false}),
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
        let s = store.lock().unwrap();
        let original = s
            .ledger
            .latest_undo_plan()
            .map_err(io::Error::other)?
            .clone();
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
        let actions = s
            .ledger
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
    let mut result = execute(&plan, permit, None, store.clone(), &undo_config).await?;
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
        let mut s = store.lock().unwrap();
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
                    (!s.ledger.undo.is_empty()).then(|| plan.plan_hash.clone())
                })
            } else {
                Some(original.plan_hash.clone())
            };
        }
        if let Err(error) = s.save() {
            result["ledger_error"] = json!(error.to_string());
        }
    }
    Ok(result)
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
    let pending = {
        let mut cursor = CURSOR.lock().unwrap();
        let s = store.lock().unwrap();
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
    };
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
        let mut s = store.lock().unwrap();
        if s.ledger.operations.get(&id).is_some_and(|record| {
            matches!(
                record.state,
                OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
            )
        }) {
            continue;
        }
        let projected = if let Some(record) = s.ledger.operations.get(&id) {
            crate::result_projection::for_ledger(
                &value,
                &record.action,
                record.result.as_ref().unwrap_or(&Value::Null),
            )
        } else {
            continue;
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
        }
    }
    Ok(())
}

#[cfg(test)]
mod undo_tests {
    use super::*;
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
