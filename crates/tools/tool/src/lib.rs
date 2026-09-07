//! Public contracts for controlled Tool discovery, execution, and canonical recording.
//!
//! Raw [`Tool`] implementations are deliberately hidden behind a turn-scoped [`ToolGateway`].
//! The selected Tool runtime owns policy, limits, cancellation, and the exact `ToolCall` /
//! `ToolResult` pair recorded through [`ToolEventRecorder`].

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use jingwei_budget::{BudgetError, BudgetReport, BudgetScope};
use jingwei_core::{CancellationSignal, CapabilityId, SessionEvent, SessionEventKind};
pub use jingwei_core::{ToolCall, ToolFailureCategory, ToolRecordedOutcome, ToolResult};
use jingwei_session::SessionRuntimeError;
use serde::{Deserialize, Serialize};

/// The singular capability implemented by Tool runtime providers.
pub const TOOL_RUNTIME: CapabilityId = CapabilityId::new("jingwei.tool.runtime");

/// A boxed asynchronous Tool operation.
pub type ToolFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Hard pre-recording limit for a requested Tool name.
pub const MAX_TOOL_NAME_BYTES: usize = 256;

/// Hard pre-recording limit for serialized Tool arguments.
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 1_048_576;

/// Maximum number of closure failures retained by a turn finisher.
pub const MAX_TOOL_TURN_FAILURES: usize = 16;

/// One model-visible Tool schema.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Whether a Tool invocation requires a keyed approval provider.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "key", rename_all = "snake_case")]
pub enum ApprovalRequirement {
    #[default]
    None,
    Required(String),
}

impl ApprovalRequirement {
    pub fn required(key: impl Into<String>) -> Self {
        Self::Required(key.into())
    }

    pub fn key(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Required(key) => Some(key),
        }
    }
}

/// Static metadata supplied by a raw Tool implementation.
#[derive(Clone, Debug)]
pub struct ToolMetadata {
    description: String,
    input_schema: serde_json::Value,
    timeout_ceiling: Option<Duration>,
    output_ceiling_bytes: Option<usize>,
    approval: ApprovalRequirement,
}

impl ToolMetadata {
    pub fn new(description: impl Into<String>, input_schema: serde_json::Value) -> Self {
        Self {
            description: description.into(),
            input_schema,
            timeout_ceiling: None,
            output_ceiling_bytes: None,
            approval: ApprovalRequirement::None,
        }
    }

    #[must_use]
    pub fn with_timeout_ceiling(mut self, timeout: Duration) -> Self {
        self.timeout_ceiling = Some(timeout);
        self
    }

    #[must_use]
    pub fn with_output_ceiling(mut self, bytes: usize) -> Self {
        self.output_ceiling_bytes = Some(bytes);
        self
    }

    #[must_use]
    pub fn with_approval(mut self, approval: ApprovalRequirement) -> Self {
        self.approval = approval;
        self
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn input_schema(&self) -> &serde_json::Value {
        &self.input_schema
    }

    pub fn timeout_ceiling(&self) -> Option<Duration> {
        self.timeout_ceiling
    }

    pub fn output_ceiling_bytes(&self) -> Option<usize> {
        self.output_ceiling_bytes
    }

    pub fn approval(&self) -> &ApprovalRequirement {
        &self.approval
    }
}

/// Runtime-owned identity and arguments passed to a raw Tool body.
#[derive(Clone, Copy, Debug)]
pub struct ToolBodyRequest<'a> {
    call_id: &'a str,
    arguments: &'a serde_json::Value,
}

impl<'a> ToolBodyRequest<'a> {
    pub fn new(call_id: &'a str, arguments: &'a serde_json::Value) -> Self {
        Self { call_id, arguments }
    }

    pub fn call_id(&self) -> &'a str {
        self.call_id
    }

    pub fn arguments(&self) -> &'a serde_json::Value {
        self.arguments
    }
}

