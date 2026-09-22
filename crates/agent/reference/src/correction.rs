use jingwei_action::{
    ActionError, ActionProtocol, ActionStepFailure, AgentAction, ContextActionError,
    ContextActionProtocol, JsonActionProtocol, NativeToolProtocol,
};
use jingwei_core::{DecisionContext, ModelProtocolError, TurnId};
use jingwei_llm::{LlmError, ModelGatewayError};
use serde::{Deserialize, Serialize};

pub const REFERENCE_CORRECTION_EVENT: &str = "correction_v1";
/// Closed diagnostic vocabulary; never echoes model text or tool data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CorrectionReason {
    InvalidFormat,
    IncompleteResponse,
    CompletionRejected,
    NoProgress,
    EmptyText,
    MultipleActions,
    InvalidArguments,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCorrectionReport {
    pub version: u16,
    pub turn_id: TurnId,
    pub decision: DecisionContext,
    /// Failed inference ordinal. Correction is charged before this report is emitted.
    pub ordinal: u32,
    pub correction: u32,
    pub reason: CorrectionReason,
}
pub(crate) fn eligible(
    error: &ContextActionError,
    protocol: ContextActionProtocol,
) -> Option<(&DecisionContext, CorrectionReason)> {
    let ContextActionError::Action { error, tools, .. } = error else {
        return None;
    };
    if error.completed_execution.is_some() {
        return None;
    }
    let reason = match error.failure.as_ref() {
        ActionStepFailure::Model(ModelGatewayError::SchemaRejected { response }) => {
            let parsed = match protocol {
                ContextActionProtocol::Native => NativeToolProtocol.parse(response),
                ContextActionProtocol::Json => JsonActionProtocol.parse(response),
            };
            match parsed {
                Ok(AgentAction::CallTool { name, .. })
                    if tools.tools.iter().any(|t| t.name == name) =>
                {
                    CorrectionReason::InvalidArguments
                }
                Ok(AgentAction::CallTool { .. }) => return None,
                Err(ActionError::EmptyText) => CorrectionReason::EmptyText,
                Err(ActionError::InvalidAction) => CorrectionReason::InvalidFormat,
                _ => return None,
            }
        }
        ActionStepFailure::Validation(ActionError::ModelProtocol(
            ModelProtocolError::IncompleteResponse,
        )) => CorrectionReason::IncompleteResponse,
        ActionStepFailure::Validation(ActionError::InvalidAction) => {
            CorrectionReason::InvalidFormat
        }
        ActionStepFailure::Validation(ActionError::EmptyText) => CorrectionReason::EmptyText,
        ActionStepFailure::Validation(ActionError::MultipleActions) => {
            CorrectionReason::MultipleActions
        }
        ActionStepFailure::Validation(ActionError::InvalidArguments) => {
            CorrectionReason::InvalidArguments
        }
        // Only an invalid JSON output has an unambiguous, pre-tool interpretation.
        // Other model/recording/runtime errors keep their typed stop behavior.
        ActionStepFailure::Model(ModelGatewayError::Model(LlmError::Protocol(
            ModelProtocolError::InvalidJsonOutput,
        ))) => CorrectionReason::InvalidFormat,
        _ => return None,
    };
    Some((&error.context, reason))
}
pub(crate) fn feedback(reason: CorrectionReason) -> String {
    let instruction = match reason {
        CorrectionReason::IncompleteResponse => {
            "The previous response was incomplete and no tool executed. Return one shorter complete action. Use concise code and bounded output; omit commentary and repeated print statements."
        }
        CorrectionReason::CompletionRejected => {
            "The host rejected completion. Fulfill the task submission contract using an available tool, then finish. A completion claim alone is insufficient."
        }
        CorrectionReason::NoProgress => {
            "Repeated actions produced no progress. Change strategy using the observations already obtained; execute or submit the solution instead of repeating inspection."
        }
        _ => {
            "The previous proposal was rejected before tool execution. Return exactly one action satisfying the supplied schema. Do not repeat unavailable or denied operations."
        }
    };
    serde_json::json!({
        "type":"jingwei_action_correction_v1", "reason":reason,
        "instruction":instruction
    })
    .to_string()
}
