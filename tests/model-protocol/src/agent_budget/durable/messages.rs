use super::*;

fn messages(events: &[SessionEvent]) -> Vec<&SessionEvent> {
    events
        .iter()
        .filter(|event| matches!(event.kind, SessionEventKind::AssistantMessage { .. }))
        .collect()
}

#[tokio::test]
async fn final_only_streamed_empty_and_waiting_outputs_have_one_complete_message() {
    for command in ["observe", "step", "empty", "ask"] {
        let fixture = Fixture::new().await;
        let report = fixture
            .run(AgentTurnRequest::new(SessionId::new(), "probe", command))
            .await
            .unwrap();
        let complete = messages(report.events());
        assert_eq!(complete.len(), 1);
        assert!(complete[0].message_id.is_some());
        let SessionEventKind::AssistantMessage { text, .. } = &complete[0].kind else {
            unreachable!()
        };
        assert_eq!(text, report.final_text());
        assert_eq!(report.events().iter().rev().nth(2), Some(complete[0]));
        assert_report_before_terminal(report.events());
        let projection = jingwei::context::ConversationProjector::project(
            &jingwei::context::CanonicalConversationProjector,
            report.session_id(),
            report.events(),
            Default::default(),
        )
        .unwrap();
        assert_eq!(
            projection.messages().last(),
            Some(&ModelMessage::assistant(report.final_text()))
        );
        let target = jingwei::context::ContextTarget {
            model: "test-host".into(),
            template_revision: "v1".into(),
        };
        let next = [ModelMessage::user("follow-up")];
        let built = jingwei::context::ContextBuilder::build(
            &jingwei::context::CanonicalContextBuilder,
            jingwei::context::ContextBuildInput {
                history: &projection,
                system: &[],
                current: &next,
                constraint: &jingwei::llm::GenerationConstraint::Text,
                pinned_turns: &[],
                state_version: Some("real-session"),
                target: &target,
                budget: jingwei::context::ContextBudget {
                    window_tokens: 4096,
                    output_reserve: 256,
                    output_evidence: jingwei::context::TokenBoundEvidence::Estimate,
                    safety_margin: 64,
                    mode: jingwei::context::TokenBudgetMode::Soft,
                },
                limits: Default::default(),
            },
            &jingwei::context::ByteHeuristicCounter::default(),
        )
        .unwrap();
        assert_eq!(built.request.messages.last(), next.last());
        assert_eq!(built.report.source_tail, projection.source_tail);
        assert!(built.report.turns.iter().all(|turn| turn.retained));
        if command == "step" {
            assert!(report.events().iter().any(|e| matches!(&e.kind, SessionEventKind::AssistantDelta { text } if text == "step confirmed")));
            assert_ne!(text, "step confirmed");
        }
        fixture.close().await;
    }
}

#[tokio::test]
async fn subsequent_turn_receives_complete_reply_in_canonical_history() {
    let fixture = Fixture::new().await;
    let session = SessionId::new();
    let first = fixture
        .run(AgentTurnRequest::new(session.clone(), "probe", "observe"))
        .await
        .unwrap();
    fixture
        .run(AgentTurnRequest::new(session, "probe", "observe"))
        .await
        .unwrap();
    let histories = fixture.probe.histories.lock().unwrap().clone();
    assert_eq!(histories[1], first.events());
    assert_eq!(messages(&histories[1]).len(), 1);
    fixture.close().await;
}

#[tokio::test]
async fn message_write_failure_retains_attempt_and_cannot_complete() {
    let fixture = Fixture::new().await;
    fixture.memory.fail_message.store(true, Ordering::SeqCst);
    let error = fixture
        .run(AgentTurnRequest::new(SessionId::new(), "probe", "observe"))
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("wrong error");
    };
    let Some(DriveFailure::AssistantMessage(attempt)) = failure.drive_failure() else {
        panic!("missing attempt");
    };
    assert!(matches!(
        attempt.source,
        AssistantMessageCommitError::Persistence(_)
    ));
    assert!(
        matches!(attempt.draft.kind(), SessionEventKind::AssistantMessage { text, .. } if text == "agent claims completion")
    );
    assert_eq!(failure.disposition(), TurnDisposition::Failed);
    let events = fixture.memory.events.lock().unwrap().clone();
    assert!(messages(&events).is_empty());
    assert!(
        matches!(&events.last().unwrap().kind, SessionEventKind::Error { code, .. } if code == "assistant_message_recording")
    );
    fixture.close().await;
}

