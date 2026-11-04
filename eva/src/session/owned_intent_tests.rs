//! Contract tests for independent owned intent evidence. No RPC, model or native
//! dispatch is involved. Mount as a child of owned_intent.rs, not session.rs.
//! Exercises exact payload, identity and freshness against real registry
//! validation; a successful intent proof is never a dispatch permit.
use super::*;
use serde_json::json;

const FRAME: (u64, u64) = (7, 11);
fn captured() -> Value {
    json!({"wm_instance_id":"native-wm","wm_pid":1234,"wm_start_ticks":"9876",
        "connection_epoch":4,"wake_generation":0,"assistant_permit":{"assistant_session_id":"owned-session","generation":1},
        "observed_monotonic_ns":100,"session_unlocked":true,"windows_complete":true,
        "selected_monitor_token":"native-m1","activation_monitor_token":"native-m1","tag_count":9,
        "speech_focus_uncertain":false,"speech_focus_ref":"w1","focus_token":"native-w1",
        "available_actions":["set_layout","switch_tag","setgaps","setnmaster","setmfact","move_window_to_tag"],
        "monitors":[{"monitor_ref":"m1","monitor_token":"native-m1","visible_tag_mask":1,"layout":"tile","locked":false},
                    {"monitor_ref":"m2","monitor_token":"native-m2","visible_tag_mask":2,"layout":"tile","locked":false}],
        "windows":[{"window_ref":"w1","target_token":"native-w1","id":"42","monitor_token":"native-m1","tag_mask":1,"focused":true},
                   {"window_ref":"w2","target_token":"native-w2","id":"43","monitor_token":"native-m1","tag_mask":1,"focused":false}]})
}
fn fresh(captured: &Value) -> Value {
    let mut value = captured.clone();
    value["observed_monotonic_ns"] = json!(200);
    // Native get_execution_context need not repeat actor-owned speech metadata.
    value.as_object_mut().unwrap().remove("speech_focus_ref");
    value
        .as_object_mut()
        .unwrap()
        .remove("speech_focus_uncertain");
    value
}
fn plan(actions: Value, context: &Value) -> (Plan, FrozenPlan) {
    let semantic: Plan = serde_json::from_value(
        json!({"disposition":"execute","actions":actions,"question":null,"reason":null}),
    )
    .unwrap();
    let frozen = validate_plan(&semantic, context)
        .expect("candidate uses real registry schema and real context resolution");
    (semantic, frozen)
}
fn scoped(name: &str, field: Option<(&str, Value)>, tag: u64) -> Value {
    let mut args = json!({"monitor_ref":"m1","tag_number":tag});
    if let Some((key, value)) = field {
        args[key] = value;
    }
    json!({"name":name,"arguments":args})
}
fn prove(text: &str, plan: &Plan, frozen: &FrozenPlan, old: &Value, new: &Value) -> Proof {
    match assess(Some(text), plan, frozen, old, new, FRAME) {
        Verdict::Proved(proof) => proof,
        other => panic!("expected complete owned match for {text:?}, got {other:?}"),
    }
}
fn rejects(text: &str, plan: &Plan, frozen: &FrozenPlan, old: &Value, new: &Value) {
    assert!(
        matches!(
            assess(Some(text), plan, frozen, old, new, FRAME),
            Verdict::Reject(_)
        ),
        "recognized request must veto instead of falling back: {text:?}"
    );
}
fn move_follow(context: &Value) -> (Plan, FrozenPlan) {
    plan(
        json!([{"name":"move_window_to_tag","arguments":{"window_ref":"w1","tag_number":8}},scoped("switch_tag",None,8)]),
        context,
    )
}

#[test]
fn five_primitives_match_real_frozen_parameters_without_rewriting_any_input() {
    let old = captured();
    let new = fresh(&old);
    for (text, action) in [
        (
            "当前工作区换成网格布局",
            scoped("set_layout", Some(("layout", json!("grid"))), 1),
        ),
        (
            "把窗口间距设为十二像素",
            scoped("setgaps", Some(("value", json!(12))), 1),
        ),
        (
            "主区域放两个窗口",
            scoped("setnmaster", Some(("value", json!(2))), 1),
        ),
        (
            "主区域占六成",
            scoped("setmfact", Some(("value", json!(0.6))), 1),
        ),
        ("切到第五个工作区", scoped("switch_tag", None, 5)),
    ] {
        let (semantic, frozen) = plan(json!([action]), &old);
        let before = json!({"semantic":semantic,"frozen":frozen,"captured":old,"fresh":new});
        let proof = prove(text, &semantic, &frozen, &old, &new);
        assert!(proof.accepts(&semantic, &frozen, FRAME));
        assert!(proof.can_correct_model_domain());
        assert_eq!(
            before,
            json!({"semantic":semantic,"frozen":frozen,"captured":old,"fresh":new})
        );
    }
}

