//! Canonical owned-turn implementation of the Jingwei AgentRuntime interface.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt;
use jingwei_agent::{
    AGENT_RUNTIME, Agent, AgentContext, AgentError, AgentEventKind, AgentFuture, AgentRuntime,
    AgentRuntimeError, AgentTurnCanceller, AgentTurnController, AgentTurnInput, AgentTurnOutput,
    AgentTurnReport, AgentTurnRequest, CapabilityTurnFailure, DriveFailure, NotAdmittedFailure,
    SettlementAttempt, TerminalAttempt, TurnClosureStatus, TurnDisposition, TurnFailure,
    TurnFailureContext, TurnFinally,
};
use jingwei_core::{
    CancellationFuture, CancellationSignal, DoneStatus, SessionEvent, SessionEventKind, SessionId,
    TurnId,
};
use jingwei_llm::{
    LLM_RUNTIME, LlmError, LlmRuntime, ModelEventRecorder, ModelFinishMode, ModelGateway,
    ModelGatewayError, ModelRecord, ModelRuntimeError, ModelTurn, ModelTurnBinding,
    ModelTurnFailure,
};
use jingwei_plugin::{
    AgentBinding, FactoryContext, Hook, HookBinding, LifecycleFuture, ManagedService, MountContext,
    MountError, Plugin, PluginDescriptor, PluginId, RuntimeError, ServiceFactory, ServiceLifecycle,
    StopReason,
};
use jingwei_session::{
    SESSION_RUNTIME, SessionEventDraft, SessionRuntime, SessionRuntimeError, SessionTurn,
    TurnCommitSummary,
};
use jingwei_tool::{
    TOOL_RUNTIME, ToolEventRecorder, ToolFinishMode, ToolGateway, ToolRecord, ToolRuntime,
    ToolRuntimeError, ToolTurn, ToolTurnBinding, ToolTurnFailure,
};
use tokio::runtime::Handle;
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};
use tokio_util::sync::CancellationToken;

const AGENT_RUNTIME_DEPENDENCIES: &[jingwei_core::CapabilityId] = &[SESSION_RUNTIME];
const AGENT_RUNTIME_OPTIONAL_USES: &[jingwei_core::CapabilityId] = &[LLM_RUNTIME, TOOL_RUNTIME];
const DEFAULT_MAX_IN_FLIGHT: usize = 1024;
const MAX_DETACHED_FAILURES: usize = 16;

/// Explicit canonical AgentRuntime provider plugin.
pub struct CanonicalAgentRuntimePlugin {
    max_in_flight: usize,
}

impl CanonicalAgentRuntimePlugin {
    pub const fn new() -> Self {
        Self {
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }

    #[must_use]
    pub const fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight;
        self
    }
}

impl Default for CanonicalAgentRuntimePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for CanonicalAgentRuntimePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider(
            "jingwei-agent-runtime-canonical",
            AGENT_RUNTIME,
            "canonical",
        )
        .requires_capabilities(AGENT_RUNTIME_DEPENDENCIES)
        .uses_capabilities(AGENT_RUNTIME_OPTIONAL_USES)
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_agent_runtime_factory(Arc::new(CanonicalAgentRuntimeFactory {
            max_in_flight: self.max_in_flight,
        }))
    }
}

struct CanonicalAgentRuntimeFactory {
    max_in_flight: usize,
}

impl ServiceFactory<dyn AgentRuntime> for CanonicalAgentRuntimeFactory {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn AgentRuntime>, RuntimeError>> {
        let max_in_flight = self.max_in_flight;
        Box::pin(async move {
            let sessions = ctx.session_runtime().ok_or_else(|| {
                RuntimeError::new("canonical AgentRuntime requires the selected SessionRuntime")
            })?;
            let llm_runtime = ctx.llm_runtime();
            let tool_runtime = ctx.tool_runtime();
            let agents = freeze_agents(ctx.agent_bindings().ok_or_else(|| {
                RuntimeError::new("Agent bindings are hidden from the AgentRuntime factory")
            })?);
            if agents.values().any(|entry| entry.tools_allowed) && tool_runtime.is_none() {
                return Err(RuntimeError::new(
                    "a tool-enabled Agent requires the selected ToolRuntime",
                ));
            }
            let hooks = freeze_hooks(ctx.hook_bindings().ok_or_else(|| {
                RuntimeError::new("Hook bindings are hidden from the AgentRuntime factory")
            })?);
            let executor = Handle::try_current().map_err(|error| {
                RuntimeError::new(format!("AgentRuntime requires a Tokio executor: {error}"))
            })?;
            let state = Arc::new(RuntimeState::new(
                sessions,
                llm_runtime,
                tool_runtime,
                agents,
                hooks,
                executor,
                max_in_flight,
            ));
            let runtime: Arc<dyn AgentRuntime> = Arc::new(CanonicalAgentRuntime {
                state: Arc::clone(&state),
            });
            Ok(ManagedService::new(
                runtime,
                Box::new(AgentRuntimeLifecycle { state }),
            ))
        })
    }
}

