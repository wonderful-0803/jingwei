//! Llm 能力族接口。
//!
//! 设计纪律（吸收 writing-rust 的教训）：
//! - 错误词汇不得携带 reqwest/HTTP 具体类型（`ModelGatewayError` 携带
//!   `reqwest::Error` 是反面教材）；
//! - 原始生成接口都显式接收取消令牌，适配器不得静默丢弃；
//! - 文本与结构化输出共用请求、响应和类型化流式协议。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use jingwei_budget::{BudgetError, BudgetReport, BudgetScope};
use jingwei_core::{CancellationSignal, CapabilityId, SessionEvent, SessionEventKind};
use jingwei_session::SessionRuntimeError;
use tokio_util::sync::CancellationToken;

mod budget;
mod scheduling;
pub use budget::*;
pub use scheduling::*;

pub use jingwei_core::model::*;
pub use jingwei_core::{
    ModelCallMode, ModelFailureCategory, ModelRecordedOutcome, ModelRequest, ModelRequestOptions,
    ModelResult, ModelTimeout,
};

/// The singular capability implemented by model provider plugins.
pub const LLM_PROVIDER: CapabilityId = CapabilityId::new("jingwei.llm.provider");

/// The singular controlled model runtime capability.
pub const LLM_RUNTIME: CapabilityId = CapabilityId::new("jingwei.llm.runtime");

/// 单次调用参数。可选字段沿用运行时/提供方配置，收集上限始终显式生效。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GenerationOptions {
    /// Host-assigned correlation, recorded but never sent as model input.
    pub context: Option<jingwei_core::DecisionContext>,
    pub max_tokens: Option<u32>,
    /// Canonical runtime: None inherits its finite timeout; explicit values can only
    /// tighten it. The deadline starts at admission, before queueing and recording.
    /// Raw providers still receive this effective duration unchanged.
    pub timeout: Option<Duration>,
    pub limits: GenerationLimits,
}

/// 模型错误词汇（框架级分类，与传输无关）。
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LlmError {
    #[error("upstream status {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("request timed out")]
    Timeout,
    #[error("stream parse failed: {0}")]
    StreamParse(String),
    #[error("model protocol rejected: {0}")]
    Protocol(#[from] ModelProtocolError),
    /// 取消是一等错误，不是旁路（不变式 4）。
    #[error("cancelled")]
    Cancelled,
    #[error("adapter failed: {0}")]
    Adapter(String),
}

/// The only canonical event families a controlled model runtime may request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRecord {
    Request(ModelRequest),
    Result(ModelResult),
}

impl ModelRecord {
    pub fn into_session_event_kind(self) -> SessionEventKind {
        match self {
            Self::Request(request) => SessionEventKind::ModelRequest { request },
            Self::Result(result) => SessionEventKind::ModelResult { result },
        }
    }
}

/// Narrow writer into the host-owned canonical Session turn.
pub trait ModelEventRecorder: Send + Sync {
    fn append(
        &self,
        record: ModelRecord,
    ) -> ModelFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>>;
}

/// Trusted, owned host binding for one model turn.
pub struct ModelTurnBinding {
    cancellation: Arc<dyn CancellationSignal>,
    recorder: Arc<dyn ModelEventRecorder>,
    budget: Option<BudgetScope>,
}

impl ModelTurnBinding {
    pub fn new(
        cancellation: Arc<dyn CancellationSignal>,
        recorder: Arc<dyn ModelEventRecorder>,
    ) -> Self {
        Self {
            cancellation,
            recorder,
            budget: None,
        }
    }

    pub fn cancellation(&self) -> Arc<dyn CancellationSignal> {
        Arc::clone(&self.cancellation)
    }

    pub fn recorder(&self) -> Arc<dyn ModelEventRecorder> {
        Arc::clone(&self.recorder)
    }

    /// Bind the host-owned run shared with Agent and Tool runtimes.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetScope) -> Self {
        self.budget = Some(budget);
        self
    }

    pub fn budget(&self) -> Option<&BudgetScope> {
        self.budget.as_ref()
    }

    pub fn into_parts(self) -> (Arc<dyn CancellationSignal>, Arc<dyn ModelEventRecorder>) {
        (self.cancellation, self.recorder)
    }

    /// Runtime implementations must use this method to retain budget authority.
    pub fn into_budget_parts(
        self,
    ) -> (
        Arc<dyn CancellationSignal>,
        Arc<dyn ModelEventRecorder>,
        Option<BudgetScope>,
    ) {
        (self.cancellation, self.recorder, self.budget)
    }
}