#[tokio::test]
async fn forged_message_receipts_preserve_evidence_and_freeze_durable_claim() {
    for fault in [
        SessionFault::MessageId,
        SessionFault::MessagePayload,
        SessionFault::MessageSession,
        SessionFault::MessageEventId,
    ] {
        let (registry, runtime, _, _) = faulty_session_fixture(fault).await;
        let store = Store::new();
        let error = runtime
            .start_turn(request(store.acquire().await.unwrap(), "observe"))
            .unwrap()
            .wait()
            .await
            .unwrap_err();
        let AgentRuntimeError::Durability {
            source: BudgetExecutionError::IncompleteTurn,
            outcome,
        } = error
        else {
            panic!("invalid receipt authorized closure");
        };
        let Err(AgentRuntimeError::Turn(failure)) = *outcome else {
            panic!("missing turn evidence");
        };
        assert!(
            matches!(failure.drive_failure(), Some(DriveFailure::AssistantMessage(attempt)) if matches!(attempt.source, AssistantMessageCommitError::InvalidReceipt(_)))
        );
        assert!(store.latest().execution_id().is_some());
        registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn settlement_omitting_confirmed_message_is_not_success() {
    let (registry, runtime, _, _) =
        faulty_session_fixture(SessionFault::SettlementOmitMessage).await;
    let error = runtime
        .start_turn(AgentTurnRequest::new(SessionId::new(), "probe", "observe"))
        .unwrap()
        .wait()
        .await
        .unwrap_err();
    let AgentRuntimeError::Turn(failure) = error else {
        panic!("wrong error");
    };
    assert!(matches!(
        failure.settlement_attempt(),
        SettlementAttempt::Failed(SessionRuntimeError::InvalidMessageSettlement { .. })
    ));
    assert!(failure.report().is_none());
    registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_and_agent_failure_do_not_fabricate_complete_messages() {
    for command in ["cancelled_output", "fail"] {
        let fixture = Fixture::new().await;
        let _ = fixture
            .run(AgentTurnRequest::new(SessionId::new(), "probe", command))
            .await;
        assert!(messages(&fixture.memory.events.lock().unwrap()).is_empty());
        fixture.close().await;
    }
}

#[tokio::test]
async fn dropped_waiter_and_shutdown_drain_message_before_report_and_checkpoint() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let blocked = gate();
    *fixture.memory.message_gate.lock().unwrap() = Some(blocked.clone());
    let controller = fixture
        .runtime
        .start_turn(request(store.acquire().await.unwrap(), "observe"))
        .unwrap();
    entered(&blocked).await;
    drop(controller);
    assert!(store.latest().execution_id().is_some());
    assert!(!fixture.memory.events.lock().unwrap().iter().any(is_report));
    let mut shutdown = Box::pin(fixture.registry.shutdown());
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    blocked.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(store.latest().execution_id().is_none());
    assert_eq!(messages(&fixture.memory.events.lock().unwrap()).len(), 1);
}

#[test]
fn complete_message_version_is_required_and_unknown_versions_fail() {
    let wire = json!({"type":"assistant_message", "version":1, "text":"完整回复"});
    let event: SessionEventKind = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(json!(event), wire);
    let mut invalid = wire.clone();
    invalid["version"] = json!(2);
    assert!(serde_json::from_value::<SessionEventKind>(invalid).is_err());
    let mut invalid = wire;
    invalid.as_object_mut().unwrap().remove("version");
    assert!(serde_json::from_value::<SessionEventKind>(invalid).is_err());
    assert!(
        serde_json::from_value::<SessionEventKind>(
            json!({"type":"assistant_delta", "text":"legacy"})
        )
        .is_ok()
    );
}

#[test]
fn complete_message_survives_process_restart_into_next_admission() {
    let disk = Disk::new();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        disk.store().compare_exchange(0, &initial()).await.unwrap();
    });
    ChildGuard::spawn(&disk, "ask").finish();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_budget::durable::messages::message_history_child",
            "--nocapture",
        ])
        .env("JW_MESSAGE_ROOT", &disk.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn message_history_child() {
    let Ok(root) = std::env::var("JW_MESSAGE_ROOT") else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let mut registrar = Registrar::default();
        let probe = Arc::new(Probe::default());
        registrar.add(JsonlSessionPersistencePlugin::new(root));
        registrar.add(ProbePlugin(probe.clone(), false));
        registrar.add(CanonicalSessionRuntimePlugin::new());
        registrar.add(CanonicalAgentRuntimePlugin::new());
        registrar.require(AGENT_RUNTIME);
        let registry = registrar.finish().await.unwrap();
        registry.agent_runtime().unwrap().start_turn(AgentTurnRequest::new(binding().session_id, "probe", "observe")).unwrap().wait().await.unwrap();
        let history = probe.histories.lock().unwrap().clone();
        let complete = messages(&history[0]);
        let projection = jingwei::context::ConversationProjector::project(
            &jingwei::context::CanonicalConversationProjector,
            &binding().session_id, &history[0], Default::default(),
        ).unwrap();
        assert_eq!(projection.turns[0].state, jingwei::context::ProjectedTurnState::WaitingForInput);
        assert_eq!(projection.messages().count(), 2);
        assert_eq!(complete.len(), 1);
        assert!(matches!(&complete[0].kind, SessionEventKind::AssistantMessage { text, .. } if text == "agent claims completion"));
        registry.shutdown().await.unwrap();
    });
}
