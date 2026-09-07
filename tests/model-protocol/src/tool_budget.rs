//! Private integration checks for shared Tool budget authority and durable cleanup.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jingwei::agent::*;
use jingwei::budget::*;
use jingwei::id::*;
use jingwei::plugin::*;
use jingwei::session::SessionRuntimeError;
use jingwei::tool::*;
use jingwei_core::{CancellationFuture, CancellationSignal, SessionEvent, SessionEventKind};
use jingwei_tool_runtime::CanonicalToolRuntimePlugin;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

struct Clock(tokio::time::Instant);
impl BudgetClock for Clock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

struct Signal(CancellationToken);
impl CancellationSignal for Signal {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    fn cancelled(&self) -> CancellationFuture<'_> {
        Box::pin(self.0.cancelled())
    }
}

#[derive(Clone, Copy, Default)]
enum Body {
    #[default]
    Success,
    Error,
    Pending,
}

struct Probe {
    body: Body,
    output: String,
    approval: bool,
    calls: AtomicUsize,
}
impl Tool for Probe {
    fn metadata(&self) -> ToolMetadata {
        let metadata =
            ToolMetadata::new("budget probe", json!({"type":"object", "required":["ok"]}));
        if self.approval {
            metadata.with_approval(ApprovalRequirement::required("human"))
        } else {
            metadata
        }
    }
    fn execute<'a>(
        &'a self,
        _: ToolBodyRequest<'a>,
        _: Arc<dyn CancellationSignal>,
    ) -> ToolFuture<'a, Result<String, ToolBodyError>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.body {
                Body::Success => Ok(self.output.clone()),
                Body::Error => Err(ToolBodyError::new("failed", "unknown output", false)),
                Body::Pending => std::future::pending().await,
            }
        })
    }
}

struct Deny;
impl ToolGuard for Deny {
    fn evaluate<'a>(
        &'a self,
        _: ToolGuardRequest<'a>,
    ) -> ToolFuture<'a, Result<Option<ToolDenial>, ToolGuardError>> {
        Box::pin(async { Ok(Some(ToolDenial::new("denied", "policy", false))) })
    }
}
struct Approval(Arc<AtomicUsize>);
impl ToolAuthorizer for Approval {
    fn authorize<'a>(
        &'a self,
        _: ToolAuthorizationRequest<'a>,
    ) -> ToolFuture<'a, Result<ToolAuthorizationDecision, ToolAuthorizationError>> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolAuthorizationDecision::Denied(ToolDenial::new(
                "denied", "approval", false,
            )))
        })
    }
}
struct ToolsPlugin {
    probe: Arc<Probe>,
    guard: bool,
    approvals: Arc<AtomicUsize>,
}
impl Plugin for ToolsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("private-budget-tools")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_tool("probe", self.probe.clone())?;
        ctx.register_tool_authorizer("human", Arc::new(Approval(self.approvals.clone())))?;
        if self.guard {
            ctx.register_tool_guard(Arc::new(Deny));
        }
        Ok(())
    }
}

type Slot = Arc<Mutex<Option<Arc<dyn ToolRuntime>>>>;
struct Capture(Slot);
struct Idle;
impl AgentRuntime for Idle {
    fn start_turn(
        &self,
        _: AgentTurnRequest,
    ) -> Result<Box<dyn AgentTurnController>, AgentRuntimeError> {
        Err(AgentRuntimeError::Stopped)
    }
}
impl ServiceFactory<dyn AgentRuntime> for Capture {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn AgentRuntime>, RuntimeError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = ctx.tool_runtime();
            Ok(ManagedService::ready(
                Arc::new(Idle) as Arc<dyn AgentRuntime>
            ))
        })
    }
}
impl Plugin for Capture {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-budget-capture", AGENT_RUNTIME, "capture")
            .requires_capabilities(&[TOOL_RUNTIME])
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_agent_runtime_factory(Arc::new(Self(self.0.clone())))
    }
}

