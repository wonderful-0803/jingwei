use super::*;

fn malformed(json_mode: bool) -> GenerationResponse {
    if json_mode {
        json_response(json!({"action":"call_tool","name":"lookup","arguments":{"query":42}}))
    } else {
        native("lookup", json!({"query":42}))
    }
}
fn ask(json_mode: bool) -> GenerationResponse {
    if json_mode {
        json_response(json!({"action":"ask_user","question":"哪个目录？"}))
    } else {
        native("jingwei_ask_user", json!({"question":"哪个目录？"}))
    }
}
fn corrections(fixture: &RefFixture) -> Vec<ReferenceCorrectionReport> {
    fixture
        .log
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            SessionEventKind::Custom {
                plugin,
                kind,
                payload,
            } if plugin == REFERENCE_EVENT_PLUGIN && kind == REFERENCE_CORRECTION_EVENT => {
                Some(serde_json::from_value(payload.clone()).unwrap())
            }
            _ => None,
        })
        .collect()
}
fn task(session: &SessionId, limits: BudgetLimits) -> TaskBudget {
    TaskBudget::new(
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
    .unwrap()
}
#[tokio::test]
async fn invalid_parameters_receive_structured_feedback_before_any_tool_execution() {
    for json_mode in [false, true] {
        let fixture = RefFixture::new(
            config(5, json_mode),
            vec![
                malformed(json_mode),
                call_response(json_mode),
                final_response(json_mode),
            ],
            true,
            ToolMode::Success,
        )
        .await;
        let report = fixture
            .harness
            .run_turn(&SessionId::new(), "reference", "lookup")
            .await
            .unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 3);
        assert_eq!(corrections(&fixture).len(), 1);
        assert_eq!(fixture.reports()[0].corrections, 1);
        let budget = &report.task_run_report().unwrap().budget;
        assert_eq!(budget.charged.corrections, 1);
        assert_eq!(budget.charged.steps, 3);
        let requests = fixture.model.requests.lock().unwrap().clone();
        let retry = serde_json::to_string(&requests[1]).unwrap();
        assert!(retry.contains("jingwei_action_correction_v1"));
        assert!(!retry.contains("\\\"query\\\":42"));
        assert!(
            !serde_json::to_string(&requests[2])
                .unwrap()
                .contains("jingwei_action_correction_v1")
        );
        fixture.audit_closed_turns();
        fixture.harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn malformed_json_can_be_corrected_without_hiding_its_canonical_failure() {
    let fixture = RefFixture::new(
        config(4, true),
        vec![
            GenerationResponse::text("not json", FinishReason::Stop),
            final_response(true),
        ],
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
    assert_eq!(
        corrections(&fixture)[0].reason,
        CorrectionReason::InvalidFormat
    );
    assert_eq!(
        report
            .task_run_report()
            .unwrap()
            .budget
            .charged
            .model_requests,
        2
    );
    assert_eq!(
        report
            .task_run_report()
            .unwrap()
            .budget
            .run
            .as_ref()
            .unwrap()
            .metrics
            .confirmed
            .model_results,
        2
    );
    fixture.audit_closed_turns();
    fixture.harness.shutdown().await.unwrap();
}
#[tokio::test]
async fn local_and_shared_correction_limits_never_start_an_extra_inference() {
    for json_mode in [false, true] {
        for shared in [false, true] {
            let mut cfg = config(8, json_mode);
            cfg.max_corrections = if shared { 8 } else { 1 };
            let fixture =
                RefFixture::new(cfg, vec![malformed(json_mode); 2], true, ToolMode::Success).await;
            let session = SessionId::new();
            let mut limits = BudgetLimits::default();
            limits.resources.corrections = 1;
            let task = task(&session, limits);
            assert!(
                fixture
                    .harness
                    .start_turn_request(
                        AgentTurnRequest::new(session, "reference", "lookup")
                            .with_budget(task.clone(), limits)
                    )
                    .unwrap()
                    .wait()
                    .await
                    .is_err()
            );
            assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
            assert_eq!(corrections(&fixture).len(), 1);
            assert_eq!(task.report().unwrap().charged.corrections, 1);
            if !shared {
                assert_eq!(fixture.reports()[0].stop, ReferenceStop::CorrectionLimit);
            }
            fixture.audit_closed_turns();
            fixture.harness.shutdown().await.unwrap();
        }
    }
}
#[tokio::test]
async fn no_correction_is_charged_without_another_step_or_when_disabled() {
    for (steps, max_corrections, stop) in [
        (1, 2, ReferenceStop::StepLimit),
        (4, 0, ReferenceStop::CorrectionLimit),
    ] {
        let mut cfg = config(steps, false);
        cfg.max_corrections = max_corrections;
        let fixture = RefFixture::new(cfg, vec![malformed(false)], true, ToolMode::Success).await;
        assert!(
            fixture
                .harness
                .run_turn(&SessionId::new(), "reference", "lookup")
                .await
                .is_err()
        );
        assert!(corrections(&fixture).is_empty());
        assert_eq!(fixture.reports()[0].stop, stop);
        assert_eq!(fixture.reports()[0].corrections, 0);
        fixture.audit_closed_turns();
        fixture.harness.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn invisible_tools_and_denied_execution_are_never_corrected() {
    for json_mode in [false, true] {
        for denied in [false, true] {
            let response = if denied {
                call_response(json_mode)
            } else if json_mode {
                json_response(json!({"action":"call_tool","name":"root_shell","arguments":{}}))
            } else {
                native("root_shell", json!({}))
            };
            let fixture = RefFixture::new(
                config(8, json_mode),
                vec![response],
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
            assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), usize::from(denied));
            assert!(corrections(&fixture).is_empty());
            fixture.audit_closed_turns();
            fixture.harness.shutdown().await.unwrap();
        }
    }
}
#[tokio::test]
async fn waiting_handoff_reuses_corrections_and_rejects_mismatched_reply_or_reset_ledger() {
    for json_mode in [false, true] {
        let fixture = RefFixture::new(
            config(8, json_mode),
            vec![malformed(json_mode), ask(json_mode), malformed(json_mode)],
            true,
            ToolMode::Success,
        )
        .await;
        let session = SessionId::new();
        let mut limits = BudgetLimits::default();
        limits.resources.corrections = 1;
        let task = task(&session, limits);
        let first = fixture
            .harness
            .start_turn_request(
                AgentTurnRequest::new(session.clone(), "reference", "lookup")
                    .with_budget(task.clone(), limits),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        let waiting = ReferenceWaiting::from_report(&first, task.clone()).unwrap();
        let pending = waiting.pending().clone();
        let serialized = serde_json::to_string(&pending).unwrap();
        assert_eq!(
            serde_json::from_str::<PendingQuestion>(&serialized).unwrap(),
            pending
        );
        assert_eq!(pending.turn_id, *first.turn_id());
        assert!(waiting.resume(&TurnId::new(), "A", limits).is_err());
        let reset = TaskBudget::new(
            task.identity().clone(),
            limits,
            limits,
            TokenBudgetMode::Soft,
            Arc::new(MonotonicBudgetClock::default()),
        )
        .unwrap();
        assert!(ReferenceWaiting::from_report(&first, reset).is_err());
        let waiting = ReferenceWaiting::from_report(&first, task.clone()).unwrap();
        let request = waiting.resume(first.turn_id(), "目录 A", limits).unwrap();
        assert!(
            fixture
                .harness
                .start_turn_request(request)
                .unwrap()
                .wait()
                .await
                .is_err()
        );
        assert_eq!(task.report().unwrap().charged.corrections, 1);
        assert_eq!(task.report().unwrap().charged.model_requests, 3);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert!(ReferenceWaiting::from_report(&first, task).is_err());
        let requests = fixture.model.requests.lock().unwrap().clone();
        let resumed = serde_json::to_string(&requests[2]).unwrap();
        assert!(resumed.contains("reference_reply_v1"));
        assert!(resumed.contains(first.turn_id().as_str()));
        fixture.audit_closed_turns();
        fixture.harness.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn correction_report_failure_keeps_charge_and_prevents_retry() {
    let fixture = RefFixture::agent(
        Arc::new(GatedAgent {
            agent: ReferenceAgent::new(config(4, false), policies()).unwrap(),
            fail_report: true,
            hide_budget: false,
        }),
        vec![malformed(false)],
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
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(corrections(&fixture).is_empty());
    assert!(fixture.log.lock().unwrap().iter().any(|e| matches!(&e.kind, SessionEventKind::TaskRunReport { report } if report.budget.charged.corrections == 1 && report.capabilities_drained)));
    fixture.audit_closed_turns();
    fixture.harness.shutdown().await.unwrap();
}
