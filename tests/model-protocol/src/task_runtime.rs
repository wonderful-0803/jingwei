use super::*;
use jingwei::budget::*;
use jingwei::task::*;
use jingwei_budget_file::{FileBudgetCheckpointConfig, FileBudgetCheckpointStore};
use jingwei_journal_jsonl::JsonlSessionPersistencePlugin;
use jingwei_task_file::{FileTaskStateConfig, FileTaskStateStore};
use jingwei_task_runtime::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

struct Disk(PathBuf);
impl Disk {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("jingwei-task-runtime-{}", TaskId::new()));
        fs::create_dir(&root).unwrap();
        for name in ["budget.jsonl", "task.jsonl"] {
            fs::File::create_new(root.join(name))
                .unwrap()
                .sync_all()
                .unwrap();
        }
        Self(root)
    }
}
impl Drop for Disk {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn identity() -> TaskIdentity {
    TaskIdentity {
        task_id: TaskId::from("resume-task"),
        session_id: SessionId::from("resume-session"),
        agent_key: "reference".into(),
    }
}
fn compatibility() -> TaskCompatibility {
    TaskCompatibility {
        agent_revision: "reference-checkpoint-v1".into(),
        policy_revision: "policy-v1".into(),
        tool_revisions: BTreeMap::from([("lookup".into(), "v1".into())]),
        payload_schemas: BTreeMap::from([("app".into(), 1)]),
    }
}
fn budgets(root: &Path) -> Arc<FileBudgetCheckpointStore> {
    Arc::new(
        FileBudgetCheckpointStore::open(
            root.join("budget.jsonl"),
            FileBudgetCheckpointConfig::default(),
        )
        .unwrap(),
    )
}
fn states(root: &Path) -> Arc<FileTaskStateStore> {
    Arc::new(
        FileTaskStateStore::open(root.join("task.jsonl"), FileTaskStateConfig::default()).unwrap(),
    )
}
async fn initialize(root: &Path) {
    let task = TaskBudget::new(
        identity(),
        BudgetLimits::default(),
        BudgetLimits::default(),
        TokenBudgetMode::Soft,
        Arc::new(MonotonicBudgetClock::default()),
    )
    .unwrap();
    let budget = task.seal_checkpoint(None).unwrap();
    budgets(root).compare_exchange(0, &budget).await.unwrap();
    let snapshot = TaskSnapshot::capture(
        0,
        compatibility(),
        BTreeMap::from([("app".into(), json!({"events":0}))]),
        &budget,
        &[],
    )
    .unwrap();
    states(root).compare_exchange(0, &snapshot).await.unwrap();
}
struct Policy {
    allowed: AtomicBool,
    calls: AtomicUsize,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}
impl Policy {
    fn new(allowed: bool) -> Arc<Self> {
        Arc::new(Self {
            allowed: AtomicBool::new(allowed),
            calls: AtomicUsize::new(0),
            gate: None,
        })
    }
}
impl TaskResumePolicy for Policy {
    fn authorize<'a>(
        &'a self,
        _: &'a TaskSnapshot,
        request: &'a TaskResumeRequest,
    ) -> TaskStateFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.acquire().await.unwrap().forget();
            }
            if self.allowed.load(Ordering::SeqCst) && request.compatibility == compatibility() {
                Ok(())
            } else {
                Err("current actor is denied".into())
            }
        })
    }
}
struct Reducer;
impl TaskPayloadReducer for Reducer {
    fn reduce(
        &self,
        _: &TaskSnapshot,
        history: &[SessionEvent],
    ) -> Result<BTreeMap<String, Value>, String> {
        Ok(BTreeMap::from([(
            "app".into(),
            json!({"events":history.len()}),
        )]))
    }
}
struct Fixture {
    registry: PluginRegistry,
    coordinator: TaskCoordinator,
    model: Arc<Model>,
    calls: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new(
        root: &Path,
        responses: Vec<GenerationResponse>,
        policy: Arc<dyn TaskResumePolicy>,
    ) -> Self {
        Self::custom(root, responses, policy, states(root), true).await
    }
    async fn custom(
        root: &Path,
        responses: Vec<GenerationResponse>,
        policy: Arc<dyn TaskResumePolicy>,
        state_store: Arc<dyn TaskStateStore>,
        grant: bool,
    ) -> Self {
        Self::with_mode(
            root,
            responses,
            policy,
            state_store,
            grant,
            ToolMode::Success,
        )
        .await
    }
    async fn with_mode(
        root: &Path,
        responses: Vec<GenerationResponse>,
        policy: Arc<dyn TaskResumePolicy>,
        state_store: Arc<dyn TaskStateStore>,
        grant: bool,
        mode: ToolMode,
    ) -> Self {
        let model = Arc::new(Model {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(vec![]),
            capabilities: caps(),
            cancel_after: None,
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let mut cfg = config(4, true);
        cfg.checkpoint_each_step = true;
        let mut registrar = Registrar::default();
        registrar.add(ModelPlugin(model.clone()));
        registrar.add(Registered(
            Arc::new(ReferenceAgent::new(cfg, policies()).unwrap()),
            true,
        ));
        registrar.add(RefTools {
            calls: calls.clone(),
            mode,
        });
        registrar.add(JsonlSessionPersistencePlugin::new(root));
        registrar.add(CanonicalSessionRuntimePlugin::new());
        registrar.add(CanonicalLlmRuntimePlugin::new());
        registrar.add(CanonicalAgentRuntimePlugin::new());
        let tools = CanonicalToolRuntimePlugin::new();
        registrar.add(if grant {
            tools.grant_tool(PluginId::new("reference-owner"), "lookup")
        } else {
            tools
        });
        registrar.select(AGENT_RUNTIME, "canonical");
        registrar.select(LLM_RUNTIME, "canonical");
        registrar.select(TOOL_RUNTIME, "canonical");
        registrar.select(jingwei::session::SESSION_RUNTIME, "canonical");
        registrar.select(jingwei::session::SESSION_PERSISTENCE, "jsonl");
        let registry = registrar.finish().await.unwrap();
        let coordinator = TaskCoordinator::new(TaskCoordinatorConfig {
            identity: identity(),
            runtime: registry.agent_runtime().unwrap(),
            session: registry.session_persistence().unwrap(),
            budgets: budgets(root),
            states: state_store,
            clock: Arc::new(MonotonicBudgetClock::default()),
            policy,
            reducer: Arc::new(Reducer),
        });
        Self {
            registry,
            coordinator,
            model,
            calls,
        }
    }
    async fn finish(self) {
        self.coordinator.close().await;
        self.registry.shutdown().await.unwrap();
    }
}
fn request(revision: u64, intent: TaskIntent) -> TaskResumeRequest {
    TaskResumeRequest {
        expected_revision: revision,
        compatibility: compatibility(),
        host_limits: BudgetLimits::default(),
        run_limits: BudgetLimits::default(),
        intent,
    }
}
fn start() -> TaskResumeRequest {
    request(
        1,
        TaskIntent::Start {
            message: "lookup once then finish".into(),
        },
    )
}
fn continuation(revision: u64) -> TaskResumeRequest {
    request(
        revision,
        TaskIntent::Continue {
            instruction: "Use the confirmed result and finish; do not repeat lookup.".into(),
        },
    )
}

#[tokio::test]
async fn tool_step_closes_and_reopened_runtime_continues_without_replay() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let first = Fixture::new(&disk.0, vec![call_response(true)], Policy::new(true)).await;
    let result = first
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    let report = result.turn.unwrap();
    let snapshot = result.state.unwrap();
    assert_eq!(report.disposition(), TurnDisposition::Checkpointed);
    assert_eq!(snapshot.phase(), &TaskPhase::Checkpointed);
    assert_eq!(snapshot.settled_steps().len(), 1);
    assert_eq!(snapshot.budget().charged.tool_calls, 1);
    assert!(report.budget_checkpoint().is_some());
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    let original = first
        .registry
        .session_persistence()
        .unwrap()
        .load(&identity().session_id)
        .await
        .unwrap();
    acceptance::assert_closed(&original);
    first.finish().await;
    let second = Fixture::new(&disk.0, vec![final_response(true)], Policy::new(true)).await;
    let result = second
        .coordinator
        .start(continuation(2))
        .unwrap()
        .wait()
        .await
        .unwrap();
    let next = result.state.unwrap();
    let report2 = result.turn.unwrap();
    assert_ne!(report.turn_id(), report2.turn_id());
    assert_eq!(next.phase(), &TaskPhase::Completed);
    assert_eq!(next.budget().charged.tool_calls, 1);
    assert_eq!(next.budget().charged.model_requests, 2);
    assert_eq!(next.settled_steps().len(), 2);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    let history = second
        .registry
        .session_persistence()
        .unwrap()
        .load(&identity().session_id)
        .await
        .unwrap();
    assert_eq!(&history[..original.len()], original.as_slice());
    acceptance::assert_closed(&history);
    {
        let requests = second.model.requests.lock().unwrap();
        assert!(
            serde_json::to_string(&requests[0])
                .unwrap()
                .contains("found data")
        );
    }
    second.finish().await;
}

#[tokio::test]
async fn waiting_reply_is_correlated_after_reopen_and_permissions_are_fresh() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let first = Fixture::new(
        &disk.0,
        vec![json_response(
            json!({"action":"ask_user","question":"哪个目录？"}),
        )],
        Policy::new(true),
    )
    .await;
    let result = first
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    let turn_id = result.turn.unwrap().turn_id().clone();
    assert!(matches!(
        result.state.unwrap().phase(),
        TaskPhase::WaitingForInput { .. }
    ));
    first.finish().await;
    let policy = Policy::new(false);
    let second = Fixture::new(&disk.0, vec![final_response(true)], policy.clone()).await;
    let original_budget = budgets(&disk.0).load(&identity()).await.unwrap().unwrap();
    let wrong = request(
        2,
        TaskIntent::Reply {
            reply_to: TurnId::new(),
            answer: "src".into(),
        },
    );
    assert!(matches!(
        second.coordinator.start(wrong).unwrap().wait().await,
        Err(TaskCoordinatorError::InvalidIntent)
    ));
    let reply = request(
        2,
        TaskIntent::Reply {
            reply_to: turn_id,
            answer: "src".into(),
        },
    );
    assert!(matches!(
        second
            .coordinator
            .start(reply.clone())
            .unwrap()
            .wait()
            .await,
        Err(TaskCoordinatorError::Denied(_))
    ));
    assert_eq!(
        budgets(&disk.0).load(&identity()).await.unwrap().unwrap(),
        original_budget
    );
    assert_eq!(second.model.calls.load(Ordering::SeqCst), 0);
    policy.allowed.store(true, Ordering::SeqCst);
    let result = second
        .coordinator
        .start(reply)
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.state.unwrap().phase(), &TaskPhase::Completed);
    result.turn.unwrap();
    assert_eq!(policy.calls.load(Ordering::SeqCst), 2);
    assert!(
        serde_json::to_string(&second.model.requests.lock().unwrap()[0])
            .unwrap()
            .contains("src")
    );
    second.finish().await;
}

