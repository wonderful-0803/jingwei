//! Transport-free Agent and AgentRuntime vocabulary.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use jingwei_budget::{
    BudgetAmounts, BudgetCheckpoint, BudgetError, BudgetExecutionError, BudgetExecutionLease,
    BudgetLimits, BudgetReport, BudgetRequest, BudgetScope, TaskBudget, TaskRunReport,
};

use jingwei_core::{
    CancellationSignal, CapabilityId, DoneStatus, SessionEvent, SessionEventKind, SessionId,
    StageStatus, TurnId,
};
use jingwei_llm::{LlmError, ModelGateway, ModelGatewayError, ModelTurnFailure};
use jingwei_session::SessionRuntimeError;
use jingwei_tool::{ToolGateway, ToolRuntimeError, ToolTurnFailure};
use serde::{Deserialize, Serialize};

/// Singular capability supplied by an Agent turn runtime.
pub const AGENT_RUNTIME: CapabilityId = CapabilityId::new("jingwei.agent.runtime");

/// A boxed Agent-side asynchronous operation.
pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An outcome an Agent may resolve without owning the canonical terminal event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    WaitingForInput,
    Cancelled,
}

/// Complete semantic disposition observed by callers and finally hooks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnDisposition {
    Completed,
    WaitingForInput,
    Cancelled,
    Failed,
}

impl From<TurnOutcome> for TurnDisposition {
    fn from(value: TurnOutcome) -> Self {
        match value {
            TurnOutcome::Completed => Self::Completed,
            TurnOutcome::WaitingForInput => Self::WaitingForInput,
            TurnOutcome::Cancelled => Self::Cancelled,
        }
    }
}

/// Non-boundary events an Agent is allowed to append during its body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEventKind {
    AssistantDelta {
        text: String,
    },
    StageProgress {
        stage: String,
        status: StageStatus,
        message: Option<String>,
    },
    Mode {
        mode: String,
        decided_by: String,
        reason: String,
    },
    PlanReady {
        plan: serde_json::Value,
    },
    Custom {
        plugin: String,
        kind: String,
        payload: serde_json::Value,
    },
}

impl AgentEventKind {
    pub fn into_session_event_kind(self) -> SessionEventKind {
        match self {
            Self::AssistantDelta { text } => SessionEventKind::AssistantDelta { text },
            Self::StageProgress {
                stage,
                status,
                message,
            } => SessionEventKind::StageProgress {
                stage,
                status,
                message,
            },
            Self::Mode {
                mode,
                decided_by,
                reason,
            } => SessionEventKind::Mode {
                mode,
                decided_by,
                reason,
            },
            Self::PlanReady { plan } => SessionEventKind::PlanReady { plan },
            Self::Custom {
                plugin,
                kind,
                payload,
            } => SessionEventKind::Custom {
                plugin,
                kind,
                payload,
            },
        }
    }
}

/// Immutable inputs visible to one Agent invocation.
pub struct AgentTurnInput<'a> {
    pub session_id: &'a SessionId,
    pub turn_id: TurnId,
    pub user_message: &'a str,
    pub history: &'a [SessionEvent],
}

/// Semantic output returned to AgentRuntime. Agent code does not append its terminal event.
pub struct AgentTurnOutput {
    pub final_text: String,
    pub outcome: TurnOutcome,
    pub artifact: Option<serde_json::Value>,
}

/// Stable Agent-owned failure payload used to construct a canonical Error event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentFailure {
    code: String,
    message: String,
    retryable: bool,
}

impl AgentFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

/// A typed failure escaping an Agent body.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent cancelled")]
    Cancelled,
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("agent failed ({code}): {message}", code = .0.code(), message = .0.message())]
    Failed(AgentFailure),
    #[error("model runtime failed: {0}")]
    Model(ModelGatewayError),
    #[error("Tool runtime failed: {0}")]
    Tool(ToolRuntimeError),
    #[error(transparent)]
    CapabilityClosure(CapabilityTurnFailure),
    #[error("Session runtime failed: {0}")]
    Session(#[from] SessionRuntimeError),
}

