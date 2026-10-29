use crate::protocol::{Action, digest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Execute,
    Clarify,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub disposition: Disposition,
    pub actions: Vec<Action>,
    #[serde(deserialize_with = "required_nullable")]
    pub question: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<String>,
}
fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrozenPlan {
    pub plan_hash: String,
    pub wm_instance_id: String,
    pub disposition: Disposition,
    /// Resolved actions contain only server tokens, never ephemeral model refs.
    pub actions: Vec<Action>,
    pub question: Option<String>,
    pub reason: Option<String>,
    pub requires_confirmation: bool,
}

pub fn tag_mask(tag_number: u64, tag_count: u64) -> Result<u32, String> {
    if !(1..=32).contains(&tag_number) || tag_number > tag_count {
        return Err("tag_number_out_of_range".into());
    }
    Ok(1u32 << (tag_number - 1))
}

/// Normalizes JWM's Debug layout labels before comparing actual post-state.
pub fn normalize_layout(label: &str) -> Option<&'static str> {
    let lower = label.to_ascii_lowercase();
    match lower.as_str() {
        "centeredmaster" | "centered_master" => Some("centered_master"),
        "threecol" | "three_col" => Some("three_col"),
        "v_stack" | "vstack" => Some("vstack"),
        "网格" => Some("grid"),
        "滚动" | "滚动布局" => Some("scrolling"),
        value => super::LAYOUTS
            .iter()
            .copied()
            .find(|layout| *layout == value),
    }
}

pub fn validate_plan(plan: &Plan, context: &Value) -> Result<FrozenPlan, String> {
    if plan.actions.len() > super::MAX_ACTIONS_PER_PLAN {
        return Err("too_many_actions".into());
    }
    if plan
        .actions
        .iter()
        .any(|action| action.name == "undo_last_plan")
        && plan.actions.len() != 1
    {
        return Err("undo_must_be_only_action".into());
    }
    for text in [&plan.question, &plan.reason].into_iter().flatten() {
        if text.chars().count() > 160 {
            return Err("plan_text_too_long".into());
        }
    }
    match plan.disposition {
        Disposition::Execute if plan.actions.is_empty() || plan.question.is_some() => {
            return Err("execute_requires_actions_without_question".into());
        }
        Disposition::Clarify
            if !plan.actions.is_empty()
                || plan.question.as_deref().is_none_or(|q| q.trim().is_empty()) =>
        {
            return Err("clarify_requires_question_without_actions".into());
        }
        Disposition::Noop if !plan.actions.is_empty() || plan.question.is_some() => {
            return Err("noop_cannot_contain_actions_or_question".into());
        }
        _ => {}
    }
    if context["session_unlocked"].as_bool() != Some(true) {
        return Err("session_locked".into());
    }
    let wm_instance_id = context["wm_instance_id"]
        .as_str()
        .ok_or("missing_wm_instance_id")?
        .to_owned();
    let tag_count = context["tag_count"].as_u64().ok_or("missing_tag_count")?;
    let specs = super::semantic_actions();
    let mut actions = Vec::new();
    let mut requires_confirmation = false;
    for action in &plan.actions {
        let spec = specs
            .iter()
            .find(|s| s.canonical_name == action.name)
            .ok_or("unsupported_action")?;
        validate_schema(&action.arguments, &spec.parameters_schema)?;
        requires_confirmation |= spec.risk_class == "confirmation";
        let mut arguments = action.arguments.clone();
        let args = arguments
            .as_object_mut()
            .ok_or("arguments_must_be_object")?;
        if let Some(reference) = args.remove("monitor_ref") {
            let reference = reference.as_str().ok_or("invalid_monitor_ref")?;
            let monitor = context["monitors"]
                .as_array()
                .and_then(|xs| {
                    xs.iter()
                        .find(|m| m["monitor_ref"].as_str() == Some(reference))
                })
                .ok_or("target_monitor_gone")?;
            if monitor["locked"].as_bool() == Some(true) {
                return Err("target_monitor_locked".into());
            }
            args.insert("monitor_token".into(), monitor["monitor_token"].clone());
        }
        if let Some(reference) = args.remove("window_ref") {
            let reference = reference.as_str().ok_or("invalid_window_ref")?;
            let window = context["windows"]
                .as_array()
                .and_then(|xs| {
                    xs.iter()
                        .find(|w| w["window_ref"].as_str() == Some(reference))
                })
                .ok_or("target_window_gone")?;
            let token = window["target_token"]
                .as_str()
                .ok_or("missing_window_token")?;
            args.insert("window_token".into(), json!(token));
        }
        if let Some(number) = args.remove("tag_number") {
            args.insert(
                "tag_mask".into(),
                json!(tag_mask(
                    number.as_u64().ok_or("invalid_tag_number")?,
                    tag_count
                )?),
            );
        }
        if let Some(number) = args.remove("tab_number") {
            let index = number
                .as_u64()
                .ok_or("invalid_tab_number")?
                .checked_sub(1)
                .ok_or("invalid_tab_number")?;
            args.insert("tab_index".into(), json!(index));
            let expected = context["monitors"]
                .as_array()
                .and_then(|monitors| {
                    monitors
                        .iter()
                        .find(|m| m["monitor_token"] == args["monitor_token"])
                })
                .and_then(|m| m["tab_tokens"].as_array())
                .and_then(|tabs| tabs.get(index as usize))
                .and_then(Value::as_str)
                .ok_or("target_tab_gone")?;
            args.insert("expected_window_token".into(), json!(expected));
        }
        if let Some(numbers) = args.remove("tag_numbers") {
            let mask = numbers
                .as_array()
                .ok_or("invalid_tag_numbers")?
                .iter()
                .try_fold(0u32, |mask, n| {
                    Ok::<_, String>(
                        mask | tag_mask(n.as_u64().ok_or("invalid_tag_number")?, tag_count)?,
                    )
                })?;
            args.insert("tag_mask".into(), json!(mask));
        }
        if matches!(
            action.name.as_str(),
            "start_recording" | "start_audio_recording" | "open_media"
        ) {
            let path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or("missing_path")?;
            let path = std::path::Path::new(path);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("path_must_be_absolute_without_parent_traversal".into());
            }
        }
        actions.push(Action {
            name: action.name.clone(),
            arguments,
        });
    }
    let hash = digest(
        &json!({"wm_instance_id":wm_instance_id,"disposition":plan.disposition,"actions":actions,"question":plan.question,"reason":plan.reason}),
    );
    Ok(FrozenPlan {
        plan_hash: hash,
        wm_instance_id,
        disposition: plan.disposition,
        actions,
        question: plan.question.clone(),
        reason: plan.reason.clone(),
        requires_confirmation,
    })
}

