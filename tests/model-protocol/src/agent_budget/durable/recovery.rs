use super::*;
use jingwei_journal_jsonl::{JsonlRecoveryOwnership, JsonlSessionPersistence};

struct Evidence {
    session: SessionId,
    events: Vec<SessionEvent>,
}
impl SessionRecoveryOwnership for Evidence {
    fn session_id(&self) -> &SessionId {
        &self.session
    }
    fn ownership_id(&self) -> &str {
        "private-exclusive-owner"
    }
    fn history(&self) -> SessionFuture<'_, Result<Vec<SessionEvent>, SessionPersistenceError>> {
        Box::pin(async { Ok(self.events.clone()) })
    }
}

async fn interrupted(
    uncertain: bool,
) -> (
    Arc<Store>,
    BudgetCheckpoint,
    BudgetRecoveryRequest,
    Arc<Evidence>,
) {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let lease = store.acquire().await.unwrap();
    let claim = store.latest();
    store.fail_finish.store(true, Ordering::SeqCst);
    store.indeterminate.store(uncertain, Ordering::SeqCst);
    let error = fixture.run(request(lease, "step")).await.unwrap_err();
    let AgentRuntimeError::Durability {
        source: BudgetExecutionError::Commit { checkpoint, .. },
        ..
    } = error
    else {
        panic!("missing exact candidate");
    };
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 1);
    let evidence = Evidence {
        session: binding().session_id,
        events: fixture.memory.events.lock().unwrap().clone(),
    };
    fixture.close().await;
    store.fail_finish.store(false, Ordering::SeqCst);
    store.indeterminate.store(false, Ordering::SeqCst);
    let request = BudgetRecoveryRequest {
        operation_id: jingwei_core::BudgetOperationId::new(),
        execution_id: claim.execution_id().unwrap().clone(),
        source_revision: claim.revision(),
        actor: "host-operator".into(),
        source: "private recovery test".into(),
        reason: "final checkpoint acknowledgment failed".into(),
    };
    (store, *checkpoint, request, Arc::new(evidence))
}

#[tokio::test]
async fn exact_candidate_recovers_once_and_audit_survives_next_execution() {
    let (store, candidate, request, evidence) = interrupted(false).await;
    assert_eq!(
        inspect_budget_recovery(&store.latest(), evidence.as_ref())
            .await
            .unwrap(),
        BudgetRecoveryAssessment::FrozenClosedTailNeedsCandidate
    );
    let BudgetRecoveryOutcome::Applied(recovered) =
        recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
            .await
            .unwrap()
    else {
        panic!("not applied");
    };
    assert_eq!(recovered.report(), candidate.report());
    assert_eq!(recovered.recoveries().len(), 1);
    assert_eq!(recovered.recoveries()[0].request, request);
    assert!(matches!(
        recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
            .await
            .unwrap(),
        BudgetRecoveryOutcome::AlreadyApplied(_)
    ));
    let lease = store.acquire().await.unwrap();
    lease.verify_history(&evidence.events).unwrap();
    let fixture = Fixture::new().await;
    *fixture.memory.events.lock().unwrap() = evidence.events.clone();
    fixture.run(super::request(lease, "step")).await.unwrap();
    assert_eq!(store.latest().report().charged.steps, 2);
    assert_eq!(store.latest().recoveries(), recovered.recoveries());
    // A historical receipt is not a restored lease or a second authorization.
    assert!(matches!(
        recover_budget_candidate(store.clone(), evidence.clone(), request, &candidate)
            .await
            .unwrap(),
        BudgetRecoveryOutcome::AlreadyApplied(_)
    ));
    fixture.close().await;
}

#[tokio::test]
async fn original_commit_and_uncertain_recovery_are_idempotent() {
    let (store, candidate, request, evidence) = interrupted(true).await;
    assert!(matches!(
        recover_budget_candidate(store.clone(), evidence.clone(), request, &candidate)
            .await
            .unwrap(),
        BudgetRecoveryOutcome::OriginalCommitted(_)
    ));
    assert!(store.latest().recoveries().is_empty());
    let (store, candidate, request, evidence) = interrupted(false).await;
    store.fail_finish.store(true, Ordering::SeqCst);
    store.indeterminate.store(true, Ordering::SeqCst);
    assert!(matches!(
        recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
            .await,
        Err(BudgetRecoveryError::Commit { .. })
    ));
    store.fail_finish.store(false, Ordering::SeqCst);
    assert!(matches!(
        recover_budget_candidate(store.clone(), evidence.clone(), request, &candidate)
            .await
            .unwrap(),
        BudgetRecoveryOutcome::AlreadyApplied(_)
    ));
    assert_eq!(store.latest().recoveries().len(), 1);
}

