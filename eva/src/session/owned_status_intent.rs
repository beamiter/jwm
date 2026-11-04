//! Complete owned recording-status questions can corroborate only an existing
//! read-only query. No state answer, action, permission or reference is built.
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use serde_json::Value;
#[derive(Debug)]
pub(super) enum Verdict {
    Unsupported,
    Reject(&'static str),
    Proved(Proof),
}
#[derive(Debug)]
pub(super) struct Proof {
    candidate: String,
    frozen: String,
    hash: String,
    frame: (u64, u64),
}
fn digest(value: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(value).expect("typed plan"))
}
impl Proof {
    pub(super) fn accepts(&self, plan: &Plan, frozen: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == frozen.plan_hash
            && self.candidate == digest(plan)
            && self.frozen == digest(frozen)
    }
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    let Some(text) = text else {
        return false;
    };
    if text.len() > 256 || text.chars().any(char::is_control) {
        return false;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '.', '?', '？'])
        .trim()
        .to_ascii_lowercase();
    matches!(
        text.as_str(),
        "录屏还在继续吗"
            | "录屏还在进行吗"
            | "屏幕录制还在继续吗"
            | "是否还在录屏"
            | "is screen recording still active"
    )
}
pub(super) fn native_frame_identity(c: &Value, f: &Value) -> bool {
    if c["session_unlocked"] != true
        || f["session_unlocked"] != true
        || c["wm_instance_id"].as_str().is_none_or(str::is_empty)
        || c["wm_pid"].as_u64().is_none_or(|n| n == 0)
        || c["wake_generation"].as_u64().is_none()
    {
        return false;
    }
    let ticks = c["wm_start_ticks"]
        .as_str()
        .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == s));
    if !ticks {
        return false;
    }
    let Some(permit) = c.get("assistant_permit") else {
        return false;
    };
    if !permit.is_null()
        && serde_json::from_value::<jwm_eva_core::protocol::AssistantPermit>(permit.clone())
            .is_err()
    {
        return false;
    }
    if !permit.is_null()
        && (permit["generation"].as_u64().is_none_or(|n| n == 0)
            || permit["assistant_session_id"]
                .as_str()
                .is_none_or(str::is_empty))
    {
        return false;
    }
    for key in [
        "wm_instance_id",
        "wm_pid",
        "wm_start_ticks",
        "wake_generation",
        "assistant_permit",
        "connection_epoch",
    ] {
        if c.get(key) != f.get(key) {
            return false;
        }
    }
    if c.get("connection_epoch")
        .is_some_and(|v| v.as_u64().is_none())
    {
        return false;
    }
    c["observed_monotonic_ns"].as_u64().is_some_and(|old| {
        old > 0
            && f["observed_monotonic_ns"]
                .as_u64()
                .is_some_and(|new| new > old)
    }) && c["available_actions"].is_array()
        && c["available_actions"] == f["available_actions"]
}
pub(super) fn assess(
    text: Option<&str>,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
) -> Verdict {
    if !recognizes(text) {
        return Verdict::Unsupported;
    }
    if frame.0 == 0
        || frame.1 == 0
        || !native_frame_identity(captured, fresh)
        || captured["wm_instance_id"] != frozen.wm_instance_id
    {
        return Verdict::Reject("unproven_native_readonly_frame");
    }
    if plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || plan.actions.len() != 1
        || frozen.actions.len() != 1
        || frozen.requires_confirmation
    {
        return Verdict::Reject("not_single_readonly_query");
    }
    let action = &plan.actions[0];
    let native = &frozen.actions[0];
    if action.name != "query_state"
        || native.name != "query_state"
        || action
            .arguments
            .as_object()
            .is_none_or(|a| a.len() != 1 || !a.contains_key("query"))
        || native.arguments != action.arguments
        || !matches!(
            action.arguments["query"].as_str(),
            Some("get_recording_status")
        )
    {
        return Verdict::Reject("recording_question_cannot_authorize_another_effect_or_query");
    }
    if !captured["available_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "query_state")
        || jwm_eva_core::actions::semantic_actions()
            .iter()
            .find(|s| s.canonical_name == "query_state")
            .is_none_or(|s| s.target_resolver != "read_only" || s.risk_class != "ordinary")
    {
        return Verdict::Reject("readonly_query_capability_unavailable");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return Verdict::Reject("invalid_original_readonly_plan");
    };
    if digest(&rebuilt) != digest(frozen) {
        return Verdict::Reject("original_readonly_frozen_changed");
    }
    Verdict::Proved(Proof {
        candidate: digest(plan),
        frozen: digest(frozen),
        hash: frozen.plan_hash.clone(),
        frame,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn context() -> Value {
        json!({"wm_instance_id":"private","wm_pid":1,"wm_start_ticks":"2","wake_generation":0,"assistant_permit":null,"session_unlocked":true,"observed_monotonic_ns":100,"tag_count":9,"available_actions":["query_state","stop_recording"],"monitors":[],"windows":[]})
    }
    fn plan(c: &Value, name: &str, args: Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":name,"arguments":args}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, c).unwrap();
        (p, f)
    }
    #[test]
    fn complete_question_keeps_original_read_query_and_never_claims_answer() {
        let c = context();
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(101);
        {
            let q = "get_recording_status";
            let (p, f) = plan(&c, "query_state", json!({"query":q}));
            let Verdict::Proved(proof) = assess(Some("录屏还在继续吗"), &p, &f, &c, &n, (1, 1))
            else {
                panic!("declared readonly query must be proved")
            };
            assert!(proof.accepts(&p, &f, (1, 1)));
            assert!(!proof.accepts(&p, &f, (1, 2)));
            let mut changed = f.clone();
            changed.actions[0].arguments["query"] = json!("get_windows");
            assert!(!proof.accepts(&p, &changed, (1, 1)));
        }
    }
    #[test]
    fn query_intent_never_launders_mutation_other_read_missing_state_or_forged_binding() {
        let c = context();
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(101);
        for (name, args) in [
            ("stop_recording", json!({})),
            ("query_state", json!({"query":"get_windows"})),
            ("query_state", json!({"query":"get_capture_status"})),
        ] {
            let (p, f) = plan(&c, name, args);
            assert!(matches!(
                assess(Some("录屏还在继续吗"), &p, &f, &c, &n, (1, 1)),
                Verdict::Reject(_)
            ));
        }
        let (p, f) = plan(&c, "query_state", json!({"query":"get_recording_status"}));
        for key in [
            "assistant_permit",
            "wm_start_ticks",
            "wm_pid",
            "observed_monotonic_ns",
        ] {
            let mut bad = n.clone();
            bad[key] = if key == "assistant_permit" {
                json!({"generation":0})
            } else {
                Value::Null
            };
            assert!(matches!(
                assess(Some("录屏还在继续吗"), &p, &f, &c, &bad, (1, 1)),
                Verdict::Reject(_)
            ));
        }
        for text in [
            "开始录屏",
            "停止录屏",
            "录屏还在继续吗，然后退出",
            "标题写着录屏还在继续吗",
            "录屏大一点",
            "",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
    }
}