/// Validates the deliberately small JSON Schema subset generated by the registry.
/// Unknown schema keywords are a programmer error, never an implicit allow rule.
pub fn validate_schema(value: &Value, schema: &Value) -> Result<(), String> {
    if let Some(choices) = schema.get("anyOf").and_then(Value::as_array) {
        return if choices
            .iter()
            .any(|choice| validate_schema(value, choice).is_ok())
        {
            Ok(())
        } else {
            Err("no_schema_alternative_matches".into())
        };
    }
    if let Some(constant) = schema.get("const")
        && value != constant
    {
        return Err("constant_mismatch".into());
    }
    if let Some(choices) = schema.get("enum").and_then(Value::as_array)
        && !choices.contains(value)
    {
        return Err("value_not_in_enum".into());
    }
    match schema["type"].as_str().ok_or("schema_type_missing")? {
        "object" => {
            let object = value.as_object().ok_or("expected_object")?;
            let properties = schema["properties"]
                .as_object()
                .ok_or("schema_properties_missing")?;
            for required in schema["required"]
                .as_array()
                .ok_or("schema_required_missing")?
            {
                if !object.contains_key(required.as_str().ok_or("schema_invalid_required")?) {
                    return Err("missing_required_argument".into());
                }
            }
            for (key, v) in object {
                let property = properties
                    .get(key)
                    .ok_or_else(|| format!("unknown_argument:{key}"))?;
                validate_schema(v, property)?;
            }
        }
        "string" => {
            let len = value.as_str().ok_or("expected_string")?.chars().count() as u64;
            if schema["minLength"].as_u64().is_some_and(|min| len < min)
                || schema["maxLength"].as_u64().is_some_and(|max| len > max)
            {
                return Err("string_length_out_of_range".into());
            }
        }
        "integer" => {
            let n = value.as_i64().ok_or("expected_integer")?;
            if schema["minimum"].as_i64().is_some_and(|min| n < min)
                || schema["maximum"].as_i64().is_some_and(|max| n > max)
            {
                return Err("integer_out_of_range".into());
            }
        }
        "number" => {
            let n = value.as_f64().ok_or("expected_number")?;
            if !n.is_finite()
                || schema["minimum"].as_f64().is_some_and(|min| n < min)
                || schema["maximum"].as_f64().is_some_and(|max| n > max)
            {
                return Err("number_out_of_range".into());
            }
        }
        "null" => {
            if !value.is_null() {
                return Err("expected_null".into());
            }
        }
        "boolean" => {
            if !value.is_boolean() {
                return Err("expected_boolean".into());
            }
        }
        "array" => {
            let items = value.as_array().ok_or("expected_array")?;
            if schema["minItems"]
                .as_u64()
                .is_some_and(|n| items.len() < (n as usize))
                || schema["maxItems"]
                    .as_u64()
                    .is_some_and(|n| items.len() > (n as usize))
            {
                return Err("array_length_out_of_range".into());
            }
            if schema["uniqueItems"].as_bool() == Some(true)
                && items
                    .iter()
                    .enumerate()
                    .any(|(i, item)| items[..i].contains(item))
            {
                return Err("duplicate_array_item".into());
            }
            for item in items {
                validate_schema(item, &schema["items"])?;
            }
        }
        _ => return Err("unsupported_schema_type".into()),
    }
    Ok(())
}

