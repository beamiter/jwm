//! Pure lazy-schema completion. `eligible` MUST come from the caller's current
//! runtime/config-policy-filtered ActionSpecs, exactly as bounded discovery does.
//! This grants no operation, target, application registration or confirmation.
use jwm_eva_core::actions::{ActionSpec, Plan};
use std::collections::BTreeSet;

/// Append only missing schemas named by the unchanged original candidate.
/// Failure is transactional: a mixed allowed/unknown candidate cannot leave a
/// prefix loaded. Existing narrower selected schemas are never replaced.
pub(super) fn ensure_selected(
    plan: &Plan,
    selected: &mut Vec<ActionSpec>,
    eligible: &[ActionSpec],
) -> Result<Vec<String>, &'static str> {
    let mut pending = Vec::new();
    let mut seen = BTreeSet::new();
    for action in &plan.actions {
        let mut allowed = eligible
            .iter()
            .filter(|spec| spec.canonical_name == action.name);
        let spec = allowed.next().ok_or("action_schema_not_loaded")?;
        if allowed.next().is_some() {
            return Err("action_schema_not_loaded");
        }
        let mut existing = selected
            .iter()
            .filter(|spec| spec.canonical_name == action.name);
        let loaded = existing.next().is_some();
        if existing.next().is_some() {
            return Err("action_schema_not_loaded");
        }
        if !loaded && seen.insert(action.name.as_str()) {
            pending.push(spec.clone());
        }
    }
    let names = pending
        .iter()
        .map(|spec| spec.canonical_name.clone())
        .collect();
    selected.extend(pending);
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_eva_core::actions::{semantic_actions, validate_plan, validate_schema};
    use serde_json::{Value, json};
    fn spec(name: &str) -> ActionSpec {
        semantic_actions()
            .into_iter()
            .find(|spec| spec.canonical_name == name)
            .expect("real canonical registry name")
    }
    fn plan(actions: Value) -> Plan {
        serde_json::from_value(
            json!({"disposition":"execute","actions":actions,"question":null,"reason":null}),
        )
        .unwrap()
    }
    fn context() -> Value {
        json!({"wm_instance_id":"wm","session_unlocked":true,"tag_count":9,"monitors":[],"windows":[]})
    }
    #[test]
    fn repaired135_known_missing_schema_is_loaded_without_candidate_or_other_category_changes() {
        let original = plan(json!([{"name":"stop_audio_recording","arguments":{}}]));
        let before = json!(original);
        let mut selected = vec![spec("switch_tag")];
        let eligible = vec![
            spec("switch_tag"),
            spec("stop_audio_recording"),
            spec("start_audio_recording"),
        ];
        assert_eq!(
            ensure_selected(&original, &mut selected, &eligible),
            Ok(vec!["stop_audio_recording".into()])
        );
        assert_eq!(json!(original), before);
        assert_eq!(
            selected
                .iter()
                .map(|s| s.canonical_name.as_str())
                .collect::<Vec<_>>(),
            vec!["switch_tag", "stop_audio_recording"]
        );
        assert_eq!(json!(selected[1]), json!(eligible[1]));
        assert!(
            validate_schema(
                &original.actions[0].arguments,
                &selected[1].parameters_schema
            )
            .is_ok()
        );
        let frozen = validate_plan(&original, &context()).unwrap();
        assert_eq!(frozen.actions[0].name, "stop_audio_recording");
        assert!(!frozen.requires_confirmation);
        assert_eq!(
            ensure_selected(&original, &mut selected, &eligible),
            Ok(vec![])
        );
    }
    #[test]
    fn existing_narrower_schema_is_not_upgraded_from_eligible_registry() {
        let mut narrow = spec("query_state");
        narrow.parameters_schema["properties"]["query"]["enum"] = json!(["get_version"]);
        let before = json!(narrow);
        let original = plan(json!([{"name":"query_state","arguments":{"query":"get_windows"}}]));
        let mut selected = vec![narrow];
        assert_eq!(
            ensure_selected(&original, &mut selected, &[spec("query_state")]),
            Ok(vec![])
        );
        assert_eq!(json!(selected[0]), before);
        assert!(
            validate_schema(
                &original.actions[0].arguments,
                &selected[0].parameters_schema
            )
            .is_err()
        );
    }
    #[test]
    fn unknown_alias_unavailable_or_mixed_candidates_fail_without_partial_loading() {
        let eligible = vec![spec("stop_audio_recording")];
        for names in [
            vec!["invented_action"],
            vec!["minimize"],
            vec!["start_audio_recording"],
            vec!["stop_audio_recording", "invented_action"],
            vec!["stop_audio_recording", "start_audio_recording"],
        ] {
            let actions = names
                .iter()
                .map(|name| json!({"name":name,"arguments":{}}))
                .collect::<Vec<_>>();
            let original = plan(json!(actions));
            let before = json!(original);
            let mut selected = vec![spec("switch_tag")];
            let old = json!(selected);
            assert_eq!(
                ensure_selected(&original, &mut selected, &eligible),
                Err("action_schema_not_loaded")
            );
            assert_eq!(json!(selected), old);
            assert_eq!(json!(original), before);
        }
        // Existing selection is not evidence that current runtime/policy allows
        // an action: even that name must still be in caller-supplied eligible.
        let original = plan(json!([{"name":"stop_audio_recording","arguments":{}}]));
        let mut selected = vec![spec("stop_audio_recording")];
        let old = json!(selected);
        assert_eq!(
            ensure_selected(&original, &mut selected, &[]),
            Err("action_schema_not_loaded")
        );
        assert_eq!(json!(selected), old);
    }
    #[test]
    fn newly_loaded_schema_still_rejects_missing_wrong_type_and_untrusted_extra_arguments() {
        for args in [
            json!({}),
            json!({"path":12}),
            json!({"path":"/owned/test.wav","confirmed":true}),
        ] {
            let original = plan(json!([{"name":"start_audio_recording","arguments":args}]));
            let mut selected = vec![];
            assert_eq!(
                ensure_selected(&original, &mut selected, &[spec("start_audio_recording")]),
                Ok(vec!["start_audio_recording".into()])
            );
            assert!(
                validate_schema(
                    &original.actions[0].arguments,
                    &selected[0].parameters_schema
                )
                .is_err()
            );
            assert!(validate_plan(&original, &context()).is_err());
        }
    }
    #[test]
    fn protected_risk_and_native_target_lock_validation_remain_independent() {
        let original = plan(json!([{"name":"quit","arguments":{}}]));
        let mut selected = vec![];
        ensure_selected(&original, &mut selected, &[spec("quit")]).unwrap();
        assert_eq!(selected[0].risk_class, "confirmation");
        assert!(
            validate_plan(&original, &context())
                .unwrap()
                .requires_confirmation
        );
        // No local confirmation is generated here. Guarded execution still
        // needs the original trusted local token; this pure helper never runs it.
        let original = plan(
            json!([{"name":"move_window_to_tag","arguments":{"window_ref":"w1","tag_number":2}}]),
        );
        let mut selected = vec![];
        ensure_selected(&original, &mut selected, &[spec("move_window_to_tag")]).unwrap();
        assert!(
            validate_schema(
                &original.actions[0].arguments,
                &selected[0].parameters_schema
            )
            .is_ok()
        );
        assert!(
            validate_plan(&original, &context())
                .unwrap_err()
                .contains("target_window_gone")
        );
        let mut locked = context();
        locked["session_unlocked"] = json!(false);
        assert_eq!(
            validate_plan(&original, &locked).unwrap_err(),
            "session_locked"
        );
    }
    #[test]
    fn repeated_names_load_once_and_ambiguous_spec_sources_fail_closed() {
        let original = plan(
            json!([{"name":"stop_audio_recording","arguments":{}},{"name":"stop_audio_recording","arguments":{}}]),
        );
        let mut selected = vec![];
        assert_eq!(
            ensure_selected(&original, &mut selected, &[spec("stop_audio_recording")]),
            Ok(vec!["stop_audio_recording".into()])
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(original.actions.len(), 2);
        let mut selected = vec![];
        assert_eq!(
            ensure_selected(
                &original,
                &mut selected,
                &[spec("stop_audio_recording"), spec("stop_audio_recording")]
            ),
            Err("action_schema_not_loaded")
        );
        assert!(selected.is_empty());
    }
}