/// A provider diagnostic which the canonical runtime will budget before recording.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolDiagnostic {
    code: String,
    message: String,
    retryable: bool,
}

impl ToolDiagnostic {
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

    pub fn retryable(&self) -> bool {
        self.retryable
    }
}

macro_rules! diagnostic_error {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(ToolDiagnostic);

        impl $name {
            pub fn new(
                code: impl Into<String>,
                message: impl Into<String>,
                retryable: bool,
            ) -> Self {
                Self(ToolDiagnostic::new(code, message, retryable))
            }

            pub fn diagnostic(&self) -> &ToolDiagnostic {
                &self.0
            }

            pub fn into_diagnostic(self) -> ToolDiagnostic {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "{} `{}`: {}",
                    $label, self.0.code, self.0.message
                )
            }
        }

        impl Error for $name {}
    };
}

diagnostic_error!(ToolBodyError, "Tool body failure");
diagnostic_error!(ToolDenial, "Tool denied");
diagnostic_error!(ToolGuardError, "Tool guard failure");
diagnostic_error!(ToolAuthorizationError, "Tool authorizer failure");

/// A raw Tool implementation. Only ToolRuntime invokes this seam.
pub trait Tool: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    fn execute<'a>(
        &'a self,
        request: ToolBodyRequest<'a>,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> ToolFuture<'a, Result<String, ToolBodyError>>;
}

/// Kernel-owned identity of the Agent plugin using a turn gateway.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolCaller(String);

impl ToolCaller {
    pub fn new(owner: impl Into<String>) -> Self {
        Self(owner.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ToolCaller {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolCaller {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// The only canonical event families a Tool runtime may request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRecord {
    Call(ToolCall),
    Result(ToolResult),
}

impl ToolRecord {
    pub fn into_session_event_kind(self) -> SessionEventKind {
        match self {
            Self::Call(call) => SessionEventKind::ToolCall { call },
            Self::Result(result) => SessionEventKind::ToolResult { result },
        }
    }
}

/// Narrow writer into the host-owned canonical Session turn.
pub trait ToolEventRecorder: Send + Sync {
    fn append(
        &self,
        record: ToolRecord,
    ) -> ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>>;
}

/// Trusted, owned host binding for one Tool turn.
pub struct ToolTurnBinding {
    caller: ToolCaller,
    cancellation: Arc<dyn CancellationSignal>,
    recorder: Arc<dyn ToolEventRecorder>,
    budget: Option<BudgetScope>,
}

impl ToolTurnBinding {
    pub fn new(
        caller: ToolCaller,
        cancellation: Arc<dyn CancellationSignal>,
        recorder: Arc<dyn ToolEventRecorder>,
    ) -> Self {
        Self {
            caller,
            cancellation,
            recorder,
            budget: None,
        }
    }

    pub fn caller(&self) -> &ToolCaller {
        &self.caller
    }

    pub fn cancellation(&self) -> Arc<dyn CancellationSignal> {
        Arc::clone(&self.cancellation)
    }

    pub fn recorder(&self) -> Arc<dyn ToolEventRecorder> {
        Arc::clone(&self.recorder)
    }

    /// Attach host-owned consumption authority shared with the Agent and model turn.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetScope) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn budget(&self) -> Option<&BudgetScope> {
        self.budget.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        ToolCaller,
        Arc<dyn CancellationSignal>,
        Arc<dyn ToolEventRecorder>,
    ) {
        (self.caller, self.cancellation, self.recorder)
    }

    pub fn into_budget_parts(
        self,
    ) -> (
        ToolCaller,
        Arc<dyn CancellationSignal>,
        Arc<dyn ToolEventRecorder>,
        Option<BudgetScope>,
    ) {
        (self.caller, self.cancellation, self.recorder, self.budget)
    }
}

/// Per-call limits. A runtime may only tighten these against configured ceilings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ToolCallOptions {
    /// Host-assigned origin, not an authorization or provider-controlled identity.
    pub action: Option<jingwei_core::ActionContext>,
    pub timeout: Option<Duration>,
    pub max_output_bytes: Option<usize>,
}

/// Why supplied committed evidence cannot describe one Tool execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum InvalidToolExecution {
    #[error("ToolCall and ToolResult call IDs differ")]
    CallIdMismatch,
    #[error("committed call event does not contain the supplied ToolCall")]
    CallEventMismatch,
    #[error("committed result event does not contain the supplied ToolResult")]
    ResultEventMismatch,
    #[error("committed Tool events belong to different Sessions")]
    DifferentSession,
    #[error("committed Tool events belong to different turns")]
    DifferentTurn,
    #[error("committed ToolResult does not follow its ToolCall")]
    ReversedSequence,
}

/// The exact online and replay evidence for one completed semantic Tool invocation.
#[derive(Clone, Debug)]
pub struct ToolExecution {
    call: ToolCall,
    result: ToolResult,
    call_event: Arc<SessionEvent>,
    result_event: Arc<SessionEvent>,
}

impl ToolExecution {
    pub fn new(
        call: ToolCall,
        result: ToolResult,
        call_event: Arc<SessionEvent>,
        result_event: Arc<SessionEvent>,
    ) -> Result<Self, InvalidToolExecution> {
        if call.id != result.call_id {
            return Err(InvalidToolExecution::CallIdMismatch);
        }
        if call_event.kind != (SessionEventKind::ToolCall { call: call.clone() }) {
            return Err(InvalidToolExecution::CallEventMismatch);
        }
        if result_event.kind
            != (SessionEventKind::ToolResult {
                result: result.clone(),
            })
        {
            return Err(InvalidToolExecution::ResultEventMismatch);
        }
        if call_event.session_id != result_event.session_id {
            return Err(InvalidToolExecution::DifferentSession);
        }
        if call_event.turn_id != result_event.turn_id {
            return Err(InvalidToolExecution::DifferentTurn);
        }
        if call_event.seq >= result_event.seq {
            return Err(InvalidToolExecution::ReversedSequence);
        }
        Ok(Self {
            call,
            result,
            call_event,
            result_event,
        })
    }

    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    pub fn result(&self) -> &ToolResult {
        &self.result
    }

