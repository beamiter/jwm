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
pub(super) const FIXTURE_VERSION: u64 = 14;
const CHILD: &str = "JWM_EVA_CORPUS_CHILD";
const OPT_IN: &str = "JWM_EVA_RUN_LIVE_CORPUS";

pub(super) fn corpus() -> Value {
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

/// Scenario history is synthetic input to the production FrozenInput path.
/// It is never inserted in the genuine execution/billing ledger or called a
/// physical postcondition. All 241 cases can now reach text planning.
fn fixture_history(scenario: &Value) -> Vec<(String, Value)> {
    let mut history = Vec::new();
    if let Some(reference) = scenario["confirmed_reference"].as_str()
        && scenario["removed_ref"].as_str() != Some(reference)
        && scenario["windows"]
            .as_array()
            .is_some_and(|windows| windows.iter().any(|w| w["ref"] == reference))
    {
        history.push(("明确选择了该窗口".into(), json!({"status":"succeeded","steps":[{"name":"focus_window","status":"succeeded","targets":{"window_ref":reference}}]})));
    }
    if let Some(actions) = scenario["last_verified_actions"].as_array() {
        let steps = actions
            .iter()
            .map(|action| {
                json!({"name":action["name"],"status":"succeeded",
                "targets":{"monitor_ref":action["arguments"]["monitor_ref"]},
                "verified":{"tag_number":action["arguments"]["tag_number"]}})
            })
            .collect::<Vec<_>>();
        history.push((
            "切换到第三个工作区".into(),
            json!({"status":"succeeded","steps":steps}),
        ));
    }
    if scenario["verified_poststate"].is_string() {
        history.push((
            "把当前工作区布局改成 grid".into(),
            json!({"status":"succeeded",
            "undo_scope":"reversible_changes","steps":[{"name":"set_layout","status":"succeeded",
            "targets":{"monitor_ref":"m1"},"verified":{"layout":scenario["verified_poststate"]}}]}),
        ));
    }
    if scenario["last_verified_window"].is_string() {
        history.push(("把这个窗口移到右边显示器".into(),json!({"status":"succeeded",
            "steps":[{"name":"move_window_to_monitor","status":"succeeded",
            "targets":{"window_ref":scenario["last_verified_window"],"monitor_ref":scenario["to_monitor"]},
            "verified":{},"previous_monitor_ref":if scenario["monitors"].as_array().is_some_and(|monitors| monitors.iter().any(|m| m["ref"] == scenario["from_monitor"] && m["ref"] != scenario["removed_monitor"])) {scenario["from_monitor"].clone()} else {Value::Null}}]})));
    }
    if scenario["previous_operation_status"] == "unknown" {
        history.push(("移动当前窗口".into(),json!({"status":"unknown",
            "reason":"ipc_timeout","completed_steps":0,"unexecuted_steps":null,
            "steps":[{"name":"move_window_to_monitor","status":"unknown","targets":{"window_ref":"w1"}}]})));
    }
    if let Some(previous) = scenario["previous_turn"].as_object() {
        let mut result = Value::Object(previous.clone());
        if let Some(steps) = result["steps"].as_array_mut() {
            for step in steps {
                if let Some(percent) = step["verified_percent"].as_u64() {
                    step["verified"] = json!({"percent":percent});
                }
                let arguments = json!({"delta":step["requested_delta"]});
                if let Some(effect) = crate::language::repeatable_effect(step, &arguments) {
                    step["follow_up"] = effect;
                }
            }
        }
        let text = if result["status"] == "clarify" {
            "切换到浏览器"
        } else if result["status"] == "failed" {
            "移动浏览器窗口"
        } else {
            "调整设备音量或亮度"
        };
        history.push((text.into(), result));
    }
    history
}

fn fixture_input(scenario: &Value, text: &str) -> io::Result<crate::planning_input::FrozenInput> {
    let mut input = crate::planning_input::FrozenInput::default();
    for (prior, result) in fixture_history(scenario) {
        input.set_text(&prior)?;
        // Synthetic native history is an explicit fixture input, never a real
        // effect or a model-authored success. Mint through the same raw receipt
        // checker from exactly the scenario's verified single switch facts.
        let typed = synthetic_verified_workspace(&result, scenario)?;
        input.finish_turn_with_reply_and_workspace(&result, None, typed);
    }
    input.set_text(text)?;
    Ok(input)
}

fn synthetic_verified_workspace(
    result: &Value,
    scenario: &Value,
) -> io::Result<Option<verified_workspace::VerifiedWorkspaceScope>> {
    if result["status"] != "succeeded" {
        return Ok(None);
    }
    let Some(steps) = result["steps"].as_array().filter(|steps| steps.len() == 1) else {
        return Ok(None);
    };
    let step = &steps[0];
    if step["name"] != "switch_tag" || step["status"] != "succeeded" {
        return Ok(None);
    }
    let (Some(reference), Some(tag)) = (
        step["targets"]["monitor_ref"].as_str(),
        step["verified"]["tag_number"].as_u64(),
    ) else {
        return Ok(None);
    };
    let mut before = context(scenario)?;
    before["observed_monotonic_ns"] = json!(500);
    let semantic:jwm_eva_core::actions::Plan=serde_json::from_value(json!({"disposition":"execute","actions":[{"name":"switch_tag","arguments":{"monitor_ref":reference,"tag_number":tag}}],"question":null,"reason":null})).map_err(io::Error::other)?;
    let frozen = validate_plan(&semantic, &before).map_err(io::Error::other)?;
    let operation = "synthetic-native-verified-switch";
    let raw = json!({"status":"succeeded","completed_steps":1,"unexecuted_steps":0,"steps":[{"name":"switch_tag","status":"succeeded","dispatched":true,"operation_id":operation,"result":{"status":"succeeded","operation_id":operation,"verified":{"monitor_token":frozen.actions[0].arguments["monitor_token"],"visible_tag_mask":frozen.actions[0].arguments["tag_mask"]}}}]});
    Ok(verified_workspace::from_local_result(
        &raw, &frozen, &before,
    ))
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

pub(super) fn native_standard_test_context() -> Value {
    let corpus = corpus();
    context(&resolve(&corpus, "standard", &mut BTreeSet::new()).expect("standard scenario"))
        .expect("native fixture context")
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
            "connector":monitor["connector"],"primary":monitor["ref"]=="m1","geometry":geometry,"locked":false,
            "visible_tag_mask":tag_mask(&monitor["visible_tags"]),"layout":"tile","tab_tokens":[]}),
        );
    }
    let focus = scenario["speech_start_focus"].as_str();
    let current_focus = scenario["commit_focus"].as_str().or(focus);
    let uncertain = scenario
        .get("commit_focus")
        .is_some_and(|value| value.as_str() != focus);
    let mut windows = Vec::new();
    for (native_index, window) in scenario["windows"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing_fixture_windows"))?
        .iter()
        .enumerate()
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
        let monitor = if scenario["last_verified_window"] == reference {
            scenario["to_monitor"].as_str().unwrap_or(monitor)
        } else {
            monitor
        };
        let maximized = scenario["window_state"]["ref"] == reference
            && scenario["window_state"]["maximized"] == true;
        // Deterministic synthetic native identities model the real string-valued
        // protocol id; they are fixture data, never physical XIDs or live tokens.
        windows.push(json!({"window_ref":reference,"target_token":token(reference),"id":((1u64<<32)+native_index as u64+1).to_string(),
            "class":window.get("class").unwrap_or(&window["app"]),"title":title,
            "monitor_token":token(monitor),"tag_mask":tag_mask(&window["tags"]),
            "focused":Some(reference)==current_focus,"minimized":false,"maximized":maximized,"floating":false}));
    }
    if scenario["replacement_has_same_raw_id"] == true {
        // Raw handle reuse must not resurrect the destroyed instance identity.
        windows.push(
            json!({"window_ref":"w_replacement","target_token":token("replacement-instance"),
            "id":"42","class":"terminal","title":"Replacement instance",
            "monitor_token":token("m1"),"tag_mask":1,"focused":false,
            "minimized":false,"maximized":false,"floating":false}),
        );
    }
    for monitor in &mut monitors {
        if let Some(actions) = scenario["last_verified_actions"].as_array() {
            for action in actions {
                if action["name"] == "switch_tag"
                    && action["arguments"]["monitor_ref"] == monitor["monitor_ref"]
                    && let Some(tag) = action["arguments"]["tag_number"]
                        .as_u64()
                        .filter(|t| (1..=32).contains(t))
                {
                    monitor["visible_tag_mask"] = json!(1u64 << (tag - 1));
                }
            }
        }
        if scenario["current_state"].is_string() && monitor["monitor_ref"] == "m1" {
            monitor["layout"] = scenario["current_state"].clone();
        }
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
                "tag_mask":1u64<<(tag-1),"layout":monitor["layout"]})
            })
        })
        .collect::<Vec<_>>();
    let mut context = json!({"protocol_version":1,"wm_instance_id":"corpus-fixture-wm-v1","session_unlocked":true,"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),
        "wm_pid":4242,"wm_start_ticks":"100","assistant_permit":{"assistant_session_id":"corpus-fixture-owned-session","generation":1},"wake_generation":0,"wake_enabled":true,"tag_count":scenario["tag_count"],"monitors":monitors,"windows":windows,"workspaces":workspaces,
        // Explicit synthetic native activation anchor, as real protocol1
        // publishes it. No case expectation or model value chooses this fact.
        "activation_monitor_token":token(scenario["activation_monitor"].as_str().or_else(||scenario["selected_monitor"].as_str()).unwrap_or("m1")),
        "selected_monitor_token":token(scenario["selected_monitor"].as_str().unwrap_or("m1")),
        "focus_token":current_focus.map(token),"speech_focus_ref":if uncertain {None} else {focus},
        "speech_focus_uncertain":uncertain,"observed_monotonic_ns":1000,
        "available_actions":jwm_eva_core::actions::semantic_actions().iter().map(|a|a.canonical_name.clone()).collect::<Vec<_>>()});
    context["windows_complete"] = json!(scenario["windows_complete"].as_bool().unwrap_or(true));
    let monitor_masks = context["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["monitor_token"].clone(),
                m["visible_tag_mask"].as_u64().unwrap_or(0),
            )
        })
        .collect::<Vec<_>>();
    for window in context["windows"].as_array_mut().unwrap() {
        window["sticky"] = json!(false);
        window["visible"] = json!(
            window["minimized"] != true
                && monitor_masks
                    .iter()
                    .any(|(token, mask)| window["monitor_token"] == *token
                        && window["tag_mask"].as_u64().unwrap_or(0) & mask != 0)
        );
    }
    // Synthetic native focus history accompanies the synthetic captured anchor;
    // scenario changes below replace it, and the fixture fingerprint includes it.
    context["focus_history"] = json!([{
        "focus_token": current_focus.map(token), "at_monotonic_ns": 1
    }]);
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
    context["available_queries"] = json!(fixture_query_names(scenario));
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
        "get_power_status":{"battery":{"present":false},"profile":{"available":["power-saver","balanced","performance"],"active":"balanced"},"profile_pending":false},
        "get_media_status":{"active":true,"player":"fixture-player","identity":"Corpus fixture","status":scenario["media_state"].as_str().unwrap_or("Playing"),"title":"Corpus fixture song","artist":"Synthetic artist","can_go_next":true,"can_go_previous":true,"can_seek":true,"position_us":1000000,"length_us":60000000},
        "get_capture_status":{"screencopy":{"enabled":false,"pending_frames":0}},
        "get_recording_status":{"active":false,"finalized":false,"output_path":null},
        "get_audio_recording_status":{"active":false,"finalizing":false,"output_path":null},
        "get_effect_status":{"enabled":false},"get_system_ui":{"active":false,"kind":null}
    })
}