#[derive(Clone)]
struct AgentEntry {
    owner: PluginId,
    agent: Arc<dyn Agent>,
    model_allowed: bool,
    tools_allowed: bool,
}

fn freeze_agents(bindings: Vec<AgentBinding>) -> BTreeMap<String, AgentEntry> {
    bindings
        .into_iter()
        .map(|binding| {
            (
                binding.key().to_string(),
                AgentEntry {
                    owner: binding.owner(),
                    agent: binding.agent(),
                    model_allowed: binding.requires(LLM_RUNTIME),
                    tools_allowed: binding.requires(TOOL_RUNTIME),
                },
            )
        })
        .collect()
}

#[derive(Clone)]
struct HookEntry {
    owner: PluginId,
    ordinal: usize,
    hook: Arc<dyn Hook>,
}

fn freeze_hooks(bindings: Vec<HookBinding>) -> Vec<HookEntry> {
    bindings
        .into_iter()
        .map(|binding| HookEntry {
            owner: binding.owner(),
            ordinal: binding.ordinal(),
            hook: binding.hook(),
        })
        .collect()
}

struct CanonicalAgentRuntime {
    state: Arc<RuntimeState>,
}

struct RuntimeState {
    sessions: Arc<dyn SessionRuntime>,
    llm_runtime: Option<Arc<dyn LlmRuntime>>,
    tool_runtime: Option<Arc<dyn ToolRuntime>>,
    agents: BTreeMap<String, AgentEntry>,
    hooks: Vec<HookEntry>,
    executor: Handle,
    max_in_flight: usize,
    control: Mutex<Control>,
    drained: Notify,
}

impl RuntimeState {
    fn new(
        sessions: Arc<dyn SessionRuntime>,
        llm_runtime: Option<Arc<dyn LlmRuntime>>,
        tool_runtime: Option<Arc<dyn ToolRuntime>>,
        agents: BTreeMap<String, AgentEntry>,
        hooks: Vec<HookEntry>,
        executor: Handle,
        max_in_flight: usize,
    ) -> Self {
        Self {
            sessions,
            llm_runtime,
            tool_runtime,
            agents,
            hooks,
            executor,
            max_in_flight,
            control: Mutex::new(Control::new()),
            drained: Notify::new(),
        }
    }

