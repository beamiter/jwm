//! Corroborates only the original single previous-window focus candidate.
//! Caller must supply full native contexts, a completed bound review and fresh RPC.
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
fn digest(v: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(v).expect("typed plan"))
}
impl Proof {
    pub(super) fn accepts(&self, p: &Plan, f: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == f.plan_hash
            && self.candidate == digest(p)
            && self.frozen == digest(f)
    }
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    let Some(text) = text else {
        return false;
    };
    if text.len() > 256
        || text
            .chars()
            .any(|c| c.is_control() || "\"'“”‘’「」『』`?？".contains(c))
    {
        return false;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '.', '!', '！'])
        .trim()
        .to_ascii_lowercase();
    let text = text.strip_prefix('请').unwrap_or(&text).trim();
    matches!(
        text,
        "go back to the window i was just using"
            | "go back to the previous window"
            | "go back to the nearest previous window"
            | "switch back to the previous window"
            | "切回刚才那个窗口"
            | "回刚才的窗口"
            | "切回上一个窗口"
            | "回到上一个窗口"
            | "切回刚才的窗口"
    )
}
fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
fn keys(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|m| m.len() == keys.len() && keys.iter().all(|key| m.contains_key(*key)))
}
fn positive_native_ticks(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        !text.is_empty()
            && !text.starts_with('0')
            && text.bytes().all(|c| c.is_ascii_digit())
            && text.parse::<u64>().is_ok_and(|ticks| ticks > 0)
    })
}
fn identity(captured: &Value, fresh: &Value) -> bool {
    if captured["session_unlocked"] != true || fresh["session_unlocked"] != true {
        return false;
    }
    if captured["wm_pid"].as_u64().is_none_or(|pid| pid == 0)
        || captured["wm_pid"] != fresh["wm_pid"]
        || !positive_native_ticks(&captured["wm_start_ticks"])
        || captured["wm_start_ticks"] != fresh["wm_start_ticks"]
        || captured["wake_generation"].as_u64().is_none()
        || captured["wake_generation"] != fresh["wake_generation"]
    {
        return false;
    }
    if nonempty(&captured["wm_instance_id"]).is_none()
        || captured["wm_instance_id"] != fresh["wm_instance_id"]
    {
        return false;
    }
    let (Some(before), Some(after)) = (
        captured["observed_monotonic_ns"].as_u64(),
        fresh["observed_monotonic_ns"].as_u64(),
    ) else {
        return false;
    };
    if before == 0 || after <= before {
        return false;
    }
    if !captured
        .as_object()
        .is_some_and(|m| m.contains_key("assistant_permit"))
        || !fresh
            .as_object()
            .is_some_and(|m| m.contains_key("assistant_permit"))
        || captured["assistant_permit"] != fresh["assistant_permit"]
    {
        return false;
    }
    if !captured["assistant_permit"].is_null() {
        let Ok(permit) = serde_json::from_value::<jwm_eva_core::protocol::AssistantPermit>(
            captured["assistant_permit"].clone(),
        ) else {
            return false;
        };
        // Manual assistant activation increments assistant generation; wake
        // configuration increments a separate wake_generation counter.
        if permit.assistant_session_id.is_empty()
            || captured["assistant_permit"]["generation"]
                .as_u64()
                .is_none_or(|n| n == 0)
        {
            return false;
        }
    }
    if captured.get("connection_epoch") != fresh.get("connection_epoch")
        || captured
            .get("connection_epoch")
            .is_some_and(|v| v.as_u64().is_none())
    {
        return false;
    }
    captured["available_actions"].is_array()
        && captured["available_actions"] == fresh["available_actions"]
}
#[derive(Debug, PartialEq)]
struct Window {
    reference: String,
    token: String,
    monitor: String,
    tags: u64,
    raw_id: String,
    pid: Option<Value>,
}
fn window(context: &Value, wanted: &str) -> Option<Window> {
    if context["windows_complete"] != true {
        return None;
    }
    let rows = context["windows"].as_array().filter(|r| r.len() <= 128)?;
    let mut matching = rows.iter().filter(|w| w["window_ref"] == wanted);
    let row = matching.next()?;
    let token = nonempty(&row["target_token"])?;
    if matching.next().is_some()
        || row["closed"] == true
        || row["live"] == false
        || rows.iter().filter(|w| w["target_token"] == token).count() != 1
    {
        return None;
    }
    if row
        .get("pid")
        .is_some_and(|p| !p.is_null() && p.as_u64().is_none_or(|n| n == 0))
    {
        return None;
    }
    let monitor = nonempty(&row["monitor_token"])?;
    let monitors = context["monitors"]
        .as_array()
        .filter(|m| !m.is_empty() && m.len() <= 32)?;
    let mut matching = monitors.iter().filter(|m| m["monitor_token"] == monitor);
    let m = matching.next()?;
    let mref = nonempty(&m["monitor_ref"])?;
    if matching.next().is_some()
        || m["locked"] != false
        || monitors.iter().filter(|m| m["monitor_ref"] == mref).count() != 1
    {
        return None;
    }
    let count = context["tag_count"]
        .as_u64()
        .filter(|n| (1..=32).contains(n))?;
    let tags = row["tag_mask"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= u32::MAX as u64 && n & !((1u64 << count) - 1) == 0)?;
    Some(Window {
        reference: wanted.into(),
        token: token.into(),
        monitor: monitor.into(),
        tags,
        raw_id: nonempty(&row["id"])?.into(),
        pid: row.get("pid").cloned(),
    })
}

