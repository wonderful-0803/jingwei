//! Durable execution integration, including real-process crash and restart.
use super::*;
use jingwei_budget_file::{FileBudgetCheckpointConfig, FileBudgetCheckpointStore};
use jingwei_journal_jsonl::{JsonlSessionPersistencePlugin, session_file_path};
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Instant;
use tokio::sync::Barrier;

mod grants;

fn binding() -> BudgetIdentity {
    BudgetIdentity {
        task_id: TaskId::from("durable-task"),
        session_id: SessionId::from("durable-session"),
        agent_key: "probe".into(),
    }
}

fn initial() -> BudgetCheckpoint {
    TaskBudget::new(
        binding(),
        BudgetLimits::default(),
        BudgetLimits::default(),
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap()
    .seal_checkpoint(None)
    .unwrap()
}

fn expectations(checkpoint: &BudgetCheckpoint) -> BudgetRestoreContext {
    // Fixtures know expected state; runtime independently checks canonical history.
    BudgetRestoreContext {
        identity: binding(),
        revision: checkpoint.revision(),
        confirmed_anchor: checkpoint.anchor().cloned(),
        host_limits: BudgetLimits::default(),
    }
}

fn gate() -> Arc<ReportGate> {
    Arc::new(ReportGate {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    })
}

async fn entered(gate: &ReportGate) {
    tokio::time::timeout(Duration::from_secs(5), gate.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

struct Store {
    latest: Mutex<Option<BudgetCheckpoint>>,
    claims: Mutex<Vec<BudgetCheckpoint>>,
    race: Mutex<Option<Arc<Barrier>>>,
    fail_claim: AtomicBool,
    fail_finish: AtomicBool,
    indeterminate: AtomicBool,
    claim_ack_gate: Mutex<Option<Arc<ReportGate>>>,
    finish_gate: Mutex<Option<Arc<ReportGate>>>,
    finish_ack_gate: Mutex<Option<Arc<ReportGate>>>,
}

impl Store {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            latest: Mutex::new(Some(initial())),
            claims: Mutex::new(vec![]),
            race: Mutex::new(None),
            fail_claim: AtomicBool::new(false),
            fail_finish: AtomicBool::new(false),
            indeterminate: AtomicBool::new(false),
            claim_ack_gate: Mutex::new(None),
            finish_gate: Mutex::new(None),
            finish_ack_gate: Mutex::new(None),
        })
    }
    fn latest(&self) -> BudgetCheckpoint {
        self.latest.lock().unwrap().clone().unwrap()
    }
    async fn acquire(self: &Arc<Self>) -> Result<BudgetExecutionLease, BudgetExecutionError> {
        BudgetExecutionLease::acquire(
            self.clone(),
            expectations(&self.latest()),
            Arc::new(MonotonicBudgetClock::default()),
        )
        .await
    }
}

impl BudgetCheckpointStore for Store {
    fn load<'a>(
        &'a self,
        _: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>, BudgetCheckpointStoreError>>
    {
        Box::pin(async { Ok(self.latest.lock().unwrap().clone()) })
    }
    fn compare_exchange<'a>(
        &'a self,
        expected: u64,
        checkpoint: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>>
    {
        Box::pin(async move {
            checkpoint.validate_successor(expected)?;
            let is_claim = checkpoint.execution_id().is_some();
            if is_claim {
                self.claims.lock().unwrap().push(checkpoint.clone());
                let race = self.race.lock().unwrap().clone();
                if let Some(race) = race {
                    race.wait().await;
                }
            } else {
                let gate = self.finish_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                }
            }
            let fail = if is_claim {
                self.fail_claim.load(Ordering::SeqCst)
            } else {
                self.fail_finish.load(Ordering::SeqCst)
            };
            let indeterminate = self.indeterminate.load(Ordering::SeqCst);
            if !fail || indeterminate {
                let mut latest = self.latest.lock().unwrap();
                if latest.as_ref() == Some(checkpoint) {
                    return Ok(BudgetCheckpointCommit::ReplayedExact);
                }
                let actual = latest.as_ref().map_or(0, BudgetCheckpoint::revision);
                if actual != expected {
                    return Err(BudgetCheckpointStoreError::Conflict { expected, actual });
                }
                checkpoint.validate_transition(latest.as_ref())?;
                *latest = Some(checkpoint.clone());
            }
            if is_claim {
                let gate = self.claim_ack_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                }
            } else {
                let gate = self.finish_ack_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.entered.add_permits(1);
                    gate.release.acquire().await.unwrap().forget();
                }
            }
            if fail {
                return Err(BudgetCheckpointStoreError::Storage {
                    certainty: if indeterminate {
                        BudgetCheckpointCommitCertainty::Indeterminate
                    } else {
                        BudgetCheckpointCommitCertainty::DefinitelyNotCommitted
                    },
                    message: "private checkpoint barrier fault".into(),
                });
            }
            Ok(BudgetCheckpointCommit::Committed)
        })
    }
}

