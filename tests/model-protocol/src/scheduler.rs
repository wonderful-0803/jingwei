//! Independent admission and deadline tests with paused time and controlled providers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream};
use jingwei::id::{EventId, ModelCallId, SessionId, StepId, TaskId, TurnId};
use jingwei::llm::*;
use jingwei::plugin::*;
use jingwei::session::SessionRuntimeError;
use jingwei_core::{CancellationFuture, CancellationSignal, DecisionContext, SessionEvent};
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use tokio::sync::Semaphore;
use tokio::time::advance;
use tokio_util::sync::CancellationToken;

fn input(name: &str) -> GenerationRequest {
    GenerationRequest::text(vec![ModelMessage::user(name)])
}

fn label(request: &GenerationRequest) -> String {
    match request.messages.last().unwrap() {
        ModelMessage::User { content } => content.clone(),
        _ => panic!("test requests use a user message as their label"),
    }
}

fn options(seconds: u64) -> GenerationOptions {
    GenerationOptions {
        timeout: Some(Duration::from_secs(seconds)),
        ..Default::default()
    }
}

fn config(concurrent: usize, queued: usize, inflight: usize) -> ModelSchedulerConfig {
    ModelSchedulerConfig {
        max_concurrency: concurrent,
        max_queued: queued,
        max_inflight: inflight,
        ..Default::default()
    }
}

enum Call<'a> {
    Complete(ModelFuture<'a, Result<GenerationResponse, ModelGatewayError>>),
    Stream(ModelStream<'a>),
}

impl Call<'_> {
    async fn finish(self) -> Result<GenerationResponse, ModelGatewayError> {
        match self {
            Self::Complete(future) => future.await,
            Self::Stream(mut events) => {
                let mut finished = None;
                while let Some(event) = events.next().await {
                    if let GenerationStreamEvent::Finished(response) = event? {
                        assert!(finished.is_none(), "at most one successful terminal");
                        finished = Some(response);
                    }
                }
                Ok(finished.expect("successful streams have a terminal"))
            }
        }
    }
}

fn call<'a>(
    gateway: &'a dyn ModelGateway,
    request: &'a GenerationRequest,
    opts: GenerationOptions,
    streaming: bool,
) -> Call<'a> {
    if streaming {
        Call::Stream(gateway.generate_stream(request.clone(), opts))
    } else {
        Call::Complete(gateway.generate(request, opts))
    }
}

#[derive(Default)]
struct ModelState {
    gates: Mutex<BTreeMap<String, Arc<Semaphore>>>,
    entered: Mutex<Vec<String>>,
    tokens: Mutex<Vec<(String, CancellationToken)>>,
    timeouts: Mutex<BTreeMap<String, Option<Duration>>>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl ModelState {
    fn gate(&self, name: &str) -> Arc<Semaphore> {
        self.gates
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(0)))
            .clone()
    }

    fn release(&self, name: &str) {
        self.gate(name).add_permits(1);
    }

    fn names(&self) -> Vec<String> {
        self.entered.lock().unwrap().clone()
    }

    fn enter(
        self: &Arc<Self>,
        name: String,
        cancel: CancellationToken,
        timeout: Option<Duration>,
    ) -> ActiveCall {
        self.entered.lock().unwrap().push(name.clone());
        self.timeouts.lock().unwrap().insert(name.clone(), timeout);
        self.tokens.lock().unwrap().push((name, cancel));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        ActiveCall(self.clone())
    }

    fn cancelled(&self, name: &str) -> bool {
        self.tokens
            .lock()
            .unwrap()
            .iter()
            .find(|(called, _)| called == name)
            .is_some_and(|(_, token)| token.is_cancelled())
    }
}

struct ActiveCall(Arc<ModelState>);

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct GateModel(Arc<ModelState>);

