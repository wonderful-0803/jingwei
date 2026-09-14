use super::*;
use jingwei::budget::{
    BudgetIdentity, BudgetLimits, MonotonicBudgetClock, TaskBudget, TaskRunStop,
};
use jingwei::context::*;
use jingwei::reference::*;

struct Clock;
impl ReferenceClock for Clock {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError> {
        Ok(1)
    }
}
fn config(max_steps: u32, json_mode: bool) -> ReferenceAgentConfig {
    let mut config = ReferenceAgentConfig::new(
        ContextTarget {
            model: "test".into(),
            template_revision: "v1".into(),
        },
        ContextBudget {
            window_tokens: 8192,
            output_reserve: 128,
            output_evidence: TokenBoundEvidence::Estimate,
            safety_margin: 64,
            mode: TokenBudgetMode::Soft,
        },
    );
    config.max_steps = max_steps;
    config.protocol = if json_mode {
        jingwei::action::ContextActionProtocol::Json
    } else {
        jingwei::action::ContextActionProtocol::Native
    };
    config
}
fn policies() -> ReferenceAgentPolicies {
    ReferenceAgentPolicies {
        counter: Arc::new(ByteHeuristicCounter::default()),
        selector: Arc::new(GroupedToolSelector::default()),
        result: Arc::new(BoundedResultPolicy::default()),
        store: Arc::new(
            MemoryContentStore::new(ContentStoreConfig {
                store_id: "reference".into(),
                max_entries: 128,
                max_bytes: 1024 * 1024,
                max_content_bytes: 64 * 1024,
                max_page_bytes: 4096,
                max_ttl_ms: 600_000,
            })
            .unwrap(),
        ),
        clock: Arc::new(Clock),
    }
}
struct Registered(Arc<dyn Agent>, bool);
impl Plugin for Registered {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("reference-owner").requires_capabilities(if self.1 {
            &[LLM_RUNTIME, TOOL_RUNTIME]
        } else {
            &[LLM_RUNTIME]
        })
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_agent("reference", self.0.clone())
    }
}
struct Body {
    calls: Arc<AtomicUsize>,
    mode: ToolMode,
}
impl Tool for Body {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("lookup", schema())
    }
    fn execute<'a>(
        &'a self,
        _: ToolBodyRequest<'a>,
        _: Arc<dyn CancellationSignal>,
    ) -> ToolFuture<'a, Result<String, ToolBodyError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                ToolMode::Success => Ok("found data".into()),
                ToolMode::Failure => Err(ToolBodyError::new("denied", "do not retry", false)),
                ToolMode::Pending => std::future::pending().await,
            }
        })
    }
}
struct RefTools {
    calls: Arc<AtomicUsize>,
    mode: ToolMode,
}
impl Plugin for RefTools {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("reference-tools")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_tool(
            "lookup",
            Arc::new(Body {
                calls: self.calls.clone(),
                mode: self.mode,
            }),
        )
    }
}
struct RefFixture {
    harness: jingwei::Harness,
    log: Arc<Mutex<Vec<SessionEvent>>>,
    model: Arc<Model>,
    calls: Arc<AtomicUsize>,
}
impl RefFixture {
    async fn new(
        config: ReferenceAgentConfig,
        responses: Vec<GenerationResponse>,
        with_tools: bool,
        mode: ToolMode,
    ) -> Self {
        Self::agent(
            Arc::new(ReferenceAgent::new(config, policies()).unwrap()),
            responses,
            with_tools,
            mode,
        )
        .await
    }
    async fn agent(
        agent: Arc<dyn Agent>,
        responses: Vec<GenerationResponse>,
        with_tools: bool,
        mode: ToolMode,
    ) -> Self {
        let model = Arc::new(Model {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            capabilities: caps(),
            cancel_after: None,
        });
        let log = Arc::new(Mutex::new(vec![]));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = jingwei::HarnessBuilder::new()
            .plugin(ModelPlugin(model.clone()))
            .plugin(Registered(agent, with_tools))
            .plugin(MemoryPlugin(Arc::new(Memory(log.clone()))))
            .plugin(CanonicalLlmRuntimePlugin::new())
            .plugin(CanonicalSessionRuntimePlugin::new())
            .plugin(CanonicalAgentRuntimePlugin::new())
            .select_agent_runtime("canonical")
            .select_llm_runtime("canonical")
            .select_session_runtime("canonical")
            .select_persistence("memory");
        if with_tools {
            builder = builder
                .plugin(RefTools {
                    calls: calls.clone(),
                    mode,
                })
                .plugin(
                    CanonicalToolRuntimePlugin::new()
                        .grant_tool(PluginId::new("reference-owner"), "lookup"),
                )
                .select_tool_runtime("canonical");
        }
        Self {
            harness: builder.build().await.unwrap(),
            log,
            model,
            calls,
        }
    }
    fn reports(&self) -> Vec<ReferenceRunReport> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                SessionEventKind::Custom {
                    plugin,
                    kind,
                    payload,
                } if plugin == REFERENCE_EVENT_PLUGIN && kind == REFERENCE_RUN_EVENT => {
                    Some(serde_json::from_value(payload.clone()).unwrap())
                }
                _ => None,
            })
            .collect()
    }
    fn steps(&self) -> Vec<ReferenceStepReport> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                SessionEventKind::Custom {
                    plugin,
                    kind,
                    payload,
                } if plugin == REFERENCE_EVENT_PLUGIN && kind == REFERENCE_STEP_EVENT => {
                    Some(serde_json::from_value(payload.clone()).unwrap())
                }
                _ => None,
            })
            .collect()
    }
}
#[tokio::test]
async fn public_reference_agent_answers_without_a_tool_gateway_and_marks_claim_unverified() {
    for json_mode in [false, true] {
        let fixture = RefFixture::new(
            config(3, json_mode),
            vec![final_response(json_mode)],
            false,
            ToolMode::Success,
        )
        .await;
        let report = fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "answer")
            .await
            .unwrap();
        assert_eq!(report.disposition(), TurnDisposition::Completed);
        assert_eq!(report.final_text(), "已找到资料");
        assert_eq!(report.artifact().unwrap()["business_verified"], false);
        let runs = fixture.reports();
        assert_eq!(runs[0].stop, ReferenceStop::ModelClaimedComplete);
        assert!(!runs[0].business_verified);
        assert_eq!(runs[0].confirmed_steps, 1);
        assert_eq!(report.task_run_report().unwrap().budget.charged.steps, 1);
        assert!(fixture.steps()[0].visible_tools.is_empty());
        let events = fixture.log.lock().unwrap().clone();
        let run=events.iter().position(|e|matches!(&e.kind,SessionEventKind::Custom{kind,..} if kind==REFERENCE_RUN_EVENT)).unwrap();
        let complete = events
            .iter()
            .position(|e| matches!(e.kind, SessionEventKind::AssistantMessage { .. }))
            .unwrap();
        assert!(run < complete);
        fixture.harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn public_reference_agent_runs_dependent_tools_without_application_loop() {
    for json_mode in [false, true] {
        let fixture = RefFixture::new(
            config(3, json_mode),
            vec![
                call_response(json_mode),
                call_response(json_mode),
                final_response(json_mode),
            ],
            true,
            ToolMode::Success,
        )
        .await;
        let report = fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup twice")
            .await
            .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 3);
        assert!(
            serde_json::to_string(&fixture.model.requests.lock().unwrap()[1].messages)
                .unwrap()
                .contains("found data")
        );
        assert_eq!(report.task_run_report().unwrap().budget.charged.steps, 3);
        assert_eq!(
            report.task_run_report().unwrap().budget.charged.tool_calls,
            2
        );
        let steps = fixture.steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].tool_events.len(), 2);
        assert_ne!(steps[0].decision.step_id, steps[1].decision.step_id);
        assert_eq!(
            fixture.reports()[0].stop,
            ReferenceStop::ModelClaimedComplete
        );
        fixture.harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn finite_limit_stops_repeated_successful_calls_without_extra_inference() {
    let fixture = RefFixture::new(
        config(2, false),
        vec![call_response(false), call_response(false)],
        true,
        ToolMode::Success,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "repeat")
            .await
            .is_err()
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.reports()[0].stop, ReferenceStop::StepLimit);
    assert_eq!(fixture.reports()[0].confirmed_steps, 2);
    assert!(
        !fixture
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::AssistantMessage { .. }))
    );
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn invalid_action_and_semantic_tool_failure_halt_without_fallback() {
    let fixture = RefFixture::new(
        config(8, false),
        vec![native("root_shell", json!({}))],
        true,
        ToolMode::Success,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "escalate")
            .await
            .is_err()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.log.lock().unwrap().iter().any(|event| matches!(&event.kind, SessionEventKind::Error { code, .. } if code == "model_protocol")));
    fixture.harness.shutdown().await.unwrap();
    let fixture = RefFixture::new(
        config(8, false),
        vec![call_response(false)],
        true,
        ToolMode::Failure,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup")
            .await
            .is_err()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.steps()[0].outcome, ReferenceStepOutcome::ToolFailed);
    assert_eq!(fixture.reports()[0].stop, ReferenceStop::ToolHalted);
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn ask_user_closes_turn_and_host_reuses_same_task_budget_on_reply() {
    let fixture = RefFixture::new(
        config(4, true),
        vec![
            json_response(json!({"action":"ask_user","question":"哪个目录？"})),
            final_response(true),
        ],
        false,
        ToolMode::Success,
    )
    .await;
    let session = SessionId::new();
    let limits = BudgetLimits::default();
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session.clone(),
            agent_key: "reference".into(),
        },
        limits,
        limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    let first = fixture
        .harness
        .start_turn_request(
            AgentTurnRequest::new(session.clone(), "reference", "find")
                .with_budget(task.clone(), limits),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(first.disposition(), TurnDisposition::WaitingForInput);
    assert_eq!(first.artifact().unwrap()["pending_question"], "哪个目录？");
    let second = fixture
        .harness
        .start_turn_request(
            AgentTurnRequest::new(session, "reference", "目录 A").with_budget(task, limits),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        second
            .task_run_report()
            .unwrap()
            .budget
            .charged
            .model_requests,
        2
    );
    assert_eq!(fixture.reports()[0].task_id, fixture.reports()[1].task_id);
    assert!(
        serde_json::to_string(&fixture.model.requests.lock().unwrap()[1])
            .unwrap()
            .contains("哪个目录？")
    );
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn task_budget_stops_before_reference_step_limit_and_runtime_retains_final_reason() {
    let fixture = RefFixture::new(
        config(8, false),
        vec![call_response(false)],
        true,
        ToolMode::Success,
    )
    .await;
    let session = SessionId::new();
    let mut limits = BudgetLimits::default();
    limits.resources.model_requests = 1;
    limits.resources.steps = 1;
    let task = TaskBudget::new(
        BudgetIdentity {
            task_id: TaskId::new(),
            session_id: session.clone(),
            agent_key: "reference".into(),
        },
        limits,
        limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    assert!(
        fixture
            .harness
            .start_turn_request(
                AgentTurnRequest::new(session, "reference", "repeat").with_budget(task, limits)
            )
            .unwrap()
            .wait()
            .await
            .is_err()
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    assert!(fixture.log.lock().unwrap().iter().any(|e|matches!(&e.kind,SessionEventKind::TaskRunReport{report} if matches!(report.stop,TaskRunStop::Budget(_))&&report.capabilities_drained)));
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn cancellation_drains_pending_tool_and_never_starts_another_step() {
    let fixture = RefFixture::new(
        config(8, false),
        vec![call_response(false)],
        true,
        ToolMode::Pending,
    )
    .await;
    let controller = fixture
        .harness
        .start_turn(&SessionId::new(), "reference", "wait")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    controller.canceller().cancel();
    let report = controller.wait().await.unwrap();
    assert_eq!(report.disposition(), TurnDisposition::Cancelled);
    assert!(report.task_run_report().unwrap().capabilities_drained);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    assert!(report.events().iter().any(|e|matches!(&e.kind,SessionEventKind::ToolResult{result} if matches!(result.outcome,ToolRecordedOutcome::Failed{category:ToolFailureCategory::Cancelled,..}))));
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn report_limit_failure_does_not_turn_model_claim_into_success() {
    let mut settings = config(3, false);
    settings.max_report_bytes = 1;
    let fixture = RefFixture::new(
        settings,
        vec![final_response(false)],
        false,
        ToolMode::Success,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "answer")
            .await
            .is_err()
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    assert!(
        !fixture
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::AssistantMessage { .. }))
    );
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn custom_agent_remains_replaceable_with_the_same_runtime() {
    struct Custom;
    impl Agent for Custom {
        fn run_turn<'a>(
            &'a self,
            input: AgentTurnInput<'a>,
            _: &'a dyn AgentContext,
        ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
            Box::pin(async move {
                Ok(AgentTurnOutput {
                    final_text: input.user_message.into(),
                    outcome: TurnOutcome::Completed,
                    artifact: None,
                })
            })
        }
    }
    let fixture = RefFixture::agent(Arc::new(Custom), vec![], false, ToolMode::Success).await;
    let report = fixture
        .harness
        .run_turn(&SessionId::new(), "reference", "custom")
        .await
        .unwrap();
    assert_eq!(report.final_text(), "custom");
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.reports().is_empty());
    fixture.harness.shutdown().await.unwrap();
}
#[test]
fn invalid_finite_configuration_is_rejected() {
    assert!(ReferenceAgent::new(config(0, false), policies()).is_err());
    assert!(ReferenceAgent::new(config(1025, false), policies()).is_err());
}

struct GatedAgent {
    agent: ReferenceAgent,
    fail_report: bool,
    hide_budget: bool,
}
struct GatedContext<'a> {
    inner: &'a dyn AgentContext,
    fail_report: bool,
    hide_budget: bool,
}
impl AgentContext for GatedContext<'_> {
    fn emit(
        &self,
        kind: jingwei::agent::AgentEventKind,
    ) -> AgentFuture<'_, Result<(), AgentError>> {
        if self.fail_report
            && matches!(&kind,jingwei::agent::AgentEventKind::Custom{kind,..} if kind==REFERENCE_STEP_EVENT)
        {
            Box::pin(async { Err(AgentError::Session(SessionRuntimeError::Stopped)) })
        } else {
            self.inner.emit(kind)
        }
    }
    fn model(&self) -> Option<&dyn ModelGateway> {
        self.inner.model()
    }
    fn tools(&self) -> Option<&dyn ToolGateway> {
        self.inner.tools()
    }
    fn cancellation(&self) -> &dyn CancellationSignal {
        self.inner.cancellation()
    }
    fn budget(&self) -> Option<&dyn jingwei::agent::AgentBudget> {
        if self.hide_budget {
            None
        } else {
            self.inner.budget()
        }
    }
}
impl Agent for GatedAgent {
    fn run_turn<'a>(
        &'a self,
        input: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            self.agent
                .run_turn(
                    input,
                    &GatedContext {
                        inner: ctx,
                        fail_report: self.fail_report,
                        hide_budget: self.hide_budget,
                    },
                )
                .await
        })
    }
}
#[tokio::test]
async fn report_write_failure_preserves_canonical_tool_evidence_and_prevents_next_step() {
    let agent = GatedAgent {
        agent: ReferenceAgent::new(config(4, false), policies()).unwrap(),
        fail_report: true,
        hide_budget: false,
    };
    let fixture = RefFixture::agent(
        Arc::new(agent),
        vec![call_response(false)],
        true,
        ToolMode::Success,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup")
            .await
            .is_err()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::ToolResult { .. }))
    );
    assert!(fixture.steps().is_empty());
    assert!(
        !fixture
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::AssistantMessage { .. }))
    );
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn missing_shared_budget_is_rejected_before_any_model_or_tool_call() {
    let agent = GatedAgent {
        agent: ReferenceAgent::new(config(4, false), policies()).unwrap(),
        fail_report: false,
        hide_budget: true,
    };
    let fixture = RefFixture::agent(Arc::new(agent), vec![], true, ToolMode::Success).await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup")
            .await
            .is_err()
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.log.lock().unwrap().iter().any(
        |e| matches!(&e.kind,SessionEventKind::Error{code,..} if code=="reference_budget_required")
    ));
    fixture.harness.shutdown().await.unwrap();
}
struct BackwardsClock(AtomicUsize);
impl ReferenceClock for BackwardsClock {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError> {
        Ok(if self.0.fetch_add(1, Ordering::SeqCst) < 2 {
            10
        } else {
            9
        })
    }
}
#[tokio::test]
async fn clock_regression_and_context_rejection_stop_without_extra_inference() {
    let mut policy = policies();
    policy.clock = Arc::new(BackwardsClock(AtomicUsize::new(0)));
    let fixture = RefFixture::agent(
        Arc::new(ReferenceAgent::new(config(4, false), policy).unwrap()),
        vec![call_response(false)],
        true,
        ToolMode::Success,
    )
    .await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup")
            .await
            .is_err()
    );
    assert_eq!(fixture.reports()[0].stop, ReferenceStop::ClockFailure);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    fixture.harness.shutdown().await.unwrap();
    let mut settings = config(4, false);
    settings.context_budget.window_tokens = 1;
    let fixture = RefFixture::new(settings, vec![], false, ToolMode::Success).await;
    assert!(
        fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "answer")
            .await
            .is_err()
    );
    assert_eq!(fixture.reports()[0].stop, ReferenceStop::ContextRejected);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.harness.shutdown().await.unwrap();
}
