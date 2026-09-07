//! Model admission and accounting through the public runtime boundary.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream};
use jingwei::budget::*;
use jingwei::llm::*;
use jingwei::plugin::*;
use jingwei_core::{
    CancellationFuture, CancellationSignal, DecisionContext, EventId, SessionEvent, SessionId,
    StepId, TaskId, TurnId,
};
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use tokio::sync::Semaphore;
use tokio::time::{Instant, advance};
use tokio_util::sync::CancellationToken;

struct Clock(Instant);
impl BudgetClock for Clock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

fn ledger(mode: TokenBudgetMode, limits: BudgetLimits) -> (TaskBudget, BudgetRun) {
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::from("model-budget-task"),
            session_id: SessionId::from("model-budget-session"),
            agent_key: "model-budget-agent".into(),
        },
        limits,
        limits,
        mode,
        Arc::new(Clock(Instant::now())),
    )
    .unwrap();
    let run = task
        .begin_run(TurnId::from("model-budget-turn"), limits)
        .unwrap();
    (task, run)
}

struct Verified;
impl ModelBudgetEstimator for Verified {
    fn estimate(
        &self,
        _: &GenerationRequest,
        _: &GenerationOptions,
    ) -> Result<ModelTokenEstimate, BudgetError> {
        Ok(ModelTokenEstimate {
            input_tokens: 100,
            output_tokens: 100,
            input_evidence: TokenBoundEvidence::VerifiedUpperBound,
            output_evidence: TokenBoundEvidence::VerifiedUpperBound,
        })
    }
}

#[derive(Default)]
struct Provider {
    calls: AtomicUsize,
    mismatched_terminal: AtomicBool,
    usage: Mutex<TokenUsage>,
    gate: Mutex<Option<Arc<Semaphore>>>,
}

impl Provider {
    fn response(&self) -> GenerationResponse {
        let mut response = GenerationResponse::text("answer", FinishReason::Stop);
        response.usage = self.usage.lock().unwrap().clone();
        response
    }
}

impl Llm for Provider {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::text_only()
    }
    fn generate<'a>(
        &'a self,
        _: &'a GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self.response();
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.acquire_owned().await.unwrap().forget();
            }
            Ok(response)
        })
    }
    fn generate_stream(
        &self,
        _: GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> GenerationStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self.response();
        let gate = self.gate.lock().unwrap().clone();
        let delta_text = if self.mismatched_terminal.load(Ordering::SeqCst) {
            "different"
        } else {
            "answer"
        };
        Box::pin(
            stream::once(async move {
                if let Some(gate) = gate {
                    gate.acquire_owned().await.unwrap().forget();
                }
                Ok(GenerationStreamEvent::Delta(GenerationDelta::Text {
                    text: delta_text.into(),
                }))
            })
            .chain(stream::once(async move {
                Ok(GenerationStreamEvent::Finished(response))
            })),
        )
    }
}

struct ProviderPlugin(Arc<Provider>);
impl ServiceFactory<dyn Llm> for ProviderPlugin {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async { Ok(ManagedService::ready(self.0.clone() as Arc<dyn Llm>)) })
    }
}
impl Plugin for ProviderPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-budget-model", LLM_PROVIDER, "budget-model")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(Self(self.0.clone())))
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

#[derive(Default)]
struct Recorder {
    records: Mutex<Vec<ModelRecord>>,
    entered: AtomicUsize,
    fail: AtomicUsize,
    wrong: AtomicBool,
    request_gate: Mutex<Option<Arc<Semaphore>>>,
    result_gate: Mutex<Option<Arc<Semaphore>>>,
}

impl ModelEventRecorder for Recorder {
    fn append(
        &self,
        record: ModelRecord,
    ) -> ModelFuture<'_, Result<Arc<SessionEvent>, jingwei::session::SessionRuntimeError>> {
        Box::pin(async move {
            let stage = if matches!(&record, ModelRecord::Request(_)) {
                1
            } else {
                2
            };
            self.entered.fetch_add(1, Ordering::SeqCst);
            let gate = if stage == 1 {
                self.request_gate.lock().unwrap().clone()
            } else {
                self.result_gate.lock().unwrap().clone()
            };
            if let Some(gate) = gate {
                gate.acquire_owned().await.unwrap().forget();
            }
            if self.fail.load(Ordering::SeqCst) == stage {
                return Err(jingwei::session::SessionRuntimeError::Stopped);
            }
            let mut records = self.records.lock().unwrap();
            let seq = records.len() as u64;
            records.push(record.clone());
            Ok(Arc::new(SessionEvent {
                event_id: EventId::new(),
                session_id: SessionId::from(if self.wrong.load(Ordering::SeqCst) {
                    "wrong-session"
                } else {
                    "model-budget-session"
                }),
                turn_id: TurnId::from("model-budget-turn"),
                generation_id: None,
                message_id: None,
                seq,
                kind: record.into_session_event_kind(),
            }))
        })
    }
}

