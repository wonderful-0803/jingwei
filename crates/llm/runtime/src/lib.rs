//! Canonical controlled model runtime.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::{FutureExt, StreamExt};
use jingwei_core::{CancellationFuture, CancellationSignal, CapabilityId, ModelCallId};
use jingwei_llm::{
    GenerationAccumulator, GenerationDelta, GenerationOptions, GenerationPartial,
    GenerationRequest, GenerationResponse, GenerationStreamEvent, LLM_PROVIDER, LLM_RUNTIME, Llm,
    LlmError, LlmRuntime, ModelCallMode, ModelClosureFailure, ModelEventRecorder,
    ModelFailureCategory, ModelFinishMode, ModelGateway, ModelGatewayError, ModelJobPhase,
    ModelJobStopReason, ModelOverloadKind, ModelProtocolError, ModelRecord, ModelRecordStage,
    ModelRecordVersion, ModelRecordedOutcome, ModelRequest, ModelRequestOptions, ModelResult,
    ModelRuntimeError, ModelSchedulerConfig, ModelSchedulerSnapshot, ModelStream, ModelTimeout,
    ModelTurn, ModelTurnBinding, ModelTurnFailure,
};
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, RuntimeError, ServiceFactory, ServiceLifecycle, StopReason,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

mod scheduling;
mod validation;
use scheduling::{JobExecution, ScheduledJob, validate_request_size};

const RAW_LLM_DEPENDENCY: &[CapabilityId] = &[LLM_PROVIDER];
const STREAM_DELTA_BUFFER: usize = 16;
const MAX_LIFECYCLE_FAILURES: usize = 32;

/// Explicit canonical provider for [`LLM_RUNTIME`].
pub struct CanonicalLlmRuntimePlugin {
    default_timeout: Duration,
    scheduler: ModelSchedulerConfig,
}

impl CanonicalLlmRuntimePlugin {
    pub const fn new() -> Self {
        Self {
            default_timeout: Duration::from_secs(600),
            scheduler: ModelSchedulerConfig::new(),
        }
    }

    #[must_use]
    pub const fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn with_scheduler_config(mut self, config: ModelSchedulerConfig) -> Self {
        self.scheduler = config;
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
            scheduler: self.scheduler,
        }))
    }
}

struct CanonicalLlmRuntimeFactory {
    default_timeout: Duration,
    scheduler: ModelSchedulerConfig,
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
            if Instant::now().checked_add(self.default_timeout).is_none() {
                return Err(RuntimeError::new(
                    "canonical LlmRuntime default timeout exceeds the executor clock range",
                ));
            }
            if self.scheduler.max_concurrency == 0
                || self.scheduler.max_inflight < self.scheduler.max_concurrency
                || self.scheduler.max_request_bytes == 0
            {
                return Err(RuntimeError::new(
                    "model scheduler requires positive concurrency and request bytes, and max_inflight >= max_concurrency",
                ));
            }
            let state = Arc::new(RuntimeState::new(
                llm,
                self.default_timeout,
                self.scheduler,
                executor,
            ));
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
    fn scheduler_snapshot(&self) -> Option<ModelSchedulerSnapshot> {
        Some(self.state.scheduler_snapshot())
    }

    fn bind_turn(
        &self,
        binding: ModelTurnBinding,
    ) -> Result<Box<dyn ModelTurn>, ModelRuntimeError> {
        self.state.bind_turn(binding)
    }
}

struct RuntimeState {
    llm: Arc<dyn Llm>,
    default_timeout: Duration,
    scheduler: ModelSchedulerConfig,
    executor: Handle,
    control: Mutex<RuntimeControl>,
    drained: Notify,
}

