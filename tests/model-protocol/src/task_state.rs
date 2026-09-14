use super::*;
use jingwei::budget::{BudgetCheckpoint, BudgetEventCursor};
use jingwei::task::*;
use std::collections::BTreeMap;

fn compatibility() -> TaskCompatibility {
    TaskCompatibility {
        agent_revision: "reference-v1".into(),
        policy_revision: "test-policy-v1".into(),
        tool_revisions: BTreeMap::from([("lookup".into(), "schema-v1".into())]),
        payload_schemas: BTreeMap::from([("app.example".into(), 1)]),
    }
}
fn payloads() -> BTreeMap<String, Value> {
    BTreeMap::from([("app.example".into(), json!({"business":"host-owned"}))])
}
async fn recorded(waiting: bool) -> (BudgetCheckpoint, Vec<SessionEvent>) {
    let responses = if waiting {
        vec![json_response(
            json!({"action":"ask_user","question":"哪个目录？"}),
        )]
    } else {
        vec![call_response(true), final_response(true)]
    };
    let fixture = RefFixture::new(config(4, true), responses, true, ToolMode::Success).await;
    let session = SessionId::new();
    let limits = BudgetLimits::default();
    let task = TaskBudget::new(
        TaskIdentity {
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
    fixture
        .harness
        .start_turn_request(
            AgentTurnRequest::new(session, "reference", "lookup").with_budget(task.clone(), limits),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    fixture.audit_closed_turns();
    let history = fixture.log.lock().unwrap().clone();
    let budget = task
        .seal_checkpoint(history.last().map(BudgetEventCursor::from))
        .unwrap();
    fixture.harness.shutdown().await.unwrap();
    (budget, history)
}
#[tokio::test]
async fn captures_completed_and_waiting_boundaries_without_inventing_business_state() {
    for waiting in [false, true] {
        let (budget, history) = recorded(waiting).await;
        let snapshot =
            TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
        assert_eq!(snapshot.revision(), 1);
        assert_eq!(snapshot.cursor(), budget.anchor());
        assert_eq!(snapshot.budget().charged, budget.report().charged);
        assert_eq!(snapshot.payloads(), &payloads());
        assert_eq!(snapshot.settled_steps().len(), if waiting { 1 } else { 2 });
        assert_eq!(
            snapshot.phase(),
            &if waiting {
                TaskPhase::WaitingForInput {
                    question: "哪个目录？".into(),
                }
            } else {
                TaskPhase::Completed
            }
        );
        snapshot
            .verify_evidence(budget.identity(), 1, &compatibility(), &budget, &history)
            .unwrap();
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let restored = TaskSnapshot::from_json(&encoded).unwrap();
        assert_eq!(restored, snapshot);
        restored
            .verify_evidence(budget.identity(), 1, &compatibility(), &budget, &history)
            .unwrap();
    }
}
#[tokio::test]
async fn missing_tool_result_requires_review_without_replaying_accepted_work() {
    let (budget, history) = recorded(false).await;
    let call = history
        .iter()
        .position(|e| matches!(e.kind, SessionEventKind::ToolCall { .. }))
        .unwrap();
    let prefix = &history[..=call];
    let assessment = assess_task_history(budget.identity(), prefix).unwrap();
    assert!(matches!(
        assessment.boundary,
        TaskHistoryBoundary::Open { .. }
    ));
    assert_eq!(assessment.unresolved.len(), 1);
    assert_eq!(assessment.unresolved[0].kind, TaskOperationKind::Tool);
    assert_eq!(
        assessment.unresolved[0].intent,
        BudgetEventCursor::from(&history[call])
    );
    assert!(assessment.settled_steps.is_empty());
    assert!(matches!(
        TaskSnapshot::capture(0, compatibility(), payloads(), &budget, prefix),
        Err(TaskStateError::NeedsReview)
    ));
    let request = history
        .iter()
        .position(|e| matches!(e.kind, SessionEventKind::ModelRequest { .. }))
        .unwrap();
    assert_eq!(
        assess_task_history(budget.identity(), &history[..=request])
            .unwrap()
            .unresolved[0]
            .kind,
        TaskOperationKind::Model
    );
    let result = history
        .iter()
        .position(|e| matches!(e.kind, SessionEventKind::ToolResult { .. }))
        .unwrap();
    assert!(
        assess_task_history(budget.identity(), &history[..=result])
            .unwrap()
            .unresolved
            .is_empty()
    );
    assert!(matches!(
        TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history[..=result]),
        Err(TaskStateError::NeedsReview)
    ));
}
#[tokio::test]
async fn mismatched_contract_identity_revision_and_forged_evidence_are_rejected() {
    let (budget, history) = recorded(false).await;
    let snapshot =
        TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
    for field in ["agent", "policy", "tool", "schema"] {
        let mut expected = compatibility();
        match field {
            "agent" => expected.agent_revision = "v2".into(),
            "policy" => expected.policy_revision = "v2".into(),
            "tool" => {
                expected.tool_revisions.clear();
            }
            _ => {
                expected.payload_schemas.insert("app.example".into(), 2);
            }
        }
        assert!(matches!(
            snapshot.verify_evidence(budget.identity(), 1, &expected, &budget, &history),
            Err(TaskStateError::Incompatible)
        ));
    }
    let mut wrong_identity = budget.identity().clone();
    wrong_identity.task_id = TaskId::new();
    assert!(matches!(
        snapshot.verify_evidence(&wrong_identity, 1, &compatibility(), &budget, &history),
        Err(TaskStateError::IdentityMismatch)
    ));
    assert!(matches!(
        snapshot.verify_evidence(budget.identity(), 2, &compatibility(), &budget, &history),
        Err(TaskStateError::RevisionMismatch { .. })
    ));
    for field in ["steps", "budget", "phase", "cursor"] {
        let mut wire = serde_json::to_value(&snapshot).unwrap();
        match field {
            "steps" => wire["settled_steps"] = json!([]),
            "budget" => wire["budget"]["charged"]["model_requests"] = json!(0),
            "phase" => wire["phase"] = json!({"WaitingForInput":{"question":"forged"}}),
            _ => wire["cursor"]["event_id"] = json!("forged"),
        }
        let forged = TaskSnapshot::from_json(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert!(matches!(
            forged.verify_evidence(budget.identity(), 1, &compatibility(), &budget, &history),
            Err(TaskStateError::EvidenceMismatch)
        ));
    }
}
#[tokio::test]
async fn corrupted_or_mispaired_history_is_not_silently_rebuilt() {
    let (budget, history) = recorded(false).await;
    for corruption in ["seq", "id", "result", "counts", "turn"] {
        let mut changed = history.clone();
        match corruption {
            "seq" => changed[1].seq = 99,
            "id" => changed[1].event_id = changed[0].event_id.clone(),
            "result" => {
                for e in &mut changed {
                    if let SessionEventKind::ToolResult { result } = &mut e.kind {
                        result.call_id = "other-call".into();
                    }
                }
            }
            "counts" => {
                for e in &mut changed {
                    if let SessionEventKind::TaskRunReport { report } = &mut e.kind {
                        report
                            .budget
                            .run
                            .as_mut()
                            .unwrap()
                            .metrics
                            .confirmed
                            .tool_results = 99;
                    }
                }
            }
            _ => changed[1].turn_id = TurnId::new(),
        }
        assert!(
            TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &changed).is_err(),
            "{corruption}"
        );
    }
    let mut wrong = history.clone();
    wrong[0].session_id = SessionId::new();
    assert!(matches!(
        assess_task_history(budget.identity(), &wrong),
        Err(TaskStateError::IdentityMismatch)
    ));
}
#[tokio::test]
async fn store_cas_race_exact_retry_and_identity_collision_keep_existing_state() {
    let (budget, history) = recorded(false).await;
    let initial = TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
    let store = Arc::new(MemoryTaskStateStore::new(1).unwrap());
    assert_eq!(
        store.compare_exchange(0, &initial).await.unwrap(),
        TaskWriteOutcome::Applied
    );
    assert_eq!(
        store.compare_exchange(0, &initial).await.unwrap(),
        TaskWriteOutcome::AlreadyPresent
    );
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [1, 2]
        .into_iter()
        .map(|value| {
            let candidate = TaskSnapshot::capture(
                1,
                compatibility(),
                BTreeMap::from([("app.example".into(), json!({"writer":value}))]),
                &budget,
                &history,
            )
            .unwrap();
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                futures::executor::block_on(store.compare_exchange(1, &candidate))
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(r, Ok(TaskWriteOutcome::Applied)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|r| matches!(
                r,
                Err(TaskStoreError::Conflict {
                    expected: 1,
                    actual: 2
                })
            ))
            .count(),
        1
    );
    let saved = store.load(budget.identity()).await.unwrap().unwrap();
    assert_eq!(saved.revision(), 2);
    assert!(matches!(
        store.compare_exchange(0, &initial).await,
        Err(TaskStoreError::Conflict { .. })
    ));
    let mut wrong = budget.identity().clone();
    wrong.session_id = SessionId::new();
    assert!(matches!(
        store.load(&wrong).await,
        Err(TaskStoreError::State(TaskStateError::IdentityMismatch))
    ));
    assert_eq!(store.load(budget.identity()).await.unwrap().unwrap(), saved);
    let (other_budget, other_history) = recorded(true).await;
    let other = TaskSnapshot::capture(
        0,
        compatibility(),
        payloads(),
        &other_budget,
        &other_history,
    )
    .unwrap();
    assert!(matches!(
        store.compare_exchange(0, &other).await,
        Err(TaskStoreError::Capacity)
    ));
}
#[tokio::test]
async fn bounded_encoding_unknown_versions_and_monotonicity_fail_closed() {
    let (budget, history) = recorded(false).await;
    let snapshot =
        TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
    for field in ["version", "unknown", "namespace"] {
        let mut wire = serde_json::to_value(&snapshot).unwrap();
        match field {
            "version" => wire["version"] = json!(999),
            "unknown" => wire["extra"] = json!(true),
            _ => wire["payloads"] = json!({"undeclared":{}}),
        }
        assert!(TaskSnapshot::from_json(&serde_json::to_vec(&wire).unwrap()).is_err());
    }
    assert!(matches!(
        TaskSnapshot::from_json(&vec![b' '; MAX_TASK_SNAPSHOT_BYTES + 1]),
        Err(TaskStateError::Limit)
    ));
    let too_large = BTreeMap::from([(
        "app.example".into(),
        json!("x".repeat(MAX_TASK_SNAPSHOT_BYTES)),
    )]);
    assert!(matches!(
        TaskSnapshot::capture(0, compatibility(), too_large, &budget, &history),
        Err(TaskStateError::Limit)
    ));
    assert!(matches!(
        TaskSnapshot::capture(u64::MAX, compatibility(), payloads(), &budget, &history),
        Err(TaskStateError::RevisionExhausted)
    ));
    for field in ["steps", "budget", "cursor", "contract"] {
        let mut wire = serde_json::to_value(&snapshot).unwrap();
        wire["revision"] = json!(2);
        match field {
            "steps" => wire["settled_steps"] = json!([]),
            "budget" => wire["budget"]["charged"]["model_requests"] = json!(0),
            "cursor" => wire["cursor"]["seq"] = json!(0),
            _ => wire["compatibility"]["policy_revision"] = json!("changed"),
        }
        let candidate = TaskSnapshot::from_json(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert!(candidate.validate_successor(Some(&snapshot)).is_err());
    }
}
#[test]
fn empty_task_is_created_but_its_snapshot_is_not_execution_authority() {
    let limits = BudgetLimits::default();
    let task = TaskBudget::new(
        TaskIdentity {
            task_id: TaskId::new(),
            session_id: SessionId::new(),
            agent_key: "reference".into(),
        },
        limits,
        limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    let budget = task.seal_checkpoint(None).unwrap();
    let snapshot = TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &[]).unwrap();
    assert_eq!(snapshot.phase(), &TaskPhase::Created);
    assert!(snapshot.cursor().is_none());
    assert!(snapshot.settled_steps().is_empty());
    assert!(task.begin_run(TurnId::new(), limits).is_err());
}

#[tokio::test]
async fn failed_and_cancelled_turns_stay_stopped_and_frozen_images_are_not_safe() {
    for cancelled in [false, true] {
        let fixture = RefFixture::new(
            config(4, false),
            vec![call_response(false)],
            true,
            if cancelled {
                ToolMode::Pending
            } else {
                ToolMode::Failure
            },
        )
        .await;
        let session = SessionId::new();
        let limits = BudgetLimits::default();
        let task = TaskBudget::new(
            TaskIdentity {
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
        let controller = fixture
            .harness
            .start_turn_request(
                AgentTurnRequest::new(session, "reference", "lookup")
                    .with_budget(task.clone(), limits),
            )
            .unwrap();
        if cancelled {
            tokio::time::timeout(Duration::from_secs(2), async {
                while fixture.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            controller.canceller().cancel();
        }
        let result = controller.wait().await;
        assert_eq!(result.is_ok(), cancelled);
        let history = fixture.log.lock().unwrap().clone();
        let budget = task
            .seal_checkpoint(history.last().map(BudgetEventCursor::from))
            .unwrap();
        let snapshot =
            TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
        assert_eq!(
            snapshot.phase(),
            &TaskPhase::Stopped {
                reason: if cancelled {
                    TaskRunStop::CallerCancelled
                } else {
                    TaskRunStop::Failed
                }
            }
        );
        let mut wire = serde_json::to_value(&budget).unwrap();
        wire["recovery_frozen"] = json!(true);
        let frozen: BudgetCheckpoint = serde_json::from_value(wire).unwrap();
        assert!(frozen.requires_recovery());
        assert!(matches!(
            TaskSnapshot::capture(0, compatibility(), payloads(), &frozen, &history),
            Err(TaskStateError::NeedsReview)
        ));
        fixture.audit_closed_turns();
        fixture.harness.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn reference_agent_evidence_survives_file_store_reopen() {
    use jingwei_task_file::{FileTaskStateConfig, FileTaskStateStore};
    for waiting in [false, true] {
        let (budget, history) = recorded(waiting).await;
        let snapshot =
            TaskSnapshot::capture(0, compatibility(), payloads(), &budget, &history).unwrap();
        let path =
            std::env::temp_dir().join(format!("jingwei-reference-state-{}.jsonl", TaskId::new()));
        let file = std::fs::File::create_new(&path).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let store = FileTaskStateStore::open(&path, FileTaskStateConfig::default()).unwrap();
        store.compare_exchange(0, &snapshot).await.unwrap();
        store.close().await;
        drop(store);
        let store = FileTaskStateStore::open(&path, FileTaskStateConfig::default()).unwrap();
        let restored = store.load(budget.identity()).await.unwrap().unwrap();
        assert_eq!(restored, snapshot);
        restored
            .verify_evidence(budget.identity(), 1, &compatibility(), &budget, &history)
            .unwrap();
        store.close().await;
        std::fs::remove_file(path).unwrap();
    }
}
