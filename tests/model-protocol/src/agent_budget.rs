//! Canonical Agent/Session budget integration; no external services or durable fixtures.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jingwei::agent::*;
use jingwei::budget::*;
use jingwei::id::{SessionId, TaskId, TurnId};
use jingwei::llm::*;
use jingwei::plugin::*;
use jingwei::session::*;
use jingwei_agent_runtime::CanonicalAgentRuntimePlugin;
use jingwei_core::{SessionEvent, SessionEventKind};
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use jingwei_session_runtime::CanonicalSessionRuntimePlugin;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Memory {
    events: Mutex<Vec<SessionEvent>>,
    fail_report: AtomicBool,
    report_attempts: Mutex<Vec<Value>>,
    report_gate: Mutex<Option<Arc<ReportGate>>>,
}

struct ReportGate {
    entered: Semaphore,
    release: Semaphore,
}

fn is_report(event: &SessionEvent) -> bool {
    json!(&event.kind)["type"] == "task_run_report"
}

impl SessionPersistence for Memory {
    fn load<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Vec<SessionEvent>, SessionPersistenceError>> {
        Box::pin(async move {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| &event.session_id == session_id)
                .cloned()
                .collect())
        })
    }

    fn commit_durable<'a>(
        &'a self,
        event: &'a SessionEvent,
    ) -> SessionFuture<'a, Result<PersistAppendOutcome, SessionPersistenceError>> {
        Box::pin(async move {
            if is_report(event) {
                self.report_attempts
                    .lock()
                    .unwrap()
                    .push(json!(&event.kind));
                let gate = self.report_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                }
                if self.fail_report.load(Ordering::SeqCst) {
                    return Err(SessionPersistenceError::Io {
                        operation: "private_task_run_report",
                        message: "injected report append failure".into(),
                        certainty: CommitCertainty::DefinitelyNotCommitted,
                    });
                }
            }
            let mut events = self.events.lock().unwrap();
            let next = events
                .iter()
                .filter(|existing| existing.session_id == event.session_id)
                .count() as u64;
            assert_eq!(
                event.seq, next,
                "fixture preserves canonical physical order"
            );
            events.push(event.clone());
            Ok(PersistAppendOutcome::Appended)
        })
    }
}

struct MemoryPlugin(Arc<Memory>);

impl ServiceFactory<dyn SessionPersistence> for MemoryPlugin {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionPersistence>, RuntimeError>> {
        Box::pin(async {
            Ok(ManagedService::ready(
                self.0.clone() as Arc<dyn SessionPersistence>
            ))
        })
    }
}

impl Plugin for MemoryPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-agent-budget-memory", SESSION_PERSISTENCE, "memory")
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_persistence_factory(Arc::new(Self(self.0.clone())))
    }
}

struct Probe {
    calls: AtomicUsize,
    observations: Mutex<Vec<BudgetReport>>,
    swallowed: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            observations: Mutex::new(Vec::new()),
            swallowed: AtomicUsize::new(0),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

impl Agent for Probe {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let budget = ctx
                .budget()
                .expect("canonical Agent always has a finite scope");
            self.observations
                .lock()
                .unwrap()
                .push(budget.report().unwrap());
            match input.user_message {
                "step" => {
                    budget.consume_step().unwrap();
                    ctx.emit_text_delta("step confirmed").await?;
                }
                "swallow_step" => {
                    if budget.consume_step().is_err() {
                        self.swallowed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                "swallow_second_step" => {
                    budget.consume_step().unwrap();
                    if budget.consume_step().is_err() {
                        self.swallowed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                "swallow_correction" => {
                    budget.consume_correction().unwrap();
                    if budget.consume_correction().is_err() {
                        self.swallowed.fetch_add(1, Ordering::SeqCst);
                    }
                }
                "swallow_model_budget" => {
                    let model = ctx.model().expect("fixture grants model capability");
                    let request = GenerationRequest::text(vec![ModelMessage::user("one call")]);
                    model
                        .generate(&request, GenerationOptions::default())
                        .await
                        .unwrap();
                    let error = model
                        .generate(&request, GenerationOptions::default())
                        .await
                        .unwrap_err();
                    assert!(matches!(
                        error,
                        ModelGatewayError::Runtime(ModelRuntimeError::Budget(_))
                    ));
                    self.swallowed.fetch_add(1, Ordering::SeqCst);
                }
                "hold" => {
                    self.entered.add_permits(1);
                    self.release.acquire().await.unwrap().forget();
                }
                "observe" => {}
                other => panic!("unknown fixture command {other}"),
            }
            Ok(AgentTurnOutput {
                final_text: "agent claims completion".into(),
                outcome: TurnOutcome::Completed,
                artifact: None,
            })
        })
    }
}

struct ProbePlugin(Arc<Probe>, bool);

impl Plugin for ProbePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        let descriptor = PluginDescriptor::new("private-agent-budget-caller");
        if self.1 {
            descriptor.requires_capabilities(&[LLM_RUNTIME])
        } else {
            descriptor
        }
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_agent("probe", self.0.clone())
    }
}

