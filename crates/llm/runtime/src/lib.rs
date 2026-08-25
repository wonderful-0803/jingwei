//! Canonical controlled model runtime.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use jingwei_core::{CancellationFuture, CancellationSignal, CapabilityId, ModelCallId};
use jingwei_llm::{
    ChatMessage, LLM_PROVIDER, LLM_RUNTIME, Llm, LlmCallOptions, LlmCompletion, LlmError,
    LlmRuntime, ModelCallMode, ModelClosureFailure, ModelDeltaStream, ModelEventRecorder,
    ModelFailureCategory, ModelFinishMode, ModelGateway, ModelGatewayError, ModelRecord,
    ModelRecordedOutcome, ModelRequest, ModelRequestOptions, ModelResult, ModelRuntimeError,
    ModelTimeout, ModelTurn, ModelTurnBinding, ModelTurnFailure,
};
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, RuntimeError, ServiceFactory, ServiceLifecycle, StopReason,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const RAW_LLM_DEPENDENCY: &[CapabilityId] = &[LLM_PROVIDER];
const STREAM_DELTA_BUFFER: usize = 16;
const MAX_LIFECYCLE_FAILURES: usize = 32;

/// Explicit canonical provider for [`LLM_RUNTIME`].
pub struct CanonicalLlmRuntimePlugin {
    default_timeout: Option<Duration>,
}

impl CanonicalLlmRuntimePlugin {
    pub const fn new() -> Self {
        Self {
            default_timeout: None,
        }
    }

    #[must_use]
    pub const fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }
}

impl Default for CanonicalLlmRuntimePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CanonicalLlmRuntimePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("llm-runtime-canonical", LLM_RUNTIME, "canonical")
            .requires_capabilities(RAW_LLM_DEPENDENCY)
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_runtime_factory(Arc::new(CanonicalLlmRuntimeFactory {
            default_timeout: self.default_timeout,
        }))
    }
}

struct CanonicalLlmRuntimeFactory {
    default_timeout: Option<Duration>,
}

impl ServiceFactory<dyn LlmRuntime> for CanonicalLlmRuntimeFactory {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn LlmRuntime>, RuntimeError>> {
        Box::pin(async move {
            let llm = ctx
                .llm()
                .ok_or_else(|| RuntimeError::new("selected raw LLM dependency is unavailable"))?;
            let executor = Handle::try_current().map_err(|error| {
                RuntimeError::new(format!("LlmRuntime requires a Tokio executor: {error}"))
            })?;
            if self.default_timeout.is_some_and(|timeout| {
                !timeout.is_zero() && Instant::now().checked_add(timeout).is_none()
            }) {
                return Err(RuntimeError::new(
                    "canonical LlmRuntime default timeout exceeds the executor clock range",
                ));
            }
            let state = Arc::new(RuntimeState::new(llm, self.default_timeout, executor));
            let runtime: Arc<dyn LlmRuntime> = Arc::new(CanonicalLlmRuntime {
                state: Arc::clone(&state),
            });
            Ok(ManagedService::new(
                runtime,
                Box::new(LlmRuntimeLifecycle { state }),
            ))
        })
    }
}

struct CanonicalLlmRuntime {
    state: Arc<RuntimeState>,
}

impl LlmRuntime for CanonicalLlmRuntime {
    fn bind_turn(
        &self,
        binding: ModelTurnBinding,
    ) -> Result<Box<dyn ModelTurn>, ModelRuntimeError> {
        self.state.bind_turn(binding)
    }
}

struct RuntimeState {
    llm: Arc<dyn Llm>,
    default_timeout: Option<Duration>,
    executor: Handle,
    control: Mutex<RuntimeControl>,
    drained: Notify,
}

impl RuntimeState {
    fn new(llm: Arc<dyn Llm>, default_timeout: Option<Duration>, executor: Handle) -> Self {
        Self {
            llm,
            default_timeout,
            executor,
            control: Mutex::new(RuntimeControl::new()),
            drained: Notify::new(),
        }
    }