fn request(lease: BudgetExecutionLease, message: &str) -> AgentTurnRequest {
    AgentTurnRequest::new(binding().session_id, "probe", message)
        .with_durable_budget(lease, BudgetLimits::default())
}

#[tokio::test]
async fn claim_is_durable_unique_and_cannot_be_restored_as_execution_authority() {
    let store = Store::new();
    let lease = store.acquire().await.unwrap();
    assert_eq!(lease.claim(), &store.latest());
    assert_eq!(lease.claim().revision(), 2);
    assert_eq!(lease.claim().execution_id(), Some(lease.execution_id()));
    let clone = lease.task().clone();
    let restored = TaskBudget::restore_checkpoint(
        store.latest(),
        expectations(&store.latest()),
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    assert!(matches!(
        restored.begin_admission(BudgetLimits::default()),
        Err(BudgetError::Stopped(BudgetStopReason::RecoveryRequired))
    ));
    assert!(matches!(
        store.acquire().await,
        Err(BudgetExecutionError::RecoveryRequired)
    ));
    drop(lease);
    assert!(matches!(
        clone.begin_admission(BudgetLimits::default()),
        Err(BudgetError::CheckpointSealed)
    ));
    assert!(store.latest().execution_id().is_some());
}

#[tokio::test]
async fn racing_acquisitions_never_share_an_exact_retry_claim() {
    let store = Store::new();
    *store.race.lock().unwrap() = Some(Arc::new(Barrier::new(2)));
    let (left, right) = tokio::join!(store.acquire(), store.acquire());
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert!(matches!(
        left.as_ref().err().or(right.as_ref().err()),
        Some(BudgetExecutionError::Commit {
            source: BudgetCheckpointStoreError::Conflict {
                expected: 1,
                actual: 2
            },
            ..
        })
    ));
    let claims = store.claims.lock().unwrap();
    assert_eq!(claims.len(), 2);
    assert_ne!(claims[0].execution_id(), claims[1].execution_id());
}

#[tokio::test]
async fn missing_or_stale_state_never_initializes_a_task() {
    let store = Store::new();
    let mut stale = expectations(&initial());
    stale.revision = 9;
    assert!(matches!(
        BudgetExecutionLease::acquire(
            store.clone(),
            stale,
            Arc::new(MonotonicBudgetClock::default())
        )
        .await,
        Err(BudgetExecutionError::Checkpoint(
            BudgetCheckpointError::RevisionMismatch { .. }
        ))
    ));
    *store.latest.lock().unwrap() = None;
    assert!(matches!(
        BudgetExecutionLease::acquire(
            store.clone(),
            expectations(&initial()),
            Arc::new(MonotonicBudgetClock::default())
        )
        .await,
        Err(BudgetExecutionError::Missing)
    ));
    assert!(store.claims.lock().unwrap().is_empty());
}

#[tokio::test]
async fn v1_ready_images_upgrade_but_cannot_carry_claims() {
    let store = Store::new();
    let mut old = json!(initial());
    old["version"] = json!(1);
    *store.latest.lock().unwrap() = Some(serde_json::from_value(old).unwrap());
    let lease = store.acquire().await.unwrap();
    assert_eq!(json!(lease.claim())["version"], 3);
    for (field, value) in [
        ("version", json!(1)),
        ("recovery_frozen", json!(false)),
        ("execution_id", json!(" ")),
    ] {
        let mut wire = json!(lease.claim());
        wire[field] = value;
        let image: BudgetCheckpoint = serde_json::from_value(wire).unwrap();
        assert!(image.validate().is_err());
    }
}

#[tokio::test]
async fn failed_and_indeterminate_claims_never_deliver_a_lease() {
    for uncertain in [false, true] {
        let store = Store::new();
        store.fail_claim.store(true, Ordering::SeqCst);
        store.indeterminate.store(uncertain, Ordering::SeqCst);
        assert!(matches!(
            store.acquire().await,
            Err(BudgetExecutionError::Commit {
                source: BudgetCheckpointStoreError::Storage { .. },
                ..
            })
        ));
        assert_eq!(store.latest().execution_id().is_some(), uncertain);
    }
}

#[tokio::test]
async fn cancelling_claim_waiter_does_not_erase_a_committed_claim() {
    let store = Store::new();
    let ack = gate();
    *store.claim_ack_gate.lock().unwrap() = Some(ack.clone());
    let worker_store = store.clone();
    let acquire = tokio::spawn(async move { worker_store.acquire().await });
    entered(&ack).await;
    acquire.abort();
    assert!(matches!(acquire.await, Err(error) if error.is_cancelled()));
    assert!(store.latest().execution_id().is_some());
    assert!(matches!(
        store.acquire().await,
        Err(BudgetExecutionError::RecoveryRequired)
    ));
}

#[tokio::test]
async fn canonical_ask_and_next_turn_reacquire_the_remaining_task_budget() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    for (index, message) in ["ask", "step", "step"].into_iter().enumerate() {
        let report = fixture
            .run(request(store.acquire().await.unwrap(), message))
            .await
            .unwrap();
        assert_eq!(
            report.disposition(),
            if message == "ask" {
                TurnDisposition::WaitingForInput
            } else {
                TurnDisposition::Completed
            }
        );
        let latest = store.latest();
        assert_eq!(report.budget_checkpoint(), Some(&latest));
        assert!(latest.execution_id().is_none());
        assert_eq!(latest.revision(), 3 + 2 * index as u64);
        assert_eq!(latest.report().charged.steps, index as u64 + 1);
        assert_eq!(
            latest.anchor(),
            report.events().last().map(BudgetEventCursor::from).as_ref()
        );
    }
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 3);
    fixture.close().await;
}