/// Host-only settlement mode for accepted model work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelFinishMode {
    Graceful,
    Cancel,
}

/// Which canonical model event could not be recorded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRecordStage {
    Request,
    Result,
}

/// Why a canonical model record attempt did not produce confirmed evidence.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ModelRecordFailure {
    #[error("Session runtime rejected the canonical model event")]
    Session(#[source] SessionRuntimeError),
    #[error("canonical model recorder panicked")]
    RecorderPanicked,
    #[error("canonical model event does not match its budget binding")]
    Budget(#[source] BudgetError),
}

impl ModelRecordFailure {
    pub fn source_error(&self) -> Option<&SessionRuntimeError> {
        match self {
            Self::Session(source) => Some(source),
            Self::RecorderPanicked | Self::Budget(_) => None,
        }
    }
}

/// Exact evidence retained when an accepted model operation cannot be canonically closed.
#[derive(Clone, thiserror::Error)]
#[error("failed to record canonical model event at {stage:?} stage")]
pub struct ModelClosureFailure {
    request: ModelRequest,
    result: Option<ModelResult>,
    stage: ModelRecordStage,
    #[source]
    source: ModelRecordFailure,
}

impl fmt::Debug for ModelClosureFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let result_status = self.result.as_ref().map(|result| match &result.outcome {
            ModelRecordedOutcome::Succeeded { .. } => "succeeded",
            ModelRecordedOutcome::Failed { category, .. } => match category {
                ModelFailureCategory::Upstream => "failed_upstream",
                ModelFailureCategory::Timeout => "failed_timeout",
                ModelFailureCategory::StreamParse => "failed_stream_parse",
                ModelFailureCategory::Protocol => "failed_protocol",
                ModelFailureCategory::Cancelled => "failed_cancelled",
                ModelFailureCategory::Adapter => "failed_adapter",
                ModelFailureCategory::Internal => "failed_internal",
                ModelFailureCategory::Budget => "failed_budget",
            },
        });
        let source = match self.source {
            ModelRecordFailure::Session(_) => "session",
            ModelRecordFailure::RecorderPanicked => "recorder_panicked",
            ModelRecordFailure::Budget(_) => "budget",
        };
        formatter
            .debug_struct("ModelClosureFailure")
            .field("call_id", &self.request.call_id)
            .field("stage", &self.stage)
            .field("result_status", &result_status)
            .field("source", &source)
            .finish()
    }
}

impl ModelClosureFailure {
    pub fn request(request: ModelRequest, source: SessionRuntimeError) -> Self {
        Self::request_failure(request, ModelRecordFailure::Session(source))
    }

    pub fn result(request: ModelRequest, result: ModelResult, source: SessionRuntimeError) -> Self {
        Self::result_failure(request, result, ModelRecordFailure::Session(source))
    }

    pub fn request_recorder_panicked(request: ModelRequest) -> Self {
        Self::request_failure(request, ModelRecordFailure::RecorderPanicked)
    }

    pub fn result_recorder_panicked(request: ModelRequest, result: ModelResult) -> Self {
        Self::result_failure(request, result, ModelRecordFailure::RecorderPanicked)
    }

    pub fn request_budget_failure(request: ModelRequest, source: BudgetError) -> Self {
        Self::request_failure(request, ModelRecordFailure::Budget(source))
    }

    pub fn result_budget_failure(
        request: ModelRequest,
        result: ModelResult,
        source: BudgetError,
    ) -> Self {
        Self::result_failure(request, result, ModelRecordFailure::Budget(source))
    }

    fn request_failure(request: ModelRequest, source: ModelRecordFailure) -> Self {
        Self {
            request,
            result: None,
            stage: ModelRecordStage::Request,
            source,
        }
    }

    fn result_failure(
        request: ModelRequest,
        result: ModelResult,
        source: ModelRecordFailure,
    ) -> Self {
        Self {
            request,
            result: Some(result),
            stage: ModelRecordStage::Result,
            source,
        }
    }

    pub fn request_evidence(&self) -> &ModelRequest {
        &self.request
    }

    pub fn result_evidence(&self) -> Option<&ModelResult> {
        self.result.as_ref()
    }

