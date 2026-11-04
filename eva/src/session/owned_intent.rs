//! Independent complete owned intent evidence, never an action constructor.
//! The caller obtains `fresh` from a read-only native RPC after a correlated,
//! structurally valid review. No provider/transport failure can mint a proof.
use super::intent_syntax::{ParsedIntent, Primitive, Rational, parse_owned};
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
    hash: String,
    candidate: String,
    frozen: String,
    frame: (u64, u64),
    single: bool,
}
impl Proof {
    pub(super) fn accepts(&self, plan: &Plan, frozen: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == frozen.plan_hash
            && self.frozen == jwm_eva_core::protocol::digest(&serde_json::to_value(frozen).unwrap())
            && self.candidate
                == jwm_eva_core::protocol::digest(&serde_json::to_value(plan).unwrap())
    }
    pub(super) fn can_correct_model_domain(&self) -> bool {
        self.single
    }
}

pub(super) fn recognizes(text: Option<&str>) -> bool {
    parse_owned(text).is_some()
}

#[derive(Debug, PartialEq, Eq)]
struct Profile {
    wm: String,
    wake: u64,
    token: String,
    reference: String,
    active: Option<u32>,
    visible: u64,
    layout: Option<String>,
    tag_count: u64,
}
fn positive_native_ticks(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        !text.is_empty()
            && text.len() <= 20
            && text.bytes().all(|byte| byte.is_ascii_digit())
            && text
                .parse::<u64>()
                .is_ok_and(|ticks| ticks > 0 && ticks.to_string() == text)
    })
}
fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 512 && !text.chars().any(char::is_control))
}
fn profile(context: &Value, needs_current_profile: bool) -> Option<Profile> {
    if context["session_unlocked"] != true {
        return None;
    }
    if context
        .get("activation_monitor_token")
        .is_some_and(|token| !token.is_null() && token != &context["selected_monitor_token"])
    {
        return None;
    }
    let token = context
        .get("activation_monitor_token")
        .filter(|v| !v.is_null())
        .unwrap_or(&context["selected_monitor_token"]);
    let token = nonempty(token)?;
    let monitors = context["monitors"]
        .as_array()
        .filter(|m| !m.is_empty() && m.len() <= 32)?;
    let mut matching = monitors.iter().filter(|m| m["monitor_token"] == token);
    let monitor = matching.next()?;
    let reference = nonempty(&monitor["monitor_ref"])?;
    if matching.next().is_some()
        || monitor["locked"] != false
        || monitors
            .iter()
            .filter(|m| m["monitor_ref"] == reference)
            .count()
            != 1
    {
        return None;
    }
    let mask = monitor["visible_tag_mask"]
        .as_u64()
        .filter(|mask| *mask > 0 && *mask <= u32::MAX as u64)?;
    let tag_count = context["tag_count"]
        .as_u64()
        .filter(|n| (1..=32).contains(n))?;
    if mask & !((1u64 << tag_count) - 1) != 0 {
        return None;
    }
    let active = (mask.count_ones() == 1).then_some(mask.trailing_zeros() + 1);
    let layout = monitor["layout"]
        .as_str()
        .and_then(jwm_eva_core::actions::normalize_layout)
        .map(str::to_owned);
    if needs_current_profile && (active.is_none() || layout.is_none()) {
        return None;
    }
    Some(Profile {
        wm: nonempty(&context["wm_instance_id"])?.into(),
        wake: context["wake_generation"].as_u64()?,
        token: token.into(),
        reference: reference.into(),
        active,
        visible: mask,
        layout,
        tag_count,
    })
}
fn exact_keys(value: &Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == keys.len() && keys.iter().all(|key| map.contains_key(*key)))
}
fn decimal_number(value: &Value) -> Option<Rational> {
    let text = value.as_number()?.to_string();
    let (mantissa, exponent) = if let Some((left, right)) = text.split_once(['e', 'E']) {
        (left, right.parse::<i32>().ok()?)
    } else {
        (text.as_str(), 0)
    };
    if !(-18..=18).contains(&exponent) || mantissa.starts_with('-') {
        return None;
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let mut num = digits.parse::<u64>().ok()?;
    let mut den = 10u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    if exponent >= 0 {
        num = num.checked_mul(10u64.checked_pow(exponent as u32)?)?;
    } else {
        den = den.checked_mul(10u64.checked_pow((-exponent) as u32)?)?;
    }
    Some(Rational { num, den })
}
fn same_ratio(value: &Value, expected: Rational) -> bool {
    decimal_number(value).is_some_and(|actual| {
        u128::from(actual.num) * u128::from(expected.den)
            == u128::from(expected.num) * u128::from(actual.den)
    })
}
fn native_available(context: &Value, name: &str) -> bool {
    context["available_actions"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item == name))
}
fn ordinary(name: &str) -> bool {
    jwm_eva_core::actions::semantic_actions()
        .iter()
        .any(|spec| {
            spec.canonical_name == name
                && matches!(
                    name,
                    "set_layout"
                        | "setgaps"
                        | "setnmaster"
                        | "setmfact"
                        | "switch_tag"
                        | "move_window_to_tag"
                )
                && matches!(spec.risk_class.as_str(), "ordinary" | "reversible")
        })
}
fn scoped_action(
    plan: &Plan,
    frozen: &FrozenPlan,
    index: usize,
    name: &str,
    tag: u32,
    p: &Profile,
    value: Option<(&str, &Value)>,
) -> bool {
    let (Some(action), Some(native)) = (plan.actions.get(index), frozen.actions.get(index)) else {
        return false;
    };
    if action.name != name || native.name != name || u64::from(tag) > p.tag_count || tag == 0 {
        return false;
    }
    let semantic_keys = if let Some((key, _)) = value {
        vec!["monitor_ref", "tag_number", key]
    } else {
        vec!["monitor_ref", "tag_number"]
    };
    let native_keys = if let Some((key, _)) = value {
        vec!["monitor_token", "tag_mask", key]
    } else {
        vec!["monitor_token", "tag_mask"]
    };
    exact_keys(&action.arguments, &semantic_keys)
        && exact_keys(&native.arguments, &native_keys)
        && action.arguments["monitor_ref"] == p.reference
        && action.arguments["tag_number"].as_u64() == Some(u64::from(tag))
        && native.arguments["monitor_token"] == p.token
        && native.arguments["tag_mask"].as_u64() == Some(1u64 << (tag - 1))
        && value.is_none_or(|(key, expected)| {
            &action.arguments[key] == expected && &native.arguments[key] == expected
        })
}
fn relative_target(profile: &Profile, delta: i8) -> Option<u32> {
    let active = profile.active?;
    let target = match delta {
        1 => active.checked_add(1)?,
        -1 => active.checked_sub(1)?,
        _ => return None,
    };
    (target > 0 && u64::from(target) <= profile.tag_count).then_some(target)
}
fn single_value(primitive: &Primitive, plan: &Plan, frozen: &FrozenPlan, p: &Profile) -> bool {
    if plan.actions.len() != 1 || frozen.actions.len() != 1 {
        return false;
    }
    match primitive {
        Primitive::Layout(layout) => scoped_action(
            plan,
            frozen,
            0,
            "set_layout",
            p.active.unwrap_or(0),
            p,
            Some(("layout", &Value::String(layout.clone()))),
        ),
        Primitive::SwitchTag(tag) => scoped_action(plan, frozen, 0, "switch_tag", *tag, p, None),
        Primitive::RelativeSwitch(delta) => relative_target(p, *delta)
            .is_some_and(|tag| scoped_action(plan, frozen, 0, "switch_tag", tag, p, None)),
        Primitive::Gaps(number) => scoped_action(
            plan,
            frozen,
            0,
            "setgaps",
            p.active.unwrap_or(0),
            p,
            Some(("value", &Value::from(*number))),
        ),
        Primitive::MasterCount(number) => {
            p.layout.as_deref() != Some("scrolling")
                && scoped_action(
                    plan,
                    frozen,
                    0,
                    "setnmaster",
                    p.active.unwrap_or(0),
                    p,
                    Some(("value", &Value::from(*number))),
                )
        }
        Primitive::MasterFactor(ratio) => {
            p.layout.as_deref() != Some("scrolling")
                && scoped_action(
                    plan,
                    frozen,
                    0,
                    "setmfact",
                    p.active.unwrap_or(0),
                    p,
                    Some(("value", &plan.actions[0].arguments["value"])),
                )
                && same_ratio(&plan.actions[0].arguments["value"], *ratio)
                && same_ratio(&frozen.actions[0].arguments["value"], *ratio)
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
struct WindowAnchor {
    reference: String,
    token: String,
    tags: u64,
    raw_id: Option<Value>,
}
fn current_window(context: &Value, p: &Profile, captured_anchor: bool) -> Option<WindowAnchor> {
    if context["speech_focus_uncertain"] == true
        || context["windows_complete"] != true
        || (captured_anchor && context["speech_focus_uncertain"] != false)
    {
        return None;
    }
    let token = nonempty(&context["focus_token"])?;
    let windows = context["windows"].as_array().filter(|w| w.len() <= 128)?;
    let mut matching = windows.iter().filter(|w| w["target_token"] == token);
    let window = matching.next()?;
    let reference = nonempty(&window["window_ref"])?;
    if matching.next().is_some()
        || windows
            .iter()
            .filter(|w| w["window_ref"] == reference)
            .count()
            != 1
        || window["closed"] == true
        || window["live"] == false
        || window["monitor_token"] != p.token
    {
        return None;
    }
    if captured_anchor && context["speech_focus_ref"] != reference {
        return None;
    }
    if context
        .get("speech_focus_ref")
        .is_some_and(|r| !r.is_null() && r != reference)
    {
        return None;
    }
    let tags = window["tag_mask"].as_u64().filter(|tags| {
        *tags > 0 && *tags <= u32::MAX as u64 && tags & !((1u64 << p.tag_count) - 1) == 0
    })?;
    Some(WindowAnchor {
        reference: reference.into(),
        token: token.into(),
        tags,
        raw_id: window.get("id").cloned(),
    })
}
fn chain_value(
    intent: &ParsedIntent,
    plan: &Plan,
    frozen: &FrozenPlan,
    p: &Profile,
    captured: &Value,
    fresh: &Value,
) -> bool {
    if plan.actions.len() != 2 || frozen.actions.len() != 2 {
        return false;
    }
    match intent {
        ParsedIntent::SwitchThenLayout { tag, layout } => {
            scoped_action(plan, frozen, 0, "switch_tag", *tag, p, None)
                && scoped_action(
                    plan,
                    frozen,
                    1,
                    "set_layout",
                    *tag,
                    p,
                    Some(("layout", &Value::String(layout.clone()))),
                )
        }
        ParsedIntent::MoveCurrentThenFollow { tag } => {
            let Some(anchor) = current_window(captured, p, true) else {
                return false;
            };
            if current_window(fresh, p, false).as_ref() != Some(&anchor)
                || *tag == 0
                || u64::from(*tag) > p.tag_count
            {
                return false;
            }
            let a = &plan.actions[0];
            let n = &frozen.actions[0];
            a.name == "move_window_to_tag"
                && n.name == a.name
                && exact_keys(&a.arguments, &["window_ref", "tag_number"])
                && exact_keys(&n.arguments, &["window_token", "tag_mask"])
                && a.arguments["window_ref"] == anchor.reference
                && a.arguments["tag_number"].as_u64() == Some(u64::from(*tag))
                && n.arguments["window_token"] == anchor.token
                && n.arguments["tag_mask"].as_u64() == Some(1u64 << (*tag - 1))
                && scoped_action(plan, frozen, 1, "switch_tag", *tag, p, None)
        }
        _ => false,
    }
}

/// Chain evidence cannot excuse an unused contradictory target annotation.
/// Generic matching alone does not consume container fields on tag moves.
pub(super) fn exact_chain_targets(review: &Value, plan: &Plan, context: &Value) -> bool {
    let Some(rows) = review["steps"]
        .as_array()
        .filter(|rows| rows.len() == plan.actions.len())
    else {
        return false;
    };
    for (row, action) in rows.iter().zip(&plan.actions) {
        let window = action.arguments["window_ref"].as_str();
        let expected_windows = window
            .map(|reference| vec![Value::from(reference)])
            .unwrap_or_default();
        if row["window_refs"].as_array() != Some(&expected_windows) {
            return false;
        }
        for key in ["session_refs", "device_ids", "application_ids"] {
            if row[key].as_array().is_none_or(|refs| !refs.is_empty()) {
                return false;
            }
        }
        let Some(tag) = action.arguments["tag_number"].as_u64() else {
            return false;
        };
        if row["tag_numbers"] != serde_json::json!([tag]) {
            return false;
        }
        let Some(monitors) = row["monitor_refs"].as_array() else {
            return false;
        };
        if let Some(reference) = action.arguments["monitor_ref"].as_str() {
            if monitors != &vec![Value::from(reference)] {
                return false;
            }
        } else if !monitors.is_empty() {
            if action.name != "move_window_to_tag" || monitors.len() != 1 {
                return false;
            }
            let Some(window) = context["windows"].as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| item["window_ref"].as_str() == window)
            }) else {
                return false;
            };
            let Some(container) = context["monitors"].as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| item["monitor_token"] == window["monitor_token"])
            }) else {
                return false;
            };
            if monitors[0] != container["monitor_ref"] {
                return false;
            }
        }
    }
    true
}