#[test]
fn exact_rational_is_semantic_point_six_not_native_legacy_one_point_six_or_epsilon() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = plan(
        json!([scoped("setmfact", Some(("value", json!(0.6))), 1)]),
        &old,
    );
    assert_eq!(frozen.actions[0].arguments["value"], json!(0.6));
    for text in [
        "主区域占六成",
        "主区比例设为60%",
        "主区比例设为百分之六十",
        "主区比例设为0.6",
    ] {
        assert!(prove(text, &semantic, &frozen, &old, &new).accepts(&semantic, &frozen, FRAME));
    }
    for value in [0.7, 0.6000000000000001, 0.5999999999999999] {
        let (wrong, wrong_frozen) = plan(
            json!([scoped("setmfact", Some(("value", json!(value))), 1)]),
            &old,
        );
        rejects("主区域占六成", &wrong, &wrong_frozen, &old, &new);
    }
    // The core legacy adapter's +1 encoding is not a legal semantic argument.
    let bad: Plan=serde_json::from_value(json!({"disposition":"execute","actions":[scoped("setmfact",Some(("value",json!(1.6))),1)],"question":null,"reason":null})).unwrap();
    assert!(validate_plan(&bad, &old).is_err());
    let proof = prove("主区域占六成", &semantic, &frozen, &old, &new);
    let mut tampered = frozen.clone();
    tampered.actions[0].arguments["value"] = json!(1.6);
    assert!(!proof.accepts(&semantic, &tampered, FRAME));
    rejects("主区域占六成", &semantic, &tampered, &old, &new);
}

#[test]
fn recognized_wrong_domain_tag_and_monitor_are_reject_not_unsupported() {
    let old = captured();
    let new = fresh(&old);
    for action in [
        scoped("setgaps", Some(("value", json!(12))), 1),
        scoped("setmfact", Some(("value", json!(0.6))), 2),
    ] {
        let (semantic, frozen) = plan(json!([action]), &old);
        rejects("主区域占六成", &semantic, &frozen, &old, &new);
    }
    let mut other = scoped("setmfact", Some(("value", json!(0.6))), 1);
    other["arguments"]["monitor_ref"] = json!("m2");
    let (semantic, frozen) = plan(json!([other]), &old);
    rejects("主区域占六成", &semantic, &frozen, &old, &new);
    // Invalid extra arguments are rejected at registry validation, not granted
    // a fake FrozenPlan by the test helper.
    let mut extra = scoped("setgaps", Some(("value", json!(12))), 1);
    extra["arguments"]["window_ref"] = json!("w1");
    let semantic: Plan = serde_json::from_value(
        json!({"disposition":"execute","actions":[extra],"question":null,"reason":null}),
    )
    .unwrap();
    assert!(validate_plan(&semantic, &old).is_err());
}

#[test]
fn two_special_chains_preserve_exact_order_and_current_window_identity() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = plan(
        json!([
            scoped("switch_tag", None, 3),
            scoped("set_layout", Some(("layout", json!("grid"))), 3)
        ]),
        &old,
    );
    let text = "切到第三个 tag，再改成网格";
    let proof = prove(text, &semantic, &frozen, &old, &new);
    assert!(proof.accepts(&semantic, &frozen, FRAME));
    // Independent allow/operation declarations remain pipeline gates. A chain
    // proof alone must not correct a model domain verdict like a single does.
    assert!(!proof.can_correct_model_domain());
    let mut reversed = semantic.clone();
    reversed.actions.reverse();
    let frozen_reversed = validate_plan(&reversed, &old).unwrap();
    rejects(text, &reversed, &frozen_reversed, &old, &new);
    let (semantic, frozen) = move_follow(&old);
    let text = "把这个窗口移到第八个 tag，然后切过去";
    assert!(!prove(text, &semantic, &frozen, &old, &new).can_correct_model_domain());
    let mut wrong = semantic.clone();
    wrong.actions[0].arguments["window_ref"] = json!("w2");
    let wrong_frozen = validate_plan(&wrong, &old).unwrap();
    rejects(text, &wrong, &wrong_frozen, &old, &new);
    let mut extra = semantic.clone();
    extra.actions.push(extra.actions[1].clone());
    let extra_frozen = validate_plan(&extra, &old).unwrap();
    rejects(text, &extra, &extra_frozen, &old, &new);
}