#[derive(Default)]
struct Model(AtomicUsize);

impl Llm for Model {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            text: GenerationSupport {
                complete: CapabilitySupport::Supported,
                stream: CapabilitySupport::Unsupported,
            },
            ..ModelCapabilities::default()
        }
    }

    fn generate<'a>(
        &'a self,
        _: &'a GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(GenerationResponse::text(
                "one confirmed response",
                FinishReason::Stop,
            ))
        })
    }

    fn generate_stream(
        &self,
        _: GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> GenerationStream {
        panic!("fixture exercises direct complete generation")
    }
}

struct ModelPlugin(Arc<Model>);

impl ServiceFactory<dyn Llm> for ModelPlugin {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async { Ok(ManagedService::ready(self.0.clone() as Arc<dyn Llm>)) })
    }
}

impl Plugin for ModelPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-agent-budget-model", LLM_PROVIDER, "fake")
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(Self(self.0.clone())))
    }
}

struct Fixture {
    registry: PluginRegistry,
    runtime: Arc<dyn AgentRuntime>,
    memory: Arc<Memory>,
    probe: Arc<Probe>,
    model: Arc<Model>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_model(false).await
    }

    async fn with_model(model_enabled: bool) -> Self {
        let memory = Arc::new(Memory::default());
        let probe = Arc::new(Probe::default());
        let model = Arc::new(Model::default());
        let mut registrar = Registrar::default();
        registrar.add(MemoryPlugin(memory.clone()));
        registrar.add(ProbePlugin(probe.clone(), model_enabled));
        if model_enabled {
            registrar.add(ModelPlugin(model.clone()));
            registrar.add(CanonicalLlmRuntimePlugin::new());
        }
        registrar.add(CanonicalSessionRuntimePlugin::new());
        registrar.add(CanonicalAgentRuntimePlugin::new());
        registrar.require(AGENT_RUNTIME);
        let registry = registrar.finish().await.unwrap();
        let runtime = registry.agent_runtime().unwrap();
        Self {
            registry,
            runtime,
            memory,
            probe,
            model,
        }
    }

    async fn run(&self, request: AgentTurnRequest) -> Result<AgentTurnReport, AgentRuntimeError> {
        self.runtime.start_turn(request)?.wait().await
    }

    async fn close(self) {
        self.registry.shutdown().await.unwrap();
    }
}

#[derive(Clone, Copy)]
enum SessionFault {
    AdmissionSession,
    ReportSession,
    ReportTurn,
    ReportKind,
    ReportPayload,
}

struct FaultySession {
    fault: SessionFault,
    events: Mutex<Vec<SessionEvent>>,
    returned_report: Mutex<Option<SessionEvent>>,
    settled: AtomicUsize,
}

struct FaultySessionRuntime(Arc<FaultySession>);

impl SessionRuntime for FaultySessionRuntime {
    fn begin_turn<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Box<dyn SessionTurn>, SessionRuntimeError>> {
        Box::pin(async move {
            let admitted_session = if matches!(self.0.fault, SessionFault::AdmissionSession) {
                SessionId::new()
            } else {
                session_id.clone()
            };
            Ok(Box::new(FaultySessionTurn {
                state: self.0.clone(),
                admission: TurnAdmission::new(admitted_session, TurnId::new(), Arc::from([])),
            }) as Box<dyn SessionTurn>)
        })
    }
}

struct FaultySessionTurn {
    state: Arc<FaultySession>,
    admission: TurnAdmission,
}

impl SessionTurn for FaultySessionTurn {
    fn admission(&self) -> &TurnAdmission {
        &self.admission
    }

