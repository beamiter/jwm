//! Complete owned single-monitor intents; never constructs actions or refs.
//! NativeInventoryEvidence must be minted ONLY by the actor after its actual
//! full ContextStore/native-RPC capture and fresh_owned_snapshot path. Shape
//! checks here do not authenticate IPC provenance. Cloud/model/history Values
//! cannot supply that provenance, and the evidence cannot be serialized.
use super::intent_syntax::{ParsedIntent, Primitive, parse_owned};
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

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
    pub(super) fn accepts(&self, p: &Plan, f: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == f.plan_hash
            && self.candidate == digest(p)
            && self.frozen == digest(f)
    }
}
#[derive(Debug)]
pub(super) struct NativeInventoryEvidence {
    captured: String,
    fresh: String,
    frame: (u64, u64),
}
fn digest(v: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(v).expect("typed evidence serializes"))
}
fn nonempty(v: &Value) -> Option<&str> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
fn native_shape(c: &Value) -> bool {
    c["protocol_version"] == 1
        && c["windows_complete"] == true
        && c["windows"].is_array()
        && c["monitors"].as_array().is_some_and(|m| m.len() == 2)
        && c.get("monitors_complete").is_none_or(|v| v == true)
        && c.get("monitors_truncated").is_none_or(|v| v == false)
        && c.get("windows_truncated").is_none_or(|v| v == false)
}
/// Sole production mint call belongs immediately after the actor's real native
/// full-inventory queries. Caller provenance is a requirement, NOT inferred
/// from these Values; this function only binds exact native payloads + frame.
pub(super) fn bind_native_inventory(
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
) -> Option<NativeInventoryEvidence> {
    if frame.0 == 0 || frame.1 == 0 || !native_shape(captured) || !native_shape(fresh) {
        return None;
    }
    Some(NativeInventoryEvidence {
        captured: digest(captured),
        fresh: digest(fresh),
        frame,
    })
}
#[derive(Debug)]
enum Intent {
    FocusHdmi,
    FocusPrimary,
    RightSwitch { tag: u32 },
    LeftLayout { tag: u32, layout: String },
    CorrectMoveLeft,
}
fn parse(text: Option<&str>) -> Option<Intent> {
    let text = text?;
    if text.len() > 256 || text.chars().any(char::is_control) {
        return None;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '!', '！'])
        .trim()
        .to_ascii_lowercase();
    let text = text
        .strip_prefix('请')
        .or_else(|| text.strip_prefix("please "))
        .unwrap_or(&text)
        .trim();
    if text == "focus the hdmi screen" {
        return Some(Intent::FocusHdmi);
    }
    if matches!(
        text,
        "切回主显示器" | "切到主显示器" | "聚焦主显示器" | "focus the primary monitor"
    ) {
        return Some(Intent::FocusPrimary);
    }

    for prefix in ["右屏", "右边屏幕", "right screen "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let rest = rest.trim();
            if let Some(operand) = [
                "切换到",
                "切到",
                "切去",
                "换到",
                "去",
                "switch to ",
                "go to ",
            ]
            .iter()
            .find_map(|verb| rest.strip_prefix(verb))
                && let Some(tag) = super::intent_syntax::literal_tag_operand(operand.trim())
            {
                return Some(Intent::RightSwitch { tag });
            }
        }
    }
    if matches!(
        text,
        "把窗口移到右屏，不，是左屏"
            | "把这个窗口移到右屏，不，是左屏"
            | "把当前窗口移到右屏，不，是左屏"
    ) {
        return Some(Intent::CorrectMoveLeft);
    }
    let rest = text.strip_prefix("把左屏")?;
    let (tags, layout_text) = ["换成", "改成"].iter().find_map(|s| rest.split_once(s))?;
    if tags.contains(['，', ',']) {
        return None;
    }
    let tag_request = format!("切到{}", tags.trim());
    let ParsedIntent::Single(Primitive::SwitchTag(tag)) = parse_owned(Some(&tag_request))? else {
        return None;
    };
    let layout_request = format!("换成{}", layout_text.trim());
    let ParsedIntent::Single(Primitive::Layout(layout)) = parse_owned(Some(&layout_request))?
    else {
        return None;
    };
    Some(Intent::LeftLayout { tag, layout })
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    parse(text).is_some()
}
fn ticks(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.parse::<u64>().is_ok_and(|n| n > 0 && n.to_string() == s))
}
fn same_identity(a: &Value, b: &Value) -> bool {
    if a["session_unlocked"] != true
        || b["session_unlocked"] != true
        || nonempty(&a["wm_instance_id"]).is_none()
        || a["wm_pid"].as_u64().is_none_or(|n| n == 0)
        || !ticks(&a["wm_start_ticks"])
        || a["wake_generation"].as_u64().is_none()
    {
        return false;
    }
    // An explicitly inactive permit is a valid planning snapshot, never an
    // execution authorization. The actor/admission/core permit gates remain.
    let Some(permit_value) = a.get("assistant_permit") else {
        return false;
    };
    if !permit_value.is_null() {
        let Ok(permit) =
            serde_json::from_value::<jwm_eva_core::protocol::AssistantPermit>(permit_value.clone())
        else {
            return false;
        };
        if permit.generation == 0 || nonempty(&permit_value["assistant_session_id"]).is_none() {
            return false;
        }
    }
    for key in [
        "wm_instance_id",
        "wm_pid",
        "wm_start_ticks",
        "wake_generation",
        "assistant_permit",
        "connection_epoch",
    ] {
        if a.get(key) != b.get(key) {
            return false;
        }
    }
    if a.get("connection_epoch")
        .is_some_and(|v| v.as_u64().is_none())
    {
        return false;
    }
    let (Some(old), Some(new)) = (
        a["observed_monotonic_ns"].as_u64(),
        b["observed_monotonic_ns"].as_u64(),
    ) else {
        return false;
    };
    old > 0
        && new > old
        && a["available_actions"].is_array()
        && a["available_actions"] == b["available_actions"]
}
#[derive(Debug, PartialEq, Eq)]
struct Output {
    reference: String,
    token: String,
    connector: String,
    raw: Value,
    rect: [i64; 4],
    visible: u64,
    layout: String,
}
#[derive(Debug, PartialEq, Eq)]
struct Scene {
    outputs: BTreeMap<String, Output>,
    left: String,
    count: u64,
}
fn scene(c: &Value) -> Option<Scene> {
    if !native_shape(c) {
        return None;
    }
    let count = c["tag_count"].as_u64().filter(|n| (1..=32).contains(n))?;
    let mut outputs = BTreeMap::new();
    let mut refs = BTreeSet::new();
    let mut connectors = BTreeSet::new();
    for row in c["monitors"].as_array()? {
        let token = nonempty(&row["monitor_token"])?;
        let reference = nonempty(&row["monitor_ref"])?;
        if row["locked"] != false || !refs.insert(reference.to_owned()) {
            return None;
        }
        let g = row["geometry"].as_array().filter(|g| g.len() == 4)?;
        let rect = [
            g[0].as_i64()?,
            g[1].as_i64()?,
            g[2].as_i64()?,
            g[3].as_i64()?,
        ];
        if rect[2] <= 0 || rect[3] <= 0 {
            return None;
        }
        rect[0].checked_add(rect[2])?;
        rect[1].checked_add(rect[3])?;
        let visible = row["visible_tag_mask"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= u32::MAX as u64 && n & !((1u64 << count) - 1) == 0)?;
        let connector = nonempty(&row["connector"])?;
        if !known_connector(connector) || !connectors.insert(connector.to_owned()) {
            return None;
        }
        let output = Output {
            reference: reference.into(),
            token: token.into(),
            connector: connector.into(),
            raw: row.clone(),
            rect,
            visible,
            layout: jwm_eva_core::actions::normalize_layout(row["layout"].as_str()?)?.into(),
        };
        if outputs.insert(token.into(), output).is_some() {
            return None;
        }
    }
    let pair = outputs.values().collect::<Vec<_>>();
    let (a, b) = (pair[0], pair[1]);
    if a.rect[1].max(b.rect[1]) >= (a.rect[1] + a.rect[3]).min(b.rect[1] + b.rect[3]) {
        return None;
    }
    let left = if a.rect[0] + a.rect[2] <= b.rect[0] {
        a.token.clone()
    } else if b.rect[0] + b.rect[2] <= a.rect[0] {
        b.token.clone()
    } else {
        return None;
    };
    Some(Scene {
        outputs,
        left,
        count,
    })
}
fn connector_suffix(connector: &str, prefixes: &[&str], allow_zero: bool) -> bool {
    prefixes.iter().any(|prefix| {
        connector.strip_prefix(prefix).is_some_and(|number| {
            !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit())
                && number
                    .parse::<u32>()
                    .is_ok_and(|n| (allow_zero || n > 0) && n.to_string() == number)
        })
    })
}
fn known_connector(connector: &str) -> bool {
    hdmi(connector)
        || connector_suffix(
            connector,
            &["DP-", "eDP-", "DVI-D-", "DVI-I-", "VGA-", "LVDS-"],
            true,
        )
}
fn hdmi(connector: &str) -> bool {
    connector_suffix(connector, &["HDMI-"], true)
        || connector_suffix(connector, &["HDMI-A-", "HDMI-B-"], false)
}
fn workspace(c: &Value, token: &str, tag: u32, count: u64) -> Option<String> {
    if tag == 0 || u64::from(tag) > count {
        return None;
    }
    let mut rows = c["workspaces"].as_array()?.iter().filter(|r| {
        r["monitor_token"] == token && r["tag_number"].as_u64() == Some(u64::from(tag))
    });
    let row = rows.next()?;
    if rows.next().is_some() || row["tag_mask"].as_u64() != Some(1u64 << (tag - 1)) {
        return None;
    }
    Some(jwm_eva_core::actions::normalize_layout(row["layout"].as_str()?)?.into())
}
#[derive(Debug, PartialEq)]
struct Window {
    reference: String,
    token: String,
    monitor: String,
    tags: u64,
    id: String,
    pid: Value,
    visible: Value,
    minimized: Value,
    raw: Value,
}
fn current(c: &Value, captured_ref: &str, scene: &Scene) -> Option<Window> {
    let rows = c["windows"].as_array().filter(|r| r.len() <= 128)?;
    let mut matches = rows.iter().filter(|w| w["window_ref"] == captured_ref);
    let w = matches.next()?;
    let token = nonempty(&w["target_token"])?;
    let mon = nonempty(&w["monitor_token"])?;
    if matches.next().is_some()
        || rows.iter().filter(|w| w["target_token"] == token).count() != 1
        || c["focus_token"] != token
        || !scene.outputs.contains_key(mon)
        || w["closed"] == true
        || w["live"] == false
        || w["minimized"] != false
        || w["visible"] != true
    {
        return None;
    }
    let tags = w["tag_mask"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= u32::MAX as u64 && n & !((1u64 << scene.count) - 1) == 0)?;
    Some(Window {
        reference: captured_ref.into(),
        token: token.into(),
        monitor: mon.into(),
        tags,
        id: nonempty(&w["id"])?.into(),
        pid: w["pid"].clone(),
        visible: w["visible"].clone(),
        minimized: w["minimized"].clone(),
        raw: w.clone(),
    })
}
fn focus_history(c: &Value, token: &str, since: u64) -> bool {
    let Some(observed) = c["observed_monotonic_ns"].as_u64() else {
        return false;
    };
    let Some(history) = c["focus_history"]
        .as_array()
        .filter(|h| !h.is_empty() && h.len() <= 128)
    else {
        return false;
    };
    let (mut previous, mut baseline) = (0, None);
    for row in history {
        let Some(at) = row["at_monotonic_ns"]
            .as_u64()
            .filter(|n| *n > 0 && *n > previous && *n <= observed)
        else {
            return false;
        };
        previous = at;
        if at <= since {
            baseline = row["focus_token"].as_str();
        } else if row["focus_token"] != token {
            return false;
        }
    }
    baseline == Some(token)
}
fn keys(v: &Value, wanted: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|m| m.len() == wanted.len() && wanted.iter().all(|k| m.contains_key(*k)))
}
pub(super) fn assess(
    text: Option<&str>,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
    origin: Option<&NativeInventoryEvidence>,
) -> Verdict {
    let Some(intent) = parse(text) else {
        return Verdict::Unsupported;
    };
    let Some(origin) = origin else {
        return Verdict::Reject("native_inventory_origin_unavailable");
    };
    if origin.frame != frame
        || origin.captured != digest(captured)
        || origin.fresh != digest(fresh)
        || !same_identity(captured, fresh)
    {
        return Verdict::Reject("native_inventory_or_identity_changed");
    }
    if plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || frozen.requires_confirmation
        || plan.actions.len() != 1
        || frozen.actions.len() != 1
    {
        return Verdict::Reject("not_single_ordinary_execute");
    }
    let (Some(old), Some(new)) = (scene(captured), scene(fresh)) else {
        return Verdict::Reject("native_monitor_inventory_unproven");
    };
    if old != new || captured["wm_instance_id"] != frozen.wm_instance_id {
        return Verdict::Reject("native_monitor_inventory_drift");
    }
    let action = &plan.actions[0];
    let native = &frozen.actions[0];
    let (name, target, extra) = match &intent {
        Intent::FocusPrimary => {
            if old.outputs.values().any(|m| !m.raw["primary"].is_boolean()) {
                return Verdict::Reject("native_primary_role_unknown");
            }
            let mut primary = old.outputs.values().filter(|m| m.raw["primary"] == true);
            let Some(target) = primary.next() else {
                return Verdict::Reject("native_primary_role_unresolved");
            };
            if primary.next().is_some() {
                return Verdict::Reject("native_primary_role_not_unique");
            }
            ("focus_monitor", target, None)
        }
        Intent::FocusHdmi => {
            let mut hdmis = old.outputs.values().filter(|m| hdmi(&m.connector));
            let Some(target) = hdmis.next() else {
                return Verdict::Reject("hdmi_family_unresolved");
            };
            if hdmis.next().is_some() {
                return Verdict::Reject("hdmi_family_not_unique");
            }
            ("focus_monitor", target, None)
        }
        Intent::RightSwitch { tag } => {
            let Some(target) = old.outputs.values().find(|m| m.token != old.left) else {
                return Verdict::Reject("native_right_monitor_unresolved");
            };
            let (Some(before), Some(after)) = (
                workspace(captured, &target.token, *tag, old.count),
                workspace(fresh, &target.token, *tag, old.count),
            ) else {
                return Verdict::Reject("workspace_profile_unavailable");
            };
            if before != after {
                return Verdict::Reject("workspace_profile_drift");
            }
            ("switch_tag", target, None)
        }
        Intent::LeftLayout { tag, layout } => {
            let target = &old.outputs[&old.left];
            let (Some(before), Some(after)) = (
                workspace(captured, &target.token, *tag, old.count),
                workspace(fresh, &target.token, *tag, old.count),
            ) else {
                return Verdict::Reject("workspace_profile_unavailable");
            };
            if before != after {
                return Verdict::Reject("workspace_profile_drift");
            }
            ("set_layout", target, Some((*tag, layout.as_str())))
        }
        Intent::CorrectMoveLeft => ("move_window_to_monitor", &old.outputs[&old.left], None),
    };
    if action.name != name
        || native.name != name
        || !jwm_eva_core::actions::semantic_actions().iter().any(|s| {
            s.canonical_name == name && matches!(s.risk_class.as_str(), "ordinary" | "reversible")
        })
        || !captured["available_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == name)
    {
        return Verdict::Reject("operation_or_capability_mismatch");
    }
    let matches = if let Intent::RightSwitch { tag } = &intent {
        keys(&action.arguments, &["monitor_ref", "tag_number"])
            && keys(&native.arguments, &["monitor_token", "tag_mask"])
            && action.arguments["tag_number"].as_u64() == Some(u64::from(*tag))
            && native.arguments["tag_mask"].as_u64() == Some(1u64 << (*tag - 1))
    } else if let Some((tag, layout)) = extra {
        keys(&action.arguments, &["monitor_ref", "tag_number", "layout"])
            && keys(&native.arguments, &["monitor_token", "tag_mask", "layout"])
            && action.arguments["tag_number"].as_u64() == Some(u64::from(tag))
            && native.arguments["tag_mask"].as_u64() == Some(1u64 << (tag - 1))
            && action.arguments["layout"] == layout
            && native.arguments["layout"] == layout
    } else if matches!(intent, Intent::CorrectMoveLeft) {
        if captured["speech_focus_uncertain"] != false {
            return Verdict::Reject("current_window_uncertain");
        }
        let Some(reference) = nonempty(&captured["speech_focus_ref"]) else {
            return Verdict::Reject("speech_focus_anchor_missing");
        };
        let (Some(a), Some(b)) = (
            current(captured, reference, &old),
            current(fresh, reference, &new),
        ) else {
            return Verdict::Reject("current_window_unavailable");
        };
        let at = captured["observed_monotonic_ns"].as_u64().unwrap();
        if a != b
            || !focus_history(captured, &a.token, at)
            || !focus_history(fresh, &a.token, at)
            || fresh
                .get("speech_focus_uncertain")
                .is_some_and(|v| v != false)
            || fresh
                .get("speech_focus_ref")
                .is_some_and(|v| !v.is_null() && v != reference)
        {
            return Verdict::Reject("current_window_or_focus_drift");
        }
        keys(&action.arguments, &["window_ref", "monitor_ref"])
            && keys(&native.arguments, &["window_token", "monitor_token"])
            && action.arguments["window_ref"] == a.reference
            && native.arguments["window_token"] == a.token
    } else {
        keys(&action.arguments, &["monitor_ref"]) && keys(&native.arguments, &["monitor_token"])
    };
    if !matches
        || action.arguments["monitor_ref"] != target.reference
        || native.arguments["monitor_token"] != target.token
    {
        return Verdict::Reject("original_value_or_target_mismatch");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return Verdict::Reject("original_plan_invalid");
    };
    if serde_json::to_value(rebuilt).ok() != serde_json::to_value(frozen).ok() {
        return Verdict::Reject("original_frozen_changed");
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
    fn captured() -> Value {
        let mut c = json!({"protocol_version":1,"wm_instance_id":"native","wm_pid":101,"wm_start_ticks":"12345","wake_generation":0,"assistant_permit":{"assistant_session_id":"session","generation":1},"connection_epoch":3,"session_unlocked":true,"observed_monotonic_ns":100,"tag_count":9,"windows_complete":true,"speech_focus_ref":"w1","speech_focus_uncertain":false,"focus_token":"native-w1","selected_monitor_token":"native-right","activation_monitor_token":"native-right","focus_history":[{"at_monotonic_ns":1,"focus_token":"native-w1"}],"available_actions":["focus_monitor","set_layout","move_window_to_monitor"],"monitors":[{"monitor_token":"native-left","monitor_ref":"m1","connector":"DP-1","name":"Left","geometry":[-1920,0,1920,1080],"visible_tag_mask":1,"layout":"tile","locked":false},{"monitor_token":"native-right","monitor_ref":"m2","connector":"HDMI-1","name":"Right","geometry":[0,0,1920,1080],"visible_tag_mask":1,"layout":"tile","locked":false}],"windows":[{"window_ref":"w1","target_token":"native-w1","id":"42","pid":202,"monitor_token":"native-right","tag_mask":1,"visible":true,"minimized":false}],"workspaces":[]});
        c["workspaces"]=json!(["native-left","native-right"].iter().flat_map(|token|(1u64..=9).map(move|tag|json!({"monitor_token":token,"tag_number":tag,"tag_mask":1u64<<(tag-1),"layout":"tile"}))).collect::<Vec<_>>());
        c
    }
    fn fresh(c: &Value) -> Value {
        let mut n = c.clone();
        n["observed_monotonic_ns"] = json!(200);
        n.as_object_mut().unwrap().remove("speech_focus_ref");
        n.as_object_mut().unwrap().remove("speech_focus_uncertain");
        n
    }
    fn plan(c: &Value, name: &str, args: Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":name,"arguments":args}],"question":null,"reason":null})).unwrap();
        let f = validate_plan(&p, c).unwrap();
        (p, f)
    }
    fn assessed(text: &str, p: &Plan, f: &FrozenPlan, c: &Value, n: &Value) -> Verdict {
        // Explicit local synthetic fixture provenance, never a claim of a
        // physical native observation. Production mint belongs to the actor.
        let evidence = bind_native_inventory(c, n, FRAME);
        assess(Some(text), p, f, c, n, FRAME, evidence.as_ref())
    }
    #[test]
    fn actual_whole_requests_prove_only_original_single_values_and_scope() {
        let c = captured();
        let n = fresh(&c);
        for (text, name, args) in [
            (
                "focus the HDMI screen",
                "focus_monitor",
                json!({"monitor_ref":"m2"}),
            ),
            (
                "把左屏第二个 tag 换成网格",
                "set_layout",
                json!({"monitor_ref":"m1","tag_number":2,"layout":"grid"}),
            ),
            (
                "把窗口移到右屏，不，是左屏",
                "move_window_to_monitor",
                json!({"window_ref":"w1","monitor_ref":"m1"}),
            ),
        ] {
            let (p, f) = plan(&c, name, args);
            let Verdict::Proved(proof) = assessed(text, &p, &f, &c, &n) else {
                panic!("{text}");
            };
            assert!(proof.accepts(&p, &f, FRAME));
            assert!(!proof.accepts(&p, &f, (4, 9)));
            let mut bad = p.clone();
            bad.reason = Some("different".into());
            assert!(!proof.accepts(&bad, &f, FRAME));
            let mut bad = f.clone();
            bad.plan_hash.push('a');
            assert!(!proof.accepts(&p, &bad, FRAME));
            let mut bad = f.clone();
            bad.requires_confirmation = true;
            assert!(!proof.accepts(&p, &bad, FRAME));
            assert!(matches!(
                assess(Some(text), &p, &f, &c, &n, FRAME, None),
                Verdict::Reject(_)
            ));
        }
        // Tag 2 is deliberately inactive: explicit complete scope is sufficient.
        assert_eq!(c["monitors"][0]["visible_tag_mask"], 1);
    }
    #[test]
    fn hdmi_connector_family_is_exact_and_unique_across_native_spellings() {
        for connector in ["HDMI-0", "HDMI-1", "HDMI-A-1", "HDMI-B-2"] {
            let mut c = captured();
            c["monitors"][1]["connector"] = json!(connector);
            let n = fresh(&c);
            let (p, f) = plan(&c, "focus_monitor", json!({"monitor_ref":"m2"}));
            assert!(
                matches!(
                    assessed("focus the HDMI screen", &p, &f, &c, &n),
                    Verdict::Proved(_)
                ),
                "{connector}"
            );
        }
        for connector in [
            "HDMI-01",
            "HDMI-A-0",
            "HDMI-B-01",
            "HDMI-A-4294967296",
            "HDMI-random",
            "HDMI-A-1 extra",
            "my HDMI-A-1",
            "HDMI-A-+1",
        ] {
            assert!(!hdmi(connector), "{connector}");
        }
        let mut native_x11 = captured();
        native_x11["monitors"][0]["connector"] = json!("DP-2");
        native_x11["monitors"][1]["connector"] = json!("DP-0");
        assert!(
            scene(&native_x11).is_some(),
            "actual zero-based RandR label is legal inventory"
        );
        let next = fresh(&native_x11);
        let (p, f) = plan(&native_x11, "focus_monitor", json!({"monitor_ref":"m2"}));
        assert!(matches!(
            assessed("focus the HDMI screen", &p, &f, &native_x11, &next),
            Verdict::Reject("hdmi_family_unresolved")
        ));
        let mut c = captured();
        c["monitors"][0]["connector"] = json!("HDMI-A-1");
        c["monitors"][1]["connector"] = json!("HDMI-B-2");
        let n = fresh(&c);
        let (p, f) = plan(&c, "focus_monitor", json!({"monitor_ref":"m2"}));
        assert!(matches!(
            assessed("focus the HDMI screen", &p, &f, &c, &n),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn complete_inventory_origin_geometry_and_hdmi_family_are_not_guessed() {
        let c = captured();
        let n = fresh(&c);
        let (p, f) = plan(&c, "focus_monitor", json!({"monitor_ref":"m2"}));
        let original = bind_native_inventory(&c, &n, FRAME).unwrap();
        let mut drift = n.clone();
        drift["monitors"][0]["name"] = json!("changed");
        assert!(matches!(
            assess(
                Some("focus the HDMI screen"),
                &p,
                &f,
                &c,
                &drift,
                FRAME,
                Some(&original)
            ),
            Verdict::Reject(_)
        ));
        for fault in [
            "two_hdmi",
            "no_connector",
            "unknown_connector",
            "three",
            "overlap",
            "unknown_geometry",
            "missing_raw_field",
            "false_complete",
            "truncated",
            "missing_protocol",
            "missing_windows_complete",
        ] {
            let mut a = c.clone();
            let mut b = n.clone();
            for ctx in [&mut a, &mut b] {
                match fault {
                    "two_hdmi" => ctx["monitors"][0]["connector"] = json!("HDMI-2"),
                    "no_connector" => ctx["monitors"][0]["connector"] = Value::Null,
                    "unknown_connector" => ctx["monitors"][0]["connector"] = json!("unknown"),
                    "three" => {
                        let r = ctx["monitors"][0].clone();
                        ctx["monitors"].as_array_mut().unwrap().push(r);
                    }
                    "overlap" => ctx["monitors"][0]["geometry"] = json!([-100, 0, 1920, 1080]),
                    "unknown_geometry" => ctx["monitors"][0]["geometry"] = Value::Null,
                    "false_complete" => ctx["monitors_complete"] = json!(false),
                    "truncated" => ctx["monitors_truncated"] = json!(true),
                    "missing_protocol" => {
                        ctx.as_object_mut().unwrap().remove("protocol_version");
                    }
                    "missing_windows_complete" => {
                        ctx.as_object_mut().unwrap().remove("windows_complete");
                    }
                    "missing_raw_field" => {}
                    _ => unreachable!(),
                }
            }
            if fault == "missing_raw_field" {
                b["monitors"][0].as_object_mut().unwrap().remove("name");
            }
            assert!(
                matches!(
                    assessed("focus the HDMI screen", &p, &f, &a, &b),
                    Verdict::Reject(_)
                ),
                "{fault}"
            );
        }
    }
    #[test]
    fn wrong_original_target_value_steps_identity_or_native_profile_is_a_veto() {
        let c = captured();
        let n = fresh(&c);
        let (p, f) = plan(
            &c,
            "set_layout",
            json!({"monitor_ref":"m1","tag_number":2,"layout":"grid"}),
        );
        let text = "把左屏第二个 tag 换成网格";
        for args in [
            json!({"monitor_ref":"m2","tag_number":2,"layout":"grid"}),
            json!({"monitor_ref":"m1","tag_number":3,"layout":"grid"}),
            json!({"monitor_ref":"m1","tag_number":2,"layout":"tile"}),
        ] {
            let (a, b) = plan(&c, "set_layout", args);
            assert!(matches!(assessed(text, &a, &b, &c, &n), Verdict::Reject(_)));
        }
        let mut bad = p.clone();
        bad.actions.push(p.actions[0].clone());
        assert!(matches!(
            assessed(text, &bad, &f, &c, &n),
            Verdict::Reject(_)
        ));
        let mut bad = f.clone();
        bad.actions[0].arguments["tag_mask"] = json!(4);
        assert!(matches!(
            assessed(text, &p, &bad, &c, &n),
            Verdict::Reject(_)
        ));
        for (key, value) in [
            ("wake_generation", json!(1)),
            (
                "assistant_permit",
                json!({"assistant_session_id":"session","generation":2}),
            ),
            ("wm_start_ticks", json!(12345)),
            ("wm_pid", json!(102)),
            ("connection_epoch", json!(4)),
            ("session_unlocked", json!(false)),
            ("observed_monotonic_ns", json!(100)),
        ] {
            let mut bad = n.clone();
            bad[key] = value;
            assert!(
                matches!(assessed(text, &p, &f, &c, &bad), Verdict::Reject(_)),
                "{key}"
            );
        }
        let mut inactive = c.clone();
        inactive["assistant_permit"] = Value::Null;
        let inactive_fresh = fresh(&inactive);
        let (inactive_plan, inactive_frozen) = plan(
            &inactive,
            "set_layout",
            json!({"monitor_ref":"m1","tag_number":2,"layout":"grid"}),
        );
        assert!(
            matches!(
                assessed(
                    text,
                    &inactive_plan,
                    &inactive_frozen,
                    &inactive,
                    &inactive_fresh
                ),
                Verdict::Proved(_)
            ),
            "explicit inactive state can prove only planning identity"
        );
        for value in [
            json!({}),
            json!({"assistant_session_id":"session","generation":0}),
            json!({"assistant_session_id":"session","generation":"1"}),
        ] {
            let mut old = c.clone();
            let mut new = n.clone();
            old["assistant_permit"] = value.clone();
            new["assistant_permit"] = value;
            assert!(matches!(
                assessed(text, &p, &f, &old, &new),
                Verdict::Reject(_)
            ));
        }
        let mut old = c.clone();
        let mut new = n.clone();
        old.as_object_mut().unwrap().remove("assistant_permit");
        new.as_object_mut().unwrap().remove("assistant_permit");
        assert!(
            matches!(assessed(text, &p, &f, &old, &new), Verdict::Reject(_)),
            "missing is not explicit inactive"
        );
        let mut bad = n.clone();
        bad["workspaces"][1]["layout"] = json!("scrolling");
        assert!(matches!(
            assessed(text, &p, &f, &c, &bad),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn current_window_is_an_owned_speech_anchor_not_prior_or_reused_identity() {
        let c = captured();
        let n = fresh(&c);
        let (p, f) = plan(
            &c,
            "move_window_to_monitor",
            json!({"window_ref":"w1","monitor_ref":"m1"}),
        );
        let text = "把窗口移到右屏，不，是左屏";
        for fault in [
            "uncertain",
            "no_speech",
            "no_history",
            "reuse",
            "focus_drift",
            "tags_drift",
            "history_transition",
            "truncated",
        ] {
            let mut a = c.clone();
            let mut b = n.clone();
            match fault {
                "uncertain" => a["speech_focus_uncertain"] = json!(true),
                "no_speech" => a["speech_focus_ref"] = Value::Null,
                "no_history" => a["focus_history"] = Value::Null,
                "reuse" => b["windows"][0]["target_token"] = json!("replacement"),
                "focus_drift" => b["focus_token"] = json!("other"),
                "tags_drift" => b["windows"][0]["tag_mask"] = json!(2),
                "history_transition" => {
                    b["focus_history"] = json!([{"at_monotonic_ns":1,"focus_token":"native-w1"},{"at_monotonic_ns":150,"focus_token":"other"},{"at_monotonic_ns":160,"focus_token":"native-w1"}])
                }
                "truncated" => b["windows_complete"] = json!(false),
                _ => unreachable!(),
            }
            assert!(
                matches!(assessed(text, &p, &f, &a, &b), Verdict::Reject(_)),
                "{fault}"
            );
        }
    }
    #[test]
    fn only_finite_complete_grammar_can_supply_native_single_evidence() {
        assert!(!recognizes(None));
        for text in [
            "focus the external screen",
            "focus the HDMI screen and close the browser",
            "he said focus the HDMI screen",
            "不要focus the HDMI screen",
            "把左屏第二个 tag 换成网格然后关闭窗口",
            "把左屏第二三个tag换成网格",
            "把左屏第三个，不对，第四个tag换成网格",
            "把左屏第二个tag换成未知",
            "把第三屏第二个tag换成网格",
            "把左屏第二个tag换成全屏布局布局",
            "把窗口移到右屏，不，是左屏，然后关闭",
            "把那个窗口移到右屏，不，是左屏",
            "把窗口移到右屏，不，是左屏，不，是右屏",
            "不要把窗口移到右屏，不，是左屏",
            "标题：把窗口移到右屏，不，是左屏",
            "‘把窗口移到右屏，不，是左屏’",
            "取消任务",
            "停止监听",
            "退出JWM",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
    }
    #[test]
    #[ignore = "requires explicit private actual read-only native pair; no desktop effects"]
    fn actual_full_native_inventory_pair_accepts_protocol_and_rejects_absent_hdmi() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("JWM_EVA_ACTUAL_MONITOR_PAIR_DIR")
                .expect("explicit actual native pair required"),
        );
        let read = |name: &str| -> Value {
            let v: Value = serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap();
            assert_eq!(v["success"], true);
            v["data"].clone()
        };
        let mut captured = read("context-1.json");
        let mut fresh = read("context-2.json");
        let schema = read("schema.json");
        jwm_eva_core::protocol::validate_assistant_features(
            &schema,
            jwm_eva_core::protocol::ASSISTANT_EXECUTION_FEATURES,
        )
        .unwrap();
        let actions = schema["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["support_status"] == "implemented")
            .map(|s| s["canonical_name"].clone())
            .collect::<Vec<_>>();
        captured["available_actions"] = serde_json::json!(actions);
        fresh["available_actions"] = captured["available_actions"].clone();
        assert!(captured["wm_start_ticks"].is_string());
        assert!(captured["assistant_permit"].is_null());
        assert!(captured.get("monitors_complete").is_none());
        assert!(
            captured["monitors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["connector"] == "DP-0")
        );
        let frame = (1, 1);
        let evidence=bind_native_inventory(&captured,&fresh,frame).expect("actual full-native RPC inventories can mint origin binding without fake completeness fields");
        let reference = captured["monitors"][0]["monitor_ref"].clone();
        let plan:Plan=serde_json::from_value(serde_json::json!({"disposition":"execute","actions":[{"name":"focus_monitor","arguments":{"monitor_ref":reference}}],"question":null,"reason":null})).unwrap();
        let frozen = validate_plan(&plan, &captured).unwrap();
        let result = assess(
            Some("focus the HDMI screen"),
            &plan,
            &frozen,
            &captured,
            &fresh,
            frame,
            Some(&evidence),
        );
        assert!(
            matches!(result, Verdict::Reject("hdmi_family_unresolved")),
            "actual two DP outputs must not invent HDMI or fail native protocol shape: {result:?}"
        );
        assert!(matches!(
            assess(
                Some("focus the HDMI screen"),
                &plan,
                &frozen,
                &captured,
                &fresh,
                frame,
                None
            ),
            Verdict::Reject("native_inventory_origin_unavailable")
        ));
    }
    #[test]
    fn complete_primary_focus_uses_native_unique_role_not_selected_order_or_names() {
        let mut c = captured();
        c["monitors"][0]["primary"] = json!(true);
        c["monitors"][1]["primary"] = json!(false);
        let n = fresh(&c);
        let (p, f) = plan(&c, "focus_monitor", json!({"monitor_ref":"m1"}));
        assert!(matches!(
            assessed("切回主显示器", &p, &f, &c, &n),
            Verdict::Proved(_)
        ));
        let (wrong_p, wrong_f) = plan(&c, "focus_monitor", json!({"monitor_ref":"m2"}));
        assert!(matches!(
            assessed("切回主显示器", &wrong_p, &wrong_f, &c, &n),
            Verdict::Reject(_)
        ));
        for role in [Value::Null, json!(true)] {
            let mut a = c.clone();
            a["monitors"][1]["primary"] = role;
            let b = fresh(&a);
            assert!(matches!(
                assessed("切回主显示器", &p, &f, &a, &b),
                Verdict::Reject(_)
            ));
        }
        let mut a = c.clone();
        a["monitors"][0]["primary"] = json!(false);
        let b = fresh(&a);
        assert!(matches!(
            assessed("切回主显示器", &p, &f, &a, &b),
            Verdict::Reject(_)
        ));
        for text in [
            "主显示器是什么？",
            "切回第三个显示器",
            "把窗口移到主显示器",
            "切回主显示器然后关闭窗口",
            "标题写着切回主显示器",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        let mut changed = n.clone();
        changed["monitors"][0]["primary"] = json!(false);
        assert!(matches!(
            assessed("切回主显示器", &p, &f, &c, &changed),
            Verdict::Reject(_)
        ));
    }
    #[test]
    fn right_workspace_switch_keeps_original_monitor_tag_and_complete_geometry() {
        let mut c = captured();
        c["available_actions"]
            .as_array_mut()
            .unwrap()
            .push(json!("switch_tag"));
        let n = fresh(&c);
        let (p, f) = plan(&c, "switch_tag", json!({"monitor_ref":"m2","tag_number":3}));
        assert!(matches!(
            assessed("右屏切到第三个 tag", &p, &f, &c, &n),
            Verdict::Proved(_)
        ));
        let (wp, wf) = plan(&c, "switch_tag", json!({"monitor_ref":"m1","tag_number":3}));
        assert!(matches!(
            assessed("右屏切到第三个 tag", &wp, &wf, &c, &n),
            Verdict::Reject(_)
        ));
        let (wp, wf) = plan(&c, "switch_tag", json!({"monitor_ref":"m2","tag_number":2}));
        assert!(matches!(
            assessed("右屏切到第三个 tag", &wp, &wf, &c, &n),
            Verdict::Reject(_)
        ));
        for text in [
            "第三屏切到第三个tag",
            "右屏切到下一个tag",
            "右屏切到第三个tag然后关窗",
            "右屏切到第三个tag吗",
            "右屏当前屏切到第三个tag",
            "右屏当前屏幕切到第三个tag",
            "右屏切到当前屏第三个tag",
            "右屏左屏切到第三个tag",
            "窗口标题右屏切到第三个tag",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        let mut a = c.clone();
        a["monitors"][1]["geometry"] = c["monitors"][0]["geometry"].clone();
        assert!(matches!(
            assessed("右屏切到第三个 tag", &p, &f, &a, &fresh(&a)),
            Verdict::Reject(_)
        ));
        let mut a = n.clone();
        let target = c["monitors"][1]["monitor_token"].clone();
        a["workspaces"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|w| w["monitor_token"] == target && w["tag_number"] == 3)
            .unwrap()["layout"] = json!("scrolling");
        assert!(matches!(
            assessed("右屏切到第三个 tag", &p, &f, &c, &a),
            Verdict::Reject(_)
        ));
    }
}