    fn lock_control(&self) -> MutexGuard<'_, RuntimeControl> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start(&self) -> Result<(), RuntimeError> {
        let mut control = self.lock_control();
        if control.lifecycle != LifecycleState::Constructed {
            return Err(RuntimeError::new("LlmRuntime lifecycle start was repeated"));
        }
        control.lifecycle = LifecycleState::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<(), RuntimeError> {
        {
            let mut control = self.lock_control();
            match control.lifecycle {
                LifecycleState::Stopped => return Ok(()),
                LifecycleState::Constructed
                | LifecycleState::Running
                | LifecycleState::Stopping => {
                    control.lifecycle = LifecycleState::Stopping;
                    for cancellation in control.jobs.values() {
                        cancellation.cancel();
                    }
                }
            }
        }
        loop {
            let notified = self.drained.notified();
            if self.lock_control().jobs.is_empty() {
                break;
            }
            notified.await;
        }
        let failures = {
            let mut control = self.lock_control();
            control.lifecycle = LifecycleState::Stopped;
            std::mem::take(&mut control.unacknowledged_failures)
        };
        if failures.is_empty() {
            Ok(())
        } else {
            let count = failures.values().fold(0usize, |count, ledger| {
                count.saturating_add(ledger.total_count())
            });
            Err(RuntimeError::new(format!(
                "{count} model operation(s) failed canonical closure before shutdown"
            )))
        }
    }

    fn bind_turn(
        self: &Arc<Self>,
        binding: ModelTurnBinding,
    ) -> Result<Box<dyn ModelTurn>, ModelRuntimeError> {
        let (cancellation, recorder) = binding.into_parts();
        if cancellation.is_cancelled() {
            return Err(ModelRuntimeError::TurnClosed);
        }
        let turn_id =
            {
                let mut control = self.lock_control();
                if control.lifecycle != LifecycleState::Running {
                    return Err(ModelRuntimeError::Stopped);
                }
                let turn_id = control.next_turn_id;
                control.next_turn_id = control.next_turn_id.checked_add(1).ok_or_else(|| {
                    ModelRuntimeError::Internal {
                        code: "model_turn_id_exhausted".to_string(),
                        message: "Model turn ID space is exhausted".to_string(),
                    }
                })?;
                turn_id
            };
        let state = Arc::new(TurnState {
            id: turn_id,
            runtime: Arc::clone(self),
            cancellation,
            recorder,
            control: Mutex::new(TurnControl::new()),
            drained: Notify::new(),
        });
        Ok(Box::new(BoundModelTurn {
            gateway: TurnModelGateway { state },
        }))
    }

    fn admit(
        self: &Arc<Self>,
        turn: &Arc<TurnState>,
    ) -> Result<(u64, Arc<JobCancellation>, JobGuard), ModelGatewayError> {
        let mut runtime_control = self.lock_control();
        if runtime_control.lifecycle != LifecycleState::Running {
            return Err(ModelRuntimeError::Stopped.into());
        }
        let mut turn_control = turn.lock_control();
        if !turn_control.open {
            return Err(ModelRuntimeError::TurnClosed.into());
        }
        let job_id = runtime_control.next_job_id;
        runtime_control.next_job_id =
            runtime_control.next_job_id.checked_add(1).ok_or_else(|| {
                ModelRuntimeError::Internal {
                    code: "model_job_id_exhausted".to_string(),
                    message: "Model operation ID space is exhausted".to_string(),
                }
            })?;
        let cancellation = Arc::new(JobCancellation::new());
        runtime_control
            .jobs
            .insert(job_id, Arc::clone(&cancellation));
        turn_control.jobs.insert(job_id, Arc::clone(&cancellation));
        drop(turn_control);
        drop(runtime_control);
        Ok((
            job_id,
            cancellation,
            JobGuard::new(Arc::clone(self), Arc::clone(turn), job_id),
        ))
    }