#[tokio::test]
async fn mismatched_claim_session_history_and_reused_operation_fail_closed() {
    let (store, candidate, request, mut evidence) = interrupted(false).await;
    let claim = store.latest();
    let mut wrong = request.clone();
    wrong.execution_id = jingwei_core::BudgetExecutionId::new();
    assert!(
        recover_budget_candidate(store.clone(), evidence.clone(), wrong, &candidate)
            .await
            .is_err()
    );
    Arc::get_mut(&mut evidence).unwrap().session = SessionId::new();
    assert!(
        recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
            .await
            .is_err()
    );
    Arc::get_mut(&mut evidence).unwrap().session = binding().session_id;
    let terminal = Arc::get_mut(&mut evidence).unwrap().events.pop().unwrap();
    assert_eq!(
        inspect_budget_recovery(&claim, evidence.as_ref())
            .await
            .unwrap(),
        BudgetRecoveryAssessment::FrozenUnconfirmedWork
    );
    assert!(
        recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
            .await
            .is_err()
    );
    Arc::get_mut(&mut evidence).unwrap().events.push(terminal);
    assert_eq!(store.latest(), claim);
    recover_budget_candidate(store.clone(), evidence.clone(), request.clone(), &candidate)
        .await
        .unwrap();
    let committed = store.latest();
    let mut wrong = request;
    wrong.reason = "changed evidence".into();
    assert!(
        recover_budget_candidate(store.clone(), evidence.clone(), wrong, &candidate)
            .await
            .is_err()
    );
    assert_eq!(store.latest(), committed);
    let mut tampered = json!(&committed);
    tampered["revision"] = json!(committed.revision() + 1);
    tampered["recoveries"][0]["request"]["reason"] = json!("rewritten");
    let tampered: BudgetCheckpoint = serde_json::from_value(tampered).unwrap();
    assert!(tampered.validate_transition(Some(&committed)).is_err());
}

#[tokio::test]
async fn no_new_events_never_clears_a_claim() {
    let store = Store::new();
    let lease = store.acquire().await.unwrap();
    drop(lease);
    let claim = store.latest();
    let evidence = Arc::new(Evidence {
        session: binding().session_id,
        events: vec![],
    });
    assert_eq!(
        inspect_budget_recovery(&claim, evidence.as_ref())
            .await
            .unwrap(),
        BudgetRecoveryAssessment::FrozenNoNewEvents
    );
    assert_eq!(store.latest(), claim);
    assert!(store.acquire().await.is_err());
}

#[tokio::test]
async fn jsonl_ownership_spans_clones_and_blocks_recovery_until_drop() {
    let disk = Disk::new();
    let session = binding().session_id;
    let writer = JsonlSessionPersistence::new(&disk.0);
    writer.load(&session).await.unwrap();
    let clone = writer.clone();
    drop(writer);
    assert!(
        JsonlRecoveryOwnership::acquire(&disk.0, session.clone())
            .await
            .is_err()
    );
    // Unrelated Session writers remain independent.
    JsonlSessionPersistence::new(&disk.0)
        .load(&SessionId::new())
        .await
        .unwrap();
    drop(clone);
    let owner = JsonlRecoveryOwnership::acquire(&disk.0, session.clone())
        .await
        .unwrap();
    assert!(
        JsonlSessionPersistence::new(&disk.0)
            .load(&session)
            .await
            .is_err()
    );
    assert!(owner.history().await.unwrap().is_empty());
    drop(owner);
    JsonlSessionPersistence::new(&disk.0)
        .load(&session)
        .await
        .unwrap();
}

#[tokio::test]
async fn disk_recovery_is_readable_in_a_fresh_process() {
    let (memory, candidate, request, evidence) = interrupted(false).await;
    let disk = Disk::new();
    let store = disk.store();
    store.compare_exchange(0, &initial()).await.unwrap();
    store.compare_exchange(1, &memory.latest()).await.unwrap();
    let writer = JsonlSessionPersistence::new(&disk.0);
    for event in &evidence.events {
        writer.commit_durable(event).await.unwrap();
    }
    drop(writer);
    let owner = JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
        .await
        .unwrap();
    recover_budget_candidate(store.clone(), Arc::new(owner), request, &candidate)
        .await
        .unwrap();
    store.close().await;
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_budget::durable::recovery::recovery_child",
            "--nocapture",
        ])
        .env("JINGWEI_RECOVERY_ROOT", &disk.0)
        .env("JINGWEI_RECOVERY_MODE", "read")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn recovery_child() {
    let Ok(root) = std::env::var("JINGWEI_RECOVERY_ROOT") else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let owner = JsonlRecoveryOwnership::acquire(&root, binding().session_id)
            .await
            .unwrap();
        if std::env::var("JINGWEI_RECOVERY_MODE").unwrap() == "hold" {
            fs::write(PathBuf::from(&root).join("ready"), "locked").unwrap();
            std::future::pending::<()>().await;
        }
        let store = FileBudgetCheckpointStore::open(
            PathBuf::from(root).join("budget.jsonl"),
            FileBudgetCheckpointConfig::default(),
        )
        .unwrap();
        let checkpoint = store.load(&binding()).await.unwrap().unwrap();
        assert_eq!(checkpoint.recoveries().len(), 1);
        assert_eq!(checkpoint.report().charged.steps, 1);
        assert_eq!(
            inspect_budget_recovery(&checkpoint, &owner).await.unwrap(),
            BudgetRecoveryAssessment::Ready
        );
        let lease = BudgetExecutionLease::acquire(
            Arc::new(store),
            expectations(&checkpoint),
            Arc::new(MonotonicBudgetClock::default()),
        )
        .await
        .unwrap();
        lease
            .verify_history(&owner.history().await.unwrap())
            .unwrap();
    });
}