    pub fn stage(&self) -> ModelRecordStage {
        self.stage
    }

    pub fn record_failure(&self) -> &ModelRecordFailure {
        &self.source
    }

    pub fn source_error(&self) -> Option<&SessionRuntimeError> {
        self.source.source_error()
    }
}

/// Turn binding/admission failure from the selected model runtime.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelRuntimeError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("model runtime is stopped")]
    Stopped,
    #[error("model turn is closed")]
    TurnClosed,
    #[error("model scheduler is overloaded: {capacity:?}")]
    Overloaded { capacity: ModelOverloadKind },
    #[error("model request exceeds {limit_bytes} serialized bytes")]
    RequestTooLarge { limit_bytes: usize },
    #[error("model request timed out waiting for an execution slot")]
    QueueTimeout,
    #[error("model timeout exceeds the executor clock range")]
    InvalidTimeout,
    #[error("model runtime internal failure `{code}`: {message}")]
    Internal { code: String, message: String },
}

/// Controlled model call failure exposed through [`ModelGateway`].
#[derive(Clone, thiserror::Error)]
pub enum ModelGatewayError {
    #[error(transparent)]
    Model(#[from] LlmError),
    #[error(transparent)]
    Runtime(#[from] ModelRuntimeError),
    #[error("canonical model recording failed: {0}")]
    Recording(Arc<ModelClosureFailure>),
    #[error("model runtime internal failure `{code}`: {message}")]
    Internal { code: String, message: String },
}

impl fmt::Debug for ModelGatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ModelGatewayError");
        match self {
            Self::Runtime(ModelRuntimeError::Budget(error)) => {
                debug.field("kind", &"model_budget").field("error", error)
            }
            Self::Model(LlmError::Upstream { status, .. }) => debug
                .field("kind", &"model_upstream")
                .field("status", status),
            Self::Model(LlmError::Timeout) => debug.field("kind", &"model_timeout"),
            Self::Model(LlmError::StreamParse(_)) => debug.field("kind", &"model_stream_parse"),
            Self::Model(LlmError::Protocol(_)) => debug.field("kind", &"model_protocol"),
            Self::Model(LlmError::Cancelled) => debug.field("kind", &"model_cancelled"),
            Self::Model(LlmError::Adapter(_)) => debug.field("kind", &"model_adapter"),
            Self::Runtime(ModelRuntimeError::Stopped) => {
                debug.field("kind", &"model_runtime_stopped")
            }
            Self::Runtime(ModelRuntimeError::TurnClosed) => {
                debug.field("kind", &"model_turn_closed")
            }
            Self::Runtime(ModelRuntimeError::Overloaded { capacity }) => debug
                .field("kind", &"model_overloaded")
                .field("capacity", capacity),
            Self::Runtime(ModelRuntimeError::RequestTooLarge { limit_bytes }) => debug
                .field("kind", &"model_request_too_large")
                .field("limit_bytes", limit_bytes),
            Self::Runtime(ModelRuntimeError::QueueTimeout) => {
                debug.field("kind", &"model_queue_timeout")
            }
            Self::Runtime(ModelRuntimeError::InvalidTimeout) => {
                debug.field("kind", &"model_invalid_timeout")
            }
            Self::Runtime(ModelRuntimeError::Internal { code, .. }) => debug
                .field("kind", &"model_runtime_internal")
                .field("code", code),
            Self::Recording(failure) => debug
                .field("kind", &"model_recording")
                .field("failure", failure),
            Self::Internal { code, .. } => debug
                .field("kind", &"model_runtime_internal")
                .field("code", code),
        };
        debug.finish()
    }
}

impl From<Arc<ModelClosureFailure>> for ModelGatewayError {
    fn from(value: Arc<ModelClosureFailure>) -> Self {
        Self::Recording(value)
    }
}

const MAX_MODEL_TURN_FAILURES: usize = 16;

/// Bounded aggregate returned only after model turn admission has closed and drained.
#[derive(Clone)]
pub struct ModelTurnFailure {
    failures: Arc<[ModelGatewayError]>,
    omitted: usize,
}

impl fmt::Debug for ModelTurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelTurnFailure")
            .field("failure_count", &self.failures.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}

/// A semantic or admission error cannot be presented as host drain evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("ModelTurnFailure may contain only recording or internal closure failures")]
pub struct InvalidModelTurnFailure;