#[test]
fn captured_speech_anchor_is_mandatory_but_fresh_native_snapshot_need_not_repeat_it() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = move_follow(&old);
    let text = "把这个窗口移到第八个 tag，然后切过去";
    assert!(prove(text, &semantic, &frozen, &old, &new).accepts(&semantic, &frozen, FRAME));
    for mode in ["missing_uncertain", "uncertain", "wrong_ref", "missing_ref"] {
        let mut changed = old.clone();
        match mode {
            "missing_uncertain" => {
                changed
                    .as_object_mut()
                    .unwrap()
                    .remove("speech_focus_uncertain");
            }
            "uncertain" => changed["speech_focus_uncertain"] = json!(true),
            "wrong_ref" => changed["speech_focus_ref"] = json!("w2"),
            _ => {
                changed.as_object_mut().unwrap().remove("speech_focus_ref");
            }
        }
        rejects(text, &semantic, &frozen, &changed, &new);
    }
    for field in ["target_token", "id", "monitor_token", "tag_mask"] {
        let mut changed = new.clone();
        changed["windows"][0][field] = match field {
            "tag_mask" => json!(4),
            "monitor_token" => json!("native-m2"),
            _ => json!("replacement"),
        };
        rejects(text, &semantic, &frozen, &old, &changed);
    }
}

#[test]
fn native_identity_lease_capability_lock_or_default_monitor_drift_cannot_be_proved() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = plan(
        json!([scoped("setgaps", Some(("value", json!(12))), 1)]),
        &old,
    );
    let text = "把窗口间距设为十二像素";
    for field in [
        "wm_instance_id",
        "wm_pid",
        "wm_start_ticks",
        "connection_epoch",
        "wake_generation",
        "assistant_permit",
    ] {
        let mut changed = new.clone();
        changed[field] = json!("different_identity");
        rejects(text, &semantic, &frozen, &old, &changed);
        // Required native identity cannot be inferred from matching absence;
        // the optional connection epoch is checked when actually advertised.
        let mut before = old.clone();
        let mut after = new.clone();
        before.as_object_mut().unwrap().remove(field);
        after.as_object_mut().unwrap().remove(field);
        if field == "connection_epoch" {
            // This optional transport field is not part of the actual native
            // get_execution_context contract. Frame/WM/permit gates remain.
            let proof = prove(text, &semantic, &frozen, &before, &after);
            let mut tampered = frozen.clone();
            tampered.requires_confirmation = true;
            assert!(!proof.accepts(&semantic, &tampered, FRAME));
        } else {
            rejects(text, &semantic, &frozen, &before, &after);
        }
    }
    let mut locked = new.clone();
    locked["session_unlocked"] = json!(false);
    rejects(text, &semantic, &frozen, &old, &locked);
    let mut locked = new.clone();
    locked["monitors"][0]["locked"] = json!(true);
    rejects(text, &semantic, &frozen, &old, &locked);
    let mut unavailable = new.clone();
    unavailable["available_actions"] = json!([]);
    rejects(text, &semantic, &frozen, &old, &unavailable);
    let mut missing = new.clone();
    missing.as_object_mut().unwrap().remove("available_actions");
    rejects(text, &semantic, &frozen, &old, &missing);
    // An unrelated new capability grants no authority over a different plan.
    let mut changed = new.clone();
    changed["available_actions"]
        .as_array_mut()
        .unwrap()
        .push(json!("quit"));
    let proof = prove(text, &semantic, &frozen, &old, &changed);
    let mut different = frozen.clone();
    different.actions[0].arguments["value"] = json!(99);
    assert!(!proof.accepts(&semantic, &different, FRAME));
    // Activation is not permission to ignore a contradictory selected monitor.
    let mut both_old = old.clone();
    both_old["selected_monitor_token"] = json!("native-m2");
    let mut both_new = fresh(&both_old);
    both_new["activation_monitor_token"] = json!("native-m1");
    rejects(text, &semantic, &frozen, &both_old, &both_new);
    let mut changed = new.clone();
    changed["selected_monitor_token"] = json!("native-m2");
    rejects(text, &semantic, &frozen, &old, &changed);
}

