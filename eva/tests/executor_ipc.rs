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
        "level_receipt",
        "level_unknown",
        "level_wrong_receipt",
        "level_multi_stop",
        "level_multi_success",
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
        let status_queries = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (s, d, q) = (stop.clone(), dispatches.clone(), queries.clone());
        let receipts = status_queries.clone();
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
                            if mode.starts_with("level_") {
                                json!({"success":true,"data":{"operation_id":request["args"]["operation_id"],"status":"accepted","verified":{"completion":"awaiting_control_readback"}}})
                            } else if mode == "second_failed" && count == 2 {
                                json!({"success":false,"error":"target_gone"})
                            } else {
                                json!({"success":true,"data":{"status":"succeeded","verified":{"minimized":true}}})
                            }
                        } else if request["query"] == "get_execution_status" {
                            let receipt_count = {
                                let mut records = receipts.lock().unwrap();
                                records.push(request.clone());
                                records
                                    .iter()
                                    .filter(|record| {
                                        record["args"]["operation_id"]
                                            == request["args"]["operation_id"]
                                    })
                                    .count()
                            };
                            if mode.starts_with("level_") {
                                let status = match mode.as_str() {
                                    "level_unknown" => "unknown",
                                    "level_wrong_receipt" => "succeeded",
                                    "level_multi_stop" => "failed",
                                    _ if receipt_count == 1 => "accepted",
                                    _ => "succeeded",
                                };
                                let operation_id = if mode == "level_wrong_receipt" {
                                    json!("different-operation")
                                } else {
                                    request["args"]["operation_id"].clone()
                                };
                                json!({"success":true,"data":{"operation_id":operation_id,"status":status,"verified":{"read_back":{"domain":"volume","percent":45,"muted":false}}}})
                            } else {
                                json!({"success":true,"data":{"status":if mode=="late_receipt"{"succeeded"}else{"unknown"},"verified":{"minimized":true}}})
                            }
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
        let expected = if matches!(scenario, "second_failed" | "level_multi_success") {
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
        if scenario.starts_with("level_") {
            let records = dispatches.lock().unwrap();
            let receipts = status_queries.lock().unwrap();
            assert!(
                !receipts.is_empty(),
                "{scenario}: acceptance needs read-back"
            );
            for receipt in receipts.iter() {
                assert_eq!(receipt["args"]["wm_instance_id"], "test-wm");
                assert!(
                    records.iter().any(|record| {
                        record["args"]["operation_id"] == receipt["args"]["operation_id"]
                    }),
                    "{scenario}: status query must address an already dispatched operation"
                );
            }
            if matches!(scenario, "level_receipt" | "level_multi_success") {
                assert_eq!(
                    receipts.len(),
                    2 * count,
                    "must wait past accepted to a receipt"
                );
            } else {
                assert_eq!(
                    receipts.len(),
                    1,
                    "unknown or failure must not be polled into a retry"
                );
            }
        }
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
        let level = scenario.starts_with("level_");
        let action = if level {
            Action {
                name: "adjust_volume".into(),
                arguments: json!({"delta":-5}),
            }
        } else {
            Action {
                name: "set_window_minimized".into(),
                arguments: json!({"window_token":"original-window","enabled":true}),
            }
        };
        let actions = if matches!(scenario.as_str(), "level_multi_stop" | "level_multi_success") {
            vec![action.clone(), action]
        } else if level {
            vec![action]
        } else {
            vec![action.clone(), action.clone(), action]
        };
        let plan = FrozenPlan {
            plan_hash: "123456789abcdef".into(),
            wm_instance_id: "test-wm".into(),
            disposition: Disposition::Execute,
            actions,
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
            store.clone(),
            &config,
        )
        .await
        .unwrap();
        if level {
            let expected_status = match scenario.as_str() {
                "level_receipt" | "level_multi_success" => "succeeded",
                "level_multi_stop" => "failed",
                _ => "unknown",
            };
            assert_eq!(result["status"], expected_status, "{scenario}");
            let expected_steps = usize::from(scenario == "level_multi_success") + 1;
            assert_eq!(result["steps"].as_array().unwrap().len(), expected_steps);
            assert_eq!(
                result["unexecuted_steps"],
                usize::from(scenario == "level_multi_stop"),
                "the remaining plan must stop without a verified receipt"
            );
            let storage = store.lock().unwrap();
            assert_eq!(storage.ledger.operations.len(), expected_steps);
            if expected_status == "succeeded" {
                assert!(storage.ledger.operations.values().all(|record| {
                    record.state == jwm_eva_core::ledger::OperationState::Succeeded
                }));
                assert_eq!(result["steps"][0]["result"]["verified"]["read_back"]["percent"], 45);
            }
        } else if scenario == "reply_lost" {
            assert_eq!(result["status"], "unknown");
            assert_eq!(result["steps"].as_array().unwrap().len(), 1);
        } else {
            assert_eq!(result["status"], "partial");
            assert_eq!(result["steps"][0]["status"], "succeeded");
        }
    });
}
