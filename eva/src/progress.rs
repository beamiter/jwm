//! Presentation follows locally verified receipts, never model estimates.
use jwm_eva_core::actions::{FrozenPlan, MAX_ACTIONS_PER_PLAN};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Preparing,
    Executing,
    Verifying,
    Succeeded,
    Partial,
    Unknown,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProgress {
    pub completed: usize,
    pub total: usize,
    pub current_step: usize,
    pub action: String,
    pub stage: Stage,
}
/// A final playback receipt takes precedence over an earlier process-start
/// read-back. Match the public receipt projection without exposing its data.
pub(crate) fn verified_step(step: &Value) -> bool {
    let playback = step["verified"]["playback_verified"]
        .as_bool()
        .or_else(|| step["result"]["verified"]["playback_verified"].as_bool())
        .or_else(|| step["result"]["verified"]["read_back"]["playback_verified"].as_bool());
    step["status"] == "succeeded" && playback != Some(false)
}
impl ExecutionProgress {
    pub fn at(plan: &FrozenPlan, index: usize, completed: usize, stage: Stage) -> Option<Self> {
        if plan.actions.is_empty()
            || plan.actions.len() > MAX_ACTIONS_PER_PLAN
            || index >= plan.actions.len()
            || completed > index + 1
            || (matches!(
                stage,
                Stage::Preparing | Stage::Executing | Stage::Verifying
            ) && completed != index)
            || (stage == Stage::Succeeded
                && (completed != plan.actions.len() || index + 1 != plan.actions.len()))
        {
            return None;
        }
        Some(Self {
            completed,
            total: plan.actions.len(),
            current_step: index + 1,
            action: plan.actions[index].name.clone(),
            stage,
        })
    }
    pub fn finished(plan: &FrozenPlan, report: &Value) -> Option<Self> {
        let steps = report["steps"].as_array()?;
        let completed = steps.iter().filter(|step| verified_step(step)).count();
        let uncertain = steps.iter().any(|step| {
            matches!(
                step["status"].as_str(),
                Some("unknown" | "accepted" | "queued" | "in_flight")
            ) || (step["status"] == "succeeded" && !verified_step(step))
        });
        let stage = if uncertain {
            Stage::Unknown
        } else {
            match report["status"].as_str() {
                Some("succeeded") if completed == plan.actions.len() => Stage::Succeeded,
                Some("cancelled") => Stage::Cancelled,
                Some("failed") => Stage::Failed,
                _ if completed > 0 => Stage::Partial,
                _ => Stage::Unknown,
            }
        };
        Self::at(plan, steps.len().saturating_sub(1), completed, stage)
    }
    pub fn text(&self) -> String {
        let prefix = format!("已完成 {}/{} 步", self.completed, self.total);
        match self.stage {
            Stage::Preparing => format!("{prefix}，正在准备第 {} 步", self.current_step),
            Stage::Executing => format!("{prefix}，正在处理第 {} 步", self.current_step),
            Stage::Verifying => format!("{prefix}，正在核验第 {} 步", self.current_step),
            Stage::Succeeded => format!("已完成全部 {} 步", self.total),
            Stage::Partial => format!("{prefix}，后续步骤已停止"),
            Stage::Unknown => format!("{prefix}，当前操作结果待核验"),
            Stage::Failed => format!("{prefix}，第 {} 步失败", self.current_step),
            Stage::Cancelled => format!("{prefix}，后续步骤已取消"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_eva_core::{actions::Disposition, protocol::Action};
    use serde_json::json;
    fn plan() -> FrozenPlan {
        FrozenPlan {
            plan_hash: "private-hash".into(),
            wm_instance_id: "private-wm".into(),
            disposition: Disposition::Execute,
            actions: vec![
                Action {
                    name: "switch_tag".into(),
                    arguments: json!({"monitor_token":"private-token","tag_mask":2})
                };
                3
            ],
            question: None,
            reason: None,
            requires_confirmation: false,
        }
    }
    #[test]
    fn accepted_and_unknown_results_never_increment_verified_completion() {
        let p = plan();
        let report =
            json!({"status":"partial","steps":[{"status":"succeeded"},{"status":"accepted"}]});
        let value = ExecutionProgress::finished(&p, &report).unwrap();
        assert_eq!(value.completed, 1);
        assert_eq!(value.current_step, 2);
        assert_eq!(value.stage, Stage::Unknown);
        assert!(value.text().contains("待核验"));
        let public = serde_json::to_string(&value).unwrap();
        assert!(!public.contains("private"));
        assert!(!public.contains("token"));
    }
    #[test]
    fn complete_and_partial_receipts_report_verified_counts_only() {
        let p = plan();
        let done = json!({"status":"succeeded","steps":[{"status":"succeeded"},{"status":"succeeded"},{"status":"succeeded"}]});
        assert_eq!(
            ExecutionProgress::finished(&p, &done).unwrap().stage,
            Stage::Succeeded
        );
        let partial =
            json!({"status":"partial","steps":[{"status":"succeeded"},{"status":"failed"}]});
        let progress = ExecutionProgress::finished(&p, &partial).unwrap();
        assert_eq!(progress.completed, 1);
        assert_eq!(progress.stage, Stage::Partial);
        assert!(ExecutionProgress::at(&p, 3, 0, Stage::Preparing).is_none());
        assert!(ExecutionProgress::at(&p, 0, 4, Stage::Preparing).is_none());
    }
    #[test]
    fn playback_start_never_flashes_full_completion_before_final_feedback() {
        let p = plan();
        for verification in [
            json!({"playback_verified": false}),
            json!({"read_back": {"playback_verified": false}}),
        ] {
            let report = json!({"status":"succeeded","steps":[
                {"status":"succeeded"}, {"status":"succeeded"},
                {"status":"succeeded","result":{"verified":verification}}
            ]});
            let progress = ExecutionProgress::finished(&p, &report).unwrap();
            assert_eq!(progress.completed, 2);
            assert_eq!(progress.stage, Stage::Unknown);
        }
        let public = json!({"status":"succeeded","verified":{"playback_verified":false}});
        assert!(!verified_step(&public));
        let final_receipt = json!({"status":"succeeded","result":{"verified":{
            "playback_verified":true,"read_back":{"playback_verified":false}
        }}});
        assert!(verified_step(&final_receipt));
        assert!(verified_step(&json!({"status":"succeeded","result":{
            "before":{"playback_verified":false}
        }})));
    }
}
