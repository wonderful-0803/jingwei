//! AT-F3 integration examples: application assembly, never an application tool loop.
use super::*;
use jingwei::budget::{BudgetAmounts, BudgetEventCounts};
use jingwei_core::DoneStatus;
use std::collections::{BTreeMap, BTreeSet};

/// Audit committed canonical evidence, independently of Agent body reports.
/// Used by every reference fixture after its turns have stopped.
pub(super) fn assert_closed(events: &[SessionEvent]) {
    assert!(!events.is_empty());
    let mut sessions = BTreeMap::<String, u64>::new();
    let mut turns = BTreeMap::<String, Vec<&SessionEvent>>::new();
    let mut ids = BTreeSet::new();
    for event in events {
        let seq = sessions
            .entry(event.session_id.to_string())
            .or_insert(event.seq);
        assert_eq!(event.seq, *seq);
        *seq += 1;
        assert!(ids.insert(event.event_id.clone()));
        turns
            .entry(event.turn_id.to_string())
            .or_default()
            .push(event);
    }
    for events in turns.values() {
        let mut models = BTreeSet::new();
        let mut tools = BTreeSet::new();
        let mut counts = BudgetEventCounts::default();
        let mut settlements = 0;
        let mut terminals = 0;
        for (index, event) in events.iter().enumerate() {
            match &event.kind {
                SessionEventKind::ModelRequest { request } => {
                    assert_eq!(settlements, 0);
                    assert!(models.insert(request.call_id.clone()));
                    counts.model_requests += 1;
                }
                SessionEventKind::ModelResult { result } => {
                    assert_eq!(settlements, 0);
                    assert!(models.remove(&result.call_id));
                    counts.model_results += 1;
                }
                SessionEventKind::ToolCall { call } => {
                    assert_eq!(settlements, 0);
                    assert!(tools.insert(call.id.clone()));
                    counts.tool_calls += 1;
                }
                SessionEventKind::ToolResult { result } => {
                    assert_eq!(settlements, 0);
                    assert!(tools.remove(&result.call_id));
                    counts.tool_results += 1;
                }
                SessionEventKind::TaskRunReport { report } => {
                    settlements += 1;
                    assert!(models.is_empty() && tools.is_empty());
                    assert!(report.capabilities_drained);
                    assert_eq!(report.budget.reserved, BudgetAmounts::default());
                    assert!(report.budget.pending.is_empty());
                    let run = report.budget.run.as_ref().unwrap();
                    assert!(!run.open);
                    assert_eq!(run.turn_id.as_ref(), Some(&event.turn_id));
                    assert_eq!(run.metrics.confirmed, counts);
                    assert_eq!(run.metrics.unconfirmed, BudgetEventCounts::default());
                    assert_eq!(run.charged.model_requests, counts.model_requests);
                    assert_eq!(run.charged.tool_calls, counts.tool_calls);
                    let last = &events.last().unwrap().kind;
                    match report.stop {
                        TaskRunStop::Completed => assert!(matches!(
                            last,
                            SessionEventKind::Done {
                                status: DoneStatus::Completed,
                                ..
                            }
                        )),
                        TaskRunStop::Checkpointed => assert!(matches!(
                            last,
                            SessionEventKind::Done {
                                status: DoneStatus::Checkpointed,
                                ..
                            }
                        )),
                        TaskRunStop::WaitingForInput => assert!(matches!(
                            last,
                            SessionEventKind::Done {
                                status: DoneStatus::WaitingForInput,
                                ..
                            }
                        )),
                        TaskRunStop::CallerCancelled | TaskRunStop::RuntimeStopping => {
                            assert!(matches!(
                                last,
                                SessionEventKind::Done {
                                    status: DoneStatus::Cancelled,
                                    ..
                                }
                            ))
                        }
                        TaskRunStop::Failed | TaskRunStop::Budget(_) => {
                            assert!(matches!(last, SessionEventKind::Error { .. }))
                        }
                    }
                }
                SessionEventKind::Done { .. } | SessionEventKind::Error { .. } => {
                    terminals += 1;
                    assert_eq!(index, events.len() - 1);
                    assert_eq!(settlements, 1);
                }
                SessionEventKind::Custom {
                    plugin,
                    kind,
                    payload,
                } if plugin == REFERENCE_EVENT_PLUGIN && kind == REFERENCE_STEP_EVENT => {
                    assert_eq!(settlements, 0);
                    let step: ReferenceStepReport =
                        serde_json::from_value(payload.clone()).unwrap();
                    for source in step.tool_events {
                        assert!(events[..index].iter().any(|e| e.event_id == source
                            && matches!(
                                e.kind,
                                SessionEventKind::ToolCall { .. }
                                    | SessionEventKind::ToolResult { .. }
                            )));
                    }
                }
                _ => {}
            }
        }
        assert_eq!(settlements, 1);
        assert_eq!(terminals, 1);
    }
}