impl AgentError {
    pub fn failed(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::Failed(AgentFailure::new(code, message, retryable))
    }

    /// Whether this typed Agent failure represents cancellation rather than failure.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Cancelled
                | Self::Model(ModelGatewayError::Model(LlmError::Cancelled))
                | Self::Tool(ToolRuntimeError::Cancelled { .. })
        )
    }
}

impl From<LlmError> for AgentError {
    fn from(error: LlmError) -> Self {
        Self::from(ModelGatewayError::from(error))
    }
}

impl From<ModelGatewayError> for AgentError {
    fn from(error: ModelGatewayError) -> Self {
        let error = Self::Model(error);
        if error.is_cancelled() {
            Self::Cancelled
        } else {
            error
        }
    }
}

impl From<ToolRuntimeError> for AgentError {
    fn from(error: ToolRuntimeError) -> Self {
        if matches!(error, ToolRuntimeError::Cancelled { .. }) {
            Self::Cancelled
        } else {
            Self::Tool(error)
        }
    }
}

impl From<ToolTurnFailure> for AgentError {
    fn from(error: ToolTurnFailure) -> Self {
        Self::CapabilityClosure(
            CapabilityTurnFailure::new(None, Some(error))
                .expect("a Tool closure failure is non-empty capability evidence"),
        )
    }
}

impl From<ModelTurnFailure> for AgentError {
    fn from(error: ModelTurnFailure) -> Self {
        Self::CapabilityClosure(
            CapabilityTurnFailure::new(Some(error), None)
                .expect("a model closure failure is non-empty capability evidence"),
        )
    }
}

/// Typed all-attempt evidence from closing the model and Tool scopes of one turn.
pub struct CapabilityTurnFailure {
    model: Option<ModelTurnFailure>,
    tool: Option<ToolTurnFailure>,
}

/// A capability closure aggregate must retain at least one failed scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("CapabilityTurnFailure requires a model or Tool closure failure")]
pub struct InvalidCapabilityTurnFailure;

impl CapabilityTurnFailure {
    pub fn new(
        model: Option<ModelTurnFailure>,
        tool: Option<ToolTurnFailure>,
    ) -> Result<Self, InvalidCapabilityTurnFailure> {
        if model.is_none() && tool.is_none() {
            return Err(InvalidCapabilityTurnFailure);
        }
        Ok(Self { model, tool })
    }

    pub fn model(&self) -> Option<&ModelTurnFailure> {
        self.model.as_ref()
    }

    pub fn tool(&self) -> Option<&ToolTurnFailure> {
        self.tool.as_ref()
    }
}

impl fmt::Debug for CapabilityTurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilityTurnFailure")
            .field("model_failed", &self.model.is_some())
            .field("tool_failed", &self.tool.is_some())
            .finish()
    }
}

impl fmt::Display for CapabilityTurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("one or more capability turn scopes failed to close")
    }
}

impl Error for CapabilityTurnFailure {}

/// Restricted consumption interface: no token evidence, limit grants or ledger replacement.
pub trait AgentBudget: Send + Sync {
    fn report(&self) -> Result<BudgetReport, BudgetError>;
    fn consume_step(&self) -> Result<(), BudgetError>;
    fn consume_correction(&self) -> Result<(), BudgetError>;
}

impl AgentBudget for BudgetScope {
    fn report(&self) -> Result<BudgetReport, BudgetError> {
        BudgetScope::report(self)
    }
    fn consume_step(&self) -> Result<(), BudgetError> {
        self.reserve(BudgetRequest::new(BudgetAmounts {
            steps: 1,
            ..BudgetAmounts::default()
        }))?
        .cancel_before_start()
    }
    fn consume_correction(&self) -> Result<(), BudgetError> {
        self.reserve(BudgetRequest::new(BudgetAmounts {
            corrections: 1,
            ..BudgetAmounts::default()
        }))?
        .cancel_before_start()
    }
}

