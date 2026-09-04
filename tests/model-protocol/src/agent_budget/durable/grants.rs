//! Private host grant regressions; no production failure switches or dependencies.
use super::*;
use jingwei::id::BudgetOperationId;

fn limited() -> BudgetCheckpoint {
    let mut limits = BudgetLimits::default();
    limits.resources.steps = 1;
    TaskBudget::new(
        binding(),
        limits,
        limits,
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap()
    .seal_checkpoint(None)
    .unwrap()
}

fn setup() -> Arc<Store> {
    let store = Store::new();
    *store.latest.lock().unwrap() = Some(limited());
    store
}

fn grant(steps: u64) -> BudgetGrantRequest {
    let mut new_limits = limited().report().limits;
    new_limits.resources.steps = steps;
    BudgetGrantRequest {
        operation_id: BudgetOperationId::new(),
        actor: "host-operator".into(),
        source: "private-admin-test".into(),
        reason: "approved continuation".into(),
        new_limits,
    }
}

async fn apply(
    store: &Store,
    request: &BudgetGrantRequest,
) -> Result<BudgetGrantOutcome, BudgetGrantError> {
    grant_budget(store, &expectations(&store.latest()), &[], request).await
}

#[tokio::test]
async fn grant_is_one_atomic_audited_transition_and_never_an_execution_lease() {
    let store = setup();
    let before = store.latest();
    let request = grant(3);
    let outcome = apply(&store, &request).await.unwrap();
    let BudgetGrantOutcome::Applied { checkpoint, commit } = outcome else {
        panic!()
    };
    assert_eq!(commit, BudgetCheckpointCommit::Committed);
    assert_eq!(checkpoint.as_ref(), &store.latest());
    assert_eq!(checkpoint.revision(), 2);
    assert_eq!(json!(&checkpoint)["version"], 3);
    assert_eq!(checkpoint.report().charged, before.report().charged);
    assert_eq!(checkpoint.report().usage, before.report().usage);
    assert_eq!(checkpoint.report().token_mode, before.report().token_mode);
    assert_eq!(
        checkpoint.grants(),
        &[BudgetGrantRecord {
            request,
            source_revision: 1,
            confirmed_anchor: None,
            old_limits: before.report().limits,
            old_stop: None,
        }]
    );
    assert!(checkpoint.execution_id().is_none());
    let lease = store.acquire().await.unwrap();
    lease.verify_history(&[]).unwrap();
    assert_eq!(lease.task().report().unwrap().limits.resources.steps, 3);
}

#[tokio::test]
async fn exhausted_task_resumes_only_after_audited_increase_and_keeps_consumption() {
    let fixture = Fixture::new().await;
    let store = setup();
    assert!(
        fixture
            .run(request(
                store.acquire().await.unwrap(),
                "swallow_second_step"
            ))
            .await
            .is_err()
    );
    let before = store.latest();
    assert_eq!(
        before.report().stop,
        Some(BudgetStopReason::ResourceLimit(BudgetResource::Steps))
    );
    assert_eq!(before.report().charged.steps, 1);
    let history = fixture.memory.events.lock().unwrap().clone();
    let request_grant = grant(3);
    let expected = expectations(&before);
    grant_budget(store.as_ref(), &expected, &history, &request_grant)
        .await
        .unwrap();
    let after = store.latest();
    assert_eq!(after.report().stop, None);
    assert_eq!(after.report().charged, before.report().charged);
    assert_eq!(after.report().active_time, before.report().active_time);
    assert_eq!(after.report().cleanup_time, before.report().cleanup_time);
    assert_eq!(fixture.memory.events.lock().unwrap().as_slice(), history);
    fixture
        .run(request(store.acquire().await.unwrap(), "step"))
        .await
        .unwrap();
    let latest = store.latest();
    assert_eq!(latest.report().charged.steps, 2);
    assert!(latest.report().active_time >= before.report().active_time);
    assert_eq!(latest.grants(), after.grants());
    // A retry after later execution returns evidence, not another increase.
    assert!(matches!(
        grant_budget(store.as_ref(), &expected, &[], &request_grant)
            .await
            .unwrap(),
        BudgetGrantOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(store.latest(), latest);
    fixture.close().await;
}

#[tokio::test]
async fn successive_grants_explain_only_limits_and_keep_unknown_usage() {
    let fixture = Fixture::with_model(true).await;
    let store = setup();
    let mut image = json!(limited());
    image["report"]["limits"]["resources"]["steps"] = json!(3);
    *store.latest.lock().unwrap() = Some(serde_json::from_value(image).unwrap());
    fixture
        .run(request(store.acquire().await.unwrap(), "model"))
        .await
        .unwrap();
    let before = store.latest();
    assert!(before.report().usage.input_tokens.unknown > 0);
    let history = fixture.memory.events.lock().unwrap().clone();
    for steps in [4, 5] {
        grant_budget(
            store.as_ref(),
            &expectations(&store.latest()),
            &history,
            &grant(steps),
        )
        .await
        .unwrap();
    }
    assert_eq!(store.latest().report().usage, before.report().usage);
    assert_eq!(store.latest().report().charged, before.report().charged);
    let lease = store.acquire().await.unwrap();
    lease.verify_history(&history).unwrap();
    let mut tampered = history.clone();
    let index = tampered.len() - 2;
    if let SessionEventKind::TaskRunReport { report } = &mut tampered[index].kind {
        report.budget.charged.input_tokens = 0;
    } else {
        panic!()
    }
    assert!(lease.verify_history(&tampered).is_err());
    fixture.run(request(lease, "model")).await.unwrap();
    assert_eq!(store.latest().report().charged.model_requests, 2);
    assert_eq!(store.latest().report().usage.input_tokens.unknown, 2);
    assert_eq!(store.latest().grants().len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn frozen_claim_cannot_be_cleared_by_an_increase() {
    let store = setup();
    let lease = store.acquire().await.unwrap();
    let before = store.latest();
    assert!(matches!(
        apply(&store, &grant(4)).await,
        Err(BudgetGrantError::RecoveryRequired)
    ));
    assert_eq!(store.latest(), before);
    drop(lease);
    assert!(matches!(
        apply(&store, &grant(4)).await,
        Err(BudgetGrantError::RecoveryRequired)
    ));
    assert_eq!(store.latest(), before);
}

#[tokio::test]
async fn incomplete_ledger_snapshots_cannot_be_granted() {
    for pending in [false, true] {
        let task = TaskBudget::restore_checkpoint(
            limited(),
            expectations(&limited()),
            Arc::new(MonotonicBudgetClock::default()),
        )
        .unwrap();
        let run = task
            .begin_run(TurnId::new(), BudgetLimits::default())
            .unwrap();
        if pending {
            let reservation = run
                .scope()
                .reserve(BudgetRequest::new(BudgetAmounts {
                    steps: 1,
                    ..Default::default()
                }))
                .unwrap();
            drop(reservation);
        }
        let image = task.seal_checkpoint(None).unwrap();
        let store = setup();
        *store.latest.lock().unwrap() = Some(image.clone());
        assert!(matches!(
            apply(&store, &grant(4)).await,
            Err(BudgetGrantError::RecoveryRequired)
        ));
        assert_eq!(store.latest(), image);
        drop(run);
    }
}

#[tokio::test]
async fn unsafe_stop_reasons_are_not_converted_into_budget_exhaustion() {
    for stop in [
        BudgetStopReason::RecoveryRequired,
        BudgetStopReason::UsageExceededReservation(BudgetResource::InputTokens),
        BudgetStopReason::AbandonedReservation,
        BudgetStopReason::RunAbandoned,
        BudgetStopReason::ClockMovedBackwards,
        BudgetStopReason::AccountingOverflow,
        BudgetStopReason::IdentityMismatch,
        BudgetStopReason::InvalidRequest,
    ] {
        let mut image = json!(limited());
        image["report"]["stop"] = json!(stop);
        let store = setup();
        *store.latest.lock().unwrap() = Some(serde_json::from_value(image).unwrap());
        let before = store.latest();
        assert!(matches!(
            apply(&store, &grant(2)).await,
            Err(BudgetGrantError::Invalid(_))
        ));
        assert_eq!(store.latest(), before);
    }
}

#[tokio::test]
async fn grant_rejects_noop_shrink_host_excess_and_wrong_stopped_resource() {
    for mode in 0..5 {
        let store = setup();
        let mut request = grant(2);
        match mode {
            0 => request.new_limits.resources.steps = 1,
            1 => request.new_limits.resources.tool_calls -= 1,
            2 => request.new_limits.resources.steps = 129,
            3 => {
                let mut image = json!(limited());
                image["report"]["stop"] =
                    json!(BudgetStopReason::ResourceLimit(BudgetResource::ToolCalls));
                *store.latest.lock().unwrap() = Some(serde_json::from_value(image).unwrap());
            }
            _ => request.new_limits.active_time = Duration::ZERO,
        }
        let before = store.latest();
        assert!(matches!(
            apply(&store, &request).await,
            Err(BudgetGrantError::Invalid(_))
        ));
        assert_eq!(store.latest(), before);
    }
}

#[tokio::test]
async fn missing_stale_wrong_identity_or_anchor_never_authorizes_a_grant() {
    let store = setup();
    for mode in 0..3 {
        let mut context = expectations(&limited());
        match mode {
            0 => context.revision = 99,
            1 => context.identity.task_id = TaskId::new(),
            _ => {
                context.confirmed_anchor = Some(BudgetEventCursor {
                    session_id: binding().session_id,
                    turn_id: TurnId::new(),
                    event_id: jingwei::id::EventId::new(),
                    seq: 0,
                })
            }
        }
        assert!(matches!(
            grant_budget(store.as_ref(), &context, &[], &grant(2)).await,
            Err(BudgetGrantError::Checkpoint(_))
        ));
        assert_eq!(store.latest(), limited());
    }
    *store.latest.lock().unwrap() = None;
    assert!(matches!(
        grant_budget(store.as_ref(), &expectations(&limited()), &[], &grant(2)).await,
        Err(BudgetGrantError::Missing)
    ));
}

#[tokio::test]
async fn untrusted_audit_text_is_bounded_and_cannot_be_empty() {
    let store = setup();
    for mode in 0..5 {
        let mut request = grant(2);
        match mode {
            0 => request.operation_id = BudgetOperationId::from(" "),
            1 => request.actor.clear(),
            2 => request.source = "\t".into(),
            3 => request.reason = "\n".into(),
            _ => request.reason = "x".repeat(MAX_BUDGET_AUDIT_TEXT_BYTES + 1),
        }
        assert!(matches!(
            apply(&store, &request).await,
            Err(BudgetGrantError::Invalid(_))
        ));
        assert_eq!(store.latest(), limited());
    }
}

#[tokio::test]
async fn duplicate_operation_with_different_intent_or_source_is_a_conflict() {
    let store = setup();
    let original = grant(2);
    apply(&store, &original).await.unwrap();
    let before = store.latest();
    for mode in 0..7 {
        let mut request = original.clone();
        let mut expected = expectations(&limited());
        match mode {
            0 => request.actor.push('x'),
            1 => request.source.push('x'),
            2 => request.reason.push('x'),
            3 => request.new_limits.resources.steps = 3,
            4 => expected.revision = 2,
            5 => {
                expected.confirmed_anchor = Some(BudgetEventCursor {
                    session_id: binding().session_id,
                    turn_id: TurnId::new(),
                    event_id: jingwei::id::EventId::new(),
                    seq: 0,
                })
            }
            _ => {
                // A changed ceiling is only a new policy, not a second operation.
                expected.host_limits.resources.steps = 0;
                assert!(matches!(
                    grant_budget(store.as_ref(), &expected, &[], &request)
                        .await
                        .unwrap(),
                    BudgetGrantOutcome::AlreadyApplied { .. }
                ));
                continue;
            }
        }
        assert!(matches!(
            grant_budget(store.as_ref(), &expected, &[], &request).await,
            Err(BudgetGrantError::OperationConflict)
        ));
    }
    assert_eq!(store.latest(), before);
}

#[tokio::test]
async fn failed_or_uncertain_commit_retains_exact_image_and_idempotent_receipt() {
    for uncertain in [false, true] {
        let store = setup();
        let original = grant(2);
        store.fail_finish.store(true, Ordering::SeqCst);
        store.indeterminate.store(uncertain, Ordering::SeqCst);
        let BudgetGrantError::Commit {
            expected_revision,
            checkpoint,
            source,
        } = apply(&store, &original).await.unwrap_err()
        else {
            panic!()
        };
        assert_eq!(expected_revision, 1);
        assert!(
            matches!(source, BudgetCheckpointStoreError::Storage { certainty, .. }
            if certainty == if uncertain { BudgetCheckpointCommitCertainty::Indeterminate } else { BudgetCheckpointCommitCertainty::DefinitelyNotCommitted })
        );
        assert_eq!(store.latest().grants().len(), usize::from(uncertain));
        store.fail_finish.store(false, Ordering::SeqCst);
        assert_eq!(
            store
                .compare_exchange(expected_revision, &checkpoint)
                .await
                .unwrap(),
            if uncertain {
                BudgetCheckpointCommit::ReplayedExact
            } else {
                BudgetCheckpointCommit::Committed
            }
        );
        assert!(matches!(
            grant_budget(store.as_ref(), &expectations(&limited()), &[], &original)
                .await
                .unwrap(),
            BudgetGrantOutcome::AlreadyApplied { .. }
        ));
        assert_eq!(store.latest().grants().len(), 1);
    }
}

#[tokio::test]
async fn simultaneous_grants_have_one_transition_and_exact_retries_are_safe() {
    for same_id in [true, false] {
        let store = setup();
        let barrier = gate();
        *store.finish_gate.lock().unwrap() = Some(barrier.clone());
        let first = grant(2);
        let second = if same_id { first.clone() } else { grant(3) };
        let left_store = store.clone();
        let left = tokio::spawn(async move { apply(&left_store, &first).await });
        entered(&barrier).await;
        let right_store = store.clone();
        let right = tokio::spawn(async move { apply(&right_store, &second).await });
        entered(&barrier).await;
        barrier.release.add_permits(2);
        let left = left.await.unwrap();
        let right = right.await.unwrap();
        if same_id {
            assert!(left.is_ok() && right.is_ok());
            assert_ne!(left.unwrap(), right.unwrap()); // Committed vs ReplayedExact.
        } else {
            assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
            assert!(matches!(
                left.err().or(right.err()),
                Some(BudgetGrantError::Commit {
                    source: BudgetCheckpointStoreError::Conflict { .. },
                    ..
                })
            ));
        }
        assert_eq!(store.latest().revision(), 2);
        assert_eq!(store.latest().grants().len(), 1);
    }
}

#[tokio::test]
async fn execution_winning_the_cas_prevents_a_prepared_grant_from_clearing_its_claim() {
    let store = setup();
    let barrier = gate();
    *store.finish_gate.lock().unwrap() = Some(barrier.clone());
    let worker_store = store.clone();
    let worker = tokio::spawn(async move { apply(&worker_store, &grant(2)).await });
    entered(&barrier).await;
    let lease = store.acquire().await.unwrap();
    let claim = store.latest();
    barrier.release.add_permits(1);
    assert!(matches!(
        worker.await.unwrap(),
        Err(BudgetGrantError::Commit {
            source: BudgetCheckpointStoreError::Conflict { .. },
            ..
        })
    ));
    assert_eq!(store.latest(), claim);
    assert_eq!(lease.task().report().unwrap().limits.resources.steps, 1);
}

#[tokio::test]
async fn audit_bound_exhaustion_never_discards_old_operation_ids() {
    let store = setup();
    for index in 0..MAX_BUDGET_GRANTS {
        apply(&store, &grant(index as u64 + 2)).await.unwrap();
    }
    let before = store.latest();
    assert!(matches!(
        apply(&store, &grant(MAX_BUDGET_GRANTS as u64 + 2)).await,
        Err(BudgetGrantError::AuditFull)
    ));
    assert_eq!(store.latest(), before);
}

#[tokio::test]
async fn v1_and_v2_ready_checkpoints_upgrade_without_discarding_budget() {
    for version in [1, 2] {
        let store = setup();
        let mut image = json!(limited());
        image["version"] = json!(version);
        *store.latest.lock().unwrap() = Some(serde_json::from_value(image).unwrap());
        apply(&store, &grant(2)).await.unwrap();
        assert_eq!(json!(store.latest())["version"], 3);
        let mut bad = json!(store.latest());
        bad["version"] = json!(version);
        assert!(
            serde_json::from_value::<BudgetCheckpoint>(bad)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}

#[tokio::test]
async fn file_store_checks_grant_history_and_rejects_unaudited_limit_changes() {
    let disk = Disk::new();
    let file = disk.store();
    file.compare_exchange(0, &limited()).await.unwrap();
    grant_budget(file.as_ref(), &expectations(&limited()), &[], &grant(2))
        .await
        .unwrap();
    let before = file.load(&binding()).await.unwrap().unwrap();
    let bytes = fs::read(disk.0.join("budget.jsonl")).unwrap();
    for mode in 0..5 {
        let mut invalid = json!(&before);
        invalid["revision"] = json!(before.revision() + 1);
        match mode {
            0 => invalid["grants"] = json!([]),
            1 => invalid["grants"][0]["request"]["reason"] = json!("rewritten"),
            2 => invalid["report"]["limits"]["resources"]["steps"] = json!(3),
            3 => invalid["grants"][0]["source_revision"] = json!(0),
            _ => {
                invalid["grants"][0]["confirmed_anchor"] =
                    json!({"session_id":"other", "turn_id":"t", "event_id":"e", "seq":0})
            }
        }
        let invalid: BudgetCheckpoint = serde_json::from_value(invalid).unwrap();
        assert!(
            file.compare_exchange(before.revision(), &invalid)
                .await
                .is_err()
        );
        assert_eq!(fs::read(disk.0.join("budget.jsonl")).unwrap(), bytes);
    }
    // Restoration/export preserves the audit; old handles remain sealed.
    let task = TaskBudget::restore_checkpoint(
        before.clone(),
        expectations(&before),
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    let next = task.seal_checkpoint(before.anchor().cloned()).unwrap();
    assert_eq!(next.grants(), before.grants());
    file.compare_exchange(before.revision(), &next)
        .await
        .unwrap();
    file.close().await;
    let reopened = disk.store();
    assert_eq!(reopened.load(&binding()).await.unwrap(), Some(next));
    reopened.close().await;
}

pub(super) async fn child_grant(store: &FileBudgetCheckpointStore) {
    let mut request = grant(2);
    request.operation_id = BudgetOperationId::from("cross-process-approved-operation");
    let outcome = grant_budget(store, &expectations(&limited()), &[], &request)
        .await
        .unwrap();
    match outcome {
        BudgetGrantOutcome::Applied { .. } => println!("GRANT=applied"),
        BudgetGrantOutcome::AlreadyApplied { .. } => println!("GRANT=already-applied"),
    }
    assert_eq!(
        store
            .load(&binding())
            .await
            .unwrap()
            .unwrap()
            .grants()
            .len(),
        1
    );
}

#[tokio::test]
async fn cancelled_acknowledgment_leaves_auditable_receipt_not_a_second_grant() {
    let store = setup();
    let original = grant(2);
    let ack = gate();
    *store.finish_ack_gate.lock().unwrap() = Some(ack.clone());
    let worker_store = store.clone();
    let worker_request = original.clone();
    let worker = tokio::spawn(async move { apply(&worker_store, &worker_request).await });
    entered(&ack).await;
    let committed = store.latest();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(matches!(
        grant_budget(store.as_ref(), &expectations(&limited()), &[], &original)
            .await
            .unwrap(),
        BudgetGrantOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(store.latest(), committed);
    let lease = store.acquire().await.unwrap();
    // Historical receipt remains readable while a new run is claimed, without
    // changing its limits, identity, revision, or frozen status.
    let claim = store.latest();
    assert!(matches!(
        grant_budget(store.as_ref(), &expectations(&limited()), &[], &original)
            .await
            .unwrap(),
        BudgetGrantOutcome::AlreadyApplied { .. }
    ));
    assert_eq!(store.latest(), claim);
    drop(lease);
}

#[tokio::test]
async fn fresh_physical_history_is_required_before_any_new_grant() {
    let fixture = Fixture::new().await;
    let store = setup();
    fixture
        .run(request(store.acquire().await.unwrap(), "step"))
        .await
        .unwrap();
    let before = store.latest();
    let history = fixture.memory.events.lock().unwrap().clone();
    for mode in 0..5 {
        let mut invalid = history.clone();
        let report_index = invalid.len() - 2;
        match mode {
            0 => {
                invalid.pop();
            }
            1 => invalid.last_mut().unwrap().seq += 1,
            2 => {
                let mut later = invalid.last().unwrap().clone();
                later.seq += 1;
                later.event_id = jingwei::id::EventId::new();
                later.kind = SessionEventKind::UserMessage {
                    text: "newer work".into(),
                };
                invalid.push(later);
            }
            _ => {
                if let SessionEventKind::TaskRunReport { report } = &mut invalid[report_index].kind
                {
                    if mode == 3 {
                        report.capabilities_drained = false;
                    } else {
                        report
                            .budget
                            .run
                            .as_mut()
                            .unwrap()
                            .metrics
                            .confirmed
                            .model_requests = 1;
                    }
                } else {
                    panic!()
                }
            }
        }
        assert!(matches!(
            grant_budget(store.as_ref(), &expectations(&before), &invalid, &grant(2)).await,
            Err(BudgetGrantError::Boundary(_))
        ));
        assert_eq!(store.latest(), before);
    }
    fixture.close().await;
}

#[tokio::test]
async fn active_time_grant_preserves_elapsed_time_and_clears_only_its_budget_stop() {
    struct Clock(std::sync::atomic::AtomicU64);
    impl BudgetClock for Clock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }
    let clock = Arc::new(Clock(std::sync::atomic::AtomicU64::new(0)));
    let limits = BudgetLimits {
        active_time: Duration::from_secs(1),
        ..BudgetLimits::default()
    };
    let task = TaskBudget::new(
        binding(),
        limits,
        limits,
        TokenBudgetMode::Hard,
        clock.clone(),
    )
    .unwrap();
    let turn_id = TurnId::new();
    let mut run = task.begin_run(turn_id.clone(), limits).unwrap();
    run.scope().consume_step().unwrap();
    clock.0.store(1000, Ordering::SeqCst);
    assert!(matches!(
        run.scope().check_active(),
        Err(BudgetError::Stopped(BudgetStopReason::ActiveTime))
    ));
    let report = TaskRunReport {
        version: TaskRunReportVersion::V1,
        budget: run.prepare_report().unwrap(),
        stop: TaskRunStop::Budget(BudgetStopReason::ActiveTime),
        capabilities_drained: true,
    };
    run.finish().unwrap();
    let history: Vec<_> = [
        SessionEventKind::TaskRunReport {
            report: Box::new(report),
        },
        SessionEventKind::Error {
            code: "task_budget_stopped".into(),
            message: "time exhausted".into(),
            retryable: false,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(seq, kind)| SessionEvent {
        event_id: jingwei::id::EventId::new(),
        session_id: binding().session_id,
        turn_id: turn_id.clone(),
        generation_id: None,
        message_id: None,
        seq: seq as u64,
        kind,
    })
    .collect();
    let before = task
        .seal_checkpoint(history.last().map(BudgetEventCursor::from))
        .unwrap();
    let store = setup();
    *store.latest.lock().unwrap() = Some(before.clone());
    let mut wrong = grant(129);
    wrong.new_limits.active_time = limits.active_time;
    assert!(matches!(
        grant_budget(store.as_ref(), &expectations(&before), &history, &wrong).await,
        Err(BudgetGrantError::Invalid(_))
    ));
    let mut request = grant(128);
    request.new_limits.active_time = Duration::from_secs(2);
    grant_budget(store.as_ref(), &expectations(&before), &history, &request)
        .await
        .unwrap();
    assert_eq!(store.latest().report().active_time, Duration::from_secs(1));
    assert_eq!(store.latest().report().token_mode, TokenBudgetMode::Hard);
    assert_eq!(store.latest().report().charged, before.report().charged);
    let lease = store.acquire().await.unwrap();
    lease.verify_history(&history).unwrap();
    assert_eq!(lease.task().report().unwrap().stop, None);
}

#[tokio::test]
async fn file_load_rejects_an_audit_rewrite_in_an_otherwise_contiguous_log() {
    use std::io::Write;
    let disk = Disk::new();
    let store = disk.store();
    store.compare_exchange(0, &limited()).await.unwrap();
    grant_budget(store.as_ref(), &expectations(&limited()), &[], &grant(2))
        .await
        .unwrap();
    let before = store.load(&binding()).await.unwrap().unwrap();
    store.close().await;
    let mut corrupt = json!(&before);
    corrupt["revision"] = json!(before.revision() + 1);
    corrupt["grants"][0]["request"]["actor"] = json!("rewritten-operator");
    let path = disk.0.join("budget.jsonl");
    {
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        serde_json::to_writer(
            &mut file,
            &json!({"version":1, "expected_revision":before.revision(), "checkpoint":corrupt}),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
    }
    let bytes = fs::read(&path).unwrap();
    let reopened = disk.store();
    assert!(matches!(
        reopened.load(&binding()).await,
        Err(BudgetCheckpointStoreError::Corrupt { record: 3, .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    reopened.close().await;
}

#[tokio::test]
async fn a_grant_record_cannot_hide_other_ledger_mutations_in_the_same_cas() {
    let memory = setup();
    apply(&memory, &grant(2)).await.unwrap();
    let valid = memory.latest();
    let disk = Disk::new();
    let store = disk.store();
    store.compare_exchange(0, &limited()).await.unwrap();
    let bytes = fs::read(disk.0.join("budget.jsonl")).unwrap();
    for mode in 0..4 {
        let mut candidate = json!(&valid);
        match mode {
            0 => {
                candidate["next_id"] = json!(1);
                candidate["report"]["charged"]["steps"] = json!(1);
            }
            1 => candidate["report"]["token_mode"] = json!(TokenBudgetMode::Hard),
            2 => candidate["next_id"] = json!(99),
            _ => {
                candidate["next_id"] = json!(1);
                candidate["report"]["active_time"] = json!(Duration::from_millis(1));
            }
        }
        let candidate: BudgetCheckpoint = serde_json::from_value(candidate).unwrap();
        candidate.validate().unwrap(); // Individually valid, but not this grant.
        assert!(matches!(
            store.compare_exchange(1, &candidate).await,
            Err(BudgetCheckpointStoreError::Invalid(_))
        ));
        assert_eq!(fs::read(disk.0.join("budget.jsonl")).unwrap(), bytes);
    }
    store.compare_exchange(1, &valid).await.unwrap();
    assert_eq!(store.load(&binding()).await.unwrap(), Some(valid));
    store.close().await;
}

#[test]
fn grant_and_retry_survive_new_processes_and_intervening_execution() {
    let disk = Disk::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        disk.store().compare_exchange(0, &limited()).await.unwrap();
    });
    assert!(
        ChildGuard::spawn(&disk, "grant")
            .finish()
            .contains("GRANT=applied")
    );
    ChildGuard::spawn(&disk, "ask").finish();
    ChildGuard::spawn(&disk, "step").finish();
    let bytes = fs::read(disk.0.join("budget.jsonl")).unwrap();
    let events = fs::read(disk.log_path()).unwrap();
    assert!(
        ChildGuard::spawn(&disk, "grant")
            .finish()
            .contains("GRANT=already-applied")
    );
    assert_eq!(fs::read(disk.0.join("budget.jsonl")).unwrap(), bytes);
    assert_eq!(fs::read(disk.log_path()).unwrap(), events);
    runtime.block_on(async {
        let image = disk.store().load(&binding()).await.unwrap().unwrap();
        assert_eq!(image.report().charged.steps, 2);
        assert_eq!(image.report().limits.resources.steps, 2);
        assert_eq!(image.grants().len(), 1);
    });
}
