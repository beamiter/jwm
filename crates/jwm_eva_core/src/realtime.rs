//! The network reader may decode these events but has no execution authority.
use crate::actions::Plan;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct CompletedPlan {
    pub response_id: String,
    pub call_id: String,
    pub plan: Plan,
}

#[derive(Debug, Clone)]
pub struct CompletedToolCall {
    pub response_id: String,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

pub fn completed_tool_call(
    event: &Value,
    expected_response_id: &str,
    allowed_names: &[&str],
) -> Result<Option<CompletedToolCall>, String> {
    if event["type"].as_str() != Some("response.done") {
        return Ok(None);
    }
    let response = &event["response"];
    if response["id"].as_str() != Some(expected_response_id) {
        return Ok(None);
    }
    if response["status"].as_str() != Some("completed") {
        return Err("response_not_completed".into());
    }
    let output = response["output"]
        .as_array()
        .ok_or("missing_response_output")?;
    if output.len() != 1 {
        return Err("expected_exactly_one_tool_call".into());
    }
    let item = &output[0];
    let name = item["name"].as_str().ok_or("missing_tool_name")?;
    if item["type"].as_str() != Some("function_call")
        || !allowed_names.contains(&name)
        || item
            .get("status")
            .is_some_and(|status| status.as_str() != Some("completed"))
    {
        return Err("unexpected_tool_output".into());
    }
    let call_id = item["call_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or("invalid_call_id")?;
    let arguments = item["arguments"]
        .as_str()
        .filter(|args| args.len() <= 128 * 1024)
        .ok_or("invalid_tool_arguments")?;
    Ok(Some(CompletedToolCall {
        response_id: expected_response_id.into(),
        call_id: call_id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }))
}

/// Deltas and argument-done events are never executable. The final response
/// is re-parsed in full after its completion status and correlation are checked.
pub fn completed_plan(
    event: &Value,
    expected_response_id: &str,
) -> Result<Option<CompletedPlan>, String> {
    if event["type"].as_str() != Some("response.done") {
        return Ok(None);
    }
    let response = &event["response"];
    if response["id"].as_str() != Some(expected_response_id) {
        return Ok(None);
    }
    if response["status"].as_str() != Some("completed") {
        return Err("response_not_completed".into());
    }
    let output = response["output"]
        .as_array()
        .ok_or("missing_response_output")?;
    if output.len() != 1 {
        return Err("expected_exactly_one_plan_call".into());
    }
    let item = &output[0];
    if item["type"].as_str() != Some("function_call")
        || item["name"].as_str() != Some("eva_submit_plan")
        || item
            .get("status")
            .is_some_and(|s| s.as_str() != Some("completed"))
    {
        return Err("unexpected_plan_output".into());
    }
    let call_id = item["call_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or("invalid_call_id")?
        .to_owned();
    let arguments = item["arguments"]
        .as_str()
        .filter(|s| s.len() <= 128 * 1024)
        .ok_or("invalid_plan_arguments")?;
    // Check required nullable fields before serde's Option missing-field behavior.
    let value: Value = serde_json::from_str(arguments).map_err(|_| "plan_invalid_json")?;
    let object = value.as_object().ok_or("plan_not_object")?;
    if ["disposition", "actions", "question", "reason"]
        .iter()
        .any(|k| !object.contains_key(*k))
    {
        return Err("missing_plan_field".into());
    }
    let plan: Plan = serde_json::from_str(arguments).map_err(|_| "plan_invalid_structure")?;
    Ok(Some(CompletedPlan {
        response_id: expected_response_id.into(),
        call_id,
        plan,
    }))
}

/// Actor-owned single-turn latch: even another call ID cannot claim twice.
#[derive(Debug, Default)]
pub struct PlanAdmission {
    session_epoch: u64,
    turn_epoch: u64,
    response_id: String,
    claimed: bool,
    used_call_ids: BTreeSet<String>,
}
impl PlanAdmission {
    pub fn begin(&mut self, session_epoch: u64, turn_epoch: u64, response_id: String) {
        // A new local session fences every event of the old session, so its
        // call-ID retention can be released without admitting any old work.
        if self.session_epoch != session_epoch {
            self.used_call_ids.clear();
        }
        let already_claimed =
            self.session_epoch == session_epoch && self.turn_epoch == turn_epoch && self.claimed;
        self.session_epoch = session_epoch;
        self.turn_epoch = turn_epoch;
        self.response_id = response_id;
        self.claimed = already_claimed;
    }
    pub fn claim(
        &mut self,
        session_epoch: u64,
        turn_epoch: u64,
        plan: &CompletedPlan,
    ) -> Result<(), String> {
        if session_epoch != self.session_epoch
            || turn_epoch != self.turn_epoch
            || plan.response_id != self.response_id
        {
            return Err("stale_response".into());
        }
        if self.claimed || self.used_call_ids.contains(&plan.call_id) {
            return Err("plan_already_claimed".into());
        }
        if self.used_call_ids.len() >= 1024 {
            return Err("admission_capacity_exceeded".into());
        }
        self.claimed = true;
        self.used_call_ids.insert(plan.call_id.clone());
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.claimed = true;
        self.response_id.clear();
    }
}

pub fn function_output(call_id: &str, result: &Value) -> Value {
    serde_json::json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":call_id,"output":serde_json::to_string(result).expect("JSON result")}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn done(status: &str) -> Value {
        json!({"type":"response.done","response":{"id":"r1","status":status,"output":[{"type":"function_call","status":"completed","name":"eva_submit_plan","call_id":"c1","arguments":"{\"disposition\":\"noop\",\"actions\":[],\"question\":null,\"reason\":null}"}]}})
    }
    #[test]
    fn arguments_done_and_cancelled_response_never_authorize() {
        assert!(
            completed_plan(
                &json!({"type":"response.function_call_arguments.done","arguments":"{}"}),
                "r1"
            )
            .unwrap()
            .is_none()
        );
        for status in ["cancelled", "failed", "incomplete", "in_progress"] {
            assert!(completed_plan(&done(status), "r1").is_err());
        }
        assert!(
            completed_plan(&done("completed"), "other")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn one_turn_has_one_claim_even_different_call_ids() {
        let mut gate = PlanAdmission::default();
        gate.begin(1, 1, "r1".into());
        let mut plan = completed_plan(&done("completed"), "r1").unwrap().unwrap();
        gate.claim(1, 1, &plan).unwrap();
        plan.call_id = "c2".into();
        assert!(gate.claim(1, 1, &plan).is_err());
        gate.cancel();
        assert!(gate.claim(1, 1, &plan).is_err());
    }
    #[test]
    fn multiple_tool_calls_fail_as_whole_response() {
        let mut event = done("completed");
        let item = event["response"]["output"][0].clone();
        event["response"]["output"]
            .as_array_mut()
            .unwrap()
            .push(item);
        assert!(completed_plan(&event, "r1").is_err());
    }
    #[test]
    fn new_response_cannot_reset_a_claimed_turn() {
        let mut gate = PlanAdmission::default();
        gate.begin(1, 1, "r1".into());
        let mut first = completed_plan(&done("completed"), "r1").unwrap().unwrap();
        gate.claim(1, 1, &first).unwrap();
        gate.begin(1, 1, "r2".into());
        first.response_id = "r2".into();
        first.call_id = "different".into();
        assert!(gate.claim(1, 1, &first).is_err());
    }
}