#[tokio::test]
async fn stale_revision_or_compatibility_is_rejected_before_claim_or_model() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::new(&disk.0, vec![], Policy::new(true)).await;
    let base = fs::read(disk.0.join("budget.jsonl")).unwrap();
    let mut requests = vec![];
    let mut stale = start();
    stale.expected_revision = 9;
    requests.push(stale);
    let mut changed = start();
    changed.compatibility.agent_revision = "other".into();
    requests.push(changed);
    let mut changed = start();
    changed.compatibility.tool_revisions.clear();
    requests.push(changed);
    let mut changed = start();
    changed
        .compatibility
        .payload_schemas
        .insert("app".into(), 2);
    requests.push(changed);
    for req in requests {
        assert!(matches!(
            fixture.coordinator.start(req).unwrap().wait().await,
            Err(TaskCoordinatorError::State(_))
        ));
    }
    assert_eq!(fs::read(disk.0.join("budget.jsonl")).unwrap(), base);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

struct FailPublish {
    inner: Arc<FileTaskStateStore>,
}
impl TaskStateStore for FailPublish {
    fn load<'a>(
        &'a self,
        id: &'a TaskIdentity,
    ) -> TaskStateFuture<'a, Result<Option<TaskSnapshot>, TaskStoreError>> {
        self.inner.load(id)
    }
    fn compare_exchange<'a>(
        &'a self,
        _: u64,
        _: &'a TaskSnapshot,
    ) -> TaskStateFuture<'a, Result<TaskWriteOutcome, TaskStoreError>> {
        Box::pin(async {
            Err(TaskStoreError::Storage {
                certainty: TaskCommitCertainty::Indeterminate,
                message: "private publication failure".into(),
            })
        })
    }
}
#[tokio::test]
async fn publication_failure_retains_candidate_and_retry_never_reruns_tool() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::custom(
        &disk.0,
        vec![call_response(true)],
        Policy::new(true),
        Arc::new(FailPublish {
            inner: states(&disk.0),
        }),
        true,
    )
    .await;
    let result = fixture
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    result.turn.unwrap();
    let Err(TaskPublishError::Commit { candidate, .. }) = result.state else {
        panic!("exact failed candidate required")
    };
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        fixture.coordinator.start(start()).unwrap().wait().await,
        Err(TaskCoordinatorError::State(_))
    ));
    states(&disk.0)
        .compare_exchange(1, &candidate)
        .await
        .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn detached_waiter_is_drained_and_same_instance_is_bounded() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let policy = Arc::new(Policy {
        allowed: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let fixture = Fixture::new(&disk.0, vec![final_response(true)], policy.clone()).await;
    let controller = fixture.coordinator.start(start()).unwrap();
    assert!(matches!(
        fixture.coordinator.start(start()),
        Err(TaskCoordinatorError::Unavailable)
    ));
    drop(controller);
    while policy.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let mut close = Box::pin(fixture.coordinator.close());
    assert!(futures::poll!(close.as_mut()).is_pending());
    drop(close);
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), fixture.coordinator.close())
        .await
        .unwrap();
    assert_eq!(
        states(&disk.0)
            .load(&identity())
            .await
            .unwrap()
            .unwrap()
            .phase(),
        &TaskPhase::Completed
    );
    assert!(matches!(
        fixture.coordinator.start(start()),
        Err(TaskCoordinatorError::Unavailable)
    ));
    fixture.finish().await;
}