    fn append<'a>(
        &'a self,
        draft: &'a SessionEventDraft,
    ) -> SessionFuture<'a, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move {
            let mut events = self.state.events.lock().unwrap();
            let mut event = SessionEvent {
                event_id: draft.event_id().clone(),
                session_id: self.admission.session_id().clone(),
                turn_id: self.admission.turn_id().clone(),
                generation_id: draft.generation_id().cloned(),
                message_id: draft.message_id().cloned(),
                seq: events.len() as u64,
                kind: draft.kind().clone(),
            };
            // The attempted event is retained, but this faulty authority returns
            // a different acknowledgment. The caller must preserve both facts.
            events.push(event.clone());
            if is_report(&event) {
                match self.state.fault {
                    SessionFault::ReportSession => event.session_id = SessionId::new(),
                    SessionFault::ReportTurn => event.turn_id = TurnId::new(),
                    SessionFault::ReportKind => {
                        event.kind = SessionEventKind::AssistantDelta {
                            text: "wrong acknowledgment".into(),
                        };
                    }
                    SessionFault::ReportPayload => {
                        let SessionEventKind::TaskRunReport { report } = &mut event.kind else {
                            unreachable!()
                        };
                        report.budget.charged.steps += 1;
                    }
                    SessionFault::AdmissionSession => {}
                }
                *self.state.returned_report.lock().unwrap() = Some(event.clone());
            }
            Ok(Arc::new(event))
        })
    }

    fn settle(
        self: Box<Self>,
    ) -> SessionFuture<'static, Result<TurnCommitSummary, SessionRuntimeError>> {
        Box::pin(async move {
            self.state.settled.fetch_add(1, Ordering::SeqCst);
            Ok(TurnCommitSummary::new(
                self.admission.turn_id().clone(),
                self.state.events.lock().unwrap().clone(),
            ))
        })
    }
}

struct FaultySessionPlugin(Arc<FaultySession>);

impl ServiceFactory<dyn SessionRuntime> for FaultySessionPlugin {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionRuntime>, RuntimeError>> {
        Box::pin(async {
            Ok(ManagedService::ready(
                Arc::new(FaultySessionRuntime(self.0.clone())) as Arc<dyn SessionRuntime>,
            ))
        })
    }
}

impl Plugin for FaultySessionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider(
            "private-agent-budget-faulty-session",
            SESSION_RUNTIME,
            "fake",
        )
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_runtime_factory(Arc::new(Self(self.0.clone())))
    }
}

async fn faulty_session_fixture(
    fault: SessionFault,
) -> (
    PluginRegistry,
    Arc<dyn AgentRuntime>,
    Arc<FaultySession>,
    Arc<Probe>,
) {
    let sessions = Arc::new(FaultySession {
        fault,
        events: Mutex::new(Vec::new()),
        returned_report: Mutex::new(None),
        settled: AtomicUsize::new(0),
    });
    let probe = Arc::new(Probe::default());
    let mut registrar = Registrar::default();
    registrar.add(FaultySessionPlugin(sessions.clone()));
    registrar.add(ProbePlugin(probe.clone(), false));
    registrar.add(CanonicalAgentRuntimePlugin::new());
    registrar.require(AGENT_RUNTIME);
    let registry = registrar.finish().await.unwrap();
    let runtime = registry.agent_runtime().unwrap();
    (registry, runtime, sessions, probe)
}