    pub fn call_event(&self) -> &Arc<SessionEvent> {
        &self.call_event
    }

    pub fn result_event(&self) -> &Arc<SessionEvent> {
        &self.result_event
    }

    pub fn into_parts(self) -> (ToolCall, ToolResult, Arc<SessionEvent>, Arc<SessionEvent>) {
        (self.call, self.result, self.call_event, self.result_event)
    }
}

/// Read-only context supplied to every deny-only guard.
#[derive(Clone, Copy, Debug)]
pub struct ToolGuardRequest<'a> {
    caller: &'a ToolCaller,
    call: &'a ToolCall,
}

impl<'a> ToolGuardRequest<'a> {
    pub fn new(caller: &'a ToolCaller, call: &'a ToolCall) -> Self {
        Self { caller, call }
    }

    pub fn caller(&self) -> &'a ToolCaller {
        self.caller
    }

    pub fn call(&self) -> &'a ToolCall {
        self.call
    }
}

/// A policy plugin which may abstain or deny, but can never grant.
pub trait ToolGuard: Send + Sync {
    fn evaluate<'a>(
        &'a self,
        request: ToolGuardRequest<'a>,
    ) -> ToolFuture<'a, Result<Option<ToolDenial>, ToolGuardError>>;
}

/// Read-only context supplied to the Tool's selected keyed authorizer.
#[derive(Clone, Copy, Debug)]
pub struct ToolAuthorizationRequest<'a> {
    caller: &'a ToolCaller,
    call: &'a ToolCall,
}

