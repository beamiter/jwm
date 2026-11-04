//! Exact, bounded local source evidence for literal quantified close requests.
//! These values select no actions and are never execution/confirmation permits.
use jwm_eva_core::actions::{Disposition, FrozenPlan, Plan};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    CurrentTagVisible,
}
impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::CurrentTagVisible => "current_tag_visible",
        }
    }
}
fn literal_scope(text: &str) -> Option<Scope> {
    if text.len() > 512 || text.chars().any(char::is_control) {
        return None;
    }
    let normalized = text
        .trim()
        .trim_end_matches(['。', '!', '！', '.'])
        .trim()
        .to_lowercase();
    let text = normalized
        .strip_prefix("请")
        .or_else(|| normalized.strip_prefix("please "))
        .unwrap_or(&normalized)
        .trim();
    // A spoken assertion of approval contributes no authority. Recognize only
    // this explicit request preamble; the frozen batch still requires the same
    // trusted local confirmation as every other close request.
    let text = text.strip_prefix("我已经确认了，直接").unwrap_or(text);
    match text {
        "关闭所有窗口" | "关掉所有窗口" | "关闭全部窗口" | "关掉全部窗口" | "close all windows" => {
            Some(Scope::All)
        }
        "关闭当前 tag 的所有窗口"
        | "关闭当前tag的所有窗口"
        | "关闭当前工作区所有窗口"
        | "关闭当前工作区的所有窗口"
        | "关闭当前工作区所有可见窗口"
        | "关闭当前工作区的所有可见窗口"
        | "关闭当前桌面的所有窗口"
        | "关闭当前桌面的所有可见窗口"
        | "close all windows on the current workspace"
        | "close all visible windows on the current workspace" => Some(Scope::CurrentTagVisible),
        _ => None,
    }
}
fn bounded(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}
#[derive(PartialEq, Eq)]
struct Window {
    token: String,
    monitor: String,
    monitor_ref: String,
    tags: u64,
    visible: bool,
    sticky: bool,
}
struct SetSnapshot {
    wm: String,
    scope: Scope,
    windows: BTreeMap<String, Window>,
    eligible: BTreeSet<String>,
    selected: Option<(String, u64)>,
}
impl SetSnapshot {
    fn capture(context: &Value, scope: Scope) -> Option<Self> {
        if context["windows_complete"] != true || context["session_unlocked"] != true {
            return None;
        }
        let wm = bounded(&context["wm_instance_id"])?.to_owned();
        let monitors = context["monitors"]
            .as_array()
            .filter(|m| !m.is_empty() && m.len() <= 32)?;
        let mut monitor_tokens = BTreeSet::new();
        let mut monitor_refs = BTreeSet::new();
        for m in monitors {
            if m["locked"] != false
                || !monitor_tokens.insert(bounded(&m["monitor_token"])?)
                || !monitor_refs.insert(bounded(&m["monitor_ref"])?)
            {
                return None;
            }
        }
        let selected = if scope == Scope::CurrentTagVisible {
            let selected = bounded(&context["selected_monitor_token"])?;
            let monitor = monitors.iter().find(|m| m["monitor_token"] == selected)?;
            let mask = monitor["visible_tag_mask"]
                .as_u64()
                .filter(|mask| *mask <= u32::MAX as u64 && mask.count_ones() == 1)?;
            Some((selected, mask))
        } else {
            None
        };
        let source = context["windows"]
            .as_array()
            .filter(|w| !w.is_empty() && w.len() <= 128)?;
        let mut windows = BTreeMap::new();
        let mut tokens = BTreeSet::new();
        let mut eligible = BTreeSet::new();
        for w in source {
            if w["closed"] == true || w["live"] == false {
                return None;
            }
            let reference = bounded(&w["window_ref"])?.to_owned();
            let token = bounded(&w["target_token"])?.to_owned();
            let monitor = bounded(&w["monitor_token"])?.to_owned();
            let monitor_ref = monitors
                .iter()
                .find(|m| m["monitor_token"] == monitor)
                .and_then(|m| bounded(&m["monitor_ref"]))?
                .to_owned();
            let tags = w["tag_mask"]
                .as_u64()
                .filter(|tags| *tags <= u32::MAX as u64)?;
            let visible = w["visible"].as_bool()?;
            let sticky = w["sticky"].as_bool()?;
            if !monitor_tokens.contains(monitor.as_str())
                || !tokens.insert(token.clone())
                || windows.contains_key(&reference)
            {
                return None;
            }
            let include = selected.is_none_or(|(selected, mask)| {
                monitor == selected && visible && (tags & mask != 0 || sticky)
            });
            if include {
                eligible.insert(token.clone());
            }
            windows.insert(
                reference,
                Window {
                    token,
                    monitor,
                    monitor_ref,
                    tags,
                    visible,
                    sticky,
                },
            );
        }
        if eligible.is_empty() || eligible.len() > 32 {
            return None;
        }
        Some(Self {
            wm,
            scope,
            windows,
            eligible,
            selected: selected.map(|(token, mask)| (token.to_owned(), mask)),
        })
    }
    fn exact_plan(&self, semantic: &Plan, frozen: &FrozenPlan) -> bool {
        if semantic.disposition != Disposition::Execute
            || frozen.disposition != Disposition::Execute
            || !frozen.requires_confirmation
            || frozen.wm_instance_id != self.wm
            || semantic.actions.len() != self.eligible.len()
            || frozen.actions.len() != self.eligible.len()
        {
            return false;
        }
        let mut actual = BTreeSet::new();
        for (semantic, frozen) in semantic.actions.iter().zip(&frozen.actions) {
            if semantic.name != "killclient" || frozen.name != "killclient" {
                return false;
            }
            let Some(args) = semantic.arguments.as_object().filter(|args| {
                args.keys()
                    .all(|key| matches!(key.as_str(), "window_ref" | "monitor_ref" | "tag_number"))
            }) else {
                return false;
            };
            let Some(window) = args
                .get("window_ref")
                .and_then(Value::as_str)
                .and_then(|reference| self.windows.get(reference))
            else {
                return false;
            };
            if args
                .get("monitor_ref")
                .is_some_and(|reference| reference.as_str() != Some(window.monitor_ref.as_str()))
            {
                return false;
            }
            if let Some(number) = args.get("tag_number") {
                let Some(number) = number.as_u64().filter(|number| (1..=32).contains(number))
                else {
                    return false;
                };
                if window.tags != 1u64 << (number - 1) {
                    return false;
                }
            }
            if frozen.arguments["window_token"].as_str() != Some(window.token.as_str())
                || !actual.insert(window.token.clone())
            {
                return false;
            }
        }
        actual == self.eligible
    }
    fn review_rows(&self, review: &Value, semantic: &Plan, owned: bool) -> bool {
        if review["decision"] != "allow"
            || review["session_intent"] != "continue"
            || !review["question"].is_null()
        {
            return false;
        }
        let Some(rows) = review["steps"]
            .as_array()
            .filter(|rows| rows.len() == semantic.actions.len())
        else {
            return false;
        };
        for (row, action) in rows.iter().zip(&semantic.actions) {
            let Some(reference) = action.arguments["window_ref"].as_str() else {
                return false;
            };
            let Some(window) = self.windows.get(reference) else {
                return false;
            };
            if row["name"] != "killclient"
                || row["window_refs"]
                    .as_array()
                    .is_none_or(|refs| refs.len() != 1 || refs[0] != reference)
            {
                return false;
            }
            if row
                .get("target_scope")
                .is_some_and(|scope| scope != "window" && scope != "selected_window")
            {
                return false;
            }
            for key in ["session_refs", "device_ids", "application_ids"] {
                if row
                    .get(key)
                    .is_some_and(|refs| refs.as_array().is_none_or(|refs| !refs.is_empty()))
                {
                    return false;
                }
            }
            for key in [
                "device_quote",
                "session_quote",
                "application_quote",
                "effect_quote",
            ] {
                if row.get(key).is_some_and(|quote| quote.as_str() != Some("")) {
                    return false;
                }
            }
            // Scope annotations cannot introduce another screen/tag target.
            let Some(monitors) = row["monitor_refs"].as_array() else {
                return false;
            };
            if (!owned && !monitors.is_empty())
                || monitors.len() > 1
                || monitors
                    .first()
                    .is_some_and(|reference| reference != &window.monitor_ref)
                || row["tag_numbers"]
                    .as_array()
                    .is_none_or(|tags| !tags.is_empty())
            {
                return false;
            }
            if !self.eligible.contains(&window.token) {
                return false;
            }
        }
        true
    }
}
/// Constructed only from the caller's current owned text. Not serializable.
pub(super) struct OwnedBatchCloseSetProof {
    snapshot: SetSnapshot,
    plan_hash: String,
    review_digest: String,
}
/// Fresh NLU correlation, never an owned text or acoustic certificate.
pub(super) struct FreshAudioReview {
    item: String,
    response: String,
    binding: String,
    review_digest: String,
}
impl FreshAudioReview {
    pub(super) fn copy_for_review(marker: &Self, review: &Value) -> Option<Self> {
        (marker.review_digest == jwm_eva_core::protocol::digest(review)).then(|| Self {
            item: marker.item.clone(),
            response: marker.response.clone(),
            binding: marker.binding.clone(),
            review_digest: marker.review_digest.clone(),
        })
    }
    pub(super) fn from_correlated_receipt(
        item: &str,
        response: &str,
        binding: &str,
        review: &Value,
    ) -> Option<Self> {
        if [item, response, binding]
            .iter()
            .any(|s| s.is_empty() || s.len() > 512)
        {
            return None;
        }
        Some(Self {
            item: item.into(),
            response: response.into(),
            binding: binding.into(),
            review_digest: jwm_eva_core::protocol::digest(review),
        })
    }
}
pub(super) struct ModelAudioBatchCloseSetClaim {
    snapshot: SetSnapshot,
    plan_hash: String,
    correlation: FreshAudioReview,
}
pub(super) enum BatchCloseEvidence {
    Owned(OwnedBatchCloseSetProof),
    UnverifiedAudio(ModelAudioBatchCloseSetClaim),
}
impl BatchCloseEvidence {
    pub(super) fn matches(
        &self,
        semantic: &Plan,
        frozen: &FrozenPlan,
        context: &Value,
        review: &Value,
    ) -> bool {
        let (snapshot, hash, owned) = match self {
            Self::Owned(proof) => {
                if proof.review_digest != jwm_eva_core::protocol::digest(review) {
                    return false;
                }
                (&proof.snapshot, &proof.plan_hash, true)
            }
            Self::UnverifiedAudio(claim) => {
                if claim.correlation.item.is_empty()
                    || claim.correlation.response.is_empty()
                    || claim.correlation.binding.is_empty()
                    || claim.correlation.review_digest != jwm_eva_core::protocol::digest(review)
                {
                    return false;
                }
                (&claim.snapshot, &claim.plan_hash, false)
            }
        };
        let Some(fresh) = SetSnapshot::capture(context, snapshot.scope) else {
            return false;
        };
        hash == &frozen.plan_hash
            && snapshot.wm == fresh.wm
            && snapshot.windows == fresh.windows
            && snapshot.eligible == fresh.eligible
            && snapshot.selected == fresh.selected
            && snapshot.exact_plan(semantic, frozen)
            && snapshot.review_rows(review, semantic, owned)
    }
}
/// Does not create, remove, reorder or retarget a single candidate action.
pub(super) fn prove(
    owned: Option<&str>,
    audio: Option<FreshAudioReview>,
    review: &Value,
    semantic: &Plan,
    frozen: &FrozenPlan,
    context: &Value,
) -> Option<BatchCloseEvidence> {
    let scope = if let Some(owned) = owned {
        literal_scope(owned)?
    } else {
        let quote = review["batch_close_quote"].as_str()?;
        literal_scope(quote)?
    };
    if let Some(owned_text) = owned {
        // The actor consumes the entire literal request, including the exact
        // scope. An omitted model annotation is not a replacement authority.
        if review
            .get("batch_close_scope")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(scope.name()))
        {
            return None;
        }
        if let Some(quote) = review["batch_close_quote"]
            .as_str()
            .filter(|quote| !quote.is_empty())
        {
            let quote_lower = quote.to_lowercase();
            if !owned_text.to_lowercase().contains(&quote_lower)
                || !(quote_lower == owned_text.to_lowercase()
                    || quote_lower.contains("所有窗口")
                    || quote_lower.contains("全部窗口")
                    || quote_lower.contains("所有可见窗口")
                    || quote_lower.contains("全部可见窗口")
                    || quote_lower.contains("all windows")
                    || quote_lower.contains("all visible windows"))
            {
                return None;
            }
        }
    } else if review["batch_close_scope"].as_str() != Some(scope.name()) {
        return None;
    }
    let rebuilt = jwm_eva_core::actions::validate_plan(semantic, context).ok()?;
    if serde_json::to_value(&rebuilt).ok()? != serde_json::to_value(frozen).ok()? {
        return None;
    }
    let snapshot = SetSnapshot::capture(context, scope)?;
    if !snapshot.exact_plan(semantic, frozen)
        || !snapshot.review_rows(review, semantic, owned.is_some())
    {
        return None;
    }
    let plan_hash = frozen.plan_hash.clone();
    if owned.is_some() {
        Some(BatchCloseEvidence::Owned(OwnedBatchCloseSetProof {
            snapshot,
            plan_hash,
            review_digest: jwm_eva_core::protocol::digest(review),
        }))
    } else {
        Some(BatchCloseEvidence::UnverifiedAudio(
            ModelAudioBatchCloseSetClaim {
                snapshot,
                plan_hash,
                correlation: audio?,
            },
        ))
    }
}
pub(super) fn has_quantifier(owned: Option<&str>, review: &Value) -> bool {
    let owned_quantifier = owned.is_some_and(|text| {
        let text = text.to_lowercase();
        ["所有", "全部", "all "]
            .iter()
            .any(|word| text.contains(word))
            && ["窗口", "windows"].iter().any(|word| text.contains(word))
            && ["关闭", "关掉", "close"]
                .iter()
                .any(|word| text.contains(word))
    });
    owned_quantifier
        || review["batch_close_scope"]
            .as_str()
            .is_some_and(|scope| scope != "none")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn context() -> Value {
        json!({"wm_instance_id":"wm","session_unlocked":true,"windows_complete":true,"tag_count":9,"selected_monitor_token":"mon1","focus_token":"win1","monitors":[{"monitor_ref":"m1","monitor_token":"mon1","visible_tag_mask":1,"locked":false},{"monitor_ref":"m2","monitor_token":"mon2","visible_tag_mask":1,"locked":false}],"windows":[
        {"window_ref":"w1","target_token":"win1","monitor_token":"mon1","tag_mask":1,"visible":true,"sticky":false,"minimized":false},
        {"window_ref":"w2","target_token":"win2","monitor_token":"mon1","tag_mask":2,"visible":false,"sticky":false,"minimized":true},
        {"window_ref":"w3","target_token":"win3","monitor_token":"mon2","tag_mask":1,"visible":true,"sticky":false,"minimized":false},
        {"window_ref":"w4","target_token":"win4","monitor_token":"mon1","tag_mask":2,"visible":true,"sticky":true,"minimized":false}]})
    }
    fn candidate(ctx: &Value, refs: &[&str]) -> (Plan, FrozenPlan) {
        let plan=serde_json::from_value(json!({"disposition":"execute","actions":refs.iter().map(|reference|json!({"name":"killclient","arguments":{"window_ref":reference}})).collect::<Vec<_>>(),"question":null,"reason":null})).unwrap();
        let frozen = jwm_eva_core::actions::validate_plan(&plan, ctx).unwrap();
        (plan, frozen)
    }
    fn review(scope: &str, quote: &str, refs: &[&str]) -> Value {
        json!({"decision":"allow","session_intent":"continue","question":null,"batch_close_scope":scope,"batch_close_quote":quote,"steps":refs.iter().map(|reference|json!({"name":"killclient","window_refs":[reference],"window_basis":"named","window_quote":"所有窗口","monitor_refs":[],"tag_numbers":[],"session_refs":[],"device_ids":[],"application_ids":[],"effect_quote":""})).collect::<Vec<_>>()})
    }
    #[test]
    fn owned_whole_scope_supplies_only_missing_source_annotations_and_real_containers() {
        let ctx = context();
        let text = "我已经确认了，直接关闭全部窗口";
        let refs = ["w1", "w2", "w3", "w4"];
        let (plan, frozen) = candidate(&ctx, &refs);
        let mut raw = review("all", "全部窗口", &refs);
        raw.as_object_mut().unwrap().remove("batch_close_scope");
        for (row, reference) in raw["steps"].as_array_mut().unwrap().iter_mut().zip(refs) {
            row["window_basis"] = json!("conversation");
            row["window_quote"] = json!("该窗口");
            let window = ctx["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["window_ref"] == reference)
                .unwrap();
            let monitor = ctx["monitors"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["monitor_token"] == window["monitor_token"])
                .unwrap();
            row["monitor_refs"] = json!([monitor["monitor_ref"]]);
        }
        let before = raw.clone();
        let hash = frozen.plan_hash.clone();
        let proof = prove(Some(text), None, &raw, &plan, &frozen, &ctx).unwrap();
        assert!(proof.matches(&plan, &frozen, &ctx, &raw));
        assert!(frozen.requires_confirmation);
        assert_eq!(raw, before);
        assert_eq!(frozen.plan_hash, hash);
        for variant in [
            "scope", "none", "quote", "screen", "tag", "row", "name", "effect",
        ] {
            let mut wrong = raw.clone();
            match variant {
                "scope" => wrong["batch_close_scope"] = json!("current_tag_visible"),
                "none" => wrong["batch_close_scope"] = json!("none"),
                "quote" => wrong["batch_close_quote"] = json!("我已经确认了"),
                "screen" => wrong["steps"][0]["monitor_refs"] = json!(["m2"]),
                "tag" => wrong["steps"][0]["tag_numbers"] = json!([1]),
                "row" => wrong["steps"].as_array_mut().unwrap().swap(0, 1),
                "name" => wrong["steps"][0]["name"] = json!("quit"),
                _ => wrong["steps"][0]["effect_quote"] = json!("关闭再退出"),
            }
            assert!(
                prove(Some(text), None, &wrong, &plan, &frozen, &ctx).is_none(),
                "{variant}"
            );
            assert!(
                !proof.matches(&plan, &frozen, &ctx, &wrong),
                "bound original raw review {variant}"
            );
        }
        for owned in ["不要关闭全部窗口", "关闭全部窗口然后退出"] {
            assert!(prove(Some(owned), None, &raw, &plan, &frozen, &ctx).is_none());
        }
        let (subset, subset_frozen) = candidate(&ctx, &["w1", "w2"]);
        assert!(prove(Some(text), None, &raw, &subset, &subset_frozen, &ctx).is_none());
        let mut unconfirmed = frozen.clone();
        unconfirmed.requires_confirmation = false;
        assert!(prove(Some(text), None, &raw, &plan, &unconfirmed, &ctx).is_none());
        let audio =
            FreshAudioReview::from_correlated_receipt("audio", "resp", "binding", &raw).unwrap();
        assert!(prove(None, Some(audio), &raw, &plan, &frozen, &ctx).is_none());
        let mut audio_raw = raw.clone();
        audio_raw["batch_close_scope"] = json!("all");
        audio_raw["batch_close_quote"] = json!("关闭全部窗口");
        let audio =
            FreshAudioReview::from_correlated_receipt("audio", "resp", "binding", &audio_raw)
                .unwrap();
        assert!(
            prove(None, Some(audio), &audio_raw, &plan, &frozen, &ctx).is_none(),
            "audio cannot acquire owned descriptive-container proof"
        );
    }

    #[test]
    fn literal_all_and_current_visible_sets_are_exact_and_require_confirmation() {
        let ctx = context();
        for (text, scope, refs) in [
            (
                "我已经确认了，直接关闭全部窗口",
                "all",
                vec!["w4", "w2", "w1", "w3"],
            ),
            (
                "关闭当前 tag 的所有窗口",
                "current_tag_visible",
                vec!["w4", "w1"],
            ),
            ("关闭所有窗口", "all", vec!["w4", "w2", "w1", "w3"]),
            (
                "关闭当前工作区的所有可见窗口",
                "current_tag_visible",
                vec!["w4", "w1"],
            ),
        ] {
            let (plan, frozen) = candidate(&ctx, &refs);
            let review = review(scope, text, &refs);
            let proof = prove(Some(text), None, &review, &plan, &frozen, &ctx).unwrap();
            assert!(matches!(proof, BatchCloseEvidence::Owned(_)));
            assert!(proof.matches(&plan, &frozen, &ctx, &review));
            assert!(frozen.requires_confirmation);
            let local_summary = super::super::confirmation_summary(&frozen, &ctx);
            for reference in &refs {
                assert!(
                    local_summary.contains(reference),
                    "every frozen close target is in local confirmation"
                );
            }

            let mut annotated = plan.clone();
            for action in &mut annotated.actions {
                let reference = action.arguments["window_ref"].as_str().unwrap();
                let w = ctx["windows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|w| w["window_ref"] == reference)
                    .unwrap();
                action.arguments["monitor_ref"] = json!(if w["monitor_token"] == "mon1" {
                    "m1"
                } else {
                    "m2"
                });
                action.arguments["tag_number"] = json!(if w["tag_mask"] == 1 { 1 } else { 2 });
            }
            let af = jwm_eva_core::actions::validate_plan(&annotated, &ctx).unwrap();
            assert!(prove(Some(text), None, &review, &annotated, &af, &ctx).is_some());
            annotated.actions[0].arguments["monitor_ref"] = json!("m2");
            assert_eq!(
                jwm_eva_core::actions::validate_plan(&annotated, &ctx).unwrap_err(),
                "close_source_scope_mismatch"
            );

            let mut unconfirmed = frozen.clone();
            unconfirmed.requires_confirmation = false;
            assert!(prove(Some(text), None, &review, &plan, &unconfirmed, &ctx).is_none());
            let mut changed = ctx.clone();
            changed["wm_instance_id"] = json!("restart");
            assert!(!proof.matches(&plan, &frozen, &changed, &review));
            let mut changed = ctx.clone();
            changed["windows"][0]["tag_mask"] = json!(4);
            assert!(!proof.matches(&plan, &frozen, &changed, &review));
        }
    }
    #[test]
    fn incomplete_duplicate_missing_extra_and_mixed_effects_never_form_set_proof() {
        let ctx = context();
        let refs = ["w1", "w2", "w3", "w4"];
        let text = "关闭所有窗口";
        let (plan, frozen) = candidate(&ctx, &refs);
        let review = review("all", text, &refs);
        for field in ["windows_complete", "session_unlocked"] {
            let mut bad = ctx.clone();
            bad[field] = json!(false);
            assert!(prove(Some(text), None, &review, &plan, &frozen, &bad).is_none());
        }
        for field in ["target_token", "window_ref"] {
            let mut bad = ctx.clone();
            bad["windows"][1][field] = bad["windows"][0][field].clone();
            assert!(prove(Some(text), None, &review, &plan, &frozen, &bad).is_none());
        }
        for refs in [vec!["w1", "w2", "w3"], vec!["w1", "w2", "w3", "w3"]] {
            let (bad, bf) = candidate(&ctx, &refs);
            assert!(prove(Some(text), None, &review, &bad, &bf, &ctx).is_none());
        }
        let (mut mixed, _) = candidate(&ctx, &refs);
        mixed.actions[0].name = "focus_window".into();
        let mf = jwm_eva_core::actions::validate_plan(&mixed, &ctx).unwrap();
        assert!(prove(Some(text), None, &review, &mixed, &mf, &ctx).is_none());
        let mut extra = review.clone();
        extra["steps"][0]["device_ids"] = json!(["mic"]);
        assert!(prove(Some(text), None, &extra, &plan, &frozen, &ctx).is_none());
        for text in [
            "不要关闭所有窗口",
            "关闭所有窗口，除了邮件",
            "关闭所有浏览器窗口",
            "关闭所有窗口然后静音",
            "已确认，关闭所有窗口",
        ] {
            assert!(prove(Some(text), None, &review, &plan, &frozen, &ctx).is_none());
        }
        let mut truncated = ctx.clone();
        for n in 5..=129 {
            truncated["windows"].as_array_mut().unwrap().push(json!({"window_ref":format!("w{n}"),"target_token":format!("win{n}"),"monitor_token":"mon1","tag_mask":1,"visible":true,"sticky":false}));
        }
        assert!(prove(Some(text), None, &review, &plan, &frozen, &truncated).is_none());
    }
    #[test]
    fn current_tag_needs_one_live_selected_monitor_and_one_visible_profile() {
        let ctx = context();
        let text = "关闭当前工作区的所有可见窗口";
        let refs = ["w1", "w4"];
        let (plan, frozen) = candidate(&ctx, &refs);
        let review = review("current_tag_visible", text, &refs);
        for visible in [0, 3] {
            let mut bad = ctx.clone();
            bad["monitors"][0]["visible_tag_mask"] = json!(visible);
            assert!(prove(Some(text), None, &review, &plan, &frozen, &bad).is_none());
        }
        let mut bad = ctx.clone();
        bad["selected_monitor_token"] = json!("gone");
        assert!(prove(Some(text), None, &review, &plan, &frozen, &bad).is_none());
        let mut bad = ctx.clone();
        let duplicate = bad["monitors"][0].clone();
        bad["monitors"].as_array_mut().unwrap().push(duplicate);
        assert!(prove(Some(text), None, &review, &plan, &frozen, &bad).is_none());
        let (extra, ef) = candidate(&ctx, &["w1", "w3", "w4"]);
        assert!(prove(Some(text), None, &review, &extra, &ef, &ctx).is_none());
        let proof = prove(Some(text), None, &review, &plan, &frozen, &ctx).unwrap();
        let mut changed = ctx.clone();
        changed["monitors"][0]["visible_tag_mask"] = json!(2);
        assert!(!proof.matches(&plan, &frozen, &changed, &review));
    }
    #[test]
    fn audio_is_separate_fresh_bound_nlu_claim_never_owned_or_self_attested() {
        let ctx = context();
        let refs = ["w1", "w2", "w3", "w4"];
        let text = "close all windows";
        let (plan, frozen) = candidate(&ctx, &refs);
        let review = review("all", text, &refs);
        assert!(prove(None, None, &review, &plan, &frozen, &ctx).is_none());
        let marker = FreshAudioReview::from_correlated_receipt(
            "owned-audio",
            "fresh-review",
            "machine-binding",
            &review,
        )
        .unwrap();
        let claim = prove(None, Some(marker), &review, &plan, &frozen, &ctx).unwrap();
        assert!(matches!(claim, BatchCloseEvidence::UnverifiedAudio(_)));
        assert!(claim.matches(&plan, &frozen, &ctx, &review));
        let mut forged = review.clone();
        forged["batch_close_scope"] = json!("current_tag_visible");
        assert!(!claim.matches(&plan, &frozen, &ctx, &forged));
        let marker = FreshAudioReview::from_correlated_receipt(
            "owned-audio",
            "fresh-review",
            "machine-binding",
            &review,
        )
        .unwrap();
        assert!(FreshAudioReview::copy_for_review(&marker, &forged).is_none());
        assert!(
            prove(
                Some("不要关闭所有窗口"),
                None,
                &review,
                &plan,
                &frozen,
                &ctx
            )
            .is_none()
        );
    }
    #[test]
    fn source_projection_and_connected_matcher_preserve_complete_set_contract() {
        let ctx = context();
        assert_eq!(
            super::super::cloud_context_for_categories(&ctx, &[])["windows_complete"],
            false
        );
        assert_eq!(
            super::super::cloud_context_for_categories(&ctx, &["windows".into()])["windows_complete"],
            true
        );
        let refs = ["w1", "w2", "w3", "w4"];
        let text = "关闭所有窗口";
        let (plan, frozen) = candidate(&ctx, &refs);
        let review = review("all", text, &refs);
        let mut input = crate::planning_input::FrozenInput::default();
        input.set_text(text).unwrap();
        let source =
            super::super::grounding_review::SourceEvidence::from_actor(&input, &ctx, None, None);
        assert!(super::super::grounding_review::matches_grounded_candidate(
            &review, &plan, &frozen, &ctx, &source
        ));
        let mut denied = review.clone();
        denied["steps"][0]["name"] = json!("query_window");
        assert!(!super::super::grounding_review::matches_grounded_candidate(
            &denied, &plan, &frozen, &ctx, &source
        ));
        let mut audio_input = crate::planning_input::FrozenInput::default();
        audio_input.set_audio("owned-audio").unwrap();
        let mut source = super::super::grounding_review::SourceEvidence::from_actor(
            &audio_input,
            &ctx,
            None,
            None,
        );
        assert!(!super::super::grounding_review::matches_grounded_candidate(
            &review, &plan, &frozen, &ctx, &source
        ));
        source.audio_batch_review = FreshAudioReview::from_correlated_receipt(
            "owned-audio",
            "fresh-response",
            "machine-binding",
            &review,
        );
        assert!(super::super::grounding_review::matches_grounded_candidate(
            &review, &plan, &frozen, &ctx, &source
        ));
        let mut incomplete = ctx.clone();
        incomplete["windows_complete"] = json!(false);
        assert!(!super::super::grounding_review::matches_grounded_candidate(
            &review,
            &plan,
            &frozen,
            &incomplete,
            &source
        ));
    }
}
