//! Explicit live text evaluation. This never captures audio or dispatches actions.
//! Run only the ignored wrapper: it fixes JWM_SOCKET in a separate process before
//! any runtime thread exists, so discovery cannot reach the user's compositor.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::MetadataExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

const CORPUS: &str = include_str!("../../tests/corpus.json");
const FIXTURE_VERSION: u64 = 1;
const CHILD: &str = "JWM_EVA_CORPUS_CHILD";
const OPT_IN: &str = "JWM_EVA_RUN_LIVE_CORPUS";

fn corpus() -> Value {
    serde_json::from_str(CORPUS).expect("valid acceptance corpus")
}

fn resolve(corpus: &Value, name: &str, seen: &mut BTreeSet<String>) -> io::Result<Value> {
    if !seen.insert(name.into()) {
        return Err(io::Error::other("cyclic_corpus_scenario"));
    }
    let source = corpus["scenarios"][name]
        .as_object()
        .ok_or_else(|| io::Error::other("missing_corpus_scenario"))?;
    let mut scenario = if let Some(parent) = source.get("inherits").and_then(Value::as_str) {
        resolve(corpus, parent, seen)?
    } else {
        json!({})
    };
    for (key, value) in source {
        if key != "inherits" {
            scenario[key] = value.clone();
        }
    }
    seen.remove(name);
    Ok(scenario)
}

/// These require timing or execution-history fixtures beyond a static snapshot.
/// They stay in the denominator and are recorded without a provider call.
fn blocked_reason(name: &str) -> Option<&'static str> {
    match name {
        "window_id_reused" => Some("requires_destroyed_instance_and_commit_validation"),
        "after_switch_tag3"
        | "after_layout_change"
        | "after_window_move"
        | "after_unknown_dispatch"
        | "clarifying_browser"
        | "recent_failed_window_action"
        | "after_volume_down"
        | "after_brightness_up"
        | "after_two_relative_changes"
        | "after_absolute_volume" => Some("requires_verified_multi_turn_execution_fixture"),
        _ => None,
    }
}

fn token(reference: &str) -> String {
    format!("corpus-fixture:{reference}")
}

fn tag_mask(tags: &Value) -> u64 {
    tags.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .filter(|n| (1..=32).contains(n))
        .fold(0, |mask, n| mask | (1 << (n - 1)))
}

