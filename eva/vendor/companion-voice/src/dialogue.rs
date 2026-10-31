//! References to the existing Eva pipeline, never a second execution channel.
use companion_protocol::identifier;
use serde::{Deserialize, Serialize};

pub const VERSION: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Plan,
    Discovery,
    SchemaRepair,
    Feedback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Completed,
    Incomplete,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    #[serde(rename = "eva_submit_plan")]
    SubmitPlan,
    #[serde(rename = "eva_describe_actions")]
    DescribeActions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Partial,
    Unknown,
    Cancelled,
    Noop,
    Clarify,
    Described,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub operation_id: String,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    InputCommitted {
        item_id: String,
    },
    ResponseCompleted {
        response_id: String,
        phase: Phase,
        status: ResponseStatus,
    },
    ReplyGenerated {
        response_id: String,
        content_digest: String,
    },
    ToolCall {
        response_id: String,
        call_id: String,
        tool: Tool,
        arguments_digest: String,
    },
    ToolResult {
        call_id: String,
        result_digest: String,
        outcome: Outcome,
        operations: Vec<Operation>,
    },
    OperationRegistered {
        call_id: String,
        operation_id: String,
        plan_hash: String,
        wm_instance_id: String,
        request_digest: String,
    },
    OperationSettled {
        operation_id: String,
        request_digest: String,
        result_digest: String,
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub voice_session_id: String,
    pub turn_id: String,
    pub observed_at_ns: u64,
    pub event: Event,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl Record {
    pub fn valid(&self) -> bool {
        if !identifier(&self.voice_session_id)
            || !identifier(&self.turn_id)
            || self.observed_at_ns == 0
        {
            return false;
        }
        match &self.event {
            Event::InputCommitted { item_id } => identifier(item_id),
            Event::ResponseCompleted { response_id, .. } => identifier(response_id),
            Event::ReplyGenerated {
                response_id,
                content_digest,
            } => identifier(response_id) && digest(content_digest),
            Event::OperationRegistered {
                call_id,
                operation_id,
                plan_hash,
                wm_instance_id,
                request_digest,
            } => {
                identifier(call_id)
                    && identifier(operation_id)
                    && identifier(wm_instance_id)
                    && digest(plan_hash)
                    && digest(request_digest)
            }
            Event::OperationSettled {
                operation_id,
                request_digest,
                result_digest,
                outcome,
            } => {
                identifier(operation_id)
                    && digest(request_digest)
                    && digest(result_digest)
                    && matches!(
                        outcome,
                        Outcome::Succeeded
                            | Outcome::Failed
                            | Outcome::Cancelled
                            | Outcome::Unknown
                    )
            }
            Event::ToolCall {
                response_id,
                call_id,
                arguments_digest,
                ..
            } => identifier(response_id) && identifier(call_id) && digest(arguments_digest),
            Event::ToolResult {
                call_id,
                result_digest,
                outcome,
                operations,
            } => {
                let mut ids = std::collections::BTreeSet::new();
                identifier(call_id)
                    && digest(result_digest)
                    && operations.len() <= 8
                    && (*outcome != Outcome::Succeeded
                        || operations
                            .iter()
                            .all(|operation| operation.outcome == Outcome::Succeeded))
                    && (!matches!(
                        outcome,
                        Outcome::Noop | Outcome::Clarify | Outcome::Described | Outcome::Rejected
                    ) || operations.is_empty())
                    && operations.iter().all(|operation| {
                        identifier(&operation.operation_id)
                            && ids.insert(&operation.operation_id)
                            && matches!(
                                operation.outcome,
                                Outcome::Succeeded
                                    | Outcome::Failed
                                    | Outcome::Partial
                                    | Outcome::Unknown
                                    | Outcome::Cancelled
                            )
                    })
            }
        }
    }
}