impl RuntimeState {
    fn new(
        llm: Arc<dyn Llm>,
        default_timeout: Duration,
        scheduler: ModelSchedulerConfig,
        executor: Handle,
    ) -> Self {
        Self {
            llm,
            default_timeout,
            scheduler,
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
                    for job in control.jobs.values() {
                        job.cancellation.cancel();
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
        call_id: ModelCallId,
        timeout: Duration,
    ) -> Result<(u64, Arc<JobCancellation>, JobGuard, Instant), ModelGatewayError> {
        let mut runtime_control = self.lock_control();
        if runtime_control.lifecycle != LifecycleState::Running {
            return Err(ModelRuntimeError::Stopped.into());
        }
        let mut turn_control = turn.lock_control();
        if !turn_control.open {
            return Err(ModelRuntimeError::TurnClosed.into());
        }
        let now = Instant::now();
        let deadline = now
            .checked_add(timeout)
            .ok_or(ModelRuntimeError::InvalidTimeout)?;
        if runtime_control.jobs.len() >= self.scheduler.max_inflight {
            return Err(ModelRuntimeError::Overloaded {
                capacity: ModelOverloadKind::InflightFull,
            }
            .into());
        }
        let dispatched = runtime_control.occupied < self.scheduler.max_concurrency
            && runtime_control.queue.is_empty();
        if !dispatched && runtime_control.queue.len() >= self.scheduler.max_queued {
            return Err(ModelRuntimeError::Overloaded {
                capacity: ModelOverloadKind::QueueFull,
            }
            .into());
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
        runtime_control.jobs.insert(
            job_id,
            ScheduledJob::new(
                call_id,
                Arc::clone(&cancellation),
                now,
                deadline,
                timeout,
                dispatched,
            ),
        );
        if dispatched {
            runtime_control.occupied += 1;
        } else {
            runtime_control.queue.push_back(job_id);
        }
        turn_control.jobs.insert(job_id, Arc::clone(&cancellation));
        drop(turn_control);
        drop(runtime_control);
        Ok((
            job_id,
            cancellation,
            JobGuard::new(Arc::clone(self), Arc::clone(turn), job_id),
            deadline,
        ))
    }

    fn complete_job(
        &self,
        turn: &TurnState,
        job_id: u64,
        closure_failure: Option<ModelGatewayError>,
    ) {
        self.release_execution(job_id, None);
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
    jobs: BTreeMap<u64, ScheduledJob>,
    queue: VecDeque<u64>,
    occupied: usize,
    unacknowledged_failures: BTreeMap<u64, FailureLedger>,
}

impl RuntimeControl {
    fn new() -> Self {
        Self {
            lifecycle: LifecycleState::Constructed,
            next_turn_id: 0,
            next_job_id: 0,
            jobs: BTreeMap::new(),
            queue: VecDeque::new(),
            occupied: 0,
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

    fn generate(
        self: &Arc<Self>,
        input: &GenerationRequest,
        mut options: GenerationOptions,
    ) -> jingwei_llm::ModelFuture<'static, Result<GenerationResponse, ModelGatewayError>> {
        let timeout = effective_timeout(options.timeout, self.runtime.default_timeout);
        options.timeout = Some(timeout);
        if let Err(error) =
            validate_request_size(input, &options, self.runtime.scheduler.max_request_bytes)
        {
            return ready_complete_error(error);
        }
        let call_id = ModelCallId::new();
        let (job_id, local_cancellation, guard, deadline) =
            match self.runtime.admit(self, call_id.clone(), timeout) {
                Ok(admission) => admission,
                Err(error) => return ready_complete_error(error),
            };
        let request = model_request(call_id, ModelCallMode::Complete, input.clone(), &options);
        let operation_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
            parent: Arc::clone(&self.cancellation),
            local: local_cancellation.token.clone(),
        });
        let job = JobExecution {
            turn: Arc::clone(self),
            job_id,
            deadline,
            cancellation: operation_cancellation,
            dispatch: local_cancellation,
        };
        let (sender, receiver) = oneshot::channel();
        let task = async move {
            let result = run_complete(job, request, options).await;
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

    fn generate_stream(
        self: &Arc<Self>,
        input: GenerationRequest,
        mut options: GenerationOptions,
    ) -> ModelStream<'static> {
        let timeout = effective_timeout(options.timeout, self.runtime.default_timeout);
        options.timeout = Some(timeout);
        if let Err(error) =
            validate_request_size(&input, &options, self.runtime.scheduler.max_request_bytes)
        {
            return ready_stream_error(error);
        }
        let call_id = ModelCallId::new();
        let (job_id, local_cancellation, guard, deadline) =
            match self.runtime.admit(self, call_id.clone(), timeout) {
                Ok(admission) => admission,
                Err(error) => return ready_stream_error(error),
            };
        let request = model_request(call_id, ModelCallMode::Stream, input, &options);
        let operation_cancellation: Arc<dyn CancellationSignal> = Arc::new(CombinedCancellation {
            parent: Arc::clone(&self.cancellation),
            local: local_cancellation.token.clone(),
        });
        let job = JobExecution {
            turn: Arc::clone(self),
            job_id,
            deadline,
            cancellation: operation_cancellation,
            dispatch: local_cancellation,
        };
        let (delta_sender, delta_receiver) = mpsc::channel(STREAM_DELTA_BUFFER);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let task = async move {
            let result = run_stream(job, request, options, delta_sender).await;
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
    fn capabilities(&self) -> jingwei_llm::ModelCapabilities {
        self.state.runtime.llm.capabilities()
    }

    fn generate<'a>(
        &'a self,
        input: &'a GenerationRequest,
        options: GenerationOptions,
    ) -> jingwei_llm::ModelFuture<'a, Result<GenerationResponse, ModelGatewayError>> {
        self.state.generate(input, options)
    }

    fn generate_stream<'a>(
        &'a self,
        input: GenerationRequest,
        options: GenerationOptions,
    ) -> ModelStream<'a> {
        self.state.generate_stream(input, options)
    }
}

fn model_request(
    call_id: ModelCallId,
    mode: ModelCallMode,
    input: GenerationRequest,
    options: &GenerationOptions,
) -> ModelRequest {
    ModelRequest {
        version: ModelRecordVersion::V1,
        call_id,
        mode,
        input,
        options: ModelRequestOptions {
            context: options.context.clone(),
            max_tokens: options.max_tokens,
            timeout: options.timeout.map(ModelTimeout::from_duration),
            limits: options.limits,
        },
    }
}

fn ready_complete_error(
    error: ModelGatewayError,
) -> jingwei_llm::ModelFuture<'static, Result<GenerationResponse, ModelGatewayError>> {
    Box::pin(async move { Err(error) })
}

fn ready_stream_error(error: ModelGatewayError) -> ModelStream<'static> {
    Box::pin(async_stream::stream! {
        yield Err(error);
    })
}