    fn lock_control(&self) -> MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn start(&self) -> Result<(), RuntimeError> {
        let mut control = self.lock_control();
        if control.lifecycle != RuntimeLifecycleState::Constructed {
            return Err(RuntimeError::new(
                "AgentRuntime lifecycle start was repeated",
            ));
        }
        control.lifecycle = RuntimeLifecycleState::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<(), RuntimeError> {
        {
            let mut control = self.lock_control();
            match control.lifecycle {
                RuntimeLifecycleState::Stopped => return Ok(()),
                RuntimeLifecycleState::Constructed
                | RuntimeLifecycleState::Running
                | RuntimeLifecycleState::Stopping => {
                    control.lifecycle = RuntimeLifecycleState::Stopping;
                    for job in control.jobs.values() {
                        job.request_cancel(CancelReason::RuntimeStopping);
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
            control.lifecycle = RuntimeLifecycleState::Stopped;
            std::mem::take(&mut control.detached_closure_failures)
        };
        if failures.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::new(format!(
                "{} detached admitted turn(s) failed canonical closure: {}",
                failures.len(),
                failures.join("; ")
            )))
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RuntimeLifecycleState {
    Constructed,
    Running,
    Stopping,
    Stopped,
}

struct Control {
    lifecycle: RuntimeLifecycleState,
    next_job_id: u64,
    jobs: BTreeMap<u64, Arc<JobCancellation>>,
    detached_closure_failures: Vec<String>,
}

impl Control {
    fn new() -> Self {
        Self {
            lifecycle: RuntimeLifecycleState::Constructed,
            next_job_id: 0,
            jobs: BTreeMap::new(),
            detached_closure_failures: Vec::new(),
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
enum CancelReason {
    None = 0,
    Caller = 1,
    RuntimeStopping = 2,
}

struct JobCancellation {
    token: CancellationToken,
    reason: AtomicU8,
}

impl JobCancellation {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: AtomicU8::new(CancelReason::None as u8),
        }
    }

    fn request_cancel(&self, reason: CancelReason) {
        if self
            .reason
            .compare_exchange(
                CancelReason::None as u8,
                reason as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.token.cancel();
        }
    }

    fn reason(&self) -> CancelReason {
        match self.reason.load(Ordering::Acquire) {
            value if value == CancelReason::Caller as u8 => CancelReason::Caller,
            value if value == CancelReason::RuntimeStopping as u8 => CancelReason::RuntimeStopping,
            _ => CancelReason::None,
        }
    }

    fn not_admitted_failure(&self) -> NotAdmittedFailure {
        match self.reason() {
            CancelReason::Caller => NotAdmittedFailure::CallerCancelled,
            CancelReason::None | CancelReason::RuntimeStopping => {
                NotAdmittedFailure::RuntimeStopping
            }
        }
    }
}

impl AgentTurnCanceller for JobCancellation {
    fn cancel(&self) {
        self.request_cancel(CancelReason::Caller);
    }
}

struct TurnController {
    receiver: oneshot::Receiver<Result<AgentTurnReport, AgentRuntimeError>>,
    canceller: Arc<JobCancellation>,
}

impl AgentTurnController for TurnController {
    fn canceller(&self) -> Arc<dyn AgentTurnCanceller> {
        Arc::clone(&self.canceller) as Arc<dyn AgentTurnCanceller>
    }

    fn wait(self: Box<Self>) -> AgentFuture<'static, Result<AgentTurnReport, AgentRuntimeError>> {
        Box::pin(async move {
            self.receiver
                .await
                .unwrap_or(Err(AgentRuntimeError::Stopped))
        })
    }
}

struct DriverRequest {
    session_id: SessionId,
    agent_key: String,
    user_message: String,
    agent: AgentEntry,
    cancellation: Arc<JobCancellation>,
}

impl AgentRuntime for CanonicalAgentRuntime {
    fn start_turn(
        &self,
        request: AgentTurnRequest,
    ) -> Result<Box<dyn AgentTurnController>, AgentRuntimeError> {
        let (session_id, agent_key, user_message) = request.into_parts();
        let (job_id, agent, cancellation) = {
            let mut control = self.state.lock_control();
            if control.lifecycle != RuntimeLifecycleState::Running {
                return Err(AgentRuntimeError::Stopped);
            }
            let agent = self.state.agents.get(&agent_key).cloned().ok_or_else(|| {
                AgentRuntimeError::AgentNotFound {
                    key: agent_key.clone(),
                }
            })?;
            if control.jobs.len() >= self.state.max_in_flight {
                return Err(AgentRuntimeError::Overloaded);
            }
            let job_id = control.next_job_id;
            control.next_job_id = control
                .next_job_id
                .checked_add(1)
                .ok_or(AgentRuntimeError::Overloaded)?;
            let cancellation = Arc::new(JobCancellation::new());
            control.jobs.insert(job_id, Arc::clone(&cancellation));
            (job_id, agent, cancellation)
        };

        let (sender, receiver) = oneshot::channel();
        let state = Arc::clone(&self.state);
        let executor = self.state.executor.clone();
        let driver_cancellation = Arc::clone(&cancellation);
        let guard = JobGuard::new(Arc::clone(&state), job_id);
        let task = executor.spawn(async move {
            let result = drive_turn(
                Arc::clone(&state),
                DriverRequest {
                    session_id,
                    agent_key,
                    user_message,
                    agent,
                    cancellation: driver_cancellation,
                },
            )
            .await;
            guard.complete(sender, result);
        });
        drop(task);

        Ok(Box::new(TurnController {
            receiver,
            canceller: cancellation,
        }))
    }
}

struct JobGuard {
    state: Arc<RuntimeState>,
    job_id: u64,
    armed: bool,
}

impl JobGuard {
    fn new(state: Arc<RuntimeState>, job_id: u64) -> Self {
        Self {
            state,
            job_id,
            armed: true,
        }
    }

    fn complete(
        mut self,
        sender: oneshot::Sender<Result<AgentTurnReport, AgentRuntimeError>>,
        result: Result<AgentTurnReport, AgentRuntimeError>,
    ) {
        let undelivered = sender.send(result).err();
        let mut control = self.state.lock_control();
        if let Some(Err(AgentRuntimeError::Turn(failure))) = undelivered
            && failure.closure_status() != TurnClosureStatus::Closed
        {
            tracing::error!(
                session_id = %failure.session_id(),
                turn_id = %failure.turn_id(),
                disposition = ?failure.disposition(),
                closure = ?failure.closure_status(),
                "detached Agent turn failed canonical closure"
            );
            if control.detached_closure_failures.len() < MAX_DETACHED_FAILURES {
                control.detached_closure_failures.push(failure.to_string());
            }
        }
        control.jobs.remove(&self.job_id);
        if control.jobs.is_empty() {
            self.state.drained.notify_one();
        }
        self.armed = false;
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut control = self.state.lock_control();
        if control.detached_closure_failures.len() < MAX_DETACHED_FAILURES {
            control.detached_closure_failures.push(format!(
                "internal Agent driver {} unwound before closure",
                self.job_id
            ));
        }
        control.jobs.remove(&self.job_id);
        if control.jobs.is_empty() {
            self.state.drained.notify_one();
        }
        tracing::error!(
            job_id = self.job_id,
            "Agent driver unwound before resolution"
        );
    }
}

/// Shared ownership adapter around the one canonical Session lease.
///
/// ModelRuntime and ToolRuntime receive only their narrowed recorder views. The
/// underlying lease remains owned here until AgentRuntime drains capability
/// work and consumes it exactly once for settlement.
struct SharedTurn {
    turn: AsyncMutex<Option<Box<dyn SessionTurn>>>,
}

impl SharedTurn {
    fn new(turn: Box<dyn SessionTurn>) -> Self {
        Self {
            turn: AsyncMutex::new(Some(turn)),
        }
    }

    async fn append_kind(
        &self,
        kind: SessionEventKind,
    ) -> Result<Arc<SessionEvent>, SessionRuntimeError> {
        let draft = SessionEventDraft::new(kind);
        let guard = self.turn.lock().await;
        let turn = guard
            .as_ref()
            .expect("canonical AgentRuntime owns the Session lease until settlement");
        turn.append(&draft).await
    }

    async fn settle(&self) -> Result<TurnCommitSummary, SessionRuntimeError> {
        let turn = {
            let mut guard = self.turn.lock().await;
            guard
                .take()
                .expect("canonical AgentRuntime settles the Session lease exactly once")
        };
        turn.settle().await
    }
}

impl ToolEventRecorder for SharedTurn {
    fn append(
        &self,
        record: ToolRecord,
    ) -> jingwei_tool::ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move { self.append_kind(record.into_session_event_kind()).await })
    }
}

impl ModelEventRecorder for SharedTurn {
    fn append(
        &self,
        record: ModelRecord,
    ) -> jingwei_llm::ModelFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move { self.append_kind(record.into_session_event_kind()).await })
    }
}

async fn drive_turn(
    state: Arc<RuntimeState>,
    request: DriverRequest,
) -> Result<AgentTurnReport, AgentRuntimeError> {
    let mut admission = state.sessions.begin_turn(&request.session_id);
    let session_turn = tokio::select! {
        biased;
        _ = request.cancellation.token.cancelled() => {
            return Err(AgentRuntimeError::NotAdmitted(
                request.cancellation.not_admitted_failure(),
            ));
        }
        result = &mut admission => {
            result.map_err(|error| {
                AgentRuntimeError::NotAdmitted(NotAdmittedFailure::Session(error))
            })?
        }
    };
    drop(admission);

    let session_id = session_turn.admission().session_id().clone();
    let turn_id = session_turn.admission().turn_id().clone();
    let history = session_turn.admission().history();
    let shared_turn = Arc::new(SharedTurn::new(session_turn));

    run_start_hooks(&state.hooks, &session_id, &turn_id);

    let start_result = shared_turn
        .append_kind(SessionEventKind::UserMessage {
            text: request.user_message.clone(),
        })
        .await;
    let partial_text = Mutex::new(String::new());
    let cancellation_signal: Arc<dyn CancellationSignal> =
        Arc::new(TokenCancellationSignal(request.cancellation.token.clone()));
    let mut model_turn: Option<Box<dyn ModelTurn>> = None;
    let mut tool_turn: Option<Box<dyn ToolTurn>> = None;
    let mut resolved = match start_result {
        Err(error) => ResolvedIntent::start_envelope_failed(error),
        Ok(_) if request.cancellation.token.is_cancelled() => {
            ResolvedIntent::cancelled(String::new())
        }
        Ok(_) => {
            let model_binding_failure = if request.agent.model_allowed {
                let recorder: Arc<dyn ModelEventRecorder> = shared_turn.clone();
                let binding = ModelTurnBinding::new(Arc::clone(&cancellation_signal), recorder);
                let bound = state
                    .llm_runtime
                    .as_ref()
                    .ok_or_else(|| ModelRuntimeError::Internal {
                        code: "model_runtime_missing".to_string(),
                        message: "model-enabled Agent has no selected LlmRuntime".to_string(),
                    });
                match bound.and_then(|runtime| runtime.bind_turn(binding)) {
                    Ok(bound) => {
                        model_turn = Some(bound);
                        None
                    }
                    Err(error) => Some(error),
                }
            } else {
                None
            };
            let tool_binding_failure = if model_binding_failure.is_none()
                && request.agent.tools_allowed
            {
                let recorder: Arc<dyn ToolEventRecorder> = shared_turn.clone();
                let binding = ToolTurnBinding::new(
                    jingwei_tool::ToolCaller::new(request.agent.owner.as_str()),
                    Arc::clone(&cancellation_signal),
                    recorder,
                );
                let bound = state
                    .tool_runtime
                    .as_ref()
                    .ok_or_else(|| ToolRuntimeError::Internal {
                        code: "tool_runtime_missing".to_string(),
                        message: "tool-enabled Agent has no selected ToolRuntime".to_string(),
                    });
                match bound.and_then(|runtime| runtime.bind_turn(binding)) {
                    Ok(bound) => {
                        tool_turn = Some(bound);
                        None
                    }
                    Err(error) => Some(error),
                }
            } else {
                None
            };
            if let Some(error) = model_binding_failure {
                ResolvedIntent::model_runtime_failed(error, String::new())
            } else if let Some(error) = tool_binding_failure {
                ResolvedIntent::tool_failed(error, String::new())
            } else {
                let context = TurnContext {
                    session_turn: shared_turn.as_ref(),
                    model: model_turn.as_ref().map(|turn| turn.gateway()),
                    tools: tool_turn.as_ref().map(|turn| turn.gateway()),
                    cancellation: cancellation_signal.as_ref(),
                    partial_text: &partial_text,
                };
                run_agent_body(&request, &session_id, &turn_id, history.as_ref(), &context).await
            }
        }
    };

    if !resolved.has_agent_output {
        resolved.final_text = lock_unpoisoned(&partial_text).clone();
    }

    if request.cancellation.token.is_cancelled()
        && matches!(
            resolved.disposition,
            TurnDisposition::Completed | TurnDisposition::WaitingForInput
        )
    {
        resolved = ResolvedIntent::cancelled(lock_unpoisoned(&partial_text).clone());
    }

    let mut model_failure: Option<ModelTurnFailure> = None;
    if let Some(bound) = model_turn {
        let mode = match resolved.disposition {
            TurnDisposition::Completed | TurnDisposition::WaitingForInput => {
                ModelFinishMode::Graceful
            }
            TurnDisposition::Cancelled | TurnDisposition::Failed => ModelFinishMode::Cancel,
        };
        model_failure = bound.finish(mode).await.err();
    }

    if request.cancellation.token.is_cancelled()
        && matches!(
            resolved.disposition,
            TurnDisposition::Completed | TurnDisposition::WaitingForInput
        )
    {
        resolved = ResolvedIntent::cancelled(lock_unpoisoned(&partial_text).clone());
    }

    let mut tool_failure: Option<ToolTurnFailure> = None;
    if let Some(bound) = tool_turn {
        let mode = match (resolved.disposition, model_failure.is_none()) {
            (TurnDisposition::Completed | TurnDisposition::WaitingForInput, true) => {
                ToolFinishMode::Graceful
            }
            _ => ToolFinishMode::Cancel,
        };
        tool_failure = bound.finish(mode).await.err();
    }

    if request.cancellation.token.is_cancelled()
        && matches!(
            resolved.disposition,
            TurnDisposition::Completed | TurnDisposition::WaitingForInput
        )
    {
        resolved = ResolvedIntent::cancelled(lock_unpoisoned(&partial_text).clone());
    }

    if model_failure.is_some() || tool_failure.is_some() {
        let failure = CapabilityTurnFailure::new(model_failure, tool_failure)
            .expect("at least one capability drain failed");
        resolved = ResolvedIntent::capability_closure_failed(failure, resolved.final_text);
    }

    let terminal_result = shared_turn.append_kind(resolved.terminal_kind()).await;
    let settlement_result = shared_turn.settle().await;

    if let Ok(summary) = &settlement_result
        && (resolved.disposition == TurnDisposition::Failed
            || (resolved.disposition == TurnDisposition::Cancelled && !resolved.has_agent_output))
    {
        resolved.final_text = canonical_partial_text(summary.events());
    }

    let closure = closure_status_from_results(&terminal_result, &settlement_result);
    run_finally_hooks(
        &state.hooks,
        &TurnFinally::new(
            session_id.clone(),
            turn_id.clone(),
            resolved.disposition,
            closure,
        ),
    );

    let terminal_attempt = match terminal_result {
        Ok(event) => TerminalAttempt::Committed(event),
        Err(error) => TerminalAttempt::Failed(error),
    };
    let settlement_attempt = match settlement_result {
        Ok(summary) => SettlementAttempt::Settled(summary.into_events().into()),
        Err(error) => SettlementAttempt::Failed(error),
    };
    let report = match (&terminal_attempt, &settlement_attempt) {
        (TerminalAttempt::Committed(_), SettlementAttempt::Settled(events)) => {
            Some(AgentTurnReport::new(
                session_id.clone(),
                turn_id.clone(),
                resolved.disposition,
                resolved.final_text.clone(),
                resolved.artifact.clone(),
                Arc::clone(events),
            ))
        }
        _ => None,
    };

    if resolved.disposition != TurnDisposition::Failed
        && let Some(report) = report
    {
        return Ok(report);
    }

    let context = TurnFailureContext::new(
        session_id,
        turn_id,
        resolved.disposition,
        resolved.final_text,
        resolved.artifact,
        report,
        resolved.drive_failure,
    );
    let failure = TurnFailure::new(context, terminal_attempt, settlement_attempt)
        .expect("canonical AgentRuntime must produce valid total failure evidence");
    Err(AgentRuntimeError::Turn(Box::new(failure)))
}

fn closure_status_from_results(
    terminal_result: &Result<Arc<SessionEvent>, SessionRuntimeError>,
    settlement_result: &Result<jingwei_session::TurnCommitSummary, SessionRuntimeError>,
) -> TurnClosureStatus {
    match (terminal_result.is_ok(), settlement_result.is_ok()) {
        (true, true) => TurnClosureStatus::Closed,
        (false, true) => TurnClosureStatus::TerminalFailed,
        (true, false) => TurnClosureStatus::SettlementFailed,
        (false, false) => TurnClosureStatus::TerminalAndSettlementFailed,
    }
}

async fn run_agent_body(
    request: &DriverRequest,
    session_id: &SessionId,
    turn_id: &TurnId,
    history: &[SessionEvent],
    context: &TurnContext<'_>,
) -> ResolvedIntent {
    if request.cancellation.token.is_cancelled() {
        return ResolvedIntent::cancelled(context.partial_text());
    }
    let input = AgentTurnInput {
        session_id,
        turn_id: turn_id.clone(),
        user_message: &request.user_message,
        history,
    };
    let future = match catch_unwind(AssertUnwindSafe(|| {
        request.agent.agent.run_turn(input, context)
    })) {
        Ok(future) => future,
        Err(_) => {
            tracing::warn!(
                agent = %request.agent_key,
                owner = %request.agent.owner,
                "Agent panicked before returning its turn future"
            );
            return ResolvedIntent::panicked(context.partial_text());
        }
    };
    let polled = AssertUnwindSafe(future).catch_unwind();
    let result = tokio::select! {
        biased;
        _ = request.cancellation.token.cancelled() => {
            return ResolvedIntent::cancelled(context.partial_text());
        }
        result = polled => result,
    };
    match result {
        Err(_) => {
            tracing::warn!(
                agent = %request.agent_key,
                owner = %request.agent.owner,
                "Agent future panicked"
            );
            ResolvedIntent::panicked(context.partial_text())
        }
        Ok(Err(error)) if error.is_cancelled() => ResolvedIntent::cancelled(context.partial_text()),
        Ok(Err(error)) => ResolvedIntent::agent_failed(error, context.partial_text()),
        Ok(Ok(output)) => ResolvedIntent::from_output(output),
    }
}

struct TurnContext<'a> {
    session_turn: &'a SharedTurn,
    model: Option<&'a dyn ModelGateway>,
    tools: Option<&'a dyn ToolGateway>,
    cancellation: &'a dyn CancellationSignal,
    partial_text: &'a Mutex<String>,
}