struct Recorder {
    fail: Option<ToolRecordStage>,
    mismatch: Option<ToolRecordStage>,
    calls: AtomicUsize,
    results: AtomicUsize,
    call_gate: Semaphore,
    result_gate: Semaphore,
    events: Mutex<Vec<Arc<SessionEvent>>>,
}
impl ToolEventRecorder for Recorder {
    fn append(
        &self,
        record: ToolRecord,
    ) -> ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move {
            let stage = match &record {
                ToolRecord::Call(_) => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    self.call_gate.acquire().await.unwrap().forget();
                    ToolRecordStage::Call
                }
                ToolRecord::Result(_) => {
                    self.results.fetch_add(1, Ordering::SeqCst);
                    self.result_gate.acquire().await.unwrap().forget();
                    ToolRecordStage::Result
                }
            };
            if self.fail == Some(stage) {
                return Err(SessionRuntimeError::Stopped);
            }
            let mut events = self.events.lock().unwrap();
            let event = Arc::new(SessionEvent {
                event_id: EventId::new(),
                session_id: SessionId::from(if self.mismatch == Some(stage) {
                    "wrong-session"
                } else {
                    "tool-budget-session"
                }),
                turn_id: TurnId::from("tool-budget-turn"),
                generation_id: None,
                message_id: None,
                seq: events.len() as u64,
                kind: record.into_session_event_kind(),
            });
            events.push(event.clone());
            Ok(event)
        })
    }
}