/// Three states are intentional. A recognized request that disagrees with the
/// original op/value/target or native state never falls back to scope-only NLU.
pub(super) fn assess(
    text: Option<&str>,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
) -> Verdict {
    let Some(parsed) = parse_owned(text) else {
        return Verdict::Unsupported;
    };
    let reject = |reason| Verdict::Reject(reason);
    if frame.0 == 0
        || frame.1 == 0
        || plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || frozen.requires_confirmation
        || plan.actions.iter().any(|a| !ordinary(&a.name))
    {
        return reject("not_ordinary_original_execute");
    }
    let needs_current_profile = matches!(&parsed, ParsedIntent::Single(primitive) if !matches!(primitive,Primitive::SwitchTag(_)));
    let (Some(old), Some(new)) = (
        profile(captured, needs_current_profile),
        profile(fresh, needs_current_profile),
    ) else {
        return reject("unproven_native_profile");
    };
    if matches!(&parsed, ParsedIntent::Single(Primitive::RelativeSwitch(_)))
        && (captured["windows_complete"] != true
            || fresh["windows_complete"] != true
            || captured["assistant_permit"].is_null()
            || captured
                .get("activation_monitor_token")
                .is_none_or(|v| v.as_str().is_none_or(str::is_empty))
            || fresh
                .get("activation_monitor_token")
                .is_none_or(|v| v.as_str().is_none_or(str::is_empty))
            || captured.get("selected_monitor_token") != fresh.get("selected_monitor_token")
            || captured.get("activation_monitor_token") != fresh.get("activation_monitor_token")
            || captured.get("monitors_complete").is_some_and(|v| v != true)
            || fresh.get("monitors_complete").is_some_and(|v| v != true)
            || [captured, fresh].iter().any(|context| {
                ["monitors_truncated", "windows_truncated"]
                    .iter()
                    .any(|key| context.get(*key).is_some_and(|value| value != false))
            }))
    {
        return reject("relative_workspace_scope_unverified");
    }
    if old != new || frozen.wm_instance_id != old.wm {
        return reject("native_profile_changed");
    }
    if captured["wm_pid"].as_u64().is_none_or(|pid| pid == 0)
        || !positive_native_ticks(&captured["wm_start_ticks"])
    {
        return reject("native_process_identity_unverified");
    }
    if !captured
        .as_object()
        .is_some_and(|object| object.contains_key("assistant_permit"))
        || !fresh
            .as_object()
            .is_some_and(|object| object.contains_key("assistant_permit"))
    {
        return reject("native_permit_state_unverified");
    }
    if !captured["assistant_permit"].is_null() {
        let Ok(permit) = serde_json::from_value::<jwm_eva_core::protocol::AssistantPermit>(
            captured["assistant_permit"].clone(),
        ) else {
            return reject("native_permit_state_malformed");
        };
        if permit.assistant_session_id.is_empty() || permit.generation == 0 {
            return reject("native_permit_generation_mismatch");
        }
    }
    if captured
        .get("connection_epoch")
        .is_some_and(|value| value.as_u64().is_none())
        || fresh
            .get("connection_epoch")
            .is_some_and(|value| value.as_u64().is_none())
    {
        return reject("native_connection_epoch_malformed");
    }
    for key in [
        "wm_pid",
        "wm_start_ticks",
        "connection_epoch",
        "assistant_permit",
    ] {
        if captured.get(key) != fresh.get(key) {
            return reject("native_connection_identity_changed");
        }
    }
    let (Some(before), Some(after)) = (
        captured["observed_monotonic_ns"].as_u64(),
        fresh["observed_monotonic_ns"].as_u64(),
    ) else {
        return reject("native_observation_unverified");
    };
    if before == 0 || after <= before {
        return reject("native_observation_regressed");
    }
    if plan
        .actions
        .iter()
        .any(|a| !native_available(captured, &a.name) || !native_available(fresh, &a.name))
    {
        return reject("capability_not_available");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return reject("original_plan_invalid");
    };
    if serde_json::to_value(&rebuilt).ok() != serde_json::to_value(frozen).ok() {
        return reject("original_frozen_changed");
    }
    let single = matches!(parsed, ParsedIntent::Single(_));
    let matched = match &parsed {
        ParsedIntent::Single(primitive) => single_value(primitive, plan, frozen, &old),
        _ => chain_value(&parsed, plan, frozen, &old, captured, fresh),
    };
    if !matched {
        return reject("owned_operation_value_or_target_mismatch");
    }
    Verdict::Proved(Proof {
        hash: frozen.plan_hash.clone(),
        candidate: jwm_eva_core::protocol::digest(&serde_json::to_value(plan).unwrap()),
        frozen: jwm_eva_core::protocol::digest(&serde_json::to_value(frozen).unwrap()),
        frame,
        single,
    })
}