#[tokio::test]
async fn mismatched_session_admission_never_binds_budget_or_enters_agent() {
    let (registry, runtime, sessions, probe) =
        faulty_session_fixture(SessionFault::AdmissionSession).await;
    let session = SessionId::new();
    let budget = task(&session, limits(2));
    let error = runtime
        .start_turn(
            AgentTurnRequest::new(session.clone(), "probe", "observe")
                .with_budget(budget.clone(), limits(2)),
        )
        .unwrap()
        .wait()
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("acquired wrong lease still requires explicit closure");
    };
    assert_eq!(failure.disposition(), TurnDisposition::Failed);
    let TaskRunReportAttempt::Rejected { report, error } =
        failure.task_run_report_attempt().unwrap()
    else {
        panic!("wrong Session authority cannot commit this Task's report");
    };
    assert_eq!(*error, BudgetError::IdentityMismatch);
    assert_eq!(report.budget.identity.session_id, session);
    assert!(report.budget.run.as_ref().unwrap().turn_id.is_none());
    assert_eq!(
        report.stop,
        TaskRunStop::Budget(BudgetStopReason::IdentityMismatch)
    );
    assert_eq!(report.budget.charged, BudgetAmounts::default());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(sessions.settled.load(Ordering::SeqCst), 1);
    assert!(sessions.returned_report.lock().unwrap().is_none());
    {
        let events = sessions.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, SessionEventKind::Error { .. }));
        assert_ne!(events[0].session_id, session);
    }
    assert!(budget.report().unwrap().run.is_none());
    assert!(
        failure
            .report()
            .is_none_or(|report| report.task_run_report().is_none())
    );
    registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn mismatched_report_acknowledgment_retains_invalid_evidence_and_cannot_confirm_success() {
    for fault in [
        SessionFault::ReportSession,
        SessionFault::ReportTurn,
        SessionFault::ReportKind,
        SessionFault::ReportPayload,
    ] {
        let (registry, runtime, sessions, probe) = faulty_session_fixture(fault).await;
        let session = SessionId::new();
        let budget = task(&session, limits(2));
        let error = runtime
            .start_turn(
                AgentTurnRequest::new(session.clone(), "probe", "observe")
                    .with_budget(budget.clone(), limits(2)),
            )
            .unwrap()
            .wait()
            .await
            .unwrap_err();
        let AgentRuntimeError::Turn(failure) = error else {
            panic!("invalid acknowledgment needs structured failure");
        };
        assert_eq!(failure.disposition(), TurnDisposition::Failed);
        let TaskRunReportAttempt::Invalid { report, event } =
            failure.task_run_report_attempt().unwrap()
        else {
            panic!("returned abnormal event must remain inspectable");
        };
        assert_eq!(report.budget.identity.session_id, session);
        assert_eq!(
            event.as_ref(),
            sessions.returned_report.lock().unwrap().as_ref().unwrap()
        );
        assert!(failure.report().unwrap().task_run_report().is_none());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(sessions.settled.load(Ordering::SeqCst), 1);
        {
            let events = sessions.events.lock().unwrap();
            assert_report_before_terminal(&events);
            assert!(matches!(
                events.last().unwrap().kind,
                SessionEventKind::Error { .. }
            ));
            assert!(events.iter().all(|event| event.session_id == session));
        }
        assert!(budget.report().unwrap().run.is_none());
        registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn direct_model_gateway_charges_shared_task_and_swallowed_budget_error_cannot_complete() {
    let fixture = Fixture::with_model(true).await;
    let session = SessionId::new();
    let mut one_call = limits(4);
    one_call.resources.model_requests = 1;
    let budget = task(&session, one_call);
    let error = fixture
        .run(
            AgentTurnRequest::new(session, "probe", "swallow_model_budget")
                .with_budget(budget.clone(), one_call),
        )
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("expected admitted Turn failure");
    };
    let TaskRunReportAttempt::Committed { report, .. } = failure.task_run_report_attempt().unwrap()
    else {
        panic!("stopped direct gateway needs confirmed report");
    };
    assert_eq!(fixture.model.0.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.swallowed.load(Ordering::SeqCst), 1);
    assert_eq!(
        report.stop,
        TaskRunStop::Budget(BudgetStopReason::ResourceLimit(
            BudgetResource::ModelRequests
        ))
    );
    assert_eq!(report.budget.charged.model_requests, 1);
    assert_eq!(report.budget.charged.steps, 1);
    let run = report.budget.run.as_ref().unwrap();
    assert_eq!(run.metrics.confirmed.model_requests, 1);
    assert_eq!(run.metrics.confirmed.model_results, 1);
    assert!(report.budget.pending.is_empty());
    assert!(report.capabilities_drained);
    let events = fixture.memory.events.lock().unwrap().clone();
    assert_report_before_terminal(&events);
    assert!(matches!(
        events.last().unwrap().kind,
        SessionEventKind::Error { .. }
    ));
    fixture.close().await;
}

fn limits(steps: u64) -> BudgetLimits {
    BudgetLimits {
        resources: BudgetAmounts {
            steps,
            model_requests: 4,
            tool_calls: 4,
            corrections: 1,
            input_tokens: 100_000,
            output_tokens: 100_000,
            tool_output_bytes: 100_000,
        },
        active_time: Duration::from_secs(60),
    }
}

fn task(session_id: &SessionId, task_limits: BudgetLimits) -> TaskBudget {
    TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session_id.clone(),
            agent_key: "probe".into(),
        },
        task_limits,
        task_limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap()
}

