//! Private action contract/integration tests; no live model or public test delivery.

#[tokio::test]
async fn cancellation_during_tool_body_retains_committed_cancelled_execution() {
    let fixture = Fixture::new(Setup {
        mode: ToolMode::Pending,
        ..Setup::new(vec![call_response(false)])
    })
    .await;
    let step = ActionStep::new(protocol(false));
    let (result, ()) = tokio::join!(fixture.run(&step), async {
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.tool.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fixture.signal.0.cancel();
    });
    let error = result.unwrap_err();
    assert!(matches!(
        *error.failure,
        ActionStepFailure::Tool(ToolRuntimeError::Cancelled { .. })
    ));
    let execution = error.completed_execution.unwrap();
    assert_eq!(
        execution.call().action.as_ref().unwrap().decision,
        error.context
    );
    assert!(matches!(
        execution.result().outcome,
        ToolRecordedOutcome::Failed {
            category: ToolFailureCategory::Cancelled,
            ..
        }
    ));
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

struct CancelAfterParse(CancellationToken);
impl ActionProtocol for CancelAfterParse {
    fn request(
        &self,
        messages: Vec<ModelMessage>,
        tools: &[ModelToolDefinition],
    ) -> Result<GenerationRequest, ActionError> {
        NativeToolProtocol.request(messages, tools)
    }
    fn parse(&self, response: &GenerationResponse) -> Result<AgentAction, ActionError> {
        let action = NativeToolProtocol.parse(response)?;
        self.0.cancel();
        Ok(action)
    }
    fn continuation(
        &self,
        response: &GenerationResponse,
        action: &AgentAction,
        execution: Option<&ToolExecution>,
    ) -> Result<Vec<ModelMessage>, ActionError> {
        NativeToolProtocol.continuation(response, action, execution)
    }
}
#[tokio::test]
async fn cancellation_after_parse_prevents_tool_admission() {
    let fixture = Fixture::new(Setup::new(vec![call_response(false)])).await;
    let error = fixture
        .run(&ActionStep::new(Arc::new(CancelAfterParse(
            fixture.signal.0.clone(),
        ))))
        .await
        .unwrap_err();
    assert!(matches!(*error.failure, ActionStepFailure::Cancelled));
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.recorder.events.lock().unwrap().len(), 2);
    fixture.close().await;
}

use jingwei::session::{
    CommitCertainty, PersistAppendOutcome, SESSION_PERSISTENCE, SessionFuture, SessionPersistence,
    SessionPersistenceError,
};
use jingwei_agent_runtime::CanonicalAgentRuntimePlugin;
use jingwei_session_runtime::CanonicalSessionRuntimePlugin;