#[tokio::test]
async fn public_single_tool_path_has_one_execution_and_queryable_closed_evidence() {
    for json_mode in [false, true] {
        let fixture = RefFixture::new(
            config(3, json_mode),
            vec![call_response(json_mode), final_response(json_mode)],
            true,
            ToolMode::Success,
        )
        .await;
        let session = SessionId::new();
        let report = fixture
            .harness
            .run_turn(&session, "reference", "lookup once")
            .await
            .unwrap();
        let persisted = Memory(fixture.log.clone()).load(&session).await.unwrap();
        assert_eq!(persisted, report.events());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
        assert_closed(&persisted);
        fixture.harness.shutdown().await.unwrap();
    }
}

struct ExampleModel {
    json_mode: bool,
    calls: AtomicUsize,
}
fn proposal(json_mode: bool, name: &str, arguments: Value) -> GenerationResponse {
    if json_mode {
        json_response(json!({"action":"call_tool","name":name,"arguments":arguments}))
    } else {
        native(name, arguments)
    }
}
impl Llm for ExampleModel {
    fn capabilities(&self) -> ModelCapabilities {
        caps()
    }
    fn generate<'a>(
        &'a self,
        request: &'a GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> LlmFuture<'a, Result<GenerationResponse, LlmError>> {
        Box::pin(async move {
            let ordinal = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match ordinal {
                0 => proposal(self.json_mode, "issue_ticket", json!({})),
                1 => {
                    // The second action can only be formed from the first result.
                    let content = match request.messages.last().unwrap() {
                        ModelMessage::Tool { content, .. } | ModelMessage::User { content } => {
                            content
                        }
                        _ => panic!("a result view must precede the dependent action"),
                    };
                    let feedback: Value = serde_json::from_str(content).unwrap();
                    let ticket = feedback["view"]["outcome"]["output"]["text"]
                        .as_str()
                        .unwrap();
                    assert!(ticket.starts_with("evt_"));
                    proposal(self.json_mode, "consume_ticket", json!({"ticket":ticket}))
                }
                2 => final_response(self.json_mode),
                _ => panic!("no extra inference or fallback"),
            })
        })
    }
    fn generate_stream(
        &self,
        _: GenerationRequest,
        _: GenerationOptions,
        _: CancellationToken,
    ) -> GenerationStream {
        panic!("complete only")
    }
}
struct ExampleProvider(Arc<ExampleModel>);
impl ServiceFactory<dyn Llm> for ExampleProvider {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn Llm>, RuntimeError>> {
        Box::pin(async { Ok(ManagedService::ready(self.0.clone() as Arc<dyn Llm>)) })
    }
}
impl Plugin for ExampleProvider {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("example-model", LLM_PROVIDER, "fake")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_llm_factory(Arc::new(Self(self.0.clone())))
    }
}
struct TicketTool {
    ticket: Arc<Mutex<Option<String>>>,
    calls: Arc<AtomicUsize>,
    consume: bool,
    approval: bool,
}
impl Tool for TicketTool {
    fn metadata(&self) -> ToolMetadata {
        let schema = if self.consume {
            json!({"type":"object","properties":{"ticket":{"type":"string"}},"required":["ticket"],"additionalProperties":false})
        } else {
            json!({"type":"object","additionalProperties":false})
        };
        let metadata = ToolMetadata::new("private ticket example", schema);
        if self.approval {
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
            let mut ticket = self.ticket.lock().unwrap();
            if self.consume {
                assert_eq!(request.arguments()["ticket"].as_str(), ticket.as_deref());
                assert!(ticket.take().is_some());
                Ok("consumed".into())
            } else {
                let issued = EventId::new().to_string();
                *ticket = Some(issued.clone());
                Ok(issued)
            }
        })
    }
}
struct DirectAgent;
impl Agent for DirectAgent {
    fn run_turn<'a>(
        &'a self,
        _: AgentTurnInput<'a>,
        ctx: &'a dyn AgentContext,
    ) -> AgentFuture<'a, Result<AgentTurnOutput, AgentError>> {
        Box::pin(async move {
            let tools = ctx.tools().unwrap();
            let first = tools
                .call_with_options("issue_ticket", json!({}), Default::default())
                .await?;
            let ToolRecordedOutcome::Succeeded { output: ticket } = &first.result().outcome else {
                return Err(AgentError::failed(
                    "ticket_failed",
                    "ticket issue failed",
                    false,
                ));
            };
            let second = tools
                .call_with_options(
                    "consume_ticket",
                    json!({"ticket":ticket}),
                    Default::default(),
                )
                .await?;
            if !matches!(
                second.result().outcome,
                ToolRecordedOutcome::Succeeded { .. }
            ) {
                return Err(AgentError::failed(
                    "consume_failed",
                    "ticket consume failed",
                    false,
                ));
            }
            Ok(AgentTurnOutput {
                final_text: "custom complete".into(),
                outcome: TurnOutcome::Completed,
                artifact: None,
            })
        })
    }
}
struct ExamplePlugin {
    agent: Arc<ReferenceAgent>,
    ticket: Arc<Mutex<Option<String>>>,
    calls: Arc<AtomicUsize>,
    approval: Arc<Approval>,
    require_approval: bool,
}
impl Plugin for ExamplePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("example-owner").requires_capabilities(&[LLM_RUNTIME, TOOL_RUNTIME])
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_agent("reference", self.agent.clone())?;
        ctx.register_agent("custom", Arc::new(DirectAgent))?;
        ctx.register_tool_authorizer("human", self.approval.clone())?;
        for (name, consume) in [("issue_ticket", false), ("consume_ticket", true)] {
            ctx.register_tool(
                name,
                Arc::new(TicketTool {
                    ticket: self.ticket.clone(),
                    calls: self.calls.clone(),
                    consume,
                    approval: self.require_approval && !consume,
                }),
            )?;
        }
        Ok(())
    }
}
async fn example(
    json_mode: bool,
    require_approval: bool,
) -> (
    jingwei::Harness,
    Arc<ExampleModel>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Memory>,
) {
    let model = Arc::new(ExampleModel {
        json_mode,
        calls: AtomicUsize::new(0),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let approvals = Arc::new(AtomicUsize::new(0));
    let memory = Arc::new(Memory(Arc::new(Mutex::new(vec![]))));
    let harness = jingwei::HarnessBuilder::new()
        .plugin(ExampleProvider(model.clone()))
        .plugin(ExamplePlugin {
            agent: Arc::new(ReferenceAgent::new(config(8, json_mode), policies()).unwrap()),
            ticket: Arc::new(Mutex::new(None)),
            calls: calls.clone(),
            approval: Arc::new(Approval {
                allow: false,
                calls: approvals.clone(),
            }),
            require_approval,
        })
        .plugin(MemoryPlugin(memory.clone()))
        .plugin(CanonicalLlmRuntimePlugin::new())
        .plugin(
            CanonicalToolRuntimePlugin::new()
                .grant_tool(PluginId::new("example-owner"), "issue_ticket")
                .grant_tool(PluginId::new("example-owner"), "consume_ticket"),
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
    (harness, model, calls, approvals, memory)
}
#[tokio::test]
async fn complete_example_uses_dynamic_tool_dependency_and_swaps_agent_on_same_runtime() {
    for json_mode in [false, true] {
        let (harness, model, calls, _, memory) = example(json_mode, false).await;
        for key in ["reference", "custom"] {
            let session = SessionId::new();
            let report = harness
                .run_turn(&session, key, "issue and consume ticket")
                .await
                .unwrap();
            assert_eq!(report.disposition(), TurnDisposition::Completed);
            let events = memory.load(&session).await.unwrap();
            assert_eq!(events, report.events());
            assert_closed(&events);
            let budget = &report.task_run_report().unwrap().budget;
            assert_eq!(budget.charged.tool_calls, 2);
            assert_eq!(
                budget.charged.model_requests,
                if key == "reference" { 3 } else { 0 }
            );
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn user_refusal_never_falls_back_to_another_granted_tool() {
    for json_mode in [false, true] {
        let (harness, model, calls, approvals, memory) = example(json_mode, true).await;
        let session = SessionId::new();
        assert!(
            harness
                .run_turn(&session, "reference", "issue ticket")
                .await
                .is_err()
        );
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(approvals.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let events = memory.load(&session).await.unwrap();
        assert_closed(&events);
        assert!(events.iter().any(|e| matches!(&e.kind, SessionEventKind::ToolResult { result } if matches!(result.outcome, ToolRecordedOutcome::Failed { category:ToolFailureCategory::ApprovalDenied, .. }))));
        assert!(!events.iter().any(|e| matches!(&e.kind, SessionEventKind::ToolCall { call } if call.name == "consume_ticket")));
        assert!(events.iter().any(|e| matches!(&e.kind, SessionEventKind::TaskRunReport { report } if report.budget.charged.corrections == 0)));
        harness.shutdown().await.unwrap();
    }
}

struct ExpiringClock(AtomicUsize);
impl ReferenceClock for ExpiringClock {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError> {
        Ok(self.0.fetch_add(1, Ordering::SeqCst) as u64)
    }
}
struct FailedClock;
impl ReferenceClock for FailedClock {
    fn now_ms(&self) -> Result<u64, ReferenceConfigError> {
        Err(ReferenceConfigError)
    }
}
struct RejectView;
impl ToolResultPolicy for RejectView {
    fn select(&self, _: &str) -> Result<ResultSelection, ViewError> {
        Err(ViewError::Invalid)
    }
}
#[tokio::test]
async fn expiry_clock_and_result_view_stops_preserve_canonical_closure() {
    for stop in [
        ReferenceStop::ContentExpired,
        ReferenceStop::ClockFailure,
        ReferenceStop::ResultViewFailed,
        ReferenceStop::InvalidAction,
    ] {
        let mut cfg = config(4, false);
        let mut policy = policies();
        let responses = match stop {
            ReferenceStop::ContentExpired => {
                cfg.content_ttl_ms = 1;
                policy.clock = Arc::new(ExpiringClock(AtomicUsize::new(0)));
                vec![]
            }
            ReferenceStop::ClockFailure => {
                policy.clock = Arc::new(FailedClock);
                vec![]
            }
            ReferenceStop::ResultViewFailed => {
                policy.result = Arc::new(RejectView);
                vec![call_response(false)]
            }
            _ => vec![GenerationResponse::text("incomplete", FinishReason::Length)],
        };
        let fixture = RefFixture::agent(
            Arc::new(ReferenceAgent::new(cfg, policy).unwrap()),
            responses,
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
        assert_eq!(fixture.reports()[0].stop, stop);
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            usize::from(stop == ReferenceStop::ResultViewFailed)
        );
        fixture.audit_closed_turns();
        fixture.harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn shutdown_drains_accepted_reference_tool_without_another_inference() {
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
    tokio::time::timeout(Duration::from_secs(2), fixture.harness.shutdown())
        .await
        .unwrap()
        .unwrap();
    let report = controller.wait().await.unwrap();
    assert_eq!(
        report.task_run_report().unwrap().stop,
        TaskRunStop::RuntimeStopping
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    fixture.audit_closed_turns();
}

#[derive(Clone)]
struct ResultFaultStore {
    memory: Arc<Memory>,
    failed: Arc<std::sync::atomic::AtomicBool>,
}
impl SessionPersistence for ResultFaultStore {
    fn load<'a>(
        &'a self,
        session: &'a SessionId,
    ) -> SessionFuture<'a, Result<Vec<SessionEvent>, SessionPersistenceError>> {
        if self.failed.load(Ordering::SeqCst) {
            Box::pin(async {
                Err(SessionPersistenceError::Io {
                    operation: "acceptance_reconcile",
                    message: "simulated unavailable reconciliation".into(),
                    certainty: CommitCertainty::Indeterminate,
                })
            })
        } else {
            self.memory.load(session)
        }
    }
    fn commit_durable<'a>(
        &'a self,
        event: &'a SessionEvent,
    ) -> SessionFuture<'a, Result<PersistAppendOutcome, SessionPersistenceError>> {
        if matches!(event.kind, SessionEventKind::ToolResult { .. }) {
            self.failed.store(true, Ordering::SeqCst);
            Box::pin(async {
                Err(SessionPersistenceError::Io {
                    operation: "acceptance_result",
                    message: "simulated unknown result commit".into(),
                    certainty: CommitCertainty::Indeterminate,
                })
            })
        } else {
            self.memory.commit_durable(event)
        }
    }
}
impl ServiceFactory<dyn SessionPersistence> for ResultFaultStore {
    fn construct<'a>(
        &'a self,
        _: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionPersistence>, RuntimeError>> {
        Box::pin(async {
            Ok(ManagedService::ready(
                Arc::new(self.clone()) as Arc<dyn SessionPersistence>
            ))
        })
    }
}
impl Plugin for ResultFaultStore {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("result-fault-store", SESSION_PERSISTENCE, "memory")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_persistence_factory(Arc::new(self.clone()))
    }
}
#[tokio::test]
async fn uncertain_tool_result_commit_never_replays_and_returns_failure_evidence() {
    let model = Arc::new(Model {
        responses: Mutex::new(vec![call_response(false)].into()),
        calls: AtomicUsize::new(0),
        requests: Mutex::new(vec![]),
        capabilities: caps(),
        cancel_after: None,
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let memory = Arc::new(Memory(Arc::new(Mutex::new(vec![]))));
    let harness = jingwei::HarnessBuilder::new()
        .plugin(ModelPlugin(model.clone()))
        .plugin(Registered(
            Arc::new(ReferenceAgent::new(config(8, false), policies()).unwrap()),
            true,
        ))
        .plugin(RefTools {
            calls: calls.clone(),
            mode: ToolMode::Success,
        })
        .plugin(ResultFaultStore {
            memory: memory.clone(),
            failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
        .plugin(CanonicalLlmRuntimePlugin::new())
        .plugin(
            CanonicalToolRuntimePlugin::new()
                .grant_tool(PluginId::new("reference-owner"), "lookup"),
        )
        .plugin(CanonicalSessionRuntimePlugin::new())
        .plugin(CanonicalAgentRuntimePlugin::new())
        .select_llm_runtime("canonical")
        .select_tool_runtime("canonical")
        .select_session_runtime("canonical")
        .select_agent_runtime("canonical")
        .select_persistence("memory")
        .build()
        .await
        .unwrap();
    let session = SessionId::new();
    let error = harness
        .run_turn(&session, "reference", "lookup")
        .await
        .unwrap_err();
    let jingwei::HarnessError::AgentRuntime(AgentRuntimeError::Turn(failure)) = error else {
        panic!("typed turn failure required");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(!failure.canonical_events_complete());
    let evidence = tool_closure_evidence(failure.drive_failure().unwrap())
        .expect("accepted tool evidence remains queryable");
    assert_eq!(evidence.stage(), ToolRecordStage::Result);
    assert_eq!(evidence.call_evidence().name, "lookup");
    let result = evidence.result_evidence().unwrap();
    assert_eq!(result.call_id, evidence.call_evidence().id);
    assert!(
        matches!(&result.outcome, ToolRecordedOutcome::Succeeded { output } if output == "found data")
    );
    assert!(matches!(
        failure.terminal_attempt(),
        TerminalAttempt::Failed(_)
    ));
    assert!(matches!(
        failure.settlement_attempt(),
        SettlementAttempt::Failed(_)
    ));
    let TaskRunReportAttempt::Failed { report, .. } = failure.task_run_report_attempt().unwrap()
    else {
        panic!("unconfirmed report must remain available to host");
    };
    assert_eq!(
        report
            .budget
            .run
            .as_ref()
            .unwrap()
            .metrics
            .unconfirmed
            .tool_results,
        1
    );
    assert_eq!(report.budget.reserved, BudgetAmounts::default());
    assert!(report.budget.pending.is_empty());
    let persisted = memory.load(&session).await.unwrap();
    assert_eq!(
        persisted
            .iter()
            .filter(|e| matches!(e.kind, SessionEventKind::ToolCall { .. }))
            .count(),
        1
    );
    assert!(!persisted.iter().any(|e| matches!(
        e.kind,
        SessionEventKind::ToolResult { .. } | SessionEventKind::Done { .. }
    )));
    harness.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_history_without_complete_assistant_message_stops_before_inference() {
    let fixture = RefFixture::new(config(4, false), vec![], false, ToolMode::Success).await;
    let session = SessionId::new();
    let old_turn = TurnId::new();
    let legacy: Vec<_> = [
        SessionEventKind::UserMessage {
            text: "old request".into(),
        },
        SessionEventKind::Done {
            status: DoneStatus::Completed,
            artifact: None,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, kind)| SessionEvent {
        event_id: EventId::new(),
        session_id: session.clone(),
        turn_id: old_turn.clone(),
        generation_id: None,
        message_id: None,
        seq: seq as u64,
        kind,
    })
    .collect();
    // Load genuine legacy facts before the runtime caches this Session.
    fixture.log.lock().unwrap().extend(legacy.clone());
    assert!(
        fixture
            .harness
            .run_turn(&session, "reference", "continue")
            .await
            .is_err()
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.reports().last().unwrap().stop,
        ReferenceStop::HistoryRejected
    );
    let events = fixture.log.lock().unwrap().clone();
    assert_eq!(&events[..2], legacy);
    assert_closed(&events[2..]);
    fixture.harness.shutdown().await.unwrap();
}

fn tool_closure_evidence(failure: &DriveFailure) -> Option<&ToolClosureFailure> {
    match failure {
        DriveFailure::Agent(AgentError::Tool(ToolRuntimeError::Recording(evidence))) => {
            Some(evidence)
        }
        DriveFailure::Agent(AgentError::CapabilityClosure(failure)) => failure
            .tool()?
            .failures()
            .iter()
            .find_map(|error| match error {
                ToolRuntimeError::Recording(evidence) => Some(evidence.as_ref()),
                _ => None,
            }),
        DriveFailure::BudgetReport { prior: Some(prior) } => tool_closure_evidence(prior),
        _ => None,
    }
}