#[tokio::test]
async fn unexpected_history_rejects_before_user_envelope_or_agent_body() {
    let fixture = Fixture::new().await;
    fixture.memory.events.lock().unwrap().push(SessionEvent {
        event_id: jingwei::id::EventId::new(),
        session_id: binding().session_id,
        turn_id: TurnId::new(),
        generation_id: None,
        message_id: None,
        seq: 0,
        kind: SessionEventKind::UserMessage {
            text: "unaccounted history".into(),
        },
    });
    let store = Store::new();
    assert!(matches!(
        fixture
            .run(request(store.acquire().await.unwrap(), "step"))
            .await,
        Err(AgentRuntimeError::RecoveryBoundary {
            settlement: Ok(()),
            ..
        })
    ));
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.memory.events.lock().unwrap().len(), 1);
    assert!(store.latest().execution_id().is_some());
    fixture.close().await;
}

#[tokio::test]
async fn invalid_tail_addresses_or_report_payload_cannot_authorize_recovery() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    fixture
        .run(request(store.acquire().await.unwrap(), "step"))
        .await
        .unwrap();
    let lease = store.acquire().await.unwrap();
    let history = fixture.memory.events.lock().unwrap().clone();
    lease.verify_history(&history).unwrap();
    let mut wrong = history.clone();
    wrong[0].seq = 9;
    assert!(lease.verify_history(&wrong).is_err());
    let mut wrong = history.clone();
    wrong[1].event_id = wrong[0].event_id.clone();
    assert!(lease.verify_history(&wrong).is_err());
    for unconfirmed in [false, true] {
        let mut wrong = history.clone();
        let index = wrong.len() - 2;
        if let SessionEventKind::TaskRunReport { report } = &mut wrong[index].kind {
            let metrics = &mut report.budget.run.as_mut().unwrap().metrics;
            if unconfirmed {
                metrics.unconfirmed.tool_results = 1;
            } else {
                metrics.confirmed.model_requests = 1;
            }
        }
        assert!(lease.verify_history(&wrong).is_err());
    }
    let mut wrong = history;
    let index = wrong.len() - 2;
    if let SessionEventKind::TaskRunReport { report } = &mut wrong[index].kind {
        report.budget.charged.steps = 0;
    }
    assert!(lease.verify_history(&wrong).is_err());
    fixture.close().await;
}