impl Llm for GateModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::text_only()
    }

    fn generate<'a>(
        &'a self,
        request: &'a GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        let name = label(request);
        let active = self.0.enter(name.clone(), cancel, opts.timeout);
        let gate = self.0.gate(&name);
        Box::pin(async move {
            let _active = active;
            gate.acquire_owned().await.unwrap().forget();
            Ok(GenerationResponse::text(name, FinishReason::Stop))
        })
    }

    fn generate_stream(
        &self,
        request: GenerationRequest,
        opts: GenerationOptions,
        cancel: CancellationToken,
    ) -> GenerationStream {
        let name = label(&request);
        let active = self.0.enter(name.clone(), cancel, opts.timeout);
        if name == "flood" {
            let deltas = stream::iter((0..100).map(|_| {
                Ok(GenerationStreamEvent::Delta(GenerationDelta::Text {
                    text: "x".into(),
                }))
            }));
            Box::pin(deltas.chain(stream::once(async move {
                let _active = active;
                Ok(GenerationStreamEvent::Finished(GenerationResponse::text(
                    "x".repeat(100),
                    FinishReason::Stop,
                )))
            })))
        } else {
            let gate = self.0.gate(&name);
            let text = name.clone();
            let delta = stream::once(async move {
                gate.acquire_owned().await.unwrap().forget();
                Ok(GenerationStreamEvent::Delta(GenerationDelta::Text { text }))
            });
            Box::pin(delta.chain(stream::once(async move {
                let _active = active;
                Ok(GenerationStreamEvent::Finished(GenerationResponse::text(
                    name,
                    FinishReason::Stop,
                )))
            })))
        }
    }
}

struct Provider(Arc<dyn Llm>);

impl ServiceFactory<dyn Llm> for Provider {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async { Ok(ManagedService::ready(self.0.clone())) })
    }
}

impl Plugin for Provider {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-scheduler-model", LLM_PROVIDER, "gate")
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

struct RecordingGate {
    name: String,
    stage: ModelRecordStage,
    semaphore: Arc<Semaphore>,
}

#[derive(Default)]
struct Recorder {
    names: Mutex<BTreeMap<ModelCallId, String>>,
    entered: Mutex<Vec<(String, ModelRecordStage)>>,
    records: Mutex<Vec<ModelRecord>>,
    blocked: Mutex<Option<RecordingGate>>,
    failure: Mutex<Option<(String, ModelRecordStage)>>,
}

impl Recorder {
    fn block(&self, name: &str, stage: ModelRecordStage) -> Arc<Semaphore> {
        let semaphore = Arc::new(Semaphore::new(0));
        *self.blocked.lock().unwrap() = Some(RecordingGate {
            name: name.into(),
            stage,
            semaphore: semaphore.clone(),
        });
        semaphore
    }

    fn has_entered(&self, name: &str, stage: ModelRecordStage) -> bool {
        self.entered
            .lock()
            .unwrap()
            .iter()
            .any(|(recorded, recorded_stage)| recorded == name && *recorded_stage == stage)
    }

    fn records(&self) -> Vec<ModelRecord> {
        self.records.lock().unwrap().clone()
    }

