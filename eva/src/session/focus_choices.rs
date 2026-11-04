//! Actor-owned focus disambiguation with owned-text proofs and separate
//! lower-assurance model-audio claims. No model prose supplies ordering
//! or purpose; no execution/confirmation authority is created here.
use crate::clarification::*;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Read-only purpose fact from an actually published live actor question.
/// Not a selection, target reference, confirmation or serialized history DTO.
pub(super) struct PendingQuestionFacts {
    _private: (),
}
impl PendingQuestionFacts {
    pub(super) fn object_question(&self) -> &'static str {
        "你要聚焦哪个窗口？请明确窗口名称或位置。"
    }
}

pub(super) struct Actor {
    slot: WindowChoiceSlot,
    intent: Option<ActorCheckedFocusIntent>,
    draft: Option<QuestionDraft>,
    question: Option<QuestionId>,
    selected: Option<SelectedWindow>,
    turn: Option<TurnStamp>,
    audio_claim: Option<ModelAudioSelectionClaim>,
    audio_binding: Option<(String, String)>,
    choices: Vec<String>,
    audio_purpose: bool,
}
impl Default for Actor {
    fn default() -> Self {
        Self {
            slot: WindowChoiceSlot::new(Duration::from_secs(60)).expect("valid fixed TTL"),
            intent: None,
            draft: None,
            question: None,
            selected: None,
            turn: None,
            audio_claim: None,
            audio_binding: None,
            choices: Vec::new(),
            audio_purpose: false,
        }
    }
}
impl Actor {
    fn clear_active(&mut self) {
        self.slot.clear();
        self.intent = None;
        self.draft = None;
        self.question = None;
        self.selected = None;
        self.audio_claim = None;
        self.audio_binding = None;
        self.choices.clear();
        self.audio_purpose = false;
        // Keep actor turn, including after revocation, to avoid re-consuming
        // an input during a bounded same-turn model retry.
    }
    fn live_question(&self, context: &Value) -> bool {
        self.question
            .as_ref()
            .zip(self.turn)
            .is_some_and(|(id, turn)| {
                self.slot
                    .has_live_presentation(id, turn, context, Instant::now())
            })
    }
    pub(super) fn pending_question_facts(&self, context: &Value) -> Option<PendingQuestionFacts> {
        (!self.audio_purpose
            && self.intent.is_some()
            && self.draft.is_none()
            && self.selection(context).is_none()
            && self.live_question(context))
        .then_some(PendingQuestionFacts { _private: () })
    }
    pub(super) fn begin(&mut self, text: Option<&str>, context: &Value, turn: TurnStamp) {
        // Never re-resolve the same turn during a bounded model retry.
        if self.turn == Some(turn) {
            if self.draft.is_none()
                && self.selection(context).is_none()
                && !self.live_question(context)
            {
                self.clear_active();
            }
            return;
        }
        self.selected = None;
        self.audio_claim = None;
        self.audio_binding = None;
        self.slot.on_turn(turn);
        self.turn = Some(turn);
        if !self.live_question(context) {
            self.clear_active();
            return;
        }
        if let (Some(id), Some(text)) = (self.question.clone(), text) {
            self.selected = self
                .slot
                .resolve(
                    &id,
                    turn,
                    SelectionInput::OwnedText(text),
                    context,
                    Instant::now(),
                )
                .ok();
        }
        if self.selected.is_none() && (text.is_some() || self.question.is_none()) {
            self.clear_active();
        }
    }
    pub(super) fn selection(&self, context: &Value) -> Option<&str> {
        if let (Some(claim), Some((item, response)), Some(intent), Some(turn)) = (
            &self.audio_claim,
            &self.audio_binding,
            &self.intent,
            self.turn,
        ) {
            let scope = AudioClaimScope {
                turn,
                owned_audio_item_id: item,
                review_response_id: response,
            };
            if claim.matches_focus_window(
                "focus_window",
                claim.window_ref(),
                intent,
                scope,
                context,
                Instant::now(),
            ) {
                return Some(claim.window_ref());
            }
        }
        let selected = self.selected.as_ref()?;
        selected
            .proof()
            .matches_focus_window(
                "focus_window",
                selected.window_ref(),
                self.intent.as_ref()?,
                self.turn?,
                context,
                Instant::now(),
            )
            .then(|| selected.window_ref())
    }
    pub(super) fn hint(&self, context: &Value) -> Value {
        if let Some(reference) = self.selection(context) {
            return json!({"purpose":"focus_window","window_ref":reference,"basis":"clarification_choice","published_local_text":true,"selection_complete":true,"execution_authorized":false,"purpose_assurance":if self.audio_purpose {"model_audio_quote"} else {"owned_text"},"selection_assurance":if self.audio_claim.is_some() {"unverified_audio"} else {"owned_text"}});
        }
        if self.intent.is_some() && self.draft.is_none() && self.live_question(context) {
            return json!({"purpose":"focus_window","ordered_window_refs":self.choices,"published_local_text":true,"action_approval":false,"audio_assurance":"unverified_audio","purpose_assurance":if self.audio_purpose {"model_audio_quote"} else {"owned_text"}});
        }
        Value::Null
    }
    // Called only after the caller verified a fresh, server-correlated review
    // response. The model quotation remains a lower-assurance audio NLU claim.
    pub(super) fn reviewed_audio(
        &mut self,
        item: &str,
        response: &str,
        review: &Value,
        context: &Value,
    ) {
        let Some(id) = self.question.as_ref() else {
            return;
        };
        let Some(turn) = self.turn else { return };
        let Some(steps) = review["steps"].as_array().filter(|steps| steps.len() == 1) else {
            return;
        };
        if review["decision"] != "allow" || steps[0]["window_basis"] != "clarification_choice" {
            return;
        }
        let Some(quote) = steps[0]["window_quote"].as_str() else {
            return;
        };
        let scope = AudioClaimScope {
            turn,
            owned_audio_item_id: item,
            review_response_id: response,
        };
        let now = Instant::now();
        if self
            .slot
            .bind_audio_review(id, scope, context, now)
            .is_err()
        {
            return;
        }
        let Ok(quote) = ModelAudioQuote::from_actor_response(
            id,
            AudioClaimScope {
                turn,
                owned_audio_item_id: item,
                review_response_id: response,
            },
            quote,
            Instant::now(),
        ) else {
            return;
        };
        if let Ok(claim) = self
            .slot
            .resolve_model_audio_claim(quote, context, Instant::now())
        {
            self.audio_claim = Some(claim);
            self.audio_binding = Some((item.to_owned(), response.to_owned()));
        }
    }
    pub(super) fn prepare(&mut self, text: Option<&str>, context: &Value) -> Option<String> {
        self.prepare_quoted(text, context, "actor-owned-text-focus")
    }
    fn prepare_quoted(
        &mut self,
        text: Option<&str>,
        context: &Value,
        source_id: &str,
    ) -> Option<String> {
        let text = text?.trim();
        // Deliberately bounded first production surface. Negation, quoted titles,
        // close requests and compound instructions cannot mint focus purpose.
        let family = match text.to_lowercase().as_str() {
            "聚焦浏览器" | "切换到浏览器" | "focus browser" | "focus the browser" => {
                "browser"
            }
            "聚焦终端" | "切换到终端" | "focus terminal" | "focus the terminal" => {
                "terminal"
            }
            _ => return None,
        };
        let windows = context["windows"].as_array()?;
        if windows.len() > 128 {
            return None;
        }
        let mut refs = windows
            .iter()
            .filter(|window| {
                window["class"]
                    .as_str()
                    .and_then(crate::language::application_family)
                    == Some(family)
            })
            .filter_map(|window| window["window_ref"].as_str())
            .collect::<Vec<_>>();
        refs.sort_unstable();
        if !(2..=3).contains(&refs.len()) {
            return None;
        }
        let turn = self.turn?;
        let intent = ActorCheckedFocusIntent::new(
            "focus_window",
            ActorIntentVerdict::AffirmativeFocus,
            turn,
            source_id,
        )
        .ok()?;
        let draft = self
            .slot
            .prepare_focus(intent.clone(), context, &refs, turn, Instant::now())
            .ok()?;
        let result = draft.text().to_owned();
        self.choices = refs
            .iter()
            .map(|reference| (*reference).to_owned())
            .collect();
        self.question = Some(draft.id());
        self.draft = Some(draft);
        self.intent = Some(intent);
        Some(result)
    }
    pub(super) fn prepare_request(
        &mut self,
        request: &Value,
        owned_text: Option<&str>,
        current_audio: Option<&str>,
        context: &Value,
    ) -> Option<String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            request_quote: String,
            selector_quote: String,
            window_refs: Vec<String>,
        }
        let request: Request = serde_json::from_value(request.clone()).ok()?;
        if request.request_quote.len() > 256
            || request.selector_quote.len() > 64
            || !(2..=3).contains(&request.window_refs.len())
        {
            return None;
        }
        // Owned text must be the whole actual input. Raw audio remains model NLU,
        // never an OwnedText value or acoustic certificate.
        if owned_text.is_some_and(|text| text != request.request_quote)
            || (owned_text.is_none() && current_audio.is_none())
        {
            return None;
        }
        let selector = request.selector_quote.to_lowercase();
        if !["浏览器", "browser", "终端", "terminal"].contains(&selector.as_str())
            || !request.request_quote.to_lowercase().contains(&selector)
        {
            return None;
        }
        let source_id = current_audio.map(|id| format!("model-audio-focus:{id}"));
        let question = self.prepare_quoted(
            Some(&request.request_quote),
            context,
            source_id.as_deref().unwrap_or("actor-owned-text-focus"),
        )?;
        let mut proposed = request.window_refs;
        proposed.sort();
        if proposed != self.choices {
            self.slot.clear();
            self.question = None;
            self.draft = None;
            self.intent = None;
            return None;
        }
        self.audio_purpose = owned_text.is_none();
        Some(question)
    }
    pub(super) fn finish_proposal(&mut self, plan: &jwm_eva_core::actions::Plan, context: &Value) {
        let new_question = self.draft.as_ref().is_some_and(|draft| {
            plan.disposition == jwm_eva_core::actions::Disposition::Clarify
                && plan.question.as_deref() == Some(draft.text())
        });
        let selected_focus = self.selection(context).is_some()
            && plan.disposition == jwm_eva_core::actions::Disposition::Execute
            && plan.actions.len() == 1
            && plan.actions[0].name == "focus_window";
        if !new_question && !selected_focus {
            self.clear_active();
        }
    }
    pub(super) fn published(&mut self, text: &str, context: &Value) {
        if let (Some(draft), Some(turn)) = (self.draft.take(), self.turn)
            && self
                .slot
                .commit_presented_text(&draft.id(), text, turn, context, Instant::now())
                .is_err()
        {
            self.slot.clear();
            self.question = None;
            self.intent = None;
        }
    }
}