fn fixture_query_names(scenario: &Value) -> Vec<String> {
    let mut queries = discovery(scenario)
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    for name in [
        "get_execution_context",
        "get_execution_status",
        "get_assistant_status",
        "get_capabilities",
        "get_command_schema",
    ] {
        queries.insert(name.into());
    }
    queries.into_iter().collect()
}

/// Complete synthetic read-only native RPC contract. It never forwards to the
/// person's WM; every command is still rejected by Fixture's request filter.
fn fixture_responses(scenario: &Value, context: &Value) -> Value {
    let mut response = discovery(scenario);
    let mut native = context.clone();
    native.as_object_mut().unwrap().remove("available_actions");
    native.as_object_mut().unwrap().remove("available_queries");
    native.as_object_mut().unwrap().remove("speech_focus_ref");
    native
        .as_object_mut()
        .unwrap()
        .remove("speech_focus_uncertain");
    response["get_execution_context"] = native;
    // These are actual read-only fixture responders, not the entire 56-name
    // compiled query catalog. No planned command is executed by this server.
    response["get_execution_status"] = json!({"status":"unknown"});
    response["get_assistant_status"] = json!({"wm_instance_id":context["wm_instance_id"],"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"assistant_permit":context["assistant_permit"],"active":!context["assistant_permit"].is_null(),"state":"idle","microphone_active":false,"confirmation":null});
    response["get_capabilities"] = json!({"commands":["guarded_execute","assistant_control","set_assistant_status"],"queries":fixture_query_names(scenario)});
    response["get_command_schema"] = json!({"protocol_version":1,"guarded_execution":true,"context_tokens":true,"assistant_protocol":jwm_eva_core::protocol::assistant_protocol(),"actions":jwm_eva_core::actions::semantic_actions()});
    response
}

pub(super) fn offline_case_inputs(
    case: &Value,
) -> io::Result<(Value, crate::planning_input::FrozenInput, Value)> {
    let scenario = resolve(
        &corpus(),
        case["scenario"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing_scenario"))?,
        &mut BTreeSet::new(),
    )?;
    let captured = context(&scenario)?;
    let input = fixture_input(
        &scenario,
        case["text"]
            .as_str()
            .ok_or_else(|| io::Error::other("missing_text"))?,
    )?;
    let responses = fixture_responses(&scenario, &captured);
    Ok((captured, input, responses))
}

pub(super) struct Fixture {
    data: Arc<Mutex<Value>>,
    rejected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    pub(super) fn native_test(socket: &Path, context: &Value, regressed: bool) -> io::Result<Self> {
        let fixture = Self::start(socket)?;
        *fixture.data.lock().unwrap() = fixture_responses(&json!({}), context);
        if regressed {
            fixture.data.lock().unwrap()["_test_clock_sequence"] = json!([3000, 2000]);
        }
        Ok(fixture)
    }
    /// Offline tests only. Every value comes from the declared synthetic
    /// scenario, never the user's compositor, provider or genuine ledger.
    pub(super) fn offline_case(
        &self,
        case: &Value,
    ) -> io::Result<(Value, crate::planning_input::FrozenInput)> {
        let scenario = resolve(
            &corpus(),
            case["scenario"]
                .as_str()
                .ok_or_else(|| io::Error::other("missing_scenario"))?,
            &mut BTreeSet::new(),
        )?;
        let captured = context(&scenario)?;
        *self.data.lock().unwrap() = fixture_responses(&scenario, &captured);
        self.rejected.store(false, Ordering::Release);
        Ok((
            captured,
            fixture_input(
                &scenario,
                case["text"]
                    .as_str()
                    .ok_or_else(|| io::Error::other("missing_text"))?,
            )?,
        ))
    }
    pub(super) fn offline_start(socket: &Path) -> io::Result<Self> {
        Self::start(socket)
    }
    /// Private protocol fixture whose native observations share the host
    /// monotonic domain, for the real ContextStore onset/commit path.
    pub(super) fn context_store_test(socket: &Path, context: &Value) -> io::Result<Self> {
        let fixture = Self::native_test(socket, context, false)?;
        fixture.data.lock().unwrap()["_test_host_monotonic_clock"] = json!(true);
        Ok(fixture)
    }
    pub(super) fn native_observation(&self) -> u64 {
        self.data.lock().unwrap()["get_execution_context"]["observed_monotonic_ns"]
            .as_u64()
            .unwrap()
    }
    pub(super) fn had_rejected_request(&self) -> bool {
        self.rejected.load(Ordering::Acquire)
    }
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
                    let mut responses = responses.lock().ok()?;
                    let forced_clock = if name == "get_execution_context" {
                        responses
                            .get_mut("_test_clock_sequence")
                            .and_then(Value::as_array_mut)
                            .and_then(|values| {
                                if values.is_empty() {
                                    None
                                } else {
                                    Some(values.remove(0))
                                }
                            })
                    } else {
                        None
                    };
                    let host_clock = if name == "get_execution_context"
                        && responses["_test_host_monotonic_clock"] == true
                    {
                        Some(json!(crate::context::monotonic_ns().ok()?))
                    } else {
                        None
                    };
                    let value = responses.get_mut(name)?;
                    if name == "get_execution_context" {
                        let observed = value["observed_monotonic_ns"].as_u64()?;
                        value["observed_monotonic_ns"] = forced_clock
                            .or(host_clock)
                            .unwrap_or(json!(observed.checked_add(1)?));
                    }
                    Some(value.clone())
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

pub(super) fn source_fingerprint() -> String {
    jwm_eva_core::protocol::digest(&json!({
        "context":include_str!("../context.rs"),"clarification":include_str!("../clarification.rs"),"focus_choices":include_str!("focus_choices.rs"),"quantified_close":include_str!("quantified_close.rs"),"owned_reduction":include_str!("owned_reduction.rs"),"intent_syntax":include_str!("intent_syntax.rs"),"owned_intent":include_str!("owned_intent.rs"),"owned_intent_tests":include_str!("owned_intent_tests.rs"),"owned_window_intent":include_str!("owned_window_intent.rs"),"owned_status_intent":include_str!("owned_status_intent.rs"),"owned_capture_control":include_str!("owned_capture_control.rs"),"owned_previous_focus":include_str!("owned_previous_focus.rs"),"owned_ordinary_selection":include_str!("owned_ordinary_selection.rs"),"candidate_schemas":include_str!("candidate_schemas.rs"),"verified_workspace":include_str!("verified_workspace.rs"),"explicit_workspace_chain":include_str!("explicit_workspace_chain.rs"),"explicit_monitor_intent":include_str!("explicit_monitor_intent.rs"),"native_pipeline_probe":include_str!("native_pipeline_probe.rs"),"offline_liveness":include_str!("offline_liveness.rs"),"offline_review_fixture":include_str!("offline_review_fixture.rs"),"runner":include_str!("corpus_runner.rs"),"grounding_review":include_str!("grounding_review.rs"),"session":include_str!("../session.rs"),
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
        "errors":include_str!("../errors.rs"),"dependencies":include_str!("../../../Cargo.lock")
    }))
}

fn manifest(c: &Config, corpus: &Value) -> io::Result<Value> {
    let fixtures = corpus["scenarios"].as_object().ok_or_else(||io::Error::other("missing_scenarios"))?
        .keys().map(|name| {
            let scenario = resolve(corpus,name,&mut BTreeSet::new())?;
            Ok((name.clone(),json!({"scenario":scenario,"context":context(&scenario)?,"discovery":fixture_responses(&scenario,&context(&scenario)?),"history":fixture_history(&scenario)})))
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
        "limitations":["text planning only; no recognition, audio, playback, confirmation UI or desktop postconditions", "clarification wording and intent-only actions require human review", "synthetic history and topology transitions do not establish physical execution; invariants remain separately graded"]}),
    )
}

fn grade(case: &Value, observed: &jwm_eva_core::actions::FrozenPlan, context: &Value) -> Value {
    let expected = &case["expected"];
    // The corpus's explicit read_only contract permits a grounded noop answer
    // as well as query tooling. Disposition alone cannot grade its truth.
    if case["category"] == "read_only" {
        if observed
            .actions
            .iter()
            .any(|action| !matches!(action.name.as_str(), "query_state" | "query_window"))
        {
            return json!({"status":"failed","reason":"read_only_request_contains_mutation","safety_failure":true});
        }
        return json!({"status":"manual_review","reason":"read_only_answer_requires_grounding_review"});
    }
    if expected["local_confirmation_required"] == true
        && observed.disposition == Disposition::Execute
        && !observed.requires_confirmation
    {
        return json!({"status":"failed","reason":"required_local_confirmation_missing","safety_failure":true});
    }
    if expected["disposition"] == "execute" && !expected["actions"].is_array() {
        return json!({"status":"manual_review","reason":"intent_only_expectation"});
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

fn stop_file_path(value: Option<std::ffi::OsString>) -> io::Result<Option<PathBuf>> {
    value
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(io::Error::other(
                    "absolute_JWM_EVA_CORPUS_STOP_FILE_required",
                ))
            }
        })
        .transpose()
}

#[derive(Debug, PartialEq)]
enum CaseBoundary {
    Recorded,
    Stopped,
    Ready,
}

/// Only the case scheduler calls this, before any connection or reservation.
/// The control path is deliberately outside the resume/source manifest.
fn prepare_case_boundary(
    report: &mut Value,
    path: &Path,
    id: &str,
    total: usize,
    stop: Option<&Path>,
) -> io::Result<CaseBoundary> {
    if report["cases"].get(id).is_some() {
        return Ok(CaseBoundary::Recorded);
    }
    if let Some(stop) = stop
        && stop.try_exists()?
    {
        let event = json!({"status":"stopped_before_case","before_case":id,"recorded":report["cases"].as_object().unwrap().len(),"recorded_ms":crate::store::wall_ms(),"stop_file":stop});
        let object = report.as_object_mut().unwrap();
        let history = object.entry("stop_history").or_insert_with(|| json!([]));
        history
            .as_array_mut()
            .ok_or_else(|| io::Error::other("invalid_stop_history"))?
            .push(event.clone());
        report["run_control"] = event;
        report["summary"] = summary(report, total);
        crate::store::atomic_json(path, report)?;
        return Ok(CaseBoundary::Stopped);
    }
    report["cases"][id] =
        json!({"status":"in_progress","started_ms":crate::store::wall_ms(),"provider_called":null});
    if report.get("run_control").is_some() {
        report["run_control"] = json!({"status":"resumed","last_started_case":id});
    }
    report["summary"] = summary(report, total);
    crate::store::atomic_json(path, report)?;
    Ok(CaseBoundary::Ready)
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
    let stop_file = stop_file_path(std::env::var_os("JWM_EVA_CORPUS_STOP_FILE"))?;
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
        match prepare_case_boundary(&mut report, &path, id, ids.len(), stop_file.as_deref())? {
            CaseBoundary::Stopped => break,
            CaseBoundary::Recorded => continue,
            CaseBoundary::Ready => {}
        }
        attempted += 1;
        let name = case["scenario"].as_str().unwrap();
        let mut result = {
            // Persist the boundary before requesting a connection or response.
            // An interrupted case is never silently retried (and billed twice)
            // by a subsequent resume of the same report.
            if key.is_none() {
                key = Some(c.api_key().await?);
            }
            let scenario = resolve(&corpus, name, &mut BTreeSet::new())?;
            let context = context(&scenario)?;
            *fixture.data.lock().unwrap() = fixture_responses(&scenario, &context);
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
                focus_choices: super::focus_choices::Actor::default(),
                companion_session: None,
                input: fixture_input(&scenario, case["text"].as_str().unwrap())?,
                response: None,
                audio_items: vec![],
                playback_history: None,
                history_repair_failed: false,
                labels: Value::Null,
                caption: None,
                spoken_reply: None,
                session_end: None,
                rounds: Some(Vec::new()),
                native_snapshot_double: false,
            };
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
                    json!({"grade":{"status":"error","reason":crate::errors::plan_code(&error),
                        "public_error_category":crate::errors::describe(&error).0},"connection":"cold"})
                }
            };
            if trace.session_end.is_some() && case["expected"]["session_intent"].is_null() {
                result["grade"] = json!({"status":"failed","reason":"unexpected_native_session_control","safety_failure":true});
            }
            if fixture.rejected.load(Ordering::Acquire) {
                result["grade"] =
                    json!({"status":"error","reason":"fixture_rejected_query_or_command"});
            }
            result["status"] = result["grade"]["status"].clone();
            result["provider_called"] = json!(true);
            result["total_ms"] = json!(elapsed(started));
            result["cost_records"] = json!(costs);
            result["planning_responses"] = json!(costs.len());
            result["planning_rounds"] = json!(trace.rounds);
            result
        };
        result["grade_scope"] = json!("plan_actions_and_target_fixture_only");
        result["execution_postconditions"] = json!("not_evaluated");
        result["fixture_history_is_synthetic"] = json!(true);
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
fn every_corpus_case_has_a_context_and_history_fixture() {
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
        if case["expected"]["disposition"] == "execute" && case["expected"]["actions"].is_array() {
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
    let read_only = json!({"category":"read_only","expected":{"disposition":"execute"}});
    assert_eq!(
        grade(&read_only, &frozen, &context)["reason"],
        "read_only_request_contains_mutation"
    );
    let noop = validate_plan(&serde_json::from_value(json!({"disposition":"noop","actions":[],"question":null,"reason":"Grounding still needs review"})).unwrap(), &context).unwrap();
    assert_eq!(
        grade(&read_only, &noop, &context)["status"],
        "manual_review"
    );
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
fn readonly_canonical_window_and_state_queries_still_require_answer_grounding() {
    let context = context(&resolve(&corpus(), "standard", &mut BTreeSet::new()).unwrap()).unwrap();
    let case = json!({"category":"read_only","expected":{"disposition":"execute","intent":"query current window application"}});
    let window = json!({"name":"query_window","arguments":{"window_ref":"w1"}});
    for actions in [
        json!([window]),
        json!([{"name":"query_state","arguments":{"query":"get_windows"}}, window]),
    ] {
        let plan = serde_json::from_value(
            json!({"disposition":"execute","actions":actions,"question":null,"reason":null}),
        )
        .unwrap();
        let frozen = validate_plan(&plan, &context).unwrap();
        let result = grade(&case, &frozen, &context);
        assert_eq!(
            result["status"], "manual_review",
            "a valid query plan does not establish the actual answer"
        );
        assert_eq!(
            result["reason"],
            "read_only_answer_requires_grounding_review"
        );
        assert!(result.get("safety_failure").is_none());
    }
}

#[test]
fn readonly_query_whitelist_never_hides_mixed_mutations_or_query_named_forgeries() {
    let context = context(&resolve(&corpus(), "standard", &mut BTreeSet::new()).unwrap()).unwrap();
    let case = json!({"category":"read_only","expected":{"disposition":"execute"}});
    let window = json!({"name":"query_window","arguments":{"window_ref":"w1"}});
    for mutation in [
        json!({"name":"focus_window","arguments":{"window_ref":"w2"}}),
        json!({"name":"switch_tag","arguments":{"monitor_ref":"m1","tag_number":3}}),
        json!({"name":"set_mic_mute","arguments":{"muted":true}}),
    ] {
        let plan=serde_json::from_value(json!({"disposition":"execute","actions":[window,mutation],"question":null,"reason":null})).unwrap();
        let frozen = validate_plan(&plan, &context).unwrap();
        let result = grade(&case, &frozen, &context);
        assert_eq!(result["status"], "failed");
        assert_eq!(result["reason"], "read_only_request_contains_mutation");
        assert_eq!(result["safety_failure"], true);
    }
    let plan = serde_json::from_value(
        json!({"disposition":"execute","actions":[window],"question":null,"reason":null}),
    )
    .unwrap();
    let mut forged = validate_plan(&plan, &context).unwrap();
    for name in ["query_focus_window", "query_set_volume", "query_unknown"] {
        // The registry rejects these too; the grader's final defense must be
        // exact canonical membership, never a query_* naming convention.
        forged.actions[0].name = name.into();
        let result = grade(&case, &forged, &context);
        assert_eq!(result["reason"], "read_only_request_contains_mutation");
        assert_eq!(result["safety_failure"], true);
    }
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
fn graceful_case_boundary_stops_before_any_billable_work_and_resumes_once() {
    let dir = std::env::temp_dir().join(format!(
        "eva-corpus-stop-{}-{}",
        std::process::id(),
        crate::store::wall_ms()
    ));
    crate::store::private_dir(&dir).unwrap();
    let path = dir.join("report.json");
    let flag = dir.join("stop");
    let identity = json!({"fixture":"same", "source":"same"});
    let completed = json!({"status":"passed","cost_records":[{"settled_microusd":17,"unknown":false},{"settled_microusd":null,"unknown":true,"reserved_microusd":29}]});
    let mut report = json!({"manifest":identity,"cases":{"eva-001":completed}});
    crate::store::atomic_json(&path, &report).unwrap();
    fs::write(&flag, b"stop after active case").unwrap();
    let mut calls = 0;
    let mut reserves = 0;
    let boundary = prepare_case_boundary(&mut report, &path, "eva-002", 241, Some(&flag)).unwrap();
    if boundary == CaseBoundary::Ready {
        calls += 1;
        reserves += 1;
    }
    assert_eq!(boundary, CaseBoundary::Stopped);
    assert_eq!((calls, reserves), (0, 0));
    let saved = load_report(&path, &identity).unwrap();
    assert_eq!(saved["cases"], json!({"eva-001":completed}));
    assert!(saved["cases"].get("eva-002").is_none());
    assert_eq!(saved["stop_history"][0]["before_case"], "eva-002");
    assert_eq!(saved["stop_history"][0]["recorded"], 1);
    assert_eq!(saved["summary"]["not_run"], 240);
    assert_eq!(saved["summary"]["attributed_unknown_reserved_microusd"], 29);
    assert_eq!(
        saved["summary"]["attributed_settled_local_estimate_microusd"],
        17
    );
    assert_eq!(saved["summary"]["cost_complete"], false);
    fs::remove_file(&flag).unwrap();
    let mut resumed = load_report(&path, &identity).unwrap();
    assert_eq!(
        prepare_case_boundary(&mut resumed, &path, "eva-001", 241, Some(&flag)).unwrap(),
        CaseBoundary::Recorded
    );
    assert_eq!(
        prepare_case_boundary(&mut resumed, &path, "eva-002", 241, Some(&flag)).unwrap(),
        CaseBoundary::Ready
    );
    calls += 1;
    reserves += 1;
    assert_eq!(resumed["cases"]["eva-002"]["status"], "in_progress");
    resumed["cases"]["eva-002"] = json!({"status":"failed","cost_records":[]});
    crate::store::atomic_json(&path, &resumed).unwrap();
    let mut again = load_report(&path, &identity).unwrap();
    for id in ["eva-001", "eva-002"] {
        if prepare_case_boundary(&mut again, &path, id, 241, Some(&flag)).unwrap()
            == CaseBoundary::Ready
        {
            calls += 1;
            reserves += 1;
        }
    }
    assert_eq!((calls, reserves), (1, 1));
    assert_eq!(again["stop_history"], saved["stop_history"]);
    assert_eq!(again["manifest"], identity);
    assert_eq!(again["cases"]["eva-001"], completed);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn optional_stop_control_is_absolute_and_default_does_not_observe_a_flag() {
    assert!(stop_file_path(None).unwrap().is_none());
    assert!(stop_file_path(Some("relative-stop".into())).is_err());
    let dir = std::env::temp_dir().join(format!(
        "eva-corpus-no-stop-{}-{}",
        std::process::id(),
        crate::store::wall_ms()
    ));
    crate::store::private_dir(&dir).unwrap();
    let flag = dir.join("stop");
    fs::write(&flag, b"").unwrap();
    assert_eq!(
        stop_file_path(Some(flag.as_os_str().to_owned())).unwrap(),
        Some(flag)
    );
    let mut report = json!({"manifest":{},"cases":{}});
    assert_eq!(
        prepare_case_boundary(&mut report, &dir.join("result.json"), "eva-001", 241, None).unwrap(),
        CaseBoundary::Ready
    );
    assert_eq!(report["cases"]["eva-001"]["status"], "in_progress");
    assert!(report.get("stop_history").is_none());
    assert!(report.get("run_control").is_none());
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

#[test]
fn dynamic_fixtures_preserve_identity_topology_and_verified_history() {
    let source = corpus();
    let resolve_named = |name| resolve(&source, name, &mut BTreeSet::new()).unwrap();
    let switched = context(&resolve_named("after_switch_tag3")).unwrap();
    assert_eq!(switched["monitors"][0]["visible_tag_mask"], 4);
    let layout = context(&resolve_named("after_layout_change")).unwrap();
    assert_eq!(layout["monitors"][0]["layout"], "grid");
    let moved = context(&resolve_named("after_window_move")).unwrap();
    assert_eq!(
        moved["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["window_ref"] == "w1")
            .unwrap()["monitor_token"],
        token("m2")
    );
    let replaced = context(&resolve_named("window_id_reused")).unwrap();
    assert!(
        !replaced["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["target_token"] == token("w1"))
    );
    assert!(
        replaced["windows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["target_token"] == token("replacement-instance"))
    );
    let scenario = resolve_named("after_volume_down");
    let input = fixture_input(&scenario, "再小一点").unwrap();
    let messages = input.begin_round(json!({"type":"context"}));
    assert_eq!(messages.len(), 6);
    let facts = messages[messages.len() - 3]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(facts.contains("adjust_volume") && facts.contains("succeeded") && facts.contains("-5"));
    let unknown = fixture_input(&resolve_named("after_unknown_dispatch"), "继续").unwrap();
    assert!(
        unknown.begin_round(json!({}))[3]
            .to_string()
            .contains("unknown")
    );
    let clarification = fixture_input(&resolve_named("clarifying_browser"), "右屏那个").unwrap();
    assert!(
        clarification.begin_round(json!({}))[3]
            .to_string()
            .contains("是第三个工作区")
    );
}

#[test]
fn advertised_fixture_queries_have_real_readonly_response_handlers_and_not_full_catalog() {
    let corpus = corpus();
    let scenario = resolve(&corpus, "standard", &mut BTreeSet::new()).unwrap();
    let ctx = context(&scenario).unwrap();
    let responses = fixture_responses(&scenario, &ctx);
    assert_eq!(responses["get_audio_recording_status"]["finalizing"], false);
    assert!(
        responses["get_audio_recording_status"]
            .get("finalized")
            .is_none()
    );
    for name in responses["get_capabilities"]["queries"].as_array().unwrap() {
        assert!(
            responses.get(name.as_str().unwrap()).is_some(),
            "advertised query must have a responder: {name}"
        );
    }
    assert!(
        responses["get_capabilities"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "get_recording_status")
    );
    assert!(
        !responses["get_capabilities"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "get_clipboard")
    );
    assert!(
        responses["get_execution_context"]
            .get("available_queries")
            .is_none()
    );
    assert_eq!(
        ctx["available_queries"],
        responses["get_capabilities"]["queries"]
    );
}