struct Fixture {
    registry: PluginRegistry,
    runtime: Arc<dyn LlmRuntime>,
    provider: Arc<Provider>,
    recorder: Arc<Recorder>,
}
impl Fixture {
    async fn new(verified: bool) -> Self {
        let provider = Arc::new(Provider::default());
        let mut registrar = Registrar::default();
        registrar.add(ProviderPlugin(provider.clone()));
        let mut plugin =
            CanonicalLlmRuntimePlugin::new().with_scheduler_config(ModelSchedulerConfig {
                max_concurrency: 1,
                max_queued: 1,
                max_inflight: 4,
                ..Default::default()
            });
        if verified {
            plugin = plugin.with_budget_estimator(Arc::new(Verified));
        }
        registrar.add(plugin);
        registrar.select(LLM_RUNTIME, "canonical");
        let registry = registrar.finish().await.unwrap();
        let runtime = registry.llm_runtime().unwrap();
        Self {
            registry,
            runtime,
            provider,
            recorder: Arc::new(Recorder::default()),
        }
    }
    fn bind(&self, scope: Option<BudgetScope>) -> Box<dyn ModelTurn> {
        let binding = ModelTurnBinding::new(
            Arc::new(Signal(CancellationToken::new())),
            self.recorder.clone(),
        );
        self.runtime
            .bind_turn(if let Some(scope) = scope {
                binding.with_budget(scope)
            } else {
                binding
            })
            .unwrap()
    }
    async fn wait(&self, condition: impl Fn() -> bool) {
        for _ in 0..10_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("budget model condition was not reached");
    }
}

fn input() -> GenerationRequest {
    GenerationRequest::text(vec![ModelMessage::user("hello")])
}