#[tokio::test]
async fn cancellation_before_claim_preserves_initial_evidence() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::new(&disk.0, vec![], Policy::new(true)).await;
    let controller = fixture.coordinator.start(start()).unwrap();
    controller.cancel();
    assert!(matches!(
        controller.wait().await,
        Err(TaskCoordinatorError::Cancelled)
    ));
    assert_eq!(
        budgets(&disk.0)
            .load(&identity())
            .await
            .unwrap()
            .unwrap()
            .revision(),
        1
    );
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn current_runtime_tool_grants_are_not_restored_from_snapshot() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::custom(
        &disk.0,
        vec![call_response(true)],
        Policy::new(true),
        states(&disk.0),
        false,
    )
    .await;
    let result = fixture
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(result.turn.is_err());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        result.state.unwrap().phase(),
        TaskPhase::Stopped { .. }
    ));
    fixture.finish().await;
}

#[test]
fn task_continuation_child() {
    let Ok(root) = std::env::var("JW_TASK_CONTINUE_ROOT") else {
        return;
    };
    let stage = std::env::var("JW_TASK_CONTINUE_STAGE").unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let root = Path::new(&root);
        let snapshot = states(root).load(&identity()).await.unwrap().unwrap();
        let (response, request) = match stage.as_str() {
            "step" => (call_response(true), start()),
            "ask" => (
                json_response(json!({"action":"ask_user","question":"Which directory?"})),
                continuation(2),
            ),
            "reply" => (
                final_response(true),
                request(
                    3,
                    TaskIntent::Reply {
                        reply_to: snapshot.cursor().unwrap().turn_id.clone(),
                        answer: "src".into(),
                    },
                ),
            ),
            _ => panic!("unknown child stage"),
        };
        let fixture = Fixture::new(root, vec![response], Policy::new(true)).await;
        let result = fixture
            .coordinator
            .start(request)
            .unwrap()
            .wait()
            .await
            .unwrap();
        result.turn.unwrap();
        let snapshot = result.state.unwrap();
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            usize::from(stage == "step")
        );
        assert_eq!(snapshot.budget().charged.tool_calls, 1);
        let history = fixture
            .registry
            .session_persistence()
            .unwrap()
            .load(&identity().session_id)
            .await
            .unwrap();
        acceptance::assert_closed(&history);
        fixture.finish().await;
        fs::write(
            root.join(format!("{stage}.ok")),
            snapshot.revision().to_string(),
        )
        .unwrap();
    });
}
#[test]
fn three_processes_resume_step_then_question_then_reply_with_one_tool_effect() {
    let disk = Disk::new();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(initialize(&disk.0));
    for stage in ["step", "ask", "reply"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actions::reference_agent::task_runtime::task_continuation_child",
                "--nocapture",
            ])
            .env("JW_TASK_CONTINUE_ROOT", &disk.0)
            .env("JW_TASK_CONTINUE_STAGE", stage)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let began = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if began.elapsed() > Duration::from_secs(20) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("continuation child timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            disk.0.join(format!("{stage}.ok")).exists(),
            "child did not execute"
        );
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let snapshot = states(&disk.0).load(&identity()).await.unwrap().unwrap();
        assert_eq!(snapshot.phase(), &TaskPhase::Completed);
        assert_eq!(snapshot.revision(), 4);
        assert_eq!(snapshot.budget().charged.model_requests, 3);
        assert_eq!(snapshot.budget().charged.tool_calls, 1);
        assert_eq!(snapshot.settled_steps().len(), 3);
    });
}