#[tokio::test]
async fn failed_report_barrier_retains_claim_and_typed_session_failure() {
    let fixture = Fixture::new().await;
    fixture.memory.fail_report.store(true, Ordering::SeqCst);
    let store = Store::new();
    let error = fixture
        .run(request(store.acquire().await.unwrap(), "step"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, AgentRuntimeError::Durability { outcome, .. } if matches!(*outcome, Err(AgentRuntimeError::Turn(_))))
    );
    assert!(store.latest().execution_id().is_some());
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 1);
    fixture.close().await;
}

#[tokio::test]
async fn final_checkpoint_failure_preserves_the_already_completed_turn() {
    for uncertain in [false, true] {
        let fixture = Fixture::new().await;
        let store = Store::new();
        let lease = store.acquire().await.unwrap();
        let old = lease.task().clone();
        store.fail_finish.store(true, Ordering::SeqCst);
        store.indeterminate.store(uncertain, Ordering::SeqCst);
        let error = fixture.run(request(lease, "step")).await.unwrap_err();
        let AgentRuntimeError::Durability {
            source:
                BudgetExecutionError::Commit {
                    expected_revision,
                    checkpoint,
                    source: BudgetCheckpointStoreError::Storage { certainty, .. },
                },
            outcome,
        } = error
        else {
            panic!("wrong failure");
        };
        assert_eq!(expected_revision, 2);
        assert_eq!(checkpoint.revision(), 3);
        assert_eq!(checkpoint.report().charged.steps, 1);
        assert_eq!(
            certainty,
            if uncertain {
                BudgetCheckpointCommitCertainty::Indeterminate
            } else {
                BudgetCheckpointCommitCertainty::DefinitelyNotCommitted
            }
        );
        assert_eq!(
            outcome
                .unwrap()
                .task_run_report()
                .unwrap()
                .budget
                .charged
                .steps,
            1
        );
        assert!(old.report().is_err());
        assert_eq!(store.latest().execution_id().is_none(), uncertain);
        assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 1);
        store.fail_finish.store(false, Ordering::SeqCst);
        store.indeterminate.store(false, Ordering::SeqCst);
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
        assert_eq!(store.latest(), *checkpoint);
        fixture.close().await;
    }
}

#[tokio::test]
async fn detached_completion_and_shutdown_wait_for_checkpoint_barrier() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let finish = gate();
    *store.finish_gate.lock().unwrap() = Some(finish.clone());
    let controller = fixture
        .runtime
        .start_turn(request(store.acquire().await.unwrap(), "step"))
        .unwrap();
    drop(controller);
    entered(&finish).await;
    assert!(store.latest().execution_id().is_some());
    let mut shutdown = Box::pin(fixture.registry.shutdown());
    assert!(futures::poll!(shutdown.as_mut()).is_pending());
    finish.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap();
    assert!(store.latest().execution_id().is_none());
    assert_eq!(store.latest().report().charged.steps, 1);
}