/// Converts the exact model schema to the resolved-token wire schema.
pub fn execution_schema(name: &str) -> Option<Value> {
    let lookup = if name == "set_visible_tags" {
        "switch_tags"
    } else {
        name
    };
    let mut schema = super::semantic_actions()
        .into_iter()
        .find(|s| s.canonical_name == lookup)?
        .parameters_schema;
    let object = schema.as_object_mut()?;
    let mut properties = object.remove("properties")?.as_object()?.clone();
    if name == "launch_application" {
        properties.insert("argv".into(),json!({"type":"array","minItems":1,"maxItems":32,"items":{"type":"string","minLength":1,"maxLength":4096}}));
    }
    if name == "focus_tab" {
        properties.insert(
            "expected_window_token".into(),
            json!({"type":"string","minLength":1,"maxLength":256}),
        );
    }
    for (source, target) in [
        ("monitor_ref", "monitor_token"),
        ("window_ref", "window_token"),
        ("tag_number", "tag_mask"),
        ("tag_numbers", "tag_mask"),
        ("tab_number", "tab_index"),
    ] {
        if properties.remove(source).is_some() {
            properties.insert(
                target.into(),
                if target == "tag_mask" {
                    json!({"type":"integer","minimum":1,"maximum":4294967295u64})
                } else if target == "tab_index" {
                    json!({"type":"integer","minimum":0,"maximum":1023})
                } else {
                    json!({"type":"string","minLength":1,"maxLength":256})
                },
            );
        }
    }
    object.insert(
        "required".into(),
        json!(properties.keys().collect::<Vec<_>>()),
    );
    if name == "move_window_to_monitor" {
        properties.insert(
            "restore_tag_mask".into(),
            json!({"type":"integer","minimum":1,"maximum":4294967295u64}),
        );
    }
    // Local undo attaches the frozen post-state; the model never receives or
    // supplies this field. JWM checks it in the same turn as the compensation.
    properties.insert(
        "expected_state".into(),
        json!({"type":"object","properties":{
        "monitor_token":{"type":"string","minLength":1,"maxLength":256},
        "window_token":{"type":"string","minLength":1,"maxLength":256},
        "focus_token":{"anyOf":[{"type":"string","minLength":1,"maxLength":256},{"type":"null"}]},
        "selected_monitor_token":{"anyOf":[{"type":"string","minLength":1,"maxLength":256},{"type":"null"}]},
        "visible_tag_mask":{"type":"integer","minimum":1,"maximum":4294967295u64},
        "tag_mask":{"type":"integer","minimum":1,"maximum":4294967295u64},
        "layout":{"type":"string","enum":super::LAYOUTS},
        "minimized":{"type":"boolean"},"maximized":{"type":"boolean"},
        "maximized_horz":{"type":"boolean"},"maximized_vert":{"type":"boolean"},
        "floating":{"type":"boolean"},"sticky":{"type":"boolean"},"pip":{"type":"boolean"}
    },"required":[],"additionalProperties":false}),
    );
    object.insert("properties".into(), Value::Object(properties));
    Some(schema)
}
pub fn validate_execution_action(action: &Action) -> Result<(), String> {
    let schema = execution_schema(&action.name).ok_or("unsupported_action")?;
    validate_schema(&action.arguments, &schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> Value {
        json!({"session_unlocked":true,"wm_instance_id":"wm1","tag_count":9,"monitors":[{"monitor_ref":"m1","monitor_token":"mon1","locked":false}],"windows":[{"window_ref":"w1","target_token":"win1"}]})
    }
    #[test]
    fn long_ordered_plans_have_one_shared_bounded_capacity() {
        let action = Action {
            name: "focus_window".into(),
            arguments: json!({"window_ref":"w1"}),
        };
        let mut plan = Plan {
            disposition: Disposition::Execute,
            actions: vec![action; super::super::MAX_ACTIONS_PER_PLAN],
            question: None,
            reason: None,
        };
        let frozen = validate_plan(&plan, &context()).unwrap();
        assert_eq!(frozen.actions.len(), 32);
        assert_eq!(frozen.actions[31].arguments["window_token"], "win1");
        plan.actions.push(plan.actions[0].clone());
        assert_eq!(
            validate_plan(&plan, &context()).unwrap_err(),
            "too_many_actions"
        );
    }
    #[test]
    fn tags_are_human_numbered() {
        assert_eq!(tag_mask(3, 9), Ok(4));
        assert!(tag_mask(0, 9).is_err());
        assert!(tag_mask(10, 9).is_err());
        assert_eq!(tag_mask(32, 32), Ok(1 << 31));
    }
    #[test]
    fn freeze_resolves_instance_tokens_and_rejects_raw_authority() {
        let mut plan: Plan = serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"move_window_to_tag","arguments":{"window_ref":"w1","tag_number":3}}],"question":null,"reason":null})).unwrap();
        let frozen = validate_plan(&plan, &context()).unwrap();
        assert_eq!(
            frozen.actions[0].arguments,
            json!({"window_token":"win1","tag_mask":4})
        );
        plan.actions[0].arguments["window_token"] = json!("attacker");
        assert!(validate_plan(&plan, &context()).is_err());
    }
    #[test]
    fn no_actions_on_clarification_or_noop() {
        for disposition in [Disposition::Clarify, Disposition::Noop] {
            let plan = Plan {
                disposition,
                actions: vec![Action {
                    name: "focus_monitor".into(),
                    arguments: json!({"monitor_ref":"m1"}),
                }],
                question: Some("哪一个？".into()),
                reason: None,
            };
            assert!(validate_plan(&plan, &context()).is_err());
        }
    }
    #[test]
    fn missing_nullable_and_extra_fields_are_not_accepted() {
        assert!(
            serde_json::from_value::<Plan>(json!({"disposition":"noop","actions":[]})).is_err()
        );
        assert!(serde_json::from_value::<Plan>(json!({"disposition":"noop","actions":[],"question":null,"reason":null,"confirmed":true})).is_err());
    }
    #[test]
    fn set_membership_uses_human_tags_and_rejects_duplicates() {
        let mut plan = Plan {
            disposition: Disposition::Execute,
            actions: vec![Action {
                name: "switch_tags".into(),
                arguments: json!({"monitor_ref":"m1","tag_numbers":[1,3,9]}),
            }],
            question: None,
            reason: None,
        };
        let frozen = validate_plan(&plan, &context()).unwrap();
        assert_eq!(frozen.actions[0].arguments["tag_mask"], 261);
        plan.actions[0].arguments["tag_numbers"] = json!([1, 1]);
        assert!(validate_plan(&plan, &context()).is_err());
    }
    #[test]
    fn argv_cannot_be_supplied_by_model_but_is_required_at_execution() {
        let mut plan = Plan {
            disposition: Disposition::Execute,
            actions: vec![Action {
                name: "launch_application".into(),
                arguments: json!({"application_id":"browser"}),
            }],
            question: None,
            reason: None,
        };
        let frozen = validate_plan(&plan, &context()).unwrap();
        assert!(validate_execution_action(&frozen.actions[0]).is_err());
        plan.actions[0].arguments["argv"] = json!(["sh", "-c", "anything"]);
        assert!(validate_plan(&plan, &context()).is_err());
        let mut action = frozen.actions[0].clone();
        action.arguments["argv"] = json!(["browser"]);
        assert!(validate_execution_action(&action).is_ok());
    }
    #[test]
    fn compensation_can_freeze_an_explicit_absent_focus() {
        let mut action = Action {
            name: "focus_monitor".into(),
            arguments: json!({"monitor_token":"m1","expected_state":{"focus_token":null,"selected_monitor_token":"m2"}}),
        };
        assert!(validate_execution_action(&action).is_ok());
        action.arguments["expected_state"]["focus_token"] = json!(3);
        assert!(validate_execution_action(&action).is_err());
        action.arguments["expected_state"]["focus_token"] = json!(false);
        assert!(validate_execution_action(&action).is_err());
    }
    #[test]
    fn compensation_poststate_is_strict_and_not_model_supplied() {
        let mut action = Action {
            name: "set_visible_tags".into(),
            arguments: json!({"monitor_token":"m1","tag_mask":4,"expected_state":{"monitor_token":"m1","visible_tag_mask":8}}),
        };
        assert!(validate_execution_action(&action).is_ok());
        action.arguments["expected_state"]["arbitrary_field"] = json!(true);
        assert!(validate_execution_action(&action).is_err());
        let plan = Plan {
            disposition: Disposition::Execute,
            actions: vec![Action {
                name: "switch_tag".into(),
                arguments: json!({"monitor_ref":"m1","tag_number":3,"expected_state":{}}),
            }],
            question: None,
            reason: None,
        };
        assert!(validate_plan(&plan, &context()).is_err());
    }
}
