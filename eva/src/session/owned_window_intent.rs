//! Complete owned window intent evidence. No action, reference or permit is
//! constructed here. Call only after a valid correlated review and a genuine
//! fresh native query; transport/billing failures cannot be bypassed by proof.
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan, validate_plan};
use serde_json::Value;
use std::collections::BTreeSet;

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
    noop: Option<&'static str>,
}
impl Proof {
    pub(super) fn accepts(&self, plan: &Plan, frozen: &FrozenPlan, frame: (u64, u64)) -> bool {
        self.frame == frame
            && self.hash == frozen.plan_hash
            && self.candidate == digest(plan)
            && self.frozen == digest(frozen)
    }
    pub(super) fn factual_noop_reason(&self) -> Option<&'static str> {
        self.noop
    }
}
fn digest(value: &impl serde::Serialize) -> String {
    jwm_eva_core::protocol::digest(&serde_json::to_value(value).expect("typed plan serializes"))
}
#[derive(Debug, Clone, Copy)]
enum Subject {
    Current,
    VerifiedPrior,
}
#[derive(Debug)]
enum Intent {
    Move(Subject, Vec<u32>),
    Unminimize,
    Unmaximize,
    Sticky(bool),
    Resize {
        horizontal: bool,
        positive: bool,
        repeat: bool,
    },
    Snap(&'static str),
}
fn chinese(n: u32) -> String {
    const D: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
    match n {
        0..=9 => D[n as usize].into(),
        10 => "十".into(),
        11..=19 => format!("十{}", D[(n % 10) as usize]),
        _ if n.is_multiple_of(10) => format!("{}十", D[(n / 10) as usize]),
        _ => format!("{}十{}", D[(n / 10) as usize], D[(n % 10) as usize]),
    }
}
fn number(text: &str) -> Option<u32> {
    let text = text.trim().strip_prefix('第').unwrap_or(text.trim());
    let text = text
        .strip_suffix('个')
        .or_else(|| text.strip_suffix('号'))
        .unwrap_or(text)
        .trim();
    if text == "两" {
        return Some(2);
    }
    if !text.is_empty()
        && text.len() <= 4
        && text.bytes().all(|c| c.is_ascii_digit())
        && (text.len() == 1 || !text.starts_with('0'))
    {
        return text.parse().ok();
    }
    (0..=99).find(|n| chinese(*n) == text)
}
fn tags(text: &str) -> Option<Vec<u32>> {
    let text = text.trim();
    let body = ["工作区", "桌面", "tag"]
        .iter()
        .find_map(|unit| text.strip_suffix(*unit))?
        .trim();
    if ["工作区", "桌面", "tag"]
        .iter()
        .any(|word| body.contains(word))
    {
        return None;
    }
    let mut body = body.to_owned();
    for conjunction in ["和", "与", "及", "、"] {
        body = body.replace(conjunction, ",");
    }
    let values = body.split(',').map(number).collect::<Option<Vec<_>>>()?;
    (!values.is_empty() && values.len() <= 32).then_some(values)
}
fn parse(text: Option<&str>) -> Option<Intent> {
    let text = text?;
    if text.len() > 256
        || text
            .chars()
            .any(|c| c.is_control() || "\"'“”‘’「」『』`?？".contains(c))
    {
        return None;
    }
    let text = text
        .trim()
        .trim_end_matches(['。', '.', '!', '！'])
        .trim()
        .to_ascii_lowercase();
    let text = text.strip_prefix('请').unwrap_or(&text).trim();
    for subject in ["这个窗口", "当前窗口"] {
        if [
            format!("取消{subject}最小化"),
            format!("取消{subject}的最小化"),
        ]
        .iter()
        .any(|form| form == text)
        {
            return Some(Intent::Unminimize);
        }
    }
    if matches!(text, "恢复这个最小化窗口" | "恢复当前最小化窗口") {
        return Some(Intent::Unminimize);
    }
    if text == "取消最大化"
        || ["这个窗口", "当前窗口"].iter().any(|subject| {
            text == format!("取消{subject}最大化") || text == format!("取消{subject}的最大化")
        })
    {
        return Some(Intent::Unmaximize);
    }
    // All-tag visibility is sticky. Bare '置顶'/always-on-top is deliberately
    // absent: it does not uniquely request visibility across workspaces.
    for subject in ["这个窗口", "当前窗口"] {
        if [
            format!("跨工作区显示{subject}"),
            format!("让{subject}跨工作区显示"),
            format!("把{subject}设为跨工作区显示"),
            format!("{subject}在所有工作区显示"),
        ]
        .iter()
        .any(|form| form.as_str() == text)
            || ["所有 tag", "所有tag", "所有工作区", "所有桌面"]
                .iter()
                .any(|scope| text == format!("把{subject}置顶到{scope}"))
        {
            return Some(Intent::Sticky(true));
        }
        if [
            format!("不要跨工作区显示{subject}"),
            format!("取消{subject}跨工作区显示"),
            format!("取消{subject}的跨工作区显示"),
            format!("让{subject}不再跨工作区显示"),
        ]
        .iter()
        .any(|form| form.as_str() == text)
        {
            return Some(Intent::Sticky(false));
        }
    }
    for prefix in [
        "make this window a little ",
        "make the current window a little ",
    ] {
        if let Some(axis) = text.strip_prefix(prefix) {
            let (horizontal, positive) = match axis {
                "wider" => (true, true),
                "narrower" => (true, false),
                "taller" => (false, true),
                "shorter" => (false, false),
                _ => return None,
            };
            return Some(Intent::Resize {
                horizontal,
                positive,
                repeat: false,
            });
        }
    }
    // Exactly one withdrawn snap operand, still the same explicitly current
    // window. Parse both operands; never discard an unknown earlier source.
    for marker in ["，不对，", ",不对,"] {
        if let Some((first, last)) = text.split_once(marker) {
            current_snap(first.trim(), true)?;
            return Some(Intent::Snap(snap_direction(last.trim())?));
        }
    }
    for subject in ["这个窗口", "当前窗口", ""] {
        if let Some(rest) = text.strip_prefix(subject) {
            let (rest, repeat) = rest
                .strip_prefix('再')
                .map(|s| (s, true))
                .unwrap_or((rest, false));
            for (word, horizontal, positive) in [
                ("宽", true, true),
                ("窄", true, false),
                ("高", false, true),
                ("矮", false, false),
            ] {
                if rest == format!("{word}一点") || rest == format!("{word}一点点") {
                    return Some(Intent::Resize {
                        horizontal,
                        positive,
                        repeat,
                    });
                }
            }
        }
    }
    if let Some(direction) = current_snap(text, false) {
        return Some(Intent::Snap(direction));
    }
    for (subject, kind) in [
        ("这个窗口", Subject::Current),
        ("当前窗口", Subject::Current),
        ("它", Subject::VerifiedPrior),
    ] {
        for verb in ["放在", "放到", "移到"] {
            if let Some(rest) = text.strip_prefix(&format!("把{subject}{verb}")) {
                return Some(Intent::Move(kind, tags(rest)?));
            }
        }
    }
    None
}
fn snap_direction(text: &str) -> Option<&'static str> {
    Some(match text {
        "左边" | "左半边" | "屏幕左边" | "左半屏" | "left half" => "left",
        "右边" | "右半边" | "屏幕右边" | "右半屏" | "right half" => "right",
        "左上角" | "top-left" | "top left corner" => "top-left",
        "右上角" | "top-right" | "top right corner" => "top-right",
        "左下角" | "bottom-left" | "bottom left corner" => "bottom-left",
        "右下角" | "bottom-right" | "bottom right corner" => "bottom-right",
        "全屏" | "铺满屏幕" => "maximize",
        _ => return None,
    })
}
fn current_snap(text: &str, correction_operand: bool) -> Option<&'static str> {
    for prefix in [
        "这个窗口放",
        "当前窗口放",
        "这个窗口贴到",
        "当前窗口贴到",
        "把这个窗口贴到",
        "把当前窗口贴到",
        "把这个贴到",
        "把这个窗口放到",
        "把当前窗口放到",
        "put this window on the ",
    ] {
        if let Some(rest) = text.strip_prefix(prefix)
            && let Some(direction) = snap_direction(rest)
        {
            return Some(direction);
        }
    }
    // This short current NP is admitted only in the complete correction
    // production, not as a generic way to choose another window or source.
    if correction_operand {
        return text.strip_prefix("这个放").and_then(snap_direction);
    }
    None
}
pub(super) fn recognizes(text: Option<&str>) -> bool {
    parse(text).is_some()
}
/// Question-only fallback for an explicitly unresolved current target. This
/// neither broadens executable grammar nor supplies any reference/permission.
pub(super) fn uncertain_current_question(
    text: Option<&str>,
    context: &Value,
) -> Option<&'static str> {
    if context["speech_focus_uncertain"] != true {
        return None;
    }
    let text = text?;
    if text.len() > 256
        || text
            .chars()
            .any(|c| c.is_control() || "\"'“”‘’「」『』`?？".contains(c))
    {
        return None;
    }
    let current = match parse(Some(text)) {
        Some(Intent::Move(Subject::VerifiedPrior, _)) | None => false,
        Some(_) => true,
    };
    let body = text.trim().trim_end_matches(['。', '.', '!', '！']).trim();
    let body = body.strip_prefix('请').unwrap_or(body).trim();
    let bounded_repeat = [
        "把这个窗口再放到",
        "把当前窗口再放到",
        "这个窗口再放到",
        "当前窗口再放到",
    ]
    .iter()
    .any(|prefix| body.strip_prefix(prefix).and_then(snap_direction).is_some());
    (current || bounded_repeat).then_some("当前无法确认本轮指向的窗口，请重新指定要操作的窗口。")
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
        if permit.assistant_session_id.is_empty() {
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
    raw_id: Option<Value>,
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
        raw_id: row.get("id").cloned(),
    })
}
fn current_reference(context: &Value) -> Option<&str> {
    if context["speech_focus_uncertain"] != false {
        return None;
    }
    let reference = nonempty(&context["speech_focus_ref"])?;
    let w = window(context, reference)?;
    if context["focus_token"] != w.token
        || context["selected_monitor_token"] != w.monitor
        || context
            .get("activation_monitor_token")
            .is_some_and(|m| !m.is_null() && m.as_str() != Some(w.monitor.as_str()))
    {
        return None;
    }
    Some(reference)
}
fn mask(values: &[u32], count: u64) -> Option<u64> {
    let mut seen = BTreeSet::new();
    let mut bits = 0u64;
    for tag in values {
        if *tag == 0 || u64::from(*tag) > count || *tag > 32 || !seen.insert(*tag) {
            return None;
        }
        bits |= 1u64 << (*tag - 1);
    }
    (!seen.is_empty()).then_some(bits)
}
// New current edits require unchanged complete window data, not only a
// matching semantic ref. Missing flags are unknown, never satisfied facts.
fn stable_current_row(captured: &Value, fresh: &Value, anchor: &Window) -> bool {
    let row = |context: &Value| {
        context["windows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|w| w["target_token"] == anchor.token))
            .cloned()
    };
    let (Some(old), Some(new)) = (row(captured), row(fresh)) else {
        return false;
    };
    if old != new || nonempty(&old["id"]).is_none() || captured["assistant_permit"].is_null() {
        return false;
    }
    if old
        .get("pid")
        .is_some_and(|pid| !pid.is_null() && pid.as_u64().is_none_or(|n| n == 0))
    {
        return false;
    }
    for flag in ["visible", "minimized"] {
        if old.get(flag).is_some_and(|v| !v.is_boolean()) {
            return false;
        }
    }
    fresh["focus_token"] == anchor.token
        && fresh["selected_monitor_token"] == anchor.monitor
        && captured["activation_monitor_token"] == anchor.monitor
        && fresh["activation_monitor_token"] == anchor.monitor
}
fn stable_container_row(captured: &Value, fresh: &Value, anchor: &Window) -> bool {
    let source = |context: &Value| {
        context["monitors"]
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["monitor_token"] == anchor.monitor)
            })
            .cloned()
    };
    match (source(captured), source(fresh)) {
        (Some(old), Some(new)) => old == new && old["locked"] == false,
        _ => false,
    }
}
#[derive(PartialEq)]
struct Container {
    reference: String,
    token: String,
    tag: u64,
    mask: u64,
    monitor: Value,
    workspace: Value,
}
fn current_container(context: &Value, anchor: &Window) -> Option<Container> {
    let mut rows = context["monitors"]
        .as_array()?
        .iter()
        .filter(|m| m["monitor_token"] == anchor.monitor);
    let m = rows.next()?;
    if rows.next().is_some()
        || m["locked"] != false
        || context["selected_monitor_token"] != anchor.monitor
        || context["activation_monitor_token"] != anchor.monitor
    {
        return None;
    }
    let mask = m["visible_tag_mask"].as_u64().filter(|v| {
        *v > 0 && *v <= u32::MAX as u64 && v.count_ones() == 1 && v & anchor.tags != 0
    })?;
    let count = context["tag_count"]
        .as_u64()
        .filter(|n| (1..=32).contains(n))?;
    let tag = u64::from(mask.trailing_zeros()) + 1;
    if tag > count {
        return None;
    }
    let geometry = m["geometry"].as_array().filter(|g| g.len() == 4)?;
    let (x, y, w, h) = (
        geometry[0].as_i64()?,
        geometry[1].as_i64()?,
        geometry[2].as_i64()?,
        geometry[3].as_i64()?,
    );
    if w <= 0 || h <= 0 {
        return None;
    }
    x.checked_add(w)?;
    y.checked_add(h)?;
    let row = context["windows"]
        .as_array()?
        .iter()
        .find(|w| w["target_token"] == anchor.token)?;
    if row["visible"] == false || row["minimized"] == true {
        return None;
    }
    let mut workspaces = context["workspaces"]
        .as_array()?
        .iter()
        .filter(|w| w["monitor_token"] == anchor.monitor && w["tag_number"].as_u64() == Some(tag));
    let workspace = workspaces.next()?;
    let layout = jwm_eva_core::actions::normalize_layout(m["layout"].as_str()?)?;
    let workspace_layout = jwm_eva_core::actions::normalize_layout(workspace["layout"].as_str()?)?;
    if workspaces.next().is_some()
        || workspace["tag_mask"].as_u64() != Some(mask)
        || layout != workspace_layout
    {
        return None;
    }
    Some(Container {
        reference: nonempty(&m["monitor_ref"])?.into(),
        token: anchor.monitor.clone(),
        tag,
        mask,
        monitor: m.clone(),
        workspace: workspace.clone(),
    })
}
fn container_matches(
    captured: &Value,
    fresh: &Value,
    anchor: &Window,
    semantic: &Value,
    native: &Value,
) -> bool {
    let (Some(old), Some(new)) = (
        current_container(captured, anchor),
        current_container(fresh, anchor),
    ) else {
        return false;
    };
    old == new
        && semantic["monitor_ref"] == old.reference
        && native["monitor_token"] == old.token
        && semantic["tag_number"].as_u64() == Some(old.tag)
        && native["tag_mask"].as_u64() == Some(old.mask)
}
// Exact rational comparison of the canonical JSON number, never an epsilon or
// a pixel amount. Registry uses work-area fractions; +/-0.05 is +/-1/20.
fn exact_twentieth(value: &Value, positive: bool) -> bool {
    let Some(number) = value.as_number() else {
        return false;
    };
    let text = number.to_string();
    let negative = text.starts_with('-');
    if negative == positive {
        return false;
    }
    let text = text.strip_prefix('-').unwrap_or(&text);
    let (mantissa, exponent) = text
        .split_once(['e', 'E'])
        .map(|(a, b)| (a, b.parse::<i32>().ok()))
        .unwrap_or((text, Some(0)));
    let Some(exponent) = exponent.filter(|n| (-18..=18).contains(n)) else {
        return false;
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || fraction.len() > 18
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let Ok(mut num) = format!("{whole}{fraction}").parse::<u128>() else {
        return false;
    };
    let Some(mut den) = 10u128.checked_pow(fraction.len() as u32) else {
        return false;
    };
    let Some(scale) = 10u128.checked_pow(exponent.unsigned_abs()) else {
        return false;
    };
    if exponent >= 0 {
        let Some(next) = num.checked_mul(scale) else {
            return false;
        };
        num = next;
    } else {
        let Some(next) = den.checked_mul(scale) else {
            return false;
        };
        den = next;
    }
    num.checked_mul(20) == Some(den)
}
/// Unsupported grammar supplies no permission. Recognized-but-unproved intent
/// is a veto, not a fallback to the old scope-only model-allow path.
pub(super) fn assess(
    text: Option<&str>,
    latest_grounding: &Value,
    plan: &Plan,
    frozen: &FrozenPlan,
    captured: &Value,
    fresh: &Value,
    frame: (u64, u64),
) -> Verdict {
    let Some(intent) = parse(text) else {
        return Verdict::Unsupported;
    };
    let reject = |why| Verdict::Reject(why);
    // Native snap maximize is a floating-only TOGGLE, not an absolute request
    // to fill the screen. Never certify that semantic substitution, even if
    // the schema lists the direction. The original candidate stays unchanged.
    if matches!(intent, Intent::Snap("maximize")) {
        return reject("snap_maximize_toggle_is_not_absolute_maximize");
    }

    if frame.0 == 0
        || frame.1 == 0
        || plan.disposition != Disposition::Execute
        || frozen.disposition != Disposition::Execute
        || plan.actions.len() != 1
        || frozen.actions.len() != 1
        || frozen.requires_confirmation
    {
        return reject("not_single_original_ordinary_execute");
    }
    if !identity(captured, fresh)
        || captured["tag_count"] != fresh["tag_count"]
        || frozen.wm_instance_id != captured["wm_instance_id"].as_str().unwrap_or("")
    {
        return reject("native_identity_or_freshness_unverified");
    }
    let subject = match &intent {
        Intent::Move(s, _) => *s,
        Intent::Unminimize
        | Intent::Unmaximize
        | Intent::Sticky(_)
        | Intent::Resize { .. }
        | Intent::Snap(_) => Subject::Current,
    };
    let wanted = match subject {
        Subject::Current => current_reference(captured),
        Subject::VerifiedPrior => {
            // This DTO MUST be actor FrozenInput::latest_grounding(), never a
            // model history/result. Native failures/unknown project to null.
            if latest_grounding
                .get("status")
                .is_some_and(|v| v != "succeeded")
            {
                return reject("prior_result_not_verified");
            }
            nonempty(&latest_grounding["window_ref"])
        }
    };
    let Some(wanted) = wanted else {
        return reject("owned_window_source_unverified");
    };
    let (Some(old), Some(new)) = (window(captured, wanted), window(fresh, wanted)) else {
        return reject("native_window_not_unique_live");
    };
    if old != new
        || (matches!(subject, Subject::Current)
            && (fresh["focus_token"] != old.token
                || fresh["selected_monitor_token"] != old.monitor
                || captured.get("activation_monitor_token")
                    != fresh.get("activation_monitor_token")
                || fresh
                    .get("speech_focus_ref")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some(old.reference.as_str()))
                || fresh
                    .get("speech_focus_uncertain")
                    .is_some_and(|v| !v.is_null() && v != false)))
    {
        return reject("native_window_identity_or_source_changed");
    }
    let action = &plan.actions[0];
    let native = &frozen.actions[0];
    let name = match &intent {
        Intent::Move(_, tags) if tags.len() == 1 => "move_window_to_tag",
        Intent::Move(_, _) => "move_window_to_tags",
        Intent::Unminimize => "set_window_minimized",
        Intent::Unmaximize => "set_window_maximized",
        Intent::Sticky(_) => "set_window_sticky",
        Intent::Resize {
            horizontal: true, ..
        } => "resize_horizontal",
        Intent::Resize {
            horizontal: false, ..
        } => "resize_vertical",
        Intent::Snap(_) => "snap_window",
    };
    let specs = jwm_eva_core::actions::semantic_actions();
    if specs
        .iter()
        .find(|s| s.canonical_name == name)
        .is_none_or(|s| {
            !matches!(s.risk_class.as_str(), "ordinary" | "reversible")
                || s.target_resolver
                    != match name {
                        "move_window_to_tags" | "resize_horizontal" | "resize_vertical" => {
                            "workspace"
                        }
                        "snap_window" => "selected_window",
                        _ => "window",
                    }
                || (!matches!(
                    name,
                    "resize_horizontal" | "resize_vertical" | "snap_window"
                ) && !s.undo_strategy.starts_with("restore_"))
        })
        || !captured["available_actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|a| a == name))
    {
        return reject("ordinary_window_capability_unavailable");
    }
    let Ok(rebuilt) = validate_plan(plan, captured) else {
        return reject("original_plan_invalid");
    };
    if digest(&rebuilt) != digest(frozen) {
        return reject("original_frozen_changed");
    }
    if action.name != name
        || native.name != name
        || action.arguments["window_ref"] != old.reference
        || native.arguments["window_token"] != old.token
    {
        return reject("owned_operation_or_window_mismatch");
    }
    if matches!(
        intent,
        Intent::Unmaximize | Intent::Sticky(_) | Intent::Resize { .. } | Intent::Snap(_)
    ) && !stable_current_row(captured, fresh, &old)
    {
        return reject("native_current_window_profile_changed");
    }
    if let Intent::Resize { repeat: true, .. } = &intent
        && latest_grounding.get("window_ref").is_some_and(|reference| {
            !reference.is_null() && reference.as_str() != Some(old.reference.as_str())
        })
    {
        return reject("repeat_verified_window_conflict");
    }
    let mut noop = None;
    match &intent {
        Intent::Move(_, tags) => {
            let Some(count) = captured["tag_count"]
                .as_u64()
                .filter(|n| (1..=32).contains(n))
            else {
                return reject("invalid_native_tag_count");
            };
            let Some(expected) = mask(tags, count) else {
                return reject("owned_destination_tags_invalid");
            };
            let field = if tags.len() == 1 {
                "tag_number"
            } else {
                "tag_numbers"
            };
            if !keys(&action.arguments, &["window_ref", field])
                || !keys(&native.arguments, &["window_token", "tag_mask"])
                || native.arguments["tag_mask"].as_u64() != Some(expected)
            {
                return reject("owned_destination_tags_mismatch");
            }
            let actual = if tags.len() == 1 {
                action.arguments["tag_number"]
                    .as_u64()
                    .filter(|n| *n <= u32::MAX as u64)
                    .map(|n| vec![n as u32])
            } else {
                action.arguments["tag_numbers"].as_array().and_then(|a| {
                    a.iter()
                        .map(|v| {
                            v.as_u64()
                                .filter(|n| *n <= u32::MAX as u64)
                                .map(|n| n as u32)
                        })
                        .collect::<Option<Vec<_>>>()
                })
            };
            if actual.as_ref().and_then(|a| mask(a, count)) != Some(expected) {
                return reject("owned_destination_tags_mismatch");
            }
        }
        Intent::Sticky(enabled) => {
            if !keys(&action.arguments, &["window_ref", "enabled"])
                || !keys(&native.arguments, &["window_token", "enabled"])
                || action.arguments["enabled"].as_bool() != Some(*enabled)
                || native.arguments["enabled"].as_bool() != Some(*enabled)
            {
                return reject("owned_sticky_value_mismatch");
            }
            if !stable_container_row(captured, fresh, &old) {
                return reject("native_sticky_container_drift");
            }
            let row = fresh["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["target_token"] == old.token)
                .unwrap();
            if row
                .get("sticky")
                .is_some_and(|v| !v.is_null() && !v.is_boolean())
            {
                return reject("native_sticky_state_malformed");
            }
            // Deliberately retain the original setter even when the exposed
            // flag is equal. This proof never claims sticky execution or Noop.
        }
        Intent::Unmaximize => {
            if !keys(&action.arguments, &["window_ref", "enabled"])
                || !keys(&native.arguments, &["window_token", "enabled"])
                || action.arguments["enabled"] != false
                || native.arguments["enabled"] != false
            {
                return reject("owned_maximized_value_mismatch");
            }
            let row = fresh["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["target_token"] == old.token)
                .unwrap();
            if row
                .get("maximized")
                .is_some_and(|v| !v.is_null() && !v.is_boolean())
            {
                return reject("native_maximized_state_malformed");
            }
            if row["maximized"] == false
                && ["maximized_horz", "maximized_vert"]
                    .iter()
                    .all(|key| row.get(*key).is_some_and(|v| v == false))
            {
                noop = Some("当前窗口未处于最大化状态，无需取消；本次未执行桌面操作。");
            }
        }
        Intent::Resize { positive, .. } => {
            if !container_matches(captured, fresh, &old, &action.arguments, &native.arguments)
                || !keys(
                    &action.arguments,
                    &["window_ref", "monitor_ref", "tag_number", "value"],
                )
                || !keys(
                    &native.arguments,
                    &["window_token", "monitor_token", "tag_mask", "value"],
                )
                || !exact_twentieth(&action.arguments["value"], *positive)
                || !exact_twentieth(&native.arguments["value"], *positive)
            {
                return reject("owned_axis_scope_or_increment_mismatch");
            }
        }
        Intent::Snap(direction) => {
            if !jwm_eva_core::actions::semantic_actions()
                .iter()
                .find(|s| s.canonical_name == "snap_window")
                .is_some_and(|s| {
                    s.parameters_schema["properties"]["direction"]["enum"]
                        .as_array()
                        .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(*direction)))
                })
                || !container_matches(captured, fresh, &old, &action.arguments, &native.arguments)
                || !keys(
                    &action.arguments,
                    &["window_ref", "monitor_ref", "tag_number", "direction"],
                )
                || !keys(
                    &native.arguments,
                    &["window_token", "monitor_token", "tag_mask", "direction"],
                )
                || action.arguments["direction"] != *direction
                || native.arguments["direction"] != *direction
            {
                return reject("owned_snap_scope_or_direction_mismatch");
            }
        }
        Intent::Unminimize => {
            if !keys(&action.arguments, &["window_ref", "minimized"])
                || !keys(&native.arguments, &["window_token", "minimized"])
                || action.arguments["minimized"] != false
                || native.arguments["minimized"] != false
            {
                return reject("owned_minimized_value_mismatch");
            }
            let row = fresh["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["target_token"] == old.token)
                .unwrap();
            if row
                .get("minimized")
                .is_some_and(|v| !v.is_null() && !v.is_boolean())
            {
                return reject("native_minimized_state_malformed");
            }
            if row["minimized"] == false {
                noop = Some("当前窗口未处于最小化状态，无需恢复；本次未执行桌面操作。");
            }
        }
    }
    Verdict::Proved(Proof {
        hash: frozen.plan_hash.clone(),
        candidate: digest(plan),
        frozen: digest(frozen),
        frame,
        noop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const FRAME: (u64, u64) = (5, 9);
    fn context() -> Value {
        json!({"wm_instance_id":"wm","wm_pid":4242,"wm_start_ticks":"100","wake_generation":0,"assistant_permit":{"assistant_session_id":"test-owned","generation":1},"observed_monotonic_ns":100,"session_unlocked":true,"windows_complete":true,"tag_count":9,"selected_monitor_token":"m-native1","activation_monitor_token":"m-native1","speech_focus_ref":"w1","speech_focus_uncertain":false,"focus_token":"w-native1","available_actions":["move_window_to_tag","move_window_to_tags","set_window_minimized"],"monitors":[{"monitor_ref":"m1","monitor_token":"m-native1","locked":false},{"monitor_ref":"m2","monitor_token":"m-native2","locked":false}],"windows":[{"window_ref":"w1","target_token":"w-native1","id":"42","monitor_token":"m-native1","tag_mask":1,"minimized":false},{"window_ref":"w2","target_token":"w-native2","id":"43","monitor_token":"m-native1","tag_mask":1,"minimized":false}]})
    }
    fn fresh(old: &Value) -> Value {
        let mut new = old.clone();
        new["observed_monotonic_ns"] = json!(200);
        new.as_object_mut().unwrap().remove("speech_focus_ref");
        new.as_object_mut()
            .unwrap()
            .remove("speech_focus_uncertain");
        new
    }
    fn prior(status: &str) -> Value {
        crate::language::conversation_grounding(
            &json!({"status":status,"steps":[{"name":"focus_window","status":status,"targets":{"window_ref":"w1"}}]}),
        )
    }
    fn plan(name: &str, args: Value, context: &Value) -> (Plan, FrozenPlan) {
        let p:Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":name,"arguments":args}],"question":null,"reason":null})).unwrap();
        let frozen = validate_plan(&p, context).expect("real registry-resolved candidate");
        (p, frozen)
    }
    fn proof(
        text: &str,
        latest: &Value,
        plan: &Plan,
        frozen: &FrozenPlan,
        old: &Value,
        new: &Value,
    ) -> Proof {
        match assess(Some(text), latest, plan, frozen, old, new, FRAME) {
            Verdict::Proved(proof) => proof,
            other => panic!("{text}: {other:?}"),
        }
    }
    fn reject(
        text: &str,
        latest: &Value,
        plan: &Plan,
        frozen: &FrozenPlan,
        old: &Value,
        new: &Value,
    ) {
        assert!(
            matches!(
                assess(Some(text), latest, plan, frozen, old, new, FRAME),
                Verdict::Reject(_)
            ),
            "{text}"
        );
    }
    #[test]
    fn actual015_current_move_and016_verified_prior_set_keep_original_hash_and_destinations() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        let before = json!({"p":p,"f":f,"old":old,"new":new});
        let proved = proof("把这个窗口放到第八个 tag", &Value::Null, &p, &f, &old, &new);
        assert!(proved.accepts(&p, &f, FRAME));
        assert!(proved.factual_noop_reason().is_none());
        assert_eq!(before, json!({"p":p,"f":f,"old":old,"new":new}));
        let (p, f) = plan(
            "move_window_to_tags",
            json!({"window_ref":"w1","tag_numbers":[1,2]}),
            &old,
        );
        assert_eq!(old["windows"][0]["tag_mask"], 1);
        assert_eq!(f.actions[0].arguments["tag_mask"], 3);
        assert!(
            proof(
                "把它放在一号和二号工作区",
                &prior("succeeded"),
                &p,
                &f,
                &old,
                &new
            )
            .accepts(&p, &f, FRAME)
        );
        // Verified prior does not silently mean the currently focused window.
        let mut changed = old.clone();
        changed["focus_token"] = json!("w-native2");
        changed["speech_focus_ref"] = json!("w2");
        let new = fresh(&changed);
        let (p, f) = plan(
            "move_window_to_tags",
            json!({"window_ref":"w1","tag_numbers":[1,2]}),
            &changed,
        );
        assert!(
            proof(
                "把它放在一号和二号工作区",
                &prior("succeeded"),
                &p,
                &f,
                &changed,
                &new
            )
            .accepts(&p, &f, FRAME)
        );
        for status in ["failed", "unknown"] {
            reject(
                "把它放在一号和二号工作区",
                &prior(status),
                &p,
                &f,
                &changed,
                &new,
            );
        }
    }
    #[test]
    fn actual038_only_explicit_unminimize_can_report_already_satisfied_native_fact() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(
            "set_window_minimized",
            json!({"window_ref":"w1","minimized":false}),
            &old,
        );
        for text in [
            "恢复这个最小化窗口",
            "取消当前窗口最小化",
            "取消这个窗口的最小化",
        ] {
            let proved = proof(text, &Value::Null, &p, &f, &old, &new);
            assert!(proved.accepts(&p, &f, FRAME));
            assert_eq!(
                proved.factual_noop_reason(),
                Some("当前窗口未处于最小化状态，无需恢复；本次未执行桌面操作。")
            );
        }
        let mut unknown = new.clone();
        unknown["windows"][0]
            .as_object_mut()
            .unwrap()
            .remove("minimized");
        assert!(
            proof("恢复这个最小化窗口", &Value::Null, &p, &f, &old, &unknown)
                .factual_noop_reason()
                .is_none()
        );
        let (wrong, wrong_f) = plan(
            "set_window_minimized",
            json!({"window_ref":"w1","minimized":true}),
            &old,
        );
        reject(
            "恢复这个最小化窗口",
            &Value::Null,
            &wrong,
            &wrong_f,
            &old,
            &new,
        );
        let (wrong, wrong_f) = plan(
            "set_window_maximized",
            json!({"window_ref":"w1","enabled":false}),
            &old,
        );
        reject(
            "恢复这个最小化窗口",
            &Value::Null,
            &wrong,
            &wrong_f,
            &old,
            &new,
        );
        for text in [
            "恢复这个窗口",
            "恢复它的最小化",
            "不要恢复这个最小化窗口",
            "取消这件事",
        ] {
            assert!(!recognizes(Some(text)));
            assert!(matches!(
                assess(Some(text), &Value::Null, &p, &f, &old, &new, FRAME),
                Verdict::Unsupported
            ));
        }
    }
    #[test]
    fn recognized_wrong_payload_never_falls_back_to_model_allow() {
        let old = context();
        let new = fresh(&old);
        let text = "把这个窗口放到第八个tag";
        for args in [
            json!({"window_ref":"w1","tag_number":7}),
            json!({"window_ref":"w2","tag_number":8}),
        ] {
            let (p, f) = plan("move_window_to_tag", args, &old);
            reject(text, &Value::Null, &p, &f, &old, &new);
        }
        let (p, f) = plan(
            "move_window_to_tags",
            json!({"window_ref":"w1","tag_numbers":[1,3]}),
            &old,
        );
        reject(
            "把它放在一号和二号工作区",
            &prior("succeeded"),
            &p,
            &f,
            &old,
            &new,
        );
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        for text in ["把这个窗口放到第零个tag", "把这个窗口放到第十个tag"] {
            assert!(recognizes(Some(text)));
            reject(text, &Value::Null, &p, &f, &old, &new);
        }
        let mut compound = p.clone();
        compound.actions.push(compound.actions[0].clone());
        let frozen = validate_plan(&compound, &old).unwrap();
        reject(text, &Value::Null, &compound, &frozen, &old, &new);
    }
    #[test]
    fn native_identity_freshness_source_and_capability_drift_veto_proof() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        let text = "把这个窗口放到第八个tag";
        for mode in [
            "wm",
            "pid",
            "start",
            "wake",
            "permit",
            "clock_equal",
            "token",
            "id",
            "tags",
            "mon",
            "dead",
            "duplicate",
            "lock",
            "incomplete",
            "availability",
            "no_speech_certainty",
            "wrong_speech_ref",
        ] {
            let mut before = old.clone();
            let mut after = new.clone();
            match mode {
                "wm" => after["wm_instance_id"] = json!("another"),
                "pid" => after["wm_pid"] = json!(99),
                "start" => after["wm_start_ticks"] = json!("101"),
                "wake" => after["wake_generation"] = json!(2),
                "permit" => after["assistant_permit"] = Value::Null,
                "clock_equal" => after["observed_monotonic_ns"] = json!(100),
                "token" => after["windows"][0]["target_token"] = json!("recycled-instance"),
                "id" => after["windows"][0]["id"] = json!("999"),
                "tags" => after["windows"][0]["tag_mask"] = json!(4),
                "mon" => after["windows"][0]["monitor_token"] = json!("m-native2"),
                "dead" => after["windows"][0]["closed"] = json!(true),
                "duplicate" => {
                    let row = after["windows"][0].clone();
                    after["windows"].as_array_mut().unwrap().push(row);
                }
                "lock" => after["monitors"][0]["locked"] = json!(true),
                "incomplete" => after["windows_complete"] = json!(false),
                "availability" => after["available_actions"] = json!([]),
                "no_speech_certainty" => {
                    before
                        .as_object_mut()
                        .unwrap()
                        .remove("speech_focus_uncertain");
                }
                _ => before["speech_focus_ref"] = json!("w2"),
            }
            reject(text, &Value::Null, &p, &f, &before, &after);
        }
    }
    #[test]
    fn unsupported_audio_names_quotes_negations_and_compounds_supply_no_strong_proof() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        for text in [
            None,
            Some("不要把这个窗口放到第八个tag"),
            Some("标题写着把这个窗口放到第八个tag"),
            Some("“把这个窗口放到第八个tag”"),
            Some("把这个窗口放到HDMI-1第八个tag"),
            Some("把这个窗口放到右屏第八个tag"),
            Some("把这个窗口放到第八个tag然后关闭窗口"),
            Some("把浏览器放到第八个tag"),
            Some("把这个窗口放到第八个tag，不对第九个tag"),
        ] {
            assert!(
                matches!(
                    assess(text, &prior("succeeded"), &p, &f, &old, &new, FRAME),
                    Verdict::Unsupported
                ),
                "{text:?}"
            );
        }
    }
    #[test]
    fn proof_acceptance_binds_full_original_payload_hash_and_actor_frame() {
        let old = context();
        let new = fresh(&old);
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        let proved = proof("把这个窗口放到第八个tag", &Value::Null, &p, &f, &old, &new);
        let mut candidate = p.clone();
        candidate.actions[0].arguments["tag_number"] = json!(7);
        assert!(!proved.accepts(&candidate, &f, FRAME));
        assert!(!proved.accepts(&p, &f, (FRAME.0, FRAME.1 + 1)));
        let mut changed = f.clone();
        changed.actions[0].arguments["tag_mask"] = json!(1);
        assert_eq!(changed.plan_hash, f.plan_hash);
        assert!(!proved.accepts(&p, &changed, FRAME));
        reject(
            "把这个窗口放到第八个tag",
            &Value::Null,
            &p,
            &changed,
            &old,
            &new,
        );
        let mut changed = f.clone();
        changed.plan_hash.push('x');
        assert!(!proved.accepts(&p, &changed, FRAME));
    }
    #[test]
    fn native_ticks_use_real_string_shape_and_wake_and_permit_are_independent() {
        let old = context();
        let new = fresh(&old);
        assert_eq!(old["wake_generation"], 0);
        assert_eq!(old["assistant_permit"]["generation"], 1);
        let (p, f) = plan(
            "move_window_to_tag",
            json!({"window_ref":"w1","tag_number":8}),
            &old,
        );
        assert!(
            proof("把这个窗口放到第八个tag", &Value::Null, &p, &f, &old, &new)
                .accepts(&p, &f, FRAME)
        );
        for value in [
            Value::Null,
            json!(100),
            json!(""),
            json!("0"),
            json!("01"),
            json!("-1"),
            json!("+1"),
            json!("1.0"),
            json!("100 "),
            json!("18446744073709551616"),
        ] {
            assert!(!positive_native_ticks(&value));
            let mut before = old.clone();
            let mut after = new.clone();
            before["wm_start_ticks"] = value.clone();
            after["wm_start_ticks"] = value;
            reject(
                "把这个窗口放到第八个tag",
                &Value::Null,
                &p,
                &f,
                &before,
                &after,
            );
        }
        let mut before = old.clone();
        let mut after = new.clone();
        before["assistant_permit"] = Value::Null;
        after["assistant_permit"] = Value::Null;
        assert!(
            proof(
                "把这个窗口放到第八个tag",
                &Value::Null,
                &p,
                &f,
                &before,
                &after
            )
            .accepts(&p, &f, FRAME)
        );
        before.as_object_mut().unwrap().remove("assistant_permit");
        after.as_object_mut().unwrap().remove("assistant_permit");
        reject(
            "把这个窗口放到第八个tag",
            &Value::Null,
            &p,
            &f,
            &before,
            &after,
        );
    }
    fn edit_context() -> Value {
        let mut c = context();
        c["available_actions"] = json!([
            "move_window_to_tag",
            "move_window_to_tags",
            "set_window_minimized",
            "set_window_maximized",
            "resize_horizontal",
            "resize_vertical",
            "snap_window"
        ]);
        for (n, m) in c["monitors"].as_array_mut().unwrap().iter_mut().enumerate() {
            m["visible_tag_mask"] = json!(1);
            m["layout"] = json!("tile");
            m["geometry"] = json!([n as i64 * 1200, 0, 1200, 800]);
        }
        for (n, w) in c["windows"].as_array_mut().unwrap().iter_mut().enumerate() {
            w["pid"] = json!(100 + n);
            w["visible"] = json!(true);
            w["maximized"] = json!(false);
            w["maximized_horz"] = json!(false);
            w["maximized_vert"] = json!(false);
        }
        c["workspaces"] = json!([{"monitor_token":"m-native1","tag_number":1,"tag_mask":1,"layout":"tile"},{"monitor_token":"m-native2","tag_number":1,"tag_mask":1,"layout":"tile"}]);
        c
    }
    #[test]
    fn explicit_unmaximize_is_exact_and_satisfied_only_with_known_native_state() {
        let old = edit_context();
        let new = fresh(&old);
        let (p, f) = plan(
            "set_window_maximized",
            json!({"window_ref":"w1","enabled":false}),
            &old,
        );
        for text in ["取消最大化", "取消这个窗口的最大化", "取消当前窗口最大化"]
        {
            assert!(
                proof(text, &Value::Null, &p, &f, &old, &new)
                    .factual_noop_reason()
                    .is_some()
            );
        }
        for state in [Value::Null, json!(true)] {
            let mut c = old.clone();
            c["windows"][0]["maximized"] = state;
            assert!(
                proof("取消最大化", &Value::Null, &p, &f, &c, &fresh(&c))
                    .factual_noop_reason()
                    .is_none()
            );
        }
        let mut missing = old.clone();
        missing["windows"][0]
            .as_object_mut()
            .unwrap()
            .remove("maximized");
        assert!(
            proof(
                "取消最大化",
                &Value::Null,
                &p,
                &f,
                &missing,
                &fresh(&missing)
            )
            .factual_noop_reason()
            .is_none()
        );
        let mut partial = old.clone();
        partial["windows"][0]["maximized_horz"] = json!(true);
        assert!(
            proof(
                "取消最大化",
                &Value::Null,
                &p,
                &f,
                &partial,
                &fresh(&partial)
            )
            .factual_noop_reason()
            .is_none()
        );
        let mut malformed = old.clone();
        malformed["windows"][0]["maximized"] = json!("false");
        reject(
            "取消最大化",
            &Value::Null,
            &p,
            &f,
            &malformed,
            &fresh(&malformed),
        );
        for args in [
            json!({"window_ref":"w1","enabled":true}),
            json!({"window_ref":"w2","enabled":false}),
        ] {
            let (a, b) = plan("set_window_maximized", args, &old);
            reject("取消最大化", &Value::Null, &a, &b, &old, &new);
        }
        let mut toggle = p.clone();
        toggle.actions[0].name = "togglemaximize".into();
        reject("取消最大化", &Value::Null, &toggle, &f, &old, &new);
    }
    #[test]
    fn four_axes_use_exact_workarea_twentieth_with_bounded_current_subject_and_repeat() {
        let old = edit_context();
        let new = fresh(&old);
        for (subject, word, horizontal, positive) in
            ["这个窗口", "当前窗口", ""].iter().flat_map(|subject| {
                [
                    (subject, "宽", true, true),
                    (subject, "窄", true, false),
                    (subject, "高", false, true),
                    (subject, "矮", false, false),
                ]
            })
        {
            let name = if horizontal {
                "resize_horizontal"
            } else {
                "resize_vertical"
            };
            let value = if positive { 0.05 } else { -0.05 };
            let (p, f) = plan(
                name,
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":value}),
                &old,
            );
            assert_eq!(f.actions[0].arguments["window_token"], "w-native1");
            assert!(
                f.actions[0]
                    .arguments
                    .get("expected_window_token")
                    .is_none()
            );
            for repeat in ["", "再"] {
                for tail in ["一点", "一点点"] {
                    let text = format!("{subject}{repeat}{word}{tail}");
                    let proved = proof(&text, &Value::Null, &p, &f, &old, &new);
                    assert!(proved.accepts(&p, &f, FRAME));
                    assert!(proved.factual_noop_reason().is_none());
                }
            }
        }
        assert!(exact_twentieth(&json!(0.05), true));
        assert!(exact_twentieth(&json!(-0.05), false));
        for invalid in [
            json!(0.05000000000001),
            json!(0.1),
            json!(-0.05),
            json!(0),
            json!("0.05"),
        ] {
            assert!(!exact_twentieth(&invalid, true), "{invalid}");
        }
        let (p, f) = plan(
            "resize_horizontal",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":-0.05}),
            &old,
        );
        reject(
            "再窄一点点",
            &json!({"window_ref":"w2"}),
            &p,
            &f,
            &old,
            &new,
        );
        assert!(
            proof(
                "这个窗口窄一点",
                &json!({"window_ref":"w2"}),
                &p,
                &f,
                &old,
                &new
            )
            .accepts(&p, &f, FRAME)
        );
    }
    #[test]
    fn snap_direction_is_literal_registry_enum_in_the_windows_own_container() {
        let old = edit_context();
        let new = fresh(&old);
        for (text, direction) in [
            ("把当前窗口贴到屏幕左边", "left"),
            ("把这个贴到右半边", "right"),
            ("这个窗口放左上角", "top-left"),
            ("这个窗口放右上角", "top-right"),
            ("这个窗口放左下角", "bottom-left"),
            ("这个窗口放右下角", "bottom-right"),
            ("这个窗口放全屏", "maximize"),
            ("put this window on the left half", "left"),
        ] {
            let (p, f) = plan(
                "snap_window",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"direction":direction}),
                &old,
            );
            if direction == "maximize" {
                reject(text, &Value::Null, &p, &f, &old, &new);
                continue;
            }
            let proved = proof(text, &Value::Null, &p, &f, &old, &new);
            assert!(proved.accepts(&p, &f, FRAME));
            let mut wrong = f.clone();
            wrong.actions[0].arguments["direction"] = json!("right");
            if direction != "right" {
                reject(text, &Value::Null, &p, &wrong, &old, &new);
            }
        }
    }
    #[test]
    fn new_current_edits_veto_wrong_value_axis_window_container_view_or_frozen_hash() {
        let old = edit_context();
        let new = fresh(&old);
        let (p, f) = plan(
            "resize_horizontal",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.05}),
            &old,
        );
        for (name, args) in [
            (
                "resize_vertical",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.05}),
            ),
            (
                "resize_horizontal",
                json!({"window_ref":"w2","monitor_ref":"m1","tag_number":1,"value":0.05}),
            ),
            (
                "resize_horizontal",
                json!({"window_ref":"w1","monitor_ref":"m2","tag_number":1,"value":0.05}),
            ),
            (
                "resize_horizontal",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":2,"value":0.05}),
            ),
            (
                "resize_horizontal",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.050000001}),
            ),
        ] {
            let (a, b) = plan(name, args, &old);
            reject("这个窗口宽一点", &Value::Null, &a, &b, &old, &new);
        }
        for (key, value) in [
            ("id", json!("99")),
            ("pid", json!(999)),
            ("target_token", json!("replacement")),
            ("tag_mask", json!(2)),
            ("monitor_token", json!("m-native2")),
            ("visible", json!(false)),
            ("minimized", json!(true)),
        ] {
            let mut drift = new.clone();
            drift["windows"][0][key] = value;
            reject("这个窗口宽一点", &Value::Null, &p, &f, &old, &drift);
        }
        for (key, value) in [
            ("visible_tag_mask", json!(3)),
            ("layout", json!("grid")),
            ("geometry", json!([0, 0, 0, 800])),
            ("locked", json!(true)),
        ] {
            let mut drift = new.clone();
            drift["monitors"][0][key] = value;
            reject("这个窗口宽一点", &Value::Null, &p, &f, &old, &drift);
        }
        for (key, value) in [
            ("speech_focus_uncertain", json!(true)),
            ("speech_focus_ref", json!("w2")),
            ("focus_token", json!("w-native2")),
            ("selected_monitor_token", json!("m-native2")),
            ("activation_monitor_token", Value::Null),
            ("windows_complete", json!(false)),
            ("wm_pid", json!(333)),
            ("assistant_permit", Value::Null),
            ("wake_generation", json!(1)),
            ("observed_monotonic_ns", json!(100)),
        ] {
            let mut drift = new.clone();
            drift[key] = value;
            reject("这个窗口宽一点", &Value::Null, &p, &f, &old, &drift);
        }
        let mut unknown = old.clone();
        unknown["monitors"][0]["layout"] = json!("unknown");
        unknown["workspaces"][0]["layout"] = json!("unknown");
        reject(
            "这个窗口宽一点",
            &Value::Null,
            &p,
            &f,
            &unknown,
            &fresh(&unknown),
        );
        let mut hash = f.clone();
        hash.plan_hash.push('a');
        reject("这个窗口宽一点", &Value::Null, &p, &hash, &old, &new);
        let mut extra = p.clone();
        extra.actions.push(p.actions[0].clone());
        reject("这个窗口宽一点", &Value::Null, &extra, &f, &old, &new);
        assert!(
            !proof("这个窗口宽一点", &Value::Null, &p, &f, &old, &new).accepts(
                &p,
                &f,
                (FRAME.0, FRAME.1 + 1)
            )
        );
    }
    #[test]
    fn new_geometry_and_flag_grammar_never_promotes_queries_ambiguous_amounts_or_other_targets() {
        for text in [
            "取消最大化？",
            "取消最大化好吗",
            "不要取消最大化",
            "取消最大化并关闭窗口",
            "取消那个窗口最大化",
            "切换最大化",
            "再小一点",
            "再大一点",
            "舒服一点",
            "这个窗口宽五像素",
            "这个窗口宽一点好吗",
            "这个窗口宽一点？",
            "这个窗口再宽一点再高一点",
            "这个窗口宽一点并高一点",
            "标题：这个窗口宽一点",
            "‘这个窗口宽一点’",
            "那个窗口宽一点",
            "左边窗口宽一点",
            "这个窗口放上半边",
            "这个窗口放第三屏右上角",
            "这个窗口放右屏右上角",
            "put this window on third screen on the left half",
            "这个窗口放右上角然后关闭",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
    }
    #[test]
    fn unmaximize_unknown_axes_never_claim_already_satisfied() {
        let mut c = edit_context();
        for w in c["windows"].as_array_mut().unwrap() {
            w.as_object_mut().unwrap().remove("maximized_horz");
            w.as_object_mut().unwrap().remove("maximized_vert");
        }
        let n = fresh(&c);
        let (p, f) = plan(
            "set_window_maximized",
            serde_json::json!({"window_ref":"w1","enabled":false}),
            &c,
        );
        assert!(
            proof("取消最大化", &Value::Null, &p, &f, &c, &n)
                .factual_noop_reason()
                .is_none()
        );
    }
    fn sticky_context() -> Value {
        let mut c = edit_context();
        c["available_actions"]
            .as_array_mut()
            .unwrap()
            .push(json!("set_window_sticky"));
        c["windows"][0]["sticky"] = json!(false);
        c
    }
    #[test]
    fn explicit_all_tag_visibility_keeps_the_exact_sticky_setter_and_never_claims_noop() {
        let old = sticky_context();
        let new = fresh(&old);
        let (p, f) = plan(
            "set_window_sticky",
            json!({"window_ref":"w1","enabled":true}),
            &old,
        );
        for text in [
            "把这个窗口置顶到所有 tag",
            "把当前窗口置顶到所有tag",
            "把这个窗口置顶到所有工作区",
            "跨工作区显示这个窗口",
            "让当前窗口跨工作区显示",
            "把这个窗口设为跨工作区显示",
            "这个窗口在所有工作区显示",
        ] {
            let original = json!({"plan":p,"frozen":f,"old":old,"new":new});
            let proved = proof(text, &Value::Null, &p, &f, &old, &new);
            assert!(proved.accepts(&p, &f, FRAME));
            assert!(proved.factual_noop_reason().is_none());
            assert_eq!(original, json!({"plan":p,"frozen":f,"old":old,"new":new}));
        }
        let (p, f) = plan(
            "set_window_sticky",
            json!({"window_ref":"w1","enabled":false}),
            &old,
        );
        for text in [
            "不要跨工作区显示这个窗口",
            "不要跨工作区显示当前窗口",
            "取消这个窗口的跨工作区显示",
            "让当前窗口不再跨工作区显示",
        ] {
            assert!(
                proof(text, &Value::Null, &p, &f, &old, &new)
                    .factual_noop_reason()
                    .is_none()
            );
        }
        for state in [json!(true), json!(false), Value::Null] {
            let mut c = old.clone();
            c["windows"][0]["sticky"] = state;
            assert!(
                proof(
                    "不要跨工作区显示这个窗口",
                    &Value::Null,
                    &p,
                    &f,
                    &c,
                    &fresh(&c)
                )
                .factual_noop_reason()
                .is_none()
            );
        }
        let mut c = old.clone();
        c["windows"][0].as_object_mut().unwrap().remove("sticky");
        assert!(
            proof(
                "不要跨工作区显示这个窗口",
                &Value::Null,
                &p,
                &f,
                &c,
                &fresh(&c)
            )
            .factual_noop_reason()
            .is_none()
        );
    }
    #[test]
    fn sticky_does_not_accept_boolean_target_domain_hash_frame_or_native_drift() {
        let old = sticky_context();
        let new = fresh(&old);
        let text = "把这个窗口置顶到所有 tag";
        let (p, f) = plan(
            "set_window_sticky",
            json!({"window_ref":"w1","enabled":true}),
            &old,
        );
        for args in [
            json!({"window_ref":"w1","enabled":false}),
            json!({"window_ref":"w2","enabled":true}),
        ] {
            let (a, b) = plan("set_window_sticky", args, &old);
            reject(text, &Value::Null, &a, &b, &old, &new);
        }
        let (toggle, toggle_f) = plan(
            "toggle_window_tags",
            json!({"window_ref":"w1","tag_numbers":[1,2]}),
            &old,
        );
        reject(text, &Value::Null, &toggle, &toggle_f, &old, &new);
        for (key, value) in [("tag_numbers", json!([1, 2])), ("monitor_ref", json!("m1"))] {
            let mut extra = p.clone();
            extra.actions[0].arguments[key] = value;
            reject(text, &Value::Null, &extra, &f, &old, &new);
        }
        let mut extra = p.clone();
        extra.actions.push(p.actions[0].clone());
        reject(text, &Value::Null, &extra, &f, &old, &new);
        let mut bad = f.clone();
        bad.plan_hash.push('a');
        reject(text, &Value::Null, &p, &bad, &old, &new);
        assert!(!proof(text, &Value::Null, &p, &f, &old, &new).accepts(
            &p,
            &f,
            (FRAME.0, FRAME.1 + 1)
        ));
        for (key, value) in [
            ("id", json!("99")),
            ("pid", json!(999)),
            ("target_token", json!("reused-os-id-instance")),
            ("sticky", json!(true)),
            ("tag_mask", json!(2)),
        ] {
            let mut changed = new.clone();
            changed["windows"][0][key] = value;
            reject(text, &Value::Null, &p, &f, &old, &changed);
        }
        let mut changed = new.clone();
        changed["monitors"][0]["visible_tag_mask"] = json!(3);
        reject(text, &Value::Null, &p, &f, &old, &changed);
        for (key, value) in [
            ("speech_focus_uncertain", json!(true)),
            ("focus_token", json!("w-native2")),
            ("selected_monitor_token", json!("m-native2")),
            ("activation_monitor_token", Value::Null),
            ("assistant_permit", Value::Null),
            ("wake_generation", json!(1)),
            ("observed_monotonic_ns", json!(100)),
        ] {
            let mut changed = new.clone();
            changed[key] = value;
            reject(text, &Value::Null, &p, &f, &old, &changed);
        }
        let mut malformed = old.clone();
        malformed["windows"][0]["sticky"] = json!("true");
        reject(text, &Value::Null, &p, &f, &malformed, &fresh(&malformed));
    }
    #[test]
    fn sticky_grammar_requires_explicit_all_workspace_visibility_not_layering_or_generic_negation()
    {
        for text in [
            "置顶",
            "把这个窗口置顶",
            "always-on-top",
            "把这个窗口放到上层",
            "不要置顶",
            "不要把这个窗口置顶",
            "跨工作区显示那个窗口",
            "把这个窗口置顶到某些tag",
            "把这个窗口置顶到所有tag好吗",
            "把这个窗口置顶到所有tag？",
            "‘把这个窗口置顶到所有tag’",
            "标题写着跨工作区显示这个窗口",
            "不要跨工作区显示这个窗口，然后关闭终端",
            "跨工作区显示这个窗口，如果安全的话",
            "不要跨工作区显示",
            "取消跨工作区显示",
            "跨工作区显示这个窗口并移到右屏",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
    }
    #[test]
    fn finite_english_current_axis_aliases_use_original_exact_workarea_fraction() {
        let old = edit_context();
        let new = fresh(&old);
        for (word, name, value) in [
            ("taller", "resize_vertical", 0.05),
            ("shorter", "resize_vertical", -0.05),
            ("wider", "resize_horizontal", 0.05),
            ("narrower", "resize_horizontal", -0.05),
        ] {
            let (p, f) = plan(
                name,
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":value}),
                &old,
            );
            for subject in ["this window", "the current window"] {
                let text = format!("make {subject} a little {word}");
                let snapshot = json!({"p":p,"f":f,"old":old,"new":new});
                let proved = proof(&text, &Value::Null, &p, &f, &old, &new);
                assert!(proved.accepts(&p, &f, FRAME));
                assert_eq!(snapshot, json!({"p":p,"f":f,"old":old,"new":new}));
                assert!(proved.factual_noop_reason().is_none());
            }
        }
        let (wrong, wrong_f) = plan(
            "resize_horizontal",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.05}),
            &old,
        );
        reject(
            "make this window a little taller",
            &Value::Null,
            &wrong,
            &wrong_f,
            &old,
            &new,
        );
        let (wrong, wrong_f) = plan(
            "resize_vertical",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.1}),
            &old,
        );
        reject(
            "make this window a little taller",
            &Value::Null,
            &wrong,
            &wrong_f,
            &old,
            &new,
        );
    }
    #[test]
    fn one_snap_correction_parses_both_same_current_operands_and_keeps_final_direction() {
        let old = edit_context();
        let new = fresh(&old);
        let (p, f) = plan(
            "snap_window",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"direction":"right"}),
            &old,
        );
        let original = json!({"plan":p,"frozen":f});
        assert!(
            proof("这个放左边，不对，右半边", &Value::Null, &p, &f, &old, &new)
                .accepts(&p, &f, FRAME)
        );
        assert_eq!(original, json!({"plan":p,"frozen":f}));
        let directions = [
            ("左边", "left"),
            ("右半边", "right"),
            ("左上角", "top-left"),
            ("右上角", "top-right"),
            ("左下角", "bottom-left"),
            ("右下角", "bottom-right"),
            ("全屏", "maximize"),
        ];
        for (first, _) in directions {
            for (last, canonical) in directions {
                let text = format!("这个窗口放{first}，不对，{last}");
                let (p, f) = plan(
                    "snap_window",
                    json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"direction":canonical}),
                    &old,
                );
                if canonical == "maximize" {
                    reject(&text, &Value::Null, &p, &f, &old, &new);
                } else {
                    assert!(proof(&text, &Value::Null, &p, &f, &old, &new).accepts(&p, &f, FRAME));
                }
            }
        }
        let (p, f) = plan(
            "snap_window",
            json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"direction":"left"}),
            &old,
        );
        reject("这个放左边，不对，右半边", &Value::Null, &p, &f, &old, &new);
        let (p, f) = plan(
            "snap_window",
            json!({"window_ref":"w2","monitor_ref":"m1","tag_number":1,"direction":"right"}),
            &old,
        );
        reject("这个放左边，不对，右半边", &Value::Null, &p, &f, &old, &new);
    }
    #[test]
    fn english_and_correction_leaves_do_not_discard_unknown_old_sources_or_extra_effects() {
        for text in [
            "make this window a little bigger",
            "make this window a little smaller",
            "make that window a little taller",
            "make the browser a little taller",
            "make this window ten pixels taller",
            "make this window a little taller and wider",
            "make this window a little taller please",
            "make this window a little taller?",
            "make this window a little taller好吗",
            "make this window a little taller. close it",
            "‘make this window a little taller’",
            "这个放左边，不对，右半边，不对，左边",
            "这个放左边，不对，那个窗口放右半边",
            "那个放左边，不对，右半边",
            "右屏的这个窗口放左边，不对，右半边",
            "这个窗口放第三屏左边，不对，右半边",
            "这个放未知，不对，右半边",
            "不要这个放左边，不对，右半边",
            "如果可以这个放左边，不对，右半边",
            "这个放左边并关闭，不对，右半边",
            "这个放左边，不对，右半边然后关闭",
            "‘这个放左边，不对，右半边’",
            "这个放左边，不对，右半边？",
            "这个放左边，不对，右半边好吗",
        ] {
            assert!(!recognizes(Some(text)), "{text}");
        }
        assert!(!recognizes(None));
        // Existing tag moves must survive the shared placement prefix, and
        // existing sticky/axis/snap productions retain their original meaning.
        let old = sticky_context();
        let new = fresh(&old);
        for (text, name, args) in [
            (
                "把这个窗口放到第八个tag",
                "move_window_to_tag",
                json!({"window_ref":"w1","tag_number":8}),
            ),
            (
                "把这个窗口置顶到所有 tag",
                "set_window_sticky",
                json!({"window_ref":"w1","enabled":true}),
            ),
            (
                "这个窗口宽一点",
                "resize_horizontal",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"value":0.05}),
            ),
            (
                "这个窗口放右上角",
                "snap_window",
                json!({"window_ref":"w1","monitor_ref":"m1","tag_number":1,"direction":"top-right"}),
            ),
        ] {
            let (p, f) = plan(name, args, &old);
            assert!(proof(text, &Value::Null, &p, &f, &old, &new).accepts(&p, &f, FRAME));
        }
    }
    #[test]
    fn uncertain_current_repeat_snap_only_builds_a_question_and_does_not_expand_execution_grammar()
    {
        let mut c = context();
        c["speech_focus_uncertain"] = json!(true);
        let text = "把这个窗口再放到右半边";
        assert!(uncertain_current_question(Some(text), &c).is_some());
        assert!(
            !recognizes(Some(text)),
            "question-only production is not execution grammar"
        );
        c["speech_focus_uncertain"] = json!(false);
        assert!(uncertain_current_question(Some(text), &c).is_none());
        c.as_object_mut().unwrap().remove("speech_focus_uncertain");
        assert!(uncertain_current_question(Some(text), &c).is_none());
        c["speech_focus_uncertain"] = json!(true);
        for text in [
            "把它放到第八个tag",
            "把那个窗口再放到右半边",
            "把这个窗口再放到第三屏右半边",
            "把这个窗口再放到右半边然后退出",
            "标题写着把这个窗口再放到右半边",
        ] {
            assert!(
                uncertain_current_question(Some(text), &c).is_none(),
                "{text}"
            );
        }
    }
}