/// Turn-scoped Agent authority.
pub trait AgentContext: Send + Sync {
    fn emit(&self, kind: AgentEventKind) -> AgentFuture<'_, Result<(), AgentError>>;

    fn emit_text_delta(&self, text: &str) -> AgentFuture<'_, Result<(), AgentError>> {
        self.emit(AgentEventKind::AssistantDelta {
            text: text.to_string(),
        })
    }

    fn model(&self) -> Option<&dyn ModelGateway>;

    fn tools(&self) -> Option<&dyn ToolGateway> {
        None
    }

    fn cancellation(&self) -> &dyn CancellationSignal;

    fn budget(&self) -> Option<&dyn AgentBudget> {
        None
    }
}

/// Pluggable Agent body.
pub trait Agent: Send + Sync {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>>;
}

/// Owned request transferred synchronously to AgentRuntime.
pub struct AgentTurnRequest {
    session_id: SessionId,
    agent_key: String,
    user_message: String,
    budget: Option<(TaskBudget, BudgetLimits)>,
    durable_budget: Option<BudgetExecutionLease>,
}

impl AgentTurnRequest {
    pub fn new(
        session_id: SessionId,
        agent_key: impl Into<String>,
        user_message: impl Into<String>,
    ) -> Self {
        Self {
            session_id,
            agent_key: agent_key.into(),
            user_message: user_message.into(),
            budget: None,
            durable_budget: None,
        }
    }

    #[must_use]
    pub fn with_budget(mut self, task: TaskBudget, run_limits: BudgetLimits) -> Self {
        self.durable_budget = None;
        self.budget = Some((task, run_limits));
        self
    }

    /// Transfer an already confirmed, unique durable claim to the runtime. Any
    /// rejection or abandoned execution retains the claim for explicit recovery.
    #[must_use]
    pub fn with_durable_budget(
        mut self,
        lease: BudgetExecutionLease,
        run_limits: BudgetLimits,
    ) -> Self {
        self.budget = Some((lease.task().clone(), run_limits));
        self.durable_budget = Some(lease);
        self
    }

    pub fn budget(&self) -> Option<&(TaskBudget, BudgetLimits)> {
        self.budget.as_ref()
    }

    pub fn into_budget_parts(
        self,
    ) -> (
        SessionId,
        String,
        String,
        Option<(TaskBudget, BudgetLimits)>,
        Option<BudgetExecutionLease>,
    ) {
        (
            self.session_id,
            self.agent_key,
            self.user_message,
            self.budget,
            self.durable_budget,
        )
    }

    pub fn into_parts(self) -> (SessionId, String, String) {
        (self.session_id, self.agent_key, self.user_message)
    }
}

/// Explicit, cloneable cancellation authority for one accepted request.
pub trait AgentTurnCanceller: Send + Sync {
    fn cancel(&self);
}

/// Owned completion handle. Dropping it or its waiter detaches without cancelling the turn.
pub trait AgentTurnController: Send {
    fn canceller(&self) -> Arc<dyn AgentTurnCanceller>;

    /// Observe owned execution/cleanup even while the completion waiter is detached.
    fn budget_report(&self) -> Option<Result<BudgetReport, BudgetError>> {
        None
    }

    fn wait(self: Box<Self>) -> AgentFuture<'static, Result<AgentTurnReport, AgentRuntimeError>>;
}

/// Selected behavior-rich runtime that owns accepted Agent work.
pub trait AgentRuntime: Send + Sync {
    fn start_turn(
        &self,
        request: AgentTurnRequest,
    ) -> Result<Box<dyn AgentTurnController>, AgentRuntimeError>;
}

/// A completed canonical turn report.
#[derive(Clone, Debug)]
pub struct AgentTurnReport {
    session_id: SessionId,
    turn_id: TurnId,
    disposition: TurnDisposition,
    final_text: String,
    artifact: Option<serde_json::Value>,
    events: Arc<[SessionEvent]>,
    task_run_report: Option<TaskRunReport>,
    budget_checkpoint: Option<BudgetCheckpoint>,
}

