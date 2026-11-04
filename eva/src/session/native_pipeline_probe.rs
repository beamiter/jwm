// Opt-in production complete_turn probe. Mount as session::tests child module.
// A synthetic loopback provider is explicit; native context and dispatch are
// real. This is not a paid-provider, microphone, or user-desktop validation.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const CORE_SHA: &str = "a49a539deceb61e24b515f932daea1b2fac6cce37613f1a37752865daeb8cf5e";
const TOOL_SHA: &str = "8d1c04d427228f02b32b0c35451bf5180b1261d8759699a1d45234958698929c";
fn sha(path: &Path) -> String {
    Sha256::digest(std::fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn owned_dir(path: &Path) {
    let m = std::fs::symlink_metadata(path).unwrap();
    assert!(m.is_dir());
    assert_eq!(m.uid(), unsafe { libc::geteuid() });
    assert_eq!(m.permissions().mode() & 0o777, 0o700);
}
fn process_identity(pid: u64, ticks: &Value) {
    assert!(pid > 1 && pid <= i32::MAX as u64 && !matches!(pid, 4281 | 4353));
    let m = std::fs::metadata(format!("/proc/{pid}")).unwrap();
    assert_eq!(m.uid(), unsafe { libc::geteuid() });
    assert_eq!(
        ticks.as_str(),
        Some(
            crate::lifecycle::process_start_ticks(pid as u32)
                .unwrap()
                .to_string()
                .as_str()
        )
    );
}
struct Fixture {
    root: PathBuf,
    marker: Value,
}
impl Fixture {
    fn open() -> Self {
        let supplied = PathBuf::from(
            std::env::var_os("JWM_NATIVE_PIPELINE_FIXTURE")
                .expect("explicit owned private manifest required"),
        );
        assert!(
            !std::fs::symlink_metadata(&supplied)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let root = supplied.canonicalize().unwrap();
        owned_dir(&root);
        assert_eq!(root.parent(), Some(Path::new("/tmp")));
        assert!(
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("jwm-native-pipeline-")
        );
        let marker_path = root.join("owned.json");
        let m = std::fs::symlink_metadata(&marker_path).unwrap();
        assert!(m.is_file());
        assert_eq!(m.uid(), unsafe { libc::geteuid() });
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        let marker: Value = serde_json::from_slice(&std::fs::read(marker_path).unwrap()).unwrap();
        let fixture = Self { root, marker };
        fixture.check();
        fixture
    }
    fn check(&self) {
        owned_dir(&self.root);
        let m = &self.marker;
        assert_eq!(m["root_dir"].as_str(), self.root.to_str());
        assert_eq!(
            m["owner_pid"].as_u64(),
            Some(unsafe { libc::getppid() } as u64)
        );
        process_identity(m["owner_pid"].as_u64().unwrap(), &m["owner_start_ticks"]);
        assert_eq!(m["synthetic_provider_scope"], true);
        assert_eq!(
            m["display"].as_str(),
            Some(std::env::var("DISPLAY").unwrap().as_str())
        );
        assert!(m["display"].as_str().unwrap().starts_with(':'));
        for (key, leaf) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
        ] {
            let expected = self.root.join(leaf);
            owned_dir(&expected);
            assert_eq!(std::env::var_os(key).map(PathBuf::from), Some(expected));
        }
        let socket = self.root.join("runtime/jwm-ipc.sock");
        assert_ne!(socket, PathBuf::from("/run/user/1000/jwm-ipc.sock"));
        assert_eq!(
            std::env::var_os("JWM_SOCKET").map(PathBuf::from),
            Some(socket.clone())
        );
        assert_eq!(m["socket"].as_str(), socket.to_str());
        let meta = std::fs::symlink_metadata(&socket).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        let wm = m["wm_pid"].as_u64().unwrap();
        process_identity(wm, &m["wm_start_ticks"]);
        assert_eq!(m["core_sha256"], CORE_SHA);
        assert_eq!(m["tool_sha256"], TOOL_SHA);
        assert_eq!(sha(Path::new(m["core_path"].as_str().unwrap())), CORE_SHA);
        assert_eq!(sha(Path::new(m["tool_path"].as_str().unwrap())), TOOL_SHA);
        assert_eq!(sha(Path::new(&format!("/proc/{wm}/exe"))), CORE_SHA);
        assert!(matches!(
            m["scenario"].as_str(),
            Some("workspace_followup" | "move_follow" | "incomplete")
        ));
    }
    fn check_context(&self, c: &Value) {
        self.check();
        assert_eq!(c["wm_pid"], self.marker["wm_pid"]);
        assert_eq!(c["wm_start_ticks"], self.marker["wm_start_ticks"]);
        assert_eq!(c["assistant_permit"], self.marker["permit"]);
        assert_eq!(c["session_unlocked"], true);
        for (_, window) in self.marker["windows"].as_object().unwrap() {
            process_identity(window["pid"].as_u64().unwrap(), &window["start_ticks"]);
            let rows = c["windows"].as_array().unwrap();
            let mut found = rows
                .iter()
                .filter(|w| w["target_token"] == window["target_token"]);
            let actual = found.next().expect("owned window remains same instance");
            assert!(found.next().is_none());
            assert_eq!(actual["pid"], window["pid"]);
            assert_eq!(actual["native_window_id"], window["native_window_id"]);
        }
    }
    async fn snapshot(&self) -> Value {
        self.check();
        let c = query("get_execution_context", json!({})).await.unwrap();
        self.check_context(&c);
        c
    }
    async fn input_context(&self) -> Value {
        let first = self.snapshot().await;
        let mut cache = crate::context::ContextStore::default();
        cache.observe(first, Instant::now());
        tokio::time::sleep(Duration::from_millis(2)).await;
        let onset = Instant::now();
        let binding = cache.bind_onset(onset).unwrap();
        let committed = crate::context::monotonic_ns().unwrap();
        let latest = self.snapshot().await;
        let mut c = cache
            .validate_commit(&binding, committed, latest)
            .expect("native focus history proves committed utterance binding");
        crate::context::planning_monitor_scope(&mut c).unwrap();
        c["available_actions"] = json!(runtime_actions().await.unwrap());
        self.check_context(&c);
        c
    }
}
fn candidate(call: &str, actions: Value, new_task: bool) -> Value {
    let mut args = json!({"session_intent":"continue","disposition":"execute","actions":actions,"question":null,"reason":null});
    if new_task {
        args["task_intent"] = json!("new");
    }
    tool("eva_submit_plan", call, args)
}
fn review(call: &str, steps: Value) -> Value {
    let mut r = tool(
        "eva_review_allow",
        call,
        json!({"session_intent":"continue","steps":steps}),
    );
    r["semantic_review"] = json!(true);
    r
}
#[allow(clippy::too_many_arguments)]
fn row(
    name: &str,
    scope: &str,
    windows: Value,
    monitors: Value,
    tags: Value,
    wb: &str,
    wq: &str,
    mb: &str,
    mq: &str,
) -> Value {
    json!({"name":name,"target_scope":scope,"window_refs":windows,"window_basis":wb,"window_quote":wq,"monitor_refs":monitors,"monitor_basis":mb,"monitor_quote":mq,"tag_numbers":tags,"session_refs":[],"session_quote":"","device_ids":[],"device_quote":"","application_ids":[],"application_quote":"","effect_quote":""})
}
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    fixture: &Fixture,
    config: &Config,
    data: &TestStore,
    actor: &mut Trace,
    state: &mut Session,
    ws: &mut Realtime,
    tx: &mpsc::Sender<Update>,
    admission: &mut PlanAdmission,
    epoch: u64,
    start: Instant,
    text: &str,
    actions: Value,
    expect_success: bool,
) -> Value {
    let context = fixture.input_context().await;
    assert!(state.speech_started(epoch, elapsed(start)));
    actor.input.set_text(text).unwrap();
    assert!(state.input_committed(epoch, state.turn_epoch, elapsed(start)));
    actor.labels = json!({"generation":state.permit.as_ref().unwrap().generation,"session_epoch":epoch,"turn_id":state.turn_epoch});
    actor.rounds = Some(vec![]);
    let semantic: jwm_eva_core::actions::Plan = serde_json::from_value(
        json!({"disposition":"execute","actions":actions,"question":null,"reason":null}),
    )
    .unwrap();
    let canonical = validate_plan(&semantic, &context).unwrap();
    assert!(!canonical.requires_confirmation);
    let ledger_before = crate::store::ledger(&data.store).await.unwrap();
    let mut feedback = LocalFeedback::default();
    let permit = state.permit.clone().unwrap();
    let completed = tokio::time::timeout(
        Duration::from_secs(25),
        complete_turn(
            config,
            &permit,
            tx,
            data.store.clone(),
            ws,
            actor,
            state,
            &context,
            false,
            admission,
            epoch,
            start,
            &mut feedback,
        ),
    )
    .await
    .expect("bounded real production turn");
    let ledger_after = crate::store::ledger(&data.store).await.unwrap();
    if expect_success {
        completed.unwrap();
        assert_eq!(feedback.status, Some(ResultStatus::Succeeded), "rounds={:?}", actor.rounds);
        assert_eq!(state.state, State::FollowUp);
        assert!(!actor.input.has_current());
        let added = ledger_after
            .operations
            .values()
            .filter(|r| !ledger_before.operations.contains_key(&r.operation_id))
            .collect::<Vec<_>>();
        assert_eq!(added.len(), canonical.actions.len());
        let actual_hash = actor.labels["plan_hash"].as_str().unwrap();
        assert_ne!(actual_hash, canonical.plan_hash);
        for (index, action) in canonical.actions.iter().enumerate() {
            let r = added
                .iter()
                .find(|r| r.action == *action)
                .expect("original action unchanged through prepare and dispatch");
            assert_eq!(r.plan_hash, actual_hash);
            assert_eq!(r.wm_instance_id, canonical.wm_instance_id);
            assert_eq!(r.state, jwm_eva_core::ledger::OperationState::Succeeded);
            assert_eq!(r.result.as_ref().unwrap()["status"], "succeeded");
            assert!(
                r.operation_id
                    .ends_with(&format!("-{index}-{}", &actual_hash[..8]))
            );
        }
        assert!(
            actor
                .rounds
                .as_ref()
                .unwrap()
                .iter()
                .any(|r| r["candidate_hash"] == canonical.plan_hash && r["outcome"] == "proved"),
            "actual native proof must retain canonical original candidate"
        );
    } else {
        assert!(
            completed.is_err(),
            "incomplete provider response must not reach execution"
        );
        assert_eq!(json!(ledger_before), json!(ledger_after));
    }
    let native_after = fixture.snapshot().await;
    json!({"text":text,"captured_context":context,"canonical_frozen":canonical,"actual_prepared_hash":actor.labels["plan_hash"],"rounds":actor.rounds,"ledger_before":ledger_before,"ledger_after":ledger_after,"feedback":format!("{:?}",feedback),"workspace_history_present":actor.input.latest_verified_workspace().is_some(),"native_after":native_after})
}

#[tokio::test]
#[ignore = "requires explicit owned private native manifest and loopback provider; never user IPC or genuine credentials"]
async fn owned_native_complete_turn_pipeline() {
    let fixture = Fixture::open();
    let scenario = fixture.marker["scenario"].as_str().unwrap();
    let output = PathBuf::from(
        std::env::var_os("JWM_NATIVE_PIPELINE_OUTPUT").expect("explicit private output required"),
    );
    assert_eq!(output, fixture.root.join("results/probe.json"));
    owned_dir(output.parent().unwrap());
    assert!(!output.exists());
    let initial = fixture.input_context().await;
    let monitor_token = initial["selected_monitor_token"].as_str().unwrap();
    let monitor = initial["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["monitor_token"] == monitor_token)
        .unwrap();
    assert_eq!(monitor["visible_tag_mask"], 1);
    let mr = monitor["monitor_ref"].as_str().unwrap();
    let b = &fixture.marker["windows"]["B"];
    assert_eq!(initial["focus_token"], b["target_token"]);
    let window = initial["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["target_token"] == b["target_token"])
        .unwrap();
    let wr = window["window_ref"].as_str().unwrap();
    assert_eq!(initial["speech_focus_ref"], wr);
    let switch = json!([{"name":"switch_tag","arguments":{"monitor_ref":mr,"tag_number":3}}]);
    let layout = json!([{"name":"set_layout","arguments":{"monitor_ref":mr,"tag_number":3,"layout":"scrolling"}}]);
    let chain = json!([{"name":"move_window_to_tag","arguments":{"window_ref":wr,"tag_number":2}},{"name":"switch_tag","arguments":{"monitor_ref":mr,"tag_number":2}}]);
    let switch_review = review(
        "switch-review",
        json!([row(
            "switch_tag",
            "monitor",
            json!([]),
            json!([mr]),
            json!([3]),
            "none",
            "",
            "selected_workspace",
            "工作区"
        )]),
    );
    let scripts = match scenario {
        "workspace_followup" => {
            let mut follow = tool(
                "eva_review_clarify",
                "follow-review",
                json!({"question":"要切换哪个工作区的布局？"}),
            );
            follow["semantic_review"] = json!(true);
            vec![
                candidate("switch", switch.clone(), false),
                switch_review,
                candidate("layout", layout.clone(), true),
                follow,
            ]
        }
        "move_follow" => vec![
            candidate("chain", chain.clone(), false),
            review(
                "chain-review",
                json!([
                    row(
                        "move_window_to_tag",
                        "window",
                        json!([wr]),
                        json!([]),
                        json!([2]),
                        "current",
                        "这个窗口",
                        "none",
                        ""
                    ),
                    row(
                        "switch_tag",
                        "monitor",
                        json!([]),
                        json!([mr]),
                        json!([2]),
                        "none",
                        "",
                        "prior_step",
                        "第二个 tag"
                    )
                ]),
            ),
        ],
        "incomplete" => {
            let mut p = candidate("incomplete", switch.clone(), false);
            p["status"] = json!("incomplete");
            vec![p]
        }
        _ => unreachable!(),
    };
    // Every review is authored above and marked explicit, disabling replay's
    // legacy automatic candidate-review echo. Metadata echo only binds requests.
    assert!(
        scripts
            .iter()
            .filter(|p| p["name"] != "eva_submit_plan")
            .all(|p| p["semantic_review"] == true)
    );
    let (mut ws, server) = replay(scripts.clone()).await;
    let data = TestStore::new();
    assert!(!data.dir.starts_with(Path::new("/home/ubuntu/.local")));
    let mut config = Config::default();
    config.interaction.feedback = "text".into();
    config.interaction.half_duplex = true;
    config.audio.playback_tail_ms = 0;
    config.execution.max_schema_repair_attempts = 0;
    config.interaction.follow_up_ms = 60_000;
    let permit: AssistantPermit = serde_json::from_value(fixture.marker["permit"].clone()).unwrap();
    let start = Instant::now();
    let mut state = configured(&config);
    let epoch = state.activate(permit.clone(), 0).unwrap();
    assert!(state.ready(epoch, elapsed(start)));
    let mut actor = trace();
    actor.native_snapshot_double = false;
    actor.input = crate::planning_input::FrozenInput::default();
    let mut admission = PlanAdmission::default();
    let (tx, mut rx) = mpsc::channel(64);
    let drain = tokio::spawn(async move {
        let mut n = 0usize;
        while rx.recv().await.is_some() {
            n += 1;
        }
        n
    });
    let mut turns = vec![];
    match scenario {
        "workspace_followup" => {
            turns.push(
                run_turn(
                    &fixture,
                    &config,
                    &data,
                    &mut actor,
                    &mut state,
                    &mut ws,
                    &tx,
                    &mut admission,
                    epoch,
                    start,
                    "切到第三个工作区",
                    switch,
                    true,
                )
                .await,
            );
            assert!(
                actor.input.latest_verified_workspace().is_some(),
                "production raw result must mint typed history after prepared hash"
            );
            turns.push(
                run_turn(
                    &fixture,
                    &config,
                    &data,
                    &mut actor,
                    &mut state,
                    &mut ws,
                    &tx,
                    &mut admission,
                    epoch,
                    start,
                    "再换成 scrolling",
                    layout,
                    true,
                )
                .await,
            );
        }
        "move_follow" => turns.push(
            run_turn(
                &fixture,
                &config,
                &data,
                &mut actor,
                &mut state,
                &mut ws,
                &tx,
                &mut admission,
                epoch,
                start,
                "把这个窗口移到第二个 tag，然后切过去",
                chain,
                true,
            )
            .await,
        ),
        "incomplete" => turns.push(
            run_turn(
                &fixture,
                &config,
                &data,
                &mut actor,
                &mut state,
                &mut ws,
                &tx,
                &mut admission,
                epoch,
                start,
                "切到第三个工作区",
                switch,
                false,
            )
            .await,
        ),
        _ => unreachable!(),
    }
    let messages = finish(ws, server).await;
    assert_eq!(
        messages
            .iter()
            .filter(|v| v["type"] == "response.create")
            .count(),
        scripts.len()
    );
    assert!(
        messages
            .iter()
            .filter(|v| v["type"] == "response.create")
            .all(|v| v["response"]["output_modalities"] == json!(["text"]))
    );
    drop(tx);
    let update_count = drain.await.unwrap();
    let final_context = fixture.snapshot().await;
    let final_monitor = final_context["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["monitor_token"] == monitor_token)
        .unwrap();
    match scenario {
        "workspace_followup" => {
            assert_eq!(final_monitor["visible_tag_mask"], 4);
            assert_eq!(
                jwm_eva_core::actions::normalize_layout(final_monitor["layout"].as_str().unwrap()),
                Some("scrolling")
            );
        }
        "move_follow" => {
            assert_eq!(final_monitor["visible_tag_mask"], 2);
            let w = final_context["windows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["target_token"] == b["target_token"])
                .unwrap();
            assert_eq!(w["tag_mask"], 2);
            assert_eq!(w["pid"], b["pid"]);
            assert_eq!(w["native_window_id"], b["native_window_id"]);
        }
        "incomplete" => {
            assert_eq!(final_monitor["visible_tag_mask"], 1);
            assert!(data.store.lock().unwrap().ledger.operations.is_empty());
        }
        _ => unreachable!(),
    }
    let store = data.store.lock().unwrap();
    let result = json!({"status":"passed","scenario":scenario,"scope":"production complete_turn with loopback scripted provider; real private native core","synthetic_provider":true,"genuine_provider_calls":0,"cost_records_are_synthetic":true,"audio":false,"native_before":initial,"native_after":final_context,"turns":turns,"scripted_provider_frames":scripts,"received_provider_frames":messages,"drained_updates":update_count,"synthetic_store":{"directory":data.dir,"ledger":store.ledger,"usage":store.usage},"compiled_source_revision":option_env!("JWM_EVA_SOURCE_REVISION").unwrap_or("unknown")});
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .unwrap();
    f.write_all(serde_json::to_string_pretty(&result).unwrap().as_bytes())
        .unwrap();
}
