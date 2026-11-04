// Offline test-input feasibility and liveness only. Expected values are synthetic
// proposals to exercise contracts, never production intent evidence or a real
// provider acceptance result. This module exists only under session::tests.
use super::*;
use std::{collections::BTreeMap, path::PathBuf};
fn executable_sha256() -> String {
    use sha2::Digest;
    sha2::Sha256::digest(std::fs::read("/proc/self/exe").unwrap()).iter().map(|b|format!("{b:02x}")).collect()
}


fn proposal(case: &Value, context: &Value) -> Result<(Value, &'static str), &'static str> {
    let expected = &case["expected"];
    if let Some(actions) = expected.get("actions") {
        return Ok((actions.clone(), "exact_expected_test_input"));
    }
    // Explicit test-only proposals for intent-only rubric rows. No path or
    // configuration change is invented when the original request omits it.
    let actions = match case["id"].as_str().unwrap() {
        "eva-078" | "eva-081" | "eva-086" => json!([{"name":"query_state","arguments":{"query":"get_monitors"}}]),
        "eva-079" => json!([{"name":"query_state","arguments":{"query":"get_layout"}}]),
        "eva-080" => json!([{"name":"query_state","arguments":{"query":"get_windows"}}]),
        "eva-082" => json!([{"name":"query_state","arguments":{"query":"get_mic_mute"}}]),
        "eva-083" => json!([{"name":"query_state","arguments":{"query":"get_media_status"}}]),
        "eva-084" => json!([{"name":"query_state","arguments":{"query":"get_power_status"}}]),
        "eva-085" => json!([{"name":"query_state","arguments":{"query":"get_recording_status"}}]),
        "eva-087" => json!([{"name":"query_window","arguments":{"window_ref":"w1"}}]),
        "eva-088" => json!([{"name":"query_state","arguments":{"query":"get_status"}}]),
        "eva-089" => return Err("capability_answer_requires_factual_manual_review_not_an_action"),
        "eva-119" => json!([{"name":"quit","arguments":{}}]),
        "eva-120" => json!([{"name":"restart","arguments":{}}]),
        "eva-121" => json!([{"name":"restore_session","arguments":{}}]),
        "eva-122" => return Err("unsupported_config_overwrite_and_no_concrete_changes"),
        "eva-132" | "eva-134" => return Err("required_nonempty_path_missing_no_product_default_assumed"),
        "eva-131" => json!([{"name":"take_screenshot_fullscreen","arguments":{}}]),
        "eva-133" => json!([{"name":"stop_recording","arguments":{}}]),
        "eva-135" => json!([{"name":"stop_audio_recording","arguments":{}}]),
        "eva-141" => json!([{"name":"undo_last_plan","arguments":{}}]),
        "eva-142" => json!([{"name":"move_window_to_monitor","arguments":{"window_ref":"w1","monitor_ref":"m1"}}]),
        "eva-139" => json!([{"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":3}},{"name":"killclient","arguments":{"window_ref":"w2"}}]),
        "eva-118" | "eva-123" => {
            let all=case["id"]=="eva-123";
            json!(context["windows"].as_array().unwrap().iter().filter(|w| all || (w["monitor_token"]==context["selected_monitor_token"] && w["visible"]==true)).map(|w|json!({"name":"killclient","arguments":{"window_ref":w["window_ref"]}})).collect::<Vec<_>>())
        }
        _ => return Err("intent_only_requires_explicit_manual_test_proposal"),
    };
    Ok((actions,"manual_rubric_test_proposal_not_model_output"))
}