#[tokio::test]
async fn detached_checkpoint_failure_is_reported_by_shutdown() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let finish = gate();
    *store.finish_gate.lock().unwrap() = Some(finish.clone());
    store.fail_finish.store(true, Ordering::SeqCst);
    drop(
        fixture
            .runtime
            .start_turn(request(store.acquire().await.unwrap(), "step"))
            .unwrap(),
    );
    entered(&finish).await;
    finish.release.add_permits(1);
    assert!(fixture.registry.shutdown().await.is_err());
    assert!(store.latest().execution_id().is_some());
}

#[tokio::test]
async fn shutdown_reports_detached_claim_abandoned_before_session_admission() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let blocker = fixture
        .runtime
        .start_turn(AgentTurnRequest::new(binding().session_id, "probe", "hold"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), fixture.probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let waiting = fixture
        .runtime
        .start_turn(request(store.acquire().await.unwrap(), "step"))
        .unwrap();
    drop(waiting);
    drop(blocker);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), fixture.registry.shutdown())
            .await
            .unwrap()
            .is_err()
    );
    assert!(store.latest().execution_id().is_some());
    assert_eq!(fixture.probe.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_runtime_admission_revokes_memory_without_clearing_claim() {
    let fixture = Fixture::new().await;
    let store = Store::new();
    let lease = store.acquire().await.unwrap();
    let clone = lease.task().clone();
    let rejected = AgentTurnRequest::new(binding().session_id, "missing", "step")
        .with_durable_budget(lease, BudgetLimits::default());
    assert!(matches!(
        fixture.runtime.start_turn(rejected),
        Err(AgentRuntimeError::AgentNotFound { .. })
    ));
    assert!(clone.report().is_err());
    assert!(store.latest().execution_id().is_some());
    fixture.close().await;
}

struct Disk(PathBuf);
impl Disk {
    fn log_path(&self) -> PathBuf {
        session_file_path(&self.0, &binding().session_id)
    }
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "jingwei-durable-{}-{}",
            std::process::id(),
            TaskId::new()
        ));
        fs::create_dir(&root).unwrap();
        File::create_new(root.join("budget.jsonl"))
            .unwrap()
            .sync_all()
            .unwrap();
        Self(root)
    }
    fn store(&self) -> Arc<FileBudgetCheckpointStore> {
        Arc::new(
            FileBudgetCheckpointStore::open(
                self.0.join("budget.jsonl"),
                FileBudgetCheckpointConfig::default(),
            )
            .unwrap(),
        )
    }
}
impl Drop for Disk {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.log_path());
        for name in ["budget.jsonl", "ready"] {
            let _ = fs::remove_file(self.0.join(name));
        }
        let _ = fs::remove_dir(&self.0);
    }
}