impl TurnContext<'_> {
    fn partial_text(&self) -> String {
        lock_unpoisoned(self.partial_text).clone()
    }
}

impl AgentContext for TurnContext<'_> {
    fn emit(&self, kind: AgentEventKind) -> AgentFuture<'_, Result<(), AgentError>> {
        Box::pin(async move {
            let delta = match &kind {
                AgentEventKind::AssistantDelta { text } => Some(text.clone()),
                _ => None,
            };
            self.session_turn
                .append_kind(kind.into_session_event_kind())
                .await?;
            if let Some(delta) = delta {
                lock_unpoisoned(self.partial_text).push_str(&delta);
            }
            Ok(())
        })
    }

    fn model(&self) -> Option<&dyn ModelGateway> {
        self.model
    }

    fn tools(&self) -> Option<&dyn ToolGateway> {
        self.tools
    }

    fn cancellation(&self) -> &dyn CancellationSignal {
        self.cancellation
    }
}

struct TokenCancellationSignal(CancellationToken);

impl CancellationSignal for TokenCancellationSignal {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    fn cancelled(&self) -> CancellationFuture<'_> {
        Box::pin(self.0.cancelled())
    }
}

struct TerminalErrorFields {
    code: String,
    message: String,
    retryable: bool,
}

struct ResolvedIntent {
    disposition: TurnDisposition,
    final_text: String,
    artifact: Option<serde_json::Value>,
    drive_failure: Option<DriveFailure>,
    terminal_error: Option<TerminalErrorFields>,
    has_agent_output: bool,
}

