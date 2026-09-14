//! Optional bounded Agent body. The host still owns runtime, grants, Task and IO.
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use jingwei_action::{
    ActionStepFailure, ActionStepOptions, ContextActionError, ContextActionInput,
    ContextActionPolicies, ContextActionProtocol, ContextActionReport, ContextualActionStep,
    StepOutcome,
};
use jingwei_agent::{
    Agent, AgentContext, AgentError, AgentEventKind, AgentFuture, AgentTurnInput, AgentTurnOutput,
    TurnOutcome,
};
use jingwei_budget::BudgetError;
use jingwei_context::*;
use jingwei_core::{DecisionContext, EventId, SessionId, TaskId, TurnId};
use jingwei_llm::{LlmError, ModelGatewayError, ModelMessage, ModelRuntimeError};
use jingwei_tool::{
    ToolCallOptions, ToolExecution, ToolFuture, ToolGateway, ToolRuntimeError, ToolSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod correction;
mod waiting;
pub use correction::*;
pub use waiting::*;
mod checks;
pub use checks::*;

pub const REFERENCE_EVENT_PLUGIN: &str = "jingwei.reference";
pub const REFERENCE_STEP_EVENT: &str = "step_v1";
pub const REFERENCE_RUN_EVENT: &str = "run_v1";

#[derive(Clone, Debug, thiserror::Error)]
#[error("invalid reference Agent configuration or clock")]
pub struct ReferenceConfigError;

/// Trusted nonblocking host clock, shared with its content store/read tools.
pub trait ReferenceClock: Send + Sync {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError>;
}
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemReferenceClock;
impl ReferenceClock for SystemReferenceClock {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ReferenceConfigError)?
            .as_millis()
            .try_into()
            .map_err(|_| ReferenceConfigError)
    }
}