fn assert_report_before_terminal(events: &[SessionEvent]) {
    let report_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| is_report(event).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(report_positions, [events.len() - 2]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
            ))
            .count(),
        1
    );
    assert!(matches!(
        events.last().unwrap().kind,
        SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
    ));
}

#[tokio::test]
async fn implicit_task_is_finite_and_runtime_records_one_report_before_terminal() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let report = fixture
        .run(AgentTurnRequest::new(session.clone(), "probe", "observe"))
        .await
        .unwrap();
    assert_eq!(report.disposition(), TurnDisposition::Completed);
    assert_report_before_terminal(report.events());
    let task_run = report
        .task_run_report()
        .expect("confirmed report is exposed");
    assert_eq!(task_run.stop, TaskRunStop::Completed);
    assert!(task_run.capabilities_drained);
    let run = task_run.budget.run.as_ref().unwrap();
    assert_eq!(run.turn_id.as_ref(), Some(report.turn_id()));
    assert!(!run.open);
    assert!(task_run.budget.pending.is_empty());
    let durable = report
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            SessionEventKind::TaskRunReport { report } => Some(report.as_ref()),
            _ => None,
        })
        .unwrap();
    assert_eq!(task_run, durable);
    let observations = fixture.probe.observations.lock().unwrap().clone();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].identity.session_id, session);
    assert_eq!(observations[0].identity.agent_key, "probe");
    assert!(observations[0].limits.active_time > Duration::ZERO);
    assert!(observations[0].limits.active_time < Duration::MAX);
    for resource in BudgetResource::ALL {
        assert!(observations[0].limits.resources.get(resource) < u64::MAX);
    }
    fixture.close().await;
}