impl ResolvedIntent {
    fn from_output(output: AgentTurnOutput) -> Self {
        Self {
            disposition: output.outcome.into(),
            final_text: output.final_text,
            artifact: output.artifact,
            drive_failure: None,
            terminal_error: None,
            has_agent_output: true,
        }
    }

    fn cancelled(final_text: String) -> Self {
        Self {
            disposition: TurnDisposition::Cancelled,
            final_text,
            artifact: None,
            drive_failure: None,
            terminal_error: None,
            has_agent_output: false,
        }
    }

    fn start_envelope_failed(error: SessionRuntimeError) -> Self {
        let terminal_error = TerminalErrorFields {
            code: "session_start_envelope".to_string(),
            message: "failed to record the canonical turn start".to_string(),
            retryable: false,
        };
        Self {
            disposition: TurnDisposition::Failed,
            final_text: String::new(),
            artifact: None,
            drive_failure: Some(DriveFailure::StartEnvelope(error)),
            terminal_error: Some(terminal_error),
            has_agent_output: false,
        }
    }

    fn agent_failed(error: AgentError, final_text: String) -> Self {
        let terminal_error = terminal_fields_for_agent_error(&error);
        Self {
            disposition: TurnDisposition::Failed,
            final_text,
            artifact: None,
            drive_failure: Some(DriveFailure::Agent(error)),
            terminal_error: Some(terminal_error),
            has_agent_output: false,
        }
    }

