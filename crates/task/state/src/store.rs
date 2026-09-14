use crate::{TaskIdentity, TaskSnapshot, TaskStateError};
use jingwei_core::TaskId;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Mutex};

pub type TaskStateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskWriteOutcome {
    Applied,
    AlreadyPresent,
}
#[derive(Debug, thiserror::Error)]
pub enum TaskStoreError {
    #[error(transparent)]
    State(#[from] TaskStateError),
    #[error("task state conflict: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("task store capacity exhausted")]
    Capacity,
    #[error("task store lock poisoned")]
    Poisoned,
}
/// Replaceable host store; zero expected revision means absent. Implementations
/// must atomically compare and replace, validate monotonicity, and never silently
/// replace unreadable state. Callers verify canonical evidence before writes.
/// No method here grants ownership of a Session or permission to execute a Task.
pub trait TaskStateStore: Send + Sync {
    fn load<'a>(
        &'a self,
        identity: &'a TaskIdentity,
    ) -> TaskStateFuture<'a, Result<Option<TaskSnapshot>, TaskStoreError>>;
    fn compare_exchange<'a>(
        &'a self,
        expected_revision: u64,
        candidate: &'a TaskSnapshot,
    ) -> TaskStateFuture<'a, Result<TaskWriteOutcome, TaskStoreError>>;
}
/// Optional bounded in-process store, not a durable checkpoint backend.
pub struct MemoryTaskStateStore {
    max_tasks: usize,
    values: Mutex<BTreeMap<TaskId, TaskSnapshot>>,
}
impl MemoryTaskStateStore {
    pub fn new(max_tasks: usize) -> Result<Self, TaskStateError> {
        if max_tasks == 0 || max_tasks > 65_536 {
            return Err(TaskStateError::Limit);
        }
        Ok(Self {
            max_tasks,
            values: Mutex::new(BTreeMap::new()),
        })
    }
}
impl TaskStateStore for MemoryTaskStateStore {
    fn load<'a>(
        &'a self,
        identity: &'a TaskIdentity,
    ) -> TaskStateFuture<'a, Result<Option<TaskSnapshot>, TaskStoreError>> {
        Box::pin(async move {
            crate::valid_text(identity.task_id.as_str())?;
            crate::valid_text(identity.session_id.as_str())?;
            crate::valid_text(&identity.agent_key)?;
            let values = self.values.lock().map_err(|_| TaskStoreError::Poisoned)?;
            let value = values.get(&identity.task_id);
            if value.is_some_and(|v| v.identity() != identity) {
                return Err(TaskStateError::IdentityMismatch.into());
            }
            Ok(value.cloned())
        })
    }
    fn compare_exchange<'a>(
        &'a self,
        expected_revision: u64,
        candidate: &'a TaskSnapshot,
    ) -> TaskStateFuture<'a, Result<TaskWriteOutcome, TaskStoreError>> {
        Box::pin(async move {
            candidate.validate()?;
            let next = expected_revision
                .checked_add(1)
                .ok_or(TaskStateError::RevisionExhausted)?;
            if candidate.revision() != next {
                return Err(TaskStateError::RevisionMismatch {
                    expected: next,
                    actual: candidate.revision(),
                }
                .into());
            }
            let mut values = self.values.lock().map_err(|_| TaskStoreError::Poisoned)?;
            let previous = values.get(&candidate.identity().task_id);
            if previous == Some(candidate) {
                return Ok(TaskWriteOutcome::AlreadyPresent);
            }
            let actual = previous.map_or(0, TaskSnapshot::revision);
            if actual != expected_revision {
                return Err(TaskStoreError::Conflict {
                    expected: expected_revision,
                    actual,
                });
            }
            candidate.validate_successor(previous)?;
            if previous.is_none() && values.len() >= self.max_tasks {
                return Err(TaskStoreError::Capacity);
            }
            values.insert(candidate.identity().task_id.clone(), candidate.clone());
            Ok(TaskWriteOutcome::Applied)
        })
    }
}