#[test]
#[ignore = "explicit offline matrix artifact; no model, Store, IPC or execution"]
fn all_cases_schema_feasibility() {
    let path=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_MATRIX").expect("explicit new output path required"));
    assert!(path.is_absolute() && !path.exists());
    crate::store::private_dir(path.parent().unwrap()).unwrap();
    let config=Config::load(None).unwrap(); // Configuration only; no api_key/Store.
    let corpus=corpus_runner::corpus();let mut rows=Vec::new();let mut counts=BTreeMap::<String,usize>::new();
    for case in corpus["cases"].as_array().unwrap() {
        let mut row=json!({"id":case["id"],"category":case["category"],"scenario":case["scenario"],"text":case["text"],"synthetic_test_input":true,"genuine_semantic_acceptance":"not_evaluated","session_liveness":"not_run_in_feasibility_stage"});
        match corpus_runner::offline_case_inputs(case) {
            Err(error)=>{row["classification"]=json!("fixture_context_or_history_error");row["error"]=json!(error.to_string());}
            Ok((context,_,_))=>match proposal(case,&context) {
                Err(reason)=>{row["classification"]=json!(reason);}
                Ok((actions,origin))=>{
                    row["proposal_origin"]=json!(origin);row["proposal_actions"]=actions.clone();
                    let disposition=case["expected"]["disposition"].as_str().unwrap_or("execute");
                    let semantic: jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":disposition,"actions":actions,"question":if disposition=="clarify"{json!("离线结构提案：需要明确对象或参数")}else{Value::Null},"reason":null})).unwrap();
                    let eligible=eligible_candidate_specs(&config,&context);
                    let schema=semantic.actions.iter().try_for_each(|a|eligible.iter().find(|s|s.canonical_name==a.name).ok_or_else(||"runtime_or_policy_capability_unavailable".to_string()).and_then(|s|jwm_eva_core::actions::validate_schema(&a.arguments,&s.parameters_schema)));
                    match schema.and_then(|_|validate_plan(&semantic,&context).map_err(|e|e.to_string())) {
                        Ok(frozen)=>{row["classification"]=json!("schema_and_native_target_feasible_only");row["requires_confirmation"]=json!(frozen.requires_confirmation);row["frozen_hash"]=json!(frozen.plan_hash);}
                        Err(error)=>{row["classification"]=json!("schema_or_native_target_contract_rejected_test_proposal");row["error"]=json!(error);}
                    }
                }
            }
        }
        *counts.entry(row["classification"].as_str().unwrap().into()).or_default()+=1;rows.push(row);
    }
    assert_eq!(rows.len(),241);
    let report=json!({"scope":"synthetic offline context/schema/target feasibility; not genuine model semantics or dispatch","genuine_provider_calls":0,"genuine_store_opened":false,"dispatch_calls":0,"source_version":env!("JWM_EVA_SOURCE_REVISION"),"source_fingerprint":corpus_runner::source_fingerprint(),"test_executable_sha256":executable_sha256(),"native_fixture_version":corpus_runner::FIXTURE_VERSION,"counts":counts,"cases":rows});
    std::fs::write(&path,serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).unwrap();
    println!("{}",serde_json::to_string(&report["counts"]).unwrap());
}

mod review_fixture { include!("offline_review_fixture.rs"); }