    fn tool_failed(error: ToolRuntimeError, final_text: String) -> Self {
        if matches!(&error, ToolRuntimeError::Cancelled { .. }) {
            Self::cancelled(final_text)
        } else {
            Self::agent_failed(AgentError::Tool(error), final_text)
        }
    }

    fn model_runtime_failed(error: ModelRuntimeError, final_text: String) -> Self {
        Self::agent_failed(
            AgentError::Model(ModelGatewayError::Runtime(error)),
            final_text,
        )
    }

    fn capability_closure_failed(error: CapabilityTurnFailure, final_text: String) -> Self {
        Self::agent_failed(AgentError::CapabilityClosure(error), final_text)
    }

    fn panicked(final_text: String) -> Self {
        Self {
            disposition: TurnDisposition::Failed,
            final_text,
            artifact: None,
            drive_failure: Some(DriveFailure::Panicked),
            terminal_error: Some(TerminalErrorFields {
                code: "agent_panicked".to_string(),
                message: "agent panicked".to_string(),
                retryable: false,
            }),
            has_agent_output: false,
        }
    }

    fn terminal_kind(&self) -> SessionEventKind {
        match self.disposition {
            TurnDisposition::Completed => SessionEventKind::Done {
                status: DoneStatus::Completed,
                artifact: self.artifact.clone(),
            },
            TurnDisposition::WaitingForInput => SessionEventKind::Done {
                status: DoneStatus::WaitingForInput,
                artifact: self.artifact.clone(),
            },
            TurnDisposition::Cancelled => SessionEventKind::Done {
                status: DoneStatus::Cancelled,
                artifact: self.artifact.clone(),
            },
            TurnDisposition::Failed => {
                let error = self
                    .terminal_error
                    .as_ref()
                    .expect("a failed intent must retain structured terminal fields");
                SessionEventKind::Error {
                    code: error.code.clone(),
                    message: error.message.clone(),
                    retryable: error.retryable,
                }
            }
        }
    }
}