/// Explicit host configuration. No model, persistence or authorization is selected.
#[derive(Clone, Debug)]
pub struct ReferenceAgentConfig {
    pub max_steps: u32,
    /// Per-turn ceiling, also charged to the shared Task correction budget.
    pub max_corrections: u32,
    pub protocol: ContextActionProtocol,
    pub system: Vec<String>,
    pub pinned_turns: Vec<TurnId>,
    pub target: ContextTarget,
    pub context_budget: ContextBudget,
    pub context_limits: ContextBuildLimits,
    pub projection: ProjectionConfig,
    pub tool_limits: ToolViewLimits,
    pub max_preview_bytes: usize,
    pub content_ttl_ms: u64,
    /// Per-event serialized payload limit, not a global process memory limit.
    pub max_report_bytes: usize,
    pub options: ActionStepOptions,
}
impl ReferenceAgentConfig {
    pub fn new(target: ContextTarget, context_budget: ContextBudget) -> Self {
        Self {
            max_steps: 8,
            max_corrections: 2,
            protocol: ContextActionProtocol::Native,
            system: vec![],
            pinned_turns: vec![],
            target,
            context_budget,
            context_limits: Default::default(),
            projection: Default::default(),
            tool_limits: Default::default(),
            max_preview_bytes: 4096,
            content_ttl_ms: 600_000,
            max_report_bytes: 256 * 1024,
            options: Default::default(),
        }
    }
}
/// Data-only strategy interfaces; none receives raw tools or budget grant handles.
pub struct ReferenceAgentPolicies {
    pub counter: Arc<dyn ContextTokenCounter>,
    pub selector: Arc<dyn ToolSelector>,
    pub result: Arc<dyn ToolResultPolicy>,
    pub store: Arc<dyn ContentStore>,
    pub clock: Arc<dyn ReferenceClock>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReferenceStop {
    ModelClaimedComplete,
    BusinessVerified,
    CompletionRejected,
    NoProgress,
    CorrectionLimit,
    WaitingForInput,
    StepLimit,
    ContextRejected,
    PolicyRejected,
    InvalidAction,
    ToolHalted,
    ResultViewFailed,
    BudgetExhausted,
    Cancelled,
    RuntimeFailure,
    HistoryRejected,
    ContentExpired,
    ClockFailure,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReferenceStepOutcome {
    ToolSucceeded,
    ToolFailed,
    ModelClaimedComplete,
    WaitingForInput,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReferenceStepReport {
    pub version: u16,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub ordinal: u32,
    pub decision: DecisionContext,
    pub outcome: ReferenceStepOutcome,
    pub build: ContextBuildReport,
    pub visible_tools: Vec<String>,
    pub tool_events: Vec<EventId>,
    pub result_view: Option<ToolResultView>,
}
/// Body observation, not canonical terminal/drain or durable recovery authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReferenceRunReport {
    pub version: u16,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub task_id: Option<TaskId>,
    pub max_steps: u32,
    pub attempted_steps: u32,
    pub confirmed_steps: u32,
    pub stop: ReferenceStop,
    /// True only when the host completion checker supplies evidence.
    pub business_verified: bool,
    #[serde(default)]
    pub completion: Option<CompletionDecision>,
    #[serde(default)]
    pub repeated_observations: u32,
    #[serde(default)]
    pub corrections: u32,
}

pub struct ReferenceAgent {
    config: ReferenceAgentConfig,
    policies: ReferenceAgentPolicies,
    checks: ReferenceChecks,
}
impl ReferenceAgent {
    pub fn new(
        config: ReferenceAgentConfig,
        policies: ReferenceAgentPolicies,
    ) -> Result<Self, ReferenceConfigError> {
        if config.max_steps == 0
            || config.max_steps > 1024
            || config.max_corrections > 1024
            || config.content_ttl_ms == 0
            || config.max_report_bytes == 0
            || config.max_preview_bytes == 0
            || config.target.model.trim().is_empty()
            || config.target.template_revision.trim().is_empty()
            || config.context_budget.window_tokens == 0
            || config.context_budget.output_reserve == 0
            || config.context_budget.output_reserve > u32::MAX as u64
        {
            return Err(ReferenceConfigError);
        }
        Ok(Self {
            config,
            policies,
            checks: ReferenceChecks::default(),
        })
    }
    /// Install data-only host checks. Limits are validated before any execution.
    pub fn with_checks(mut self, checks: ReferenceChecks) -> Result<Self, ReferenceConfigError> {
        checks.progress.validate()?;
        self.checks = checks;
        Ok(self)
    }
    async fn emit(
        &self,
        ctx: &dyn AgentContext,
        kind: &str,
        value: &impl Serialize,
    ) -> Result<(), AgentError> {
        let mut writer = BoundedPayload {
            bytes: vec![],
            max: self.config.max_report_bytes,
        };
        serde_json::to_writer(&mut writer, value).map_err(|_| {
            AgentError::failed(
                "reference_report_limit",
                "reference report exceeds its limit",
                false,
            )
        })?;
        let payload = serde_json::from_slice(&writer.bytes).map_err(|_| {
            AgentError::failed(
                "reference_report",
                "reference report serialization failed",
                false,
            )
        })?;
        ctx.emit(AgentEventKind::Custom {
            plugin: REFERENCE_EVENT_PLUGIN.into(),
            kind: kind.into(),
            payload,
        })
        .await
    }
    async fn drive(
        &self,
        input: &AgentTurnInput<'_>,
        ctx: &dyn AgentContext,
        run: &mut ReferenceRunReport,
    ) -> Result<AgentTurnOutput, AgentError> {
        let budget = ctx.budget().ok_or_else(|| {
            failed(
                "reference_budget_required",
                "a controlled Task budget is required",
            )
        })?;
        let snapshot = budget.report()?;
        if snapshot.identity.session_id != *input.session_id {
            return Err(failed(
                "reference_identity",
                "Task and Session do not match",
            ));
        }
        let task = snapshot.identity.task_id;
        run.task_id = Some(task.clone());
        if let Some(stop) = snapshot.stop {
            return Err(AgentError::Budget(BudgetError::Stopped(stop)));
        }
        let model = ctx.model().ok_or_else(|| {
            failed(
                "reference_model_required",
                "a controlled model gateway is required",
            )
        })?;
        let tools = ctx.tools().unwrap_or(&EmptyTools);
        let history = CanonicalConversationProjector
            .project(input.session_id, input.history, self.config.projection)
            .map_err(|_| {
                run.stop = ReferenceStop::HistoryRejected;
                failed("reference_history", "history projection rejected")
            })?;
        let mut now = self.policies.clock.now_ms().map_err(|_| {
            run.stop = ReferenceStop::ClockFailure;
            failed("reference_clock", "host clock failed")
        })?;
        let expires = now.checked_add(self.config.content_ttl_ms).ok_or_else(|| {
            run.stop = ReferenceStop::ClockFailure;
            failed("reference_clock", "content expiry overflow")
        })?;
        let scope = ContentScope {
            session_id: input.session_id.clone(),
            task_id: task.clone(),
            expires_at_ms: expires,
        };
        let mut current = vec![ModelMessage::user(input.user_message)];
        let step = ContextualActionStep {
            protocol: self.config.protocol,
        };
        let mut previous = None;
        let mut correction_feedback: Option<String> = None;
        for ordinal in 1..=self.config.max_steps {
            if ctx.cancellation().is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            if let Some(stop) = budget.report()?.stop {
                return Err(AgentError::Budget(BudgetError::Stopped(stop)));
            }
            let next = self.policies.clock.now_ms().map_err(|_| {
                run.stop = ReferenceStop::ClockFailure;
                failed("reference_clock", "host clock failed")
            })?;
            if next < now {
                run.stop = ReferenceStop::ClockFailure;
                return Err(failed("reference_clock", "host clock moved backwards"));
            }
            now = next;
            if now >= expires {
                run.stop = ReferenceStop::ContentExpired;
                return Err(failed("reference_content_expired", "content scope expired"));
            }
            run.attempted_steps = ordinal;
            let check_context = CheckContext {
                task_id: &task,
                turn_id: &input.turn_id,
            };
            let state_version = self.checks.state.version(&check_context).map_err(|_| {
                run.stop = ReferenceStop::PolicyRejected;
                failed("reference_state", "host state observation failed")
            })?;
            if state_version
                .as_ref()
                .is_some_and(|v| v.len() > 4096 || v.trim().is_empty())
            {
                run.stop = ReferenceStop::PolicyRejected;
                return Err(failed("reference_state", "invalid host state version"));
            }
            let mut system = self.config.system.clone();
            if let Some(feedback) = &correction_feedback {
                system.push(feedback.clone());
            }
            // ModelRuntime already charges a step for each accepted inference;
            // do not double-charge it with AgentBudget::consume_step here.
            let report = step
                .run(
                    ContextActionInput {
                        task_id: task.clone(),
                        history: &history,
                        system: &system,
                        current: &current,
                        pinned_turns: &self.config.pinned_turns,
                        state_version: state_version.as_deref(),
                        target: &self.config.target,
                        budget: self.config.context_budget,
                        limits: self.config.context_limits,
                        content_scope: &scope,
                        now_ms: now,
                    },
                    ContextActionPolicies {
                        counter: self.policies.counter.as_ref(),
                        selector: self.policies.selector.as_ref(),
                        result: self.policies.result.as_ref(),
                        store: self.policies.store.as_ref(),
                        tool_limits: self.config.tool_limits,
                        max_preview_bytes: self.config.max_preview_bytes,
                    },
                    model,
                    tools,
                    ctx.cancellation(),
                    self.config.options.clone(),
                )
                .await;
            let report = match report {
                Ok(report) => report,
                Err(error) => {
                    if let Some((decision, reason)) =
                        correction::eligible(&error, self.config.protocol)
                    {
                        if ordinal == self.config.max_steps {
                            run.stop = ReferenceStop::StepLimit;
                            return Err(failed(
                                "reference_step_limit",
                                "no action steps remain for correction",
                            ));
                        }
                        if run.corrections >= self.config.max_corrections {
                            run.stop = ReferenceStop::CorrectionLimit;
                            return Err(failed(
                                "reference_correction_limit",
                                "finite correction limit reached",
                            ));
                        }
                        if ctx.cancellation().is_cancelled() {
                            return Err(AgentError::Cancelled);
                        }
                        budget.consume_correction()?;
                        run.corrections += 1;
                        let correction = ReferenceCorrectionReport {
                            version: 1,
                            turn_id: input.turn_id.clone(),
                            decision: decision.clone(),
                            ordinal,
                            correction: run.corrections,
                            reason,
                        };
                        self.emit(ctx, REFERENCE_CORRECTION_EVENT, &correction)
                            .await?;
                        correction_feedback = Some(correction::feedback(reason));
                        continue;
                    }
                    if let ContextActionError::ResultView { report, .. } = &error {
                        // The tool is already confirmed. Preserve its event references
                        // before stopping; never retry a failed feedback projection.
                        self.emit(
                            ctx,
                            REFERENCE_STEP_EVENT,
                            &step_report(report, input, ordinal),
                        )
                        .await?;
                        run.confirmed_steps += 1;
                    }
                    let (stop, error) = action_failure(error);
                    run.stop = stop;
                    return Err(error);
                }
            };
            self.emit(
                ctx,
                REFERENCE_STEP_EVENT,
                &step_report(&report, input, ordinal),
            )
            .await?;
            run.confirmed_steps += 1;
            correction_feedback = None;
            match report.step.outcome {
                StepOutcome::Final { text } => {
                    let decision = self
                        .checks
                        .completion
                        .check(CompletionInput {
                            context: check_context,
                            claim: &text,
                            state_version: state_version.as_deref(),
                        })
                        .map_err(|_| {
                            run.stop = ReferenceStop::PolicyRejected;
                            failed("reference_completion_policy", "completion checker failed")
                        })?;
                    if !decision.valid() {
                        run.stop = ReferenceStop::PolicyRejected;
                        return Err(failed(
                            "reference_completion_policy",
                            "invalid completion evidence",
                        ));
                    }
                    run.completion = Some(decision.clone());
                    match &decision {
                        CompletionDecision::Unverified => {
                            run.stop = ReferenceStop::ModelClaimedComplete
                        }
                        CompletionDecision::Verified { .. } => {
                            run.stop = ReferenceStop::BusinessVerified;
                            run.business_verified = true;
                        }
                        CompletionDecision::Rejected { .. } => {
                            run.stop = ReferenceStop::CompletionRejected;
                            return Err(failed(
                                "reference_completion_rejected",
                                "host completion check rejected the claim",
                            ));
                        }
                    }
                    return Ok(AgentTurnOutput {
                        final_text: text,
                        outcome: TurnOutcome::Completed,
                        artifact: Some(
                            json!({"type":"reference_completion_v1","task_id":task,"business_verified":run.business_verified,"completion":decision}),
                        ),
                    });
                }
                StepOutcome::AskUser { question } => {
                    let pending = PendingQuestion {
                        version: 1,
                        session_id: input.session_id.clone(),
                        task_id: task.clone(),
                        turn_id: input.turn_id.clone(),
                        agent_key: snapshot.identity.agent_key.clone(),
                        pending_question: question.clone(),
                    };
                    if !pending.valid() {
                        run.stop = ReferenceStop::PolicyRejected;
                        return Err(failed(
                            "reference_question_limit",
                            "pending question is invalid or too large",
                        ));
                    }
                    run.stop = ReferenceStop::WaitingForInput;
                    return Ok(AgentTurnOutput {
                        final_text: question.clone(),
                        outcome: TurnOutcome::WaitingForInput,
                        artifact: Some(
                            json!({"type":"reference_waiting_v1","task_id":task,"turn_id":input.turn_id,"pending_question":question,"pending":pending}),
                        ),
                    });
                }
                StepOutcome::Halted { .. } => {
                    run.stop = ReferenceStop::ToolHalted;
                    return Err(failed(
                        "reference_tool_halted",
                        "tool failed; no retry was attempted",
                    ));
                }
                StepOutcome::ToolCompleted { execution } => {
                    let mut key = BoundedPayload {
                        bytes: vec![],
                        max: self.checks.progress.max_observation_bytes,
                    };
                    serde_json::to_writer(
                        &mut key,
                        &(
                            execution.call().name.as_str(),
                            &execution.call().arguments,
                            &execution.result().outcome,
                            &state_version,
                        ),
                    )
                    .map_err(|_| {
                        run.stop = ReferenceStop::PolicyRejected;
                        failed(
                            "reference_progress_limit",
                            "progress observation exceeds its limit",
                        )
                    })?;
                    run.repeated_observations = if previous.as_ref() == Some(&key.bytes) {
                        run.repeated_observations + 1
                    } else {
                        1
                    };
                    previous = Some(key.bytes);
                    let limit = self.checks.progress.limit(&execution.call().name);
                    if run.repeated_observations >= limit {
                        run.stop = ReferenceStop::NoProgress;
                        return Err(failed(
                            "reference_no_progress",
                            "repeated action and feedback without state change",
                        ));
                    }
                    current = report.next_current;
                }
            }
        }
        run.stop = ReferenceStop::StepLimit;
        Err(failed(
            "reference_step_limit",
            "finite action step limit reached",
        ))
    }
}
impl Agent for ReferenceAgent {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            let mut run = ReferenceRunReport {
                version: 1,
                session_id: input.session_id.clone(),
                turn_id: input.turn_id.clone(),
                task_id: None,
                max_steps: self.config.max_steps,
                attempted_steps: 0,
                confirmed_steps: 0,
                stop: ReferenceStop::RuntimeFailure,
                business_verified: false,
                completion: None,
                repeated_observations: 0,
                corrections: 0,
            };
            let output = self.drive(&input, ctx, &mut run).await;
            // Runtime-owned failures/cancellation keep their full typed evidence.
            // The runtime can also interrupt this future before we emit anything;
            // TaskRunReport and the terminal remain the authoritative final state.
            if output.is_ok() || matches!(output, Err(AgentError::Failed(_))) {
                self.emit(ctx, REFERENCE_RUN_EVENT, &run).await?;
            }
            output
        })
    }
}
fn failed(code: &str, message: &str) -> AgentError {
    AgentError::failed(code, message, false)
}
fn step_report(
    report: &ContextActionReport,
    input: &AgentTurnInput<'_>,
    ordinal: u32,
) -> ReferenceStepReport {
    let (outcome, execution) = match &report.step.outcome {
        StepOutcome::Final { .. } => (ReferenceStepOutcome::ModelClaimedComplete, None),
        StepOutcome::AskUser { .. } => (ReferenceStepOutcome::WaitingForInput, None),
        StepOutcome::ToolCompleted { execution } => (
            ReferenceStepOutcome::ToolSucceeded,
            Some(execution.as_ref()),
        ),
        StepOutcome::Halted { execution } => {
            (ReferenceStepOutcome::ToolFailed, Some(execution.as_ref()))
        }
    };
    ReferenceStepReport {
        version: 1,
        task_id: report.step.context.task_id.clone(),
        turn_id: input.turn_id.clone(),
        ordinal,
        decision: report.step.context.clone(),
        outcome,
        build: report.build.clone(),
        visible_tools: report.tools.tools.iter().map(|t| t.name.clone()).collect(),
        tool_events: execution.map_or_else(Vec::new, |e| {
            vec![
                e.call_event().event_id.clone(),
                e.result_event().event_id.clone(),
            ]
        }),
        result_view: report.result.clone(),
    }
}
fn action_failure(error: ContextActionError) -> (ReferenceStop, AgentError) {
    match error {
        ContextActionError::Action { error, .. } => match *error.failure {
            ActionStepFailure::Cancelled => (ReferenceStop::Cancelled, AgentError::Cancelled),
            ActionStepFailure::Model(error) => {
                let stop = match &error {
                    ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)) => {
                        ReferenceStop::BudgetExhausted
                    }
                    ModelGatewayError::Model(LlmError::Cancelled) => ReferenceStop::Cancelled,
                    _ => ReferenceStop::RuntimeFailure,
                };
                (stop, AgentError::Model(error))
            }
            ActionStepFailure::Tool(error) => {
                let stop = match &error {
                    ToolRuntimeError::Budget(_) => ReferenceStop::BudgetExhausted,
                    ToolRuntimeError::Cancelled { .. } => ReferenceStop::Cancelled,
                    _ => ReferenceStop::RuntimeFailure,
                };
                (stop, AgentError::Tool(error))
            }
            ActionStepFailure::Validation(_) => (
                ReferenceStop::InvalidAction,
                failed(
                    "reference_invalid_action",
                    "action validation rejected; no correction was attempted",
                ),
            ),
        },
        ContextActionError::ResultView { .. } => (
            ReferenceStop::ResultViewFailed,
            failed(
                "reference_result_view",
                "confirmed tool result could not be projected",
            ),
        ),
        ContextActionError::Build(_) => (
            ReferenceStop::ContextRejected,
            failed("reference_context", "context budget or shape rejected"),
        ),
        ContextActionError::View(_) => (
            ReferenceStop::PolicyRejected,
            failed("reference_policy", "tool visibility policy rejected"),
        ),
        ContextActionError::Protocol(_) | ContextActionError::Invalid => (
            ReferenceStop::InvalidAction,
            failed(
                "reference_configuration",
                "action configuration or protocol rejected",
            ),
        ),
    }
}
struct EmptyTools;
impl ToolGateway for EmptyTools {
    fn schemas(&self) -> &[ToolSchema] {
        &[]
    }
    fn call_with_options(
        &self,
        _: &str,
        _: Value,
        _: ToolCallOptions,
    ) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
        Box::pin(async { Err(ToolRuntimeError::Stopped) })
    }
}
struct BoundedPayload {
    bytes: Vec<u8>,
    max: usize,
}
impl std::io::Write for BoundedPayload {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("report limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