    fn complete_job(
        &self,
        turn: &TurnState,
        job_id: u64,
        closure_failure: Option<ModelGatewayError>,
    ) {
        let mut runtime_control = self.lock_control();
        let mut turn_control = turn.lock_control();
        if let Some(failure) = closure_failure {
            turn_control.failures.record(failure.clone());
            runtime_control
                .unacknowledged_failures
                .entry(turn.id)
                .or_default()
                .record(failure);
        }
        runtime_control.jobs.remove(&job_id);
        turn_control.jobs.remove(&job_id);
        let runtime_drained = runtime_control.jobs.is_empty();
        let turn_drained = turn_control.jobs.is_empty();
        drop(turn_control);
        drop(runtime_control);
        if runtime_drained {
            self.drained.notify_one();
        }
        if turn_drained {
            turn.drained.notify_one();
        }
    }

    fn acknowledge_turn(&self, turn_id: u64) {
        self.lock_control().unacknowledged_failures.remove(&turn_id);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LifecycleState {
    Constructed,
    Running,
    Stopping,
    Stopped,
}

struct RuntimeControl {
    lifecycle: LifecycleState,
    next_turn_id: u64,
    next_job_id: u64,
    jobs: BTreeMap<u64, Arc<JobCancellation>>,
    unacknowledged_failures: BTreeMap<u64, FailureLedger>,
}

impl RuntimeControl {
    fn new() -> Self {
        Self {
            lifecycle: LifecycleState::Constructed,
            next_turn_id: 0,
            next_job_id: 0,
            jobs: BTreeMap::new(),
            unacknowledged_failures: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
struct FailureLedger {
    failures: Vec<ModelGatewayError>,
    omitted: usize,
}

impl FailureLedger {
    fn record(&mut self, failure: ModelGatewayError) {
        if self.failures.len() < MAX_LIFECYCLE_FAILURES {
            self.failures.push(failure);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty() && self.omitted == 0
    }

    fn total_count(&self) -> usize {
        self.failures.len().saturating_add(self.omitted)
    }
}

struct TurnState {
    id: u64,
    runtime: Arc<RuntimeState>,
    cancellation: Arc<dyn CancellationSignal>,
    recorder: Arc<dyn ModelEventRecorder>,
    control: Mutex<TurnControl>,
    drained: Notify,
}

impl TurnState {
    fn lock_control(&self) -> MutexGuard<'_, TurnControl> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn finish(&self, mode: ModelFinishMode) -> Result<(), ModelTurnFailure> {
        {
            let mut control = self.lock_control();
            control.open = false;
            if mode == ModelFinishMode::Cancel {
                for cancellation in control.jobs.values() {
                    cancellation.cancel();
                }
            }
        }
        loop {
            let notified = self.drained.notified();
            if self.lock_control().jobs.is_empty() {
                break;
            }
            notified.await;
        }
        let failure_ledger = {
            let mut control = self.lock_control();
            std::mem::take(&mut control.failures)
        };
        self.runtime.acknowledge_turn(self.id);
        if failure_ledger.is_empty() {
            Ok(())
        } else {
            Err(ModelTurnFailure::from_bounded_failures(
                failure_ledger.failures,
                failure_ledger.omitted,
            )
            .expect("canonical ledger stores only model closure failures")
            .expect("a non-empty model closure ledger must produce ModelTurnFailure"))
        }
    }

    fn complete(
        self: &Arc<Self>,
        messages: &[ChatMessage],
        mut options: LlmCallOptions,
    ) -> jingwei_llm::ModelFuture<'static, Result<LlmCompletion, ModelGatewayError>> {
        options.timeout = effective_timeout(options.timeout, self.runtime.default_timeout);
        let (job_id, local_cancellation, guard) = match self.runtime.admit(self) {
            Ok(admission) => admission,
            Err(error) => return ready_complete_error(error),
        };
        let request = model_request(ModelCallMode::Complete, messages.to_vec(), &options);
        let operation_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
            parent: Arc::clone(&self.cancellation),
            local: local_cancellation.token.clone(),
        });
        let turn = Arc::clone(self);
        let (sender, receiver) = oneshot::channel();
        let task = async move {
            let result = run_complete(turn, request, options, operation_cancellation).await;
            guard.complete(sender, result);
        };
        let executor = self.runtime.executor.clone();
        if catch_unwind(AssertUnwindSafe(|| executor.spawn(task))).is_err() {
            return ready_complete_error(ModelGatewayError::Internal {
                code: "model_spawn_failed".to_string(),
                message: "Model runtime executor rejected an admitted operation".to_string(),
            });
        }
        Box::pin(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(ModelGatewayError::Internal {
                    code: "model_driver_lost".to_string(),
                    message: format!("Model operation {job_id} ended without a completion report"),
                })
            })
        })
    }

    fn complete_stream(
        self: &Arc<Self>,
        messages: Vec<ChatMessage>,
        mut options: LlmCallOptions,
    ) -> ModelDeltaStream<'static> {
        options.timeout = effective_timeout(options.timeout, self.runtime.default_timeout);
        let (job_id, local_cancellation, guard) = match self.runtime.admit(self) {
            Ok(admission) => admission,
            Err(error) => return ready_stream_error(error),
        };
        let request = model_request(ModelCallMode::Stream, messages, &options);
        let operation_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
            parent: Arc::clone(&self.cancellation),
            local: local_cancellation.token.clone(),
        });
        let turn = Arc::clone(self);
        let (delta_sender, delta_receiver) = mpsc::channel(STREAM_DELTA_BUFFER);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let task = async move {
            let result =
                run_stream(turn, request, options, operation_cancellation, delta_sender).await;
            guard.complete(terminal_sender, result);
        };
        let executor = self.runtime.executor.clone();
        if catch_unwind(AssertUnwindSafe(|| executor.spawn(task))).is_err() {
            return ready_stream_error(ModelGatewayError::Internal {
                code: "model_spawn_failed".to_string(),
                message: "Model runtime executor rejected an admitted operation".to_string(),
            });
        }
        stream_projection(job_id, delta_receiver, terminal_receiver)
    }
}

