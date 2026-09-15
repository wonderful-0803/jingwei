//! Host-owned side-effect declarations and persisted logical operation identity.
use crate::{EventId, TaskId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    #[default]
    Unknown,
    ReadOnly,
    /// Provider must durably deduplicate identical keys and reject changed inputs.
    Idempotent,
    NonIdempotent,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEffectContract {
    pub effect: ToolEffect,
    /// Host-maintained revision of the effect and input/result contract.
    pub revision: String,
}
impl Default for ToolEffectContract {
    fn default() -> Self {
        Self {
            effect: ToolEffect::Unknown,
            revision: "unspecified".into(),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReviewOutcome {
    NotExecuted,
    Completed,
    Uncertain,
}
/// Trusted host evidence, not model-supplied authorization or synthesized output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRetryReview {
    pub actor: String,
    pub evidence: String,
    pub outcome: ToolReviewOutcome,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRetryAudit {
    pub call_id: String,
    pub event_id: EventId,
    pub review: Option<ToolRetryReview>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOperation {
    pub task_id: TaskId,
    /// Stable across explicitly approved attempts, distinct from per-attempt call ID.
    pub key: String,
    pub contract: ToolEffectContract,
    pub retry: Option<ToolRetryAudit>,
}
