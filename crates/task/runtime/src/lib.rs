//! Explicit host continuation over existing durable budget and Session ownership.
//!
//! Supply the same exclusive, confirmed Session provider used by the runtime.
//! Keep its writer ownership alive through execution and state publication. The
//! coordinator never installs providers, restores permissions, or replays calls.
//! Each instance owns one Task and admits one operation at a time. Across instances
//! the durable budget CAS admits at most one execution at the verified boundary.
//! Drop of the completion waiter detaches; close drains accepted work before the
//! host shuts down its runtime and stores. Missing/lagging state requires explicit
//! host reconciliation, not automatic reconstruction of business payloads.
use jingwei_agent::{
    AgentRuntime, AgentRuntimeError, AgentTurnCanceller, AgentTurnReport, AgentTurnRequest,
};
use jingwei_budget::{
    BudgetCheckpointStore, BudgetClock, BudgetExecutionError, BudgetExecutionLease, BudgetLimits,
    BudgetRestoreContext,
};
use jingwei_core::{SessionEvent, TurnId};
use jingwei_session::{SessionPersistence, SessionPersistenceError};
use jingwei_task::{
    TaskCompatibility, TaskIdentity, TaskPhase, TaskSnapshot, TaskStateError, TaskStateFuture,
    TaskStateStore, TaskStoreError,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, oneshot};

/// Every continuation is a new turn. A reply is correlated to the confirmed
/// waiting turn; it never constitutes consent to an earlier denied tool action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskIntent {
    Start { message: String },
    Continue { instruction: String },
    Reply { reply_to: TurnId, answer: String },
}
#[derive(Clone, Debug)]
pub struct TaskResumeRequest {
    pub expected_revision: u64,
    /// Independently supplied current revisions, never copied blindly from disk.
    pub compatibility: TaskCompatibility,
    pub host_limits: BudgetLimits,
    pub run_limits: BudgetLimits,
    pub intent: TaskIntent,
}
/// Trusted host policy. Recheck current actor permission and actual Agent,
/// policy, tool and payload contracts on every call. No permissive default.
/// Runtime gateways still apply their current grants and per-call approval.
pub trait TaskResumePolicy: Send + Sync {
    fn authorize<'a>(
        &'a self,
        snapshot: &'a TaskSnapshot,
        request: &'a TaskResumeRequest,
    ) -> TaskStateFuture<'a, Result<(), String>>;
}
/// Trusted bounded, nonblocking application reducer. Framework fields are derived
/// separately from canonical history; payload schema semantics belong to the host.
pub trait TaskPayloadReducer: Send + Sync {
    fn reduce(
        &self,
        previous: &TaskSnapshot,
        confirmed_history: &[SessionEvent],
    ) -> Result<BTreeMap<String, Value>, String>;
}