impl AgentTurnReport {
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        disposition: TurnDisposition,
        final_text: String,
        artifact: Option<serde_json::Value>,
        events: Arc<[SessionEvent]>,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            disposition,
            final_text,
            artifact,
            events,
            task_run_report: None,
            budget_checkpoint: None,
        }
    }

    #[must_use]
    pub fn with_task_run_report(mut self, report: TaskRunReport) -> Self {
        self.task_run_report = Some(report);
        self
    }

    pub fn task_run_report(&self) -> Option<&TaskRunReport> {
        self.task_run_report.as_ref()
    }

    /// Receipt of the separate final budget commit; None for in-memory turns.
    /// This is evidence, not authority to execute or proof of current freshness.
    pub fn budget_checkpoint(&self) -> Option<&BudgetCheckpoint> {
        self.budget_checkpoint.as_ref()
    }

    #[must_use]
    pub fn with_budget_checkpoint(mut self, checkpoint: BudgetCheckpoint) -> Self {
        self.budget_checkpoint = Some(checkpoint);
        self
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub const fn disposition(&self) -> TurnDisposition {
        self.disposition
    }

    pub fn final_text(&self) -> &str {
        &self.final_text
    }

    pub fn artifact(&self) -> Option<&serde_json::Value> {
        self.artifact.as_ref()
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }
}

/// Failure before a Session lease is delivered.
#[derive(Debug, thiserror::Error)]
pub enum NotAdmittedFailure {
    #[error("budget stopped before Session admission: {error}")]
    Budget {
        error: BudgetError,
        report: Box<BudgetReport>,
    },
    #[error("caller cancelled before Session admission")]
    CallerCancelled,
    #[error("AgentRuntime stopped before Session admission")]
    RuntimeStopping,
    #[error("Session admission failed: {0}")]
    Session(#[source] SessionRuntimeError),
}

/// Semantic failure that selected the canonical Error terminal intent.
#[derive(Debug)]
pub enum DriveFailure {
    StartEnvelope(SessionRuntimeError),
    Agent(AgentError),
    Panicked,
    BudgetReport { prior: Option<Box<DriveFailure>> },
}

/// The exact report payload and total persistence outcome, before the terminal.
#[derive(Debug)]
pub enum TaskRunReportAttempt {
    Committed {
        report: TaskRunReport,
        event: Arc<SessionEvent>,
    },
    Failed {
        report: TaskRunReport,
        source: SessionRuntimeError,
    },
    /// A mismatched Session lease was rejected before report persistence.
    Rejected {
        report: TaskRunReport,
        error: BudgetError,
    },
    /// The recorder returned evidence that does not confirm the attempted report.
    Invalid {
        report: TaskRunReport,
        event: Arc<SessionEvent>,
    },
}

/// Total outcome of the one terminal append attempt.
#[derive(Debug)]
pub enum TerminalAttempt {
    Committed(Arc<SessionEvent>),
    Failed(SessionRuntimeError),
}

/// Total outcome of the one settlement attempt.
#[derive(Debug)]
pub enum SettlementAttempt {
    Settled(Arc<[SessionEvent]>),
    Failed(SessionRuntimeError),
}

/// Closure status observed after terminal and settlement have both been attempted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnClosureStatus {
    Closed,
    TerminalFailed,
    SettlementFailed,
    TerminalAndSettlementFailed,
}

/// Immutable all-path finally observation supplied to Hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnFinally {
    session_id: SessionId,
    turn_id: TurnId,
    disposition: TurnDisposition,
    closure: TurnClosureStatus,
}

impl TurnFinally {
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        disposition: TurnDisposition,
        closure: TurnClosureStatus,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            disposition,
            closure,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub const fn disposition(&self) -> TurnDisposition {
        self.disposition
    }

    pub const fn closure(&self) -> TurnClosureStatus {
        self.closure
    }
}