struct TurnControl {
    open: bool,
    jobs: BTreeMap<u64, Arc<JobCancellation>>,
    failures: FailureLedger,
}

impl TurnControl {
    fn new() -> Self {
        Self {
            open: true,
            jobs: BTreeMap::new(),
            failures: FailureLedger::default(),
        }
    }
}

struct BoundModelTurn {
    gateway: TurnModelGateway,
}

impl ModelTurn for BoundModelTurn {
    fn gateway(&self) -> &dyn ModelGateway {
        &self.gateway
    }

    fn finish(
        self: Box<Self>,
        mode: ModelFinishMode,
    ) -> jingwei_llm::ModelFuture<'static, Result<(), ModelTurnFailure>> {
        Box::pin(async move { self.gateway.state.finish(mode).await })
    }
}

struct TurnModelGateway {
    state: Arc<TurnState>,
}

impl ModelGateway for TurnModelGateway {
    fn complete<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        options: LlmCallOptions,
    ) -> jingwei_llm::ModelFuture<'a, Result<LlmCompletion, ModelGatewayError>> {
        self.state.complete(messages, options)
    }

    fn complete_stream<'a>(
        &'a self,
        messages: Vec<ChatMessage>,
        options: LlmCallOptions,
    ) -> ModelDeltaStream<'a> {
        self.state.complete_stream(messages, options)
    }
}

fn model_request(
    mode: ModelCallMode,
    messages: Vec<ChatMessage>,
    options: &LlmCallOptions,
) -> ModelRequest {
    ModelRequest {
        call_id: ModelCallId::new(),
        mode,
        messages,
        options: ModelRequestOptions {
            max_tokens: options.max_tokens,
            timeout: options.timeout.map(ModelTimeout::from_duration),
        },
    }
}