struct ChildGuard(Option<Child>);
impl ChildGuard {
    fn spawn(disk: &Disk, mode: &str) -> Self {
        Self(Some(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent_budget::durable::durable_child_worker",
                    "--nocapture",
                ])
                .env("JINGWEI_DURABLE_TEST_ROOT", &disk.0)
                .env("JINGWEI_DURABLE_TEST_MODE", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }
    fn finish(mut self) -> String {
        let started = Instant::now();
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "child timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = self.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn durable_child_worker() {
    let Some(root) = std::env::var_os("JINGWEI_DURABLE_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("JINGWEI_DURABLE_TEST_MODE").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Arc::new(
            FileBudgetCheckpointStore::open(
                root.join("budget.jsonl"),
                FileBudgetCheckpointConfig::default(),
            )
            .unwrap(),
        );
        let image = match store.load(&binding()).await {
            Err(BudgetCheckpointStoreError::Busy) if mode == "compete" => { println!("CLAIM=blocked"); return; }
            result => result.unwrap().unwrap(),
        };
        if mode == "grant" {
            grants::child_grant(&store).await;
            store.close().await;
            return;
        }
        let lease = BudgetExecutionLease::acquire(
            store.clone(),
            expectations(&image),
            Arc::new(MonotonicBudgetClock::default()),
        )
        .await;
        if mode == "compete" {
            match lease {
                Ok(lease) => { println!("CLAIM=owned"); drop(lease); }
                Err(BudgetExecutionError::RecoveryRequired)
                | Err(BudgetExecutionError::Store(BudgetCheckpointStoreError::Busy))
                | Err(BudgetExecutionError::Commit { source: BudgetCheckpointStoreError::Conflict { .. } | BudgetCheckpointStoreError::Busy, .. }) => println!("CLAIM=blocked"),
                Err(error) => panic!("unexpected claim error: {error}"),
            }
            return;
        }
        if mode == "verify_frozen" {
            assert!(matches!(lease, Err(BudgetExecutionError::RecoveryRequired)));
            assert!(image.execution_id().is_some());
            assert_eq!(image.report().charged.steps, 2);
            return;
        }
        let lease = lease.unwrap();
        let probe = Arc::new(Probe::default());
        let model = Arc::new(Model::default());
        let mut registrar = Registrar::default();
        registrar.add(JsonlSessionPersistencePlugin::new(&root));
        registrar.add(ProbePlugin(probe.clone(), true));
        registrar.add(ModelPlugin(model.clone()));
        registrar.add(CanonicalLlmRuntimePlugin::new());
        registrar.add(CanonicalSessionRuntimePlugin::new());
        registrar.add(CanonicalAgentRuntimePlugin::new());
        registrar.require(AGENT_RUNTIME);
        let registry = registrar.finish().await.unwrap();
        let controller = registry
            .agent_runtime()
            .unwrap()
            .start_turn(request(lease, &mode))
            .unwrap();
        if mode == "hold_after_model" {
            tokio::time::timeout(Duration::from_secs(5), probe.entered.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
            assert_eq!(model.0.load(Ordering::SeqCst), 1);
            File::create_new(root.join("ready"))
                .unwrap()
                .sync_all()
                .unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            panic!("parent must terminate this test child");
        }
        let report = controller.wait().await.unwrap();
        assert_eq!(
            report.task_run_report().unwrap().budget.charged.steps,
            if mode == "ask" { 1 } else { 2 }
        );
        registry.shutdown().await.unwrap();
        store.close().await;
    });
}

#[test]
fn new_processes_continue_budget_then_refuse_to_replay_interrupted_model_turn() {
    let disk = Disk::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        disk.store().compare_exchange(0, &initial()).await.unwrap();
    });
    ChildGuard::spawn(&disk, "ask").finish();
    ChildGuard::spawn(&disk, "step").finish();
    let child = ChildGuard::spawn(&disk, "hold_after_model");
    let started = Instant::now();
    while !disk.0.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "runtime never reached model completion"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(child); // Only the child created by this fixture is terminated.
    let budget_before = fs::read(disk.0.join("budget.jsonl")).unwrap();
    let log_before = fs::read(disk.log_path()).unwrap();
    ChildGuard::spawn(&disk, "verify_frozen").finish();
    assert_eq!(
        fs::read(disk.0.join("budget.jsonl")).unwrap(),
        budget_before
    );
    assert_eq!(fs::read(disk.log_path()).unwrap(), log_before);
}

