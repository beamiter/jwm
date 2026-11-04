//! Actor-local evidence from one verified native switch, never cloud history or
//! a current-monitor inference. This module constructs no actions or permits.
use super::intent_syntax::{ParsedIntent, Primitive, parse_owned};
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use jwm_eva_core::protocol::{AssistantPermit, digest};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    wm: String,
    pid: u64,
    ticks: String,
    wake: u64,
    permit: AssistantPermit,
    epoch: Option<u64>,
}
#[derive(Debug, Clone)]
pub(crate) struct VerifiedWorkspaceScope {
    identity: Identity,
    monitor_token: String,
    monitor_ref: String,
    tag: u32,
    tag_count: u64,
    captured_at: u64,
    // Local provenance only. Neither this scope nor these opaque execution
    // identities implements Serialize or supplies cloud conversation history.
    _executed_hash: String,
    _operation_id: String,
}
#[derive(Debug)]
pub(super) enum Verdict {
    Unsupported,
    Reject(&'static str),
    Proved(Proof),
}
#[derive(Debug)]
pub(super) struct Proof {
    hash: String,
    plan_digest: String,
    frozen_digest: String,
    frame: (u64, u64),
}
impl Proof {
    pub(super) fn accepts(&self, plan: &Plan, frozen: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == frozen.plan_hash
            && self.plan_digest == digest(&serde_json::to_value(plan).unwrap())
            && self.frozen_digest == digest(&serde_json::to_value(frozen).unwrap())
    }
}
fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
fn identity(context: &Value) -> Option<Identity> {
    if context["session_unlocked"] != true {
        return None;
    }
    let ticks = context["wm_start_ticks"].as_str()?;
    let parsed = ticks.parse::<u64>().ok()?;
    if parsed == 0 || parsed.to_string() != ticks {
        return None;
    }
    let permit: AssistantPermit =
        serde_json::from_value(context["assistant_permit"].clone()).ok()?;
    if permit.generation == 0
        || nonempty(&context["assistant_permit"]["assistant_session_id"]).is_none()
    {
        return None;
    }
    let epoch = match context.get("connection_epoch") {
        None => None,
        Some(value) => Some(value.as_u64()?),
    };
    Some(Identity {
        wm: nonempty(&context["wm_instance_id"])?.into(),
        pid: context["wm_pid"].as_u64().filter(|pid| *pid > 0)?,
        ticks: ticks.into(),
        wake: context["wake_generation"].as_u64()?,
        permit,
        epoch,
    })
}
fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|m| m.len() == keys.len() && keys.iter().all(|k| m.contains_key(*k)))
}
fn ordinary(name: &str) -> bool {
    jwm_eva_core::actions::semantic_actions().iter().any(|a| {
        a.canonical_name == name && matches!(a.risk_class.as_str(), "ordinary" | "reversible")
    })
}
fn monitor<'a>(context: &'a Value, token: &str) -> Option<&'a Value> {
    let monitors = context["monitors"]
        .as_array()
        .filter(|m| !m.is_empty() && m.len() <= 32)?;
    let mut matches = monitors.iter().filter(|m| m["monitor_token"] == token);
    let found = matches.next()?;
    let reference = nonempty(&found["monitor_ref"])?;
    if matches.next().is_some()
        || found["locked"] != false
        || monitors
            .iter()
            .filter(|m| m["monitor_ref"] == reference)
            .count()
            != 1
    {
        return None;
    }
    Some(found)
}
fn tag_count(context: &Value) -> Option<u64> {
    context["tag_count"]
        .as_u64()
        .filter(|n| (1..=32).contains(n))
}
fn valid_frozen_hash(frozen: &FrozenPlan) -> bool {
    frozen.plan_hash
        == digest(
            &json!({"wm_instance_id":frozen.wm_instance_id,"disposition":frozen.disposition,"actions":frozen.actions,"question":frozen.question,"reason":frozen.reason}),
        )
}

