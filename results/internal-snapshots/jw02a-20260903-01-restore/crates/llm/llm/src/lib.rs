//! Llm 能力族接口。
//!
//! 设计纪律（吸收 writing-rust 的教训）：
//! - 错误词汇不得携带 reqwest/HTTP 具体类型（`ModelGatewayError` 携带
//!   `reqwest::Error` 是反面教材）；
//! - 取消令牌是流式接口的**强制参数**，默认实现不得静默丢弃
//!   （writing-rust 的 `complete_stream_with_cancellation` 在真实实现里丢了 token）。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use jingwei_core::{CancellationSignal, CapabilityId, SessionEvent, SessionEventKind};
use jingwei_session::SessionRuntimeError;
use tokio_util::sync::CancellationToken;

pub mod protocol;
pub use protocol::{
    CapabilitySupport, FinishReason, GenerationConstraint, GenerationRequest, GenerationResponse,
    GenerationSupport, ModelCapabilities, ModelMessage, ModelProtocolError, ModelToolCall,
    ModelToolDefinition, ProviderToolCallId, TokenUsage, ToolChoice,
};

pub use jingwei_core::{
    ChatMessage, ModelCallMode, ModelFailureCategory, ModelRecordedOutcome, ModelRequest,
    ModelRequestOptions, ModelResult, ModelTimeout, Role,
};

/// The singular capability implemented by model provider plugins.
pub const LLM_PROVIDER: CapabilityId = CapabilityId::new("jingwei.llm.provider");

/// The singular controlled model runtime capability.
pub const LLM_RUNTIME: CapabilityId = CapabilityId::new("jingwei.llm.runtime");

/// 单次调用的可选运行参数；默认值表示沿用提供方全局配置。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LlmCallOptions {
    pub max_tokens: Option<u32>,
    pub timeout: Option<Duration>,
}

/// 非流式补全结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmCompletion {
    pub content: String,
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
    #[error("response missing content")]
    MissingContent,
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
}

impl ModelTurnBinding {
    pub fn new(
        cancellation: Arc<dyn CancellationSignal>,
        recorder: Arc<dyn ModelEventRecorder>,
    ) -> Self {
        Self {
            cancellation,
            recorder,
        }
    }

    pub fn cancellation(&self) -> Arc<dyn CancellationSignal> {
        Arc::clone(&self.cancellation)
    }

    pub fn recorder(&self) -> Arc<dyn ModelEventRecorder> {
        Arc::clone(&self.recorder)
    }

    pub fn into_parts(self) -> (Arc<dyn CancellationSignal>, Arc<dyn ModelEventRecorder>) {
        (self.cancellation, self.recorder)
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
}

impl ModelRecordFailure {
    pub fn source_error(&self) -> Option<&SessionRuntimeError> {
        match self {
            Self::Session(source) => Some(source),
            Self::RecorderPanicked => None,
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
                ModelFailureCategory::MissingContent => "failed_missing_content",
                ModelFailureCategory::Cancelled => "failed_cancelled",
                ModelFailureCategory::Adapter => "failed_adapter",
                ModelFailureCategory::Internal => "failed_internal",
            },
        });
        let source = match self.source {
            ModelRecordFailure::Session(_) => "session",
            ModelRecordFailure::RecorderPanicked => "recorder_panicked",
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
    #[error("model runtime is stopped")]
    Stopped,
    #[error("model turn is closed")]
    TurnClosed,
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
            Self::Model(LlmError::Upstream { status, .. }) => debug
                .field("kind", &"model_upstream")
                .field("status", status),
            Self::Model(LlmError::Timeout) => debug.field("kind", &"model_timeout"),
            Self::Model(LlmError::StreamParse(_)) => debug.field("kind", &"model_stream_parse"),
            Self::Model(LlmError::MissingContent) => debug.field("kind", &"model_missing_content"),
            Self::Model(LlmError::Cancelled) => debug.field("kind", &"model_cancelled"),
            Self::Model(LlmError::Adapter(_)) => debug.field("kind", &"model_adapter"),
            Self::Runtime(ModelRuntimeError::Stopped) => {
                debug.field("kind", &"model_runtime_stopped")
            }
            Self::Runtime(ModelRuntimeError::TurnClosed) => {
                debug.field("kind", &"model_turn_closed")
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
pub type LlmDeltaStream = Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send>>;

/// A borrowed asynchronous model operation.
pub type LlmFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A borrowed asynchronous controlled model operation.
pub type ModelFuture<'a, T> = LlmFuture<'a, T>;

/// A stream whose lifetime is bounded by its borrowed gateway.
pub type ModelDeltaStream<'a> =
    Pin<Box<dyn Stream<Item = Result<String, ModelGatewayError>> + Send + 'a>>;

/// Turn-scoped model access with cancellation and timeout policy owned by the runtime.
pub trait ModelGateway: Send + Sync {
    /// Pure capability metadata; must not initiate IO or activate a provider.
    /// Unknown capabilities do not prevent existing legacy text calls.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        options: LlmCallOptions,
    ) -> ModelFuture<'a, Result<LlmCompletion, ModelGatewayError>>;

    fn complete_stream<'a>(
        &'a self,
        messages: Vec<ChatMessage>,
        options: LlmCallOptions,
    ) -> ModelDeltaStream<'a>;
}

/// Host-owned model scope for one admitted Agent turn.
pub trait ModelTurn: Send + Sync {
    fn gateway(&self) -> &dyn ModelGateway;

    fn finish(
        self: Box<Self>,
        mode: ModelFinishMode,
    ) -> ModelFuture<'static, Result<(), ModelTurnFailure>>;
}

/// Selected model runtime that binds one controlled scope per Agent turn.
pub trait LlmRuntime: Send + Sync {
    fn bind_turn(&self, binding: ModelTurnBinding)
    -> Result<Box<dyn ModelTurn>, ModelRuntimeError>;
}

/// 模型能力 seam。提供方适配器（OpenAI/llama.cpp、本地推理等）实现本 trait。
pub trait Llm: Send + Sync {
    /// Pure metadata for this adapter and host configuration, without probing.
    /// Existing implementations remain compatible and default to unknown.
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        opts: LlmCallOptions,
    ) -> Pin<Box<dyn Future<Output = Result<LlmCompletion, LlmError>> + Send + 'a>>;

    /// 流式补全。`cancel` 触发后必须尽快停止产生新的 delta 并返回
    /// `LlmError::Cancelled`。
    fn complete_stream(
        &self,
        messages: Vec<ChatMessage>,
        opts: LlmCallOptions,
        cancel: CancellationToken,
    ) -> LlmDeltaStream;
}