#[tokio::test]
async fn mismatched_report_receipts_do_not_clear_durable_ownership() {
    for fault in [
        SessionFault::ReportSession,
        SessionFault::ReportTurn,
        SessionFault::ReportKind,
        SessionFault::ReportPayload,
        SessionFault::ReportEventId,
    ] {
        let (registry, runtime, sessions, probe) = faulty_session_fixture(fault).await;
        let store = Store::new();
        let result = runtime
            .start_turn(request(store.acquire().await.unwrap(), "step"))
            .unwrap()
            .wait()
            .await;
        assert!(matches!(result, Err(AgentRuntimeError::Durability { .. })));
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(sessions.settled.load(Ordering::SeqCst), 1);
        assert!(store.latest().execution_id().is_some());
        registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn terminal_and_settlement_receipts_must_confirm_the_same_closed_slice() {
    for fault in [
        SessionFault::TerminalKind,
        SessionFault::SettlementOmitReport,
        SessionFault::SettlementFail,
    ] {
        let (registry, runtime, sessions, _) = faulty_session_fixture(fault).await;
        let store = Store::new();
        let result = runtime
            .start_turn(request(store.acquire().await.unwrap(), "step"))
            .unwrap()
            .wait()
            .await;
        assert!(matches!(result, Err(AgentRuntimeError::Durability { .. })));
        assert_eq!(sessions.settled.load(Ordering::SeqCst), 1);
        assert!(store.latest().execution_id().is_some());
        registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn unfinished_or_mismatched_memory_never_replaces_the_claim() {
    let store = Store::new();
    let lease = store.acquire().await.unwrap();
    let task = lease.task().clone();
    let turn_id = TurnId::new();
    let mut run = task
        .begin_run(turn_id.clone(), BudgetLimits::default())
        .unwrap();
    let report = TaskRunReport {
        version: TaskRunReportVersion::V1,
        budget: run.prepare_report().unwrap(),
        stop: TaskRunStop::Completed,
        capabilities_drained: true,
    };
    let events: Vec<_> = [
        SessionEventKind::TaskRunReport {
            report: Box::new(report),
        },
        SessionEventKind::Done {
            status: jingwei_core::DoneStatus::Completed,
            artifact: None,
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
    assert!(matches!(
        lease.commit_closed(&events).await,
        Err(BudgetExecutionError::RecoveryRequired)
    ));
    assert!(task.report().is_err());
    assert!(store.latest().execution_id().is_some());
}

#[tokio::test]
async fn completed_model_turns_keep_usage_and_active_time_in_the_next_lease() {
    let fixture = Fixture::with_model(true).await;
    let store = Store::new();
    let first = fixture
        .run(request(store.acquire().await.unwrap(), "model"))
        .await
        .unwrap();
    let first_budget = first.budget_checkpoint().unwrap().report();
    let lease = store.acquire().await.unwrap();
    assert_eq!(lease.task().report().unwrap().usage, first_budget.usage);
    assert_eq!(
        lease.task().report().unwrap().active_time,
        first_budget.active_time
    );
    let next = fixture.run(request(lease, "model")).await.unwrap();
    assert_eq!(
        next.budget_checkpoint()
            .unwrap()
            .report()
            .charged
            .model_requests,
        2
    );
    assert!(next.budget_checkpoint().unwrap().report().active_time >= first_budget.active_time);
    assert_eq!(fixture.model.0.load(Ordering::SeqCst), 2);
    fixture.close().await;
}

#[test]
fn real_processes_cannot_both_acquire_the_same_ready_revision() {
    let disk = Disk::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        disk.store().compare_exchange(0, &initial()).await.unwrap();
    });
    let left = ChildGuard::spawn(&disk, "compete");
    let right = ChildGuard::spawn(&disk, "compete");
    let output = [left.finish(), right.finish()];
    assert_eq!(
        output.iter().filter(|s| s.contains("CLAIM=owned")).count(),
        1
    );
    assert_eq!(
        output
            .iter()
            .filter(|s| s.contains("CLAIM=blocked"))
            .count(),
        1
    );
    assert_eq!(
        fs::read(disk.0.join("budget.jsonl"))
            .unwrap()
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        2
    );
}
