//! Interrupted work retains semantic intent in memory, never execution authority.
//! Resumption must resolve the original object identities into a fresh context
//! and submit a new plan; an old approval or an uncertain result cannot replay.
use jwm_eva_core::{
    actions::{Disposition, FrozenPlan, Plan, execution_schema, validate_plan, validate_schema},
    ledger::{Ledger, OperationState},
    protocol::Action,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Mutex};

const MAX_STEPS: usize = 32;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Evidence {
    Unexecuted,
    Succeeded,
    SafeCancelled,
    Failed,
    Unknown(&'static str),
}
#[derive(Debug)]
struct Observed {
    id: Option<String>,
    evidence: Evidence,
}
#[derive(Debug)]
struct OutcomeCache {
    steps: Vec<Observed>,
    invalid_evidence: bool,
}

#[derive(Debug)]
pub struct InterruptedTask {
    semantic: Vec<Action>,
    frozen: Vec<Action>,
    plan_hash: String,
    wm_instance_id: String,
    window_anchors: BTreeMap<String, String>,
    monitor_anchors: BTreeMap<String, String>,
    // Terminal and uncertain evidence survives ledger pruning. Missing evidence
    // never turns a previously observed dispatch into permission to replay it.
    outcomes: Mutex<OutcomeCache>,
}
impl InterruptedTask {
    pub fn new(original: &Plan, prepared: &FrozenPlan, context: &Value) -> Result<Self, String> {
        if original.disposition != Disposition::Execute
            || original.actions.is_empty()
            || original.actions.len() > MAX_STEPS
            || prepared.disposition != Disposition::Execute
            || prepared.plan_hash.is_empty()
            || prepared.plan_hash.len() > 128
        {
            return Err("invalid_interrupted_task".into());
        }
        let normalized = validate_plan(original, context)?;
        if normalized.wm_instance_id != prepared.wm_instance_id
            || normalized.actions.len() != prepared.actions.len()
            || normalized.question != prepared.question
            || normalized.reason != prepared.reason
            || normalized.requires_confirmation != prepared.requires_confirmation
        {
            return Err("interrupted_task_plan_mismatch".into());
        }
        for (expected, prepared) in normalized.actions.iter().zip(&prepared.actions) {
            let mut actual = prepared.clone();
            // Registered application argv is deliberately local-only. Retain
            // it in the private prepared action for exact ledger correlation.
            if actual.name == "launch_application" {
                actual
                    .arguments
                    .as_object_mut()
                    .ok_or("invalid_prepared_action")?
                    .remove("argv");
            }
            if actual != *expected {
                return Err("interrupted_task_plan_mismatch".into());
            }
            let schema = execution_schema(&prepared.name).ok_or("unsupported_action")?;
            validate_schema(&prepared.arguments, &schema)?;
            if !available(context, &prepared.name) {
                return Err("interrupted_task_action_unavailable".into());
            }
        }
        let mut window_anchors = BTreeMap::new();
        let mut monitor_anchors = BTreeMap::new();
        for action in &original.actions {
            for (field, list, ref_field, token_field, anchors) in [
                (
                    "window_ref",
                    "windows",
                    "window_ref",
                    "target_token",
                    &mut window_anchors,
                ),
                (
                    "monitor_ref",
                    "monitors",
                    "monitor_ref",
                    "monitor_token",
                    &mut monitor_anchors,
                ),
            ] {
                if let Some(reference) = action.arguments.get(field) {
                    let reference = reference.as_str().ok_or("invalid_target_reference")?;
                    let object = unique_object(context, list, ref_field, reference)?;
                    let token = object[token_field]
                        .as_str()
                        .filter(|token| !token.is_empty() && token.len() <= 256)
                        .ok_or("invalid_target_identity")?;
                    anchors.insert(reference.to_owned(), token.to_owned());
                }
            }
        }
        let bytes = serde_json::to_vec(&(original, prepared, &window_anchors, &monitor_anchors))
            .map_err(|_| "invalid_interrupted_task")?;
        if bytes.len() > MAX_BYTES {
            return Err("interrupted_task_too_large".into());
        }
        Ok(Self {
            semantic: original.actions.clone(),
            frozen: prepared.actions.clone(),
            plan_hash: prepared.plan_hash.clone(),
            wm_instance_id: prepared.wm_instance_id.clone(),
            window_anchors,
            monitor_anchors,
            outcomes: Mutex::new(OutcomeCache {
                steps: original
                    .actions
                    .iter()
                    .map(|_| Observed {
                        id: None,
                        evidence: Evidence::Unexecuted,
                    })
                    .collect(),
                invalid_evidence: false,
            }),
        })
    }

    fn evidence(&self, ledger: &Ledger) -> (Vec<Evidence>, bool) {
        let mut cache = self
            .outcomes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut seen = vec![false; self.semantic.len()];
        for (key, record) in &ledger.operations {
            if record.plan_hash != self.plan_hash {
                continue;
            }
            let index = record
                .operation_id
                .rsplit('-')
                .nth(1)
                .and_then(|part| part.parse::<usize>().ok());
            let Some(index) = index.filter(|index| *index < self.frozen.len()) else {
                cache.invalid_evidence = true;
                continue;
            };
            if seen[index]
                || key != &record.operation_id
                || record.operation_id.len() > 512
                || record.wm_instance_id != self.wm_instance_id
                || record.action != self.frozen[index]
            {
                cache.invalid_evidence = true;
                continue;
            }
            seen[index] = true;
            let current = match record.state {
                OperationState::Succeeded => Evidence::Succeeded,
                OperationState::Failed => Evidence::Failed,
                OperationState::Cancelled
                    if record
                        .result
                        .as_ref()
                        .is_some_and(|result| result["dispatched"] == false) =>
                {
                    Evidence::SafeCancelled
                }
                OperationState::Cancelled => Evidence::Failed,
                OperationState::Unknown => Evidence::Unknown("unknown"),
                OperationState::Accepted => Evidence::Unknown("accepted"),
                OperationState::Dispatched => Evidence::Unknown("dispatched"),
            };
            let observed = &mut cache.steps[index];
            if observed
                .id
                .as_ref()
                .is_some_and(|id| id != &record.operation_id)
                || (matches!(
                    observed.evidence,
                    Evidence::Succeeded | Evidence::Failed | Evidence::SafeCancelled
                ) && observed.evidence != current)
            {
                cache.invalid_evidence = true;
                continue;
            }
            observed.id = Some(record.operation_id.clone());
            observed.evidence = current;
        }
        (
            cache.steps.iter().map(|step| step.evidence).collect(),
            cache.invalid_evidence,
        )
    }

    fn remaining(
        &self,
        context: &Value,
        evidence: &[Evidence],
    ) -> Result<Vec<(usize, Action)>, String> {
        if context["wm_instance_id"].as_str() != Some(&self.wm_instance_id)
            || context["session_unlocked"] != true
        {
            return Err("interrupted_task_target_unavailable".into());
        }
        let mut remaining = Vec::new();
        for (index, action) in self.semantic.iter().enumerate() {
            if !matches!(
                evidence[index],
                Evidence::Unexecuted | Evidence::SafeCancelled
            ) {
                continue;
            }
            if !available(context, &action.name) {
                return Err("interrupted_task_target_unavailable".into());
            }
            let action = self.rebind(action, context)?;
            // Fresh validation checks schema, session lock, target existence,
            // monitor lock, tag range and target-dependent constraints again.
            validate_plan(&execute_plan(vec![action.clone()]), context)?;
            remaining.push((index, action));
        }
        Ok(remaining)
    }

    fn rebind(&self, action: &Action, context: &Value) -> Result<Action, String> {
        let mut action = action.clone();
        for (field, list, ref_field, token_field, anchors) in [
            (
                "window_ref",
                "windows",
                "window_ref",
                "target_token",
                &self.window_anchors,
            ),
            (
                "monitor_ref",
                "monitors",
                "monitor_ref",
                "monitor_token",
                &self.monitor_anchors,
            ),
        ] {
            let Some(reference) = action.arguments.get(field) else {
                continue;
            };
            let token = reference
                .as_str()
                .and_then(|reference| anchors.get(reference))
                .ok_or("interrupted_task_target_unavailable")?;
            let object = unique_object(context, list, token_field, token)?;
            if list == "windows"
                && let Some(monitor_token) = object["monitor_token"].as_str()
            {
                let monitor = unique_object(context, "monitors", "monitor_token", monitor_token)?;
                if monitor["locked"] == true {
                    return Err("interrupted_task_target_unavailable".into());
                }
            }
            let fresh_ref = object[ref_field]
                .as_str()
                .ok_or("interrupted_task_target_unavailable")?;
            action.arguments[field] = json!(fresh_ref);
        }
        Ok(action)
    }

    /// A public data projection: no invocation hash, native tokens, argv or
    /// operation IDs can leave this module through the task context.
    pub fn refresh_view(&self, context: &Value, ledger: &Ledger) -> Value {
        let (evidence, invalid) = self.evidence(ledger);
        let mut completed = Vec::new();
        let mut uncertain = Vec::new();
        let mut unknown = invalid;
        let mut failed = false;
        for (index, state) in evidence.iter().enumerate() {
            let name = &self.semantic[index].name;
            match state {
                Evidence::Succeeded => completed.push(json!({"step_index":index,"name":name})),
                Evidence::Failed => {
                    failed = true;
                    uncertain.push(json!({"step_index":index,"name":name,"status":"failed"}));
                }
                Evidence::Unknown(status) => {
                    unknown = true;
                    uncertain.push(json!({"step_index":index,"name":name,"status":status}));
                }
                Evidence::Unexecuted | Evidence::SafeCancelled => {}
            }
        }
        if invalid {
            uncertain.push(
                json!({"step_index":null,"name":"unverified_step","status":"evidence_mismatch"}),
            );
        }
        let rebound = self.remaining(context, &evidence);
        let all_completed = evidence.iter().all(|state| *state == Evidence::Succeeded);
        let status = if unknown {
            "blocked_unknown"
        } else if failed {
            "blocked_failed"
        } else if all_completed {
            "completed"
        } else if rebound.is_err() {
            "target_unavailable"
        } else {
            "ready"
        };
        let remaining = rebound.unwrap_or_default().into_iter()
            .map(|(index, action)| json!({"step_index":index,"name":action.name,"arguments":action.arguments}))
            .collect::<Vec<_>>();
        json!({"status":status,"remaining":remaining,"completed":completed,"uncertain":uncertain,"requires_fresh_plan":true})
    }

    /// A corrected remainder still starts from fresh validation and cannot
    /// repeat a step whose success was already observed locally.
    pub fn validate_revision(
        &self,
        candidate: &[Action],
        context: &Value,
        ledger: &Ledger,
    ) -> Result<(), String> {
        if !matches!(
            self.refresh_view(context, ledger)["status"].as_str(),
            Some("ready" | "target_unavailable")
        ) {
            return Err("interrupted_task_not_ready".into());
        }
        let normalized = validate_available_plan(candidate, context)?;
        let (evidence, _) = self.evidence(ledger);
        for (index, state) in evidence.iter().enumerate() {
            if *state == Evidence::Succeeded
                && let Ok(completed) = self.rebind(&self.semantic[index], context)
                && let Ok(completed) = validate_plan(&execute_plan(vec![completed]), context)
                && normalized
                    .actions
                    .iter()
                    .any(|action| effect_key(action) == effect_key(&completed.actions[0]))
            {
                return Err("known_success_repeated".into());
            }
        }
        Ok(())
    }

    /// Independent new requests may proceed, but changing wording or values
    /// cannot authorize replay of an unresolved operation on the same object.
    pub fn validate_new(
        &self,
        candidate: &[Action],
        context: &Value,
        ledger: &Ledger,
    ) -> Result<(), String> {
        let normalized = validate_available_plan(candidate, context)?;
        let (evidence, invalid) = self.evidence(ledger);
        for (index, state) in evidence.iter().enumerate() {
            if !invalid && !matches!(state, Evidence::Unknown(_)) {
                continue;
            }
            let original = &self.frozen[index];
            for action in &normalized.actions {
                if effect_family(original) != effect_family(action) {
                    continue;
                }
                let same_target = if let Some(window) = original.arguments.get("window_token") {
                    action.arguments.get("window_token") == Some(window)
                } else if let Some(monitor) = original.arguments.get("monitor_token") {
                    action.arguments.get("monitor_token") == Some(monitor)
                } else {
                    true
                };
                if same_target {
                    return Err("task_unknown_outcome".into());
                }
            }
        }
        Ok(())
    }

    /// Continuation is the complete ordered remainder with exactly the original
    /// semantic arguments, rebound to the same objects. Corrections take a new
    /// plan path; completed steps and arbitrary subsets cannot be repeated here.
    pub fn validate_continuation(
        &self,
        candidate: &[Action],
        context: &Value,
        ledger: &Ledger,
    ) -> Result<(), String> {
        let view = self.refresh_view(context, ledger);
        if view["status"] != "ready" {
            return Err("interrupted_task_not_ready".into());
        }
        let expected = view["remaining"]
            .as_array()
            .ok_or("interrupted_task_not_ready")?;
        if candidate.is_empty()
            || candidate.len() != expected.len()
            || candidate.iter().zip(expected).any(|(action, expected)| {
                expected["name"].as_str() != Some(action.name.as_str())
                    || action.arguments != expected["arguments"]
            })
        {
            return Err("interrupted_task_continuation_mismatch".into());
        }
        validate_plan(&execute_plan(candidate.to_vec()), context)?;
        Ok(())
    }
}
// The registry intentionally has several semantic forms for the same local
// effect. An unresolved operation cannot be retried by selecting another form.
fn effect_family(action: &Action) -> String {
    match action.name.as_str() {
        "togglefloating" | "set_window_floating" => "window_floating".into(),
        "togglemaximize" | "set_window_maximized" => "window_maximized".into(),
        "togglesticky" | "set_window_sticky" => "window_sticky".into(),
        "togglepip" | "set_window_pip" => "window_pip".into(),
        "switch_tag" | "switch_tags" => "visible_tags".into(),
        "move_window_to_tag" | "move_window_to_tags" => "window_tags".into(),
        "set_volume" | "adjust_volume" => "output_volume".into(),
        "set_brightness" | "adjust_brightness" => "backlight".into(),
        "media_control" => format!(
            "media_{}",
            action.arguments["action"].as_str().unwrap_or("unknown")
        ),
        name => name.into(),
    }
}
fn effect_key(action: &Action) -> Value {
    let family = effect_family(action);
    let mut args = action.arguments.clone();
    if [
        "window_floating",
        "window_maximized",
        "window_sticky",
        "window_pip",
    ]
    .contains(&family.as_str())
    {
        args = json!({"window_token":args["window_token"],"enabled":args["enabled"]});
    } else if family.starts_with("media_") && action.name == "media_control" {
        args = if family == "media_seek" {
            json!({"position_us":args["position_us"]})
        } else {
            json!({})
        };
    }
    json!({"family":family,"arguments":args})
}

fn validate_available_plan(candidate: &[Action], context: &Value) -> Result<FrozenPlan, String> {
    if candidate
        .iter()
        .any(|action| !available(context, &action.name))
    {
        return Err("interrupted_task_action_unavailable".into());
    }
    validate_plan(&execute_plan(candidate.to_vec()), context)
}
fn execute_plan(actions: Vec<Action>) -> Plan {
    Plan {
        disposition: Disposition::Execute,
        actions,
        question: None,
        reason: None,
    }
}
fn available(context: &Value, name: &str) -> bool {
    context["available_actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|action| action.as_str() == Some(name)))
}
fn unique_object<'a>(
    context: &'a Value,
    list: &str,
    field: &str,
    value: &str,
) -> Result<&'a Value, String> {
    let mut matches = context[list]
        .as_array()
        .ok_or("interrupted_task_target_unavailable")?
        .iter()
        .filter(|object| object[field].as_str() == Some(value));
    let object = matches
        .next()
        .ok_or("interrupted_task_target_unavailable")?;
    if matches.next().is_some() {
        return Err("interrupted_task_target_unavailable".into());
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_eva_core::ledger::OperationRecord;

    fn context() -> Value {
        json!({"wm_instance_id":"private-wm","session_unlocked":true,"tag_count":9,
            "available_actions":["focus_window","move_window_to_tag","switch_tag","set_layout","launch_application","query_state","set_volume","adjust_volume","set_brightness","adjust_brightness","set_window_maximized","togglemaximize","switch_tags","move_window_to_tags","media_next","media_control"],
            "windows":[{"window_ref":"w1","target_token":"private-window-1","monitor_token":"private-monitor"},
                {"window_ref":"w2","target_token":"private-window-2","monitor_token":"private-monitor"}],
            "monitors":[{"monitor_ref":"m1","monitor_token":"private-monitor","locked":false}]})
    }
    fn actions() -> Vec<Action> {
        vec![
            Action {
                name: "focus_window".into(),
                arguments: json!({"window_ref":"w1"}),
            },
            Action {
                name: "move_window_to_tag".into(),
                arguments: json!({"window_ref":"w1","tag_number":3}),
            },
            Action {
                name: "switch_tag".into(),
                arguments: json!({"monitor_ref":"m1","tag_number":3}),
            },
        ]
    }
    fn task_with(actions: Vec<Action>) -> InterruptedTask {
        let original = execute_plan(actions);
        let mut prepared = validate_plan(&original, &context()).unwrap();
        prepared.plan_hash = "private-unique-plan".into();
        for action in &mut prepared.actions {
            if action.name == "launch_application" {
                action.arguments["argv"] = json!(["registered-app"]);
            }
        }
        InterruptedTask::new(&original, &prepared, &context()).unwrap()
    }
    fn task() -> InterruptedTask {
        task_with(actions())
    }
    fn record(
        task: &InterruptedTask,
        ledger: &mut Ledger,
        index: usize,
        state: OperationState,
        result: Option<Value>,
    ) {
        let id = format!("private-operation-100-{index}-prefix");
        ledger.operations.insert(
            id.clone(),
            OperationRecord {
                operation_id: id,
                plan_hash: task.plan_hash.clone(),
                wm_instance_id: task.wm_instance_id.clone(),
                action: task.frozen[index].clone(),
                state,
                result,
                created_ms: 0,
            },
        );
    }
    fn view_actions(view: &Value) -> Vec<Action> {
        view["remaining"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| Action {
                name: step["name"].as_str().unwrap().into(),
                arguments: step["arguments"].clone(),
            })
            .collect()
    }

    #[test]
    fn missing_records_are_unexecuted_and_continuation_is_the_exact_ordered_remainder() {
        let task = task();
        let ledger = Ledger::default();
        let view = task.refresh_view(&context(), &ledger);
        assert_eq!(view["status"], "ready");
        assert_eq!(view["remaining"].as_array().unwrap().len(), 3);
        assert!(
            task.validate_continuation(&actions(), &context(), &ledger)
                .is_ok()
        );
        assert!(
            task.validate_continuation(&actions()[..2], &context(), &ledger)
                .is_err()
        );
        let mut reversed = actions();
        reversed.reverse();
        assert!(
            task.validate_continuation(&reversed, &context(), &ledger)
                .is_err()
        );
    }
    #[test]
    fn successes_are_excluded_and_survive_ledger_pruning() {
        let task = task();
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Succeeded, None);
        let view = task.refresh_view(&context(), &ledger);
        assert_eq!(
            view["completed"],
            json!([{"step_index":0,"name":"focus_window"}])
        );
        assert_eq!(view["remaining"][0]["step_index"], 1);
        assert!(
            task.validate_continuation(&actions(), &context(), &ledger)
                .is_err()
        );
        ledger.operations.clear();
        assert_eq!(
            task.refresh_view(&context(), &ledger)["remaining"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn unknown_accepted_and_dispatched_block_until_the_same_operation_settles() {
        for state in [
            OperationState::Unknown,
            OperationState::Accepted,
            OperationState::Dispatched,
        ] {
            let task = task();
            let mut ledger = Ledger::default();
            record(&task, &mut ledger, 0, state, None);
            assert_eq!(
                task.refresh_view(&context(), &ledger)["status"],
                "blocked_unknown"
            );
            assert!(
                task.validate_continuation(&actions(), &context(), &ledger)
                    .is_err()
            );
            record(&task, &mut ledger, 0, OperationState::Succeeded, None);
            let view = task.refresh_view(&context(), &ledger);
            assert_eq!(view["status"], "ready");
            assert!(
                task.validate_continuation(&view_actions(&view), &context(), &ledger)
                    .is_ok()
            );
        }
    }
    #[test]
    fn lost_unknown_evidence_never_becomes_replay_permission() {
        let task = task();
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Unknown, None);
        task.refresh_view(&context(), &ledger);
        ledger.operations.clear();
        assert_eq!(
            task.refresh_view(&context(), &ledger)["status"],
            "blocked_unknown"
        );
    }
    #[test]
    fn only_explicitly_undispatched_cancellation_can_remain() {
        for (state, result, expected) in [
            (
                OperationState::Cancelled,
                Some(json!({"dispatched":false})),
                "ready",
            ),
            (OperationState::Cancelled, None, "blocked_failed"),
            (
                OperationState::Cancelled,
                Some(json!({"dispatched":true})),
                "blocked_failed",
            ),
            (
                OperationState::Failed,
                Some(json!({"dispatched":false})),
                "blocked_failed",
            ),
        ] {
            let task = task();
            let mut ledger = Ledger::default();
            record(&task, &mut ledger, 0, state, result);
            assert_eq!(task.refresh_view(&context(), &ledger)["status"], expected);
        }
    }
    #[test]
    fn fresh_references_rebind_only_to_the_original_identity() {
        let task = task();
        let mut fresh = context();
        fresh["windows"][0]["window_ref"] = json!("w9");
        fresh["windows"][1]["window_ref"] = json!("w1");
        fresh["monitors"][0]["monitor_ref"] = json!("m9");
        let view = task.refresh_view(&fresh, &Ledger::default());
        assert_eq!(view["remaining"][0]["arguments"]["window_ref"], "w9");
        assert_eq!(view["remaining"][2]["arguments"]["monitor_ref"], "m9");
        assert!(
            task.validate_continuation(&view_actions(&view), &fresh, &Ledger::default())
                .is_ok()
        );
        fresh["windows"][0]["target_token"] = json!("replacement-window");
        let view = task.refresh_view(&fresh, &Ledger::default());
        assert_eq!(view["status"], "target_unavailable");
        assert_eq!(view["remaining"], json!([]));
    }
    #[test]
    fn explicit_revision_can_replace_a_gone_target_but_not_unresolved_work() {
        let task = task();
        let mut fresh = context();
        fresh["windows"].as_array_mut().unwrap().remove(0);
        let replacement = vec![Action {
            name: "focus_window".into(),
            arguments: json!({"window_ref":"w2"}),
        }];
        let mut ledger = Ledger::default();
        assert_eq!(
            task.refresh_view(&fresh, &ledger)["status"],
            "target_unavailable"
        );
        assert!(
            task.validate_continuation(&replacement, &fresh, &ledger)
                .is_err()
        );
        assert!(
            task.validate_revision(&replacement, &fresh, &ledger)
                .is_ok()
        );
        let invalid = vec![Action {
            name: "focus_window".into(),
            arguments: json!({"window_ref":"gone"}),
        }];
        assert!(task.validate_revision(&invalid, &fresh, &ledger).is_err());
        record(&task, &mut ledger, 0, OperationState::Dispatched, None);
        assert!(
            task.validate_revision(&replacement, &fresh, &ledger)
                .is_err()
        );
    }
    #[test]
    fn fresh_locks_capability_loss_and_wm_restart_block_resumption() {
        let task = task();
        let mut locked = context();
        locked["monitors"][0]["locked"] = json!(true);
        let mut unavailable = context();
        unavailable["available_actions"] = json!([]);
        let mut restarted = context();
        restarted["wm_instance_id"] = json!("different-wm");
        let mut smaller = context();
        smaller["tag_count"] = json!(2);
        for fresh in [locked, unavailable, restarted, smaller] {
            let view = task.refresh_view(&fresh, &Ledger::default());
            assert_eq!(view["status"], "target_unavailable");
            assert_eq!(view["remaining"], json!([]));
        }
    }
    #[test]
    fn mismatched_record_action_is_never_a_success_receipt() {
        let task = task();
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Succeeded, None);
        ledger
            .operations
            .values_mut()
            .next()
            .unwrap()
            .action
            .arguments["window_token"] = json!("other-window");
        assert_eq!(
            task.refresh_view(&context(), &ledger)["status"],
            "blocked_unknown"
        );
        assert!(
            task.refresh_view(&context(), &ledger)["completed"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn malformed_or_duplicate_step_evidence_blocks_without_exposing_private_fields() {
        for malformed in [false, true] {
            let task = task();
            let mut ledger = Ledger::default();
            record(&task, &mut ledger, 0, OperationState::Succeeded, None);
            let mut duplicate = ledger.operations.values().next().unwrap().clone();
            duplicate.operation_id = if malformed {
                "malformed-id".into()
            } else {
                "second-operation-100-0-prefix".into()
            };
            ledger
                .operations
                .insert(duplicate.operation_id.clone(), duplicate);
            let view = task.refresh_view(&context(), &ledger);
            assert_eq!(view["status"], "blocked_unknown");
            let public = view.to_string();
            for private in [
                "private-window",
                "private-monitor",
                "private-operation",
                "private-unique-plan",
                "private-wm",
                "window_token",
                "argv",
            ] {
                assert!(!public.contains(private), "exposed {private}");
            }
        }
    }
    #[test]
    fn completed_task_has_no_executable_remainder() {
        let task = task();
        let mut ledger = Ledger::default();
        for index in 0..3 {
            record(&task, &mut ledger, index, OperationState::Succeeded, None);
        }
        let view = task.refresh_view(&context(), &ledger);
        assert_eq!(view["status"], "completed");
        assert_eq!(view["remaining"], json!([]));
        assert!(
            task.validate_continuation(&[], &context(), &ledger)
                .is_err()
        );
    }
    #[test]
    fn revisions_allow_modified_remainder_but_cannot_repeat_known_success() {
        let task = task();
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Succeeded, None);
        let mut revised = actions()[1..].to_vec();
        revised[0].arguments["tag_number"] = json!(4);
        assert!(
            task.validate_revision(&revised, &context(), &ledger)
                .is_ok()
        );
        assert_eq!(
            task.validate_revision(&actions(), &context(), &ledger)
                .unwrap_err(),
            "known_success_repeated"
        );
    }
    #[test]
    fn new_independent_requests_do_not_blindly_retry_unknown_work() {
        let task = task();
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Unknown, None);
        assert_eq!(
            task.validate_new(&actions()[..1], &context(), &ledger)
                .unwrap_err(),
            "task_unknown_outcome"
        );
        let independent = Action {
            name: "focus_window".into(),
            arguments: json!({"window_ref":"w2"}),
        };
        assert!(
            task.validate_new(&[independent], &context(), &ledger)
                .is_ok()
        );
        let task = task_with(vec![Action {
            name: "launch_application".into(),
            arguments: json!({"application_id":"browser"}),
        }]);
        let mut ledger = Ledger::default();
        record(&task, &mut ledger, 0, OperationState::Accepted, None);
        let other_app = Action {
            name: "launch_application".into(),
            arguments: json!({"application_id":"terminal"}),
        };
        assert_eq!(
            task.validate_new(&[other_app], &context(), &ledger)
                .unwrap_err(),
            "task_unknown_outcome"
        );
    }
    #[test]
    fn effect_aliases_cannot_repeat_an_unknown_or_successful_operation() {
        let unknown = task_with(vec![Action {
            name: "set_volume".into(),
            arguments: json!({"percent":40}),
        }]);
        let mut ledger = Ledger::default();
        record(&unknown, &mut ledger, 0, OperationState::Unknown, None);
        let adjust = Action {
            name: "adjust_volume".into(),
            arguments: json!({"delta":5}),
        };
        assert_eq!(
            unknown
                .validate_new(&[adjust], &context(), &ledger)
                .unwrap_err(),
            "task_unknown_outcome"
        );
        let completed = task_with(vec![
            Action {
                name: "set_window_maximized".into(),
                arguments: json!({"window_ref":"w1","enabled":true}),
            },
            actions()[1].clone(),
        ]);
        let mut ledger = Ledger::default();
        record(&completed, &mut ledger, 0, OperationState::Succeeded, None);
        let alias = Action {
            name: "togglemaximize".into(),
            arguments: json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"enabled":true}),
        };
        assert_eq!(
            completed
                .validate_revision(&[alias], &context(), &ledger)
                .unwrap_err(),
            "known_success_repeated"
        );
        let completed = task();
        let mut ledger = Ledger::default();
        record(&completed, &mut ledger, 2, OperationState::Succeeded, None);
        let alias = Action {
            name: "switch_tags".into(),
            arguments: json!({"monitor_ref":"m1","tag_numbers":[3]}),
        };
        assert_eq!(
            completed
                .validate_revision(&[alias], &context(), &ledger)
                .unwrap_err(),
            "known_success_repeated"
        );
    }

    #[test]
    fn construction_rejects_schema_mismatch_and_oversized_prepared_state() {
        let original = execute_plan(actions());
        let mut prepared = validate_plan(&original, &context()).unwrap();
        prepared.actions[0].arguments["window_token"] = json!("substituted-window");
        assert!(InterruptedTask::new(&original, &prepared, &context()).is_err());
        let original = execute_plan(vec![Action {
            name: "launch_application".into(),
            arguments: json!({"application_id":"browser"}),
        }]);
        let mut prepared = validate_plan(&original, &context()).unwrap();
        prepared.actions[0].arguments["argv"] = json!(vec!["a".repeat(4096); 18]);
        assert_eq!(
            InterruptedTask::new(&original, &prepared, &context()).unwrap_err(),
            "interrupted_task_too_large"
        );
        prepared.actions[0].arguments["argv"] = json!(["firefox"]);
        let task = InterruptedTask::new(&original, &prepared, &context()).unwrap();
        let public = task
            .refresh_view(&context(), &Ledger::default())
            .to_string();
        assert!(!public.contains("argv"));
        assert!(!public.contains("firefox"));
    }
}