fn ready_complete_error(
    error: ModelGatewayError,
) -> jingwei_llm::ModelFuture<'static, Result<LlmCompletion, ModelGatewayError>> {
    Box::pin(async move { Err(error) })
}

fn ready_stream_error(error: ModelGatewayError) -> ModelDeltaStream<'static> {
    Box::pin(async_stream::stream! {
        yield Err(error);
    })
}

fn stream_projection(
    job_id: u64,
    mut deltas: mpsc::Receiver<String>,
    terminal: oneshot::Receiver<Result<(), ModelGatewayError>>,
) -> ModelDeltaStream<'static> {
    Box::pin(async_stream::stream! {
        while let Some(delta) = deltas.recv().await {
            yield Ok(delta);
        }
        match terminal.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => yield Err(error),
            Err(_) => yield Err(ModelGatewayError::Internal {
                code: "model_driver_lost".to_string(),
                message: format!("Model stream operation {job_id} ended without a terminal report"),
            }),
        }
    })
}

struct JobCancellation {
    token: CancellationToken,
}

impl JobCancellation {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    fn cancel(&self) {
        self.token.cancel();
    }
}

struct CombinedCancellation {
    parent: Arc<dyn CancellationSignal>,
    local: CancellationToken,
}

impl CancellationSignal for CombinedCancellation {
    fn is_cancelled(&self) -> bool {
        self.parent.is_cancelled() || self.local.is_cancelled()
    }

    fn cancelled(&self) -> CancellationFuture<'_> {
        Box::pin(async move {
            tokio::select! {
                _ = self.parent.cancelled() => {}
                _ = self.local.cancelled() => {}
            }
        })
    }
}

struct JobGuard {
    runtime: Arc<RuntimeState>,
    turn: Arc<TurnState>,
    job_id: u64,
    armed: bool,
}

impl JobGuard {
    fn new(runtime: Arc<RuntimeState>, turn: Arc<TurnState>, job_id: u64) -> Self {
        Self {
            runtime,
            turn,
            job_id,
            armed: true,
        }
    }

    fn complete<T>(
        mut self,
        sender: oneshot::Sender<Result<T, ModelGatewayError>>,
        result: Result<T, ModelGatewayError>,
    ) {
        let closure_failure = result
            .as_ref()
            .err()
            .filter(|error| is_closure_failure(error))
            .cloned();
        let _ = sender.send(result);
        self.runtime
            .complete_job(&self.turn, self.job_id, closure_failure);
        self.armed = false;
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.runtime.complete_job(
            &self.turn,
            self.job_id,
            Some(ModelGatewayError::Internal {
                code: "model_driver_unwound".to_string(),
                message: "Model operation driver unwound before canonical closure".to_string(),
            }),
        );
    }
}

fn is_closure_failure(error: &ModelGatewayError) -> bool {
    matches!(
        error,
        ModelGatewayError::Recording(_) | ModelGatewayError::Internal { .. }
    )
}

async fn run_complete(
    turn: Arc<TurnState>,
    request: ModelRequest,
    options: LlmCallOptions,
    cancellation: Arc<dyn CancellationSignal>,
) -> Result<LlmCompletion, ModelGatewayError> {
    append_request(&turn, &request).await?;
    let semantic = resolve_complete(&turn.runtime.llm, &request, options, &cancellation).await;
    let (result, output) = match semantic {
        CompleteResolution::Succeeded(completion) => (
            successful_result(&request, completion.content.clone()),
            Ok(completion),
        ),
        CompleteResolution::ModelFailed(error) => (
            failed_result(&request, &error, String::new()),
            Err(error.into()),
        ),
        CompleteResolution::Internal { code, message } => (
            internal_result(&request, code, message),
            Err(ModelGatewayError::Internal {
                code: code.to_string(),
                message: message.to_string(),
            }),
        ),
    };
    append_result(&turn, &request, &result).await?;
    output
}