#[test]
fn identical_observation_time_is_not_post_review_freshness() {
    let old = captured();
    let (semantic, frozen) = plan(
        json!([scoped("setgaps", Some(("value", json!(12))), 1)]),
        &old,
    );
    // An unchanged observation cannot certify a post-review read.
    rejects(
        "把窗口间距设为十二像素",
        &semantic,
        &frozen,
        &old,
        &old.clone(),
    );
    let mut regressed = fresh(&old);
    regressed["observed_monotonic_ns"] = json!(99);
    rejects(
        "把窗口间距设为十二像素",
        &semantic,
        &frozen,
        &old,
        &regressed,
    );
}

#[test]
fn proof_binds_entire_frozen_candidate_and_frame_not_only_unchanged_hash() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = plan(
        json!([scoped("setgaps", Some(("value", json!(12))), 1)]),
        &old,
    );
    let proof = prove("把窗口间距设为十二像素", &semantic, &frozen, &old, &new);
    assert!(!proof.accepts(&semantic, &frozen, (FRAME.0, FRAME.1 + 1)));
    assert!(!proof.accepts(&semantic, &frozen, (FRAME.0 + 1, FRAME.1)));
    let mut changed_action = semantic.clone();
    changed_action.actions[0].arguments["value"] = json!(13);
    assert!(!proof.accepts(&changed_action, &frozen, FRAME));
    let mut candidate = semantic.clone();
    candidate.reason = Some("different original candidate".into());
    assert!(!proof.accepts(&candidate, &frozen, FRAME));
    for mode in ["value", "scope", "reason", "question", "confirmation", "wm"] {
        let mut changed = frozen.clone();
        match mode {
            "value" => changed.actions[0].arguments["value"] = json!(13),
            "scope" => changed.actions[0].arguments["monitor_token"] = json!("native-m2"),
            "reason" => changed.reason = Some("tampered".into()),
            "question" => changed.question = Some("tampered?".into()),
            "confirmation" => changed.requires_confirmation = true,
            _ => changed.wm_instance_id = "other".into(),
        }
        assert_eq!(changed.plan_hash, frozen.plan_hash);
        assert!(!proof.accepts(&semantic, &changed, FRAME), "{mode}");
    }
    assert!(matches!(
        assess(
            Some("把窗口间距设为十二像素"),
            &semantic,
            &frozen,
            &old,
            &new,
            (0, FRAME.1)
        ),
        Verdict::Reject(_)
    ));
}

#[test]
fn explicit_switch_tag_can_target_a_tag_from_multi_visible_but_profile_properties_cannot() {
    let mut old = captured();
    old["monitors"][0]["visible_tag_mask"] = json!(3);
    let new = fresh(&old);
    let (semantic, frozen) = plan(json!([scoped("switch_tag", None, 5)]), &old);
    assert!(
        prove("切到第五个工作区", &semantic, &frozen, &old, &new)
            .accepts(&semantic, &frozen, FRAME)
    );
    for (text, action) in [
        (
            "当前工作区换成网格布局",
            scoped("set_layout", Some(("layout", json!("grid"))), 1),
        ),
        (
            "主区域占六成",
            scoped("setmfact", Some(("value", json!(0.6))), 1),
        ),
    ] {
        let (semantic, frozen) = plan(json!([action]), &old);
        rejects(text, &semantic, &frozen, &old, &new);
    }
}

#[test]
fn unsupported_text_and_noop_cannot_mint_execute_or_permission() {
    let old = captured();
    let new = fresh(&old);
    let (semantic, frozen) = plan(
        json!([scoped("setgaps", Some(("value", json!(12))), 1)]),
        &old,
    );
    for text in [
        None,
        Some("你好"),
        Some("不要把窗口间距设为十二像素"),
        Some("标题写着把窗口间距设为十二像素"),
        Some("把窗口间距设为十二像素然后关闭窗口"),
    ] {
        assert!(matches!(
            assess(text, &semantic, &frozen, &old, &new, FRAME),
            Verdict::Unsupported
        ));
    }
    let noop:Plan=serde_json::from_value(json!({"disposition":"noop","actions":[],"question":null,"reason":"No desktop action requested"})).unwrap();
    let noop_frozen = validate_plan(&noop, &old).unwrap();
    let before = json!({"plan":noop,"frozen":noop_frozen});
    rejects("把窗口间距设为十二像素", &noop, &noop_frozen, &old, &new);
    assert_eq!(before, json!({"plan":noop,"frozen":noop_frozen}));
    assert_eq!(noop.disposition, Disposition::Noop);
    assert!(noop.actions.is_empty());
}