pub(super) fn add_schema(tool: &mut Value) {
    let request = json!({"type":"object","description":"仅明确请求切换/聚焦浏览器或终端且有2至3个同类窗口时，clarify/actions=[]/session_intent=continue可请求本机生成编号聚焦问题。不能用于关闭、否定或其它动作；request_quote逐字完整原请求，selector_quote逐字类别；音频引文只是未核验NLU。","properties":{"request_quote":{"type":"string","maxLength":160},"selector_quote":{"type":"string","maxLength":32},"window_refs":{"type":"array","minItems":2,"maxItems":3,"uniqueItems":true,"items":{"type":"string","maxLength":64}}},"required":["request_quote","selector_quote","window_refs"],"additionalProperties":false});
    tool["parameters"]["properties"]["focus_choice_request"] =
        json!({"anyOf":[request,{"type":"null"}]});
}
pub(super) fn decode_event(event: &Value) -> std::io::Result<(Value, Option<Value>)> {
    let mut clean = event.clone();
    let mut request = None;
    if let Some(output) = clean["response"]["output"].as_array_mut() {
        for item in output {
            if item["type"] != "function_call" || item["name"] != "eva_submit_plan" {
                continue;
            }
            let mut args: Value = serde_json::from_str(item["arguments"].as_str().unwrap_or(""))
                .map_err(std::io::Error::other)?;
            if let Some(value) = args
                .as_object_mut()
                .and_then(|args| args.remove("focus_choice_request"))
            {
                if value.is_null() {
                    item["arguments"] = json!(args.to_string());
                    continue;
                }
                if request.is_some()
                    || args["disposition"] != "clarify"
                    || args["session_intent"] != "continue"
                    || args["actions"]
                        .as_array()
                        .is_none_or(|actions| !actions.is_empty())
                {
                    return Err(std::io::Error::other("invalid_focus_choice_request"));
                }
                let mut tool = json!({"parameters":{"properties":{}}});
                add_schema(&mut tool);
                jwm_eva_core::actions::validate_schema(
                    &value,
                    &tool["parameters"]["properties"]["focus_choice_request"],
                )
                .map_err(std::io::Error::other)?;
                request = Some(value);
                item["arguments"] = json!(args.to_string());
            }
        }
    }
    Ok((clean, request))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> Value {
        json!({"wm_instance_id":"wm","windows":[
        {"window_ref":"w1","target_token":"native1","class":"browser","title":"News"},
        {"window_ref":"w2","target_token":"native2","class":"browser","title":"Mail"}]})
    }
    fn turn(id: u64) -> TurnStamp {
        TurnStamp {
            generation: 1,
            session_epoch: 1,
            turn_id: id,
        }
    }
    fn ask(actor: &mut Actor, ctx: &Value) -> String {
        actor.begin(Some("聚焦浏览器"), ctx, turn(1));
        actor.prepare(Some("聚焦浏览器"), ctx).unwrap()
    }
    #[test]
    fn only_actual_canonical_publication_enables_next_focus() {
        let ctx = context();
        let mut actor = Actor::default();
        let question = ask(&mut actor, &ctx);
        actor.begin(Some("第一个"), &ctx, turn(2));
        assert_eq!(actor.selection(&ctx), None);
        let mut actor = Actor::default();
        ask(&mut actor, &ctx);
        actor.published(&question, &ctx);
        actor.begin(Some("第一个"), &ctx, turn(2));
        assert_eq!(actor.selection(&ctx), Some("w1"));
        let proof = actor.selected.as_ref().unwrap().proof();
        assert!(!proof.matches_focus_window(
            "killclient",
            "w1",
            actor.intent.as_ref().unwrap(),
            turn(2),
            &ctx,
            Instant::now()
        ));
        actor.begin(Some("第一个"), &ctx, turn(3));
        assert_eq!(actor.selection(&ctx), None);
    }
    #[test]
    fn changed_windows_cancel_and_negated_close_do_not_select() {
        let ctx = context();
        for text in ["取消", "别关闭第一个", "第一个并关闭它"] {
            let mut actor = Actor::default();
            let q = ask(&mut actor, &ctx);
            actor.published(&q, &ctx);
            actor.begin(Some(text), &ctx, turn(2));
            assert_eq!(actor.selection(&ctx), None);
        }
        let mut actor = Actor::default();
        let q = ask(&mut actor, &ctx);
        actor.published(&q, &ctx);
        let mut changed = ctx.clone();
        changed["windows"][0]["target_token"] = json!("reused");
        actor.begin(Some("第一个"), &changed, turn(2));
        assert_eq!(actor.selection(&changed), None);
        let mut actor = Actor::default();
        actor.begin(Some("不要关闭浏览器"), &ctx, turn(1));
        assert!(actor.prepare(Some("不要关闭浏览器"), &ctx).is_none());
    }
    #[test]
    fn audio_quote_is_separate_fresh_correlated_claim() {
        let ctx = context();
        let mut actor = Actor::default();
        let q = ask(&mut actor, &ctx);
        actor.published(&q, &ctx);
        actor.begin(None, &ctx, turn(2));
        actor.reviewed_audio("owned-audio","fresh-review",&json!({"decision":"allow","steps":[{"window_basis":"clarification_choice","window_quote":"第二个"}]}),&ctx);
        assert_eq!(actor.selection(&ctx), Some("w2"));
        assert!(actor.selected.is_none());
        assert_eq!(
            actor.audio_claim.as_ref().unwrap().assurance(),
            "unverified_audio"
        );
        actor.begin(None, &ctx, turn(3));
        assert_eq!(actor.selection(&ctx), None);
    }
    #[test]
    fn initial_audio_request_is_question_only_and_schema_scoped() {
        let ctx = context();
        let mut actor = Actor::default();
        actor.begin(None, &ctx, turn(1));
        let request = json!({"request_quote":"聚焦浏览器","selector_quote":"浏览器","window_refs":["w2","w1"]});
        let question = actor
            .prepare_request(&request, None, Some("owned-original-audio"), &ctx)
            .unwrap();
        assert!(actor.audio_purpose);
        assert_eq!(actor.selection(&ctx), None);
        actor.published(&question, &ctx);
        actor.begin(None, &ctx, turn(2));
        actor.reviewed_audio("owned-answer","fresh-response",&json!({"decision":"allow","steps":[{"window_basis":"clarification_choice","window_quote":"第一个"}]}),&ctx);
        assert_eq!(actor.selection(&ctx), Some("w1"));
        let mut actor = Actor::default();
        actor.begin(Some("不要关闭浏览器"), &ctx, turn(1));
        assert!(
            actor
                .prepare_request(&request, Some("不要关闭浏览器"), None, &ctx)
                .is_none()
        );
        let mut bad = request.clone();
        bad["window_refs"] = json!(["w1", "missing"]);
        assert!(
            actor
                .prepare_request(&bad, None, Some("owned-audio"), &ctx)
                .is_none()
        );
        let event = |disposition, actions, intent, request: Value| json!({"response":{"output":[{"type":"function_call","name":"eva_submit_plan","arguments":json!({"disposition":disposition,"actions":actions,"session_intent":intent,"focus_choice_request":request}).to_string()}]}});
        assert!(
            decode_event(&event("clarify", json!([]), "continue", request.clone()))
                .unwrap()
                .1
                .is_some()
        );
        assert!(decode_event(&event("execute", json!([]), "continue", request.clone())).is_err());
        assert!(decode_event(&event("clarify", json!([]), "standby", request.clone())).is_err());
        let mut extra = request;
        extra["argv"] = json!(["bad"]);
        assert!(decode_event(&event("clarify", json!([]), "continue", extra)).is_err());
    }
    #[test]
    fn local_selection_only_grounds_new_focus_candidate() {
        use crate::session::grounding_review::{SourceEvidence, matches_grounded_candidate};
        let mut ctx = context();
        ctx["session_unlocked"] = json!(true);
        ctx["tag_count"] = json!(9);
        ctx["focus_token"] = json!("native2");
        ctx["selected_monitor_token"] = json!("mon1");
        ctx["monitors"] = json!([{"monitor_ref":"m1","monitor_token":"mon1","locked":false,"visible_tag_mask":1}]);
        for window in ctx["windows"].as_array_mut().unwrap() {
            window["monitor_token"] = json!("mon1");
            window["tag_mask"] = json!(1);
        }
        let mut actor = Actor::default();
        let q = ask(&mut actor, &ctx);
        actor.published(&q, &ctx);
        actor.begin(Some("第一个"), &ctx, turn(2));
        let mut input = crate::planning_input::FrozenInput::default();
        input.set_text("第一个").unwrap();
        let mut source = SourceEvidence::from_actor(&input, &ctx, None, None);
        source.focus_selection = actor.selection(&ctx).map(str::to_owned);
        let review = json!({"decision":"allow","session_intent":"continue","steps":[{"name":"focus_window","window_refs":["w1"],"window_basis":"clarification_choice","window_quote":"第一个","monitor_refs":[],"monitor_basis":"none","monitor_quote":"","tag_numbers":[],"session_refs":[],"device_ids":[],"application_ids":[]}]});
        let plan: jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"focus_window","arguments":{"window_ref":"w1"}}],"question":null,"reason":null})).unwrap();
        let frozen = jwm_eva_core::actions::validate_plan(&plan, &ctx).unwrap();
        assert!(matches_grounded_candidate(
            &review, &plan, &frozen, &ctx, &source
        ));
        let mut wrong = plan.clone();
        wrong.actions[0].arguments["window_ref"] = json!("w2");
        let frozen = jwm_eva_core::actions::validate_plan(&wrong, &ctx).unwrap();
        assert!(!matches_grounded_candidate(
            &review, &wrong, &frozen, &ctx, &source
        ));
        wrong.actions[0].name = "set_window_maximized".into();
        wrong.actions[0].arguments = json!({"window_ref":"w1","enabled":true});
        let frozen = jwm_eva_core::actions::validate_plan(&wrong, &ctx).unwrap();
        assert!(!matches_grounded_candidate(
            &review, &wrong, &frozen, &ctx, &source
        ));
    }
    #[test]
    fn null_request_preserves_execute_and_protected_arguments() {
        let original = json!({"disposition":"execute","session_intent":"continue","task_intent":"new","actions":[{"name":"killclient","arguments":{"window_ref":"w1","assistant_permit":"must-not-drop","confirmation_token":"must-not-drop"}}],"question":null,"reason":"owned raw attribution","focus_choice_request":null});
        let event = |args: &Value| json!({"response":{"output":[{"type":"function_call","name":"eva_submit_plan","arguments":args.to_string()}]}});
        let (clean, request) = decode_event(&event(&original)).unwrap();
        assert!(request.is_none());
        let actual: Value = serde_json::from_str(
            clean["response"]["output"][0]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let mut expected = original.clone();
        expected
            .as_object_mut()
            .unwrap()
            .remove("focus_choice_request");
        assert_eq!(actual, expected);
        assert_eq!(
            event(&original)["response"]["output"][0]["arguments"],
            json!(original.to_string())
        );
        for value in [
            json!(false),
            json!("null"),
            json!([]),
            json!({"assistant_permit":"bad"}),
        ] {
            let mut args = original.clone();
            args["focus_choice_request"] = value;
            assert!(decode_event(&event(&args)).is_err());
        }
        let mut tool = json!({"parameters":{"properties":{}}});
        add_schema(&mut tool);
        assert!(
            jwm_eva_core::actions::validate_schema(
                &Value::Null,
                &tool["parameters"]["properties"]["focus_choice_request"]
            )
            .is_ok()
        );
    }
    #[test]
    fn exact_application_family_excludes_helper_substrings() {
        let mut ctx = context();
        ctx["windows"].as_array_mut().unwrap().push(json!({"window_ref":"w3","target_token":"helper","class":"nonbrowserhelper","title":"Helper"}));
        let mut actor = Actor::default();
        ask(&mut actor, &ctx);
        assert_eq!(actor.choices, vec!["w1", "w2"]);
        ctx["windows"][1]["class"] = json!("firefox-password-helper");
        let mut actor = Actor::default();
        actor.begin(Some("聚焦浏览器"), &ctx, turn(1));
        assert!(actor.prepare(Some("聚焦浏览器"), &ctx).is_none());
        ctx["windows"][0]["class"] = json!("kitty-helper");
        ctx["windows"][1]["class"] = json!("kitty");
        let mut actor = Actor::default();
        actor.begin(Some("聚焦终端"), &ctx, turn(1));
        assert!(actor.prepare(Some("聚焦终端"), &ctx).is_none());
    }
    #[test]
    fn revoked_presentation_clears_all_actor_references_for_raw_audio() {
        let ctx = context();
        for mode in ["skipped_turn", "wm", "token"] {
            let mut actor = Actor::default();
            let q = ask(&mut actor, &ctx);
            actor.published(&q, &ctx);
            let mut current = ctx.clone();
            let next = if mode == "skipped_turn" {
                turn(3)
            } else {
                turn(2)
            };
            if mode == "wm" {
                current["wm_instance_id"] = json!("new-wm");
            }
            if mode == "token" {
                current["windows"][0]["target_token"] = json!("reused");
            }
            // Read-only hint must independently fail before actor cleanup.
            if mode != "skipped_turn" {
                assert!(actor.hint(&current).is_null());
            }
            actor.begin(None, &current, next);
            assert!(actor.hint(&current).is_null());
            assert!(actor.selection(&current).is_none());
            assert!(actor.question.is_none());
            assert!(actor.intent.is_none());
            assert!(actor.choices.is_empty());
            assert_eq!(actor.turn, Some(next));
        }
    }
    #[test]
    fn nonselection_final_audio_proposal_retires_question() {
        let ctx = context();
        let mut actor = Actor::default();
        let q = ask(&mut actor, &ctx);
        actor.published(&q, &ctx);
        actor.begin(None, &ctx, turn(2));
        assert!(!actor.hint(&ctx).is_null());
        let cancel: jwm_eva_core::actions::Plan = serde_json::from_value(
            json!({"disposition":"noop","actions":[],"question":null,"reason":null}),
        )
        .unwrap();
        actor.finish_proposal(&cancel, &ctx);
        assert!(actor.hint(&ctx).is_null());
        assert!(actor.question.is_none());
        assert_eq!(actor.turn, Some(turn(2)));
    }
    #[test]
    fn arabic_letter_mark_never_produces_a_published_numbered_question() {
        let mut ctx = context();
        ctx["windows"][0]["title"] = json!("\u{061c}Mail");
        let mut actor = Actor::default();
        actor.begin(Some("聚焦浏览器"), &ctx, turn(1));
        assert!(actor.prepare(Some("聚焦浏览器"), &ctx).is_none());
        actor.published("聚焦哪个窗口？", &ctx);
        assert!(actor.hint(&ctx).is_null());
        assert!(actor.selection(&ctx).is_none());
    }
    #[test]
    fn pending_purpose_facts_require_real_owned_canonical_live_publication_not_fixture_prose() {
        let mut ctx = context();
        ctx["previous_turn"] = json!({"status":"clarify","question":"项目文档还是邮件？","candidate_window_refs":["w1","w2"]});
        let mut actor = Actor::default();
        assert!(actor.pending_question_facts(&ctx).is_none());
        let question = ask(&mut actor, &ctx);
        assert!(
            actor.pending_question_facts(&ctx).is_none(),
            "draft has not been presented"
        );
        actor.published(&question, &ctx);
        let pending = actor
            .pending_question_facts(&ctx)
            .expect("actual canonical publication is live");
        actor.audio_purpose = true;
        assert!(
            actor.pending_question_facts(&ctx).is_none(),
            "audio purpose is not upgraded to owned text evidence"
        );
        actor.audio_purpose = false;
        assert_eq!(
            super::super::owned_reduction::ambiguous_deictic_question(
                Some("就刚才那个"),
                &Value::Null,
                false,
                Some(&pending)
            ),
            Some("你要聚焦哪个窗口？请明确窗口名称或位置。")
        );
        actor.begin(Some("第一个"), &ctx, turn(2));
        assert!(
            actor.pending_question_facts(&ctx).is_none(),
            "verified selection retains its existing binding path"
        );
        let mut actor = Actor::default();
        let question = ask(&mut actor, &ctx);
        actor.published(&question, &ctx);
        actor.begin(Some("就刚才那个"), &ctx, turn(2));
        assert!(
            actor.pending_question_facts(&ctx).is_none(),
            "unresolved input must not revive a retired question"
        );
        let mut actor = Actor::default();
        ask(&mut actor, &ctx);
        actor.published("model prose not the local question", &ctx);
        assert!(actor.pending_question_facts(&ctx).is_none());
    }
}