fn previous<'a>(c: &'a Value, current: &str) -> Option<(&'a str, u64)> {
    let observed = c["observed_monotonic_ns"].as_u64()?;
    let events = c["focus_history"]
        .as_array()
        .filter(|h| !h.is_empty() && h.len() <= 128)?;
    let mut last = 0;
    for event in events {
        if !keys(event, &["at_monotonic_ns", "focus_token"]) {
            return None;
        }
        let at = event["at_monotonic_ns"].as_u64()?;
        if at == 0 || at <= last || at > observed {
            return None;
        }
        last = at;
        if !event["focus_token"].is_null() && nonempty(&event["focus_token"]).is_none() {
            return None;
        }
    }
    if events.last()?["focus_token"] != current {
        return None;
    }
    for event in events.iter().rev() {
        if event["focus_token"] == current {
            continue;
        }
        // A null or dead nearest event is a barrier, never an invitation to search older.
        return Some((
            nonempty(&event["focus_token"])?,
            event["at_monotonic_ns"].as_u64()?,
        ));
    }
    None
}
fn full(c: &Value) -> bool {
    c["windows_complete"] == true
        && !["monitors_complete", "complete", "native_complete"]
            .iter()
            .any(|k| c.get(*k).is_some_and(|v| v != true))
        && ![
            "truncated",
            "windows_truncated",
            "monitors_truncated",
            "focus_history_truncated",
        ]
        .iter()
        .any(|k| c.get(*k).is_some_and(|v| v != false))
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
        || !identity(captured, fresh)
        || !full(captured)
        || !full(fresh)
        || captured["wm_instance_id"] != frozen.wm_instance_id
    {
        return Verdict::Reject("unproven_previous_focus_frame");
    }
    let Some(current_ref) = nonempty(&captured["speech_focus_ref"]) else {
        return Verdict::Reject("missing_current_focus");
    };
    let Some(current) = window(captured, current_ref) else {
        return Verdict::Reject("ambiguous_current_focus");
    };
    if captured["speech_focus_uncertain"] != false
        || captured["focus_token"] != current.token
        || fresh["focus_token"] != current.token
        || window(fresh, current_ref).as_ref() != Some(&current)
        || fresh
            .get("speech_focus_ref")
            .is_some_and(|v| !v.is_null() && v != current_ref)
        || fresh
            .get("speech_focus_uncertain")
            .is_some_and(|v| v != false)
    {
        return Verdict::Reject("current_focus_changed");
    }
    let Some((token, at)) = previous(captured, &current.token) else {
        return Verdict::Reject("no_distinct_previous_focus");
    };
    if previous(fresh, &current.token) != Some((token, at))
        || fresh["focus_history"].as_array().unwrap().iter().any(|e| {
            e["at_monotonic_ns"].as_u64().unwrap()
                > captured["observed_monotonic_ns"].as_u64().unwrap()
                && e["focus_token"] != current.token
        })
    {
        return Verdict::Reject("focus_history_changed");
    }
    let Some(rows) = captured["windows"].as_array() else {
        return Verdict::Reject("missing_windows");
    };
    let Some(reference) = rows
        .iter()
        .find(|w| w["target_token"] == token)
        .and_then(|w| nonempty(&w["window_ref"]))
    else {
        return Verdict::Reject("previous_instance_gone");
    };
    let Some(target) = window(captured, reference) else {
        return Verdict::Reject("ambiguous_previous_instance");
    };
    if target.token != token
        || target.token == current.token
        || target.reference == current.reference
        || window(fresh, reference).as_ref() != Some(&target)
    {
        return Verdict::Reject("previous_instance_changed");
    }
    if plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || plan.actions.len() != 1
        || frozen.actions.len() != 1
        || frozen.requires_confirmation
    {
        return Verdict::Reject("not_single_previous_focus");
    }
    let a = &plan.actions[0];
    let n = &frozen.actions[0];
    if a.name != "focus_window"
        || n.name != "focus_window"
        || !keys(&a.arguments, &["window_ref"])
        || a.arguments["window_ref"] != reference
        || !keys(&n.arguments, &["window_token"])
        || n.arguments["window_token"] != token
    {
        return Verdict::Reject("wrong_previous_focus_candidate");
    }
    if !captured["available_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "focus_window")
        || jwm_eva_core::actions::semantic_actions()
            .iter()
            .find(|s| s.canonical_name == "focus_window")
            .is_none_or(|s| {
                s.target_resolver != "window"
                    || s.risk_class != "reversible"
                    || s.support_status != "implemented"
            })
    {
        return Verdict::Reject("previous_focus_unavailable");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return Verdict::Reject("invalid_original_previous_focus");
    };
    if digest(&rebuilt) != digest(frozen) {
        return Verdict::Reject("original_previous_focus_frozen_changed");
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
    const TEXT: &str = "go back to the window I was just using";
    fn context() -> Value {
        json!({"wm_instance_id":"wm","wm_pid":42,"wm_start_ticks":"123","wake_generation":0,"assistant_permit":null,"observed_monotonic_ns":100,"session_unlocked":true,"windows_complete":true,"tag_count":9,"speech_focus_ref":"w1","speech_focus_uncertain":false,"focus_token":"token1","available_actions":["focus_window"],"monitors":[{"monitor_ref":"m1","monitor_token":"monitor1","locked":false}],"windows":[{"window_ref":"w1","target_token":"token1","id":"41","pid":12,"monitor_token":"monitor1","tag_mask":1},{"window_ref":"w2","target_token":"token2","id":"42","pid":13,"monitor_token":"monitor1","tag_mask":2,"visible":false},{"window_ref":"w3","target_token":"token3","id":"43","monitor_token":"monitor1","tag_mask":1}],"focus_history":[{"at_monotonic_ns":1,"focus_token":"token3"},{"at_monotonic_ns":2,"focus_token":"token2"},{"at_monotonic_ns":3,"focus_token":"token1"}]})
    }
    fn fresh(c: &Value) -> Value {
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(200);
        n
    }
    fn candidate(c: &Value, reference: &str) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"focus_window","arguments":{"window_ref":reference}}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, c).unwrap();
        (p, f)
    }
    fn reject(c: &Value, n: &Value, p: &Plan, f: &FrozenPlan) {
        assert!(matches!(
            assess(Some(TEXT), p, f, c, n, (1, 1)),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn original_190_preserves_hidden_nearest_candidate_and_binds_all_inputs() {
        let c = context();
        let n = fresh(&c);
        let (p, f) = candidate(&c, "w2");
        let Verdict::Proved(proof) = assess(Some(TEXT), &p, &f, &c, &n, (1, 1)) else {
            panic!("original previous focus must prove");
        };
        assert!(proof.accepts(&p, &f, (1, 1)));
        assert!(!proof.accepts(&p, &f, (1, 2)));
        let mut hash = f.clone();
        hash.plan_hash.push('x');
        reject(&c, &n, &p, &hash);
        assert!(!proof.accepts(&p, &hash, (1, 1)));
        let mut extra = p.clone();
        extra.actions.push(p.actions[0].clone());
        reject(&c, &n, &extra, &f);
        let (wrong, wf) = candidate(&c, "w1");
        reject(&c, &n, &wrong, &wf);
        let (older, of) = candidate(&c, "w3");
        reject(&c, &n, &older, &of);
    }
    #[test]
    fn nearest_event_barriers_ordering_replay_and_focus_jump_reject() {
        let c = context();
        let (p, f) = candidate(&c, "w2");
        for history in [
            json!([]),
            json!([{"at_monotonic_ns":3,"focus_token":"token1"}]),
            json!([{"at_monotonic_ns":2,"focus_token":null},{"at_monotonic_ns":3,"focus_token":"token1"}]),
            json!([{"at_monotonic_ns":2,"focus_token":"dead"},{"at_monotonic_ns":3,"focus_token":"token1"}]),
            json!([{"at_monotonic_ns":3,"focus_token":"token2"},{"at_monotonic_ns":2,"focus_token":"token1"}]),
            json!([{"at_monotonic_ns":2,"focus_token":"token2"},{"at_monotonic_ns":2,"focus_token":"token1"}]),
            json!([{"at_monotonic_ns":2,"focus_token":"token2"},{"at_monotonic_ns":101,"focus_token":"token1"}]),
        ] {
            let mut bad = c.clone();
            bad["focus_history"] = history;
            reject(&bad, &fresh(&bad), &p, &f);
        }
        let mut jump = fresh(&c);
        jump["focus_history"].as_array_mut().unwrap().extend([
            json!({"at_monotonic_ns":150,"focus_token":"token2"}),
            json!({"at_monotonic_ns":160,"focus_token":"token1"}),
        ]);
        reject(&c, &jump, &p, &f);
        let mut repeat = c.clone();
        repeat["focus_history"]
            .as_array_mut()
            .unwrap()
            .push(json!({"at_monotonic_ns":4,"focus_token":"token1"}));
        assert!(matches!(
            assess(Some(TEXT), &p, &f, &repeat, &fresh(&repeat), (1, 1)),
            Verdict::Proved(_)
        ));
    }
    #[test]
    fn native_incarnation_identity_completeness_and_locks_are_pinned() {
        let c = context();
        let (p, f) = candidate(&c, "w2");
        for (key, value) in [
            ("id", json!("new")),
            ("target_token", json!("new")),
            ("pid", json!(99)),
            ("monitor_token", json!("other")),
            ("tag_mask", json!(4)),
            ("closed", json!(true)),
            ("live", json!(false)),
        ] {
            let mut n = fresh(&c);
            n["windows"][1][key] = value;
            reject(&c, &n, &p, &f);
        }
        for (key, value) in [
            ("wm_pid", json!(43)),
            ("wm_instance_id", json!("other")),
            ("wm_start_ticks", json!("124")),
            ("wake_generation", json!(1)),
            ("assistant_permit", json!({"generation":1})),
            ("observed_monotonic_ns", json!(100)),
            ("focus_token", json!("token2")),
            ("session_unlocked", json!(false)),
            ("windows_complete", json!(false)),
            ("truncated", json!(true)),
            ("connection_epoch", json!("bad")),
        ] {
            let mut n = fresh(&c);
            n[key] = value;
            reject(&c, &n, &p, &f);
        }
        let mut n = fresh(&c);
        n["monitors"][0]["locked"] = json!(true);
        reject(&c, &n, &p, &f);
        for key in [
            "assistant_permit",
            "wm_pid",
            "wm_start_ticks",
            "wake_generation",
        ] {
            let mut a = c.clone();
            a.as_object_mut().unwrap().remove(key);
            reject(&a, &fresh(&a), &p, &f);
        }
        for key in ["target_token", "window_ref"] {
            let mut a = c.clone();
            a["windows"][2][key] = a["windows"][1][key].clone();
            reject(&a, &fresh(&a), &p, &f);
        }
        let mut a = c.clone();
        a["windows"][1]["closed"] = json!(true);
        reject(&a, &fresh(&a), &p, &f);
    }
    #[test]
    fn only_complete_unambiguous_previous_focus_commands() {
        for t in [
            "切回刚才那个窗口",
            "回刚才的窗口",
            "go back to the nearest previous window",
            TEXT,
        ] {
            assert!(recognizes(Some(t)));
        }
        for t in [
            "focus current window",
            "go back",
            "focus Firefox",
            "如果可以，切回刚才那个窗口",
            "切回刚才那个窗口并关闭它",
            "切回刚才那个窗口？",
            "‘切回刚才那个窗口’",
            "标题是切回刚才那个窗口",
            "回到上一个音频",
            "切回刚才那个窗口和终端",
            "",
        ] {
            assert!(!recognizes(Some(t)), "{t}");
        }
        assert!(!recognizes(None));
    }
}