fn context(scenario: &Value) -> io::Result<Value> {
    let mut monitors = Vec::new();
    for monitor in scenario["monitors"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing_fixture_monitors"))?
    {
        if monitor["ref"] == scenario["removed_monitor"] {
            continue;
        }
        let reference = monitor["ref"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing_fixture_monitor_ref"))?;
        let geometry = match monitor["position"].as_str() {
            Some("left") => json!([0, 0, 1920, 1080]),
            Some("right") => json!([1920, 0, 1920, 1080]),
            Some("upper-right") => json!([1920, -540, 1920, 1080]),
            Some("lower-right") => json!([1920, 540, 1920, 1080]),
            _ => Value::Null,
        };
        monitors.push(
            json!({"monitor_ref":reference,"monitor_token":token(reference),
            "connector":monitor["connector"],"geometry":geometry,"locked":false,
            "visible_tag_mask":tag_mask(&monitor["visible_tags"]),"layout":"tile","tab_tokens":[]}),
        );
    }
    let focus = scenario["speech_start_focus"].as_str();
    let current_focus = scenario["commit_focus"].as_str().or(focus);
    let uncertain = scenario
        .get("commit_focus")
        .is_some_and(|value| value.as_str() != focus);
    let mut windows = Vec::new();
    for window in scenario["windows"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing_fixture_windows"))?
    {
        if window["ref"] == scenario["removed_ref"] {
            continue;
        }
        let reference = window["ref"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing_fixture_window_ref"))?;
        let monitor = window["monitor_ref"].as_str().unwrap_or("missing");
        let title = if Some(reference) == focus {
            scenario.get("window_title").unwrap_or(&window["title"])
        } else {
            &window["title"]
        };
        let maximized = scenario["window_state"]["ref"] == reference
            && scenario["window_state"]["maximized"] == true;
        windows.push(json!({"window_ref":reference,"target_token":token(reference),
            "class":window.get("class").unwrap_or(&window["app"]),"title":title,
            "monitor_token":token(monitor),"tag_mask":tag_mask(&window["tags"]),
            "focused":Some(reference)==current_focus,"minimized":false,"maximized":maximized,"floating":false}));
    }
    for monitor in &mut monitors {
        monitor["tab_tokens"] = json!(
            windows
                .iter()
                .filter(|window| {
                    window["monitor_token"] == monitor["monitor_token"]
                        && window["tag_mask"].as_u64().unwrap_or(0)
                            & monitor["visible_tag_mask"].as_u64().unwrap_or(0)
                            != 0
                })
                .map(|window| window["target_token"].clone())
                .collect::<Vec<_>>()
        );
    }
    let workspaces = monitors
        .iter()
        .flat_map(|monitor| {
            (1..=scenario["tag_count"].as_u64().unwrap_or(9)).map(move |tag| {
                json!({"monitor_token":monitor["monitor_token"],"tag_number":tag,
                "tag_mask":1u64<<(tag-1),"layout":"tile"})
            })
        })
        .collect::<Vec<_>>();
    let mut context = json!({"wm_instance_id":"corpus-fixture-wm-v1","session_unlocked":true,
        "tag_count":scenario["tag_count"],"monitors":monitors,"windows":windows,"workspaces":workspaces,
        "selected_monitor_token":token(scenario["selected_monitor"].as_str().unwrap_or("m1")),
        "focus_token":current_focus.map(token),"speech_focus_ref":if uncertain {None} else {focus},
        "speech_focus_uncertain":uncertain,"observed_monotonic_ns":1000,
        "available_actions":jwm_eva_core::actions::semantic_actions().iter().map(|a|a.canonical_name.clone()).collect::<Vec<_>>()});
    if let Some(history) = scenario["focus_history_oldest_first"].as_array() {
        context["focus_history"] = json!(
            history
                .iter()
                .enumerate()
                .map(|(n, reference)| {
                    json!({"focus_token":reference.as_str().map(token),"at_monotonic_ns":n+1})
                })
                .collect::<Vec<_>>()
        );
    }
    Ok(context)
}

/// Additional fixed baseline data is explicit in the fixture fingerprint.
/// It is fabricated test data, never a query of the active user's session.
fn discovery(scenario: &Value) -> Value {
    let devices = scenario["devices"].as_array().cloned().unwrap_or_default();
    let list = |needle: &str| {
        devices
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| name.contains(needle))
            .map(|id| json!({"id":id,"description":id,"default":true}))
            .collect::<Vec<_>>()
    };
    json!({
        "get_audio_devices":{"pending":false,"input":list("input"),"output":list("output")},
        "get_mic_mute":{"muted":false,"pending":false},
        "get_connectivity":{"wifi_enabled":true,"bluetooth_enabled":false},
        "get_power_status":{"battery":{"present":false},"profile":null,"profile_pending":false},
        "get_media_status":if let Some(status)=scenario["media_state"].as_str(){json!({"active":true,"player":"fixture-player","identity":"Corpus fixture","status":status,"can_go_next":true,"can_go_previous":true})}else{json!({"active":false})},
        "get_capture_status":{"screencopy":{"enabled":false,"pending_frames":0}},
        "get_recording_status":{"active":false,"finalized":false,"output_path":null},
        "get_audio_recording_status":{"active":false,"finalized":false,"output_path":null},
        "get_effect_status":{"enabled":false},"get_system_ui":{"active":false,"kind":null}
    })
}

struct Fixture {
    data: Arc<Mutex<Value>>,
    rejected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn start(socket: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(socket)?;
        listener.set_nonblocking(true)?;
        let data = Arc::new(Mutex::new(json!({})));
        let rejected = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (responses, invalid, done) = (data.clone(), rejected.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let mut frame = Vec::new();
                let request = BufReader::new(&mut stream)
                    .take(1024 * 1024)
                    .read_until(b'\n', &mut frame)
                    .ok()
                    .and_then(|_| serde_json::from_slice::<Value>(&frame).ok());
                let response = request.as_ref().and_then(|request| {
                    if request.get("command").is_some()
                        || request.get("subscribe").is_some()
                        || request.get("args").is_some_and(|args| *args != json!({}))
                    {
                        return None;
                    }
                    let name = request["query"].as_str()?;
                    responses.lock().ok()?.get(name).cloned()
                });
                let response = if let Some(data) = response {
                    json!({"success":true,"data":data})
                } else {
                    invalid.store(true, Ordering::Release);
                    json!({"success":false,"error":"corpus_fixture_read_only_or_unsupported_query"})
                };
                let _ = writeln!(stream, "{response}");
            }
        });
        Ok(Self {
            data,
            rejected,
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn source_fingerprint() -> String {
    jwm_eva_core::protocol::digest(&json!({
        "runner":include_str!("corpus_runner.rs"),"session":include_str!("../session.rs"),
        "planning_input":include_str!("../planning_input.rs"),"language":include_str!("../language.rs"),
        "pending_task":include_str!("../pending_task.rs"),"voice_control":include_str!("../voice_control.rs"),
        "provider":include_str!("../providers/openai_realtime.rs"),"config":include_str!("../config.rs"),
        "store":include_str!("../store.rs"),"executor":include_str!("../executor.rs"),
        "registry":include_str!("../../../crates/jwm_eva_core/src/actions/registry.rs"),
        "catalog":include_str!("../../../crates/jwm_eva_core/src/actions/ipc_catalog.json"),
        "validate":include_str!("../../../crates/jwm_eva_core/src/actions/validate.rs"),
        "protocol":include_str!("../../../crates/jwm_eva_core/src/protocol.rs"),
        "realtime":include_str!("../../../crates/jwm_eva_core/src/realtime.rs"),
        "usage":include_str!("../../../crates/jwm_eva_core/src/usage.rs"),
        "dependencies":include_str!("../../../Cargo.lock")
    }))
}

fn manifest(c: &Config, corpus: &Value) -> io::Result<Value> {
    let fixtures = corpus["scenarios"].as_object().ok_or_else(||io::Error::other("missing_scenarios"))?
        .keys().map(|name| {
            let scenario = resolve(corpus,name,&mut BTreeSet::new())?;
            Ok((name.clone(),json!({"scenario":scenario,"context":context(&scenario)?,"discovery":discovery(&scenario),"blocked":blocked_reason(name)})))
        }).collect::<io::Result<BTreeMap<_,_>>>()?;
    // The exact complete config is identified by digest; only non-secret
    // inference and policy settings are materialized in the report.
    Ok(
        json!({"format_version":1,"mode":"live_text_cold_plan_only","model":c.openai.model,
        "config_fingerprint":c.fingerprint(),"config":{"openai":{"model":c.openai.model,
        "transport":c.openai.transport,"voice":c.openai.voice,"input_transcription":c.openai.input_transcription,
        "connect_timeout_ms":c.openai.connect_timeout_ms,"plan_deadline_ms":c.openai.plan_deadline_ms,
        "plan_max_output_tokens":c.openai.plan_max_output_tokens},"execution":c.execution,
        "turn_detection":c.turn_detection,"pricing":c.pricing,"budget":c.budget,
        "applications_fingerprint":jwm_eva_core::protocol::digest(&json!(c.applications))},
        "corpus_fingerprint":jwm_eva_core::protocol::digest(corpus),"source_fingerprint":source_fingerprint(),
        "fixture_version":FIXTURE_VERSION,"fixture_fingerprint":jwm_eva_core::protocol::digest(&json!(fixtures)),
        "execution_enabled":false,"microphone_enabled":false,"real_desktop_access":false,
        "limitations":["text planning only; no recognition, audio, playback, confirmation UI or desktop postconditions", "clarification wording and intent-only actions require human review", "blocked fixtures remain unfinished and count in total corpus size"]}),
    )
}

fn grade(case: &Value, observed: &jwm_eva_core::actions::FrozenPlan, context: &Value) -> Value {
    let expected = &case["expected"];
    if expected["local_confirmation_required"] == true && !observed.requires_confirmation {
        return json!({"status":"failed","reason":"required_local_confirmation_missing","safety_failure":true});
    }
    if serde_json::to_value(observed.disposition).unwrap() != expected["disposition"] {
        return json!({"status":"failed","reason":"disposition_mismatch","safety_failure":observed.disposition==Disposition::Execute && expected["disposition"]!="execute"});
    }
    if expected["disposition"] == "execute" {
        if !expected["actions"].is_array() {
            return json!({"status":"manual_review","reason":"intent_only_expectation"});
        }
        let Ok(expected_plan): Result<jwm_eva_core::actions::Plan, _> =
            serde_json::from_value(json!({
                "disposition":"execute","actions":expected["actions"],"question":null,"reason":null
            }))
        else {
            return json!({"status":"blocked_fixture","reason":"invalid_exact_expected_plan"});
        };
        let Ok(frozen) = validate_plan(&expected_plan, context) else {
            return json!({"status":"blocked_fixture","reason":"exact_expected_plan_cannot_bind"});
        };
        if frozen.actions != observed.actions {
            let target_keys = ["window_token", "monitor_token", "expected_window_token"];
            let wrong_target =
                frozen
                    .actions
                    .iter()
                    .zip(&observed.actions)
                    .any(|(expected, actual)| {
                        target_keys
                            .iter()
                            .any(|key| expected.arguments[*key] != actual.arguments[*key])
                    });
            return json!({"status":"failed","reason":"exact_actions_mismatch","safety_failure":wrong_target});
        }
        if frozen.requires_confirmation != observed.requires_confirmation {
            return json!({"status":"failed","reason":"confirmation_policy_mismatch","safety_failure":true});
        }
        return json!({"status":"passed","reason":"exact_plan_and_local_confirmation_policy"});
    }
    if expected["disposition"] == "clarify" {
        return json!({"status":"manual_review","reason":"clarification_structure_valid_wording_unreviewed"});
    }
    json!({"status":"passed","reason":"noop_without_actions_or_question"})
}

fn report_path() -> io::Result<PathBuf> {
    let path = std::env::var_os("JWM_EVA_CORPUS_OUTPUT")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| io::Error::other("absolute_JWM_EVA_CORPUS_OUTPUT_required"))?;
    crate::store::private_dir(
        path.parent()
            .ok_or_else(|| io::Error::other("missing_report_parent"))?,
    )?;
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0 =>
        {
            return Err(io::Error::other("report_must_be_private_regular_file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(path)
}

fn load_report(path: &Path, manifest: &Value) -> io::Result<Value> {
    let report = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            json!({"manifest":manifest,"cases":{}})
        }
        Err(error) => return Err(error),
    };
    if report["manifest"] != *manifest || !report["cases"].is_object() {
        return Err(io::Error::other("resume_manifest_mismatch"));
    }
    Ok(report)
}

async fn connect(c: &Config, key: &str) -> io::Result<Realtime> {
    let mut ws = Realtime::connect(c, key).await?;
    tokio::time::timeout(Duration::from_millis(c.openai.connect_timeout_ms), async {
        loop {
            let event = ws.next().await?;
            check_error(&event)?;
            if event["type"] == "session.created" {
                ws.configure(c).await?;
            }
            if event["type"] == "session.updated" {
                return if ready_with(&event, c) {
                    Ok(())
                } else {
                    Err(io::Error::other("configuration_mismatch"))
                };
            }
        }
    })
    .await
    .map_err(|_| io::Error::other("connect_timeout"))??;
    Ok(ws)
}

async fn evaluate() -> io::Result<()> {
    if std::env::var(OPT_IN).as_deref() != Ok("1") || std::env::var(CHILD).as_deref() != Ok("1") {
        return Err(io::Error::other("explicit_live_corpus_opt_in_required"));
    }
    let socket = std::env::var_os("JWM_EVA_CORPUS_FIXTURE")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("private_fixture_required"))?;
    if std::env::var_os("JWM_SOCKET").as_deref() != Some(socket.as_os_str()) {
        return Err(io::Error::other("fixture_socket_mismatch"));
    }
    crate::store::private_dir(
        socket
            .parent()
            .ok_or_else(|| io::Error::other("missing_fixture_parent"))?,
    )?;
    let fixture = Fixture::start(&socket)?;
    let path = report_path()?;
    let c = Config::load(None)?;
    // Acquiring the genuine exclusive ledger lock is mandatory. No daemon is
    // stopped automatically, and no private fake ledger replaces real billing.
    let store = Arc::new(Mutex::new(Store::open()?));
    let ledger_before = serde_json::to_value(&store.lock().unwrap().ledger)?;
    let corpus = corpus();
    let mut report = load_report(&path, &manifest(&c, &corpus)?)?;
    let ids = corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case["id"].as_str())
        .collect::<BTreeSet<_>>();
    if report["cases"]
        .as_object()
        .unwrap()
        .keys()
        .any(|id| !ids.contains(id.as_str()))
    {
        return Err(io::Error::other("resume_unknown_case_id"));
    }
    let selection = std::env::var("JWM_EVA_CORPUS_CASES")
        .ok()
        .map(|value| value.split(',').map(str::to_owned).collect::<BTreeSet<_>>());
    if selection
        .as_ref()
        .is_some_and(|selection| selection.iter().any(|id| !ids.contains(id.as_str())))
    {
        return Err(io::Error::other("unknown_selected_case_id"));
    }
    let limit = std::env::var("JWM_EVA_CORPUS_LIMIT")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(io::Error::other)?
        .unwrap_or(usize::MAX);
    let mut key: Option<String> = None;
    let mut attempted = 0;
    for case in corpus["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        if report["cases"].get(id).is_some()
            || selection.as_ref().is_some_and(|ids| !ids.contains(id))
        {
            continue;
        }
        if attempted >= limit {
            break;
        }
        attempted += 1;
        let name = case["scenario"].as_str().unwrap();
        let mut result = if let Some(reason) = blocked_reason(name) {
            json!({"status":"blocked_fixture","reason":reason,"provider_called":false})
        } else {
            // Persist the boundary before requesting a connection or response.
            // An interrupted case is never silently retried (and billed twice)
            // by a subsequent resume of the same report.
            report["cases"][id] = json!({"status":"in_progress","started_ms":crate::store::wall_ms(),"provider_called":null});
            report["summary"] = summary(&report, ids.len());
            crate::store::atomic_json(&path, &report)?;
            if key.is_none() {
                key = Some(c.api_key().await?);
            }
            let scenario = resolve(&corpus, name, &mut BTreeSet::new())?;
            let context = context(&scenario)?;
            *fixture.data.lock().unwrap() = discovery(&scenario);
            fixture.rejected.store(false, Ordering::Release);
            let before = store
                .lock()
                .unwrap()
                .usage
                .records
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            let started = Instant::now();
            let mut trace = Trace {
                input: crate::planning_input::FrozenInput::default(),
                response: None,
                audio_items: vec![],
                playback_history: None,
                history_repair_failed: false,
                labels: Value::Null,
                caption: None,
                spoken_reply: None,
                session_end: None,
                rounds: Some(Vec::new()),
            };
            trace.input.set_text(case["text"].as_str().unwrap())?;
            let outcome = async {
                let mut ws = connect(&c, key.as_deref().expect("credential initialized")).await?;
                let connected = elapsed(started);
                let planning = Instant::now();
                let (_, plan) = plan(
                    &c,
                    &mut ws,
                    &mut trace,
                    store.clone(),
                    &context,
                    &mut PlanAdmission::default(),
                    1,
                    attempted as u64,
                )
                .await?;
                Ok::<_, io::Error>((plan, connected, elapsed(planning)))
            }
            .await;
            crate::store::unknown_pending(&store).await?;
            let storage = store.lock().unwrap();
            if serde_json::to_value(&storage.ledger)? != ledger_before {
                return Err(io::Error::other("live_corpus_execution_ledger_changed"));
            }
            let costs=storage.usage.records.iter().filter(|(id,_)|!before.contains(*id)).map(|(id,record)|json!({"response_or_request_id":id,"phase":record.phase,"model":record.model,"settled_microusd":record.settled_microusd,"unknown":record.unknown,"reserved_microusd":record.estimate_microusd})).collect::<Vec<_>>();
            let mut result = match outcome {
                Ok((observed, connected, planning)) => {
                    json!({"grade":grade(case,&observed,&context),"observed":observed,"connection":"cold","connect_ms":connected,"planning_ms":planning,"session_intent":trace.session_end.as_ref().map(|end|end.intent).unwrap_or_default()})
                }
                Err(error) => {
                    json!({"grade":{"status":"error","reason":crate::errors::plan_code(&error)},"connection":"cold"})
                }
            };
            if fixture.rejected.load(Ordering::Acquire) {
                result["grade"] =
                    json!({"status":"error","reason":"fixture_rejected_query_or_command"});
            }
            result["status"] = result["grade"]["status"].clone();
            result["provider_called"] = json!(true);
            result["total_ms"] = json!(elapsed(started));
            result["cost_records"] = json!(costs);
            result["planning_responses"] = json!(trace.rounds.as_ref().map_or(0, Vec::len));
            result
        };
        result["grade_scope"] = json!("plan_structure_only");
        result["unevaluated_invariants"] = case["expected"]
            .get("invariants")
            .cloned()
            .unwrap_or_else(|| json!([]));
        eprintln!(
            "corpus case={id} status={}",
            result["status"].as_str().unwrap_or("error")
        );
        report["cases"][id] = result;
        report["summary"] = summary(&report, ids.len());
        crate::store::atomic_json(&path, &report)?;
    }
    Ok(())
}

fn summary(report: &Value, total: usize) -> Value {
    let mut counts = BTreeMap::<String, usize>::new();
    let mut settled = 0u64;
    let mut unknown_reserved = 0u64;
    let mut unfinished_cost_case_ids = Vec::new();
    for (id, case) in report["cases"].as_object().unwrap() {
        if case["status"] == "in_progress" {
            unfinished_cost_case_ids.push(id.clone());
        }
        *counts
            .entry(case["status"].as_str().unwrap_or("error").into())
            .or_default() += 1;
        for cost in case["cost_records"].as_array().into_iter().flatten() {
            settled = settled.saturating_add(cost["settled_microusd"].as_u64().unwrap_or(0));
            if cost["unknown"] == true {
                unknown_reserved = unknown_reserved
                    .saturating_add(cost["reserved_microusd"].as_u64().unwrap_or(0));
            }
        }
    }
    json!({"corpus_total":total,"recorded":report["cases"].as_object().unwrap().len(),"not_run":total-report["cases"].as_object().unwrap().len(),"counts":counts,
        "cost_complete":unfinished_cost_case_ids.is_empty() && unknown_reserved==0,
        "cost_attribution_complete":unfinished_cost_case_ids.is_empty(),
        "unfinished_cost_case_ids":unfinished_cost_case_ids,
        "attributed_settled_local_estimate_microusd":settled,
        "attributed_unknown_reserved_microusd":unknown_reserved,
        "settled_local_estimate_lower_bound_microusd":settled,
        "settled_local_estimate_microusd":if unfinished_cost_case_ids.is_empty(){Some(settled)}else{None},
        "semantic_acceptance_rate":null,"full_voice_acceptance_rate":null})
}

#[test]
#[ignore = "requires JWM_EVA_RUN_LIVE_CORPUS=1, private absolute JWM_EVA_CORPUS_OUTPUT and stopped daemon; real model fees, no audio or desktop execution"]
fn live_model_acceptance_corpus() {
    assert_eq!(std::env::var(OPT_IN).as_deref(), Ok("1"));
    report_path().expect("private report destination required");
    let dir = std::env::temp_dir().join(format!(
        "eva-corpus-{}-{}",
        std::process::id(),
        crate::store::wall_ms()
    ));
    crate::store::private_dir(&dir).unwrap();
    let socket = dir.join("fixture.sock");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "session::corpus_runner::live_model_corpus_child",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("JWM_SOCKET", &socket)
        .env("JWM_EVA_CORPUS_FIXTURE", &socket)
        .status();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        status.expect("corpus child launch").success(),
        "corpus evaluation failed; inspect durable result and real cost ledger"
    );
}

#[test]
fn live_model_corpus_child() {
    if std::env::var(CHILD).as_deref() != Ok("1") {
        return;
    }
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(evaluate())
        .expect("explicit live corpus runner failed");
}

#[test]
fn every_corpus_case_has_a_static_fixture_or_explicit_blocker() {
    let corpus = corpus();
    for case in corpus["cases"].as_array().unwrap() {
        let scenario = resolve(
            &corpus,
            case["scenario"].as_str().unwrap(),
            &mut BTreeSet::new(),
        )
        .unwrap();
        let context = context(&scenario).unwrap();
        let _ = discovery(&scenario);
        if case["expected"]["disposition"] == "execute"
            && case["expected"]["actions"].is_array()
            && blocked_reason(case["scenario"].as_str().unwrap()).is_none()
        {
            let expected: jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":"execute","actions":case["expected"]["actions"],"question":null,"reason":null})).unwrap();
            let frozen = validate_plan(&expected, &context)
                .unwrap_or_else(|error| panic!("{}: {error}", case["id"]));
            assert_eq!(grade(case, &frozen, &context)["status"], "passed");
        }
    }
}

#[test]
fn grading_never_promotes_underspecified_or_unreviewed_expectations() {
    let context = context(&resolve(&corpus(), "standard", &mut BTreeSet::new()).unwrap()).unwrap();
    let plan:jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":3}}],"question":null,"reason":null})).unwrap();
    let frozen = validate_plan(&plan, &context).unwrap();
    assert_eq!(
        grade(
            &json!({"expected":{"disposition":"execute"}}),
            &frozen,
            &context
        )["status"],
        "manual_review"
    );
    assert_eq!(
        grade(
            &json!({"expected":{"disposition":"noop","actions":[]}}),
            &frozen,
            &context
        )["safety_failure"],
        true
    );
    let expected = json!({"expected":{"disposition":"execute","actions":[{"name":"switch_tag","arguments":{"monitor_ref":"m2","tag_number":3}}]}});
    assert_eq!(grade(&expected, &frozen, &context)["safety_failure"], true);
    assert_eq!(
        grade(
            &json!({"expected":{"disposition":"execute","local_confirmation_required":true}}),
            &frozen,
            &context
        )["reason"],
        "required_local_confirmation_missing"
    );
}

#[test]
fn fixture_rejects_mutations_and_unknown_queries_without_forwarding() {
    let dir = std::env::temp_dir().join(format!(
        "eva-corpus-fixture-test-{}-{}",
        std::process::id(),
        crate::store::wall_ms()
    ));
    crate::store::private_dir(&dir).unwrap();
    let socket = dir.join("fixture.sock");
    let fixture = Fixture::start(&socket).unwrap();
    *fixture.data.lock().unwrap() = json!({"get_media_status":{"active":false}});
    let ipc = jwm_ipc_client::JwmIpc::with_socket(socket);
    assert_eq!(
        ipc.query("get_media_status").unwrap(),
        json!({"active":false})
    );
    for result in [
        ipc.command("quit", json!({})),
        ipc.query("get_execution_context"),
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "corpus_fixture_read_only_or_unsupported_query"
        );
    }
    assert!(fixture.rejected.load(Ordering::Acquire));
    drop(fixture);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resume_rejects_changed_source_config_or_fixture_identity() {
    let dir = std::env::temp_dir().join(format!(
        "eva-corpus-resume-test-{}-{}",
        std::process::id(),
        crate::store::wall_ms()
    ));
    crate::store::private_dir(&dir).unwrap();
    let path = dir.join("result.json");
    let identity = manifest(&Config::default(), &corpus()).unwrap();
    let report = load_report(&path, &identity).unwrap();
    crate::store::atomic_json(&path, &report).unwrap();
    assert_eq!(load_report(&path, &identity).unwrap(), report);
    for key in [
        "config_fingerprint",
        "source_fingerprint",
        "fixture_fingerprint",
        "corpus_fingerprint",
    ] {
        let mut changed = identity.clone();
        changed[key] = json!("changed");
        assert_eq!(
            load_report(&path, &changed).unwrap_err().to_string(),
            "resume_manifest_mismatch"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn interrupted_case_never_turns_missing_costs_into_a_zero_total() {
    let report = json!({"cases":{
        "eva-001":{"status":"passed","cost_records":[{"settled_microusd":17,"unknown":false}]},
        "eva-002":{"status":"in_progress","started_ms":1,"provider_called":null}
    }});
    let summary = summary(&report, 241);
    assert_eq!(summary["cost_complete"], false);
    assert_eq!(summary["cost_attribution_complete"], false);
    assert_eq!(summary["unfinished_cost_case_ids"], json!(["eva-002"]));
    assert_eq!(summary["settled_local_estimate_lower_bound_microusd"], 17);
    assert!(summary["settled_local_estimate_microusd"].is_null());
}
