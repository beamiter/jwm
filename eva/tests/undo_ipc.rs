//! Real Unix transport regression for compensation and the latest-mutation frontier.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
fn desktop() -> Value {
    json!({"protocol_version":1,"wm_instance_id":"undo-wm","assistant_permit":{"assistant_session_id":"undo-session","generation":1},"session_unlocked":true,"selected_monitor_token":"m1","focus_token":"w0","tag_count":9,
        "monitors":[{"monitor_token":"m1","visible_tag_mask":1},{"monitor_token":"m2","visible_tag_mask":4}],
        "windows":[{"target_token":"w0","monitor_token":"m1","tag_mask":1,"minimized":false},{"target_token":"w-min","monitor_token":"m1","tag_mask":1,"minimized":false},{"target_token":"w-move","monitor_token":"m1","tag_mask":3,"minimized":false},{"target_token":"w-tags","monitor_token":"m1","tag_mask":1,"minimized":false},{"target_token":"w-focus","monitor_token":"m2","tag_mask":4,"minimized":false}],"workspaces":[]})
}
#[test]
fn compensation_restores_targets_and_never_crosses_newer_mutation() {
    for scenario in [
        "compensate",
        "chained",
        "readonly",
        "nonreversible",
        "unknown",
        "manual_focus",
    ] {
        let base = if std::path::Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("eva-undo-{}-{scenario}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.join("ipc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(desktop()));
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (server_stop, server_state, server_requests) =
            (stop.clone(), state.clone(), requests.clone());
        let mode = scenario.to_owned();
        let server = std::thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        client
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut line = String::new();
                        if BufReader::new(client.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap_or(0)
                            == 0
                        {
                            continue;
                        }
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let mut context = server_state.lock().unwrap();
                        let reply = if request["query"] == "get_execution_context" {
                            json!({"success":true,"data":*context})
                        } else if request["query"] == "fixture_manual_focus" {
                            context["focus_token"] = json!("w0");
                            context["selected_monitor_token"] = json!("m1");
                            json!({"success":true,"data":{}})
                        } else if request["command"] == "guarded_execute" {
                            let wire: &Value = &request["args"];
                            let action: jwm_eva_core::protocol::Action =
                                serde_json::from_value(wire["action"].clone()).unwrap();
                            jwm_eva_core::actions::validate_execution_action(&action).unwrap();
                            assert_eq!(wire["wm_instance_id"], "undo-wm");
                            assert_eq!(wire["assistant_permit"], context["assistant_permit"]);
                            server_requests.lock().unwrap().push(wire.clone());
                            let args = &action.arguments;
                            if let Some(expected) = args["expected_state"].as_object() {
                                for (key, value) in expected {
                                    let actual = if key == "focus_token"
                                        || key == "selected_monitor_token"
                                    {
                                        context[key].clone()
                                    } else if let Some(token) = expected.get("window_token") {
                                        let w = context["windows"]
                                            .as_array()
                                            .unwrap()
                                            .iter()
                                            .find(|w| w["target_token"] == *token)
                                            .unwrap();
                                        if key == "window_token" {
                                            w["target_token"].clone()
                                        } else {
                                            w[key].clone()
                                        }
                                    } else {
                                        let m = context["monitors"]
                                            .as_array()
                                            .unwrap()
                                            .iter()
                                            .find(|m| {
                                                m["monitor_token"] == expected["monitor_token"]
                                            })
                                            .unwrap();
                                        m[key].clone()
                                    };
                                    assert_eq!(
                                        actual, *value,
                                        "compensation must freeze its observed post-state"
                                    );
                                }
                            }
                            match action.name.as_str() {
                                "set_window_minimized" => {
                                    let w = context["windows"]
                                        .as_array_mut()
                                        .unwrap()
                                        .iter_mut()
                                        .find(|w| w["target_token"] == args["window_token"])
                                        .unwrap();
                                    w["minimized"] = args["minimized"].clone();
                                }
                                "move_window_to_monitor" => {
                                    let mask =
                                        args.get("restore_tag_mask").cloned().unwrap_or(json!(4));
                                    let w = context["windows"]
                                        .as_array_mut()
                                        .unwrap()
                                        .iter_mut()
                                        .find(|w| w["target_token"] == args["window_token"])
                                        .unwrap();
                                    w["monitor_token"] = args["monitor_token"].clone();
                                    w["tag_mask"] = mask;
                                }
                                "move_window_to_tags" => {
                                    let w = context["windows"]
                                        .as_array_mut()
                                        .unwrap()
                                        .iter_mut()
                                        .find(|w| w["target_token"] == args["window_token"])
                                        .unwrap();
                                    w["tag_mask"] = args["tag_mask"].clone();
                                }
                                "focus_window" => {
                                    let token = args["window_token"].clone();
                                    let m = context["windows"]
                                        .as_array()
                                        .unwrap()
                                        .iter()
                                        .find(|w| w["target_token"] == token)
                                        .unwrap()["monitor_token"]
                                        .clone();
                                    context["focus_token"] = token;
                                    context["selected_monitor_token"] = m;
                                }
                                "query_state" | "launch_application" => {}
                                other => panic!("unexpected action {other}"),
                            }
                            json!({"success":true,"data":{"status":if mode=="unknown"&&action.name=="launch_application"{"unknown"}else{"succeeded"},"verified":{}}})
                        } else {
                            panic!("unexpected request {request}")
                        };
                        let _ = writeln!(client, "{reply}");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "undo_child", "--nocapture"])
            .env("JWM_SOCKET", &socket)
            .env("EVA_UNDO_SCENARIO", scenario)
            .env("EVA_UNDO_DIR", &dir)
            .output()
            .unwrap();
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        let guarded_undos = requests
            .iter()
            .filter(|r| r["action"]["arguments"].get("expected_state").is_some())
            .count();
        assert!(
            child.status.success(),
            "{scenario}: {}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        if matches!(scenario, "nonreversible" | "unknown" | "manual_focus") {
            assert_eq!(guarded_undos, 0, "refusal must send no compensation");
        } else {
            assert_eq!(
                guarded_undos,
                if scenario == "compensate" {
                    4
                } else if scenario == "chained" {
                    2
                } else {
                    1
                }
            );
            assert_eq!(*state.lock().unwrap(), desktop());
        }
        drop(requests);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
#[test]
fn undo_child() {
    let Ok(scenario) = std::env::var("EVA_UNDO_SCENARIO") else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use jwm_eva_core::{
            actions::{Disposition, FrozenPlan},
            protocol::{Action, AssistantPermit},
        };
        let config = jwm_eva::config::Config::default();
        let storage = Arc::new(Mutex::new(
            jwm_eva::store::Store::open_at(
                std::path::PathBuf::from(std::env::var_os("EVA_UNDO_DIR").unwrap()).join("state"),
            )
            .unwrap(),
        ));
        let permit = AssistantPermit {
            assistant_session_id: "undo-session".into(),
            generation: 1,
        };
        let action = |name: &str, arguments: Value| Action {
            name: name.into(),
            arguments,
        };
        let mut actions = vec![action(
            "set_window_minimized",
            json!({"window_token":"w-min","minimized":true}),
        )];
        if scenario == "compensate" {
            actions.extend([
                action(
                    "move_window_to_monitor",
                    json!({"window_token":"w-move","monitor_token":"m2"}),
                ),
                action(
                    "move_window_to_tags",
                    json!({"window_token":"w-tags","tag_mask":5}),
                ),
                action("focus_window", json!({"window_token":"w-focus"})),
            ]);
        }
        if scenario == "chained" {
            actions = vec![
                action(
                    "move_window_to_tags",
                    json!({"window_token":"w-move","tag_mask":5}),
                ),
                action(
                    "move_window_to_monitor",
                    json!({"window_token":"w-move","monitor_token":"m2"}),
                ),
            ];
        }
        if scenario == "manual_focus" {
            actions = vec![action("focus_window", json!({"window_token":"w-focus"}))];
        }
        let make = |hash: &str, actions| FrozenPlan {
            plan_hash: hash.into(),
            wm_instance_id: "undo-wm".into(),
            disposition: Disposition::Execute,
            actions,
            question: None,
            reason: None,
            requires_confirmation: false,
        };
        let result = jwm_eva::executor::execute(
            &make("reversible-plan", actions),
            &permit,
            None,
            storage.clone(),
            &config,
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "succeeded");
        if scenario == "readonly" {
            jwm_eva::executor::execute(
                &make(
                    "read-plan",
                    vec![action("query_state", json!({"query":"get_status"}))],
                ),
                &permit,
                None,
                storage.clone(),
                &config,
            )
            .await
            .unwrap();
        }
        if matches!(scenario.as_str(), "nonreversible" | "unknown") {
            jwm_eva::executor::execute(
                &make(
                    "new-launch",
                    vec![action(
                        "launch_application",
                        json!({"application_id":"fixture","argv":["true"]}),
                    )],
                ),
                &permit,
                None,
                storage.clone(),
                &config,
            )
            .await
            .unwrap();
        }
        if scenario == "manual_focus" {
            jwm_eva::executor::query("fixture_manual_focus", json!({}))
                .await
                .unwrap();
        }
        let result = jwm_eva::executor::undo(&permit, storage.clone(), &config).await;
        if matches!(scenario.as_str(), "nonreversible" | "unknown") {
            assert_eq!(
                result.unwrap_err().to_string(),
                "latest_plan_not_reversible"
            );
        } else if scenario == "manual_focus" {
            assert_eq!(result.unwrap_err().to_string(), "undo_conflict");
        } else {
            assert_eq!(result.unwrap()["status"], "succeeded");
            assert!(storage.lock().unwrap().ledger.undo.is_empty());
        }
    });
}