#[tokio::test]
async fn same_task_keeps_consumed_steps_and_catching_budget_error_cannot_complete() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let budget = task(&session, limits(2));
    for _ in 0..2 {
        let report = fixture
            .run(
                AgentTurnRequest::new(session.clone(), "probe", "step")
                    .with_budget(budget.clone(), limits(2)),
            )
            .await
            .unwrap();
        assert_eq!(report.disposition(), TurnDisposition::Completed);
        assert_report_before_terminal(report.events());
    }
    let failure = fixture
        .run(
            AgentTurnRequest::new(session, "probe", "swallow_step")
                .with_budget(budget.clone(), limits(2)),
        )
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = failure else {
        panic!("accepted exhausted task retains structured Turn failure");
    };
    assert_eq!(failure.disposition(), TurnDisposition::Failed);
    let TaskRunReportAttempt::Committed { report, event } =
        failure.task_run_report_attempt().unwrap()
    else {
        panic!("exhaustion report is durably confirmed");
    };
    assert!(is_report(event));
    assert_eq!(
        report.stop,
        TaskRunStop::Budget(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    assert_eq!(fixture.probe.swallowed.load(Ordering::SeqCst), 1);
    let observed = budget.report().unwrap();
    assert_eq!(observed.charged.steps, 2);
    assert_eq!(
        observed.stop,
        Some(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    assert!(observed.run.is_none());
    let events = fixture.memory.events.lock().unwrap().clone();
    assert!(matches!(
        events.last().unwrap().kind,
        SessionEventKind::Error { .. }
    ));
    fixture.close().await;
}

#[tokio::test]
async fn run_limit_stop_is_not_lost_when_task_still_has_remaining_steps() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let budget = task(&session, limits(2));
    let error = fixture
        .run(
            AgentTurnRequest::new(session.clone(), "probe", "swallow_second_step")
                .with_budget(budget.clone(), limits(1)),
        )
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("expected accepted Turn failure")
    };
    let TaskRunReportAttempt::Committed { report, .. } = failure.task_run_report_attempt().unwrap()
    else {
        panic!("run stop must have confirmed evidence");
    };
    assert!(
        report.budget.stop.is_none(),
        "task itself has remaining quota"
    );
    assert_eq!(
        report.budget.run.as_ref().unwrap().stop,
        Some(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    assert_eq!(
        report.stop,
        TaskRunStop::Budget(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    let next = fixture
        .run(AgentTurnRequest::new(session, "probe", "step").with_budget(budget.clone(), limits(1)))
        .await
        .unwrap();
    assert_eq!(next.disposition(), TurnDisposition::Completed);
    assert_eq!(budget.report().unwrap().charged.steps, 2);
    fixture.close().await;
}

#[tokio::test]
async fn correction_budget_is_charged_and_swallowed_exhaustion_cannot_complete() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let budget = task(&session, limits(2));
    assert!(
        fixture
            .run(
                AgentTurnRequest::new(session, "probe", "swallow_correction")
                    .with_budget(budget.clone(), limits(2))
            )
            .await
            .is_err()
    );
    let report = budget.report().unwrap();
    assert_eq!(report.charged.corrections, 1);
    assert_eq!(fixture.probe.swallowed.load(Ordering::SeqCst), 1);
    assert_eq!(
        report.stop,
        Some(BudgetStopReason::ResourceLimit(BudgetResource::Corrections))
    );
    fixture.close().await;
}

#[tokio::test]
async fn explicit_identity_mismatch_never_runs_agent_or_writes_a_user_message() {
    for wrong_agent in [false, true] {
        let fixture = Fixture::new().await;
        let session = SessionId::new();
        let budget = TaskBudget::new(
            BudgetIdentity {
                task_id: TaskId::new(),
                session_id: if wrong_agent {
                    session.clone()
                } else {
                    SessionId::new()
                },
                agent_key: if wrong_agent { "other-agent" } else { "probe" }.into(),
            },
            limits(2),
            limits(2),
            TokenBudgetMode::Soft,
            Arc::new(MonotonicBudgetClock::default()),
        )
        .unwrap();
        assert!(
            fixture
                .run(
                    AgentTurnRequest::new(session, "probe", "observe")
                        .with_budget(budget, limits(2))
                )
                .await
                .is_err()
        );
        assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 0);
        assert!(fixture.memory.events.lock().unwrap().is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn report_append_failure_cannot_be_returned_as_successful_agent_completion() {
    let fixture = Fixture::new().await;
    fixture.memory.fail_report.store(true, Ordering::SeqCst);
    let session = SessionId::new();
    let failure = fixture
        .run(AgentTurnRequest::new(session, "probe", "observe"))
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = failure else {
        panic!("expected structured Turn failure")
    };
    let TaskRunReportAttempt::Failed { report, source } =
        failure.task_run_report_attempt().unwrap()
    else {
        panic!("report append failure must retain its attempted payload");
    };
    assert!(!report.budget.run.as_ref().unwrap().open);
    assert!(report.capabilities_drained);
    assert!(failure.report().unwrap().task_run_report().is_none());
    assert!(matches!(
        source,
        SessionRuntimeError::Persistence(SessionPersistenceError::Io {
            operation: "private_task_run_report",
            certainty: CommitCertainty::DefinitelyNotCommitted,
            ..
        })
    ));
    assert_eq!(fixture.memory.report_attempts.lock().unwrap().len(), 1);
    let events = fixture.memory.events.lock().unwrap().clone();
    assert!(!events.iter().any(is_report));
    assert!(matches!(
        events.last().unwrap().kind,
        SessionEventKind::Error { .. }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
            ))
            .count(),
        1
    );
    fixture.close().await;
}

#[tokio::test]
async fn report_persistence_retains_task_lease_until_terminal_and_settlement() {
    let fixture = Fixture::new().await;
    let gate = Arc::new(ReportGate {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    *fixture.memory.report_gate.lock().unwrap() = Some(gate.clone());
    let session = SessionId::new();
    let budget = task(&session, limits(2));
    let controller = fixture
        .runtime
        .start_turn(
            AgentTurnRequest::new(session, "probe", "observe")
                .with_budget(budget.clone(), limits(2)),
        )
        .unwrap();
    gate.entered.acquire().await.unwrap().forget();

    let in_flight = budget.report().unwrap();
    assert!(!in_flight.run.as_ref().unwrap().open);
    assert!(matches!(
        budget.begin_run(TurnId::new(), limits(2)),
        Err(BudgetError::RunActive)
    ));
    assert!(
        fixture
            .memory
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(
                event.kind,
                SessionEventKind::TaskRunReport { .. }
                    | SessionEventKind::Done { .. }
                    | SessionEventKind::Error { .. }
            ))
    );

    gate.release.add_permits(1);
    let completed = controller.wait().await.unwrap();
    assert!(completed.task_run_report().is_some());
    assert!(budget.report().unwrap().run.is_none());
    let mut next = budget.begin_run(TurnId::new(), limits(2)).unwrap();
    next.finish().unwrap();
    fixture.close().await;
}

struct TokioClock(tokio::time::Instant);

impl BudgetClock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[tokio::test(start_paused = true)]
async fn session_admission_wait_exhausts_task_time_without_a_fabricated_turn() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let first = fixture
        .runtime
        .start_turn(AgentTurnRequest::new(session.clone(), "probe", "hold"))
        .unwrap();
    fixture.probe.entered.acquire().await.unwrap().forget();
    let mut short = limits(2);
    short.active_time = Duration::from_secs(1);
    let budget = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session.clone(),
            agent_key: "probe".into(),
        },
        short,
        short,
        TokenBudgetMode::Soft,
        Arc::new(TokioClock(tokio::time::Instant::now())),
    )
    .unwrap();
    let waiting = fixture
        .runtime
        .start_turn(
            AgentTurnRequest::new(session, "probe", "observe").with_budget(budget.clone(), short),
        )
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(waiting.budget_report().is_some());
    let error = waiting.wait().await.unwrap_err();
    let AgentRuntimeError::NotAdmitted(NotAdmittedFailure::Budget { report, .. }) = error else {
        panic!("Session admission deadline must retain unadmitted budget evidence");
    };
    assert!(report.run.as_ref().unwrap().turn_id.is_none());
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 1);
    let report = budget.report().unwrap();
    assert_eq!(report.active_time, Duration::from_secs(1));
    assert_eq!(report.charged.steps, 0);
    assert_eq!(report.stop, Some(BudgetStopReason::ActiveTime));
    let messages = fixture
        .memory
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event.kind, SessionEventKind::UserMessage { .. }))
        .count();
    assert_eq!(
        messages, 1,
        "nonadmitted request must not fabricate Session events"
    );
    fixture.probe.release.add_permits(1);
    first.wait().await.unwrap();
    fixture.close().await;
}

