//! Exercises the daemon's real serial executor across Unix sockets. The stand-in
//! injects transport/context failures; JWM's own fault suite verifies its guards.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
#[test]
fn serial_executor_retains_partial_truth_and_never_replays_unknown_mutations() {
    for scenario in [
        "second_failed",
        "context_failed",
        "reply_lost",
        "late_receipt",
    ] {
        let base = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("eva-executor-{}-{scenario}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("ipc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let dispatches = Arc::new(Mutex::new(Vec::<Value>::new()));
        let queries = Arc::new(AtomicUsize::new(0));
        let (s, d, q) = (stop.clone(), dispatches.clone(), queries.clone());
        let mode = scenario.to_string();
        let server = thread::spawn(move || {
            while !s.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        client
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut line = String::new();
                        if BufReader::new(client.try_clone().unwrap())
                            .read_line(&mut line)
                            .is_err()
                        {
                            continue;
                        }
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let response = if request["query"] == "get_execution_context" {
                            let n = q.fetch_add(1, Ordering::AcqRel) + 1;
                            if mode == "context_failed" && n == 3 {
                                json!({"success":false,"error":"context temporarily unavailable"})
                            } else {
                                json!({"success":true,"data":{"wm_instance_id":"test-wm","assistant_permit":{"assistant_session_id":"test-session","generation":1},"session_unlocked":true,"focus_token":"original-window","tag_count":9,"monitors":[{"monitor_token":"monitor","monitor_ref":"m1","visible_tag_mask":1,"layout":"tile"}],"windows":[{"target_token":"original-window","window_ref":"w1","monitor_token":"monitor","tag_mask":1,"minimized":!d.lock().unwrap().is_empty()}],"workspaces":[]}})
                            }
                        } else if request["command"] == "guarded_execute" {
                            let count = {
                                let mut records = d.lock().unwrap();
                                records.push(request.clone());
                                records.len()
                            };
                            if mode == "reply_lost" {
                                continue;
                            }
                            if mode == "second_failed" && count == 2 {
                                json!({"success":false,"error":"target_gone"})
                            } else {
                                json!({"success":true,"data":{"status":"succeeded","verified":{"minimized":true}}})
                            }
                        } else if request["query"] == "get_execution_status" {
                            json!({"success":true,"data":{"status":if mode=="late_receipt"{"succeeded"}else{"unknown"},"verified":{"minimized":true}}})
                        } else {
                            panic!("unexpected IPC request: {request}")
                        };
                        let _ = writeln!(client, "{response}");
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "executor_child", "--nocapture"])
            .env("JWM_SOCKET", &socket)
            .env("EVA_EXECUTOR_CHILD", scenario)
            .env("EVA_EXECUTOR_TEST_DIR", &dir)
            .output()
            .unwrap();
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let count = dispatches.lock().unwrap().len();
        let expected = if scenario == "second_failed" {
            2
        } else if scenario == "late_receipt" {
            0
        } else {
            1
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            child.status.success(),
            "{scenario}: {}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(
            count, expected,
            "no mutation may be replayed, and later steps must stop"
        );
    }
}
#[test]
fn executor_child() {
    let Ok(scenario) = std::env::var("EVA_EXECUTOR_CHILD") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        use jwm_eva_core::{
            actions::{Disposition, FrozenPlan},
            protocol::{Action, AssistantPermit},
        };
        let mut config = jwm_eva::config::Config::default();
        config.undo.enabled = false;
        let store = Arc::new(Mutex::new(
            jwm_eva::store::Store::open_at(
                std::path::PathBuf::from(std::env::var_os("EVA_EXECUTOR_TEST_DIR").unwrap())
                    .join("state"),
            )
            .unwrap(),
        ));
        if scenario=="late_receipt" {
            use jwm_eva_core::ledger::{OperationRecord,OperationState};
            {
                let mut state=store.lock().unwrap();
                for number in 0..21 {
                    let id=format!("late-{number:02}");
                    state.ledger.record_intent(OperationRecord{operation_id:id.clone(),plan_hash:"old-plan".into(),wm_instance_id:"test-wm".into(),action:Action{name:"set_window_minimized".into(),arguments:json!({"window_token":"original-window","minimized":true})},state:OperationState::Dispatched,result:None,created_ms:1}).unwrap();
                    state.ledger.record_result(&id,OperationState::Unknown,json!({"status":"unknown"})).unwrap();
                }
                state.save().unwrap();
            }
            for _ in 0..3 {jwm_eva::executor::reconcile(store.clone()).await.unwrap();}
            assert!(store.lock().unwrap().ledger.operations.values().all(|record|record.state==OperationState::Succeeded),"read-only reconciliation must accept old real receipts beyond the first sixteen");
            return;
        }
        let action = Action {
            name: "set_window_minimized".into(),
            arguments: json!({"window_token":"original-window","enabled":true}),
        };
        let plan = FrozenPlan {
            plan_hash: "123456789abcdef".into(),
            wm_instance_id: "test-wm".into(),
            disposition: Disposition::Execute,
            actions: vec![action.clone(), action.clone(), action],
            question: None,
            reason: None,
            requires_confirmation: false,
        };
        let result = jwm_eva::executor::execute(
            &plan,
            &AssistantPermit {
                assistant_session_id: "test-session".into(),
                generation: 1,
            },
            None,
            store,
            &config,
        )
        .await
        .unwrap();
        if scenario == "reply_lost" {
            assert_eq!(result["status"], "unknown");
            assert_eq!(result["steps"].as_array().unwrap().len(), 1);
        } else {
            assert_eq!(result["status"], "partial");
            assert_eq!(result["steps"][0]["status"], "succeeded");
        }
    });
}