/// Bounded structured failure after a Session lease has been admitted.
#[derive(Debug)]
pub struct TurnFailure {
    session_id: SessionId,
    turn_id: TurnId,
    disposition: TurnDisposition,
    final_text: String,
    artifact: Option<serde_json::Value>,
    report: Option<AgentTurnReport>,
    drive_failure: Option<DriveFailure>,
    task_run_report_attempt: Option<TaskRunReportAttempt>,
    terminal_attempt: TerminalAttempt,
    settlement_attempt: SettlementAttempt,
}

/// Semantic and identity fields paired with total closure evidence.
pub struct TurnFailureContext {
    session_id: SessionId,
    turn_id: TurnId,
    disposition: TurnDisposition,
    final_text: String,
    artifact: Option<serde_json::Value>,
    report: Option<AgentTurnReport>,
    drive_failure: Option<DriveFailure>,
    task_run_report_attempt: Option<TaskRunReportAttempt>,
}

impl TurnFailureContext {
    #[must_use]
    pub fn with_task_run_report_attempt(mut self, attempt: TaskRunReportAttempt) -> Self {
        self.task_run_report_attempt = Some(attempt);
        self
    }

    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        disposition: TurnDisposition,
        final_text: String,
        artifact: Option<serde_json::Value>,
        report: Option<AgentTurnReport>,
        drive_failure: Option<DriveFailure>,
    ) -> Self {
        Self {
            session_id,
            turn_id,
            disposition,
            final_text,
            artifact,
            report,
            drive_failure,
            task_run_report_attempt: None,
        }
    }
}

/// A rejected combination of semantic and closure evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidTurnFailure {
    #[error("TurnFailure has no semantic or closure failure")]
    MissingFailureSource,
    #[error("DriveFailure presence does not match the resolved disposition")]
    DriveFailureMismatch,
    #[error("the resolved disposition cannot carry the supplied artifact")]
    ArtifactMismatch,
    #[error("the committed terminal does not match the resolved turn identity or disposition")]
    TerminalMismatch,
    #[error("the settled event window contains an event outside the resolved turn identity")]
    SettlementMismatch,
    #[error("a fully closed turn must carry its canonical report")]
    MissingClosedReport,
    #[error("a closure-failed turn cannot carry a canonical report")]
    ReportBeforeClosure,
    #[error("the canonical report does not match the failure identity or disposition")]
    ReportMismatch,
}

