//! Opt-in owned-X11 fixture adapter. Never part of the release daemon.
use crate::{config::Config, store::Store};
use jwm_eva_core::{
    actions::{FrozenPlan, Plan},
    protocol::{AssistantPermit, ExpectedContext, GuardedRequest},
};
use serde_json::{Value, json};
use std::{
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[tokio::test]
#[ignore = "requires explicit owned private X11 fixture, real UI confirmation, and supplied core ELF; no provider/mic"]
async fn owned_native_directed_close() {
    let root = PathBuf::from(
        std::env::var_os("JWM_NATIVE_CLOSE_FIXTURE").expect("explicit owned fixture required"),
    )
    .canonicalize()
    .unwrap();
    assert!(
        root.file_name()
            .unwrap()
            .as_bytes()
            .starts_with(b"jwm-native-close-")
    );
    let metadata = std::fs::symlink_metadata(&root).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    let marker: Value =
        serde_json::from_slice(&std::fs::read(root.join("owned.json")).unwrap()).unwrap();
    assert_eq!(
        marker["owner_pid"].as_u64(),
        Some(unsafe { libc::getppid() } as u64)
    );
    assert_eq!(
        marker["display"].as_str(),
        Some(std::env::var("DISPLAY").unwrap().as_str())
    );
    assert_eq!(
        std::env::var_os("HOME").map(PathBuf::from),
        Some(root.join("home"))
    );
    assert_eq!(
        std::env::var_os("JWM_SOCKET").map(PathBuf::from),
        Some(root.join("runtime/jwm-ipc.sock"))
    );
    assert_eq!(marker["synthetic_provider_scope"], true);
    let input = PathBuf::from(std::env::var_os("JWM_NATIVE_CLOSE_INPUT").unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(input.parent(), Some(root.join("cases").as_path()));
    let payload: Value = serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
    let context = &payload["captured_context"];
    let semantic: Plan = serde_json::from_value(payload["semantic_plan"].clone()).unwrap();
    assert!(
        semantic
            .actions
            .iter()
            .all(|action| action.name == "killclient")
    );
    let frozen = jwm_eva_core::actions::validate_plan(&semantic, context).unwrap();
    assert!(frozen.requires_confirmation);
    let mut result = if payload["mode"] == "freeze" {
        json!({"frozen_plan":frozen})
    } else {
        let supplied: FrozenPlan = serde_json::from_value(payload["frozen_plan"].clone()).unwrap();
        assert_eq!(json!(supplied), json!(frozen));
        let permit: AssistantPermit = serde_json::from_value(payload["permit"].clone()).unwrap();
        assert_eq!(context["assistant_permit"], json!(permit));
        let confirmation = payload["confirmation_token"].as_str().map(str::to_owned);
        let store_dir = root.join("state").join(format!(
            "synthetic-native-{}",
            input.file_stem().unwrap().to_string_lossy()
        ));
        let store = Arc::new(Mutex::new(Store::open_at(store_dir).unwrap()));
        let result = crate::executor::execute_observed_with_context(
            &frozen,
            &permit,
            confirmation.clone(),
            store.clone(),
            &Config::default(),
            None,
            None,
            context,
        )
        .await
        .unwrap();
        if payload["settle_accepted"] == true {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            loop {
                crate::executor::reconcile(store.clone()).await.unwrap();
                let pending = store
                    .lock()
                    .unwrap()
                    .ledger
                    .operations
                    .values()
                    .any(|record| {
                        matches!(
                            record.state,
                            jwm_eva_core::ledger::OperationState::Dispatched
                                | jwm_eva_core::ledger::OperationState::Accepted
                                | jwm_eva_core::ledger::OperationState::Unknown
                        )
                    });
                if !pending {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "owned native close never reached a terminal receipt"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        let state = store.lock().unwrap();
        assert!(
            state.usage.records.is_empty(),
            "native fixture must never create model billing records"
        );
        let requests = state
            .ledger
            .operations
            .values()
            .map(|operation| {
                let mut request = GuardedRequest {
                    protocol_version: 1,
                    wm_instance_id: operation.wm_instance_id.clone(),
                    assistant_permit: permit.clone(),
                    operation_id: operation.operation_id.clone(),
                    request_digest: String::new(),
                    confirmation_token: confirmation.clone(),
                    expected_context: ExpectedContext {
                        session_unlocked: true,
                        monitor_token: operation.action.arguments["monitor_token"]
                            .as_str()
                            .map(str::to_owned),
                        focus_token: None,
                        visible_tag_mask: None,
                    },
                    action: operation.action.clone(),
                };
                request.seal();
                request
            })
            .collect::<Vec<_>>();
        json!({"result":result,"requests_for_historical_replay":requests,"synthetic_local_ledger":state.ledger,"provider_calls":0,"billing_records":0})
    };
    result["compiled_source_revision"] =
        json!(option_env!("JWM_EVA_SOURCE_REVISION").unwrap_or("unknown"));
    result["compiled_guard_fingerprint"] = json!(jwm_eva_core::protocol::digest(
        &json!({"probe":include_str!("native_close_probe.rs"),"executor":include_str!("executor.rs"),"registry":include_str!("../../crates/jwm_eva_core/src/actions/registry.rs"),"validate":include_str!("../../crates/jwm_eva_core/src/actions/validate.rs"),"protocol":include_str!("../../crates/jwm_eva_core/src/protocol.rs")})
    ));
    let output = PathBuf::from(std::env::var_os("JWM_NATIVE_CLOSE_OUTPUT").unwrap());
    assert_eq!(output.parent(), Some(root.join("results").as_path()));
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)
        .unwrap();
    file.write_all(serde_json::to_string_pretty(&result).unwrap().as_bytes())
        .unwrap();
}