/// `result` MUST be the correlated raw executor report, before cloud/public
/// projection, and `frozen`/`captured` from that same local execution. Native
/// switch receipts contain only monitor_token and visible_tag_mask; process
/// and permit provenance comes from this same actor-owned captured context.
/// No public result, guessed current target or synthetic natural history fits.
#[cfg(test)]
pub(super) fn from_local_result(
    result: &Value,
    frozen: &FrozenPlan,
    captured: &Value,
) -> Option<VerifiedWorkspaceScope> {
    scope_from_result(result, frozen, frozen, captured, false)
}

/// The actor must retain its canonical validated object and pass the actual
/// post-prepare object plus its correlated raw execution report. Preparation's
/// nonce is not returned on the wire, so this pure check cannot authenticate
/// arbitrary caller-invented nonce hashes. It verifies the stage transition,
/// unchanged complete payload and the executor's actual operation-id binding.
pub(super) fn from_prepared_local_result(
    result: &Value,
    canonical: &FrozenPlan,
    executed: &FrozenPlan,
    captured: &Value,
) -> Option<VerifiedWorkspaceScope> {
    if !valid_frozen_hash(canonical)
        || executed.plan_hash == canonical.plan_hash
        || executed.plan_hash.len() != 64
        || !executed
            .plan_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut original = serde_json::to_value(canonical).ok()?;
    let mut prepared = serde_json::to_value(executed).ok()?;
    original.as_object_mut()?.remove("plan_hash");
    prepared.as_object_mut()?.remove("plan_hash");
    if original != prepared {
        return None;
    }
    scope_from_result(result, canonical, executed, captured, true)
}

fn prepared_operation_matches(operation: &str, executed: &FrozenPlan, identity: &Identity) -> bool {
    let prefix = format!("{}-", identity.permit.assistant_session_id);
    let suffix = format!("-0-{}", &executed.plan_hash[..8]);
    let Some(timestamp) = operation
        .strip_prefix(&prefix)
        .and_then(|s| s.strip_suffix(&suffix))
    else {
        return false;
    };
    !timestamp.is_empty()
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && timestamp
            .parse::<u64>()
            .is_ok_and(|n| n > 0 && n.to_string() == timestamp)
}