enum CompleteResolution {
    Succeeded(LlmCompletion),
    ModelFailed(LlmError),
    Internal {
        code: &'static str,
        message: &'static str,
    },
}

async fn resolve_complete(
    llm: &Arc<dyn Llm>,
    request: &ModelRequest,
    options: LlmCallOptions,
    cancellation: &Arc<dyn CancellationSignal>,
) -> CompleteResolution {
    if cancellation.is_cancelled() {
        return CompleteResolution::ModelFailed(LlmError::Cancelled);
    }
    if options.timeout.is_some_and(|timeout| timeout.is_zero()) {
        return CompleteResolution::ModelFailed(LlmError::Timeout);
    }
    let deadline = match options.timeout {
        Some(timeout) => match Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => {
                return CompleteResolution::Internal {
                    code: "model_timeout_out_of_range",
                    message: "Model timeout exceeds the executor clock range",
                };
            }
        },
        None => None,
    };
    let raw = match catch_unwind(AssertUnwindSafe(|| {
        llm.complete(&request.messages, options)
    })) {
        Ok(raw) => AssertUnwindSafe(raw).catch_unwind(),
        Err(_) => {
            return CompleteResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime operation panicked",
            };
        }
    };
    let output = if let Some(deadline) = deadline {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return CompleteResolution::ModelFailed(LlmError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return CompleteResolution::ModelFailed(LlmError::Timeout),
            output = raw => output,
        }
    } else {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return CompleteResolution::ModelFailed(LlmError::Cancelled),
            output = raw => output,
        }
    };
    match output {
        Ok(Ok(completion)) => CompleteResolution::Succeeded(completion),
        Ok(Err(error)) => CompleteResolution::ModelFailed(error),
        Err(_) => CompleteResolution::Internal {
            code: "model_runtime_panicked",
            message: "Model runtime operation panicked",
        },
    }
}

async fn run_stream(
    turn: Arc<TurnState>,
    request: ModelRequest,
    options: LlmCallOptions,
    cancellation: Arc<dyn CancellationSignal>,
    deltas: mpsc::Sender<String>,
) -> Result<(), ModelGatewayError> {
    append_request(&turn, &request).await?;
    let semantic =
        resolve_stream(&turn.runtime.llm, &request, options, &cancellation, &deltas).await;
    let (result, output) = match semantic {
        StreamResolution::Succeeded(content) => (successful_result(&request, content), Ok(())),
        StreamResolution::ModelFailed { error, partial } => {
            (failed_result(&request, &error, partial), Err(error.into()))
        }
        StreamResolution::ConsumerDropped(partial) => (
            consumer_dropped_result(&request, partial),
            Err(LlmError::Cancelled.into()),
        ),
        StreamResolution::Internal {
            code,
            message,
            partial,
        } => (
            internal_result_with_partial(&request, code, message, partial),
            Err(ModelGatewayError::Internal {
                code: code.to_string(),
                message: message.to_string(),
            }),
        ),
    };
    append_result(&turn, &request, &result).await?;
    output
}

enum StreamResolution {
    Succeeded(String),
    ModelFailed {
        error: LlmError,
        partial: String,
    },
    ConsumerDropped(String),
    Internal {
        code: &'static str,
        message: &'static str,
        partial: String,
    },
}