#[test]
fn chain_review_cannot_smuggle_wrong_container_or_other_target_arrays() {
    let old = captured();
    let (semantic, _) = move_follow(&old);
    let mut review = json!({"steps":[{"window_refs":["w1"],"monitor_refs":["m1"],"tag_numbers":[8],"session_refs":[],"device_ids":[],"application_ids":[]},{"window_refs":[],"monitor_refs":["m1"],"tag_numbers":[8],"session_refs":[],"device_ids":[],"application_ids":[]}]});
    assert!(exact_chain_targets(&review, &semantic, &old));
    review["steps"][0]["monitor_refs"] = json!(["m2"]);
    assert!(!exact_chain_targets(&review, &semantic, &old));
    review["steps"][0]["monitor_refs"] = json!([]);
    assert!(exact_chain_targets(&review, &semantic, &old));
    review["steps"][1]["window_refs"] = json!(["w2"]);
    assert!(!exact_chain_targets(&review, &semantic, &old));
}

#[test]
fn real_native_wire_ticks_are_strings_and_permit_generation_is_independent_of_wake() {
    let old = captured();
    let new = fresh(&old);
    assert_eq!(old["wake_generation"], 0);
    assert_eq!(old["assistant_permit"]["generation"], 1);
    let (semantic, frozen) = plan(
        json!([scoped("setmfact", Some(("value", json!(0.6))), 1)]),
        &old,
    );
    prove("主区域占六成", &semantic, &frozen, &old, &new);
    for invalid in [
        json!(9876),
        Value::Null,
        json!("0"),
        json!("09876"),
        json!("-1"),
        json!("+1"),
        json!("18446744073709551616"),
        json!("一百"),
    ] {
        let mut before = old.clone();
        let mut after = new.clone();
        before["wm_start_ticks"] = invalid.clone();
        after["wm_start_ticks"] = invalid;
        rejects("主区域占六成", &semantic, &frozen, &before, &after);
    }
}

#[test]
#[ignore = "requires two actual private native context RPC envelopes and runtime schema"]
fn actual_private_native_wire_shape_and_independent_generations_are_compatible() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("JWM_EVA_ACTUAL_PRIVATE_CONTEXT_DIR")
            .expect("explicit actual native artifacts required"),
    );
    let read = |name: &str| -> Value {
        let envelope: Value =
            serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap();
        assert_eq!(envelope["success"], true);
        envelope["data"].clone()
    };
    let mut old = read("context-1.raw.json");
    let mut new = read("context-2.raw.json");
    let schema = read("get_command_schema.raw.json");
    jwm_eva_core::protocol::validate_assistant_features(
        &schema,
        jwm_eva_core::protocol::ASSISTANT_EXECUTION_FEATURES,
    )
    .unwrap();
    let actions = schema["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|spec| spec["support_status"] == "implemented")
        .map(|spec| spec["canonical_name"].clone())
        .collect::<Vec<_>>();
    old["available_actions"] = json!(actions);
    new["available_actions"] = old["available_actions"].clone();
    assert!(old["wm_start_ticks"].is_string());
    assert!(
        new["observed_monotonic_ns"].as_u64().unwrap()
            > old["observed_monotonic_ns"].as_u64().unwrap()
    );
    let permit: jwm_eva_core::protocol::AssistantPermit =
        serde_json::from_value(old["assistant_permit"].clone()).unwrap();
    assert_ne!(
        permit.generation,
        old["wake_generation"].as_u64().unwrap(),
        "actual core counters are independent"
    );
    let p = profile(&old, true).expect("actual native profile DTO supported");
    let (semantic, frozen) = plan(
        json!([{"name":"setmfact","arguments":{"monitor_ref":p.reference,"tag_number":p.active.unwrap(),"value":0.6}}]),
        &old,
    );
    let proof = prove("主区域占六成", &semantic, &frozen, &old, &new);
    assert!(proof.accepts(&semantic, &frozen, FRAME));
    // This proves wire/context compatibility only: no model, Store, dispatch,
    // native mutation or physical capture is performed by this regression.
}
