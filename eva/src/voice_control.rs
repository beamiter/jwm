//! Voice session control is a typed local lifecycle request, never a desktop
//! action or model-supplied IPC. The actor still owns response admission and
//! obtains the source fence from its locally verified execution context.
use jwm_eva_core::{
    actions::{Disposition, Plan},
    protocol::AssistantPermit,
    realtime::completed_tool_call,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIntent {
    #[default]
    Continue,
    Standby,
    StopListening,
}

pub fn add_schema(tool: &mut Value) {
    if tool["name"] != "eva_submit_plan" {
        return;
    }
    let Some(parameters) = tool.get_mut("parameters").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(properties) = parameters
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    properties.insert("session_intent".into(), json!({
        "type":"string","enum":["continue","standby","stop_listening"],
        "description":"continue继续对话；standby结束当前对话回到唤醒待机；stop_listening结束对话并关闭全时监听。结束只可提交noop，不包含桌面动作。"
    }));
    if let Some(required) = parameters.get_mut("required").and_then(Value::as_array_mut)
        && !required.iter().any(|field| field == "session_intent")
    {
        required.push(json!("session_intent"));
    }
}

/// Only a fully completed, singular function call can supply lifecycle intent.
/// The cleaned event must still pass completed_plan with the actor's expected
/// response ID and PlanAdmission. No deltas, prose or late response is admitted
/// by this decoder, and all unrelated argument fields remain for strict checks.
pub fn decode_event(event: &Value) -> io::Result<(Value, SessionIntent)> {
    if event["type"] != "response.done" || event["response"]["status"] != "completed" {
        return Ok((event.clone(), SessionIntent::Continue));
    }
    let id = event["response"]["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| io::Error::other("missing_response_id"))?;
    let call = completed_tool_call(event, id, &["eva_submit_plan", "eva_describe_actions"])
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("missing_final_tool_call"))?;
    if call.name != "eva_submit_plan" {
        return Ok((event.clone(), SessionIntent::Continue));
    }
    let mut arguments: Value =
        serde_json::from_str(&call.arguments).map_err(|_| io::Error::other("plan_invalid_json"))?;
    let intent = arguments
        .as_object_mut()
        .and_then(|arguments| arguments.remove("session_intent"))
        .ok_or_else(|| io::Error::other("missing_session_intent"))?;
    let intent: SessionIntent =
        serde_json::from_value(intent).map_err(|_| io::Error::other("invalid_session_intent"))?;
    let mut cleaned = event.clone();
    for item in cleaned["response"]["output"]
        .as_array_mut()
        .expect("validated tool output")
    {
        if item["type"] == "function_call" && item["name"] == "eva_submit_plan" {
            item["arguments"] = json!(arguments.to_string());
        }
    }
    Ok((cleaned, intent))
}

pub fn validate(intent: SessionIntent, plan: &Plan, task_intent: Option<&str>) -> io::Result<()> {
    if intent == SessionIntent::Continue {
        return Ok(());
    }
    if plan.disposition != Disposition::Noop || !plan.actions.is_empty() || plan.question.is_some()
    {
        return Err(io::Error::other("session_control_requires_noop"));
    }
    if task_intent.is_some_and(|intent| !["pause", "cancel", "new"].contains(&intent)) {
        return Err(io::Error::other("session_control_task_conflict"));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SessionEnd {
    pub intent: SessionIntent,
    wm_instance_id: String,
    wake_generation: Option<u64>,
}
impl SessionEnd {
    /// Read only native source context, before asynchronous speech playback or
    /// cleanup. Model arguments never supply instance IDs or wake generations.
    pub fn from_context(intent: SessionIntent, context: &Value) -> io::Result<Self> {
        if intent == SessionIntent::Continue {
            return Err(io::Error::other("session_end_requires_control_intent"));
        }
        let wm_instance_id = context["wm_instance_id"]
            .as_str()
            .filter(|id| bounded_id(id))
            .ok_or_else(|| io::Error::other("session_end_source_missing"))?
            .to_owned();
        let wake_generation = if intent == SessionIntent::StopListening {
            Some(
                context["wake_generation"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("session_end_wake_generation_missing"))?,
            )
        } else {
            None
        };
        Ok(Self {
            intent,
            wm_instance_id,
            wake_generation,
        })
    }

    /// The compositor checks the exact current permit, instance and (when
    /// disabling wake) wake generation atomically. A late finish cannot stop
    /// a newer session or a newly re-enabled wake listener.
    pub fn finish_request(&self, permit: &AssistantPermit) -> io::Result<Value> {
        if self.intent == SessionIntent::Continue {
            return Err(io::Error::other("session_end_requires_control_intent"));
        }
        if self.intent == SessionIntent::StopListening && self.wake_generation.is_none() {
            return Err(io::Error::other("session_end_wake_generation_missing"));
        }
        if !bounded_id(&permit.assistant_session_id) {
            return Err(io::Error::other("session_end_permit_invalid"));
        }
        let mut request = json!({
            "action":"finish_session","wm_instance_id":self.wm_instance_id,
            "assistant_permit":permit,"disable_wake":self.intent == SessionIntent::StopListening
        });
        if let Some(generation) = self.wake_generation {
            request["wake_generation"] = json!(generation);
        }
        Ok(request)
    }
}
fn bounded_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_eva_core::{actions::tool_schema, realtime::completed_plan};

    fn arguments() -> Value {
        json!({"session_intent":"standby","disposition":"noop","actions":[],"question":null,"reason":null})
    }
    fn done(arguments: &Value) -> Value {
        json!({"type":"response.done","response":{"id":"response-1","status":"completed","output":[{
            "type":"function_call","name":"eva_submit_plan","call_id":"call-1","status":"completed","arguments":arguments.to_string()
        }]}})
    }
    fn plan() -> Plan {
        Plan {
            disposition: Disposition::Noop,
            actions: vec![],
            question: None,
            reason: None,
        }
    }
    #[test]
    fn schema_is_required_typed_idempotent_and_only_changes_submit() {
        let mut schema = tool_schema();
        add_schema(&mut schema);
        add_schema(&mut schema);
        assert_eq!(
            schema["parameters"]["properties"]["session_intent"]["enum"],
            json!(["continue", "standby", "stop_listening"])
        );
        assert_eq!(
            schema["parameters"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|field| *field == "session_intent")
                .count(),
            1
        );
        let mut other =
            json!({"name":"eva_describe_actions","parameters":{"properties":{},"required":[]}});
        let unchanged = other.clone();
        add_schema(&mut other);
        assert_eq!(other, unchanged);
    }
    #[test]
    fn completed_submit_decodes_control_but_preserves_response_admission() {
        let (cleaned, intent) = decode_event(&done(&arguments())).unwrap();
        assert_eq!(intent, SessionIntent::Standby);
        let completed = completed_plan(&cleaned, "response-1").unwrap().unwrap();
        assert_eq!(completed.plan, plan());
        assert!(
            completed_plan(&cleaned, "stale-response")
                .unwrap()
                .is_none()
        );
        assert!(
            SessionEnd::from_context(SessionIntent::Continue, &json!({"wm_instance_id":"wm"}))
                .is_err()
        );
    }
    #[test]
    fn real_submit_has_no_implicit_default_for_missing_or_invalid_intent() {
        let mut missing = arguments();
        missing.as_object_mut().unwrap().remove("session_intent");
        assert_eq!(
            decode_event(&done(&missing)).unwrap_err().to_string(),
            "missing_session_intent"
        );
        for value in [
            json!(null),
            json!(false),
            json!("cancel"),
            json!({"action":"stop_listening"}),
        ] {
            let mut args = arguments();
            args["session_intent"] = value;
            assert_eq!(
                decode_event(&done(&args)).unwrap_err().to_string(),
                "invalid_session_intent"
            );
        }
        assert_eq!(SessionIntent::default(), SessionIntent::Continue);
    }
    #[test]
    fn deltas_incomplete_and_cancelled_responses_cannot_supply_control() {
        for event in [
            json!({"type":"response.function_call_arguments.done","name":"eva_submit_plan","arguments":arguments().to_string()}),
            {
                let mut event = done(&arguments());
                event["response"]["status"] = json!("cancelled");
                event
            },
            {
                let mut event = done(&arguments());
                event["response"]["status"] = json!("incomplete");
                event
            },
        ] {
            let (cleaned, intent) = decode_event(&event).unwrap();
            assert_eq!(intent, SessionIntent::Continue);
            assert_eq!(cleaned, event);
        }
        let mut incomplete_item = done(&arguments());
        incomplete_item["response"]["output"][0]["status"] = json!("in_progress");
        assert!(decode_event(&incomplete_item).is_err());
    }
    #[test]
    fn duplicate_or_unknown_items_are_rejected_and_prose_never_supplies_intent() {
        let mut event = done(&arguments());
        let item = event["response"]["output"][0].clone();
        event["response"]["output"]
            .as_array_mut()
            .unwrap()
            .push(item);
        assert!(decode_event(&event).is_err());
        let mut event = done(&arguments());
        event["response"]["output"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"unknown"}));
        assert!(decode_event(&event).is_err());
        let mut event = done(&arguments());
        event["response"]["output"].as_array_mut().unwrap().insert(0, json!({"type":"message","session_intent":"stop_listening","content":[{"type":"output_text","text":"stop listening"}]}));
        assert_eq!(decode_event(&event).unwrap().1, SessionIntent::Standby);
    }
    #[test]
    fn decoder_strips_only_its_field_and_core_still_rejects_bogus_authority() {
        let mut args = arguments();
        args["wm_instance_id"] = json!("model-chosen-wm");
        let (cleaned, _) = decode_event(&done(&args)).unwrap();
        assert!(completed_plan(&cleaned, "response-1").is_err());
        let mut args = arguments();
        args["task_intent"] = json!("pause");
        let (cleaned, _) = decode_event(&done(&args)).unwrap();
        let remaining: Value = serde_json::from_str(
            cleaned["response"]["output"][0]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(remaining["task_intent"], "pause");
        assert!(remaining.get("session_intent").is_none());
    }
    #[test]
    fn ending_requires_pure_noop_and_compatible_task_intent() {
        for intent in [SessionIntent::Standby, SessionIntent::StopListening] {
            for task in [None, Some("pause"), Some("cancel"), Some("new")] {
                assert!(validate(intent, &plan(), task).is_ok());
            }
            for task in [
                Some("continue"),
                Some("revise"),
                Some("clarify"),
                Some("shell"),
            ] {
                assert!(validate(intent, &plan(), task).is_err());
            }
            let mut execute = plan();
            execute.disposition = Disposition::Execute;
            assert!(validate(intent, &execute, None).is_err());
            let mut with_action = plan();
            with_action.actions.push(jwm_eva_core::protocol::Action {
                name: "stop_listening".into(),
                arguments: json!({}),
            });
            assert!(validate(intent, &with_action, None).is_err());
            let mut question = plan();
            question.question = Some("确定吗？".into());
            assert!(validate(intent, &question, None).is_err());
        }
        assert!(validate(SessionIntent::Continue, &plan(), Some("pause")).is_ok());
    }
    #[test]
    fn finish_requests_use_only_locally_frozen_source_and_typed_permit() {
        let permit = AssistantPermit {
            assistant_session_id: "wm-1-session-2".into(),
            generation: 2,
        };
        let source = json!({"wm_instance_id":"wm-1","wake_generation":7,"action":"arbitrary","disable_wake":false,"session_intent":"standby"});
        let standby = SessionEnd::from_context(SessionIntent::Standby, &source)
            .unwrap()
            .finish_request(&permit)
            .unwrap();
        assert_eq!(
            standby,
            json!({"action":"finish_session","wm_instance_id":"wm-1","assistant_permit":permit,"disable_wake":false})
        );
        let stop = SessionEnd::from_context(SessionIntent::StopListening, &source)
            .unwrap()
            .finish_request(&permit)
            .unwrap();
        assert_eq!(stop["disable_wake"], true);
        assert_eq!(stop["wake_generation"], 7);
        assert!(stop.get("session_intent").is_none());
        assert_eq!(
            serde_json::from_value::<AssistantPermit>(stop["assistant_permit"].clone()).unwrap(),
            permit
        );
    }
    #[test]
    fn source_fields_are_bounded_and_stop_requires_a_wake_generation() {
        for id in ["".to_owned(), "x".repeat(257), "wm\nforged".into()] {
            assert!(
                SessionEnd::from_context(SessionIntent::Standby, &json!({"wm_instance_id":id}))
                    .is_err()
            );
        }
        assert!(
            SessionEnd::from_context(
                SessionIntent::StopListening,
                &json!({"wm_instance_id":"wm"})
            )
            .is_err()
        );
        assert!(
            SessionEnd::from_context(SessionIntent::Standby, &json!({"wm_instance_id":"wm"}))
                .is_ok()
        );
        let source =
            SessionEnd::from_context(SessionIntent::Standby, &json!({"wm_instance_id":"wm"}))
                .unwrap();
        let invalid = AssistantPermit {
            assistant_session_id: "bad permit".into(),
            generation: 1,
        };
        assert!(source.finish_request(&invalid).is_err());
        let mut changed = source.clone();
        changed.intent = SessionIntent::StopListening;
        let permit = AssistantPermit {
            assistant_session_id: "session-1".into(),
            generation: 1,
        };
        assert!(changed.finish_request(&permit).is_err());
        changed.intent = SessionIntent::Continue;
        assert!(changed.finish_request(&permit).is_err());
    }
}