#[tokio::test]
async fn competing_coordinators_cannot_both_acquire_the_verified_boundary() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let policy = Arc::new(Policy {
        allowed: AtomicBool::new(true),
        calls: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let fixture = Fixture::new(&disk.0, vec![call_response(true)], policy.clone()).await;
    let other = TaskCoordinator::new(TaskCoordinatorConfig {
        identity: identity(),
        runtime: fixture.registry.agent_runtime().unwrap(),
        session: fixture.registry.session_persistence().unwrap(),
        budgets: budgets(&disk.0),
        states: states(&disk.0),
        clock: Arc::new(MonotonicBudgetClock::default()),
        policy: policy.clone(),
        reducer: Arc::new(Reducer),
    });
    let first = fixture.coordinator.start(start()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while policy.calls.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let second = other.start(start()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while policy.calls.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    gate.add_permits(2);
    let results = [first.wait().await, second.wait().await];
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for result in results.into_iter().flatten() {
        result.turn.unwrap();
        result.state.unwrap();
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);
    other.close().await;
    fixture.finish().await;
}

#[tokio::test]
async fn tightened_cumulative_budget_blocks_continuation_before_new_model_call() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let first = Fixture::new(&disk.0, vec![call_response(true)], Policy::new(true)).await;
    let result = first
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    result.turn.unwrap();
    result.state.unwrap();
    first.finish().await;
    let next = Fixture::new(&disk.0, vec![], Policy::new(true)).await;
    let mut req = continuation(2);
    req.host_limits.resources.model_requests = 1;
    let result = next.coordinator.start(req).unwrap().wait().await;
    assert!(result.is_err() || result.as_ref().is_ok_and(|r| r.turn.is_err()));
    assert_eq!(next.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        budgets(&disk.0)
            .load(&identity())
            .await
            .unwrap()
            .unwrap()
            .report()
            .charged
            .model_requests,
        1
    );
    next.finish().await;
}

struct FailBudgetFinal(Arc<FileBudgetCheckpointStore>);
impl BudgetCheckpointStore for FailBudgetFinal {
    fn load<'a>(
        &'a self,
        id: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>, BudgetCheckpointStoreError>>
    {
        self.0.load(id)
    }
    fn compare_exchange<'a>(
        &'a self,
        revision: u64,
        candidate: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>>
    {
        Box::pin(async move {
            if candidate.execution_id().is_none() {
                return Err(BudgetCheckpointStoreError::Storage {
                    certainty: BudgetCheckpointCommitCertainty::Indeterminate,
                    message: "private final failure".into(),
                });
            }
            self.0.compare_exchange(revision, candidate).await
        })
    }
}
#[tokio::test]
async fn failed_budget_finalization_never_publishes_ready_task_or_downgrades_lease() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::new(&disk.0, vec![call_response(true)], Policy::new(true)).await;
    let coordinator = TaskCoordinator::new(TaskCoordinatorConfig {
        identity: identity(),
        runtime: fixture.registry.agent_runtime().unwrap(),
        session: fixture.registry.session_persistence().unwrap(),
        budgets: Arc::new(FailBudgetFinal(budgets(&disk.0))),
        states: states(&disk.0),
        clock: Arc::new(MonotonicBudgetClock::default()),
        policy: Policy::new(true),
        reducer: Arc::new(Reducer),
    });
    let result = coordinator.start(start()).unwrap().wait().await.unwrap();
    assert!(matches!(
        result.turn,
        Err(AgentRuntimeError::Durability { .. })
    ));
    assert!(matches!(result.state, Err(TaskPublishError::Evidence(_))));
    let budget = budgets(&disk.0).load(&identity()).await.unwrap().unwrap();
    assert!(budget.execution_id().is_some());
    assert_eq!(
        states(&disk.0)
            .load(&identity())
            .await
            .unwrap()
            .unwrap()
            .revision(),
        1
    );
    assert!(coordinator.start(start()).unwrap().wait().await.is_err());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    coordinator.close().await;
    fixture.finish().await;
}
#[tokio::test]
async fn completed_tasks_and_changed_log_boundaries_cannot_be_resumed() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::new(&disk.0, vec![final_response(true)], Policy::new(true)).await;
    let result = fixture
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    result.turn.unwrap();
    result.state.unwrap();
    assert!(matches!(
        fixture
            .coordinator
            .start(continuation(2))
            .unwrap()
            .wait()
            .await,
        Err(TaskCoordinatorError::InvalidIntent)
    ));
    fixture.finish().await;
    // Fault fixture truncates a checkpointed log after the previous owner has exited.
    let disk2 = Disk::new();
    initialize(&disk2.0).await;
    let first = Fixture::new(&disk2.0, vec![call_response(true)], Policy::new(true)).await;
    let result = first
        .coordinator
        .start(start())
        .unwrap()
        .wait()
        .await
        .unwrap();
    result.turn.unwrap();
    result.state.unwrap();
    first.finish().await;
    let path2 = jingwei_journal_jsonl::session_file_path(&disk2.0, &identity().session_id);
    let bytes2 = fs::read(&path2).unwrap();
    let first_line2 = bytes2.iter().position(|b| *b == b'\n').unwrap() + 1;
    fs::write(&path2, &bytes2[..first_line2]).unwrap();
    let next = Fixture::new(&disk2.0, vec![], Policy::new(true)).await;
    assert!(
        next.coordinator
            .start(continuation(2))
            .unwrap()
            .wait()
            .await
            .is_err()
    );
    assert_eq!(next.model.calls.load(Ordering::SeqCst), 0);
    next.finish().await;
}

#[tokio::test]
async fn cancellation_during_tool_execution_drains_and_publishes_stopped_boundary() {
    let disk = Disk::new();
    initialize(&disk.0).await;
    let fixture = Fixture::with_mode(
        &disk.0,
        vec![call_response(true)],
        Policy::new(true),
        states(&disk.0),
        true,
        ToolMode::Pending,
    )
    .await;
    let controller = fixture.coordinator.start(start()).unwrap();
    let cancel = controller.canceller();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), controller.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.turn.unwrap().disposition(),
        TurnDisposition::Cancelled
    );
    let snapshot = result.state.unwrap();
    assert!(matches!(
        snapshot.phase(),
        TaskPhase::Stopped {
            reason: TaskRunStop::CallerCancelled
        }
    ));
    assert_eq!(snapshot.budget().charged.tool_calls, 1);
    assert!(
        budgets(&disk.0)
            .load(&identity())
            .await
            .unwrap()
            .unwrap()
            .execution_id()
            .is_none()
    );
    let history = fixture
        .registry
        .session_persistence()
        .unwrap()
        .load(&identity().session_id)
        .await
        .unwrap();
    acceptance::assert_closed(&history);
    fixture.finish().await;
}