#[cfg(test)]
#[path = "owned_intent_tests.rs"]
mod tests;

#[cfg(test)]
mod generic_grammar_tests {
    use super::*;
    use serde_json::json;
    fn context(mask: u64) -> Value {
        json!({"wm_instance_id":"native","wm_pid":123,"wm_start_ticks":"456","wake_generation":0,"assistant_permit":{"assistant_session_id":"session","generation":1},"observed_monotonic_ns":100,"session_unlocked":true,"windows_complete":true,"tag_count":9,"selected_monitor_token":"mon1","activation_monitor_token":"mon1","available_actions":["switch_tag","set_layout","move_window_to_tag"],"speech_focus_ref":"w1","speech_focus_uncertain":false,"focus_token":"win1","monitors":[{"monitor_ref":"m1","monitor_token":"mon1","visible_tag_mask":mask,"layout":"tile","locked":false},{"monitor_ref":"m2","monitor_token":"mon2","visible_tag_mask":1,"layout":"tile","locked":false}],"windows":[{"window_ref":"w1","target_token":"win1","id":"42","monitor_token":"mon1","tag_mask":mask,"focused":true}]})
    }
    fn fresh(c: &Value) -> Value {
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(200);
        n.as_object_mut().unwrap().remove("speech_focus_ref");
        n.as_object_mut().unwrap().remove("speech_focus_uncertain");
        n
    }
    fn plan(c: &Value, actions: Value) -> (Plan, FrozenPlan) {
        let p: Plan = serde_json::from_value(
            json!({"disposition":"execute","actions":actions,"question":null,"reason":null}),
        )
        .unwrap();
        let f = validate_plan(&p, c).unwrap();
        (p, f)
    }
    fn switching(c: &Value, tag: u64) -> (Plan, FrozenPlan) {
        plan(
            c,
            json!([{"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":tag}}]),
        )
    }
    #[test]
    fn relative_targets_are_computed_from_stable_single_view_not_candidate_or_wrap() {
        for (mask, text, target) in [
            (1, "去下一个工作区", 2),
            (4, "再往后翻一个桌面", 4),
            (4, "回前一个工作区", 2),
        ] {
            let c = context(mask);
            let n = fresh(&c);
            let (p, f) = switching(&c, target);
            let Verdict::Proved(proof) = assess(Some(text), &p, &f, &c, &n, (1, 2)) else {
                panic!("{text}");
            };
            assert!(proof.accepts(&p, &f, (1, 2)));
            assert!(proof.can_correct_model_domain());
            let (mut wrong, wrong_f) = switching(&c, if target == 2 { 3 } else { 2 });
            assert!(matches!(
                assess(Some(text), &wrong, &wrong_f, &c, &n, (1, 2)),
                Verdict::Reject(_)
            ));
            wrong.actions[0].arguments["monitor_ref"] = json!("m2");
            assert!(matches!(
                assess(Some(text), &wrong, &wrong_f, &c, &n, (1, 2)),
                Verdict::Reject(_)
            ));
        }
        for (mask, text, candidate) in [
            (1, "回前一个工作区", 9),
            (256, "去下一个工作区", 1),
            (3, "去下一个工作区", 3),
        ] {
            let c = context(mask);
            let n = fresh(&c);
            let (p, f) = switching(&c, candidate);
            assert!(
                matches!(
                    assess(Some(text), &p, &f, &c, &n, (1, 2)),
                    Verdict::Reject(_)
                ),
                "{text} mask{mask}"
            );
        }
        // Absolute commands retain their preexisting complete multiview path.
        let c = context(3);
        let n = fresh(&c);
        let (p, f) = switching(&c, 3);
        assert!(matches!(
            assess(Some("去三号桌面"), &p, &f, &c, &n, (1, 2)),
            Verdict::Proved(_)
        ));
    }
    #[test]
    fn relative_identity_inventory_and_fresh_profile_drift_veto() {
        let c = context(1);
        let n = fresh(&c);
        let (p, f) = switching(&c, 2);
        for (key, value) in [
            ("windows_complete", json!(false)),
            ("assistant_permit", Value::Null),
            ("monitors_complete", json!(false)),
            ("monitors_truncated", json!(true)),
            ("selected_monitor_token", json!("mon2")),
            ("activation_monitor_token", Value::Null),
            ("wake_generation", json!(1)),
            (
                "assistant_permit",
                json!({"assistant_session_id":"session","generation":2}),
            ),
            ("wm_start_ticks", json!("457")),
            ("observed_monotonic_ns", json!(100)),
        ] {
            let mut changed = n.clone();
            changed[key] = value;
            assert!(
                matches!(
                    assess(Some("去下一个工作区"), &p, &f, &c, &changed, (1, 2)),
                    Verdict::Reject(_)
                ),
                "{key}"
            );
        }
        for value in [Value::Null, json!(""), json!(42)] {
            let mut old = c.clone();
            let mut new = n.clone();
            old["activation_monitor_token"] = value.clone();
            new["activation_monitor_token"] = value;
            assert!(
                matches!(
                    assess(Some("去下一个工作区"), &p, &f, &old, &new, (1, 2)),
                    Verdict::Reject(_)
                ),
                "unknown activation on both snapshots cannot become a default"
            );
        }
        let mut changed = n.clone();
        changed["monitors"][0]["visible_tag_mask"] = json!(2);
        assert!(matches!(
            assess(Some("去下一个工作区"), &p, &f, &c, &changed, (1, 2)),
            Verdict::Reject(_)
        ));
        let mut changed = f.clone();
        changed.plan_hash.push('x');
        assert!(matches!(
            assess(Some("去下一个工作区"), &p, &changed, &c, &n, (1, 2)),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn new_layouts_and_current_follow_preserve_exact_original_payload_proof() {
        let c = context(1);
        let n = fresh(&c);
        for (text, value) in [
            ("窗口排成网格吧", "grid"),
            ("换成能左右滚动的 scrolling 布局", "scrolling"),
            ("别换网格，用平铺就好", "tile"),
        ] {
            let (p, f) = plan(
                &c,
                json!([{"name":"set_layout","arguments":{"monitor_ref":"m1","tag_number":1,"layout":value}}]),
            );
            assert!(matches!(
                assess(Some(text), &p, &f, &c, &n, (1, 2)),
                Verdict::Proved(_)
            ));
            let (wrong, wrong_f) = plan(
                &c,
                json!([{"name":"set_layout","arguments":{"monitor_ref":"m1","tag_number":1,"layout":if value=="tile"{"grid"}else{"tile"}}}]),
            );
            assert!(matches!(
                assess(Some(text), &wrong, &wrong_f, &c, &n, (1, 2)),
                Verdict::Reject(_)
            ));
            let (wrong, wrong_f) = switching(&c, 2);
            assert!(matches!(
                assess(Some(text), &wrong, &wrong_f, &c, &n, (1, 2)),
                Verdict::Reject(_)
            ));
        }
        let (p, f) = plan(
            &c,
            json!([{"name":"move_window_to_tag","arguments":{"window_ref":"w1","tag_number":8}},{"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":8}}]),
        );
        let Verdict::Proved(proof) = assess(
            Some("把这个搬到八号桌面，我也跟过去"),
            &p,
            &f,
            &c,
            &n,
            (1, 2),
        ) else {
            panic!("complete owned chain");
        };
        assert!(!proof.can_correct_model_domain());
        assert!(proof.accepts(&p, &f, (1, 2)));
        let mut changed = n.clone();
        changed["windows"][0]["target_token"] = json!("replacement");
        assert!(matches!(
            assess(
                Some("把这个搬到八号桌面，我也跟过去"),
                &p,
                &f,
                &c,
                &changed,
                (1, 2)
            ),
            Verdict::Reject(_)
        ));
        let mut extra = p.clone();
        extra.actions.push(p.actions[1].clone());
        assert!(matches!(
            assess(
                Some("把这个搬到八号桌面，我也跟过去"),
                &extra,
                &f,
                &c,
                &n,
                (1, 2)
            ),
            Verdict::Reject(_)
        ));
        assert!(matches!(
            assess(None, &p, &f, &c, &n, (1, 2)),
            Verdict::Unsupported
        ));
    }
}
