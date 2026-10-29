use crate::protocol::Action;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Dispatched,
    Accepted,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation_id: String,
    pub plan_hash: String,
    pub wm_instance_id: String,
    pub action: Action,
    pub state: OperationState,
    pub result: Option<Value>,
    pub created_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoStep {
    pub inverse: Action,
    pub after: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoPlan {
    pub plan_hash: String,
    #[serde(default)]
    pub previous_mutating_plan_hash: Option<String>,
    pub wm_instance_id: String,
    pub created_ms: u64,
    pub steps: Vec<UndoStep>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// Durable frontier: ordinary undo cannot cross a newer admitted mutation.
    #[serde(default)]
    pub last_mutating_plan_hash: Option<String>,
    #[serde(default)]
    mutation_frontier_initialized: bool,
    pub operations: BTreeMap<String, OperationRecord>,
    pub undo: Vec<UndoPlan>,
    #[serde(default)]
    pub terminal_times: BTreeMap<String, u64>,
}
impl Ledger {
    /// Persist this state successfully before any side effect is dispatched.
    pub fn record_intent(&mut self, record: OperationRecord) -> Result<(), String> {
        // Only terminal operations leave the ten-minute retention window.
        // In-flight/accepted/unknown tombstones never become replay permission.
        self.prune_terminal(record.created_ms);
        if self.operations.contains_key(&record.operation_id) {
            return Err("operation_id_already_recorded".into());
        }
        if self.operations.len() >= 4096 {
            return Err("ledger_capacity_exceeded".into());
        }
        if record.state != OperationState::Dispatched {
            return Err("intent_must_be_dispatched".into());
        }
        if is_mutating_action(&record.action) {
            self.mutation_frontier_initialized = true;
            self.last_mutating_plan_hash = Some(record.plan_hash.clone());
        }
        self.operations.insert(record.operation_id.clone(), record);
        Ok(())
    }
    pub fn prune_terminal(&mut self, now_ms: u64) -> usize {
        let before = self.operations.len();
        let terminal_times = &self.terminal_times;
        self.operations.retain(|id, record| {
            !matches!(
                record.state,
                OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
            ) || now_ms.saturating_sub(*terminal_times.get(id).unwrap_or(&record.created_ms))
                <= 600_000
        });
        self.terminal_times
            .retain(|id, _| self.operations.contains_key(id));
        before - self.operations.len()
    }
    pub fn record_result(
        &mut self,
        id: &str,
        state: OperationState,
        result: Value,
    ) -> Result<(), String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                duration.as_millis().min(u64::MAX as u128) as u64
            });
        self.record_result_at(id, state, result, now)
    }
    pub fn record_result_at(
        &mut self,
        id: &str,
        state: OperationState,
        result: Value,
        now_ms: u64,
    ) -> Result<(), String> {
        let record = self.operations.get_mut(id).ok_or("unknown_operation_id")?;
        if matches!(
            record.state,
            OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
        ) && state != record.state
        {
            return Err("terminal_result_conflict".into());
        }
        record.state = state;
        record.result = Some(result);
        if matches!(
            state,
            OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled
        ) {
            self.terminal_times.entry(id.into()).or_insert(now_ms);
        }
        Ok(())
    }
    /// Recovery must never replay dispatched operations. Read-only verification
    /// can replace Unknown later; no network completion is assumed.
    pub fn recover(&mut self) {
        for record in self.operations.values_mut() {
            if matches!(
                record.state,
                OperationState::Dispatched | OperationState::Accepted
            ) {
                record.state = OperationState::Unknown;
            }
        }
    }
    pub fn remember_undo(&mut self, plan: UndoPlan, now: u64) {
        self.undo
            .retain(|p| now.saturating_sub(p.created_ms) <= 600_000);
        if self.undo.len() >= 20 {
            self.undo.remove(0);
        }
        self.undo.push(plan);
    }
    pub fn latest_undo_plan(&self) -> Result<&UndoPlan, String> {
        let plan = self.undo.last();
        if let Some(latest) = self.last_mutating_plan_hash.as_deref()
            && plan.map(|plan| plan.plan_hash.as_str()) != Some(latest)
        {
            return Err("latest_plan_not_reversible".into());
        }
        plan.ok_or_else(|| "nothing_to_undo".into())
    }
    pub fn undo_candidate(
        &self,
        wm_instance: &str,
        now: u64,
        current: &[Value],
    ) -> Result<Vec<Action>, String> {
        let plan = self.latest_undo_plan()?;
        if plan.wm_instance_id != wm_instance || now.saturating_sub(plan.created_ms) > 600_000 {
            return Err("undo_expired".into());
        }
        if plan.steps.len() != current.len() {
            return Err("undo_conflict".into());
        }
        // One plan can modify the same window/tag more than once. Compare
        // the final writer of each related field now; each intermediate
        // post-state is still checked atomically before its inverse executes.
        let mut compared = std::collections::BTreeSet::new();
        for (index, (step, current)) in plan.steps.iter().zip(current).enumerate().rev() {
            if let (Some(expected), Some(actual)) = (step.after.as_object(), current.as_object()) {
                let scope = if let Some(token) = expected.get("window_token") {
                    format!("window:{token}")
                } else if expected.contains_key("focus_token")
                    || expected.contains_key("selected_monitor_token")
                {
                    "focus".into()
                } else if expected.contains_key("layout") {
                    format!(
                        "workspace:{}:{}",
                        step.after["monitor_token"], step.after["tag_mask"]
                    )
                } else if let Some(token) = expected.get("monitor_token") {
                    format!("monitor:{token}")
                } else {
                    format!("step:{index}")
                };
                for (field, value) in expected {
                    if compared.insert((scope.clone(), field.clone()))
                        && actual.get(field) != Some(value)
                    {
                        return Err("undo_conflict".into());
                    }
                }
            } else if step.after != *current {
                return Err("undo_conflict".into());
            }
        }
        Ok(plan.steps.iter().rev().map(|s| s.inverse.clone()).collect())
    }
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("ledger_permissions_not_private".into());
        }
        let mut text = String::new();
        OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|e| e.to_string())?
            .take(4 * 1024 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 4 * 1024 * 1024 {
            return Err("ledger_too_large".into());
        }
        let mut ledger: Self = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        // Upgrade older stores conservatively: an admitted mutation after the
        // last compensation record must never silently expose an older plan.
        if !ledger.mutation_frontier_initialized {
            ledger.last_mutating_plan_hash = ledger
                .operations
                .values()
                .filter(|record| is_mutating_action(&record.action))
                .max_by_key(|record| record.created_ms)
                .map(|record| record.plan_hash.clone());
            ledger.mutation_frontier_initialized = true;
        }
        ledger.recover();
        Ok(ledger)
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("ledger_parent_missing")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        let result = (|| {
            serde_json::to_writer(&mut file, self).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temporary, path).map_err(|e| e.to_string())?;
            OpenOptions::new()
                .read(true)
                .open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}
/// Unrecognized actions are conservatively mutations. Only the explicitly
/// read-only local query tools (and direct get_* diagnostics) bypass the frontier.
pub fn is_mutating_action(action: &Action) -> bool {
    !matches!(action.name.as_str(), "query_state" | "query_window")
        && !action.name.starts_with("get_")
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn crash_recovery_does_not_replay_or_claim_success() {
        let mut l = Ledger::default();
        l.record_intent(OperationRecord {
            operation_id: "o1".into(),
            plan_hash: "p1".into(),
            wm_instance_id: "wm1".into(),
            action: Action {
                name: "switch_tag".into(),
                arguments: json!({}),
            },
            state: OperationState::Dispatched,
            result: None,
            created_ms: 0,
        })
        .unwrap();
        l.recover();
        assert_eq!(l.operations["o1"].state, OperationState::Unknown);
    }
    #[test]
    fn undo_is_reverse_order_and_rejects_manual_changes() {
        let mut l = Ledger::default();
        let a = |name: &str| Action {
            name: name.into(),
            arguments: json!({}),
        };
        l.remember_undo(
            UndoPlan {
                plan_hash: "p".into(),
                previous_mutating_plan_hash: None,
                wm_instance_id: "wm1".into(),
                created_ms: 1,
                steps: vec![
                    UndoStep {
                        inverse: a("first"),
                        after: json!(1),
                    },
                    UndoStep {
                        inverse: a("second"),
                        after: json!(2),
                    },
                ],
            },
            2,
        );
        assert_eq!(
            l.undo_candidate("wm1", 3, &[json!(1), json!(2)]).unwrap()[0].name,
            "second"
        );
        assert!(l.undo_candidate("wm1", 3, &[json!(1), json!(3)]).is_err());
        assert!(l.undo_candidate("wm2", 3, &[json!(1), json!(2)]).is_err());
    }
    #[test]
    fn repeated_target_changes_compare_the_final_state_before_reverse_compensation() {
        let mut ledger = Ledger::default();
        let make = |mask, monitor: &str| json!({"window_token":"w1","tag_mask":mask,"monitor_token":monitor});
        ledger.remember_undo(
            UndoPlan {
                plan_hash: "plan".into(),
                previous_mutating_plan_hash: None,
                wm_instance_id: "wm".into(),
                created_ms: 1,
                steps: vec![
                    UndoStep {
                        inverse: Action {
                            name: "move_window_to_tags".into(),
                            arguments: json!({}),
                        },
                        after: make(2, "m1"),
                    },
                    UndoStep {
                        inverse: Action {
                            name: "move_window_to_monitor".into(),
                            arguments: json!({}),
                        },
                        after: make(4, "m2"),
                    },
                ],
            },
            1,
        );
        let inverse = ledger
            .undo_candidate("wm", 2, &[make(4, "m2"), make(4, "m2")])
            .unwrap();
        assert_eq!(inverse[0].name, "move_window_to_monitor");
        assert!(
            ledger
                .undo_candidate("wm", 2, &[make(8, "m2"), make(8, "m2")])
                .is_err()
        );
    }
    #[test]
    fn undo_never_skips_a_newer_nonreversible_or_uncertain_mutation() {
        for state in [
            OperationState::Dispatched,
            OperationState::Accepted,
            OperationState::Unknown,
            OperationState::Succeeded,
        ] {
            let mut ledger = Ledger::default();
            ledger.record_intent(operation("layout", 1)).unwrap();
            ledger.remember_undo(
                UndoPlan {
                    plan_hash: "plan".into(),
                    previous_mutating_plan_hash: None,
                    wm_instance_id: "wm".into(),
                    created_ms: 1,
                    steps: vec![UndoStep {
                        inverse: Action {
                            name: "set_layout".into(),
                            arguments: json!({}),
                        },
                        after: json!({"layout":"grid"}),
                    }],
                },
                1,
            );
            let mut read = operation("read", 2);
            read.plan_hash = "read-plan".into();
            read.action.name = "query_state".into();
            ledger.record_intent(read).unwrap();
            assert!(
                ledger
                    .undo_candidate("wm", 3, &[json!({"layout":"grid"})])
                    .is_ok()
            );
            let mut newer = operation("new", 4);
            newer.plan_hash = "launch-plan".into();
            newer.action.name = "launch_application".into();
            ledger.record_intent(newer).unwrap();
            if state != OperationState::Dispatched {
                ledger.record_result_at("new", state, json!({}), 5).unwrap();
            }
            assert_eq!(
                ledger
                    .undo_candidate("wm", 6, &[json!({"layout":"grid"})])
                    .unwrap_err(),
                "latest_plan_not_reversible"
            );
            ledger.prune_terminal(700_000);
            assert_eq!(
                ledger.latest_undo_plan().unwrap_err(),
                "latest_plan_not_reversible"
            );
            let restored: Ledger =
                serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
            assert_eq!(
                restored.last_mutating_plan_hash.as_deref(),
                Some("launch-plan")
            );
        }
    }
    fn operation(id: &str, created_ms: u64) -> OperationRecord {
        OperationRecord {
            operation_id: id.into(),
            plan_hash: "plan".into(),
            wm_instance_id: "wm".into(),
            action: Action {
                name: "switch_tag".into(),
                arguments: json!({}),
            },
            state: OperationState::Dispatched,
            result: None,
            created_ms,
        }
    }
    #[test]
    fn long_running_ledger_prunes_only_terminal_records_after_retention() {
        let mut ledger = Ledger::default();
        for (id, state) in [
            ("unknown", OperationState::Unknown),
            ("accepted", OperationState::Accepted),
            ("dispatched", OperationState::Dispatched),
        ] {
            ledger.record_intent(operation(id, 0)).unwrap();
            if state != OperationState::Dispatched {
                ledger.record_result_at(id, state, json!({}), 0).unwrap();
            }
        }
        for number in 0..5000 {
            let now = number * 1000;
            let id = format!("done-{number}");
            ledger.record_intent(operation(&id, now)).unwrap();
            ledger
                .record_result_at(&id, OperationState::Succeeded, json!({}), now)
                .unwrap();
        }
        assert!(ledger.operations.len() < 700);
        for id in ["unknown", "accepted", "dispatched"] {
            assert!(ledger.operations.contains_key(id));
        }
        assert!(!ledger.operations.contains_key("done-0"));
        assert!(ledger.operations.contains_key("done-4999"));
        assert_eq!(ledger.terminal_times.len(), ledger.operations.len() - 3);
    }
    #[test]
    fn terminal_retention_starts_at_completion_not_at_dispatch() {
        let mut ledger = Ledger::default();
        ledger.record_intent(operation("slow", 0)).unwrap();
        ledger
            .record_result_at("slow", OperationState::Succeeded, json!({}), 3_600_000)
            .unwrap();
        assert_eq!(ledger.prune_terminal(3_600_001), 0);
        assert_eq!(ledger.prune_terminal(4_200_000), 0);
        assert_eq!(ledger.prune_terminal(4_200_001), 1);
        assert!(ledger.terminal_times.is_empty());
    }
}