fn terminal_fields_for_agent_error(error: &AgentError) -> TerminalErrorFields {
    match error {
        AgentError::Cancelled => TerminalErrorFields {
            code: "agent_cancelled".to_string(),
            message: "agent cancelled".to_string(),
            retryable: false,
        },
        AgentError::Failed(failure) => TerminalErrorFields {
            code: failure.code().to_string(),
            message: failure.message().to_string(),
            retryable: failure.retryable(),
        },
        AgentError::Model(error) => terminal_fields_for_model_error(error),
        AgentError::Tool(error) => terminal_fields_for_tool_error(error),
        AgentError::CapabilityClosure(error) => terminal_fields_for_capability_closure(error),
        AgentError::Session(_) => TerminalErrorFields {
            code: "session_runtime".to_string(),
            message: "canonical Session operation failed".to_string(),
            retryable: false,
        },
    }
}

fn terminal_fields_for_tool_error(error: &ToolRuntimeError) -> TerminalErrorFields {
    let (code, message, retryable) = match error {
        ToolRuntimeError::Stopped => ("tool_runtime_stopped", "Tool runtime is stopped", false),
        ToolRuntimeError::TurnClosed => ("tool_turn_closed", "Tool turn is closed", false),
        ToolRuntimeError::Overloaded => (
            "tool_runtime_overloaded",
            "Tool runtime is overloaded",
            true,
        ),
        ToolRuntimeError::Preflight(_) => (
            "tool_invalid_request",
            "Tool request failed controlled preflight",
            false,
        ),
        ToolRuntimeError::Cancelled { .. } => {
            ("tool_cancelled", "Tool execution was cancelled", false)
        }
        ToolRuntimeError::Recording(_) => {
            ("tool_recording", "canonical Tool recording failed", false)
        }
        ToolRuntimeError::Internal { .. } => (
            "tool_runtime_internal",
            "Tool runtime internal failure",
            false,
        ),
    };
    TerminalErrorFields {
        code: code.to_string(),
        message: message.to_string(),
        retryable,
    }
}