impl TurnFailure {
    pub fn new(
        context: TurnFailureContext,
        terminal_attempt: TerminalAttempt,
        settlement_attempt: SettlementAttempt,
    ) -> Result<Self, InvalidTurnFailure> {
        let closure = closure_status(&terminal_attempt, &settlement_attempt);
        if context.disposition != TurnDisposition::Failed && closure == TurnClosureStatus::Closed {
            return Err(InvalidTurnFailure::MissingFailureSource);
        }
        if (context.disposition == TurnDisposition::Failed) != context.drive_failure.is_some()
            || matches!(
                context.drive_failure.as_ref(),
                Some(DriveFailure::Agent(error)) if error.is_cancelled()
            )
        {
            return Err(InvalidTurnFailure::DriveFailureMismatch);
        }
        if context.disposition == TurnDisposition::Failed && context.artifact.is_some() {
            return Err(InvalidTurnFailure::ArtifactMismatch);
        }
        if let TerminalAttempt::Committed(terminal) = &terminal_attempt
            && !terminal_matches_resolution(
                terminal,
                &context.session_id,
                &context.turn_id,
                context.disposition,
                context.artifact.as_ref(),
            )
        {
            return Err(InvalidTurnFailure::TerminalMismatch);
        }
        if let SettlementAttempt::Settled(events) = &settlement_attempt
            && events.iter().any(|event| {
                event.session_id != context.session_id || event.turn_id != context.turn_id
            })
        {
            return Err(InvalidTurnFailure::SettlementMismatch);
        }
        if matches!(&terminal_attempt, TerminalAttempt::Failed(_))
            && let SettlementAttempt::Settled(events) = &settlement_attempt
            && events.iter().any(|event| {
                matches!(
                    event.kind,
                    SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
                )
            })
        {
            return Err(InvalidTurnFailure::TerminalMismatch);
        }
        match (closure, context.report.as_ref()) {
            (TurnClosureStatus::Closed, None) => {
                return Err(InvalidTurnFailure::MissingClosedReport);
            }
            (TurnClosureStatus::Closed, Some(report))
                if report.session_id() != &context.session_id
                    || report.turn_id() != &context.turn_id
                    || report.disposition() != context.disposition
                    || report.final_text() != context.final_text
                    || report.artifact() != context.artifact.as_ref()
                    || !matches!(
                        (&terminal_attempt, &settlement_attempt),
                        (
                            TerminalAttempt::Committed(terminal),
                            SettlementAttempt::Settled(events),
                        ) if Arc::ptr_eq(&report.events, events)
                            && events.last() == Some(terminal.as_ref())
                            && events
                                .iter()
                                .filter(|event| matches!(
                                    event.kind,
                                    SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
                                ))
                                .count()
                                == 1
                    ) =>
            {
                return Err(InvalidTurnFailure::ReportMismatch);
            }
            (TurnClosureStatus::Closed, Some(_)) => {}
            (_, Some(_)) => return Err(InvalidTurnFailure::ReportBeforeClosure),
            (_, None) => {}
        }
        Ok(Self {
            session_id: context.session_id,
            turn_id: context.turn_id,
            disposition: context.disposition,
            final_text: context.final_text,
            artifact: context.artifact,
            report: context.report,
            drive_failure: context.drive_failure,
            task_run_report_attempt: context.task_run_report_attempt,
            terminal_attempt,
            settlement_attempt,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub const fn disposition(&self) -> TurnDisposition {
        self.disposition
    }

    pub fn final_text(&self) -> &str {
        &self.final_text
    }

    pub fn artifact(&self) -> Option<&serde_json::Value> {
        self.artifact.as_ref()
    }

    pub fn report(&self) -> Option<&AgentTurnReport> {
        self.report.as_ref()
    }

    pub fn task_run_report_attempt(&self) -> Option<&TaskRunReportAttempt> {
        self.task_run_report_attempt.as_ref()
    }

    pub fn drive_failure(&self) -> Option<&DriveFailure> {
        self.drive_failure.as_ref()
    }

    pub const fn terminal_attempt(&self) -> &TerminalAttempt {
        &self.terminal_attempt
    }

    pub const fn settlement_attempt(&self) -> &SettlementAttempt {
        &self.settlement_attempt
    }

    pub const fn canonical_events_complete(&self) -> bool {
        matches!(self.settlement_attempt, SettlementAttempt::Settled(_))
    }

    pub fn closure_status(&self) -> TurnClosureStatus {
        closure_status(&self.terminal_attempt, &self.settlement_attempt)
    }
}

fn terminal_matches_resolution(
    terminal: &SessionEvent,
    session_id: &SessionId,
    turn_id: &TurnId,
    disposition: TurnDisposition,
    artifact: Option<&serde_json::Value>,
) -> bool {
    if &terminal.session_id != session_id || &terminal.turn_id != turn_id {
        return false;
    }
    match (&terminal.kind, disposition) {
        (
            SessionEventKind::Done {
                status: DoneStatus::Completed,
                artifact: terminal_artifact,
            },
            TurnDisposition::Completed,
        )
        | (
            SessionEventKind::Done {
                status: DoneStatus::WaitingForInput,
                artifact: terminal_artifact,
            },
            TurnDisposition::WaitingForInput,
        )
        | (
            SessionEventKind::Done {
                status: DoneStatus::Cancelled,
                artifact: terminal_artifact,
            },
            TurnDisposition::Cancelled,
        ) => terminal_artifact.as_ref() == artifact,
        (SessionEventKind::Error { .. }, TurnDisposition::Failed) => artifact.is_none(),
        _ => false,
    }
}

fn closure_status(
    terminal_attempt: &TerminalAttempt,
    settlement_attempt: &SettlementAttempt,
) -> TurnClosureStatus {
    match (terminal_attempt, settlement_attempt) {
        (TerminalAttempt::Committed(_), SettlementAttempt::Settled(_)) => TurnClosureStatus::Closed,
        (TerminalAttempt::Failed(_), SettlementAttempt::Settled(_)) => {
            TurnClosureStatus::TerminalFailed
        }
        (TerminalAttempt::Committed(_), SettlementAttempt::Failed(_)) => {
            TurnClosureStatus::SettlementFailed
        }
        (TerminalAttempt::Failed(_), SettlementAttempt::Failed(_)) => {
            TurnClosureStatus::TerminalAndSettlementFailed
        }
    }
}

impl fmt::Display for TurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "turn {} in Session {} failed with disposition {:?}",
            self.turn_id, self.session_id, self.disposition
        )
    }
}

