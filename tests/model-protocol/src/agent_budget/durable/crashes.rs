//! Runtime process-death windows; file syscall faults are tested separately.
use super::*;
use jingwei_journal_jsonl::JsonlRecoveryOwnership;
use std::io::Write;

struct FailFinal(Arc<FileBudgetCheckpointStore>);
impl BudgetCheckpointStore for FailFinal {
    fn load<'a>(
        &'a self,
        identity: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>, BudgetCheckpointStoreError>>
    {
        self.0.load(identity)
    }
    fn compare_exchange<'a>(
        &'a self,
        expected: u64,
        checkpoint: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>>
    {
        Box::pin(async move {
            if checkpoint.execution_id().is_none() {
                return Err(BudgetCheckpointStoreError::Storage {
                    certainty: BudgetCheckpointCommitCertainty::DefinitelyNotCommitted,
                    message: "private final commit rejection".into(),
                });
            }
            self.0.compare_exchange(expected, checkpoint).await
        })
    }
}

fn spawn(disk: &Disk, phase: &str) -> ChildGuard {
    ChildGuard(Some(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_budget::durable::crashes::crash_child",
                "--nocapture",
            ])
            .env("JW_CRASH_ROOT", &disk.0)
            .env("JW_CRASH_PHASE", phase)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ))
}

fn prepare_and_kill(disk: &Disk, phase: &str) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        disk.store().compare_exchange(0, &initial()).await.unwrap();
    });
    let mut child = spawn(disk, phase);
    let started = Instant::now();
    while !disk.0.join("ready").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "child did not reach {phase}"
        );
        if child.0.as_mut().unwrap().try_wait().unwrap().is_some() {
            panic!("child exited early: {}", child.finish());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    runtime.block_on(async {
        let store = disk.store();
        let claim = store.load(&binding()).await.unwrap().unwrap();
        assert_eq!(claim.revision(), 2);
        assert!(matches!(
            BudgetExecutionLease::acquire(
                store,
                expectations(&claim),
                Arc::new(MonotonicBudgetClock::default())
            )
            .await,
            Err(BudgetExecutionError::RecoveryRequired)
        ));
        if phase == "final" {
            assert!(
                JsonlRecoveryOwnership::acquire(&disk.0, binding().session_id)
                    .await
                    .is_err()
            );
        }
    });
    drop(child);
}

#[test]
fn death_after_claim_before_session_admission_never_authorizes_replay() {
    let disk = Disk::new();
    prepare_and_kill(&disk, "claim");
    let before = fs::read(disk.0.join("budget.jsonl")).unwrap();
    spawn(&disk, "verify-claim").finish();
    assert_eq!(fs::read(disk.0.join("budget.jsonl")).unwrap(), before);
    assert!(!disk.log_path().exists());
}

#[test]
fn death_after_settle_recovers_saved_candidate_without_replaying_body() {
    let disk = Disk::new();
    prepare_and_kill(&disk, "final");
    let before = fs::read(disk.log_path()).unwrap();
    spawn(&disk, "recover").finish();
    spawn(&disk, "recover").finish();
    assert_eq!(fs::read(disk.log_path()).unwrap(), before);
}

#[test]
fn crash_child() {
    let Ok(root) = std::env::var("JW_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("JW_CRASH_PHASE").unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = Arc::new(
            FileBudgetCheckpointStore::open(
                root.join("budget.jsonl"),
                FileBudgetCheckpointConfig::default(),
            )
            .unwrap(),
        );
        let checkpoint = store.load(&binding()).await.unwrap().unwrap();
        if phase == "verify-claim" {
            let owner = JsonlRecoveryOwnership::acquire(&root, binding().session_id)
                .await
                .unwrap();
            assert_eq!(
                inspect_budget_recovery(&checkpoint, &owner).await.unwrap(),
                BudgetRecoveryAssessment::FrozenNoNewEvents
            );
            assert!(matches!(
                BudgetExecutionLease::acquire(
                    store,
                    expectations(&checkpoint),
                    Arc::new(MonotonicBudgetClock::default())
                )
                .await,
                Err(BudgetExecutionError::RecoveryRequired)
            ));
            return;
        }
        if phase == "recover" {
            let candidate: BudgetCheckpoint =
                serde_json::from_slice(&fs::read(root.join("candidate.json")).unwrap()).unwrap();
            let request: BudgetRecoveryRequest =
                serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
            let owner = Arc::new(
                JsonlRecoveryOwnership::acquire(&root, binding().session_id)
                    .await
                    .unwrap(),
            );
            let result = recover_budget_candidate(store.clone(), owner, request, &candidate)
                .await
                .unwrap();
            assert!(matches!(
                result,
                BudgetRecoveryOutcome::Applied(_) | BudgetRecoveryOutcome::AlreadyApplied(_)
            ));
            let recovered = store.load(&binding()).await.unwrap().unwrap();
            assert_eq!(recovered.report(), candidate.report());
            assert_eq!(recovered.report().charged.steps, 1);
            assert_eq!(recovered.recoveries().len(), 1);
            return;
        }
        let lease = BudgetExecutionLease::acquire(
            Arc::new(FailFinal(store.clone())),
            expectations(&checkpoint),
            Arc::new(MonotonicBudgetClock::default()),
        )
        .await
        .unwrap();
        if phase == "final" {
            let claim = store.load(&binding()).await.unwrap().unwrap();
            let probe = Arc::new(Probe::default());
            let mut registrar = Registrar::default();
            registrar.add(JsonlSessionPersistencePlugin::new(&root));
            registrar.add(ProbePlugin(probe.clone(), true));
            registrar.add(ModelPlugin(Arc::new(Model::default())));
            registrar.add(CanonicalLlmRuntimePlugin::new());
            registrar.add(CanonicalSessionRuntimePlugin::new());
            registrar.add(CanonicalAgentRuntimePlugin::new());
            registrar.require(AGENT_RUNTIME);
            let registry = registrar.finish().await.unwrap();
            let error = registry
                .agent_runtime()
                .unwrap()
                .start_turn(request(lease, "step"))
                .unwrap()
                .wait()
                .await
                .unwrap_err();
            let AgentRuntimeError::Durability {
                source: BudgetExecutionError::Commit { checkpoint, .. },
                ..
            } = error
            else {
                panic!("wrong failure");
            };
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            let request = BudgetRecoveryRequest {
                operation_id: jingwei_core::BudgetOperationId::new(),
                execution_id: claim.execution_id().unwrap().clone(),
                source_revision: claim.revision(),
                actor: "test-host".into(),
                source: "saved runtime error".into(),
                reason: "final commit failed".into(),
            };
            for (name, bytes) in [
                ("candidate.json", serde_json::to_vec(&checkpoint).unwrap()),
                ("request.json", serde_json::to_vec(&request).unwrap()),
            ] {
                let mut file = File::create(root.join(name)).unwrap();
                file.write_all(&bytes).unwrap();
                file.sync_all().unwrap();
            }
            File::create_new(root.join("ready"))
                .unwrap()
                .sync_all()
                .unwrap();
            std::future::pending::<()>().await;
            drop(registry);
        } else {
            assert_eq!(phase, "claim");
            File::create_new(root.join("ready"))
                .unwrap()
                .sync_all()
                .unwrap();
            std::future::pending::<()>().await;
            drop(lease);
        }
    });
}