    fn result(&self, name: &str) -> Option<ModelResult> {
        let names = self.names.lock().unwrap();
        self.records
            .lock()
            .unwrap()
            .iter()
            .find_map(|record| match record {
                ModelRecord::Result(result)
                    if names
                        .get(&result.call_id)
                        .is_some_and(|value| value == name) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
    }

    fn assert_pair(&self, name: &str) {
        let records = self.records();
        let requests: Vec<_> = records
            .iter()
            .filter_map(|record| match record {
                ModelRecord::Request(request) if label(&request.input) == name => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1);
        let results = records
            .iter()
            .filter(|record| {
                matches!(record, ModelRecord::Result(result) if result.call_id == requests[0].call_id)
            })
            .count();
        assert_eq!(results, 1);
    }
}

impl ModelEventRecorder for Recorder {
    fn append(
        &self,
        record: ModelRecord,
    ) -> ModelFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move {
            let (name, stage) = match &record {
                ModelRecord::Request(request) => {
                    let name = label(&request.input);
                    self.names
                        .lock()
                        .unwrap()
                        .insert(request.call_id.clone(), name.clone());
                    (name, ModelRecordStage::Request)
                }
                ModelRecord::Result(result) => (
                    self.names
                        .lock()
                        .unwrap()
                        .get(&result.call_id)
                        .unwrap()
                        .clone(),
                    ModelRecordStage::Result,
                ),
            };
            self.entered.lock().unwrap().push((name.clone(), stage));
            let gate = self
                .blocked
                .lock()
                .unwrap()
                .as_ref()
                .filter(|gate| gate.name == name && gate.stage == stage)
                .map(|gate| gate.semaphore.clone());
            if let Some(gate) = gate {
                gate.acquire_owned().await.unwrap().forget();
            }
            if self
                .failure
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(failed_name, failed_stage)| {
                    failed_name == &name && *failed_stage == stage
                })
            {
                return Err(SessionRuntimeError::Stopped);
            }
            let mut records = self.records.lock().unwrap();
            let seq = records.len() as u64;
            records.push(record.clone());
            Ok(Arc::new(SessionEvent {
                event_id: EventId::new(),
                session_id: SessionId::from("private-scheduler-session"),
                turn_id: TurnId::from("private-scheduler-turn"),
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
    model: Arc<ModelState>,
    recorder: Arc<Recorder>,
}

impl Fixture {
    async fn new(config: ModelSchedulerConfig) -> Self {
        let model = Arc::new(ModelState::default());
        let mut registrar = Registrar::default();
        registrar.add(Provider(Arc::new(GateModel(model.clone()))));
        registrar.add(CanonicalLlmRuntimePlugin::new().with_scheduler_config(config));
        registrar.select(LLM_RUNTIME, "canonical");
        let registry = registrar.finish().await.unwrap();
        let runtime = registry.llm_runtime().unwrap();
        Self {
            registry,
            runtime,
            model,
            recorder: Arc::new(Recorder::default()),
        }
    }

    fn turn(&self, cancel: CancellationToken) -> Box<dyn ModelTurn> {
        self.runtime
            .bind_turn(ModelTurnBinding::new(
                Arc::new(Signal(cancel)),
                self.recorder.clone(),
            ))
            .unwrap()
    }

    fn snapshot(&self) -> ModelSchedulerSnapshot {
        let snapshot = self.runtime.scheduler_snapshot().unwrap();
        assert_eq!(snapshot.jobs.len(), snapshot.inflight);
        assert_eq!(
            snapshot.queued + snapshot.preparing + snapshot.executing + snapshot.cleaning,
            snapshot.inflight
        );
        assert!(snapshot.preparing + snapshot.executing <= snapshot.config.max_concurrency);
        assert!(snapshot.queued <= snapshot.config.max_queued);
        assert!(snapshot.inflight <= snapshot.config.max_inflight);
        snapshot
    }

    async fn until(&self, description: &str, condition: impl Fn(&ModelSchedulerSnapshot) -> bool) {
        for _ in 0..10_000 {
            if condition(&self.snapshot()) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!(
            "scheduler did not reach {description}: {:?}",
            self.snapshot()
        );
    }

    async fn starts(&self, expected: &[&str]) {
        self.until("provider entry sequence", |_| {
            self.model.names() == expected
        })
        .await;
    }

    async fn close(&self, turns: Vec<Box<dyn ModelTurn>>) {
        for turn in turns {
            turn.finish(ModelFinishMode::Graceful).await.unwrap();
        }
        assert_eq!(self.snapshot().inflight, 0);
        assert_eq!(self.model.active.load(Ordering::SeqCst), 0);
        self.registry.shutdown().await.unwrap();
        assert!(!self.snapshot().accepting);
    }
}

fn assert_timeout(error: ModelGatewayError) {
    assert!(matches!(error, ModelGatewayError::Model(LlmError::Timeout)));
}

fn assert_cancelled(error: ModelGatewayError) {
    assert!(matches!(
        error,
        ModelGatewayError::Model(LlmError::Cancelled)
    ));
}

#[tokio::test(start_paused = true)]
async fn fifo_dispatch_is_shared_by_complete_stream_and_multiple_turns() {
    let fixture = Fixture::new(config(1, 2, 4)).await;
    let first = fixture.turn(CancellationToken::new());
    let second = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let c_input = input("c");
    let a = call(first.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(second.gateway(), &b_input, options(100), true);
    let c = call(first.gateway(), &c_input, options(100), false);
    fixture.until("two waiting calls", |s| s.queued == 2).await;
    assert_eq!(fixture.snapshot().inflight, 3);
    fixture.model.release("a");
    a.finish().await.unwrap();
    fixture.starts(&["a", "b"]).await;
    assert_eq!(fixture.snapshot().queued, 1);
    fixture.model.release("b");
    b.finish().await.unwrap();
    fixture.starts(&["a", "b", "c"]).await;
    fixture.model.release("c");
    c.finish().await.unwrap();
    assert_eq!(fixture.model.peak.load(Ordering::SeqCst), 1);
    for name in ["a", "b", "c"] {
        fixture.recorder.assert_pair(name);
    }
    fixture.close(vec![first, second]).await;
}

#[tokio::test(start_paused = true)]
async fn the_provider_execution_peak_never_exceeds_the_configured_limit() {
    let fixture = Fixture::new(config(2, 4, 8)).await;
    let turn = fixture.turn(CancellationToken::new());
    let requests: Vec<_> = (0..6).map(|i| input(&format!("job-{i}"))).collect();
    let calls: Vec<_> = requests
        .iter()
        .enumerate()
        .map(|(i, request)| call(turn.gateway(), request, options(100), i % 2 == 1))
        .collect();
    fixture
        .until("two providers and four queued calls", |s| {
            s.executing == 2 && s.queued == 4 && fixture.model.names().len() == 2
        })
        .await;
    assert_eq!(fixture.model.active.load(Ordering::SeqCst), 2);
    for i in 0..6 {
        fixture.model.release(&format!("job-{i}"));
    }
    for call in calls {
        call.finish().await.unwrap();
    }
    assert_eq!(fixture.model.names().len(), 6);
    assert_eq!(fixture.model.peak.load(Ordering::SeqCst), 2);
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn a_full_waiting_queue_rejects_without_recording_or_entering_the_provider() {
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 8)).await;
        let turn = fixture.turn(CancellationToken::new());
        let a_input = input("a");
        let b_input = input("b");
        let c_input = input("rejected");
        let a = call(turn.gateway(), &a_input, options(100), false);
        fixture.starts(&["a"]).await;
        let b = call(turn.gateway(), &b_input, options(100), true);
        fixture.until("one queued call", |s| s.queued == 1).await;
        let error = call(turn.gateway(), &c_input, options(100), streaming)
            .finish()
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ModelGatewayError::Runtime(ModelRuntimeError::Overloaded {
                capacity: ModelOverloadKind::QueueFull
            })
        ));
        assert!(
            !fixture
                .recorder
                .has_entered("rejected", ModelRecordStage::Request)
        );
        assert_eq!(fixture.snapshot().inflight, 2);
        fixture.model.release("a");
        fixture.model.release("b");
        a.finish().await.unwrap();
        b.finish().await.unwrap();
        assert_eq!(fixture.model.names(), ["a", "b"]);
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn inflight_capacity_includes_result_recording_after_the_execution_slot_is_released() {
    let fixture = Fixture::new(config(1, 2, 2)).await;
    let gate = fixture.recorder.block("a", ModelRecordStage::Result);
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let c_input = input("c");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    fixture.model.release("a");
    fixture
        .until("a blocked result record", |s| {
            s.cleaning == 1 && fixture.recorder.has_entered("a", ModelRecordStage::Result)
        })
        .await;
    advance(Duration::from_secs(150)).await;
    let pending_cleanup = fixture.snapshot();
    assert_eq!(pending_cleanup.inflight, 1);
    assert_eq!(
        pending_cleanup.jobs[0].recording,
        Some(ModelRecordStage::Result)
    );
    assert!(pending_cleanup.jobs[0].request_recorded);
    assert!(!pending_cleanup.jobs[0].result_recorded);
    let b = call(turn.gateway(), &b_input, options(100), false);
    fixture.starts(&["a", "b"]).await;
    assert_eq!(fixture.snapshot().inflight, 2);
    assert_eq!(fixture.snapshot().queued, 0);
    let error = call(turn.gateway(), &c_input, options(100), true)
        .finish()
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ModelGatewayError::Runtime(ModelRuntimeError::Overloaded {
            capacity: ModelOverloadKind::InflightFull
        })
    ));
    assert!(!fixture.recorder.has_entered("c", ModelRecordStage::Request));
    gate.add_permits(1);
    a.finish().await.unwrap();
    let c = call(turn.gateway(), &c_input, options(100), true);
    fixture
        .until("c accepted after cleanup", |s| s.queued == 1)
        .await;
    fixture.model.release("b");
    fixture.model.release("c");
    b.finish().await.unwrap();
    c.finish().await.unwrap();
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn queue_wait_and_execution_share_one_absolute_deadline() {
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        let turn = fixture.turn(CancellationToken::new());
        let a_input = input("a");
        let b_input = input("b");
        let a = call(turn.gateway(), &a_input, options(100), false);
        fixture.starts(&["a"]).await;
        let b = call(turn.gateway(), &b_input, options(5), streaming);
        fixture.until("b queued", |s| s.queued == 1).await;
        advance(Duration::from_secs(3)).await;
        fixture.model.release("a");
        a.finish().await.unwrap();
        fixture.starts(&["a", "b"]).await;
        let snapshot = fixture.snapshot();
        let job = snapshot
            .jobs
            .iter()
            .find(|job| job.phase == ModelJobPhase::Executing)
            .unwrap();
        assert_eq!(job.queue_time, Duration::from_secs(3));
        assert_eq!(job.remaining_time, Duration::from_secs(2));
        assert_eq!(
            fixture.model.timeouts.lock().unwrap().get("b").copied(),
            Some(Some(Duration::from_secs(5)))
        );
        let request = fixture
            .recorder
            .records()
            .into_iter()
            .find_map(|record| match record {
                ModelRecord::Request(request) if label(&request.input) == "b" => Some(request),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            request.options.timeout,
            Some(ModelTimeout::from_duration(Duration::from_secs(5)))
        );
        advance(Duration::from_secs(2)).await;
        assert_timeout(b.finish().await.unwrap_err());
        assert!(fixture.model.cancelled("b"));
        fixture.recorder.assert_pair("b");
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn zero_timeout_is_accepted_and_recorded_without_entering_the_provider() {
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        let turn = fixture.turn(CancellationToken::new());
        let request = input("zero-timeout");
        let error = call(turn.gateway(), &request, options(0), streaming)
            .finish()
            .await
            .unwrap_err();
        assert_timeout(error);
        fixture.recorder.assert_pair("zero-timeout");
        assert!(matches!(
            fixture.recorder.result("zero-timeout").unwrap().outcome,
            ModelRecordedOutcome::Failed {
                category: ModelFailureCategory::Timeout,
                ..
            }
        ));
        assert!(fixture.model.names().is_empty());
        assert_eq!(fixture.model.active.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.snapshot().inflight, 0);
        let next_request = input("after-zero");
        let next = call(turn.gateway(), &next_request, options(100), streaming);
        fixture.starts(&["after-zero"]).await;
        fixture.model.release("after-zero");
        next.finish().await.unwrap();
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_queued_deadline_expires_without_entering_the_provider() {
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        let turn = fixture.turn(CancellationToken::new());
        let a_input = input("a");
        let b_input = input("b");
        let a = call(turn.gateway(), &a_input, options(100), false);
        fixture.starts(&["a"]).await;
        let b = call(turn.gateway(), &b_input, options(5), streaming);
        fixture.until("b waiting", |s| s.queued == 1).await;
        advance(Duration::from_secs(5)).await;
        let error = b.finish().await.unwrap_err();
        assert!(matches!(
            error,
            ModelGatewayError::Runtime(ModelRuntimeError::QueueTimeout)
        ));
        assert_eq!(fixture.model.names(), ["a"]);
        let result = fixture.recorder.result("b").unwrap();
        assert!(matches!(
            result.outcome,
            ModelRecordedOutcome::Failed {
                category: ModelFailureCategory::Timeout,
                ref code,
                ..
            } if code == "model_queue_timeout"
        ));
        assert_eq!(fixture.snapshot().queued, 0);
        fixture.model.release("a");
        a.finish().await.unwrap();
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_request_recording_consumes_timeout_without_losing_the_record_attempt() {
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        let gate = fixture
            .recorder
            .block("slow-record", ModelRecordStage::Request);
        let turn = fixture.turn(CancellationToken::new());
        let request = input("slow-record");
        let accepted = call(turn.gateway(), &request, options(5), streaming);
        fixture
            .until("request recording started", |s| {
                s.preparing == 1
                    && fixture
                        .recorder
                        .has_entered("slow-record", ModelRecordStage::Request)
            })
            .await;
        advance(Duration::from_secs(8)).await;
        assert!(fixture.model.names().is_empty());
        let snapshot = fixture.snapshot();
        assert_eq!(snapshot.inflight, 1);
        assert_eq!(snapshot.jobs[0].recording, Some(ModelRecordStage::Request));
        assert!(!snapshot.jobs[0].request_recorded);
        gate.add_permits(1);
        assert_timeout(accepted.finish().await.unwrap_err());
        assert!(fixture.model.names().is_empty());
        fixture.recorder.assert_pair("slow-record");
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_queued_request_releases_queue_capacity_while_its_record_is_pending() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let gate = fixture.recorder.block("b", ModelRecordStage::Request);
    let a_turn = fixture.turn(CancellationToken::new());
    let b_cancel = CancellationToken::new();
    let b_turn = fixture.turn(b_cancel.clone());
    let a_input = input("a");
    let b_input = input("b");
    let c_input = input("c");
    let a = call(a_turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(b_turn.gateway(), &b_input, options(100), false);
    fixture
        .until("b queued with a pending request record", |s| {
            s.queued == 1 && fixture.recorder.has_entered("b", ModelRecordStage::Request)
        })
        .await;
    b_cancel.cancel();
    fixture
        .until("cancelled b removed from queue", |s| s.queued == 0)
        .await;
    assert_eq!(fixture.snapshot().inflight, 2);
    let c = call(a_turn.gateway(), &c_input, options(100), true);
    fixture
        .until("c can reuse the queue position", |s| s.queued == 1)
        .await;
    gate.add_permits(1);
    assert_cancelled(b.finish().await.unwrap_err());
    fixture.model.release("a");
    a.finish().await.unwrap();
    fixture.starts(&["a", "c"]).await;
    fixture.model.release("c");
    c.finish().await.unwrap();
    fixture.recorder.assert_pair("b");
    fixture.close(vec![a_turn, b_turn]).await;
}

#[tokio::test(start_paused = true)]
async fn dropping_a_queued_stream_cancels_it_before_provider_entry() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(turn.gateway(), &b_input, options(100), true);
    fixture.until("b queued", |s| s.queued == 1).await;
    drop(b);
    fixture
        .until("dropped stream canonically closed", |s| {
            s.inflight == 1 && fixture.recorder.result("b").is_some()
        })
        .await;
    assert_eq!(fixture.model.names(), ["a"]);
    assert!(matches!(
        fixture.recorder.result("b").unwrap().outcome,
        ModelRecordedOutcome::Failed {
            category: ModelFailureCategory::Cancelled,
            ..
        }
    ));
    fixture.model.release("a");
    a.finish().await.unwrap();
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn a_backpressured_stream_times_out_and_releases_the_execution_slot() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let turn = fixture.turn(CancellationToken::new());
    let flood_input = input("flood");
    let b_input = input("b");
    let flooded = call(turn.gateway(), &flood_input, options(5), true);
    fixture.starts(&["flood"]).await;
    let b = call(turn.gateway(), &b_input, options(100), false);
    fixture
        .until("b queued behind slow consumer", |s| s.queued == 1)
        .await;
    advance(Duration::from_secs(5)).await;
    fixture.starts(&["flood", "b"]).await;
    assert_timeout(flooded.finish().await.unwrap_err());
    assert!(fixture.model.cancelled("flood"));
    assert_eq!(fixture.model.peak.load(Ordering::SeqCst), 1);
    fixture.model.release("b");
    b.finish().await.unwrap();
    fixture.recorder.assert_pair("flood");
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn graceful_finish_drains_queued_complete_calls_even_when_waiters_are_dropped() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(turn.gateway(), &b_input, options(100), false);
    fixture.until("accepted b queued", |s| s.queued == 1).await;
    drop(a);
    drop(b);
    let mut finishing = turn.finish(ModelFinishMode::Graceful);
    assert!(futures::poll!(&mut finishing).is_pending());
    fixture.model.release("a");
    fixture.starts(&["a", "b"]).await;
    assert!(futures::poll!(&mut finishing).is_pending());
    fixture.model.release("b");
    finishing.await.unwrap();
    fixture.recorder.assert_pair("a");
    fixture.recorder.assert_pair("b");
    fixture.close(vec![]).await;
}

#[tokio::test(start_paused = true)]
async fn cancel_finish_waits_for_result_recording_after_stopping_running_and_queued_calls() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let gate = fixture.recorder.block("b", ModelRecordStage::Result);
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(turn.gateway(), &b_input, options(100), false);
    fixture.until("b waiting", |s| s.queued == 1).await;
    drop(a);
    drop(b);
    let mut finishing = turn.finish(ModelFinishMode::Cancel);
    assert!(futures::poll!(&mut finishing).is_pending());
    fixture
        .until("cancelled b awaiting result durability", |s| {
            s.queued == 0 && fixture.recorder.has_entered("b", ModelRecordStage::Result)
        })
        .await;
    assert!(futures::poll!(&mut finishing).is_pending());
    assert_eq!(fixture.model.names(), ["a"]);
    assert!(fixture.model.cancelled("a"));
    gate.add_permits(1);
    finishing.await.unwrap();
    fixture.recorder.assert_pair("a");
    fixture.recorder.assert_pair("b");
    fixture.close(vec![]).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_rejects_new_admission_and_waits_for_cancelled_recording_work() {
    let fixture = Fixture::new(config(1, 1, 4)).await;
    let gate = fixture.recorder.block("b", ModelRecordStage::Request);
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    let b = call(turn.gateway(), &b_input, options(100), false);
    fixture
        .until("b queued before shutdown", |s| {
            s.queued == 1 && fixture.recorder.has_entered("b", ModelRecordStage::Request)
        })
        .await;
    drop(a);
    drop(b);
    let mut shutdown = Box::pin(fixture.registry.shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    fixture
        .until("runtime stopping", |s| !s.accepting && s.queued == 0)
        .await;
    assert!(
        fixture
            .runtime
            .bind_turn(ModelTurnBinding::new(
                Arc::new(Signal(CancellationToken::new())),
                fixture.recorder.clone()
            ))
            .is_err()
    );
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(fixture.snapshot().inflight > 0);
    gate.add_permits(1);
    shutdown.await.unwrap();
    turn.finish(ModelFinishMode::Graceful).await.unwrap();
    assert_eq!(fixture.snapshot().inflight, 0);
    assert_eq!(fixture.model.names(), ["a"]);
    assert!(fixture.model.cancelled("a"));
    fixture.recorder.assert_pair("b");
}

#[tokio::test(start_paused = true)]
async fn recording_failures_release_scheduler_capacity_and_remain_in_finish_evidence() {
    for stage in [ModelRecordStage::Request, ModelRecordStage::Result] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        *fixture.recorder.failure.lock().unwrap() = Some(("a".into(), stage));
        let turn = fixture.turn(CancellationToken::new());
        let a_input = input("a");
        let b_input = input("b");
        let a = call(turn.gateway(), &a_input, options(100), false);
        let b = call(turn.gateway(), &b_input, options(100), true);
        fixture.model.release("a");
        assert!(matches!(
            a.finish().await,
            Err(ModelGatewayError::Recording(_))
        ));
        assert_eq!(
            fixture.model.names().iter().any(|name| name == "a"),
            stage == ModelRecordStage::Result
        );
        fixture
            .until("b executes after recording failure", |_| {
                fixture.model.names().last().is_some_and(|name| name == "b")
            })
            .await;
        fixture.model.release("b");
        b.finish().await.unwrap();
        assert_eq!(fixture.snapshot().inflight, 0);
        assert!(turn.finish(ModelFinishMode::Graceful).await.is_err());
        fixture.registry.shutdown().await.unwrap();
        assert_eq!(fixture.model.active.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn default_scheduling_and_timeout_are_finite_and_visible() {
    let defaults = ModelSchedulerConfig::default();
    assert_eq!(defaults.max_concurrency, 4);
    assert_eq!(defaults.max_queued, 32);
    assert_eq!(defaults.max_inflight, 64);
    assert_eq!(defaults.max_request_bytes, 8 * 1024 * 1024);
    let fixture = Fixture::new(defaults).await;
    let turn = fixture.turn(CancellationToken::new());
    let request = input("default");
    let accepted = call(
        turn.gateway(),
        &request,
        GenerationOptions::default(),
        false,
    );
    fixture.starts(&["default"]).await;
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot.config, defaults);
    assert_eq!(snapshot.jobs[0].timeout, Duration::from_secs(600));
    assert_eq!(snapshot.jobs[0].remaining_time, Duration::from_secs(600));
    fixture.model.release("default");
    accepted.finish().await.unwrap();
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn invalid_capacities_are_rejected_but_zero_queue_capacity_is_valid() {
    for invalid in [
        ModelSchedulerConfig {
            max_concurrency: 0,
            ..Default::default()
        },
        ModelSchedulerConfig {
            max_inflight: 0,
            ..Default::default()
        },
        ModelSchedulerConfig {
            max_request_bytes: 0,
            ..Default::default()
        },
        config(2, 1, 1),
    ] {
        let mut registrar = Registrar::default();
        registrar.add(Provider(Arc::new(GateModel(Arc::new(
            ModelState::default(),
        )))));
        registrar.add(CanonicalLlmRuntimePlugin::new().with_scheduler_config(invalid));
        registrar.select(LLM_RUNTIME, "canonical");
        assert!(registrar.finish().await.is_err());
    }
    let fixture = Fixture::new(config(1, 0, 4)).await;
    let turn = fixture.turn(CancellationToken::new());
    let a_input = input("a");
    let b_input = input("b");
    let a = call(turn.gateway(), &a_input, options(100), false);
    fixture.starts(&["a"]).await;
    assert!(matches!(
        call(turn.gateway(), &b_input, options(100), false)
            .finish()
            .await,
        Err(ModelGatewayError::Runtime(ModelRuntimeError::Overloaded {
            capacity: ModelOverloadKind::QueueFull
        }))
    ));
    fixture.model.release("a");
    a.finish().await.unwrap();
    fixture.close(vec![turn]).await;
}

#[tokio::test(start_paused = true)]
async fn request_byte_limits_cover_messages_and_context_before_acceptance() {
    for streaming in [false, true] {
        let fixture = Fixture::new(ModelSchedulerConfig {
            max_request_bytes: 512,
            ..config(1, 1, 4)
        })
        .await;
        let turn = fixture.turn(CancellationToken::new());
        let long_input = input(&"x".repeat(2_048));
        let short_input = input("short");
        for (request, opts) in [
            (&long_input, options(100)),
            (
                &short_input,
                GenerationOptions {
                    context: Some(DecisionContext {
                        task_id: TaskId::from("x".repeat(2_048)),
                        step_id: StepId::new(),
                    }),
                    ..options(100)
                },
            ),
        ] {
            let error = call(turn.gateway(), request, opts, streaming)
                .finish()
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                ModelGatewayError::Runtime(ModelRuntimeError::RequestTooLarge { limit_bytes: 512 })
            ));
        }
        assert!(fixture.model.names().is_empty());
        assert!(fixture.recorder.records().is_empty());
        assert_eq!(fixture.snapshot().inflight, 0);
        let accepted = call(turn.gateway(), &short_input, options(100), streaming);
        fixture.starts(&["short"]).await;
        fixture.model.release("short");
        accepted.finish().await.unwrap();
        fixture.close(vec![turn]).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_huge_call_timeout_is_tightened_while_an_unrepresentable_host_timeout_is_rejected() {
    let mut registrar = Registrar::default();
    registrar.add(Provider(Arc::new(GateModel(Arc::new(
        ModelState::default(),
    )))));
    registrar.add(CanonicalLlmRuntimePlugin::new().with_default_timeout(Duration::MAX));
    registrar.select(LLM_RUNTIME, "canonical");
    assert!(registrar.finish().await.is_err());
    for streaming in [false, true] {
        let fixture = Fixture::new(config(1, 1, 4)).await;
        let turn = fixture.turn(CancellationToken::new());
        let request = input("tightened-timeout");
        let accepted = call(
            turn.gateway(),
            &request,
            GenerationOptions {
                timeout: Some(Duration::MAX),
                ..Default::default()
            },
            streaming,
        );
        fixture.starts(&["tightened-timeout"]).await;
        assert_eq!(fixture.snapshot().jobs[0].timeout, Duration::from_secs(600));
        fixture.model.release("tightened-timeout");
        accepted.finish().await.unwrap();
        fixture.recorder.assert_pair("tightened-timeout");
        fixture.close(vec![turn]).await;
    }
}