async fn invoke(
    turn: &dyn ModelTurn,
    streaming: bool,
    options: GenerationOptions,
) -> Result<(), ModelGatewayError> {
    if streaming {
        let mut output = turn.gateway().generate_stream(input(), options);
        while let Some(event) = output.next().await {
            event?;
        }
    } else {
        turn.gateway().generate(&input(), options).await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn direct_binding_has_an_observable_finite_default_budget() {
    let fixture = Fixture::new(false).await;
    let turn = fixture.bind(None);
    let report = turn.budget_report().unwrap().unwrap();
    assert_eq!(report.token_mode, TokenBudgetMode::Soft);
    assert_eq!(report.limits, BudgetLimits::default());
    for _ in 0..128 {
        invoke(&*turn, false, GenerationOptions::default())
            .await
            .unwrap();
    }
    assert!(matches!(
        invoke(&*turn, false, GenerationOptions::default()).await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)))
    ));
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 128);
    assert_eq!(turn.budget_report().unwrap().unwrap().charged.steps, 128);
    turn.finish(ModelFinishMode::Graceful).await.unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn hard_budget_rejects_soft_evidence_before_accepting_a_model_attempt() {
    let fixture = Fixture::new(false).await;
    let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
    let turn = fixture.bind(Some(run.scope()));
    assert!(matches!(
        invoke(&*turn, false, GenerationOptions::default()).await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(
            BudgetError::UnverifiedTokenBound(_)
        )))
    ));
    let report = task.report().unwrap();
    assert_eq!(report.charged.model_requests, 0);
    assert_eq!(
        report.run.unwrap().stop,
        Some(BudgetStopReason::InvalidRequest)
    );
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.recorder.records.lock().unwrap().is_empty());
    turn.finish(ModelFinishMode::Graceful).await.unwrap();
    run.finish().unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn actual_and_unknown_usage_are_settled_independently_for_both_model_modes() {
    for streaming in [false, true] {
        let fixture = Fixture::new(true).await;
        fixture.provider.usage.lock().unwrap().input_tokens = Some(7);
        let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
        let turn = fixture.bind(Some(run.scope()));
        invoke(&*turn, streaming, GenerationOptions::default())
            .await
            .unwrap();
        let report = task.report().unwrap();
        assert_eq!(report.charged.steps, 1);
        assert_eq!(report.charged.model_requests, 1);
        assert_eq!(report.charged.input_tokens, 7);
        assert_eq!(report.charged.output_tokens, 100);
        assert_eq!(report.usage.input_tokens.actual, 7);
        assert_eq!(report.usage.output_tokens.unknown, 1);
        assert!(report.pending.is_empty());
        let metrics = report.run.unwrap().metrics;
        assert_eq!(metrics.confirmed.model_requests, 1);
        assert_eq!(metrics.confirmed.model_results, 1);
        turn.finish(ModelFinishMode::Graceful).await.unwrap();
        run.finish().unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn rejected_response_validation_preserves_raw_provider_usage() {
    for streaming in [false, true] {
        let fixture = Fixture::new(true).await;
        *fixture.provider.usage.lock().unwrap() = TokenUsage {
            input_tokens: Some(9),
            output_tokens: Some(10),
            total_tokens: Some(19),
        };
        let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
        let turn = fixture.bind(Some(run.scope()));
        let mut options = GenerationOptions::default();
        if streaming {
            fixture
                .provider
                .mismatched_terminal
                .store(true, Ordering::SeqCst);
        } else {
            options.limits.max_output_bytes = 1;
        }
        assert!(invoke(&*turn, streaming, options).await.is_err());
        let report = task.report().unwrap();
        assert_eq!(report.charged.input_tokens, 9);
        assert_eq!(report.charged.output_tokens, 10);
        assert_eq!(report.usage.output_tokens.actual, 10);
        turn.finish(ModelFinishMode::Graceful).await.unwrap();
        run.finish().unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn usage_over_the_verified_bound_stops_admission_without_erasing_actuals() {
    for streaming in [false, true] {
        let fixture = Fixture::new(true).await;
        fixture.provider.usage.lock().unwrap().input_tokens = Some(101);
        let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
        let turn = fixture.bind(Some(run.scope()));
        assert!(matches!(
            invoke(&*turn, streaming, GenerationOptions::default()).await,
            Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)))
        ));
        let report = task.report().unwrap();
        assert_eq!(report.charged.input_tokens, 101);
        assert_eq!(report.usage.input_tokens.actual, 101);
        assert!(report.pending.is_empty());
        assert!(fixture.recorder.records.lock().unwrap().iter().any(|record| {
            matches!(record, ModelRecord::Result(ModelResult {
                outcome: ModelRecordedOutcome::Failed { category: ModelFailureCategory::Budget, partial, .. }, ..
            }) if partial.content.as_deref() == Some("answer"))
        }));
        assert!(
            invoke(&*turn, false, GenerationOptions::default())
                .await
                .is_err()
        );
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
        turn.finish(ModelFinishMode::Graceful).await.unwrap();
        run.finish().unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn recording_failures_keep_attempts_and_only_refund_unstarted_variable_reservations() {
    for failed_stage in [1, 2] {
        let fixture = Fixture::new(true).await;
        fixture.recorder.fail.store(failed_stage, Ordering::SeqCst);
        *fixture.provider.usage.lock().unwrap() = TokenUsage {
            input_tokens: Some(7),
            output_tokens: Some(8),
            total_tokens: None,
        };
        let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
        let turn = fixture.bind(Some(run.scope()));
        assert!(matches!(
            invoke(&*turn, false, GenerationOptions::default()).await,
            Err(ModelGatewayError::Recording(_))
        ));
        let report = task.report().unwrap();
        assert_eq!(report.charged.model_requests, 1);
        assert_eq!(
            report.charged.input_tokens,
            if failed_stage == 1 { 0 } else { 7 }
        );
        assert_eq!(
            report.charged.output_tokens,
            if failed_stage == 1 { 0 } else { 8 }
        );
        assert!(report.pending.is_empty());
        let metrics = report.run.unwrap().metrics;
        assert_eq!(
            metrics.unconfirmed.model_requests,
            u64::from(failed_stage == 1)
        );
        assert_eq!(
            metrics.unconfirmed.model_results,
            u64::from(failed_stage == 2)
        );
        assert!(turn.finish(ModelFinishMode::Graceful).await.is_err());
        run.finish().unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn spoofed_context_and_wrong_confirmed_event_identity_freeze_the_bound_run() {
    for wrong_event in [false, true] {
        let fixture = Fixture::new(true).await;
        fixture.recorder.wrong.store(wrong_event, Ordering::SeqCst);
        let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
        let turn = fixture.bind(Some(run.scope()));
        let options = if wrong_event {
            GenerationOptions::default()
        } else {
            GenerationOptions {
                context: Some(DecisionContext {
                    task_id: TaskId::from("another-task"),
                    step_id: StepId::new(),
                }),
                ..Default::default()
            }
        };
        let result = invoke(&*turn, false, options).await;
        if wrong_event {
            assert!(matches!(result, Err(ModelGatewayError::Recording(_))));
        } else {
            assert!(matches!(
                result,
                Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(
                    BudgetError::IdentityMismatch
                )))
            ));
        }
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            task.report().unwrap().run.unwrap().stop,
            Some(BudgetStopReason::IdentityMismatch)
        );
        assert_eq!(
            turn.finish(ModelFinishMode::Graceful).await.is_err(),
            wrong_event
        );
        run.finish().unwrap();
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_budget_deadline_releases_slots_but_retains_blocked_record_ownership() {
    let fixture = Fixture::new(true).await;
    let gate = Arc::new(Semaphore::new(0));
    *fixture.recorder.request_gate.lock().unwrap() = Some(gate.clone());
    let (task, mut run) = ledger(
        TokenBudgetMode::Hard,
        BudgetLimits {
            active_time: Duration::from_secs(5),
            ..Default::default()
        },
    );
    let turn = fixture.bind(Some(run.scope()));
    let request = input();
    let accepted = turn
        .gateway()
        .generate(&request, GenerationOptions::default());
    fixture
        .wait(|| fixture.recorder.entered.load(Ordering::SeqCst) == 1)
        .await;
    advance(Duration::from_secs(5)).await;
    fixture
        .wait(|| fixture.runtime.scheduler_snapshot().unwrap().cleaning == 1)
        .await;
    assert_eq!(fixture.runtime.scheduler_snapshot().unwrap().inflight, 1);
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 0);
    gate.add_permits(1);
    assert!(matches!(
        accepted.await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)))
    ));
    let report = task.report().unwrap();
    assert_eq!(report.charged.model_requests, 1);
    assert_eq!(report.charged.input_tokens, 0);
    assert_eq!(report.reserved.input_tokens, 0);
    assert!(report.pending.is_empty());
    turn.finish(ModelFinishMode::Graceful).await.unwrap();
    run.finish().unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn budget_stop_cancels_running_and_queued_work_without_resetting_usage() {
    let fixture = Fixture::new(true).await;
    *fixture.provider.gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    let (task, mut run) = ledger(TokenBudgetMode::Hard, BudgetLimits::default());
    let turn = fixture.bind(Some(run.scope()));
    let request = input();
    let first = turn
        .gateway()
        .generate(&request, GenerationOptions::default());
    fixture
        .wait(|| fixture.provider.calls.load(Ordering::SeqCst) == 1)
        .await;
    let second = turn
        .gateway()
        .generate(&request, GenerationOptions::default());
    fixture
        .wait(|| fixture.runtime.scheduler_snapshot().unwrap().queued == 1)
        .await;
    // Another trusted runtime can stop this shared run.
    run.scope()
        .stop_with(BudgetStopReason::ResourceLimit(BudgetResource::ToolCalls))
        .unwrap();
    assert!(matches!(
        first.await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)))
    ));
    assert!(matches!(
        second.await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Budget(_)))
    ));
    let report = task.report().unwrap();
    assert_eq!(report.charged.model_requests, 2);
    assert_eq!(report.charged.input_tokens, 100);
    assert_eq!(report.charged.output_tokens, 100);
    assert_eq!(report.usage.input_tokens.unknown, 1);
    assert!(report.pending.is_empty());
    assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    turn.finish(ModelFinishMode::Graceful).await.unwrap();
    run.finish().unwrap();
    fixture.registry.shutdown().await.unwrap();
}