async fn resolve_stream(
    llm: &Arc<dyn Llm>,
    request: &ModelRequest,
    options: LlmCallOptions,
    cancellation: &Arc<dyn CancellationSignal>,
    deltas: &mpsc::Sender<String>,
) -> StreamResolution {
    let adapter_guard = AdapterCancellationGuard(CancellationToken::new());
    if cancellation.is_cancelled() {
        return StreamResolution::ModelFailed {
            error: LlmError::Cancelled,
            partial: String::new(),
        };
    }
    if options.timeout.is_some_and(|timeout| timeout.is_zero()) {
        return StreamResolution::ModelFailed {
            error: LlmError::Timeout,
            partial: String::new(),
        };
    }
    let deadline = match options.timeout {
        Some(timeout) => match Instant::now().checked_add(timeout) {
            Some(deadline) => Some(deadline),
            None => {
                return StreamResolution::Internal {
                    code: "model_timeout_out_of_range",
                    message: "Model timeout exceeds the executor clock range",
                    partial: String::new(),
                };
            }
        },
        None => None,
    };
    let mut stream = match catch_unwind(AssertUnwindSafe(|| {
        llm.complete_stream(request.messages.clone(), options, adapter_guard.token())
    })) {
        Ok(stream) => stream,
        Err(_) => {
            return StreamResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime operation panicked",
                partial: String::new(),
            };
        }
    };
    let mut partial = String::new();
    loop {
        let polled = AssertUnwindSafe(stream.next()).catch_unwind();
        let item = if let Some(deadline) = deadline {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    adapter_guard.cancel();
                    return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial };
                }
                _ = tokio::time::sleep_until(deadline) => {
                    adapter_guard.cancel();
                    return StreamResolution::ModelFailed { error: LlmError::Timeout, partial };
                }
                _ = deltas.closed() => {
                    adapter_guard.cancel();
                    return StreamResolution::ConsumerDropped(partial);
                }
                item = polled => item,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    adapter_guard.cancel();
                    return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial };
                }
                _ = deltas.closed() => {
                    adapter_guard.cancel();
                    return StreamResolution::ConsumerDropped(partial);
                }
                item = polled => item,
            }
        };
        match item {
            Err(_) => {
                adapter_guard.cancel();
                return StreamResolution::Internal {
                    code: "model_runtime_panicked",
                    message: "Model runtime operation panicked",
                    partial,
                };
            }
            Ok(Some(Err(error))) => {
                adapter_guard.cancel();
                return StreamResolution::ModelFailed { error, partial };
            }
            Ok(None) => return StreamResolution::Succeeded(partial),
            Ok(Some(Ok(delta))) => {
                partial.push_str(&delta);
                let send = deltas.send(delta);
                let sent = if let Some(deadline) = deadline {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            adapter_guard.cancel();
                            return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial };
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            adapter_guard.cancel();
                            return StreamResolution::ModelFailed { error: LlmError::Timeout, partial };
                        }
                        _ = deltas.closed() => {
                            adapter_guard.cancel();
                            return StreamResolution::ConsumerDropped(partial);
                        }
                        sent = send => sent,
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            adapter_guard.cancel();
                            return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial };
                        }
                        _ = deltas.closed() => {
                            adapter_guard.cancel();
                            return StreamResolution::ConsumerDropped(partial);
                        }
                        sent = send => sent,
                    }
                };
                if sent.is_err() {
                    adapter_guard.cancel();
                    return StreamResolution::ConsumerDropped(partial);
                }
            }
        }
    }
}

async fn append_request(turn: &TurnState, request: &ModelRequest) -> Result<(), ModelGatewayError> {
    let future = match catch_unwind(AssertUnwindSafe(|| {
        turn.recorder.append(ModelRecord::Request(request.clone()))
    })) {
        Ok(future) => future,
        Err(_) => {
            return Err(Arc::new(ModelClosureFailure::request_recorder_panicked(
                request.clone(),
            ))
            .into());
        }
    };
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(source)) => {
            Err(Arc::new(ModelClosureFailure::request(request.clone(), source)).into())
        }
        Err(_) => Err(Arc::new(ModelClosureFailure::request_recorder_panicked(
            request.clone(),
        ))
        .into()),
    }
}

