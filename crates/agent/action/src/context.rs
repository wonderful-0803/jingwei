//! Opt-in composition of canonical action protocols, context budgets and data views.
use crate::{
    ActionError, ActionProtocol, ActionStep, ActionStepError, ActionStepOptions, ActionStepReport,
    AgentAction, JsonActionProtocol, NativeToolProtocol, StepOutcome,
};
use jingwei_context::*;
use jingwei_core::{CancellationSignal, TaskId, TurnId};
use jingwei_llm::{
    GenerationRequest, GenerationResponse, ModelGateway, ModelMessage, ModelToolDefinition,
};
use jingwei_tool::{ToolExecution, ToolGateway};
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub enum ContextActionProtocol {
    Native,
    Json,
}
impl ContextActionProtocol {
    fn protocol(self) -> Arc<dyn ActionProtocol> {
        match self {
            Self::Native => Arc::new(NativeToolProtocol),
            Self::Json => Arc::new(JsonActionProtocol),
        }
    }
}
pub struct ContextActionInput<'a> {
    pub task_id: TaskId,
    pub history: &'a ConversationProjection,
    pub system: &'a [String],
    pub current: &'a [ModelMessage],
    pub pinned_turns: &'a [TurnId],
    pub state_version: Option<&'a str>,
    pub target: &'a ContextTarget,
    pub budget: ContextBudget,
    pub limits: ContextBuildLimits,
    pub content_scope: &'a ContentScope,
    /// Trusted host time snapshot, in the same units as the content scope expiry.
    pub now_ms: u64,
}
pub struct ContextActionPolicies<'a> {
    pub counter: &'a dyn ContextTokenCounter,
    pub selector: &'a dyn ToolSelector,
    pub result: &'a dyn ToolResultPolicy,
    pub store: &'a dyn ContentStore,
    pub tool_limits: ToolViewLimits,
    pub max_preview_bytes: usize,
}
#[derive(Debug)]
pub struct ContextActionReport {
    pub step: ActionStepReport,
    pub build: ContextBuildReport,
    pub tools: ToolView,
    pub result: Option<ToolResultView>,
    /// Current-turn messages only; pass this to the next step, with the same history.
    pub next_current: Vec<ModelMessage>,
}
#[derive(Debug, thiserror::Error)]
pub enum ContextActionError {
    #[error("invalid context action scope or output limit")]
    Invalid,
    #[error(transparent)]
    View(#[from] ViewError),
    #[error(transparent)]
    Protocol(#[from] ActionError),
    #[error(transparent)]
    Build(#[from] ContextBuildError),
    #[error("controlled action failed")]
    Action {
        error: Box<ActionStepError>,
        build: Box<ContextBuildReport>,
        tools: ToolView,
    },
    /// Tool already executed. The report retains original execution evidence.
    #[error("result view failed after execution: {error}")]
    ResultView {
        error: ViewError,
        report: Box<ContextActionReport>,
    },
}

/// Canonical native/JSON paths only. Custom protocols can use the lower-level
/// selection/builder APIs; this adapter does not trust arbitrary request rewrites.
pub struct ContextualActionStep {
    pub protocol: ContextActionProtocol,
}
impl ContextualActionStep {
    pub async fn run(
        &self,
        input: ContextActionInput<'_>,
        policies: ContextActionPolicies<'_>,
        model: &dyn ModelGateway,
        tools: &dyn ToolGateway,
        cancellation: &dyn CancellationSignal,
        mut options: ActionStepOptions,
    ) -> Result<ContextActionReport, ContextActionError> {
        if input.task_id != input.content_scope.task_id
            || input.history.session_id != input.content_scope.session_id
            || input.content_scope.expires_at_ms <= input.now_ms
            || policies.max_preview_bytes == 0
        {
            return Err(ContextActionError::Invalid);
        }
        let max_output =
            u32::try_from(input.budget.output_reserve).map_err(|_| ContextActionError::Invalid)?;
        if max_output == 0 || options.model.max_tokens == Some(0) {
            return Err(ContextActionError::Invalid);
        }
        options.model.max_tokens = Some(
            options
                .model
                .max_tokens
                .unwrap_or(max_output)
                .min(max_output),
        );
        let granted: Vec<_> = tools
            .schemas()
            .iter()
            .map(|t| ModelToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect();
        let view = select_tool_view(&granted, policies.selector, policies.tool_limits)?;
        let protocol = self.protocol.protocol();
        // Canonical protocol adds exactly one protected System instruction. Prepare
        // its actual constraint before budgeting, including the AskUser schema.
        let mut prepared = protocol.request(input.current.to_vec(), &view.tools)?;
        let ModelMessage::System { content } = prepared.messages.remove(0) else {
            return Err(ContextActionError::Invalid);
        };
        if prepared.messages != input.current {
            return Err(ContextActionError::Invalid);
        }
        let mut system = vec![content];
        system.extend_from_slice(input.system);
        let built = CanonicalContextBuilder.build(
            ContextBuildInput {
                history: input.history,
                system: &system,
                current: input.current,
                constraint: &prepared.constraint,
                pinned_turns: input.pinned_turns,
                state_version: input.state_version,
                target: input.target,
                budget: input.budget,
                limits: input.limits,
            },
            policies.counter,
        )?;
        // The counted request is sent verbatim. Continuations omit the generated
        // protocol instruction so it cannot accumulate across successive steps.
        let base = built.request.messages[1..].to_vec();
        let base_len = base.len();
        let adapter = PreparedProtocol {
            protocol,
            request: built.request,
            base: base.clone(),
            tools: view.tools.clone(),
        };
        let step = ActionStep::new(Arc::new(adapter))
            .with_visible_tools(view.tools.iter().map(|t| t.name.clone()));
        let step = step
            .run(input.task_id, base, model, tools, cancellation, options)
            .await
            .map_err(|error| ContextActionError::Action {
                error: Box::new(error),
                build: Box::new(built.report.clone()),
                tools: view.clone(),
            })?;
        let mut report = ContextActionReport {
            step,
            build: built.report,
            tools: view,
            result: None,
            next_current: vec![],
        };
        let execution = match &report.step.outcome {
            StepOutcome::ToolCompleted { execution } | StepOutcome::Halted { execution } => {
                Some(execution.as_ref())
            }
            _ => None,
        };
        if let Some(execution) = execution {
            let result = if execution.result_event().session_id != input.content_scope.session_id {
                Err(ViewError::Unavailable)
            } else {
                project_tool_result(
                    &execution.result().outcome,
                    &execution.result_event().event_id,
                    input.content_scope,
                    input.now_ms,
                    policies.result,
                    policies.store,
                    policies.max_preview_bytes,
                )
            };
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    return Err(ContextActionError::ResultView {
                        error,
                        report: Box::new(report),
                    });
                }
            };
            let content = json!({
                "type":"jingwei_tool_result", "trust":"untrusted_tool_data",
                "name":execution.call().name, "runtime_call_id":execution.call().id,
                "view":result,
            })
            .to_string();
            match report.step.next_messages.last_mut() {
                Some(ModelMessage::Tool { content: body, .. })
                    if matches!(self.protocol, ContextActionProtocol::Native) =>
                {
                    *body = content
                }
                Some(ModelMessage::User { content: body })
                    if matches!(self.protocol, ContextActionProtocol::Json) =>
                {
                    *body = content
                }
                _ => {
                    return Err(ContextActionError::ResultView {
                        error: ViewError::Invalid,
                        report: Box::new(report),
                    });
                }
            }
            report.result = Some(result);
        }
        report.next_current = input
            .current
            .iter()
            .cloned()
            .chain(report.step.next_messages[base_len..].iter().cloned())
            .collect();
        Ok(report)
    }
}

struct PreparedProtocol {
    protocol: Arc<dyn ActionProtocol>,
    request: GenerationRequest,
    base: Vec<ModelMessage>,
    tools: Vec<ModelToolDefinition>,
}
impl ActionProtocol for PreparedProtocol {
    fn request(
        &self,
        messages: Vec<ModelMessage>,
        tools: &[ModelToolDefinition],
    ) -> Result<GenerationRequest, ActionError> {
        if messages != self.base || tools != self.tools {
            return Err(ActionError::ToolNotVisible);
        }
        Ok(self.request.clone())
    }
    fn parse(&self, response: &GenerationResponse) -> Result<AgentAction, ActionError> {
        self.protocol.parse(response)
    }
    fn continuation(
        &self,
        response: &GenerationResponse,
        action: &AgentAction,
        execution: Option<&ToolExecution>,
    ) -> Result<Vec<ModelMessage>, ActionError> {
        self.protocol.continuation(response, action, execution)
    }
}