#[tokio::test(start_paused = true)]
async fn active_deadline_stops_an_agent_that_never_calls_a_gateway() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let mut short = limits(2);
    short.active_time = Duration::from_secs(1);
    let budget = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session.clone(),
            agent_key: "probe".into(),
        },
        short,
        short,
        TokenBudgetMode::Soft,
        Arc::new(TokioClock(tokio::time::Instant::now())),
    )
    .unwrap();
    let controller = fixture
        .runtime
        .start_turn(AgentTurnRequest::new(session, "probe", "hold").with_budget(budget, short))
        .unwrap();
    fixture.probe.entered.acquire().await.unwrap().forget();
    let observation = controller.budget_report().unwrap().unwrap();
    assert!(observation.run.as_ref().unwrap().turn_id.is_some());
    tokio::time::advance(Duration::from_secs(2)).await;
    let error = controller.wait().await.unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("expected budget-stopped Turn")
    };
    let TaskRunReportAttempt::Committed { report, .. } = failure.task_run_report_attempt().unwrap()
    else {
        panic!("deadline must preserve a confirmed budget report");
    };
    assert_eq!(
        report.stop,
        TaskRunStop::Budget(BudgetStopReason::ActiveTime)
    );
    assert_eq!(report.budget.active_time, Duration::from_secs(1));
    assert_eq!(report.budget.charged.model_requests, 0);
    assert_eq!(report.budget.charged.tool_calls, 0);
    fixture.close().await;
}

#[tokio::test]
async fn task_run_report_wire_requires_explicit_known_version() {
    let fixture = Fixture::new().await;
    let report = fixture
        .run(AgentTurnRequest::new(SessionId::new(), "probe", "observe"))
        .await
        .unwrap();
    let run = report.task_run_report().unwrap();
    let wire = json!(run);
    assert_eq!(wire["version"], 1);
    assert_eq!(
        serde_json::from_value::<TaskRunReport>(wire.clone()).unwrap(),
        *run
    );
    for bad_version in [None, Some(json!(0)), Some(json!(2)), Some(json!("1"))] {
        let mut bad = wire.clone();
        match bad_version {
            None => {
                bad.as_object_mut().unwrap().remove("version");
            }
            Some(version) => {
                bad["version"] = version;
            }
        }
        assert!(serde_json::from_value::<TaskRunReport>(bad).is_err());
    }
    fixture.close().await;
}