struct Setup {
    body: Body,
    output: String,
    grant: bool,
    guard: bool,
    approval: bool,
    fail: Option<ToolRecordStage>,
    mismatch: Option<ToolRecordStage>,
    block_call: bool,
    block_result: bool,
    limits: BudgetLimits,
    output_limit: usize,
    timeout: Duration,
}
impl Default for Setup {
    fn default() -> Self {
        Self {
            body: Body::Success,
            output: "你a".into(),
            grant: true,
            guard: false,
            approval: false,
            fail: None,
            mismatch: None,
            block_call: false,
            block_result: false,
            limits: BudgetLimits::default(),
            output_limit: 8,
            timeout: Duration::from_secs(30),
        }
    }
}
struct Fixture {
    registry: PluginRegistry,
    runtime: Arc<dyn ToolRuntime>,
    turn: Option<Box<dyn ToolTurn>>,
    task: TaskBudget,
    run: BudgetRun,
    probe: Arc<Probe>,
    recorder: Arc<Recorder>,
    signal: Arc<Signal>,
    approvals: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new(setup: Setup) -> Self {
        let probe = Arc::new(Probe {
            body: setup.body,
            output: setup.output,
            approval: setup.approval,
            calls: AtomicUsize::new(0),
        });
        let approvals = Arc::new(AtomicUsize::new(0));
        let recorder = Arc::new(Recorder {
            fail: setup.fail,
            mismatch: setup.mismatch,
            calls: AtomicUsize::new(0),
            results: AtomicUsize::new(0),
            events: Mutex::new(vec![]),
            call_gate: Semaphore::new(if setup.block_call { 0 } else { 1000 }),
            result_gate: Semaphore::new(if setup.block_result { 0 } else { 1000 }),
        });
        let slot: Slot = Arc::new(Mutex::new(None));
        let mut registrar = Registrar::default();
        registrar.add(ToolsPlugin {
            probe: probe.clone(),
            guard: setup.guard,
            approvals: approvals.clone(),
        });
        let mut runtime = CanonicalToolRuntimePlugin::new()
            .with_max_output_bytes(setup.output_limit)
            .with_default_timeout(setup.timeout);
        if setup.grant {
            runtime = runtime.grant_tool(PluginId::new("caller-owner"), "probe");
        }
        registrar.add(runtime);
        registrar.add(Capture(slot.clone()));
        registrar.select(TOOL_RUNTIME, "canonical");
        registrar.select(AGENT_RUNTIME, "capture");
        let registry = registrar.finish().await.unwrap();
        let runtime = slot.lock().unwrap().clone().unwrap();
        let task = TaskBudget::new(
            BudgetIdentity {
                task_id: TaskId::from("tool-budget-task"),
                session_id: SessionId::from("tool-budget-session"),
                agent_key: "agent-key-distinct-from-owner".into(),
            },
            setup.limits,
            setup.limits,
            TokenBudgetMode::Hard,
            Arc::new(Clock(tokio::time::Instant::now())),
        )
        .unwrap();
        let run = task
            .begin_run(TurnId::from("tool-budget-turn"), setup.limits)
            .unwrap();
        let signal = Arc::new(Signal(CancellationToken::new()));
        let turn = runtime
            .bind_turn(
                ToolTurnBinding::new(
                    ToolCaller::new("caller-owner"),
                    signal.clone(),
                    recorder.clone(),
                )
                .with_budget(run.scope()),
            )
            .unwrap();
        Self {
            registry,
            runtime,
            turn: Some(turn),
            task,
            run,
            probe,
            recorder,
            signal,
            approvals,
        }
    }
    fn call(&self) -> ToolFuture<'static, Result<ToolExecution, ToolRuntimeError>> {
        self.turn
            .as_ref()
            .unwrap()
            .gateway()
            .call("probe", json!({"ok":true}))
    }
    fn report(&self) -> BudgetReport {
        self.task.report().unwrap()
    }
    async fn finish(mut self) {
        let result = self
            .turn
            .take()
            .unwrap()
            .finish(ToolFinishMode::Graceful)
            .await;
        if self.recorder.fail.is_some() || self.recorder.mismatch.is_some() {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
        self.run.finish().unwrap();
        self.registry.shutdown().await.unwrap();
    }
}
async fn reached(counter: &AtomicUsize, target: usize) {
    // No wall-clock polling: yield to the owned driver until its explicit phase latch advances.
    for _ in 0..2000 {
        if counter.load(Ordering::SeqCst) >= target {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("driver did not reach phase {target}");
}
fn category(execution: &ToolExecution) -> ToolFailureCategory {
    match &execution.result().outcome {
        ToolRecordedOutcome::Failed { category, .. } => *category,
        outcome => panic!("expected failure, got {outcome:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn exact_utf8_output_and_tightened_reservation_share_one_task_across_bindings() {
    let mut limits = BudgetLimits::default();
    limits.resources.tool_calls = 2;
    let fixture = Fixture::new(Setup {
        limits,
        ..Default::default()
    })
    .await;
    fixture.call().await.unwrap();
    let second = fixture
        .runtime
        .bind_turn(
            ToolTurnBinding::new(
                ToolCaller::new("caller-owner"),
                fixture.signal.clone(),
                fixture.recorder.clone(),
            )
            .with_budget(fixture.run.scope()),
        )
        .unwrap();
    second
        .gateway()
        .call_with_options(
            "probe",
            json!({"ok":true}),
            ToolCallOptions {
                max_output_bytes: Some(4),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let report = fixture.report();
    assert_eq!(report.charged.tool_calls, 2);
    assert_eq!(report.charged.steps, 0);
    assert_eq!(report.charged.tool_output_bytes, 8);
    assert_eq!(report.usage.tool_output_bytes.actual, 8);
    assert_eq!(report.reserved.tool_output_bytes, 0);
    assert!(report.pending.is_empty());
    assert!(matches!(
        fixture.call().await,
        Err(ToolRuntimeError::Budget(_))
    ));
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.recorder.calls.load(Ordering::SeqCst), 2);
    second.finish(ToolFinishMode::Graceful).await.unwrap();
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn grant_schema_guard_and_approval_denials_consume_attempt_but_refund_output() {
    for mode in 0..4 {
        let fixture = Fixture::new(Setup {
            grant: mode != 0,
            guard: mode == 2,
            approval: mode == 3,
            ..Default::default()
        })
        .await;
        let arguments: Value = if mode == 1 {
            json!({})
        } else {
            json!({"ok":true})
        };
        let result = fixture
            .turn
            .as_ref()
            .unwrap()
            .gateway()
            .call("probe", arguments)
            .await
            .unwrap();
        assert_eq!(
            category(&result),
            [
                ToolFailureCategory::Unavailable,
                ToolFailureCategory::InvalidArguments,
                ToolFailureCategory::Denied,
                ToolFailureCategory::ApprovalDenied
            ][mode]
        );
        assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture.approvals.load(Ordering::SeqCst),
            usize::from(mode == 3)
        );
        let report = fixture.report();
        assert_eq!(report.charged.tool_calls, 1);
        assert_eq!(report.charged.tool_output_bytes, 0);
        assert_eq!(report.reserved.tool_output_bytes, 0);
        assert_eq!(report.usage.tool_output_bytes.unknown, 0);
        assert!(report.pending.is_empty());
        fixture.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn output_overrun_records_unclamped_actual_and_stops_further_admission() {
    let fixture = Fixture::new(Setup {
        output_limit: 3,
        ..Default::default()
    })
    .await;
    assert!(matches!(
        fixture.call().await,
        Err(ToolRuntimeError::Budget(BudgetError::Stopped(
            BudgetStopReason::UsageExceededReservation(BudgetResource::ToolOutputBytes)
        )))
    ));
    let report = fixture.report();
    assert_eq!(report.charged.tool_output_bytes, 4);
    assert_eq!(report.usage.tool_output_bytes.actual, 4);
    assert_eq!(report.reserved.tool_output_bytes, 0);
    assert!(report.pending.is_empty());
    assert!(matches!(
        fixture.call().await,
        Err(ToolRuntimeError::Budget(_))
    ));
    {
        let events = fixture.recorder.events.lock().unwrap();
        assert!(
            matches!(&events[1].kind, SessionEventKind::ToolResult { result } if matches!(result.outcome, ToolRecordedOutcome::Failed { category: ToolFailureCategory::Budget, .. }))
        );
    }
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn started_error_timeout_and_cancellation_keep_unknown_output_reservation() {
    for mode in 0..3 {
        let fixture = Fixture::new(Setup {
            body: if mode == 0 {
                Body::Error
            } else {
                Body::Pending
            },
            timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .await;
        let call = fixture.call();
        reached(&fixture.probe.calls, 1).await;
        if mode == 1 {
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        if mode == 2 {
            fixture.signal.0.cancel();
        }
        let result = call.await;
        if mode == 2 {
            assert!(matches!(result, Err(ToolRuntimeError::Cancelled { .. })));
        } else {
            assert_eq!(
                category(&result.unwrap()),
                if mode == 0 {
                    ToolFailureCategory::BodyFailure
                } else {
                    ToolFailureCategory::Timeout
                }
            );
        }
        let report = fixture.report();
        assert_eq!(report.charged.tool_output_bytes, 8);
        assert_eq!(report.usage.tool_output_bytes.unknown, 1);
        assert_eq!(report.usage.tool_output_bytes.actual, 0);
        assert!(report.pending.is_empty());
        if mode == 1 {
            assert_eq!(
                report.run.as_ref().unwrap().metrics.tool_execution,
                Duration::from_secs(2)
            );
        }
        fixture.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_shared_budget_stop_interrupts_an_already_started_tool() {
    let fixture = Fixture::new(Setup {
        body: Body::Pending,
        ..Default::default()
    })
    .await;
    let call = fixture.call();
    reached(&fixture.probe.calls, 1).await;
    fixture
        .run
        .scope()
        .stop_with(BudgetStopReason::ResourceLimit(
            BudgetResource::ModelRequests,
        ))
        .unwrap();
    assert!(matches!(call.await, Err(ToolRuntimeError::Budget(_))));
    let report = fixture.report();
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.tool_output_bytes, 8);
    assert!(report.pending.is_empty());
    assert_eq!(
        report.run.unwrap().stop,
        Some(BudgetStopReason::ResourceLimit(
            BudgetResource::ModelRequests
        ))
    );
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn call_record_wait_consumes_budget_deadline_and_cannot_be_discarded() {
    let limits = BudgetLimits {
        active_time: Duration::from_secs(2),
        ..Default::default()
    };
    let fixture = Fixture::new(Setup {
        block_call: true,
        limits,
        ..Default::default()
    })
    .await;
    let call = tokio::spawn(fixture.call());
    reached(&fixture.recorder.calls, 1).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(!call.is_finished());
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 0);
    fixture.recorder.call_gate.add_permits(1);
    assert!(matches!(
        call.await.unwrap(),
        Err(ToolRuntimeError::Budget(_))
    ));
    let report = fixture.report();
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.tool_output_bytes, 0);
    assert_eq!(report.active_time, Duration::from_secs(2));
    assert_eq!(report.cleanup_time, Duration::from_secs(1));
    assert_eq!(fixture.recorder.results.load(Ordering::SeqCst), 1);
    assert!(report.pending.is_empty());
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn result_record_wait_survives_deadline_and_finish_drains_settled_work() {
    let mut fixture = Fixture::new(Setup {
        block_result: true,
        timeout: Duration::from_secs(1),
        ..Default::default()
    })
    .await;
    let call = tokio::spawn(fixture.call());
    reached(&fixture.recorder.results, 1).await;
    assert_eq!(fixture.report().charged.tool_output_bytes, 4);
    assert!(fixture.report().pending.is_empty());
    let finish = tokio::spawn(fixture.turn.take().unwrap().finish(ToolFinishMode::Cancel));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!call.is_finished());
    assert!(!finish.is_finished());
    fixture.recorder.result_gate.add_permits(1);
    call.await.unwrap().unwrap();
    finish.await.unwrap().unwrap();
    fixture.run.finish().unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn recording_failures_retain_typed_cause_and_settle_known_or_unstarted_usage() {
    for stage in [ToolRecordStage::Call, ToolRecordStage::Result] {
        let fixture = Fixture::new(Setup {
            fail: Some(stage),
            ..Default::default()
        })
        .await;
        let ToolRuntimeError::Recording(failure) = fixture.call().await.unwrap_err() else {
            panic!("recording failure expected")
        };
        assert_eq!(failure.stage(), stage);
        assert!(matches!(
            failure.source_error(),
            Some(SessionRuntimeError::Stopped)
        ));
        let report = fixture.report();
        assert_eq!(report.charged.tool_calls, 1);
        assert_eq!(
            report.charged.tool_output_bytes,
            if stage == ToolRecordStage::Call { 0 } else { 4 }
        );
        assert!(report.pending.is_empty());
        let metrics = report.run.unwrap().metrics;
        assert_eq!(
            metrics.unconfirmed.tool_calls,
            u64::from(stage == ToolRecordStage::Call)
        );
        assert_eq!(
            metrics.unconfirmed.tool_results,
            u64::from(stage == ToolRecordStage::Result)
        );
        assert_eq!(
            metrics.confirmed.tool_calls,
            u64::from(stage == ToolRecordStage::Result)
        );
        fixture.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn metadata_mismatch_stops_the_bound_budget_before_admission() {
    let fixture = Fixture::new(Setup::default()).await;
    let options = ToolCallOptions {
        action: Some(jingwei_core::ActionContext {
            decision: jingwei_core::DecisionContext {
                task_id: TaskId::new(),
                step_id: StepId::new(),
            },
            action_index: 0,
            provider_call_id: None,
        }),
        ..Default::default()
    };
    assert!(matches!(
        fixture
            .turn
            .as_ref()
            .unwrap()
            .gateway()
            .call_with_options("probe", json!({"ok":true}), options)
            .await,
        Err(ToolRuntimeError::Budget(BudgetError::IdentityMismatch))
    ));
    assert_eq!(fixture.report().charged.tool_calls, 0);
    assert_eq!(fixture.recorder.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.report().run.unwrap().stop,
        Some(BudgetStopReason::IdentityMismatch)
    );
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn wrong_confirmed_event_identity_is_retained_and_freezes_shared_authority() {
    for stage in [ToolRecordStage::Call, ToolRecordStage::Result] {
        let fixture = Fixture::new(Setup {
            mismatch: Some(stage),
            ..Default::default()
        })
        .await;
        let ToolRuntimeError::Recording(failure) = fixture.call().await.unwrap_err() else {
            panic!("recording failure expected")
        };
        assert_eq!(failure.stage(), stage);
        let ToolRecordError::IdentityMismatch { event } = failure.record_error() else {
            panic!("wrong event must be retained")
        };
        assert_eq!(event.session_id, SessionId::from("wrong-session"));
        assert_eq!(
            fixture.report().run.unwrap().stop,
            Some(BudgetStopReason::IdentityMismatch)
        );
        assert_eq!(
            fixture.probe.calls.load(Ordering::SeqCst),
            usize::from(stage == ToolRecordStage::Result)
        );
        assert!(fixture.report().pending.is_empty());
        assert!(matches!(
            fixture.call().await,
            Err(ToolRuntimeError::Budget(_))
        ));
        fixture.finish().await;
    }
}

#[tokio::test(start_paused = true)]
async fn dropping_the_waiter_keeps_owned_settlement_and_records_alive() {
    let fixture = Fixture::new(Setup {
        block_call: true,
        ..Default::default()
    })
    .await;
    drop(fixture.call());
    reached(&fixture.recorder.calls, 1).await;
    assert_eq!(fixture.report().reserved.tool_output_bytes, 8);
    fixture.recorder.call_gate.add_permits(1);
    reached(&fixture.recorder.results, 1).await;
    assert_eq!(fixture.report().charged.tool_output_bytes, 4);
    assert!(fixture.report().pending.is_empty());
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn compatibility_binding_has_a_finite_tool_allowance() {
    let fixture = Fixture::new(Setup::default()).await;
    let turn = fixture
        .runtime
        .bind_turn(ToolTurnBinding::new(
            ToolCaller::new("caller-owner"),
            fixture.signal.clone(),
            fixture.recorder.clone(),
        ))
        .unwrap();
    assert_eq!(
        turn.budget_report().unwrap().unwrap().limits,
        BudgetLimits::default()
    );
    for _ in 0..BudgetLimits::default().resources.tool_calls {
        turn.gateway()
            .call("probe", json!({"ok":true}))
            .await
            .unwrap();
    }
    assert!(matches!(
        turn.gateway().call("probe", json!({"ok":true})).await,
        Err(ToolRuntimeError::Budget(_))
    ));
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 128);
    turn.finish(ToolFinishMode::Graceful).await.unwrap();
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn unrepresentable_budget_deadline_does_not_turn_call_timeout_into_task_expiry() {
    let fixture = Fixture::new(Setup {
        body: Body::Pending,
        timeout: Duration::from_secs(1),
        limits: BudgetLimits {
            active_time: Duration::MAX,
            ..Default::default()
        },
        ..Default::default()
    })
    .await;
    let call = fixture.call();
    reached(&fixture.probe.calls, 1).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(category(&call.await.unwrap()), ToolFailureCategory::Timeout);
    let report = fixture.report();
    assert_eq!(report.stop, None);
    assert_eq!(report.run.unwrap().stop, None);
    assert_eq!(report.active_time, Duration::from_secs(1));
    fixture.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_unbound_admission_scope_is_rejected_and_stopped() {
    let fixture = Fixture::new(Setup::default()).await;
    let limits = BudgetLimits::default();
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: SessionId::from("tool-budget-session"),
            agent_key: "unbound-agent".into(),
        },
        limits,
        limits,
        TokenBudgetMode::Hard,
        Arc::new(Clock(tokio::time::Instant::now())),
    )
    .unwrap();
    let mut run = task.begin_admission(limits).unwrap();
    let result = fixture.runtime.bind_turn(
        ToolTurnBinding::new(
            ToolCaller::new("caller-owner"),
            fixture.signal.clone(),
            fixture.recorder.clone(),
        )
        .with_budget(run.scope()),
    );
    assert!(matches!(
        result,
        Err(ToolRuntimeError::Budget(BudgetError::IdentityMismatch))
    ));
    let report = task.report().unwrap();
    assert_eq!(
        report.run.unwrap().stop,
        Some(BudgetStopReason::IdentityMismatch)
    );
    assert!(run.scope().check_active().is_err());
    assert_eq!(report.charged.tool_calls, 0);
    assert!(report.pending.is_empty());
    assert_eq!(fixture.recorder.calls.load(Ordering::SeqCst), 0);
    run.finish().unwrap();
    fixture.finish().await;
}

#[tokio::test]
async fn an_accepted_driver_dropped_before_first_poll_settles_before_turn_drain() {
    // Construct against a separate executor, then close it before call admission.
    // Handle::spawn drops this driver without ever polling its async body.
    let mut fixture = std::thread::spawn(|| {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(Fixture::new(Setup::default()))
    })
    .join()
    .unwrap();
    assert!(matches!(
        fixture.call().await,
        Err(ToolRuntimeError::Internal { .. })
    ));
    let report = fixture.report();
    assert_eq!(report.charged.tool_calls, 1);
    assert_eq!(report.charged.tool_output_bytes, 0);
    assert_eq!(report.reserved.tool_output_bytes, 0);
    assert_eq!(report.usage.tool_output_bytes.unknown, 0);
    assert!(report.pending.is_empty());
    assert_eq!(report.stop, None);
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.recorder.calls.load(Ordering::SeqCst), 0);
    let failure = fixture
        .turn
        .take()
        .unwrap()
        .finish(ToolFinishMode::Graceful)
        .await
        .unwrap_err();
    assert_eq!(failure.total_count(), 1);
    assert!(
        matches!(&failure.failures()[0], ToolRuntimeError::Internal { code, .. } if code == "tool_driver_unwound")
    );
    fixture.run.finish().unwrap();
    fixture.registry.shutdown().await.unwrap();
}