fn terminal_fields_for_model_error(error: &ModelGatewayError) -> TerminalErrorFields {
    let (code, message, retryable) = match error {
        ModelGatewayError::Model(LlmError::Upstream { status, .. }) => (
            "model_upstream",
            "model upstream request failed",
            *status == 408 || *status == 429 || *status >= 500,
        ),
        ModelGatewayError::Model(LlmError::Timeout) => {
            ("model_timeout", "model request timed out", true)
        }
        ModelGatewayError::Model(LlmError::StreamParse(_)) => {
            ("model_stream_parse", "model stream parsing failed", false)
        }
        ModelGatewayError::Model(LlmError::Protocol(_)) => {
            ("model_protocol", "model protocol validation failed", false)
        }
        ModelGatewayError::Model(LlmError::Cancelled) => {
            ("model_cancelled", "model request cancelled", false)
        }
        ModelGatewayError::Model(LlmError::Adapter(_)) => {
            ("model_adapter", "model adapter failed", true)
        }
        ModelGatewayError::Runtime(ModelRuntimeError::Stopped) => {
            ("model_runtime_stopped", "model runtime is stopped", false)
        }
        ModelGatewayError::Runtime(ModelRuntimeError::TurnClosed) => {
            ("model_turn_closed", "model turn is closed", false)
        }
        ModelGatewayError::Runtime(ModelRuntimeError::Internal { .. })
        | ModelGatewayError::Internal { .. } => (
            "model_runtime_internal",
            "model runtime internal failure",
            false,
        ),
        ModelGatewayError::Recording(_) => {
            ("model_recording", "canonical model recording failed", false)
        }
    };
    TerminalErrorFields {
        code: code.to_string(),
        message: message.to_string(),
        retryable,
    }
}

fn terminal_fields_for_capability_closure(failure: &CapabilityTurnFailure) -> TerminalErrorFields {
    let (code, message) = match (failure.model().is_some(), failure.tool().is_some()) {
        (true, true) => (
            "capability_turn_closure",
            "model and Tool turn scopes failed to close",
        ),
        (true, false) => ("model_turn_closure", "model turn scope failed to close"),
        (false, true) => ("tool_turn_closure", "Tool turn scope failed to close"),
        (false, false) => unreachable!("CapabilityTurnFailure is validated as non-empty"),
    };
    TerminalErrorFields {
        code: code.to_string(),
        message: message.to_string(),
        retryable: false,
    }
}

fn canonical_partial_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn run_start_hooks(hooks: &[HookEntry], session_id: &SessionId, turn_id: &TurnId) {
    for hook in hooks {
        if catch_unwind(AssertUnwindSafe(|| {
            hook.hook.on_turn_start(session_id, turn_id);
        }))
        .is_err()
        {
            tracing::warn!(
                owner = %hook.owner,
                ordinal = hook.ordinal,
                "turn-start Hook panicked"
            );
        }
    }
}

fn run_finally_hooks(hooks: &[HookEntry], observation: &TurnFinally) {
    for hook in hooks {
        if catch_unwind(AssertUnwindSafe(|| {
            hook.hook.on_turn_finally(observation);
        }))
        .is_err()
        {
            tracing::warn!(
                owner = %hook.owner,
                ordinal = hook.ordinal,
                "turn-finally Hook panicked"
            );
        }
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct AgentRuntimeLifecycle {
    state: Arc<RuntimeState>,
}

impl ServiceLifecycle for AgentRuntimeLifecycle {
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