impl ModelTurnFailure {
    pub fn from_failures(
        failures: Vec<ModelGatewayError>,
    ) -> Result<Option<Self>, InvalidModelTurnFailure> {
        Self::from_bounded_failures(failures, 0)
    }

    pub fn from_bounded_failures(
        mut failures: Vec<ModelGatewayError>,
        omitted: usize,
    ) -> Result<Option<Self>, InvalidModelTurnFailure> {
        if failures.iter().any(|failure| {
            !matches!(
                failure,
                ModelGatewayError::Recording(_) | ModelGatewayError::Internal { .. }
            )
        }) {
            return Err(InvalidModelTurnFailure);
        }
        if failures.is_empty() {
            return if omitted == 0 {
                Ok(None)
            } else {
                Err(InvalidModelTurnFailure)
            };
        }
        let omitted =
            omitted.saturating_add(failures.len().saturating_sub(MAX_MODEL_TURN_FAILURES));
        failures.truncate(MAX_MODEL_TURN_FAILURES);
        Ok(Some(Self {
            failures: failures.into(),
            omitted,
        }))
    }

    pub fn failures(&self) -> &[ModelGatewayError] {
        &self.failures
    }

    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn total_count(&self) -> usize {
        self.failures.len().saturating_add(self.omitted)
    }
}

impl fmt::Display for ModelTurnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} model operation(s) failed while closing the turn",
            self.total_count()
        )
    }
}

impl Error for ModelTurnFailure {}

/// 流式 delta 输出。
pub type GenerationStream =
    Pin<Box<dyn Stream<Item = Result<GenerationStreamEvent, LlmError>> + Send>>;

/// A borrowed asynchronous model operation.
pub type LlmFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A borrowed asynchronous controlled model operation.
pub type ModelFuture<'a, T> = LlmFuture<'a, T>;

/// A stream whose lifetime is bounded by its borrowed gateway.
pub type ModelStream<'a> =
    Pin<Box<dyn Stream<Item = Result<GenerationStreamEvent, ModelGatewayError>> + Send + 'a>>;

/// Turn-scoped model access with cancellation and timeout policy owned by the runtime.
pub trait ModelGateway: Send + Sync {
    /// Pure capability metadata; must not initiate IO or activate a provider.
    /// Unknown capabilities are rejected during controlled generation preflight.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn generate<'a>(
        &'a self,
        request: &'a GenerationRequest,
        options: GenerationOptions,
    ) -> ModelFuture<'a, Result<GenerationResponse, ModelGatewayError>>;

    fn generate_stream<'a>(
        &'a self,
        request: GenerationRequest,
        options: GenerationOptions,
    ) -> ModelStream<'a>;
}

/// Host-owned model scope for one admitted Agent turn.
pub trait ModelTurn: Send + Sync {
    fn gateway(&self) -> &dyn ModelGateway;

    /// Read-only accounting observation, including a canonical runtime's default scope.
    fn budget_report(&self) -> Option<Result<BudgetReport, BudgetError>> {
        None
    }

    fn finish(
        self: Box<Self>,
        mode: ModelFinishMode,
    ) -> ModelFuture<'static, Result<(), ModelTurnFailure>>;
}

/// Selected model runtime that binds one controlled scope per Agent turn.
pub trait LlmRuntime: Send + Sync {
    fn bind_turn(&self, binding: ModelTurnBinding)
    -> Result<Box<dyn ModelTurn>, ModelRuntimeError>;

    /// Optional in-memory scheduling observation. Custom runtimes may not provide it.
    fn scheduler_snapshot(&self) -> Option<ModelSchedulerSnapshot> {
        None
    }
}

/// 模型能力 seam。提供方适配器（OpenAI/llama.cpp、本地推理等）实现本 trait。
pub trait Llm: Send + Sync {
    /// Pure metadata for this adapter and host configuration, without probing.
    /// Unknown support must never be treated as permission to silently downgrade.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn generate<'a>(
        &'a self,
        request: &'a GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<GenerationResponse, LlmError>> + Send + 'a>>;

    /// 流式补全。`cancel` 触发后必须尽快停止产生新的 delta 并返回
    /// `LlmError::Cancelled`。
    fn generate_stream(
        &self,
        request: GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> GenerationStream;
}