// Memory-only persistence fixture, not a production storage provider.
struct Memory(Arc<Mutex<Vec<SessionEvent>>>);
impl SessionPersistence for Memory {
    fn load<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Vec<SessionEvent>, SessionPersistenceError>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| &e.session_id == session_id)
                .cloned()
                .collect())
        })
    }
    fn commit_durable<'a>(
        &'a self,
        event: &'a SessionEvent,
    ) -> SessionFuture<'a, Result<PersistAppendOutcome, SessionPersistenceError>> {
        Box::pin(async move {
            let mut events = self.0.lock().unwrap();
            let count = events
                .iter()
                .filter(|e| e.session_id == event.session_id)
                .count() as u64;
            if event.seq != count {
                return Err(SessionPersistenceError::Io {
                    operation: "private_commit",
                    message: "sequence conflict".into(),
                    certainty: CommitCertainty::DefinitelyNotCommitted,
                });
            }
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
        PluginDescriptor::provider("private-action-memory", SESSION_PERSISTENCE, "memory")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_persistence_factory(Arc::new(Self(self.0.clone())))
    }
}
struct TwoStepAgent {
    protocol: Arc<dyn ActionProtocol>,
    reports: Arc<Mutex<Vec<ActionStepReport>>>,
}
impl Agent for TwoStepAgent {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            let step = ActionStep::new(self.protocol.clone());
            let task = ctx
                .budget()
                .expect("canonical task scope")
                .report()?
                .identity
                .task_id;
            let mut messages = vec![ModelMessage::user(input.user_message)];
            // Private bounded driver, not a shipped reference Agent or general loop.
            for _ in 0..2 {
                let report = step
                    .run(
                        task.clone(),
                        messages,
                        ctx.model().unwrap(),
                        ctx.tools().unwrap(),
                        ctx.cancellation(),
                        ActionStepOptions::default(),
                    )
                    .await
                    .map_err(|error| match *error.failure {
                        ActionStepFailure::Model(e) => AgentError::from(e),
                        ActionStepFailure::Tool(e) => AgentError::from(e),
                        ActionStepFailure::Cancelled => AgentError::Cancelled,
                        ActionStepFailure::Validation(e) => {
                            AgentError::failed("action_validation", e.to_string(), false)
                        }
                    })?;
                let output = match &report.outcome {
                    StepOutcome::Final { text } => Some(AgentTurnOutput {
                        final_text: text.clone(),
                        outcome: TurnOutcome::Completed,
                        artifact: None,
                    }),
                    StepOutcome::AskUser { question } => Some(AgentTurnOutput {
                        final_text: question.clone(),
                        outcome: TurnOutcome::WaitingForInput,
                        artifact: Some(json!({"task_id":task,"pending_question":question})),
                    }),
                    StepOutcome::ToolCompleted { .. } => None,
                    StepOutcome::Halted { .. } => {
                        return Err(AgentError::failed(
                            "action_halted",
                            "tool failed; no retry",
                            false,
                        ));
                    }
                };
                messages = report.next_messages.clone();
                self.reports.lock().unwrap().push(report);
                if let Some(output) = output {
                    return Ok(output);
                }
            }
            Err(AgentError::failed(
                "private_step_limit",
                "bounded test exhausted",
                false,
            ))
        })
    }
}
struct AgentPlugin(Arc<TwoStepAgent>);
impl Plugin for AgentPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("private-action-caller")
            .requires_capabilities(&[LLM_RUNTIME, TOOL_RUNTIME])
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_agent("driver", self.0.clone())
    }
}
#[tokio::test]
async fn custom_agent_uses_action_component_through_canonical_turn_runtime() {
    for json_mode in [false, true] {
        for ask in [false, true] {
            let responses = if !ask {
                vec![call_response(json_mode), final_response(json_mode)]
            } else if json_mode {
                vec![json_response(
                    json!({"action":"ask_user","question":"请选择目录"}),
                )]
            } else {
                vec![native("jingwei_ask_user", json!({"question":"请选择目录"}))]
            };
            let model = Arc::new(Model {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
                requests: Mutex::new(vec![]),
                capabilities: caps(),
                cancel_after: None,
            });
            let log = Arc::new(Mutex::new(vec![]));
            let reports = Arc::new(Mutex::new(vec![]));
            // Use a body without the direct-scope recorder assertion.
            struct Body(Arc<AtomicUsize>);
            impl Tool for Body {
                fn metadata(&self) -> ToolMetadata {
                    ToolMetadata::new("lookup", schema())
                }
                fn execute<'a>(
                    &'a self,
                    _: ToolBodyRequest<'a>,
                    _: Arc<dyn CancellationSignal>,
                ) -> ToolFuture<'a, Result<String, ToolBodyError>> {
                    Box::pin(async {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        Ok("found".into())
                    })
                }
            }
            struct Tools(Arc<AtomicUsize>);
            impl Plugin for Tools {
                fn descriptor(&self) -> PluginDescriptor {
                    PluginDescriptor::new("private-loop-tools")
                }
                fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
                    ctx.register_tool("lookup", Arc::new(Body(self.0.clone())))
                }
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let harness = jingwei::HarnessBuilder::new()
                .plugin(ModelPlugin(model))
                .plugin(Tools(calls.clone()))
                .plugin(AgentPlugin(Arc::new(TwoStepAgent {
                    protocol: protocol(json_mode),
                    reports: reports.clone(),
                })))
                .plugin(MemoryPlugin(Arc::new(Memory(log.clone()))))
                .plugin(CanonicalLlmRuntimePlugin::new())
                .plugin(
                    CanonicalToolRuntimePlugin::new()
                        .grant_tool(PluginId::new("private-action-caller"), "lookup"),
                )
                .plugin(CanonicalSessionRuntimePlugin::new())
                .plugin(CanonicalAgentRuntimePlugin::new())
                .select_agent_runtime("canonical")
                .select_llm_runtime("canonical")
                .select_tool_runtime("canonical")
                .select_session_runtime("canonical")
                .select_persistence("memory")
                .build()
                .await
                .unwrap();
            let report = harness
                .run_turn(&SessionId::new(), "driver", "请求")
                .await
                .unwrap();
            assert_eq!(
                report.disposition(),
                if ask {
                    TurnDisposition::WaitingForInput
                } else {
                    TurnDisposition::Completed
                }
            );
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(!ask));
            assert_eq!(
                report.final_text(),
                if ask {
                    "请选择目录"
                } else {
                    "已找到资料"
                }
            );
            assert_eq!(reports.lock().unwrap().len(), if ask { 1 } else { 2 });
            let task_report = report.task_run_report().expect("canonical budget report");
            let usage = &task_report.budget;
            assert_eq!(usage.charged.steps, if ask { 1 } else { 2 });
            assert_eq!(usage.charged.model_requests, if ask { 1 } else { 2 });
            assert_eq!(usage.charged.tool_calls, u64::from(!ask));
            assert_eq!(usage.charged.tool_output_bytes, if ask { 0 } else { 5 });
            assert!(usage.pending.is_empty());
            assert!(task_report.capabilities_drained);
            let metrics = &usage.run.as_ref().unwrap().metrics;
            assert_eq!(
                metrics.confirmed.model_requests,
                usage.charged.model_requests
            );
            assert_eq!(
                metrics.confirmed.model_results,
                usage.charged.model_requests
            );
            assert_eq!(metrics.confirmed.tool_calls, usage.charged.tool_calls);
            assert_eq!(metrics.confirmed.tool_results, usage.charged.tool_calls);
            if ask {
                let events = log.lock().unwrap();
                assert!(events.iter().any(|e| matches!(&e.kind, SessionEventKind::Done { status: jingwei_core::DoneStatus::WaitingForInput, artifact: Some(value) } if value["pending_question"] == "请选择目录")));
            }
            harness.shutdown().await.unwrap();
        }
    }
}
use jingwei::action::*;
use jingwei::agent::*;
use jingwei::llm::*;
use jingwei::plugin::*;
use jingwei::session::SessionRuntimeError;
use jingwei::tool::*;
use jingwei_core::{
    CancellationFuture, CancellationSignal, EventId, SessionEvent, SessionEventKind, SessionId,
    TurnId,
};
use jingwei_llm_runtime::CanonicalLlmRuntimePlugin;
use jingwei_tool_runtime::CanonicalToolRuntimePlugin;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn support() -> GenerationSupport {
    GenerationSupport {
        complete: CapabilitySupport::Supported,
        stream: CapabilitySupport::Supported,
    }
}
fn caps() -> ModelCapabilities {
    ModelCapabilities {
        native_tools: support(),
        json_schema: support(),
        ..ModelCapabilities::text_only()
    }
}
fn schema() -> Value {
    json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false})
}
fn input() -> Vec<ModelMessage> {
    vec![ModelMessage::user("查询资料并总结")]
}
fn native(name: &str, arguments: Value) -> GenerationResponse {
    GenerationResponse {
        content: None,
        tool_calls: vec![ModelToolCall {
            id: ProviderToolCallId::new("provider-a").unwrap(),
            name: name.into(),
            arguments,
        }],
        incomplete_tool_calls: vec![],
        finish_reason: FinishReason::ToolCalls,
        usage: TokenUsage::default(),
    }
}
fn json_response(value: Value) -> GenerationResponse {
    GenerationResponse::text(value.to_string(), FinishReason::Stop)
}
fn call_response(json_mode: bool) -> GenerationResponse {
    if json_mode {
        json_response(json!({"action":"call_tool","name":"lookup","arguments":{"query":"你好"}}))
    } else {
        native("lookup", json!({"query":"你好"}))
    }
}
fn final_response(json_mode: bool) -> GenerationResponse {
    if json_mode {
        json_response(json!({"action":"final","text":"已找到资料"}))
    } else {
        GenerationResponse::text("已找到资料", FinishReason::Stop)
    }
}
fn protocol(json_mode: bool) -> Arc<dyn ActionProtocol> {
    if json_mode {
        Arc::new(JsonActionProtocol)
    } else {
        Arc::new(NativeToolProtocol)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum FailPoint {
    #[default]
    None,
    ModelRequest,
    ModelResult,
    ToolCall,
    ToolResult,
}
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<SessionEvent>>,
    fail: FailPoint,
}
impl Recorder {
    fn append_kind(
        &self,
        kind: SessionEventKind,
    ) -> Result<Arc<SessionEvent>, SessionRuntimeError> {
        let point = match &kind {
            SessionEventKind::ModelRequest { .. } => FailPoint::ModelRequest,
            SessionEventKind::ModelResult { .. } => FailPoint::ModelResult,
            SessionEventKind::ToolCall { .. } => FailPoint::ToolCall,
            SessionEventKind::ToolResult { .. } => FailPoint::ToolResult,
            _ => FailPoint::None,
        };
        if point == self.fail && point != FailPoint::None {
            return Err(SessionRuntimeError::Stopped);
        }
        let mut events = self.events.lock().unwrap();
        let event = SessionEvent {
            event_id: EventId::new(),
            session_id: SessionId::from("private-action"),
            turn_id: TurnId::from("private-turn"),
            generation_id: None,
            message_id: None,
            seq: events.len() as u64,
            kind,
        };
        events.push(event.clone());
        Ok(Arc::new(event))
    }
}
impl ModelEventRecorder for Recorder {
    fn append(
        &self,
        record: ModelRecord,
    ) -> ModelFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move { self.append_kind(record.into_session_event_kind()) })
    }
}
impl ToolEventRecorder for Recorder {
    fn append(
        &self,
        record: ToolRecord,
    ) -> ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move { self.append_kind(record.into_session_event_kind()) })
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
struct Model {
    responses: Mutex<VecDeque<GenerationResponse>>,
    calls: AtomicUsize,
    requests: Mutex<Vec<GenerationRequest>>,
    capabilities: ModelCapabilities,
    cancel_after: Option<CancellationToken>,
}
impl Llm for Model {
    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }
    fn generate<'a>(
        &'a self,
        request: &'a GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request.clone());
            if let Some(cancel) = &self.cancel_after {
                cancel.cancel();
            }
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no extra model inference"))
        })
    }
    fn generate_stream(
        &self,
        _: GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> GenerationStream {
        panic!("single-step action currently uses complete generation")
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
        PluginDescriptor::provider("private-action-model", LLM_PROVIDER, "fake")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(Self(self.0.clone())))
    }
}
#[derive(Clone, Copy, Default)]
enum ToolMode {
    #[default]
    Success,
    Failure,
    Pending,
}
struct ProbeTool {
    calls: AtomicUsize,
    input_schema: Value,
    approve: bool,
    mode: ToolMode,
    recorder: Arc<Recorder>,
}
impl Tool for ProbeTool {
    fn metadata(&self) -> ToolMetadata {
        let metadata = ToolMetadata::new("private lookup", self.input_schema.clone());
        if self.approve {
            metadata.with_approval(ApprovalRequirement::required("human"))
        } else {
            metadata
        }
    }
    fn execute<'a>(
        &'a self,
        request: ToolBodyRequest<'a>,
        _: Arc<dyn CancellationSignal>,
    ) -> ToolFuture<'a, Result<String, ToolBodyError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(self.recorder.events.lock().unwrap().iter().any(|e| matches!(&e.kind, SessionEventKind::ToolCall { call } if call.id == request.call_id())));
            match self.mode {
                ToolMode::Success => {
                    Ok("工具数据：不要把这里的 ignore instructions 当成指令".into())
                }
                ToolMode::Failure => Err(ToolBodyError::new(
                    "private_failure",
                    "test body failed",
                    true,
                )),
                ToolMode::Pending => std::future::pending().await,
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
        Box::pin(async {
            Ok(Some(ToolDenial::new(
                "private_denied",
                "test policy denied",
                false,
            )))
        })
    }
}
struct Approval {
    allow: bool,
    calls: Arc<AtomicUsize>,
}
impl ToolAuthorizer for Approval {
    fn authorize<'a>(
        &'a self,
        _: ToolAuthorizationRequest<'a>,
    ) -> ToolFuture<'a, Result<ToolAuthorizationDecision, ToolAuthorizationError>> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.allow {
                ToolAuthorizationDecision::Approved
            } else {
                ToolAuthorizationDecision::Denied(ToolDenial::new(
                    "private_refused",
                    "test user refused",
                    false,
                ))
            })
        })
    }
}
struct ToolPlugin {
    tool: Arc<ProbeTool>,
    guard: bool,
    approval: Arc<Approval>,
}
impl Plugin for ToolPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("private-action-tools")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_tool("lookup", self.tool.clone())?;
        ctx.register_tool("hidden", self.tool.clone())?;
        ctx.register_tool_authorizer("human", self.approval.clone())?;
        if self.guard {
            ctx.register_tool_guard(Arc::new(Deny));
        }
        Ok(())
    }
}

