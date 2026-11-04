//! Complete owned explicit-right-screen chain evidence, never an action or
//! permit constructor. Caller MUST first validate the correlated BoundAllow
//! receipt, transport completion, actor frame and billing, then query fresh
//! native state. This module cannot validate a nonce or authorize execution.
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use serde_json::Value;
use std::collections::BTreeMap;

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
impl Proof {
    pub(super) fn accepts(&self, plan: &Plan, frozen: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == frozen.plan_hash
            && self.candidate == digest(plan)
            && self.frozen == digest(frozen)
    }
}
fn digest(v: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(v).expect("typed plan serializes"))
}
#[derive(Debug)]
struct Intent {
    tag: u32,
    layout: String,
}
fn number(text: &str) -> Option<u32> {
    let text = text.trim().strip_prefix('第').unwrap_or(text.trim());
    let text = text
        .strip_suffix('个')
        .or_else(|| text.strip_suffix('号'))
        .unwrap_or(text)
        .trim();
    if !text.is_empty()
        && text.len() <= 2
        && text.bytes().all(|c| c.is_ascii_digit())
        && (text.len() == 1 || !text.starts_with('0'))
    {
        return text.parse().ok();
    }
    const EN: [&str; 33] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
        "twenty one",
        "twenty two",
        "twenty three",
        "twenty four",
        "twenty five",
        "twenty six",
        "twenty seven",
        "twenty eight",
        "twenty nine",
        "thirty",
        "thirty one",
        "thirty two",
    ];
    let english = text.replace('-', " ");
    if let Some(n) = EN.iter().position(|name| *name == english) {
        return Some(n as u32);
    }
    if text == "两" {
        return Some(2);
    }
    const D: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
    (0u32..=32).find(|n| {
        let label = match n {
            0..=9 => D[*n as usize].to_owned(),
            10 => "十".into(),
            11..=19 => format!("十{}", D[(*n % 10) as usize]),
            _ if (*n).is_multiple_of(10) => format!("{}十", D[(*n / 10) as usize]),
            _ => format!("{}十{}", D[(*n / 10) as usize], D[(*n % 10) as usize]),
        };
        label == text
    })
}
fn layout(text: &str) -> Option<String> {
    let value = text
        .trim()
        .strip_suffix("布局")
        .unwrap_or(text.trim())
        .trim();
    let canonical = match value {
        "平铺" | "主从平铺" => "tile",
        "浮动" => "float",
        "单窗口" | "单窗" => "monocle",
        "斐波那契" => "fibonacci",
        "居中主区" | "主区居中" => "centered_master",
        "底部堆叠" => "bstack",
        "网格" => "grid",
        "卡片堆叠" => "deck",
        "三列" => "three_col",
        "榻榻米" => "tatami",
        "全屏" => "fullscreen",
        "滚动" => "scrolling",
        "垂直堆叠" | "竖直堆叠" => "vstack",
        other => other,
    };
    jwm_eva_core::actions::LAYOUTS
        .contains(&canonical)
        .then(|| canonical.to_owned())
}
fn parse(text: Option<&str>) -> Option<Intent> {
    let text = text?;
    if text.len() > 256 || text.chars().any(char::is_control) {
        return None;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '.', '!', '！'])
        .trim()
        .to_ascii_lowercase();
    let text = text.strip_prefix('请').unwrap_or(&text).trim();
    let (first, second) = ["，然后", ",然后", "然后"]
        .iter()
        .find_map(|separator| text.split_once(*separator))?;
    let first = first.strip_prefix("右屏")?.trim();
    let first = first
        .strip_prefix("切换到")
        .or_else(|| first.strip_prefix("切到"))?
        .trim();
    let tag = if let Some(n) = first
        .strip_prefix("tag ")
        .or_else(|| first.strip_prefix("workspace "))
    {
        number(n)?
    } else {
        number(
            ["工作区", "桌面", "tag"]
                .iter()
                .find_map(|unit| first.strip_suffix(*unit))?,
        )?
    };
    let second = second.trim();
    let rest = ["set layout to ", "set layout ", "布局改成", "改成", "换成"]
        .iter()
        .find_map(|prefix| second.strip_prefix(*prefix))?;
    Some(Intent {
        tag,
        layout: layout(rest)?,
    })
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    parse(text).is_some()
}
fn nonempty(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
fn native_ticks(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        !s.is_empty()
            && !s.starts_with('0')
            && s.bytes().all(|c| c.is_ascii_digit())
            && s.parse::<u64>().is_ok_and(|n| n > 0)
    })
}
fn same_native_identity(old: &Value, new: &Value) -> bool {
    if old["session_unlocked"] != true
        || new["session_unlocked"] != true
        || nonempty(&old["wm_instance_id"]).is_none()
        || old["wm_instance_id"] != new["wm_instance_id"]
    {
        return false;
    }
    if old["wm_pid"].as_u64().is_none_or(|p| p == 0)
        || old["wm_pid"] != new["wm_pid"]
        || !native_ticks(&old["wm_start_ticks"])
        || old["wm_start_ticks"] != new["wm_start_ticks"]
        || old["wake_generation"].as_u64().is_none()
        || old["wake_generation"] != new["wake_generation"]
    {
        return false;
    }
    if !old
        .as_object()
        .is_some_and(|m| m.contains_key("assistant_permit"))
        || !new
            .as_object()
            .is_some_and(|m| m.contains_key("assistant_permit"))
        || old["assistant_permit"] != new["assistant_permit"]
    {
        return false;
    }
    if !old["assistant_permit"].is_null() {
        let Ok(p) = serde_json::from_value::<jwm_eva_core::protocol::AssistantPermit>(
            old["assistant_permit"].clone(),
        ) else {
            return false;
        };
        if p.assistant_session_id.is_empty() || p.generation == 0 {
            return false;
        }
    }
    if old.get("connection_epoch") != new.get("connection_epoch")
        || old
            .get("connection_epoch")
            .is_some_and(|v| v.as_u64().is_none())
    {
        return false;
    }
    let (Some(before), Some(after)) = (
        old["observed_monotonic_ns"].as_u64(),
        new["observed_monotonic_ns"].as_u64(),
    ) else {
        return false;
    };
    before > 0
        && after > before
        && old["available_actions"].is_array()
        && old["available_actions"] == new["available_actions"]
}
#[derive(Debug, PartialEq)]
struct Output {
    reference: String,
    token: String,
    rect: [i64; 4],
    visible: u64,
    layout: String,
}
#[derive(Debug, PartialEq)]
struct Scene {
    outputs: BTreeMap<String, Output>,
    right: String,
    count: u64,
    target_layout: String,
}
fn scene(context: &Value, tag: u32) -> Option<Scene> {
    let count = context["tag_count"]
        .as_u64()
        .filter(|n| (1..=32).contains(n))?;
    if tag == 0 || u64::from(tag) > count || tag > 32 {
        return None;
    }
    let rows = context["monitors"].as_array().filter(|m| m.len() == 2)?;
    let mut outputs = BTreeMap::new();
    let mut references = std::collections::BTreeSet::new();
    for row in rows {
        let token = nonempty(&row["monitor_token"])?;
        let reference = nonempty(&row["monitor_ref"])?;
        if row["locked"] != false || !references.insert(reference.to_owned()) {
            return None;
        }
        let geometry = row["geometry"].as_array().filter(|a| a.len() == 4)?;
        let rect = [
            geometry[0].as_i64()?,
            geometry[1].as_i64()?,
            geometry[2].as_i64()?,
            geometry[3].as_i64()?,
        ];
        if rect[2] <= 0 || rect[3] <= 0 {
            return None;
        }
        rect[0].checked_add(rect[2])?;
        rect[1].checked_add(rect[3])?;
        let visible = row["visible_tag_mask"]
            .as_u64()
            .filter(|m| *m > 0 && *m <= u32::MAX as u64 && m & !((1u64 << count) - 1) == 0)?;
        let output = Output {
            reference: reference.into(),
            token: token.into(),
            rect,
            visible,
            layout: jwm_eva_core::actions::normalize_layout(row["layout"].as_str()?)?.into(),
        };
        if outputs.insert(token.to_owned(), output).is_some() {
            return None;
        }
    }
    let pair = outputs.values().collect::<Vec<_>>();
    let (a, b) = (pair[0], pair[1]);
    if a.rect[1].max(b.rect[1]) >= (a.rect[1] + a.rect[3]).min(b.rect[1] + b.rect[3]) {
        return None;
    }
    let right = if a.rect[0] + a.rect[2] <= b.rect[0] {
        b.token.clone()
    } else if b.rect[0] + b.rect[2] <= a.rect[0] {
        a.token.clone()
    } else {
        return None;
    };
    let mut targets = context["workspaces"].as_array()?.iter().filter(|p| {
        p["monitor_token"] == right && p["tag_number"].as_u64() == Some(u64::from(tag))
    });
    let target = targets.next()?;
    if targets.next().is_some() || target["tag_mask"].as_u64() != Some(1u64 << (tag - 1)) {
        return None;
    }
    let target_layout =
        jwm_eva_core::actions::normalize_layout(target["layout"].as_str()?)?.to_owned();
    Some(Scene {
        outputs,
        right,
        count,
        target_layout,
    })
}
fn exact_keys(v: &Value, keys: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|m| m.len() == keys.len() && keys.iter().all(|key| m.contains_key(*key)))
}
fn review_matches(review: &Value, intent: &Intent, right: &Output, original: &str) -> bool {
    if review["decision"] != "allow"
        || review["session_intent"] != "continue"
        || review.get("question") != Some(&Value::Null)
        || review
            .get("native_control_quote")
            .is_some_and(|q| q.as_str() != Some(""))
        || review
            .get("batch_close_scope")
            .is_some_and(|s| !s.is_null() && s != "none")
        || review
            .get("batch_close_quote")
            .is_some_and(|q| q.as_str() != Some(""))
    {
        return false;
    }
    let Some(steps) = review["steps"].as_array().filter(|s| s.len() == 2) else {
        return false;
    };
    for (index, (step, name)) in steps.iter().zip(["switch_tag", "set_layout"]).enumerate() {
        if step["name"] != name
            || step["monitor_refs"]
                .as_array()
                .is_none_or(|a| a.len() != 1 || a[0] != right.reference)
            || step["tag_numbers"]
                .as_array()
                .is_none_or(|a| a.len() != 1 || a[0].as_u64() != Some(u64::from(intent.tag)))
        {
            return false;
        }
        let specs = jwm_eva_core::actions::semantic_actions();
        let Some(spec) = specs.iter().find(|s| s.canonical_name == name) else {
            return false;
        };
        if step
            .get("target_scope")
            .is_some_and(|s| s.as_str() != Some(spec.target_resolver.as_str()))
        {
            return false;
        }
        for key in [
            "window_refs",
            "device_ids",
            "application_ids",
            "session_refs",
        ] {
            if step[key].as_array().is_none_or(|a| !a.is_empty()) {
                return false;
            }
        }
        for key in [
            "window_quote",
            "device_quote",
            "application_quote",
            "session_quote",
        ] {
            if step.get(key).is_some_and(|q| q.as_str() != Some("")) {
                return false;
            }
        }
        if step.get("window_basis").is_some_and(|b| b != "none")
            || step.get("media_action").is_some_and(|v| !v.is_null())
        {
            return false;
        }
        if !matches!(
            step["monitor_basis"].as_str(),
            Some("spatial" | "named" | "current" | "selected_workspace")
        ) && !(index == 1 && step["monitor_basis"] == "prior_step")
        {
            return false;
        }
        for key in ["monitor_quote", "effect_quote"] {
            if step.get(key).is_some_and(|q| {
                q.as_str()
                    .is_none_or(|q| !q.is_empty() && !original.contains(q))
            }) {
                return false;
            }
        }
        // Parameter-bearing review fields are not offered by the strict review
        // schema. In particular no conflicting layout declaration is erased.
        if step.get("layout").is_some() {
            return false;
        }
    }
    true
}
/// A successful proof supplements only source labels of two exact independent
/// allow rows. It cannot override Clarify/Noop, drop rows/tags, or fix a bad scope.
pub(super) fn assess(
    text: Option<&str>,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    review: &Value,
    frame: (u64, u64),
) -> Verdict {
    let Some(intent) = parse(text) else {
        return Verdict::Unsupported;
    };
    let reject = |why| Verdict::Reject(why);
    if frame.0 == 0
        || frame.1 == 0
        || plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || plan.actions.len() != 2
        || frozen.actions.len() != 2
        || frozen.requires_confirmation
    {
        return reject("not_original_ordinary_two_step_execute");
    }
    if !same_native_identity(captured, fresh)
        || frozen.wm_instance_id != captured["wm_instance_id"].as_str().unwrap_or("")
    {
        return reject("native_identity_or_freshness_unverified");
    }
    let (Some(old), Some(new)) = (scene(captured, intent.tag), scene(fresh, intent.tag)) else {
        return reject("right_output_or_target_profile_unproven");
    };
    if old != new {
        return reject("native_output_or_profile_changed");
    }
    let right = &old.outputs[&old.right];
    let Some(original) = text else {
        return Verdict::Unsupported;
    };
    if !review_matches(review, &intent, right, original) {
        return reject("independent_allow_declaration_mismatch");
    }
    let specs = jwm_eva_core::actions::semantic_actions();
    for (index, name) in ["switch_tag", "set_layout"].into_iter().enumerate() {
        if specs
            .iter()
            .find(|s| s.canonical_name == name)
            .is_none_or(|s| {
                !matches!(s.risk_class.as_str(), "ordinary" | "reversible")
                    || !s.undo_strategy.starts_with("restore_")
            })
            || !captured["available_actions"]
                .as_array()
                .is_some_and(|a| a.iter().any(|n| n == name))
        {
            return reject("ordinary_capability_unavailable");
        }
        let semantic = &plan.actions[index];
        let native = &frozen.actions[index];
        let skeys = if index == 0 {
            vec!["monitor_ref", "tag_number"]
        } else {
            vec!["monitor_ref", "tag_number", "layout"]
        };
        let nkeys = if index == 0 {
            vec!["monitor_token", "tag_mask"]
        } else {
            vec!["monitor_token", "tag_mask", "layout"]
        };
        if semantic.name != name
            || native.name != name
            || !exact_keys(&semantic.arguments, &skeys)
            || !exact_keys(&native.arguments, &nkeys)
            || semantic.arguments["monitor_ref"] != right.reference
            || native.arguments["monitor_token"] != right.token
            || semantic.arguments["tag_number"].as_u64() != Some(u64::from(intent.tag))
            || native.arguments["tag_mask"].as_u64() != Some(1u64 << (intent.tag - 1))
            || (index == 1
                && (semantic.arguments["layout"] != intent.layout
                    || native.arguments["layout"] != intent.layout))
        {
            return reject("original_operation_value_or_output_mismatch");
        }
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return reject("original_plan_invalid");
    };
    if digest(&rebuilt) != digest(frozen) {
        return reject("original_frozen_changed");
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
    const FRAME: (u64, u64) = (4, 8);
    const TEXT: &str = "右屏切到 tag two，然后 set layout scrolling";
    fn context() -> Value {
        let mut context = json!({"wm_instance_id":"wm","wm_pid":4242,"wm_start_ticks":"100","wake_generation":0,"assistant_permit":{"assistant_session_id":"owned","generation":1},"observed_monotonic_ns":100,"session_unlocked":true,"tag_count":9,"selected_monitor_token":"left-token","available_actions":["switch_tag","set_layout"],"monitors":[{"monitor_ref":"left","monitor_token":"left-token","geometry":[-1920,0,1920,1080],"visible_tag_mask":1,"layout":"tile","locked":false},{"monitor_ref":"right","monitor_token":"right-token","geometry":[0,0,1920,1080],"visible_tag_mask":2,"layout":"tile","locked":false}],"windows":[]});
        context["workspaces"]=json!(["left-token","right-token"].into_iter().flat_map(|token|(1..=9).map(move|n|json!({"monitor_token":token,"tag_number":n,"tag_mask":1u64<<(n-1),"layout":"tile"}))).collect::<Vec<_>>());
        context
    }
    fn fresh(old: &Value) -> Value {
        let mut new = old.clone();
        new["observed_monotonic_ns"] = json!(200);
        new
    }
    fn plan(context: &Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"switch_tag","arguments":{"monitor_ref":"right","tag_number":2}},{"name":"set_layout","arguments":{"monitor_ref":"right","tag_number":2,"layout":"scrolling"}}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, context).unwrap();
        (p, f)
    }
    fn review() -> Value {
        json!({"decision":"allow","session_intent":"continue","question":null,"steps":[{"name":"switch_tag","monitor_refs":["right"],"monitor_basis":"named","monitor_quote":"右屏","tag_numbers":[2],"window_refs":[],"device_ids":[],"application_ids":[],"session_refs":[]},{"name":"set_layout","target_scope":"workspace","monitor_refs":["right"],"monitor_basis":"prior_step","monitor_quote":"右屏","tag_numbers":[2],"window_refs":[],"device_ids":[],"application_ids":[],"session_refs":[]}]})
    }
    fn proved(p: &Plan, f: &FrozenPlan, old: &Value, new: &Value, r: &Value) -> Proof {
        match assess(Some(TEXT), p, f, old, new, r, FRAME) {
            Verdict::Proved(proof) => proof,
            other => panic!("{other:?}"),
        }
    }
    fn rejects(p: &Plan, f: &FrozenPlan, old: &Value, new: &Value, r: &Value) {
        assert!(matches!(
            assess(Some(TEXT), p, f, old, new, r, FRAME),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn actual138_explicit_right_survives_left_selection_and_incorrect_source_labels_without_raw_changes()
     {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(&old);
        let r = review();
        let before = json!({"p":p,"f":f,"r":r,"old":old,"new":new});
        let proof = proved(&p, &f, &old, &new, &r);
        assert!(proof.accepts(&p, &f, FRAME));
        assert_eq!(f.actions[0].arguments["monitor_token"], "right-token");
        assert_eq!(old["selected_monitor_token"], "left-token");
        assert_eq!(before, json!({"p":p,"f":f,"r":r,"old":old,"new":new}));
        let mut reordered = old.clone();
        reordered["monitors"].as_array_mut().unwrap().reverse();
        let new = fresh(&reordered);
        assert!(proved(&p, &f, &reordered, &new, &r).accepts(&p, &f, FRAME));
    }
    #[test]
    fn literal_grammar_is_general_in_tag_and_public_layout_but_consumes_every_clause() {
        for text in [
            "右屏切到 tag three，然后 set layout grid",
            "右屏切到第五个工作区，然后改成网格布局",
            "右屏切到tag 2，然后set layout to scrolling",
        ] {
            assert!(recognizes(Some(text)), "{text}");
        }
        for text in [
            None,
            Some("左屏切到tag two，然后set layout scrolling"),
            Some("HDMI-1切到tag two，然后set layout scrolling"),
            Some("不要右屏切到tag two，然后set layout scrolling"),
            Some("标题写着右屏切到tag two，然后set layout scrolling"),
            Some("右屏切到tag two，然后set layout scrolling，然后关闭窗口"),
            Some("右屏切到tag two，不对tag three，然后set layout scrolling"),
            Some("右屏切到第三屏tag two，然后set layout scrolling"),
        ] {
            assert!(
                matches!(
                    assess(
                        text,
                        &plan(&context()).0,
                        &plan(&context()).1,
                        &context(),
                        &fresh(&context()),
                        &review(),
                        FRAME
                    ),
                    Verdict::Unsupported
                ),
                "{text:?}"
            );
        }
    }
    #[test]
    fn unknown_ambiguous_overlapping_or_changed_native_topology_cannot_choose_right_by_index() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(&old);
        for mode in [
            "third",
            "overlap",
            "vertical",
            "unknown",
            "locked",
            "duplicate_ref",
        ] {
            let mut changed = old.clone();
            match mode {
                "third" => {
                    let mut third = changed["monitors"][1].clone();
                    third["monitor_ref"] = json!("third");
                    third["monitor_token"] = json!("third-token");
                    changed["monitors"].as_array_mut().unwrap().push(third);
                }
                "overlap" => changed["monitors"][1]["geometry"] = json!([-100, 0, 1920, 1080]),
                "vertical" => changed["monitors"][1]["geometry"] = json!([0, 1080, 1920, 1080]),
                "unknown" => {
                    changed["monitors"][1]
                        .as_object_mut()
                        .unwrap()
                        .remove("geometry");
                }
                "locked" => changed["monitors"][1]["locked"] = json!(true),
                _ => changed["monitors"][1]["monitor_ref"] = json!("left"),
            }
            rejects(&p, &f, &changed, &fresh(&changed), &review());
        }
        for field in ["geometry", "monitor_token", "visible_tag_mask", "layout"] {
            let mut changed = new.clone();
            changed["monitors"][1][field] = match field {
                "geometry" => json!([10, 0, 1920, 1080]),
                "visible_tag_mask" => json!(4),
                "layout" => json!("grid"),
                _ => json!("replacement-token"),
            };
            rejects(&p, &f, &old, &changed, &review());
        }
    }
    #[test]
    fn independent_allow_must_declare_exact_two_ops_targets_tags_and_scopes() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(&old);
        for (index, key, value) in [
            (0, "name", json!("focus_monitor")),
            (0, "monitor_refs", json!(["left"])),
            (1, "tag_numbers", json!([])),
            (1, "tag_numbers", json!([3])),
            (1, "target_scope", json!("window")),
            (1, "window_refs", json!(["w1"])),
            (1, "layout", json!("grid")),
            (1, "device_ids", json!(["device"])),
        ] {
            let mut r = review();
            r["steps"][index][key] = value;
            rejects(&p, &f, &old, &new, &r);
        }
        let mut r = review();
        r["steps"].as_array_mut().unwrap().pop();
        rejects(&p, &f, &old, &new, &r);
        let mut r = review();
        r["steps"].as_array_mut().unwrap().reverse();
        rejects(&p, &f, &old, &new, &r);
        let mut r = review();
        r["decision"] = json!("clarify");
        r["question"] = json!("Which screen?");
        rejects(&p, &f, &old, &new, &r);
        // Wrong nonce/incomplete responses are rejected by the caller before
        // assess; this Value is a declaration, not a correlated receipt type.
    }
    #[test]
    fn candidate_value_order_hash_frame_and_real_native_identity_are_not_repaired() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(&old);
        let proof = proved(&p, &f, &old, &new, &review());
        let mut wrong = p.clone();
        wrong.actions[1].arguments["layout"] = json!("grid");
        let wf = validate_plan(&wrong, &old).unwrap();
        rejects(&wrong, &wf, &old, &new, &review());
        let mut wrong = p.clone();
        wrong.actions.reverse();
        let wf = validate_plan(&wrong, &old).unwrap();
        rejects(&wrong, &wf, &old, &new, &review());
        let mut wf = f.clone();
        wf.actions[1].arguments["tag_mask"] = json!(4);
        assert!(!proof.accepts(&p, &wf, FRAME));
        rejects(&p, &wf, &old, &new, &review());
        assert!(!proof.accepts(&p, &f, (FRAME.0, FRAME.1 + 1)));
        let mut wf = f.clone();
        wf.plan_hash.push('x');
        rejects(&p, &wf, &old, &new, &review());
        for (key, value) in [
            ("wm_start_ticks", json!(100)),
            ("wm_pid", json!(99)),
            ("wake_generation", json!(1)),
            ("assistant_permit", Value::Null),
            ("observed_monotonic_ns", json!(100)),
            ("available_actions", json!(["switch_tag"])),
        ] {
            let mut changed = new.clone();
            changed[key] = value;
            rejects(&p, &f, &old, &changed, &review());
        }
    }
}
