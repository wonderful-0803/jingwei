use super::*;
use std::{collections::BTreeMap, fs, path::PathBuf};

fn identity() -> BudgetIdentity {
    BudgetIdentity {
        task_id: TaskId::from("retry-task"),
        session_id: SessionId::from("retry-session"),
        agent_key: "retry-agent".into(),
    }
}
fn contract(effect: ToolEffect) -> ToolEffectContract {
    ToolEffectContract {
        effect,
        revision: "v1".into(),
    }
}
fn call(effect: ToolEffect) -> ToolCall {
    ToolCall {
        id: "attempt-1".into(),
        name: "probe".into(),
        arguments: json!({"ok":true}),
        action: None,
        operation: Some(Box::new(ToolOperation {
            task_id: identity().task_id,
            key: "logical-1".into(),
            contract: contract(effect),
            retry: None,
        })),
    }
}
fn event(seq: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        event_id: EventId::new(),
        session_id: identity().session_id,
        turn_id: TurnId::from("old-turn"),
        generation_id: None,
        message_id: None,
        seq,
        kind,
    }
}
fn uncertain(effect: ToolEffect) -> Vec<SessionEvent> {
    vec![event(0, SessionEventKind::ToolCall { call: call(effect) })]
}
fn review(outcome: ToolReviewOutcome) -> ToolRetryReview {
    ToolRetryReview {
        actor: "authenticated-host".into(),
        evidence: "external-receipt-1".into(),
        outcome,
    }
}
#[test]
fn uncertain_effects_require_explicit_review_and_completed_always_blocks() {
    for effect in [
        ToolEffect::Unknown,
        ToolEffect::NonIdempotent,
        ToolEffect::ReadOnly,
        ToolEffect::Idempotent,
    ] {
        let mut history = uncertain(effect);
        assert_eq!(
            inspect_tool_recovery(&history, "attempt-1").unwrap().status,
            ToolRecoveryStatus::OutcomeUnknown
        );
        let result = prepare_tool_retry(&history, "attempt-1", None);
        assert_eq!(
            result.is_ok(),
            matches!(effect, ToolEffect::ReadOnly | ToolEffect::Idempotent)
        );
        assert!(
            prepare_tool_retry(
                &history,
                "attempt-1",
                Some(review(ToolReviewOutcome::NotExecuted))
            )
            .is_ok()
        );
        assert!(matches!(
            prepare_tool_retry(
                &history,
                "attempt-1",
                Some(review(ToolReviewOutcome::Completed))
            ),
            Err(ToolRetryError::Completed)
        ));
        history.push(event(
            1,
            SessionEventKind::ToolResult {
                result: ToolResult {
                    call_id: "attempt-1".into(),
                    outcome: ToolRecordedOutcome::Succeeded {
                        output: "done".into(),
                    },
                },
            },
        ));
        assert!(matches!(
            prepare_tool_retry(
                &history,
                "attempt-1",
                Some(review(ToolReviewOutcome::NotExecuted))
            ),
            Err(ToolRetryError::Completed)
        ));
    }
}
#[test]
fn failure_retryable_flag_does_not_prove_non_execution() {
    for (category, expected) in [
        (ToolFailureCategory::BodyFailure, false),
        (ToolFailureCategory::Timeout, false),
        (ToolFailureCategory::Cancelled, false),
        (ToolFailureCategory::Budget, false),
        (ToolFailureCategory::ApprovalDenied, true),
    ] {
        let mut history = uncertain(ToolEffect::NonIdempotent);
        history.push(event(
            1,
            SessionEventKind::ToolResult {
                result: ToolResult {
                    call_id: "attempt-1".into(),
                    outcome: ToolRecordedOutcome::Failed {
                        category,
                        code: "failure".into(),
                        message: "unknown".into(),
                        retryable: true,
                    },
                },
            },
        ));
        assert_eq!(
            prepare_tool_retry(&history, "attempt-1", None).is_ok(),
            expected
        );
    }
}
#[test]
fn malformed_legacy_and_stale_attempts_fail_closed() {
    let original = uncertain(ToolEffect::Idempotent);
    let mut legacy = original.clone();
    let SessionEventKind::ToolCall { call: legacy_call } = &mut legacy[0].kind else {
        unreachable!()
    };
    legacy_call.operation = None;
    assert!(matches!(
        prepare_tool_retry(&legacy, "attempt-1", None),
        Err(ToolRetryError::LegacyCall)
    ));
    let mut malformed = original.clone();
    malformed[0].seq = 2;
    assert!(inspect_tool_recovery(&malformed, "attempt-1").is_err());
    let plan = prepare_tool_retry(&original, "attempt-1", None).unwrap();
    let op = plan
        .take_operation(
            &identity().session_id,
            &identity().task_id,
            "probe",
            &json!({"ok":true}),
            &contract(ToolEffect::Idempotent),
        )
        .unwrap();
    let mut next = call(ToolEffect::Idempotent);
    next.id = "attempt-2".into();
    next.operation = Some(Box::new(op));
    let mut history = original.clone();
    history.push(event(1, SessionEventKind::ToolCall { call: next.clone() }));
    assert!(matches!(
        prepare_tool_retry(&history, "attempt-1", None),
        Err(ToolRetryError::NotLatestAttempt)
    ));
    assert!(prepare_tool_retry(&history, "attempt-2", None).is_ok());
    next.arguments = json!({"ok":false});
    history[1].kind = SessionEventKind::ToolCall { call: next };
    assert!(matches!(
        inspect_tool_recovery(&history, "attempt-2"),
        Err(ToolRetryError::BindingChanged)
    ));
    let mut wire = serde_json::to_value(call(ToolEffect::Unknown)).unwrap();
    wire.as_object_mut().unwrap().remove("operation");
    assert!(
        serde_json::from_value::<ToolCall>(wire)
            .unwrap()
            .operation
            .is_none()
    );
}
#[test]
fn plan_clones_share_one_use_and_bind_all_execution_inputs() {
    let plan = prepare_tool_retry(&uncertain(ToolEffect::ReadOnly), "attempt-1", None).unwrap();
    let duplicate = plan.clone();
    for (session, task, name, arguments, contract) in [
        (
            SessionId::new(),
            identity().task_id,
            "probe",
            json!({"ok":true}),
            contract(ToolEffect::ReadOnly),
        ),
        (
            identity().session_id,
            TaskId::new(),
            "probe",
            json!({"ok":true}),
            contract(ToolEffect::ReadOnly),
        ),
        (
            identity().session_id,
            identity().task_id,
            "other",
            json!({"ok":true}),
            contract(ToolEffect::ReadOnly),
        ),
        (
            identity().session_id,
            identity().task_id,
            "probe",
            json!({"ok":false}),
            contract(ToolEffect::ReadOnly),
        ),
        (
            identity().session_id,
            identity().task_id,
            "probe",
            json!({"ok":true}),
            contract(ToolEffect::Idempotent),
        ),
    ] {
        assert!(matches!(
            plan.take_operation(&session, &task, name, &arguments, &contract),
            Err(ToolRetryError::BindingChanged)
        ));
    }
    plan.take_operation(
        &identity().session_id,
        &identity().task_id,
        "probe",
        &json!({"ok":true}),
        &contract(ToolEffect::ReadOnly),
    )
    .unwrap();
    assert!(matches!(
        duplicate.take_operation(
            &identity().session_id,
            &identity().task_id,
            "probe",
            &json!({"ok":true}),
            &contract(ToolEffect::ReadOnly)
        ),
        Err(ToolRetryError::Consumed)
    ));
}