async fn append_result(
    turn: &TurnState,
    request: &ModelRequest,
    result: &ModelResult,
) -> Result<(), ModelGatewayError> {
    let future = match catch_unwind(AssertUnwindSafe(|| {
        turn.recorder.append(ModelRecord::Result(result.clone()))
    })) {
        Ok(future) => future,
        Err(_) => {
            return Err(Arc::new(ModelClosureFailure::result_recorder_panicked(
                request.clone(),
                result.clone(),
            ))
            .into());
        }
    };
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(source)) => Err(Arc::new(ModelClosureFailure::result(
            request.clone(),
            result.clone(),
            source,
        ))
        .into()),
        Err(_) => Err(Arc::new(ModelClosureFailure::result_recorder_panicked(
            request.clone(),
            result.clone(),
        ))
        .into()),
    }
}

fn successful_result(request: &ModelRequest, content: String) -> ModelResult {
    ModelResult {
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Succeeded { content },
    }
}

fn failed_result(request: &ModelRequest, error: &LlmError, partial_content: String) -> ModelResult {
    let (category, code, message, retryable, upstream_status) = match error {
        LlmError::Upstream { status, .. } => (
            ModelFailureCategory::Upstream,
            "model_upstream",
            "Model provider rejected the request",
            *status == 408 || *status == 429 || *status >= 500,
            Some(*status),
        ),
        LlmError::Timeout => (
            ModelFailureCategory::Timeout,
            "model_timeout",
            "Model operation exceeded its deadline",
            true,
            None,
        ),
        LlmError::StreamParse(_) => (
            ModelFailureCategory::StreamParse,
            "model_stream_parse",
            "Model stream could not be decoded",
            false,
            None,
        ),
        LlmError::MissingContent => (
            ModelFailureCategory::MissingContent,
            "model_missing_content",
            "Model response did not contain content",
            false,
            None,
        ),
        LlmError::Cancelled => (
            ModelFailureCategory::Cancelled,
            "model_cancelled",
            "Model operation was cancelled",
            false,
            None,
        ),
        LlmError::Adapter(_) => (
            ModelFailureCategory::Adapter,
            "model_adapter",
            "Model adapter failed",
            true,
            None,
        ),
    };
    ModelResult {
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category,
            code: code.to_string(),
            message: message.to_string(),
            retryable,
            upstream_status,
            partial_content,
        },
    }
}

fn consumer_dropped_result(request: &ModelRequest, partial_content: String) -> ModelResult {
    ModelResult {
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Cancelled,
            code: "model_stream_consumer_dropped".to_string(),
            message: "Model stream consumer dropped".to_string(),
            retryable: false,
            upstream_status: None,
            partial_content,
        },
    }
}

fn internal_result(
    request: &ModelRequest,
    code: &'static str,
    message: &'static str,
) -> ModelResult {
    internal_result_with_partial(request, code, message, String::new())
}

fn internal_result_with_partial(
    request: &ModelRequest,
    code: &'static str,
    message: &'static str,
    partial_content: String,
) -> ModelResult {
    ModelResult {
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Internal,
            code: code.to_string(),
            message: message.to_string(),
            retryable: false,
            upstream_status: None,
            partial_content,
        },
    }
}

struct AdapterCancellationGuard(CancellationToken);

impl AdapterCancellationGuard {
    fn token(&self) -> CancellationToken {
        self.0.clone()
    }

    fn cancel(&self) {
        self.0.cancel();
    }
}

impl Drop for AdapterCancellationGuard {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn effective_timeout(call: Option<Duration>, runtime: Option<Duration>) -> Option<Duration> {
    match (call, runtime) {
        (Some(call), Some(runtime)) => Some(call.min(runtime)),
        (Some(call), None) => Some(call),
        (None, runtime) => runtime,
    }
}

struct LlmRuntimeLifecycle {
    state: Arc<RuntimeState>,
}

impl ServiceLifecycle for LlmRuntimeLifecycle {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { self.state.start() })
    }

    fn stop(
        self: Box<Self>,
        _reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>> {
        Box::pin(async move { self.state.stop().await })
    }
}