impl<'a> ToolAuthorizationRequest<'a> {
    pub fn new(caller: &'a ToolCaller, call: &'a ToolCall) -> Self {
        Self { caller, call }
    }

    pub fn caller(&self) -> &'a ToolCaller {
        self.caller
    }

    pub fn call(&self) -> &'a ToolCall {
        self.call
    }
}

/// Fail-closed decision from one keyed approval provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAuthorizationDecision {
    Approved,
    Denied(ToolDenial),
}

/// A keyed approval provider selected by Tool metadata.
pub trait ToolAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        request: ToolAuthorizationRequest<'a>,
    ) -> ToolFuture<'a, Result<ToolAuthorizationDecision, ToolAuthorizationError>>;
}

/// Host-only settlement mode for accepted Tool work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolFinishMode {
    Graceful,
    Cancel,
}

/// Rejection which is guaranteed to occur before reservation or event recording.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolPreflightError {
    #[error("Tool name must not be empty")]
    EmptyName,
    #[error("Tool name is {actual} bytes, exceeding the hard limit of {max}")]
    NameTooLong { actual: usize, max: usize },
    #[error("serialized Tool arguments are {actual} bytes, exceeding the hard limit of {max}")]
    ArgumentsTooLarge { actual: usize, max: usize },
}

/// Which canonical Tool event could not be recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRecordStage {
    Call,
    Result,
}

/// Evidence retained when an append does not confirm the bound canonical event.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ToolRecordError {
    #[error("Session runtime rejected the canonical Tool event")]
    Session(#[source] SessionRuntimeError),
    #[error("canonical Tool recorder panicked")]
    RecorderPanicked,
    #[error("canonical Tool recorder returned an event outside its budget binding")]
    IdentityMismatch { event: Arc<SessionEvent> },
}

/// Exact evidence retained when an accepted Tool operation cannot be canonically closed.
#[derive(Clone, Debug, thiserror::Error)]
#[error("failed to record canonical Tool event at {stage:?} stage")]
pub struct ToolClosureFailure {
    call: ToolCall,
    result: Option<ToolResult>,
    stage: ToolRecordStage,
    #[source]
    source: ToolRecordError,
}

impl ToolClosureFailure {
    pub fn call(call: ToolCall, source: SessionRuntimeError) -> Self {
        Self::call_error(call, ToolRecordError::Session(source))
    }

    pub fn call_error(call: ToolCall, source: ToolRecordError) -> Self {
        Self {
            call,
            result: None,
            stage: ToolRecordStage::Call,
            source,
        }
    }

    pub fn result(call: ToolCall, result: ToolResult, source: SessionRuntimeError) -> Self {
        Self::result_error(call, result, ToolRecordError::Session(source))
    }

    pub fn result_error(call: ToolCall, result: ToolResult, source: ToolRecordError) -> Self {
        Self {
            call,
            result: Some(result),
            stage: ToolRecordStage::Result,
            source,
        }
    }

    pub fn call_evidence(&self) -> &ToolCall {
        &self.call
    }

    pub fn result_evidence(&self) -> Option<&ToolResult> {
        self.result.as_ref()
    }

    pub fn stage(&self) -> ToolRecordStage {
        self.stage
    }

    pub fn record_error(&self) -> &ToolRecordError {
        &self.source
    }

    pub fn source_error(&self) -> Option<&SessionRuntimeError> {
        match &self.source {
            ToolRecordError::Session(error) => Some(error),
            ToolRecordError::RecorderPanicked | ToolRecordError::IdentityMismatch { .. } => None,
        }
    }
}

/// Infrastructure failure from ToolRuntime. Semantic Tool failures live in `ToolExecution`.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ToolRuntimeError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("Tool runtime is stopped")]
    Stopped,
    #[error("Tool turn is closed")]
    TurnClosed,
    #[error("Tool runtime admission is overloaded")]
    Overloaded,
    #[error(transparent)]
    Preflight(#[from] ToolPreflightError),
    #[error("accepted Tool execution was cancelled")]
    Cancelled { execution: Box<ToolExecution> },
    #[error("canonical Tool recording failed: {0}")]
    Recording(Arc<ToolClosureFailure>),
    #[error("Tool runtime internal failure `{code}`: {message}")]
    Internal { code: String, message: String },
}