struct Disk(PathBuf);
impl Disk {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("jingwei-idempotency-{}", TaskId::new()));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Disk {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct EffectTool {
    path: PathBuf,
    fail_receipt: bool,
    calls: AtomicUsize,
}
impl Tool for EffectTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("idempotent probe", json!({"type":"object"}))
            .with_effect(ToolEffect::Idempotent, "v1")
            .with_approval(ApprovalRequirement::required("current"))
    }
    fn execute<'a>(
        &'a self,
        request: ToolBodyRequest<'a>,
        _: Arc<dyn CancellationSignal>,
    ) -> ToolFuture<'a, Result<String, ToolBodyError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let op = request.operation().expect("runtime operation before body");
            let mut ledger: BTreeMap<String, Value> = if self.path.exists() {
                serde_json::from_slice(&fs::read(&self.path).unwrap()).unwrap()
            } else {
                BTreeMap::new()
            };
            if let Some(old) = ledger.get(&op.key) {
                assert_eq!(old, request.arguments());
                return Ok("applied".into());
            }
            ledger.insert(op.key.clone(), request.arguments().clone());
            fs::write(&self.path, serde_json::to_vec(&ledger).unwrap()).unwrap();
            if self.fail_receipt {
                Err(ToolBodyError::new(
                    "lost_receipt",
                    "effect may already exist",
                    true,
                ))
            } else {
                Ok("applied".into())
            }
        })
    }
}
struct CurrentApproval {
    allow: bool,
    calls: Arc<AtomicUsize>,
}
impl ToolAuthorizer for CurrentApproval {
    fn authorize<'a>(
        &'a self,
        _: ToolAuthorizationRequest<'a>,
    ) -> ToolFuture<'a, Result<ToolAuthorizationDecision, ToolAuthorizationError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.allow {
                ToolAuthorizationDecision::Approved
            } else {
                ToolAuthorizationDecision::Denied(ToolDenial::new(
                    "denied",
                    "current approval",
                    false,
                ))
            })
        })
    }
}
struct EffectPlugin {
    tool: Arc<EffectTool>,
    approval: Arc<CurrentApproval>,
}
impl Plugin for EffectPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("retry-tools")
    }
    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.register_tool("probe", self.tool.clone())?;
        ctx.register_tool_authorizer("current", self.approval.clone())
    }
}
struct Log {
    events: Arc<Mutex<Vec<SessionEvent>>>,
    turn: TurnId,
}
impl ToolEventRecorder for Log {
    fn append(
        &self,
        record: ToolRecord,
    ) -> ToolFuture<'_, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        Box::pin(async move {
            let mut events = self.events.lock().unwrap();
            let mut e = event(events.len() as u64, record.into_session_event_kind());
            e.turn_id = self.turn.clone();
            events.push(e.clone());
            Ok(Arc::new(e))
        })
    }
}
struct Runtime {
    registry: PluginRegistry,
    turn: Box<dyn ToolTurn>,
    run: BudgetRun,
    tool: Arc<EffectTool>,
    approvals: Arc<AtomicUsize>,
}
impl Runtime {
    async fn new(
        disk: &Disk,
        task: &TaskBudget,
        events: Arc<Mutex<Vec<SessionEvent>>>,
        fail: bool,
        allow: bool,
    ) -> Self {
        let tool = Arc::new(EffectTool {
            path: disk.0.join("effects.json"),
            fail_receipt: fail,
            calls: AtomicUsize::new(0),
        });
        let approvals = Arc::new(AtomicUsize::new(0));
        let slot = Arc::new(Mutex::new(None));
        let mut registrar = Registrar::default();
        registrar.add(EffectPlugin {
            tool: tool.clone(),
            approval: Arc::new(CurrentApproval {
                allow,
                calls: approvals.clone(),
            }),
        });
        registrar.add(Capture(slot.clone()));
        registrar.add(
            CanonicalToolRuntimePlugin::new().grant_tool(PluginId::new("caller-owner"), "probe"),
        );
        registrar.select(AGENT_RUNTIME, "capture");
        registrar.select(TOOL_RUNTIME, "canonical");
        let registry = registrar.finish().await.unwrap();
        let runtime = slot.lock().unwrap().take().unwrap();
        let turn_id = TurnId::new();
        let run = task
            .begin_run(turn_id.clone(), BudgetLimits::default())
            .unwrap();
        let turn = runtime
            .bind_turn(
                ToolTurnBinding::new(
                    ToolCaller::new("caller-owner"),
                    Arc::new(Signal(CancellationToken::new())),
                    Arc::new(Log {
                        events,
                        turn: turn_id,
                    }),
                )
                .with_budget(run.scope()),
            )
            .unwrap();
        Self {
            registry,
            turn,
            run,
            tool,
            approvals,
        }
    }
    async fn finish(mut self) {
        self.turn.finish(ToolFinishMode::Graceful).await.unwrap();
        self.run.finish().unwrap();
        self.registry.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn explicit_retry_reuses_key_after_runtime_reopen_and_rechecks_approval_and_budget() {
    let disk = Disk::new();
    let events = Arc::new(Mutex::new(vec![]));
    let task = TaskBudget::new(
        identity(),
        BudgetLimits::default(),
        BudgetLimits::default(),
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    let first = Runtime::new(&disk, &task, events.clone(), true, true).await;
    let original = first
        .turn
        .gateway()
        .call("probe", json!({"ok":true}))
        .await
        .unwrap();
    assert!(matches!(
        original.result().outcome,
        ToolRecordedOutcome::Failed {
            category: ToolFailureCategory::BodyFailure,
            ..
        }
    ));
    assert_eq!(first.tool.calls.load(Ordering::SeqCst), 1);
    first.finish().await;
    let plan = prepare_tool_retry(&events.lock().unwrap(), &original.call().id, None).unwrap();
    let denied = Runtime::new(&disk, &task, events.clone(), false, false).await;
    let refused = denied
        .turn
        .gateway()
        .call_with_options(
            "probe",
            json!({"ok":true}),
            ToolCallOptions {
                retry: Some(plan),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(denied.approvals.load(Ordering::SeqCst), 1);
    assert_eq!(denied.tool.calls.load(Ordering::SeqCst), 0);
    denied.finish().await;
    let plan = prepare_tool_retry(&events.lock().unwrap(), &refused.call().id, None).unwrap();
    let retried = Runtime::new(&disk, &task, events.clone(), false, true).await;
    let result = retried
        .turn
        .gateway()
        .call_with_options(
            "probe",
            json!({"ok":true}),
            ToolCallOptions {
                retry: Some(plan),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result.result().outcome,
        ToolRecordedOutcome::Succeeded { .. }
    ));
    assert_ne!(result.call().id, original.call().id);
    assert_eq!(
        result.call().operation.as_ref().unwrap().key,
        original.call().operation.as_ref().unwrap().key
    );
    assert_eq!(
        result
            .call()
            .operation
            .as_ref()
            .unwrap()
            .retry
            .as_ref()
            .unwrap()
            .call_id,
        refused.call().id
    );
    assert_eq!(retried.approvals.load(Ordering::SeqCst), 1);
    let ledger: BTreeMap<String, Value> =
        serde_json::from_slice(&fs::read(disk.0.join("effects.json")).unwrap()).unwrap();
    assert_eq!(ledger.len(), 1);
    assert!(matches!(
        prepare_tool_retry(&events.lock().unwrap(), &result.call().id, None),
        Err(ToolRetryError::Completed)
    ));
    retried.finish().await;
    assert_eq!(task.report().unwrap().charged.tool_calls, 3);
}
