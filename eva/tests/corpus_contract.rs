//! Validate acceptance fixtures against the production action registry.
//!
//! These checks exercise schema and reference binding only. They do not send
//! utterances to a model, recognize audio, execute desktop actions, or establish
//! the semantic acceptance rate documented in corpus.json.
use jwm_eva_core::{
    actions::{Disposition, Plan, semantic_actions, validate_plan, validate_schema},
    protocol::Action,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn corpus() -> Value {
    serde_json::from_str(include_str!("corpus.json")).expect("valid semantic acceptance corpus")
}

fn scenario(corpus: &Value, name: &str, visiting: &mut BTreeSet<String>) -> Value {
    assert!(
        visiting.insert(name.into()),
        "cyclic scenario inheritance: {name}"
    );
    let source = corpus["scenarios"][name]
        .as_object()
        .unwrap_or_else(|| panic!("unknown scenario: {name}"));
    let mut resolved = if let Some(parent) = source.get("inherits").and_then(Value::as_str) {
        scenario(corpus, parent, visiting)
    } else {
        json!({})
    };
    for (key, value) in source {
        if key != "inherits" {
            resolved[key] = value.clone();
        }
    }
    visiting.remove(name);
    resolved
}

fn binding_context(scenario: &Value) -> Value {
    let monitors = scenario["monitors"]
        .as_array()
        .expect("fixture monitor list")
        .iter()
        .filter(|monitor| monitor["ref"] != scenario["removed_monitor"])
        .map(|monitor| {
            let reference = monitor["ref"].as_str().expect("fixture monitor reference");
            json!({
                "monitor_ref": reference,
                "monitor_token": format!("fixture-monitor:{reference}"),
                "locked": false,
            })
        })
        .collect::<Vec<_>>();
    let windows = scenario["windows"]
        .as_array()
        .expect("fixture window list")
        .iter()
        .filter(|window| window["ref"] != scenario["removed_ref"])
        .map(|window| {
            let reference = window["ref"].as_str().expect("fixture window reference");
            json!({
                "window_ref": reference,
                "target_token": format!("fixture-window:{reference}"),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "wm_instance_id": "fixture-wm",
        "session_unlocked": true,
        "tag_count": scenario["tag_count"],
        "monitors": monitors,
        "windows": windows,
    })
}

#[test]
fn acceptance_case_ids_scenarios_and_dispositions_are_well_formed() {
    let corpus = corpus();
    assert_eq!(corpus["version"], 1);
    let cases = corpus["cases"].as_array().expect("corpus cases");
    assert!(!cases.is_empty());
    let mut ids = BTreeSet::new();
    for case in cases {
        let id = case["id"].as_str().expect("case id");
        assert!(
            !id.is_empty() && ids.insert(id),
            "duplicate or empty case id: {id}"
        );
        assert!(
            case["text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
            "{id}: missing utterance"
        );
        let name = case["scenario"].as_str().expect("scenario name");
        scenario(&corpus, name, &mut BTreeSet::new());
        let expected = &case["expected"];
        assert!(
            expected["intent"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
            "{id}: missing intent"
        );
        match expected["disposition"]
            .as_str()
            .expect("expected disposition")
        {
            "execute" => {
                if let Some(actions) = expected["actions"].as_array() {
                    assert!(
                        (1..=8).contains(&actions.len()),
                        "{id}: invalid exact plan length"
                    );
                }
            }
            "noop" | "clarify" => {
                assert!(
                    expected["actions"].as_array().is_some_and(Vec::is_empty),
                    "{id}: non-executing expectation contains actions"
                );
            }
            other => panic!("{id}: unknown disposition {other}"),
        }
    }
}

#[test]
fn exact_acceptance_plans_match_live_schemas_references_and_confirmation_policy() {
    let corpus = corpus();
    let specs = semantic_actions();
    for case in corpus["cases"].as_array().expect("corpus cases") {
        let id = case["id"].as_str().expect("case id");
        let expected = &case["expected"];
        if expected["disposition"] != "execute" || !expected["actions"].is_array() {
            // Intent-only cases still require real-model/manual acceptance.
            continue;
        }
        let actions: Vec<Action> = serde_json::from_value(expected["actions"].clone())
            .unwrap_or_else(|error| panic!("{id}: invalid action fixture: {error}"));
        for action in &actions {
            let spec = specs
                .iter()
                .find(|spec| spec.canonical_name == action.name)
                .unwrap_or_else(|| panic!("{id}: unsupported expected action {}", action.name));
            validate_schema(&action.arguments, &spec.parameters_schema)
                .unwrap_or_else(|error| panic!("{id}: {} fixture schema: {error}", action.name));
        }
        let name = case["scenario"].as_str().expect("scenario name");
        let resolved = scenario(&corpus, name, &mut BTreeSet::new());
        let plan = Plan {
            disposition: Disposition::Execute,
            actions,
            question: None,
            reason: None,
        };
        let frozen = validate_plan(&plan, &binding_context(&resolved))
            .unwrap_or_else(|error| panic!("{id}: expected plan cannot bind: {error}"));
        assert_eq!(
            frozen.requires_confirmation,
            expected["local_confirmation_required"]
                .as_bool()
                .unwrap_or(false),
            "{id}: expected local confirmation differs from production policy",
        );
    }
}
