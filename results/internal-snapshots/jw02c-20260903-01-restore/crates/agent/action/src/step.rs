use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use jingwei_core::{ActionContext, CancellationSignal, DecisionContext, StepId, TaskId};
use jingwei_llm::{
    GenerationOptions, GenerationRequest, GenerationResponse, ModelCallMode, ModelGateway,
    ModelGatewayError, ModelMessage, ModelToolDefinition,
};
use jingwei_tool::{
    ToolCallOptions, ToolExecution, ToolGateway, ToolRecordedOutcome, ToolRuntimeError,
};

use crate::{ActionError, ActionProtocol, AgentAction, validation};

#[derive(Clone, Debug, Default)]
pub struct ActionStepOptions {
    pub model: GenerationOptions,
    pub tool: ToolCallOptions,
}

/// A semantic outcome, not an Agent turn terminal or a business-success proof.
#[derive(Clone, Debug)]
pub enum StepOutcome {
    Final {
        text: String,
    },
    AskUser {
        question: String,
    },
    ToolCompleted {
        execution: Box<ToolExecution>,
    },
    /// A semantic tool failure: no retry, fallback tool or further inference was attempted.
    Halted {
        execution: Box<ToolExecution>,
    },
}

pub struct ActionStepReport {
    pub context: DecisionContext,
    pub request: GenerationRequest,
    pub response: GenerationResponse,
    pub action: AgentAction,
    pub outcome: StepOutcome,
    /// Closed model history. Generated protocol instructions are not accumulated here.
    pub next_messages: Vec<ModelMessage>,
}

impl fmt::Debug for ActionStepReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionStepReport")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