fn matrix_write(path:&std::path::Path, report:&Value) {
    use std::os::unix::fs::PermissionsExt;
    let temp=path.with_extension("partial-json");
    std::fs::write(&temp,serde_json::to_vec_pretty(report).unwrap()).unwrap();
    std::fs::set_permissions(&temp,std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::rename(temp,path).unwrap();
}

#[test]
#[ignore = "explicit synthetic production-planner matrix, isolated child IPC and temp Store; never paid API"]
fn all_cases_production_planner_liveness() {
    let output=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_LIVENESS_REPORT").expect("explicit new output path required"));
    assert!(output.is_absolute() && !output.exists());
    crate::store::private_dir(output.parent().unwrap()).unwrap();
    let dir=std::env::temp_dir().join(format!("eva-offline-liveness-{}-{}",std::process::id(),crate::context::monotonic_ns().unwrap()));
    crate::store::private_dir(&dir).unwrap();
    let child=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","session::tests::offline_liveness::production_planner_matrix_child","--ignored","--nocapture"])
        .env("JWM_EVA_OFFLINE_CHILD","1").env("JWM_SOCKET",dir.join("wm.sock")).env("JWM_EVA_OFFLINE_LIVENESS_REPORT",&output)
        .env_remove("JWM_EVA_RUN_LIVE_CORPUS").env_remove("JWM_EVA_CORPUS_CHILD").output().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(child.status.success(),"{}{}",String::from_utf8_lossy(&child.stdout),String::from_utf8_lossy(&child.stderr));
    println!("{}",String::from_utf8_lossy(&child.stdout));
}

#[tokio::test]
#[ignore = "launched only by the offline wrapper with a private fixture socket"]
async fn production_planner_matrix_child() {
    assert_eq!(std::env::var("JWM_EVA_OFFLINE_CHILD").as_deref(),Ok("1"));
    let socket=PathBuf::from(std::env::var_os("JWM_SOCKET").unwrap());
    assert!(socket.to_string_lossy().starts_with("/tmp/eva-offline-liveness-"));
    let path=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_LIVENESS_REPORT").unwrap());
    assert!(!path.exists());
    let fixture=corpus_runner::Fixture::offline_start(&socket).unwrap();
    let config=Config::load(None).unwrap();
    let corpus=corpus_runner::corpus();
    let recorded=std::env::var_os("JWM_EVA_OFFLINE_PRIOR_REPORT").map(|p|serde_json::from_slice::<Value>(&std::fs::read(p).unwrap()).unwrap());
    let mut cases=corpus["cases"].as_array().unwrap().iter().collect::<Vec<_>>();
    // Test scheduling only: first exercise cases that genuine v25 could not
    // reach. This never selects or changes a production semantic proposal.
    cases.sort_by_key(|case|recorded.as_ref().is_some_and(|v|v["cases"].get(case["id"].as_str().unwrap()).is_some()));
    let mut report=json!({"scope":"synthetic expected/rubric proposals through actual session::plan, private read-only RPC, task/voice/admission and temp fake-billing Store; not model semantics or native effects","genuine_provider_calls":0,"genuine_store_opened":false,"dispatch_calls":0,"source_version":env!("JWM_EVA_SOURCE_REVISION"),"source_fingerprint":corpus_runner::source_fingerprint(),"test_executable_sha256":executable_sha256(),"native_fixture_version":corpus_runner::FIXTURE_VERSION,"prior_unreached_first":recorded.is_some(),"cases":{},"complete":false});
    matrix_write(&path,&report);
    for (index,case) in cases.into_iter().enumerate() {
        let id=case["id"].as_str().unwrap();
        let (context,input)=fixture.offline_case(case).unwrap();
        let mut row=json!({"id":id,"category":case["category"],"scenario":case["scenario"],"text":case["text"],"synthetic_test_input":true,"genuine_semantics":"not_evaluated","dispatch":"not_evaluated"});
        let (actions,origin)=match proposal(case,&context) {
            Ok(value)=>value,
            Err(reason)=>{row["classification"]=json!(reason);report["cases"][id]=row;matrix_write(&path,&report);continue;}
        };
        row["proposal_origin"]=json!(origin);row["proposal_actions"]=actions.clone();
        let disposition=case["expected"]["disposition"].as_str().unwrap_or("execute");
        let mut args=json!({"session_intent":case["expected"].get("session_intent").cloned().unwrap_or(json!("continue")),"disposition":disposition,"actions":actions,"question":if disposition=="clarify"{json!("离线结构测试：请明确对象或必要参数")}else{Value::Null},"reason":null});
        if disposition=="clarify" && input.task_context(&context,&jwm_eva_core::ledger::Ledger::default()).is_some() {args["task_intent"]=json!("clarify");}
        let semantic: jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":args["disposition"],"actions":args["actions"],"question":args["question"],"reason":null})).unwrap();
        let eligible=eligible_candidate_specs(&config,&context);
        let possible=semantic.actions.iter().try_for_each(|a|eligible.iter().find(|s|s.canonical_name==a.name).ok_or_else(||"runtime_or_policy_capability_unavailable".to_string()).and_then(|s|jwm_eva_core::actions::validate_schema(&a.arguments,&s.parameters_schema))).and_then(|_|validate_plan(&semantic,&context).map_err(|e|e.to_string()));
        let expected_frozen=match possible {
            Ok(value)=>value,
            Err(error)=>{row["classification"]=json!("schema_or_native_target_rejected_test_proposal");row["error"]=json!(error);report["cases"][id]=row;matrix_write(&path,&report);continue;}
        };
        // The review fixture is generated from the STATIC rubric and original
        // input before constructing any runtime provider candidate; it is not
        // a real independent model verdict and cannot certify semantics.
        let variants=review_fixture::review_fixtures(case,&semantic.actions.iter().map(|a|serde_json::to_value(a).unwrap()).collect::<Vec<_>>().into(),&context);
        row["synthetic_review_variants_available"]=json!(variants.len());
        let mut categories=std::collections::BTreeSet::from(["windows".to_string(),"monitors".to_string()]);
        for action in &semantic.actions {categories.insert(action_category(&action.name).into());}
        let categories=categories.into_iter().collect::<Vec<_>>();
        let mut scripts=categories.chunks(4).enumerate().map(|(n,categories)|tool("eva_describe_actions",&format!("offline-discover-{n}"),json!({"categories":categories}))).collect::<Vec<_>>();
        scripts.push(tool("eva_submit_plan","offline-original",args));
        row["scripted_discovery_categories"]=json!(categories);
        let review=variants.first().cloned().unwrap_or_else(||json!({"name":"eva_review_allow","call_id":"offline-empty-review","arguments":{"session_intent":case["expected"].get("session_intent").cloned().unwrap_or(json!("continue")),"native_control_quote":case["text"],"steps":[]},"semantic_review":true}));
        // Both allowed attempts retain the same independently scripted test
        // declarations, with distinct call ids. The actual retry budget stays.
        for retry in 0..2 {let mut r=review.clone();r["call_id"]=json!(format!("offline-review-{retry}"));r["optional_trailing"]=json!(true);scripts.push(r);}
        let (mut ws,server)=replay(scripts).await;
        let data=TestStore::new();assert!(!data.dir.starts_with(std::path::Path::new("/home/ubuntu/.local")));
        let mut actor=trace();actor.input=input;actor.native_snapshot_double=false;actor.rounds=Some(vec![]);
        let outcome=tokio::time::timeout(Duration::from_secs(15),plan(&config,&mut ws,&mut actor,data.store.clone(),&context,&mut PlanAdmission::default(),1,(index+1)as u64)).await;
        let result=match outcome {Ok(value)=>value.map_err(|e|e.to_string()),Err(_)=>Err("offline_watchdog_timeout_not_a_semantic_result".into())};
        assert!(data.store.lock().unwrap().ledger.operations.is_empty(),"offline planner cannot execute");
        assert!(!fixture.had_rejected_request(),"only registered private fixture reads may occur: {id}");
        row["synthetic_usage_records"]=json!(data.store.lock().unwrap().usage.records.len());
        row["native_observation"]=json!(fixture.native_observation());row["expected_native_hash"]=json!(expected_frozen.plan_hash);
        let rounds=actor.rounds.take().unwrap_or_default();
        match result {
            Ok((_,frozen))=>{
                let same=serde_json::to_value(&frozen).unwrap()==serde_json::to_value(&expected_frozen).unwrap();
                let equivalent=rounds.iter().any(|r|r["outcome"]=="verified_already_satisfied_noop" && r["original_candidate_hash"]==expected_frozen.plan_hash);
                row["classification"]=json!(if disposition!="execute"{"nonexecution_shape_only_semantics_not_evaluated"}else if same{"exact_test_proposal_admitted"}else if equivalent{"verified_fixture_state_equivalent_noop"}else{"correct_test_proposal_reduced_or_changed"});
                row["observed"]=serde_json::to_value(&frozen).unwrap();
                if case["expected"]["local_confirmation_required"]==true && frozen.disposition==Disposition::Execute {assert!(frozen.requires_confirmation,"protected proposal cannot bypass confirmation: {id}");}
            }
            Err(error)=>{row["classification"]=json!("test_proposal_pipeline_error");row["error"]=json!(error);}
        }
        row["rounds"]=json!(rounds);
        let log=finish(ws,server).await;
        row["synthetic_response_requests"]=json!(log.iter().filter(|e|e["type"]=="response.create").count());
        report["cases"][id]=row;matrix_write(&path,&report);
    }
    let mut counts=BTreeMap::<String,usize>::new();
    for row in report["cases"].as_object().unwrap().values(){*counts.entry(row["classification"].as_str().unwrap().into()).or_default()+=1;}
    report["counts"]=json!(counts);report["complete"]=json!(true);matrix_write(&path,&report);
    assert_eq!(report["cases"].as_object().unwrap().len(),241);
    println!("{}",report["counts"]);
}

#[test]
#[ignore = "explicit replay of recorded arguments with NEW synthetic transport, never historical nonce authority"]
fn recorded_v25_arguments_replay() {
    let output=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_REPLAY_RESULT").expect("explicit new output required"));
    assert!(output.is_absolute()&&!output.exists());crate::store::private_dir(output.parent().unwrap()).unwrap();
    let dir=std::env::temp_dir().join(format!("eva-offline-replay-{}-{}",std::process::id(),crate::context::monotonic_ns().unwrap()));crate::store::private_dir(&dir).unwrap();
    let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","session::tests::offline_liveness::recorded_arguments_replay_child","--ignored","--nocapture"])
        .env("JWM_EVA_OFFLINE_REPLAY_CHILD","1").env("JWM_SOCKET",dir.join("wm.sock"))
        .env_remove("JWM_EVA_RUN_LIVE_CORPUS").env_remove("JWM_EVA_CORPUS_CHILD").output().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(result.status.success(),"{}{}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
    println!("{}",String::from_utf8_lossy(&result.stdout));
}

#[tokio::test]
#[ignore = "offline parent only; private fixture socket and temp Store"]
async fn recorded_arguments_replay_child() {
    assert_eq!(std::env::var("JWM_EVA_OFFLINE_REPLAY_CHILD").as_deref(),Ok("1"));
    let socket=PathBuf::from(std::env::var_os("JWM_SOCKET").unwrap());assert!(socket.to_string_lossy().starts_with("/tmp/eva-offline-replay-"));
    let output=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_REPLAY_RESULT").unwrap());assert!(!output.exists());
    let fixtures_path=PathBuf::from(std::env::var_os("JWM_EVA_OFFLINE_TRANSCRIPT_FIXTURES").expect("explicit immutable extracted transcript fixture required"));
    let fixtures:Value=serde_json::from_slice(&std::fs::read(fixtures_path).unwrap()).unwrap();
    assert_eq!(fixtures["wire_transport_unavailable"],true);assert_eq!(fixtures["missing_reviews_supplemented"],false);
    let prior_bytes=std::fs::read(fixtures["input_path"].as_str().unwrap()).unwrap();
    let prior:Value=serde_json::from_slice(&prior_bytes).unwrap();
    {use sha2::Digest;let sha=sha2::Sha256::digest(&prior_bytes).iter().map(|b|format!("{b:02x}")).collect::<String>();assert_eq!(sha,fixtures["input_sha256"].as_str().unwrap());}

    let fixture=corpus_runner::Fixture::offline_start(&socket).unwrap();let config=Config::load(None).unwrap();let corpus=corpus_runner::corpus();
    let mut report=json!({"scope":"recorded prior genuine arguments/order replay only where all primary parameters recorded; NEW scripted provider transport/binding/IDs, not historical wire or model acceptance","genuine_provider_calls":0,"genuine_store_opened":false,"dispatch_calls":0,"historical_wire_replayable":false,"input_sha256":fixtures["input_sha256"],"source_fingerprint":corpus_runner::source_fingerprint(),"test_executable_sha256":executable_sha256(),"native_context_scope":"current declared synthetic native fixture, not byte-identical historical native snapshots","native_fixture_version":corpus_runner::FIXTURE_VERSION,"cases":{},"complete":false});matrix_write(&output,&report);
    for (index,case) in corpus["cases"].as_array().unwrap().iter().enumerate() {
        let id=case["id"].as_str().unwrap();let Some(original)=fixtures["cases"].get(id) else{continue;};
        let sequence=original["sequence"].as_array().unwrap();let mut row=json!({"id":id,"original_grade":original["original_grade"],"original_error":original["original_error"],"wire_transport_unavailable":true,"original_arguments_changed":false,"synthetic_new_transport":true,"source_sequence_classification":original["classification"]});
        if sequence.iter().any(|e|e["name"]=="eva_submit_plan"&&e["provenance"]!="raw_arguments_exact") {
            // Missing question/reason/task/focus metadata are not silently
            // supplied and a projection is not mislabeled an original failure.
            row["classification"]=json!("unreplayable_incomplete_original_primary_parameters");row["sequence"]=json!(sequence);report["cases"][id]=row;matrix_write(&output,&report);continue;
        }
        let (context,input)=fixture.offline_case(case).unwrap();
        let scripts=sequence.iter().map(|event|{
            let mut script=json!({"name":event["name"],"call_id":event["call_id"],"arguments":event["arguments"],"semantic_review":event["semantic_review"],"disable_legacy_echo":true,"offline_close_when_exhausted":true,"optional_trailing":true});
            // New test transport metadata is echoed by replay, never copied
            // from historical response IDs/binding digests into authority.
            script["historical_response_id_data_only"]=event["original_response_id"].clone();script
        }).collect::<Vec<_>>();
        if scripts.is_empty(){row["classification"]=json!("unreplayable_no_recorded_model_events");report["cases"][id]=row;matrix_write(&output,&report);continue;}
        let expected_requests=scripts.len();let (mut ws,server)=replay(scripts).await;let data=TestStore::new();
        let mut actor=trace();actor.input=input;actor.native_snapshot_double=false;actor.rounds=Some(vec![]);
        let outcome=tokio::time::timeout(Duration::from_secs(15),plan(&config,&mut ws,&mut actor,data.store.clone(),&context,&mut PlanAdmission::default(),1,(index+1)as u64)).await;
        let result=match outcome{Ok(value)=>value.map_err(|e|e.to_string()),Err(_)=>Err("offline_watchdog_timeout_not_original_result".into())};
        assert!(data.store.lock().unwrap().ledger.operations.is_empty());assert!(!fixture.had_rejected_request(),"no mutation or nonfixture query allowed: {id}");
        row["new_synthetic_rounds"]=json!(actor.rounds.take());row["native_observation"]=json!(fixture.native_observation());
        match result{Ok((_,frozen))=>{row["classification"]=json!("recorded_arguments_replayed_with_new_transport");row["observed"]=serde_json::to_value(frozen).unwrap();row["original_observed"]=prior["cases"][id]["observed"].clone();row["matches_original_observed"]=json!(row["observed"]==prior["cases"][id]["observed"]);},Err(error)=>{row["classification"]=json!("recorded_arguments_replayed_error");row["replayed_error"]=json!(error);}}
        let log=finish(ws,server).await;let requested=log.iter().filter(|event|event["type"]=="response.create").count();row["recorded_responses"]=json!(expected_requests);row["new_synthetic_requests"]=json!(requested);
        if requested>expected_requests {row["classification"]=json!("new_planner_requests_unrecorded_continuation_no_response_invented");}
        report["cases"][id]=row;matrix_write(&output,&report);
    }
    let mut counts=BTreeMap::<String,usize>::new();for row in report["cases"].as_object().unwrap().values(){*counts.entry(row["classification"].as_str().unwrap().into()).or_default()+=1;}
    report["counts"]=json!(counts);report["complete"]=json!(true);matrix_write(&output,&report);assert_eq!(report["cases"].as_object().unwrap().len(),fixtures["cases"].as_object().unwrap().len());println!("{}",report["counts"]);
}