impl Error for TurnFailure {}

/// Synchronous admission or owned-turn completion failure.
#[derive(Debug, thiserror::Error)]
pub enum AgentRuntimeError {
    #[error("durable recovery boundary rejected: {source}")]
    RecoveryBoundary {
        source: BudgetExecutionError,
        settlement: Result<(), SessionRuntimeError>,
    },
    /// Session outcome is retained even when the separate checkpoint barrier fails.
    #[error("durable budget finalization failed: {source}")]
    Durability {
        source: BudgetExecutionError,
        outcome: Box<Result<AgentTurnReport, AgentRuntimeError>>,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("AgentRuntime is stopped")]
    Stopped,
    #[error("AgentRuntime admission capacity is exhausted")]
    Overloaded,
    #[error("agent `{key}` not found")]
    AgentNotFound { key: String },
    #[error(transparent)]
    NotAdmitted(#[from] NotAdmittedFailure),
    #[error(transparent)]
    Turn(#[from] Box<TurnFailure>),
}

#[cfg(test)]
mod tests {
    use jingwei_core::EventId;
    use serde_json::json;

    use super::*;

    #[derive(Clone, Copy, Debug)]
    enum InvalidEvidenceCase {
        DirectCancellationFailure,
        NestedCancellationFailure,
        TerminalIdentity,
        TerminalStatus,
        TerminalArtifact,
        FailedTerminalKind,
        FailedArtifact,
        SettlementIdentity,
        FailedTerminalContradictedBySettlement,
    }

    fn event(session_id: &str, turn_id: &str, kind: SessionEventKind) -> Arc<SessionEvent> {
        Arc::new(SessionEvent {
            event_id: EventId::new(),
            session_id: SessionId::from(session_id),
            turn_id: TurnId::from(turn_id),
            generation_id: None,
            message_id: None,
            seq: 0,
            kind,
        })
    }

    #[test]
    fn turn_failure_rejects_mismatched_terminal_and_settlement_evidence() {
        for case in [
            InvalidEvidenceCase::DirectCancellationFailure,
            InvalidEvidenceCase::NestedCancellationFailure,
            InvalidEvidenceCase::TerminalIdentity,
            InvalidEvidenceCase::TerminalStatus,
            InvalidEvidenceCase::TerminalArtifact,
            InvalidEvidenceCase::FailedTerminalKind,
            InvalidEvidenceCase::FailedArtifact,
            InvalidEvidenceCase::SettlementIdentity,
            InvalidEvidenceCase::FailedTerminalContradictedBySettlement,
        ] {
            let session_id = SessionId::from("context-session");
            let turn_id = TurnId::from("context-turn");
            let (
                disposition,
                artifact,
                drive_failure,
                terminal_attempt,
                settlement_attempt,
                expected,
            ) = match case {
                InvalidEvidenceCase::DirectCancellationFailure => (
                    TurnDisposition::Failed,
                    None,
                    Some(DriveFailure::Agent(AgentError::Cancelled)),
                    TerminalAttempt::Committed(event(
                        "context-session",
                        "context-turn",
                        SessionEventKind::Error {
                            code: "agent_cancelled".to_string(),
                            message: "agent cancelled".to_string(),
                            retryable: false,
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::DriveFailureMismatch,
                ),
                InvalidEvidenceCase::NestedCancellationFailure => (
                    TurnDisposition::Failed,
                    None,
                    Some(DriveFailure::Agent(AgentError::Model(
                        ModelGatewayError::Model(LlmError::Cancelled),
                    ))),
                    TerminalAttempt::Committed(event(
                        "context-session",
                        "context-turn",
                        SessionEventKind::Error {
                            code: "model_cancelled".to_string(),
                            message: "model cancelled".to_string(),
                            retryable: false,
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::DriveFailureMismatch,
                ),
                InvalidEvidenceCase::TerminalIdentity => (
                    TurnDisposition::Completed,
                    None,
                    None,
                    TerminalAttempt::Committed(event(
                        "other-session",
                        "context-turn",
                        SessionEventKind::Done {
                            status: DoneStatus::Completed,
                            artifact: None,
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::TerminalMismatch,
                ),
                InvalidEvidenceCase::TerminalStatus => (
                    TurnDisposition::Cancelled,
                    None,
                    None,
                    TerminalAttempt::Committed(event(
                        "context-session",
                        "context-turn",
                        SessionEventKind::Done {
                            status: DoneStatus::Completed,
                            artifact: None,
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::TerminalMismatch,
                ),
                InvalidEvidenceCase::TerminalArtifact => (
                    TurnDisposition::WaitingForInput,
                    Some(json!({ "expected": true })),
                    None,
                    TerminalAttempt::Committed(event(
                        "context-session",
                        "context-turn",
                        SessionEventKind::Done {
                            status: DoneStatus::WaitingForInput,
                            artifact: Some(json!({ "actual": true })),
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::TerminalMismatch,
                ),
                InvalidEvidenceCase::FailedTerminalKind => (
                    TurnDisposition::Failed,
                    None,
                    Some(DriveFailure::Panicked),
                    TerminalAttempt::Committed(event(
                        "context-session",
                        "context-turn",
                        SessionEventKind::Done {
                            status: DoneStatus::Completed,
                            artifact: None,
                        },
                    )),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::TerminalMismatch,
                ),
                InvalidEvidenceCase::FailedArtifact => (
                    TurnDisposition::Failed,
                    Some(json!({ "not": "valid for Error" })),
                    Some(DriveFailure::Panicked),
                    TerminalAttempt::Failed(SessionRuntimeError::Stopped),
                    SettlementAttempt::Failed(SessionRuntimeError::Stopped),
                    InvalidTurnFailure::ArtifactMismatch,
                ),
                InvalidEvidenceCase::SettlementIdentity => (
                    TurnDisposition::Completed,
                    None,
                    None,
                    TerminalAttempt::Failed(SessionRuntimeError::Stopped),
                    SettlementAttempt::Settled(
                        vec![
                            event(
                                "context-session",
                                "other-turn",
                                SessionEventKind::UserMessage {
                                    text: "hello".to_string(),
                                },
                            )
                            .as_ref()
                            .clone(),
                        ]
                        .into(),
                    ),
                    InvalidTurnFailure::SettlementMismatch,
                ),
                InvalidEvidenceCase::FailedTerminalContradictedBySettlement => (
                    TurnDisposition::Completed,
                    None,
                    None,
                    TerminalAttempt::Failed(SessionRuntimeError::Stopped),
                    SettlementAttempt::Settled(
                        vec![
                            event(
                                "context-session",
                                "context-turn",
                                SessionEventKind::Done {
                                    status: DoneStatus::Completed,
                                    artifact: None,
                                },
                            )
                            .as_ref()
                            .clone(),
                        ]
                        .into(),
                    ),
                    InvalidTurnFailure::TerminalMismatch,
                ),
            };
            let context = TurnFailureContext::new(
                session_id,
                turn_id,
                disposition,
                String::new(),
                artifact,
                None,
                drive_failure,
            );

            assert_eq!(
                TurnFailure::new(context, terminal_attempt, settlement_attempt)
                    .expect_err("mismatched public evidence must be rejected"),
                expected,
                "wrong rejection for {case:?}"
            );
        }
    }
}