fn scope_from_result(
    result: &Value,
    frozen: &FrozenPlan,
    executed: &FrozenPlan,
    captured: &Value,
    prepared: bool,
) -> Option<VerifiedWorkspaceScope> {
    if result["status"] != "succeeded"
        || result["completed_steps"] != 1
        || result["unexecuted_steps"] != 0
        || result.get("ledger_error").is_some()
        || result.get("error").is_some()
        || frozen.disposition != Disposition::Execute
        || frozen.requires_confirmation
        || frozen.actions.len() != 1
        || !ordinary("switch_tag")
        || !valid_frozen_hash(frozen)
    {
        return None;
    }
    let action = &frozen.actions[0];
    if action.name != "switch_tag" || !exact_keys(&action.arguments, &["monitor_token", "tag_mask"])
    {
        return None;
    }
    let identity = identity(captured)?;
    if frozen.wm_instance_id != identity.wm {
        return None;
    }
    let token = nonempty(&action.arguments["monitor_token"])?;
    let m = monitor(captured, token)?;
    let mask = action.arguments["tag_mask"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= u32::MAX as u64 && v.count_ones() == 1)?;
    let tag = mask.trailing_zeros() + 1;
    let count = tag_count(captured)?;
    if u64::from(tag) > count {
        return None;
    }
    let steps = result["steps"].as_array().filter(|s| s.len() == 1)?;
    let step = &steps[0];
    let operation = nonempty(&step["operation_id"])?;
    if prepared && !prepared_operation_matches(operation, executed, &identity) {
        return None;
    }
    let receipt = &step["result"];
    if step["name"] != "switch_tag"
        || step["status"] != "succeeded"
        || step["dispatched"] != true
        || receipt["status"] != "succeeded"
        || receipt["operation_id"] != operation
        || step.get("error").is_some()
        || receipt.get("error").is_some()
        || !exact_keys(&receipt["verified"], &["monitor_token", "visible_tag_mask"])
        || receipt["verified"]["monitor_token"] != token
        || receipt["verified"]["visible_tag_mask"].as_u64() != Some(mask)
    {
        return None;
    }
    Some(VerifiedWorkspaceScope {
        identity,
        monitor_token: token.into(),
        monitor_ref: nonempty(&m["monitor_ref"])?.into(),
        tag,
        tag_count: count,
        captured_at: captured["observed_monotonic_ns"]
            .as_u64()
            .filter(|n| *n > 0)?,
        _executed_hash: executed.plan_hash.clone(),
        _operation_id: operation.into(),
    })
}
fn follow_up_layout(text: Option<&str>) -> Option<String> {
    let text = text?;
    if text.chars().any(char::is_control) || text.len() > 256 {
        return None;
    }
    let text = text.trim();
    let suffix = text
        .strip_prefix("再换成")
        .or_else(|| text.strip_prefix("再改成"))?;
    // Parse just a layout value using the ordinary grammar. Do not inherit
    // ordinary correction or chain productions after the follow-up prefix.
    let request = format!("换成{suffix}");
    match parse_owned(Some(&request))? {
        ParsedIntent::Single(Primitive::Layout(layout)) => Some(layout),
        _ => None,
    }
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    follow_up_layout(text).is_some()
}
fn view(context: &Value, history: &VerifiedWorkspaceScope) -> Option<String> {
    if tag_count(context)? != history.tag_count {
        return None;
    }
    let m = monitor(context, &history.monitor_token)?;
    if m["monitor_ref"] != history.monitor_ref
        || m["visible_tag_mask"].as_u64() != Some(1u64 << (history.tag - 1))
    {
        return None;
    }
    Some(jwm_eva_core::actions::normalize_layout(m["layout"].as_str()?)?.into())
}
fn available(context: &Value) -> bool {
    context["available_actions"]
        .as_array()
        .is_some_and(|names| names.iter().any(|n| n == "set_layout"))
}
pub(super) fn assess(
    text: Option<&str>,
    history: Option<&VerifiedWorkspaceScope>,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
) -> Verdict {
    let Some(layout) = follow_up_layout(text) else {
        return Verdict::Unsupported;
    };
    let Some(history) = history else {
        return Verdict::Reject("verified_workspace_history_unavailable");
    };
    if frame.0 == 0
        || frame.1 == 0
        || plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || frozen.requires_confirmation
        || plan.actions.len() != 1
        || frozen.actions.len() != 1
        || !ordinary("set_layout")
    {
        return Verdict::Reject("not_single_ordinary_layout");
    }
    if identity(captured).as_ref() != Some(&history.identity)
        || identity(fresh).as_ref() != Some(&history.identity)
        || frozen.wm_instance_id != history.identity.wm
    {
        return Verdict::Reject("verified_workspace_identity_changed");
    }
    let (Some(old_at), Some(new_at)) = (
        captured["observed_monotonic_ns"].as_u64(),
        fresh["observed_monotonic_ns"].as_u64(),
    ) else {
        return Verdict::Reject("native_observation_unavailable");
    };
    if old_at <= history.captured_at
        || new_at <= old_at
        || !available(captured)
        || !available(fresh)
    {
        return Verdict::Reject("native_observation_or_capability_unverified");
    }
    let (Some(old), Some(new)) = (view(captured, history), view(fresh, history)) else {
        return Verdict::Reject("verified_workspace_poststate_unavailable");
    };
    if old != new {
        return Verdict::Reject("verified_workspace_poststate_drift");
    }
    let action = &plan.actions[0];
    let native = &frozen.actions[0];
    if action.name != "set_layout"
        || native.name != "set_layout"
        || !exact_keys(&action.arguments, &["monitor_ref", "tag_number", "layout"])
        || !exact_keys(&native.arguments, &["monitor_token", "tag_mask", "layout"])
        || action.arguments["monitor_ref"] != history.monitor_ref
        || native.arguments["monitor_token"] != history.monitor_token
        || action.arguments["tag_number"].as_u64() != Some(u64::from(history.tag))
        || native.arguments["tag_mask"].as_u64() != Some(1u64 << (history.tag - 1))
        || action.arguments["layout"] != layout
        || native.arguments["layout"] != layout
    {
        return Verdict::Reject("verified_workspace_target_or_layout_mismatch");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return Verdict::Reject("original_plan_invalid");
    };
    if serde_json::to_value(&rebuilt).ok() != serde_json::to_value(frozen).ok() {
        return Verdict::Reject("original_frozen_changed");
    }
    Verdict::Proved(Proof {
        hash: frozen.plan_hash.clone(),
        plan_digest: digest(&serde_json::to_value(plan).unwrap()),
        frozen_digest: digest(&serde_json::to_value(frozen).unwrap()),
        frame,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(at: u64, mask: u64) -> Value {
        json!({"wm_instance_id":"native","wm_pid":100,"wm_start_ticks":"1234","wake_generation":0,"assistant_permit":{"assistant_session_id":"private","generation":1},"connection_epoch":3,"observed_monotonic_ns":at,"session_unlocked":true,"tag_count":9,"available_actions":["switch_tag","set_layout"],"selected_monitor_token":"native-m1","monitors":[{"monitor_token":"native-m1","monitor_ref":"m1","visible_tag_mask":mask,"layout":"tile","locked":false},{"monitor_token":"native-m2","monitor_ref":"m2","visible_tag_mask":1,"layout":"tile","locked":false}]})
    }
    fn plan(name: &str, args: Value, context: &Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":name,"arguments":args}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, context).unwrap();
        (p, f)
    }
    fn seed() -> (Value, FrozenPlan, Value) {
        let before = context(100, 1);
        let (_, f) = plan(
            "switch_tag",
            json!({"monitor_ref":"m1","tag_number":3}),
            &before,
        );
        let result = json!({"status":"succeeded","completed_steps":1,"unexecuted_steps":0,"steps":[{"name":"switch_tag","status":"succeeded","dispatched":true,"operation_id":"op1","result":{"status":"succeeded","operation_id":"op1","verified":{"monitor_token":"native-m1","visible_tag_mask":4}}}]});
        (result, f, before)
    }
    #[test]
    fn verified_switch_supplies_scope_even_when_current_selection_is_another_monitor() {
        let (r, f, before) = seed();
        let history = from_local_result(&r, &f, &before).unwrap();
        let mut old = context(200, 4);
        old["selected_monitor_token"] = json!("native-m2");
        let mut fresh = old.clone();
        fresh["observed_monotonic_ns"] = json!(300);
        let (p, f) = plan(
            "set_layout",
            json!({"monitor_ref":"m1","tag_number":3,"layout":"scrolling"}),
            &old,
        );
        let Verdict::Proved(proof) = assess(
            Some("再换成 scrolling"),
            Some(&history),
            &p,
            &f,
            &old,
            &fresh,
            (1, 2),
        ) else {
            panic!("verified original scope should prove");
        };
        assert!(proof.accepts(&p, &f, (1, 2)));
        assert!(!proof.accepts(&p, &f, (1, 3)));
        let mut altered = p.clone();
        altered.reason = Some("changed".into());
        assert!(!proof.accepts(&altered, &f, (1, 2)));
        let mut altered = f.clone();
        altered.plan_hash.push('x');
        assert!(!proof.accepts(&p, &altered, (1, 2)));
        let mut altered = f.clone();
        altered.actions[0].arguments["layout"] = json!("grid");
        assert!(!proof.accepts(&p, &altered, (1, 2)));
    }
    #[test]
    fn only_exact_single_native_success_mints_history() {
        let (r, f, c) = seed();
        assert!(from_local_result(&r, &f, &c).is_some());
        for status in ["failed", "unknown", "partial", "noop"] {
            let mut bad = r.clone();
            bad["status"] = json!(status);
            assert!(from_local_result(&bad, &f, &c).is_none());
        }
        for path in ["status", "dispatched", "result"] {
            let mut bad = r.clone();
            bad["steps"][0][path] = Value::Null;
            assert!(from_local_result(&bad, &f, &c).is_none());
        }
        let mut bad = r.clone();
        bad["steps"]
            .as_array_mut()
            .unwrap()
            .push(r["steps"][0].clone());
        assert!(from_local_result(&bad, &f, &c).is_none());
        for (key, value) in [
            ("monitor_token", json!("native-m2")),
            ("visible_tag_mask", json!(2)),
        ] {
            let mut bad = r.clone();
            bad["steps"][0]["result"]["verified"][key] = value;
            assert!(from_local_result(&bad, &f, &c).is_none());
        }
        for (key, value) in [
            ("wm_start_ticks", json!(1234)),
            ("assistant_permit", Value::Null),
            ("wm_instance_id", json!("other")),
        ] {
            let mut bad = c.clone();
            bad[key] = value;
            assert!(from_local_result(&r, &f, &bad).is_none());
        }
        let mut bad = f.clone();
        bad.requires_confirmation = true;
        assert!(from_local_result(&r, &bad, &c).is_none());
        let mut bad = f.clone();
        bad.actions.push(f.actions[0].clone());
        assert!(from_local_result(&r, &bad, &c).is_none());
    }
    #[test]
    fn actual_prepare_stage_keeps_payload_but_binds_receipt_to_new_hash() {
        let (mut raw, canonical, captured) = seed();
        let mut executed = canonical.clone();
        // This is the production preparation function, not a reconstruction
        // of its nonce algorithm. It performs no Store access or dispatch.
        crate::executor::prepare_applications(&mut executed, &crate::config::Config::default())
            .unwrap();
        assert_ne!(canonical.plan_hash, executed.plan_hash);
        let operation = format!(
            "private-{}-0-{}",
            crate::store::wall_ms(),
            &executed.plan_hash[..8]
        );
        raw["steps"][0]["operation_id"] = json!(operation);
        raw["steps"][0]["result"]["operation_id"] = json!(operation);
        let history = from_prepared_local_result(&raw, &canonical, &executed, &captured).unwrap();
        assert_eq!(history._executed_hash, executed.plan_hash);
        assert_eq!(history._operation_id, operation);
        assert!(from_local_result(&raw, &executed, &captured).is_none());
        assert!(from_prepared_local_result(&raw, &canonical, &canonical, &captured).is_none());
        for id in [
            "op1".to_owned(),
            format!("other-100-0-{}", &executed.plan_hash[..8]),
            format!("private-100-1-{}", &executed.plan_hash[..8]),
            format!("private-0-0-{}", &executed.plan_hash[..8]),
            format!("private-00100-0-{}", &executed.plan_hash[..8]),
            "private-100-0-wrongone".to_owned(),
        ] {
            let mut wrong = raw.clone();
            wrong["steps"][0]["operation_id"] = json!(id);
            wrong["steps"][0]["result"]["operation_id"] = json!(id);
            assert!(
                from_prepared_local_result(&wrong, &canonical, &executed, &captured).is_none(),
                "{id}"
            );
        }
        for bad_hash in ["x".repeat(64), "a".repeat(63), "A".repeat(64)] {
            let mut wrong = executed.clone();
            wrong.plan_hash = bad_hash;
            assert!(from_prepared_local_result(&raw, &canonical, &wrong, &captured).is_none());
        }
        for key in [
            "actions",
            "wm_instance_id",
            "requires_confirmation",
            "reason",
        ] {
            let mut wrong = serde_json::to_value(&executed).unwrap();
            wrong[key] = match key {
                "actions" => json!([]),
                "requires_confirmation" => json!(true),
                _ => json!("changed"),
            };
            let wrong: FrozenPlan = serde_json::from_value(wrong).unwrap();
            assert!(
                from_prepared_local_result(&raw, &canonical, &wrong, &captured).is_none(),
                "{key}"
            );
        }
        let mut wrong_canonical = canonical.clone();
        wrong_canonical.plan_hash.push('a');
        assert!(from_prepared_local_result(&raw, &wrong_canonical, &executed, &captured).is_none());
        let mut wrong_receipt = raw.clone();
        wrong_receipt["steps"][0]["result"]["operation_id"] = json!("different");
        assert!(
            from_prepared_local_result(&wrong_receipt, &canonical, &executed, &captured).is_none()
        );
        wrong_receipt = raw.clone();
        wrong_receipt["steps"][0]["result"]["verified"]["visible_tag_mask"] = json!(2);
        assert!(
            from_prepared_local_result(&wrong_receipt, &canonical, &executed, &captured).is_none()
        );
    }
    #[test]
    fn missing_history_and_changed_operation_identity_or_poststate_reject() {
        let (r, f, c) = seed();
        let h = from_local_result(&r, &f, &c).unwrap();
        let old = context(200, 4);
        let fresh = context(300, 4);
        let (p, f) = plan(
            "set_layout",
            json!({"monitor_ref":"m1","tag_number":3,"layout":"scrolling"}),
            &old,
        );
        let rejects =
            |p: &Plan, f: &FrozenPlan, c: &Value, n: &Value, h: Option<&VerifiedWorkspaceScope>| {
                assert!(matches!(
                    assess(Some("再换成 scrolling"), h, p, f, c, n, (1, 2)),
                    Verdict::Reject(_)
                ))
            };
        rejects(&p, &f, &old, &fresh, None);
        for args in [
            json!({"monitor_ref":"m1","tag_number":3,"layout":"grid"}),
            json!({"monitor_ref":"m1","tag_number":2,"layout":"scrolling"}),
            json!({"monitor_ref":"m2","tag_number":3,"layout":"scrolling"}),
        ] {
            let (a, b) = plan("set_layout", args, &old);
            rejects(&a, &b, &old, &fresh, Some(&h));
        }
        for (key, value) in [
            ("wm_pid", json!(101)),
            ("wm_start_ticks", json!("1235")),
            ("wake_generation", json!(1)),
            ("connection_epoch", json!(4)),
            (
                "assistant_permit",
                json!({"assistant_session_id":"private","generation":2}),
            ),
            ("observed_monotonic_ns", json!(200)),
        ] {
            let mut bad = fresh.clone();
            bad[key] = value;
            rejects(&p, &f, &old, &bad, Some(&h));
        }
        for (key, value) in [
            ("visible_tag_mask", json!(2)),
            ("layout", json!("grid")),
            ("monitor_ref", json!("m3")),
            ("locked", json!(true)),
        ] {
            let mut bad = fresh.clone();
            bad["monitors"][0][key] = value;
            rejects(&p, &f, &old, &bad, Some(&h));
        }
        let mut bad = f.clone();
        bad.plan_hash.push('x');
        rejects(&p, &bad, &old, &fresh, Some(&h));
    }
    #[test]
    fn followup_is_a_complete_finite_layout_request_not_generic_repeat() {
        assert!(recognizes(Some("再换成 scrolling")));
        assert!(recognizes(Some("再改成网格。")));
        for text in [
            "再换成左屏scrolling",
            "再换成 scrolling 然后关闭窗口",
            "再换成未知",
            "不要再换成 scrolling",
            "他说再换成 scrolling",
            "‘再换成 scrolling’",
            "再来一次",
            "再换成 scrolling，不对，grid",
            "标题：再换成 scrolling",
            "再切到第三个tag",
            "再改成网格，算了还是 scrolling",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
    }
}