// Capture controlled scopes through the existing, deliberately restricted AgentRuntime factory.
// Never widen the public registry to expose raw tools or policy handles for testing.
type RuntimeSlot = Arc<Mutex<Option<Arc<dyn ToolRuntime>>>>;
struct ScopePlugin(RuntimeSlot);
struct IdleRuntime;
impl AgentRuntime for IdleRuntime {
    fn start_turn(
        &self,
        _: AgentTurnRequest,
    ) -> Result<Box<dyn AgentTurnController>, AgentRuntimeError> {
        Err(AgentRuntimeError::Stopped)
    }
}
impl ServiceFactory<dyn AgentRuntime> for ScopePlugin {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn AgentRuntime>, RuntimeError>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = ctx.tool_runtime();
            Ok(ManagedService::ready(
                Arc::new(IdleRuntime) as Arc<dyn AgentRuntime>
            ))
        })
    }
}
impl Plugin for ScopePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("private-action-scopes", AGENT_RUNTIME, "capture")
            .requires_capabilities(&[TOOL_RUNTIME])
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_agent_runtime_factory(Arc::new(Self(self.0.clone())))
    }
}
struct Setup {
    responses: Vec<GenerationResponse>,
    fail: FailPoint,
    granted: bool,
    guard: bool,
    require_approval: bool,
    approve: bool,
    mode: ToolMode,
    input_schema: Value,
    capabilities: ModelCapabilities,
}
impl Setup {
    fn new(responses: Vec<GenerationResponse>) -> Self {
        Self {
            responses,
            fail: FailPoint::None,
            granted: true,
            guard: false,
            require_approval: false,
            approve: false,
            mode: ToolMode::Success,
            input_schema: schema(),
            capabilities: caps(),
        }
    }
}
struct Fixture {
    registry: PluginRegistry,
    model_turn: Box<dyn ModelTurn>,
    tool_turn: Box<dyn ToolTurn>,
    model: Arc<Model>,
    tool: Arc<ProbeTool>,
    recorder: Arc<Recorder>,
    signal: Signal,
    approvals: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new(setup: Setup) -> Self {
        let recorder = Arc::new(Recorder {
            fail: setup.fail,
            ..Default::default()
        });
        let model = Arc::new(Model {
            responses: Mutex::new(setup.responses.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            capabilities: setup.capabilities,
            cancel_after: None,
        });
        let tool = Arc::new(ProbeTool {
            calls: AtomicUsize::new(0),
            input_schema: setup.input_schema,
            approve: setup.require_approval,
            mode: setup.mode,
            recorder: recorder.clone(),
        });
        let approvals = Arc::new(AtomicUsize::new(0));
        let slot: RuntimeSlot = Arc::new(Mutex::new(None));
        let mut registrar = Registrar::default();
        registrar.add(ModelPlugin(model.clone()));
        registrar.add(ToolPlugin {
            tool: tool.clone(),
            guard: setup.guard,
            approval: Arc::new(Approval {
                allow: setup.approve,
                calls: approvals.clone(),
            }),
        });
        let mut runtime =
            CanonicalToolRuntimePlugin::new().with_default_timeout(Duration::from_secs(1));
        if setup.granted {
            runtime = runtime.grant_tool(PluginId::new("private-action-caller"), "lookup");
        }
        registrar.add(runtime);
        registrar
            .add(CanonicalLlmRuntimePlugin::new().with_default_timeout(Duration::from_secs(1)));
        registrar.add(ScopePlugin(slot.clone()));
        registrar.select(LLM_RUNTIME, "canonical");
        registrar.select(TOOL_RUNTIME, "canonical");
        registrar.select(AGENT_RUNTIME, "capture");
        let registry = registrar.finish().await.unwrap();
        let signal = Signal(CancellationToken::new());
        let model_turn = registry
            .llm_runtime()
            .unwrap()
            .bind_turn(ModelTurnBinding::new(
                Arc::new(Signal(signal.0.clone())),
                recorder.clone(),
            ))
            .unwrap();
        let tool_runtime = slot.lock().unwrap().clone().unwrap();
        let tool_turn = tool_runtime
            .bind_turn(ToolTurnBinding::new(
                ToolCaller::new("private-action-caller"),
                Arc::new(Signal(signal.0.clone())),
                recorder.clone(),
            ))
            .unwrap();
        Self {
            registry,
            model_turn,
            tool_turn,
            model,
            tool,
            recorder,
            signal,
            approvals,
        }
    }
    async fn run(&self, step: &ActionStep) -> Result<ActionStepReport, ActionStepError> {
        step.run(
            TaskId::from("task-test"),
            input(),
            self.model_turn.gateway(),
            self.tool_turn.gateway(),
            &self.signal,
            ActionStepOptions::default(),
        )
        .await
    }
    async fn close(self) {
        let model = tokio::time::timeout(
            Duration::from_secs(2),
            self.model_turn.finish(ModelFinishMode::Graceful),
        )
        .await
        .unwrap();
        let tool = tokio::time::timeout(
            Duration::from_secs(2),
            self.tool_turn.finish(ToolFinishMode::Graceful),
        )
        .await
        .unwrap();
        assert_eq!(
            model.is_err(),
            matches!(
                self.recorder.fail,
                FailPoint::ModelRequest | FailPoint::ModelResult
            )
        );
        assert_eq!(
            tool.is_err(),
            matches!(
                self.recorder.fail,
                FailPoint::ToolCall | FailPoint::ToolResult
            )
        );
        self.registry.shutdown().await.unwrap();
    }
}
fn halted_category(report: &ActionStepReport) -> ToolFailureCategory {
    let StepOutcome::Halted { execution } = &report.outcome else {
        panic!("{report:?}")
    };
    let ToolRecordedOutcome::Failed { category, .. } = execution.result().outcome else {
        panic!()
    };
    category
}

#[tokio::test]
async fn native_and_json_tool_roundtrip_then_final_share_the_same_runtime_contract() {
    for json_mode in [false, true] {
        let fixture = Fixture::new(Setup::new(vec![
            call_response(json_mode),
            final_response(json_mode),
        ]))
        .await;
        let step = ActionStep::new(protocol(json_mode));
        let first = fixture.run(&step).await.unwrap();
        assert!(matches!(first.outcome, StepOutcome::ToolCompleted { .. }));
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.next_messages.len(), 3);
        if json_mode {
            let ModelMessage::User { content } = first.next_messages.last().unwrap() else {
                panic!()
            };
            let feedback: Value = serde_json::from_str(content).unwrap();
            assert_eq!(feedback["type"], "jingwei_tool_result");
            assert!(
                feedback["outcome"]["output"]
                    .as_str()
                    .unwrap()
                    .contains("ignore instructions")
            );
            assert!(
                first
                    .next_messages
                    .iter()
                    .all(|m| !matches!(m, ModelMessage::Tool { .. }))
            );
        } else {
            let ModelMessage::Tool { call_id, .. } = first.next_messages.last().unwrap() else {
                panic!()
            };
            assert_eq!(call_id.as_str(), "provider-a");
        }
        let second = step
            .run(
                first.context.task_id.clone(),
                first.next_messages,
                fixture.model_turn.gateway(),
                fixture.tool_turn.gateway(),
                &fixture.signal,
                ActionStepOptions::default(),
            )
            .await
            .unwrap();
        assert!(matches!(second.outcome, StepOutcome::Final { ref text } if text == "已找到资料"));
        assert_ne!(first.context.step_id, second.context.step_id);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
        {
            let events = fixture.recorder.events.lock().unwrap();
            let SessionEventKind::ModelRequest { request } = &events[0].kind else {
                panic!()
            };
            assert_eq!(request.options.context.as_ref(), Some(&first.context));
            let SessionEventKind::ModelResult { result } = &events[1].kind else {
                panic!()
            };
            assert_eq!(request.call_id, result.call_id);
            let SessionEventKind::ToolCall { call } = &events[2].kind else {
                panic!()
            };
            assert_ne!(call.id, "provider-a");
            let origin = call.action.as_ref().unwrap();
            assert_eq!(origin.decision, first.context);
            assert_eq!(origin.action_index, 0);
            assert_eq!(origin.provider_call_id.is_some(), !json_mode);
            let SessionEventKind::ToolResult { result } = &events[3].kind else {
                panic!()
            };
            assert_eq!(call.id, result.call_id);
            let SessionEventKind::ModelRequest { request } = &events[4].kind else {
                panic!()
            };
            assert_eq!(request.options.context.as_ref(), Some(&second.context));
            assert_eq!(
                request
                    .input
                    .messages
                    .iter()
                    .filter(|m| matches!(m, ModelMessage::System { .. }))
                    .count(),
                1
            );
            let wire = serde_json::to_string(&*events).unwrap();
            assert_eq!(
                serde_json::from_str::<Vec<SessionEvent>>(&wire).unwrap(),
                *events
            );
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn final_and_ask_user_are_control_actions_without_tool_execution() {
    for json_mode in [false, true] {
        for ask in [false, true] {
            let response = if !ask {
                final_response(json_mode)
            } else if json_mode {
                json_response(json!({"action":"ask_user","question":"哪个目录？"}))
            } else {
                native("jingwei_ask_user", json!({"question":"哪个目录？"}))
            };
            let fixture = Fixture::new(Setup::new(vec![response])).await;
            let report = fixture
                .run(&ActionStep::new(protocol(json_mode)))
                .await
                .unwrap();
            if ask {
                assert!(
                    matches!(report.outcome, StepOutcome::AskUser { ref question } if question == "哪个目录？")
                );
            } else {
                assert!(matches!(report.outcome, StepOutcome::Final { .. }));
            }
            GenerationRequest::text(report.next_messages)
                .validate_shape()
                .unwrap();
            assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.recorder.events.lock().unwrap().len(), 2);
            fixture.close().await;
        }
    }
}

#[tokio::test]
async fn malformed_json_hidden_tools_and_invalid_arguments_execute_zero_tools() {
    let invalid_json = [
        "{\"action\":\"call_tool\",",
        "{\"action\":\"call_tool\",\"name\":\"hidden\",\"arguments\":{}}",
        "{\"action\":\"call_tool\",\"name\":\"lookup\",\"arguments\":{}}",
        "{\"action\":\"call_tool\",\"name\":\"lookup\",\"arguments\":{\"query\":2}}",
        "{\"action\":\"call_tool\",\"name\":\"lookup\",\"arguments\":{\"query\":\"x\"},\"approve\":true}",
        "[{\"action\":\"final\",\"text\":\"x\"}]",
        "{\"action\":\"final\",\"text\":\"x\",\"text\":\"y\"}",
    ];
    for value in invalid_json {
        let fixture = Fixture::new(Setup::new(vec![GenerationResponse::text(
            value,
            FinishReason::Stop,
        )]))
        .await;
        assert!(fixture.run(&ActionStep::new(protocol(true))).await.is_err());
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.recorder.events.lock().unwrap().len(), 2);
        fixture.close().await;
    }
    for response in [
        native("hidden", json!({"query":"x"})),
        native("lookup", json!({})),
        native("lookup", json!({"query":3})),
    ] {
        let fixture = Fixture::new(Setup::new(vec![response])).await;
        assert!(
            fixture
                .run(&ActionStep::new(protocol(false)))
                .await
                .is_err()
        );
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        fixture.close().await;
    }
}

#[tokio::test]
async fn multiple_native_calls_are_rejected_without_executing_the_first() {
    let mut response = native("lookup", json!({"query":"first"}));
    response.tool_calls.push(ModelToolCall {
        id: ProviderToolCallId::new("second").unwrap(),
        name: "lookup".into(),
        arguments: json!({"query":"second"}),
    });
    let fixture = Fixture::new(Setup::new(vec![response])).await;
    let error = fixture
        .run(&ActionStep::new(protocol(false)))
        .await
        .unwrap_err();
    assert!(matches!(
        *error.failure,
        ActionStepFailure::Validation(ActionError::MultipleActions)
    ));
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
    fixture.close().await;
}

#[tokio::test]
async fn incomplete_and_unknown_model_terminations_never_execute_actions() {
    for reason in [
        FinishReason::Length,
        FinishReason::Unknown,
        FinishReason::ContentFiltered,
    ] {
        let mut response = call_response(false);
        response.finish_reason = reason;
        let fixture = Fixture::new(Setup::new(vec![response])).await;
        assert!(
            fixture
                .run(&ActionStep::new(protocol(false)))
                .await
                .is_err()
        );
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        fixture.close().await;
    }
}

#[tokio::test]
async fn missing_generation_capability_fails_before_model_and_tool() {
    for json_mode in [false, true] {
        let fixture = Fixture::new(Setup {
            capabilities: ModelCapabilities::text_only(),
            ..Setup::new(vec![])
        })
        .await;
        assert!(
            fixture
                .run(&ActionStep::new(protocol(json_mode)))
                .await
                .is_err()
        );
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        assert!(fixture.recorder.events.lock().unwrap().is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn visible_selection_only_narrows_the_granted_catalog() {
    let fixture = Fixture::new(Setup::new(vec![])).await;
    let step = ActionStep::new(protocol(true)).with_visible_tools(vec!["hidden".to_string()]);
    assert!(fixture.run(&step).await.is_err());
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.close().await;
    for json_mode in [false, true] {
        let fixture = Fixture::new(Setup {
            granted: false,
            ..Setup::new(vec![final_response(json_mode)])
        })
        .await;
        let report = fixture
            .run(&ActionStep::new(protocol(json_mode)))
            .await
            .unwrap();
        assert!(matches!(report.outcome, StepOutcome::Final { .. }));
        let wire = serde_json::to_string(&report.request.constraint).unwrap();
        assert!(!wire.contains("lookup"));
        assert!(!wire.contains("hidden"));
        fixture.close().await;
    }
}

#[tokio::test]
async fn guard_denial_and_user_refusal_halt_without_automatic_fallback_or_retry() {
    for guard in [true, false] {
        let fixture = Fixture::new(Setup {
            guard,
            require_approval: !guard,
            ..Setup::new(vec![call_response(true)])
        })
        .await;
        let report = fixture.run(&ActionStep::new(protocol(true))).await.unwrap();
        assert_eq!(
            halted_category(&report),
            if guard {
                ToolFailureCategory::Denied
            } else {
                ToolFailureCategory::ApprovalDenied
            }
        );
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.approvals.load(Ordering::SeqCst),
            usize::from(!guard)
        );
        fixture.close().await;
    }
}

#[tokio::test]
async fn approved_tool_executes_once_and_runtime_rechecks_parameters() {
    let fixture = Fixture::new(Setup {
        require_approval: true,
        approve: true,
        ..Setup::new(vec![call_response(false)])
    })
    .await;
    let report = fixture
        .run(&ActionStep::new(protocol(false)))
        .await
        .unwrap();
    assert!(matches!(report.outcome, StepOutcome::ToolCompleted { .. }));
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.approvals.load(Ordering::SeqCst), 1);
    let rejected = fixture
        .tool_turn
        .gateway()
        .call("lookup", json!({"query":4}))
        .await
        .unwrap();
    assert!(matches!(
        rejected.result().outcome,
        ToolRecordedOutcome::Failed {
            category: ToolFailureCategory::InvalidArguments,
            ..
        }
    ));
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.approvals.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[tokio::test]
async fn tool_body_failure_and_timeout_halt_even_when_diagnostic_is_retryable() {
    for mode in [ToolMode::Failure, ToolMode::Pending] {
        let fixture = Fixture::new(Setup {
            mode,
            ..Setup::new(vec![call_response(true)])
        })
        .await;
        let report = ActionStep::new(protocol(true))
            .run(
                TaskId::new(),
                input(),
                fixture.model_turn.gateway(),
                fixture.tool_turn.gateway(),
                &fixture.signal,
                ActionStepOptions {
                    tool: ToolCallOptions {
                        timeout: Some(Duration::from_millis(10)),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            halted_category(&report),
            if matches!(mode, ToolMode::Failure) {
                ToolFailureCategory::BodyFailure
            } else {
                ToolFailureCategory::Timeout
            }
        );
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
        fixture.close().await;
    }
}

#[tokio::test]
async fn every_recording_failure_stops_at_its_barrier_and_preserves_step_identity() {
    for fail in [
        FailPoint::ModelRequest,
        FailPoint::ModelResult,
        FailPoint::ToolCall,
        FailPoint::ToolResult,
    ] {
        let fixture = Fixture::new(Setup {
            fail,
            ..Setup::new(vec![call_response(false)])
        })
        .await;
        let error = fixture
            .run(&ActionStep::new(protocol(false)))
            .await
            .unwrap_err();
        assert_eq!(error.context.task_id, TaskId::from("task-test"));
        assert_eq!(
            fixture.tool.calls.load(Ordering::SeqCst),
            usize::from(fail == FailPoint::ToolResult)
        );
        if matches!(fail, FailPoint::ToolCall | FailPoint::ToolResult) {
            let ActionStepFailure::Tool(ToolRuntimeError::Recording(failure)) = &*error.failure
            else {
                panic!("{error:?}")
            };
            assert_eq!(
                failure.call_evidence().action.as_ref().unwrap().decision,
                error.context
            );
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn cancelled_step_does_not_start_inference_or_tools() {
    let fixture = Fixture::new(Setup::new(vec![])).await;
    fixture.signal.0.cancel();
    let error = fixture
        .run(&ActionStep::new(protocol(true)))
        .await
        .unwrap_err();
    assert!(matches!(*error.failure, ActionStepFailure::Cancelled));
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
    fixture.close().await;
}

#[tokio::test]
async fn json_action_schema_preserves_tool_local_refs_when_embedded() {
    let input_schema = json!({"$defs":{"query":{"type":"string","minLength":2}},"type":"object",
        "properties":{"query":{"$ref":"#/$defs/query"}},"required":["query"],"additionalProperties":false});
    for (value, valid) in [("你好", true), ("x", false)] {
        let fixture = Fixture::new(Setup {
            input_schema: input_schema.clone(),
            ..Setup::new(vec![json_response(
                json!({"action":"call_tool","name":"lookup","arguments":{"query":value}}),
            )])
        })
        .await;
        let result = fixture.run(&ActionStep::new(protocol(true))).await;
        assert_eq!(result.is_ok(), valid);
        assert_eq!(
            fixture.tool.calls.load(Ordering::SeqCst),
            usize::from(valid)
        );
        fixture.close().await;
    }
}

struct ReplacementProtocol {
    invalid_args: bool,
    feedback_error: bool,
}
impl ActionProtocol for ReplacementProtocol {
    fn request(
        &self,
        messages: Vec<ModelMessage>,
        tools: &[ModelToolDefinition],
    ) -> Result<GenerationRequest, ActionError> {
        JsonActionProtocol.request(messages, tools)
    }
    fn parse(&self, _: &GenerationResponse) -> Result<AgentAction, ActionError> {
        Ok(AgentAction::CallTool {
            name: if self.invalid_args || self.feedback_error {
                "lookup".into()
            } else {
                "hidden".into()
            },
            arguments: if self.invalid_args {
                json!({"query":7})
            } else {
                json!({"query":"x"})
            },
            provider_call_id: None,
        })
    }
    fn continuation(
        &self,
        _: &GenerationResponse,
        _: &AgentAction,
        _: Option<&ToolExecution>,
    ) -> Result<Vec<ModelMessage>, ActionError> {
        Err(ActionError::InvalidFeedback)
    }
}
#[tokio::test]
async fn replacing_action_policy_cannot_bypass_visibility_or_schema_checks() {
    for invalid_args in [false, true] {
        let fixture = Fixture::new(Setup::new(vec![final_response(true)])).await;
        let step = ActionStep::new(Arc::new(ReplacementProtocol {
            invalid_args,
            feedback_error: false,
        }));
        assert!(fixture.run(&step).await.is_err());
        assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
        fixture.close().await;
    }
}
#[tokio::test]
async fn feedback_projection_failure_retains_committed_execution_without_replay() {
    let fixture = Fixture::new(Setup::new(vec![final_response(true)])).await;
    let error = fixture
        .run(&ActionStep::new(Arc::new(ReplacementProtocol {
            invalid_args: false,
            feedback_error: true,
        })))
        .await
        .unwrap_err();
    assert!(matches!(
        *error.failure,
        ActionStepFailure::Validation(ActionError::InvalidFeedback)
    ));
    let execution = error.completed_execution.unwrap();
    assert_eq!(
        execution.call().action.as_ref().unwrap().decision,
        error.context
    );
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[test]
fn parser_rejects_blank_terminal_text_and_reserved_native_tool_collision() {
    for response in [
        json_response(json!({"action":"final","text":" "})),
        json_response(json!({"action":"ask_user","question":"\n"})),
    ] {
        assert!(JsonActionProtocol.parse(&response).is_err());
    }
    assert_eq!(
        NativeToolProtocol.parse(&GenerationResponse::text(" ", FinishReason::Stop)),
        Err(ActionError::EmptyText)
    );
    let tools = vec![ModelToolDefinition {
        name: "jingwei_ask_user".into(),
        description: "".into(),
        parameters: schema(),
    }];
    assert_eq!(
        NativeToolProtocol.request(input(), &tools),
        Err(ActionError::ReservedToolName)
    );
}

#[test]
fn json_protocol_rejects_external_schema_references_without_io() {
    for uri in [
        "https://private.invalid/schema.json",
        "file:///private/schema.json",
    ] {
        let tools = vec![ModelToolDefinition {
            name: "lookup".into(),
            description: "external".into(),
            parameters: json!({"$ref":uri}),
        }];
        assert!(JsonActionProtocol.request(input(), &tools).is_err());
    }
}