#[derive(thiserror::Error)]
pub enum ActionStepFailure {
    #[error(transparent)]
    Validation(#[from] ActionError),
    #[error("controlled model call failed")]
    Model(#[source] ModelGatewayError),
    #[error("controlled tool call failed")]
    Tool(#[source] ToolRuntimeError),
    #[error("action step cancelled")]
    Cancelled,
}

/// Carries the decision identity on every failure and any already committed tool execution.
/// Never automatically replay this step: a Tool error can include an uncertain side effect.
#[derive(thiserror::Error)]
#[error("action step failed: {failure}")]
pub struct ActionStepError {
    pub context: DecisionContext,
    pub failure: Box<ActionStepFailure>,
    pub completed_execution: Option<Box<ToolExecution>>,
}

impl fmt::Debug for ActionStepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionStepError")
            .field("context", &self.context)
            .field(
                "has_completed_execution",
                &self.completed_execution.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ActionStepFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => f.debug_tuple("Validation").field(error).finish(),
            Self::Model(_) => f.write_str("Model(..)"),
            Self::Tool(_) => f.write_str("Tool(..)"),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
}

/// Optional single-decision orchestrator. Policies are injected, not installed globally.
/// The host retains ownership of gateway scopes, cancellation, turn closure and task state.
pub struct ActionStep {
    protocol: Arc<dyn ActionProtocol>,
    visible: Option<BTreeSet<String>>,
}

impl ActionStep {
    pub fn new(protocol: Arc<dyn ActionProtocol>) -> Self {
        Self {
            protocol,
            visible: None,
        }
    }

    /// Only narrows ToolGateway's granted catalog; unknown names fail before inference.
    #[must_use]
    pub fn with_visible_tools(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.visible = Some(names.into_iter().collect());
        self
    }

    pub async fn run(
        &self,
        task_id: TaskId,
        messages: Vec<ModelMessage>,
        model: &dyn ModelGateway,
        tools: &dyn ToolGateway,
        cancellation: &dyn CancellationSignal,
        options: ActionStepOptions,
    ) -> Result<ActionStepReport, ActionStepError> {
        let context = DecisionContext {
            task_id,
            step_id: StepId::new(),
        };
        self.run_inner(
            context.clone(),
            messages,
            model,
            tools,
            cancellation,
            options,
        )
        .await
        .map_err(|(failure, execution)| ActionStepError {
            context,
            failure: Box::new(failure),
            completed_execution: execution.map(Box::new),
        })
    }

    async fn run_inner(
        &self,
        context: DecisionContext,
        mut messages: Vec<ModelMessage>,
        model: &dyn ModelGateway,
        tools: &dyn ToolGateway,
        cancellation: &dyn CancellationSignal,
        mut options: ActionStepOptions,
    ) -> Result<ActionStepReport, (ActionStepFailure, Option<ToolExecution>)> {
        let validation = |error: ActionError| (ActionStepFailure::Validation(error), None);
        if cancellation.is_cancelled() {
            return Err((ActionStepFailure::Cancelled, None));
        }
        let mut catalog = BTreeMap::new();
        for tool in tools.schemas() {
            if tool.name.trim().is_empty() || catalog.insert(tool.name.clone(), tool).is_some() {
                return Err(validation(ActionError::InvalidCatalog));
            }
        }
        if self
            .visible
            .as_ref()
            .is_some_and(|names| names.iter().any(|name| !catalog.contains_key(name)))
        {
            return Err(validation(ActionError::ToolNotVisible));
        }
        let candidates: Vec<_> = catalog
            .values()
            .filter(|tool| {
                self.visible
                    .as_ref()
                    .is_none_or(|names| names.contains(&tool.name))
            })
            .map(|tool| ModelToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect();
        let mut validators = BTreeMap::new();
        for tool in &candidates {
            validators.insert(
                tool.name.clone(),
                validation::compile(&tool.parameters).map_err(validation)?,
            );
        }
        let request = self
            .protocol
            .request(messages.clone(), &candidates)
            .map_err(validation)?;
        request
            .preflight(&model.capabilities(), ModelCallMode::Complete)
            .map_err(ActionError::from)
            .map_err(validation)?;
        options.model.context = Some(context.clone());
        let response = model
            .generate(&request, options.model)
            .await
            .map_err(|e| (ActionStepFailure::Model(e), None))?;
        if cancellation.is_cancelled() {
            return Err((ActionStepFailure::Cancelled, None));
        }
        validation::validate_response(&request, &response).map_err(validation)?;
        if response.tool_calls.len() > 1 {
            return Err(validation(ActionError::MultipleActions));
        }
        let action = self.protocol.parse(&response).map_err(validation)?;
        let execution = match &action {
            AgentAction::CallTool {
                name,
                arguments,
                provider_call_id,
            } => {
                let validator = validators
                    .get(name)
                    .ok_or_else(|| validation(ActionError::ToolNotVisible))?;
                if !arguments.is_object() || !validator.is_valid(arguments) {
                    return Err(validation(ActionError::InvalidArguments));
                }
                if cancellation.is_cancelled() {
                    return Err((ActionStepFailure::Cancelled, None));
                }
                let origin = ActionContext {
                    decision: context.clone(),
                    action_index: 0,
                    provider_call_id: provider_call_id.clone(),
                };
                options.tool.action = Some(origin.clone());
                let execution = tools
                    .call_with_options(name, arguments.clone(), options.tool)
                    .await
                    .map_err(|e| {
                        let completed = e.cancelled_execution().cloned();
                        (ActionStepFailure::Tool(e), completed)
                    })?;
                if execution.call().name != *name
                    || execution.call().arguments != *arguments
                    || execution.call().action.as_ref() != Some(&origin)
                {
                    return Err((
                        ActionStepFailure::Validation(ActionError::ToolEvidenceMismatch),
                        Some(execution),
                    ));
                }
                Some(execution)
            }
            AgentAction::Final { text } => {
                validation::text(text).map_err(validation)?;
                None
            }
            AgentAction::AskUser { question } => {
                validation::text(question).map_err(validation)?;
                None
            }
        };
        if cancellation.is_cancelled() {
            return Err((ActionStepFailure::Cancelled, execution));
        }
        let continuation = self
            .protocol
            .continuation(&response, &action, execution.as_ref())
            .map_err(|e| (ActionStepFailure::Validation(e), execution.clone()))?;
        messages.extend(continuation);
        GenerationRequest::text(messages.clone())
            .validate_shape()
            .map_err(|e| (ActionStepFailure::Validation(e.into()), execution.clone()))?;
        let outcome = match (&action, execution) {
            (AgentAction::Final { text }, None) => StepOutcome::Final { text: text.clone() },
            (AgentAction::AskUser { question }, None) => StepOutcome::AskUser {
                question: question.clone(),
            },
            (AgentAction::CallTool { .. }, Some(execution)) => {
                if matches!(
                    execution.result().outcome,
                    ToolRecordedOutcome::Succeeded { .. }
                ) {
                    StepOutcome::ToolCompleted {
                        execution: Box::new(execution),
                    }
                } else {
                    StepOutcome::Halted {
                        execution: Box::new(execution),
                    }
                }
            }
            _ => return Err(validation(ActionError::InvalidFeedback)),
        };
        Ok(ActionStepReport {
            context,
            request,
            response,
            action,
            outcome,
            next_messages: messages,
        })
    }
}