#[tokio::test]
async fn live_process_blocks_ownership_and_death_releases_it() {
    let disk = Disk::new();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_budget::durable::recovery::recovery_child",
            "--nocapture",
        ])
        .env("JINGWEI_RECOVERY_ROOT", &disk.0)
        .env("JINGWEI_RECOVERY_MODE", "hold")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard = ChildGuard(Some(child));
    let start = Instant::now();
    while !disk.0.join("ready").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
            .await
            .is_err()
    );
    drop(guard);
    JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn competing_recoveries_cannot_authorize_twice() {
    let (store, candidate, request, evidence) = interrupted(false).await;
    let mut competitor = request.clone();
    competitor.operation_id = jingwei_core::BudgetOperationId::new();
    let (a, b) = tokio::join!(
        recover_budget_candidate(store.clone(), evidence.clone(), request, &candidate),
        recover_budget_candidate(store.clone(), evidence.clone(), competitor, &candidate),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(store.latest().recoveries().len(), 1);
}

#[tokio::test]
async fn recovery_audit_bound_refuses_without_discarding_prior_records() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    for index in 0..=MAX_BUDGET_RECOVERIES {
        let lease = store.acquire().await.unwrap();
        let claim = store.latest();
        store.fail_finish.store(true, Ordering::SeqCst);
        let error = fixture.run(request(lease, "step")).await.unwrap_err();
        let AgentRuntimeError::Durability {
            source: BudgetExecutionError::Commit { checkpoint, .. },
            ..
        } = error
        else {
            panic!("missing candidate");
        };
        store.fail_finish.store(false, Ordering::SeqCst);
        let evidence = Arc::new(Evidence {
            session: binding().session_id,
            events: fixture.memory.events.lock().unwrap().clone(),
        });
        let request = BudgetRecoveryRequest {
            operation_id: jingwei_core::BudgetOperationId::new(),
            execution_id: claim.execution_id().unwrap().clone(),
            source_revision: claim.revision(),
            actor: "host".into(),
            source: "test".into(),
            reason: "retry".into(),
        };
        let result = recover_budget_candidate(store.clone(), evidence, request, &checkpoint).await;
        if index < MAX_BUDGET_RECOVERIES {
            result.unwrap();
            assert_eq!(store.latest().recoveries().len(), index + 1);
        } else {
            assert!(matches!(
                result,
                Err(BudgetRecoveryError::Refused("recovery audit full"))
            ));
            assert_eq!(store.latest(), claim);
            assert!(store.acquire().await.is_err());
        }
    }
    fixture.close().await;
}

#[tokio::test]
async fn grant_after_recovery_preserves_evidence_and_boundary() {
    let (store, candidate, request, evidence) = interrupted(false).await;
    recover_budget_candidate(store.clone(), evidence.clone(), request, &candidate)
        .await
        .unwrap();
    let before = store.latest();
    let mut limits = before.report().limits;
    limits.resources.steps += 1;
    let request = BudgetGrantRequest {
        operation_id: jingwei_core::BudgetOperationId::new(),
        actor: "host".into(),
        source: "test".into(),
        reason: "more steps".into(),
        new_limits: limits,
    };
    let mut expected = expectations(&before);
    expected.host_limits = limits;
    grant_budget(store.as_ref(), &expected, &evidence.events, &request)
        .await
        .unwrap();
    let after = store.latest();
    assert_eq!(after.recoveries(), before.recoveries());
    assert_eq!(after.report().usage, before.report().usage);
    assert_eq!(json!(&after)["version"], 4);
    assert_eq!(
        inspect_budget_recovery(&after, evidence.as_ref())
            .await
            .unwrap(),
        BudgetRecoveryAssessment::Ready
    );
}

#[test]
fn cancelled_file_commit_retains_owner_until_accepted_worker_finishes() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let disk = Disk::new();
            let store = disk.store();
            let owner = Arc::new(
                JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
                    .await
                    .unwrap(),
            );
            let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            let blocker_release = release.clone();
            let (entered, started) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                let (lock, ready) = &*blocker_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
            });
            started.await.unwrap();
            let worker_store = store.clone();
            let worker_owner = owner.clone();
            let waiter = tokio::spawn(async move {
                worker_store
                    .compare_exchange_guarded(0, &initial(), worker_owner)
                    .await
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while store.status().in_flight == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            let weak = Arc::downgrade(&owner);
            drop(owner);
            let retained = weak.upgrade().is_some();
            // Release before assertions so a failure cannot hang runtime shutdown.
            *release.0.lock().unwrap() = true;
            release.1.notify_all();
            blocker.await.unwrap();
            store.close().await;
            assert!(retained, "accepted IO discarded its ownership guard");
            assert!(weak.upgrade().is_none());
            JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
                .await
                .unwrap();
            assert_eq!(
                disk.store().load(&binding()).await.unwrap(),
                Some(initial())
            );
        });
}