pub struct TaskCoordinatorConfig {
    pub identity: TaskIdentity,
    pub runtime: Arc<dyn AgentRuntime>,
    pub session: Arc<dyn SessionPersistence>,
    pub budgets: Arc<dyn BudgetCheckpointStore>,
    pub states: Arc<dyn TaskStateStore>,
    pub clock: Arc<dyn BudgetClock>,
    pub policy: Arc<dyn TaskResumePolicy>,
    pub reducer: Arc<dyn TaskPayloadReducer>,
}
#[derive(Debug, thiserror::Error)]
pub enum TaskCoordinatorError {
    #[error("task coordinator is busy or closed")]
    Unavailable,
    #[error("task state is missing; initialize explicitly")]
    Missing,
    #[error("task phase or continuation input does not match")]
    InvalidIntent,
    #[error("task continuation denied: {0}")]
    Denied(String),
    #[error("task continuation cancelled; an acquired claim may require audit")]
    Cancelled,
    #[error("task worker was lost; reconcile storage before any retry")]
    WorkerLost,
    #[error("Tokio runtime required")]
    RuntimeRequired,
    #[error(transparent)]
    State(#[from] TaskStateError),
    #[error(transparent)]
    Store(#[from] TaskStoreError),
    #[error(transparent)]
    Budget(#[from] BudgetExecutionError),
    #[error(transparent)]
    Session(#[from] SessionPersistenceError),
}
#[derive(Debug, thiserror::Error)]
pub enum TaskPublishError {
    #[error("checkpoint evidence could not be confirmed: {0}")]
    Evidence(#[from] TaskCoordinatorError),
    #[error("application reducer rejected state: {0}")]
    Payload(String),
    /// Retain and retry this exact image with revision - 1; never rerun the turn.
    #[error("task state commit failed: {source}")]
    Commit {
        candidate: Box<TaskSnapshot>,
        source: TaskStoreError,
    },
}
/// Canonical execution outcome and separate state publication result. A failed
/// publication does not roll back a completed turn or its accumulated budget.
#[derive(Debug)]
pub struct TaskRunResult {
    pub turn: Result<AgentTurnReport, AgentRuntimeError>,
    pub state: Result<TaskSnapshot, TaskPublishError>,
}
struct Cancellation {
    requested: AtomicBool,
    turn: Mutex<Option<Arc<dyn AgentTurnCanceller>>>,
}
impl AgentTurnCanceller for Cancellation {
    fn cancel(&self) {
        Cancellation::cancel(self);
    }
}
impl Cancellation {
    fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        let canceller = self.turn.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(canceller) = canceller {
            canceller.cancel();
        }
    }
    fn check(&self) -> Result<(), TaskCoordinatorError> {
        if self.requested.load(Ordering::SeqCst) {
            Err(TaskCoordinatorError::Cancelled)
        } else {
            Ok(())
        }
    }
}
/// Dropping this controller or wait future detaches without cancelling work.
pub struct TaskRunController {
    receiver: oneshot::Receiver<Result<TaskRunResult, TaskCoordinatorError>>,
    cancellation: Arc<Cancellation>,
}
impl TaskRunController {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    pub fn canceller(&self) -> Arc<dyn AgentTurnCanceller> {
        self.cancellation.clone()
    }
    pub async fn wait(self) -> Result<TaskRunResult, TaskCoordinatorError> {
        self.receiver
            .await
            .map_err(|_| TaskCoordinatorError::WorkerLost)?
    }
}
struct Control {
    accepting: bool,
    active: bool,
}
struct Inner {
    config: TaskCoordinatorConfig,
    control: Mutex<Control>,
    drained: Notify,
}
struct Active(Arc<Inner>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = false;
        self.0.drained.notify_waiters();
    }
}
#[derive(Clone)]
pub struct TaskCoordinator(Arc<Inner>);
impl TaskCoordinator {
    pub fn new(config: TaskCoordinatorConfig) -> Self {
        Self(Arc::new(Inner {
            config,
            control: Mutex::new(Control {
                accepting: true,
                active: false,
            }),
            drained: Notify::new(),
        }))
    }
    /// Synchronous admission; the owned worker performs all checks before claiming
    /// budget authority. No TaskBudget handle is exposed to the caller or Agent.
    pub fn start(
        &self,
        request: TaskResumeRequest,
    ) -> Result<TaskRunController, TaskCoordinatorError> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| TaskCoordinatorError::RuntimeRequired)?;
        {
            let mut control = self.0.control.lock().unwrap_or_else(|e| e.into_inner());
            if !control.accepting || control.active {
                return Err(TaskCoordinatorError::Unavailable);
            }
            control.active = true;
        }
        let active = Active(self.0.clone());
        let cancellation = Arc::new(Cancellation {
            requested: AtomicBool::new(false),
            turn: Mutex::new(None),
        });
        let worker_cancel = cancellation.clone();
        let (sender, receiver) = oneshot::channel();
        runtime.spawn(async move {
            let outcome = execute(&active.0.config, request, &worker_cancel).await;
            drop(active);
            let _ = sender.send(outcome);
        });
        Ok(TaskRunController {
            receiver,
            cancellation,
        })
    }
    /// Stop admission across clones and drain even detached callers. Keep runtime,
    /// Session ownership, and stores alive until this returns.
    pub async fn close(&self) {
        self.0
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .accepting = false;
        loop {
            let notified = self.0.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self
                .0
                .control
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
            {
                return;
            }
            notified.await;
        }
    }
}
fn message(snapshot: &TaskSnapshot, intent: &TaskIntent) -> Result<String, TaskCoordinatorError> {
    let text = match (snapshot.phase(), intent) {
        (TaskPhase::Created, TaskIntent::Start { message }) => message,
        (TaskPhase::Checkpointed, TaskIntent::Continue { instruction }) => instruction,
        (TaskPhase::WaitingForInput { .. }, TaskIntent::Reply { reply_to, answer })
            if snapshot.cursor().is_some_and(|c| &c.turn_id == reply_to) =>
        {
            answer
        }
        _ => return Err(TaskCoordinatorError::InvalidIntent),
    };
    if text.trim().is_empty() || text.len() > 64 * 1024 {
        return Err(TaskCoordinatorError::InvalidIntent);
    }
    Ok(match intent {
        TaskIntent::Start { message } => message.clone(),
        TaskIntent::Continue { instruction } => {
            json!({"type":"task_continue_v1", "task_id":snapshot.identity().task_id,
            "checkpoint_turn":snapshot.cursor().map(|c| &c.turn_id), "instruction":instruction})
            .to_string()
        }
        TaskIntent::Reply { reply_to, answer } => {
            json!({"type":"reference_reply_v1", "task_id":snapshot.identity().task_id,
            "reply_to":reply_to, "answer":answer})
            .to_string()
        }
    })
}
async fn execute(
    config: &TaskCoordinatorConfig,
    request: TaskResumeRequest,
    cancellation: &Cancellation,
) -> Result<TaskRunResult, TaskCoordinatorError> {
    cancellation.check()?;
    let snapshot = config
        .states
        .load(&config.identity)
        .await?
        .ok_or(TaskCoordinatorError::Missing)?;
    let message = message(&snapshot, &request.intent)?;
    let budget = config
        .budgets
        .load(&config.identity)
        .await
        .map_err(BudgetExecutionError::from)?
        .ok_or(BudgetExecutionError::Missing)?;
    let history = config.session.load(&config.identity.session_id).await?;
    snapshot.verify_evidence(
        &config.identity,
        request.expected_revision,
        &request.compatibility,
        &budget,
        &history,
    )?;
    config
        .policy
        .authorize(&snapshot, &request)
        .await
        .map_err(TaskCoordinatorError::Denied)?;
    cancellation.check()?;
    let lease = BudgetExecutionLease::acquire(
        config.budgets.clone(),
        BudgetRestoreContext {
            identity: config.identity.clone(),
            revision: budget.revision(),
            confirmed_anchor: budget.anchor().cloned(),
            host_limits: request.host_limits,
        },
        config.clock.clone(),
    )
    .await?;
    // Recheck after claiming. Runtime checks its independently loaded admission
    // history again before any append; a changed boundary freezes the claim.
    lease.verify_history(&config.session.load(&config.identity.session_id).await?)?;
    if config.states.load(&config.identity).await?.as_ref() != Some(&snapshot) {
        return Err(TaskStateError::EvidenceMismatch.into());
    }
    cancellation.check()?;
    let turn = config.runtime.start_turn(
        AgentTurnRequest::new(
            config.identity.session_id.clone(),
            config.identity.agent_key.clone(),
            message,
        )
        .with_durable_budget(lease, request.run_limits),
    );
    let turn = match turn {
        Ok(turn) => {
            let canceller = turn.canceller();
            *cancellation.turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(canceller.clone());
            if cancellation.requested.load(Ordering::SeqCst) {
                canceller.cancel();
            }
            let result = turn.wait().await;
            *cancellation.turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
            result
        }
        Err(error) => Err(error),
    };
    let state = publish(config, &snapshot).await;
    Ok(TaskRunResult { turn, state })
}
async fn publish(
    config: &TaskCoordinatorConfig,
    previous: &TaskSnapshot,
) -> Result<TaskSnapshot, TaskPublishError> {
    let evidence = async {
        let budget = config
            .budgets
            .load(&config.identity)
            .await
            .map_err(BudgetExecutionError::from)?
            .ok_or(BudgetExecutionError::Missing)?;
        let history = config.session.load(&config.identity.session_id).await?;
        budget.verify_history(&history)?;
        if budget.requires_recovery() {
            return Err(TaskCoordinatorError::Budget(
                BudgetExecutionError::RecoveryRequired,
            ));
        }
        Ok::<_, TaskCoordinatorError>((budget, history))
    }
    .await?;
    let (budget, history) = evidence;
    let payload = config
        .reducer
        .reduce(previous, &history)
        .map_err(TaskPublishError::Payload)?;
    let candidate = TaskSnapshot::capture(
        previous.revision(),
        previous.compatibility().clone(),
        payload,
        &budget,
        &history,
    )
    .map_err(TaskCoordinatorError::from)?;
    config
        .states
        .compare_exchange(previous.revision(), &candidate)
        .await
        .map_err(|source| TaskPublishError::Commit {
            candidate: Box::new(candidate.clone()),
            source,
        })?;
    Ok(candidate)
}