impl ToolRuntimeError {
    pub fn cancelled(execution: ToolExecution) -> Self {
        Self::Cancelled {
            execution: Box::new(execution),
        }
    }

    pub fn cancelled_execution(&self) -> Option<&ToolExecution> {
        match self {
            Self::Cancelled { execution } => Some(execution),
            _ => None,
        }
    }
}

impl From<Arc<ToolClosureFailure>> for ToolRuntimeError {
    fn from(value: Arc<ToolClosureFailure>) -> Self {
        Self::Recording(value)
    }
}

/// Bounded aggregate returned only after Tool turn admission has closed and drained.
#[derive(Clone, Debug)]
pub struct ToolTurnFailure {
    failures: Arc<[ToolRuntimeError]>,
    omitted: usize,
}

/// A semantic or admission error cannot be presented as host drain evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("ToolTurnFailure may contain only recording or internal closure failures")]
pub struct InvalidToolTurnFailure;

impl ToolTurnFailure {
    pub fn from_failures(
        failures: Vec<ToolRuntimeError>,
    ) -> Result<Option<Self>, InvalidToolTurnFailure> {
        Self::from_bounded_failures(failures, 0)
    }

    /// Build an aggregate from retained closure failures plus a count whose details were omitted.
    pub fn from_bounded_failures(
        mut failures: Vec<ToolRuntimeError>,
        omitted: usize,
    ) -> Result<Option<Self>, InvalidToolTurnFailure> {
        if failures.iter().any(|failure| {
            !matches!(
                failure,
                ToolRuntimeError::Recording(_) | ToolRuntimeError::Internal { .. }
            )
        }) {
            return Err(InvalidToolTurnFailure);
        }
        if failures.is_empty() {
            return if omitted == 0 {
                Ok(None)
            } else {
                Err(InvalidToolTurnFailure)
            };
        }
        let omitted = omitted.saturating_add(failures.len().saturating_sub(MAX_TOOL_TURN_FAILURES));
        failures.truncate(MAX_TOOL_TURN_FAILURES);
        Ok(Some(Self {
            failures: failures.into(),
            omitted,
        }))
    }

    pub fn failures(&self) -> &[ToolRuntimeError] {
        &self.failures
    }

    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn total_count(&self) -> usize {
        self.failures.len().saturating_add(self.omitted)
    }
}

impl fmt::Display for ToolTurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} Tool operation(s) failed while closing the turn",
            self.total_count()
        )
    }
}

impl Error for ToolTurnFailure {}

/// Turn-scoped discovery and execution seam exposed to an Agent.
pub trait ToolGateway: Send + Sync {
    fn schemas(&self) -> &[ToolSchema];

    fn call_with_options(
        &self,
        name: &str,
        arguments: serde_json::Value,
        options: ToolCallOptions,
    ) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>>;

    fn call(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
        self.call_with_options(name, arguments, ToolCallOptions::default())
    }
}

/// Host-owned Tool scope for one admitted Agent turn.
pub trait ToolTurn: Send + Sync {
    fn budget_report(&self) -> Option<Result<BudgetReport, BudgetError>> {
        None
    }

    fn gateway(&self) -> &dyn ToolGateway;

    fn finish(
        self: Box<Self>,
        mode: ToolFinishMode,
    ) -> ToolFuture<'static, Result<(), ToolTurnFailure>>;
}

/// Selected Tool execution runtime.
pub trait ToolRuntime: Send + Sync {
    fn bind_turn(&self, binding: ToolTurnBinding) -> Result<Box<dyn ToolTurn>, ToolRuntimeError>;
}
