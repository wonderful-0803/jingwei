//! Optional, backend-neutral action decisions over controlled model and Tool gateways.
//!
//! This is a single-step component, not an autonomous loop, permission system,
//! business-success verifier, or task recovery store.
mod protocol;
mod step;
mod validation;

pub use jingwei_core::{ActionContext, DecisionContext, StepId, TaskId};
pub use protocol::{ActionProtocol, JsonActionProtocol, NativeToolProtocol};
pub use step::{
    ActionStep, ActionStepError, ActionStepFailure, ActionStepOptions, ActionStepReport,
    StepOutcome,
};

use jingwei_llm::{ModelProtocolError, ProviderToolCallId};
use serde_json::Value;

/// A complete proposal. Constructing this value does not authorize execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentAction {
    CallTool {
        name: String,
        arguments: Value,
        provider_call_id: Option<ProviderToolCallId>,
    },
    /// The model's completion claim, not verified business success.
    Final { text: String },
    /// A question for the host to present; never an indefinitely pending future.
    AskUser { question: String },
}

/// Deterministic, non-sensitive diagnostics. No prompt, arguments or tool output.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ActionError {
    #[error(transparent)]
    ModelProtocol(#[from] ModelProtocolError),
    #[error("action is not one exact tagged JSON object")]
    InvalidAction,
    #[error("action text or question must not be blank")]
    EmptyText,
    #[error("one decision may contain only one action")]
    MultipleActions,
    #[error("requested tool is not in the visible granted set")]
    ToolNotVisible,
    #[error("tool catalog has duplicate or invalid names")]
    InvalidCatalog,
    #[error("tool name conflicts with a protocol control action")]
    ReservedToolName,
    #[error("tool arguments must be an object satisfying the registered schema")]
    InvalidArguments,
    #[error("schema is invalid or requires external resources")]
    InvalidSchema,
    #[error("protocol continuation is inconsistent with the action")]
    InvalidFeedback,
    #[error("committed tool evidence does not match the proposed action")]
    ToolEvidenceMismatch,
}