fn stream_projection(
    job_id: u64,
    mut deltas: mpsc::Receiver<GenerationDelta>,
    terminal: oneshot::Receiver<Result<GenerationResponse, ModelGatewayError>>,
) -> ModelStream<'static> {
    Box::pin(async_stream::stream! {
        while let Some(delta) = deltas.recv().await {
            yield Ok(GenerationStreamEvent::Delta(delta));
        }
        match terminal.await {
            Ok(Ok(response)) => yield Ok(GenerationStreamEvent::Finished(response)),
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
    dispatched: Notify,
}

impl JobCancellation {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            dispatched: Notify::new(),
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
        self.runtime
            .complete_job(&self.turn, self.job_id, closure_failure);
        self.armed = false;
        let _ = sender.send(result);
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
    job: JobExecution,
    request: ModelRequest,
    options: GenerationOptions,
) -> Result<GenerationResponse, ModelGatewayError> {
    let stopped = job.record_request(&request, None).await?;
    let semantic = if let Some(stop) = stopped {
        CompleteResolution::Stopped(stop)
    } else if let Err(stop) = job.wait_for_slot(None).await {
        CompleteResolution::Stopped(stop)
    } else {
        resolve_complete(&job, &request, options).await
    };
    let (result, output) = match semantic {
        CompleteResolution::Succeeded(completion) => (
            successful_result(&request, completion.clone()),
            Ok(completion),
        ),
        CompleteResolution::ModelFailed(error) => (
            failed_result(&request, &error, GenerationPartial::default()),
            Err(error.into()),
        ),
        CompleteResolution::Stopped(reason) => {
            let (result, error) = stopped_result(&request, reason);
            (result, Err(error))
        }
        CompleteResolution::Internal { code, message } => (
            internal_result(&request, code, message),
            Err(ModelGatewayError::Internal {
                code: code.to_string(),
                message: message.to_string(),
            }),
        ),
    };
    job.release(result_stop_reason(&result));
    job.record_result(&request, &result).await?;
    output
}

enum CompleteResolution {
    Succeeded(GenerationResponse),
    ModelFailed(LlmError),
    Stopped(ModelJobStopReason),
    Internal {
        code: &'static str,
        message: &'static str,
    },
}

async fn resolve_complete(
    job: &JobExecution,
    request: &ModelRequest,
    options: GenerationOptions,
) -> CompleteResolution {
    let llm = &job.turn.runtime.llm;
    match catch_unwind(AssertUnwindSafe(|| {
        validation::prepare(&request.input, &llm.capabilities(), ModelCallMode::Complete)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return CompleteResolution::ModelFailed(error),
        Err(_) => {
            return CompleteResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime preflight panicked",
            };
        }
    }
    if let Err(stop) = job.start_provider(None) {
        return CompleteResolution::Stopped(stop);
    }
    let adapter_guard = AdapterCancellationGuard(CancellationToken::new());
    let raw = match catch_unwind(AssertUnwindSafe(|| {
        llm.generate(&request.input, options, adapter_guard.token())
    })) {
        Ok(raw) => AssertUnwindSafe(raw).catch_unwind(),
        Err(_) => {
            return CompleteResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime operation panicked",
            };
        }
    };
    let output = tokio::select! {
        biased;
        _ = job.cancellation.cancelled() => return CompleteResolution::ModelFailed(LlmError::Cancelled),
        _ = tokio::time::sleep_until(job.deadline) => return CompleteResolution::ModelFailed(LlmError::Timeout),
        output = raw => output,
    };
    match output {
        Ok(Ok(completion)) => {
            match validation::response(&request.input, &completion, request.options.limits) {
                Ok(()) => CompleteResolution::Succeeded(completion),
                Err(error) => CompleteResolution::ModelFailed(error),
            }
        }
        Ok(Err(error)) => CompleteResolution::ModelFailed(error),
        Err(_) => CompleteResolution::Internal {
            code: "model_runtime_panicked",
            message: "Model runtime operation panicked",
        },
    }
}

async fn run_stream(
    job: JobExecution,
    request: ModelRequest,
    options: GenerationOptions,
    deltas: mpsc::Sender<GenerationDelta>,
) -> Result<GenerationResponse, ModelGatewayError> {
    let stopped = job.record_request(&request, Some(&deltas)).await?;
    let semantic = if let Some(stop) = stopped {
        StreamResolution::Stopped(stop)
    } else if let Err(stop) = job.wait_for_slot(Some(&deltas)).await {
        StreamResolution::Stopped(stop)
    } else {
        resolve_stream(&job, &request, options, &deltas).await
    };
    let (result, output) = match semantic {
        StreamResolution::Succeeded(response) => {
            (successful_result(&request, response.clone()), Ok(response))
        }
        StreamResolution::ModelFailed { error, partial } => {
            (failed_result(&request, &error, partial), Err(error.into()))
        }
        StreamResolution::ConsumerDropped(partial) => (
            consumer_dropped_result(&request, partial),
            Err(LlmError::Cancelled.into()),
        ),
        StreamResolution::Stopped(reason) => {
            let (result, error) = stopped_result(&request, reason);
            (result, Err(error))
        }
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
    job.release(result_stop_reason(&result));
    job.record_result(&request, &result).await?;
    output
}

enum StreamResolution {
    Succeeded(GenerationResponse),
    ModelFailed {
        error: LlmError,
        partial: GenerationPartial,
    },
    ConsumerDropped(GenerationPartial),
    Stopped(ModelJobStopReason),
    Internal {
        code: &'static str,
        message: &'static str,
        partial: GenerationPartial,
    },
}

async fn resolve_stream(
    job: &JobExecution,
    request: &ModelRequest,
    options: GenerationOptions,
    deltas: &mpsc::Sender<GenerationDelta>,
) -> StreamResolution {
    let llm = &job.turn.runtime.llm;
    let adapter_guard = AdapterCancellationGuard(CancellationToken::new());
    let mut accumulator = GenerationAccumulator::new(options.limits);
    match catch_unwind(AssertUnwindSafe(|| {
        validation::prepare(&request.input, &llm.capabilities(), ModelCallMode::Stream)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return StreamResolution::ModelFailed {
                error,
                partial: GenerationPartial::default(),
            };
        }
        Err(_) => {
            return StreamResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime preflight panicked",
                partial: GenerationPartial::default(),
            };
        }
    }
    // Preparing an owned stream request can be substantial. Keep it before the
    // final admission/deadline check, so copying cannot extend provider entry.
    let input = request.input.clone();
    if let Err(stop) = job.start_provider(Some(deltas)) {
        return StreamResolution::Stopped(stop);
    }
    let mut stream = match catch_unwind(AssertUnwindSafe(|| {
        llm.generate_stream(input, options, adapter_guard.token())
    })) {
        Ok(stream) => stream,
        Err(_) => {
            return StreamResolution::Internal {
                code: "model_runtime_panicked",
                message: "Model runtime operation panicked",
                partial: GenerationPartial::default(),
            };
        }
    };
    loop {
        let item = tokio::select! {
            biased;
            _ = job.cancellation.cancelled() => return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial: accumulator.partial().clone() },
            _ = tokio::time::sleep_until(job.deadline) => return StreamResolution::ModelFailed { error: LlmError::Timeout, partial: accumulator.partial().clone() },
            _ = deltas.closed() => return StreamResolution::ConsumerDropped(accumulator.partial().clone()),
            item = AssertUnwindSafe(stream.next()).catch_unwind() => item,
        };
        let delta = match item {
            Err(_) => {
                return StreamResolution::Internal {
                    code: "model_runtime_panicked",
                    message: "Model runtime operation panicked",
                    partial: accumulator.partial().clone(),
                };
            }
            Ok(Some(Err(error))) => {
                return StreamResolution::ModelFailed {
                    error,
                    partial: accumulator.partial().clone(),
                };
            }
            Ok(None) => {
                return StreamResolution::ModelFailed {
                    error: ModelProtocolError::MissingStreamTerminal.into(),
                    partial: accumulator.partial().clone(),
                };
            }
            Ok(Some(Ok(GenerationStreamEvent::Finished(response)))) => {
                let validated = accumulator
                    .verify_terminal(&response)
                    .map_err(LlmError::from)
                    .and_then(|_| {
                        validation::response(&request.input, &response, request.options.limits)
                    });
                return match validated {
                    Ok(()) => StreamResolution::Succeeded(response),
                    Err(error) => StreamResolution::ModelFailed {
                        error,
                        partial: accumulator.partial().clone(),
                    },
                };
            }
            Ok(Some(Ok(GenerationStreamEvent::Delta(delta)))) => delta,
        };
        if let Err(error) = accumulator.push(&delta) {
            return StreamResolution::ModelFailed {
                error: error.into(),
                partial: accumulator.partial().clone(),
            };
        }
        let sent = tokio::select! {
            biased;
            _ = job.cancellation.cancelled() => return StreamResolution::ModelFailed { error: LlmError::Cancelled, partial: accumulator.partial().clone() },
            _ = tokio::time::sleep_until(job.deadline) => return StreamResolution::ModelFailed { error: LlmError::Timeout, partial: accumulator.partial().clone() },
            sent = deltas.send(delta) => sent,
        };
        if sent.is_err() {
            return StreamResolution::ConsumerDropped(accumulator.partial().clone());
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

fn successful_result(request: &ModelRequest, response: GenerationResponse) -> ModelResult {
    ModelResult {
        version: ModelRecordVersion::V1,
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Succeeded { response },
    }
}

fn stopped_result(
    request: &ModelRequest,
    reason: ModelJobStopReason,
) -> (ModelResult, ModelGatewayError) {
    let partial = GenerationPartial::default();
    match reason {
        ModelJobStopReason::QueueTimeout => {
            let mut result = failed_result(request, &LlmError::Timeout, partial);
            if let ModelRecordedOutcome::Failed { code, message, .. } = &mut result.outcome {
                *code = "model_queue_timeout".to_string();
                *message = "Model operation exceeded its deadline while queued".to_string();
            }
            (result, ModelRuntimeError::QueueTimeout.into())
        }
        ModelJobStopReason::Timeout => (
            failed_result(request, &LlmError::Timeout, partial),
            LlmError::Timeout.into(),
        ),
        ModelJobStopReason::Cancelled => (
            failed_result(request, &LlmError::Cancelled, partial),
            LlmError::Cancelled.into(),
        ),
        ModelJobStopReason::ConsumerDropped => (
            consumer_dropped_result(request, partial),
            LlmError::Cancelled.into(),
        ),
        ModelJobStopReason::Failed => (
            internal_result(
                request,
                "model_scheduler_state",
                "Model job lost its execution state",
            ),
            ModelGatewayError::Internal {
                code: "model_scheduler_state".to_string(),
                message: "Model job lost its execution state".to_string(),
            },
        ),
    }
}

fn result_stop_reason(result: &ModelResult) -> Option<ModelJobStopReason> {
    match &result.outcome {
        ModelRecordedOutcome::Succeeded { .. } => None,
        ModelRecordedOutcome::Failed { code, category, .. } => {
            Some(match (code.as_str(), category) {
                ("model_queue_timeout", _) => ModelJobStopReason::QueueTimeout,
                ("model_stream_consumer_dropped", _) => ModelJobStopReason::ConsumerDropped,
                (_, ModelFailureCategory::Timeout) => ModelJobStopReason::Timeout,
                (_, ModelFailureCategory::Cancelled) => ModelJobStopReason::Cancelled,
                _ => ModelJobStopReason::Failed,
            })
        }
    }
}

fn failed_result(
    request: &ModelRequest,
    error: &LlmError,
    partial: GenerationPartial,
) -> ModelResult {
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
        LlmError::Protocol(_) => (
            ModelFailureCategory::Protocol,
            "model_protocol",
            "Model request or response violated its protocol",
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
        version: ModelRecordVersion::V1,
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category,
            code: code.to_string(),
            message: message.to_string(),
            retryable,
            upstream_status,
            partial,
        },
    }
}

fn consumer_dropped_result(request: &ModelRequest, partial: GenerationPartial) -> ModelResult {
    ModelResult {
        version: ModelRecordVersion::V1,
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Cancelled,
            code: "model_stream_consumer_dropped".to_string(),
            message: "Model stream consumer dropped".to_string(),
            retryable: false,
            upstream_status: None,
            partial,
        },
    }
}

fn internal_result(
    request: &ModelRequest,
    code: &'static str,
    message: &'static str,
) -> ModelResult {
    internal_result_with_partial(request, code, message, GenerationPartial::default())
}

fn internal_result_with_partial(
    request: &ModelRequest,
    code: &'static str,
    message: &'static str,
    partial: GenerationPartial,
) -> ModelResult {
    ModelResult {
        version: ModelRecordVersion::V1,
        call_id: request.call_id.clone(),
        outcome: ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Internal,
            code: code.to_string(),
            message: message.to_string(),
            retryable: false,
            upstream_status: None,
            partial,
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

fn effective_timeout(call: Option<Duration>, runtime: Duration) -> Duration {
    call.map_or(runtime, |call| call.min(runtime))
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
