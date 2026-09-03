//! Host-assigned decision correlation, independent of provider IDs or permissions.
use crate::{ProviderToolCallId, StepId, TaskId};
use serde::{Deserialize, Serialize};

/// One model decision within a logical task. Not an idempotency or authorization token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionContext {
    pub task_id: TaskId,
    pub step_id: StepId,
}

/// Origin attached before the canonical ToolCall is committed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionContext {
    pub decision: DecisionContext,
    pub action_index: u32,
    pub provider_call_id: Option<ProviderToolCallId>,
}
