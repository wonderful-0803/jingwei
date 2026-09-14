//! Data-only task checkpoints. A snapshot never grants execution or restores a ledger.
mod history;
mod store;
pub use history::*;
pub use store::*;

use jingwei_budget::{
    BudgetAmounts, BudgetCheckpoint, BudgetEventCursor, BudgetResource, TaskRunStop,
};
pub use jingwei_core::budget::BudgetIdentity as TaskIdentity;
use jingwei_core::{DoneStatus, SessionEvent, SessionEventKind, StepId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_TASK_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const MAX_TASK_HISTORY_EVENTS: usize = 100_000;
pub const MAX_TASK_STEPS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum TaskSnapshotVersion {
    V1,
}
impl TryFrom<u16> for TaskSnapshotVersion {
    type Error = &'static str;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if value == 1 {
            Ok(Self::V1)
        } else {
            Err("unsupported task snapshot version")
        }
    }
}
impl From<TaskSnapshotVersion> for u16 {
    fn from(_: TaskSnapshotVersion) -> Self {
        1
    }
}

/// Independent host expectations. Revisions must change when the corresponding
/// contract changes. These declarations are not tool grants or schema validators.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCompatibility {
    pub agent_revision: String,
    pub policy_revision: String,
    pub tool_revisions: BTreeMap<String, String>,
    /// Namespace -> host schema version. All payload namespaces must be declared.
    pub payload_schemas: BTreeMap<String, u32>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum TaskPhase {
    Created,
    /// A completed canonical turn, not independent proof of business success.
    Completed,
    WaitingForInput {
        question: String,
    },
    Stopped {
        reason: TaskRunStop,
    },
}
/// Reference to the separately owned budget image, never a replacement ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBudgetReference {
    pub revision: u64,
    pub charged: BudgetAmounts,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSnapshot {
    version: TaskSnapshotVersion,
    revision: u64,
    identity: TaskIdentity,
    compatibility: TaskCompatibility,
    cursor: Option<BudgetEventCursor>,
    budget: TaskBudgetReference,
    phase: TaskPhase,
    /// Settled decision IDs in first-observed order, including rejected proposals.
    settled_steps: Vec<StepId>,
    payloads: BTreeMap<String, Value>,
}
#[derive(Debug, thiserror::Error)]
pub enum TaskStateError {
    #[error("invalid task state: {0}")]
    Invalid(&'static str),
    #[error("task state exceeds its resource limit")]
    Limit,
    #[error("task identity differs")]
    IdentityMismatch,
    #[error("Agent, policy, tool or payload schema contract differs")]
    Incompatible,
    #[error("task revision differs: expected {expected}, actual {actual}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("task revision exhausted")]
    RevisionExhausted,
    #[error("history contains unfinished work; host review is required")]
    NeedsReview,
    #[error("task snapshot does not match canonical evidence")]
    EvidenceMismatch,
    #[error("budget boundary rejected: {0}")]
    Budget(#[from] jingwei_budget::BudgetExecutionError),
    #[error("invalid task snapshot encoding")]
    Encoding,
}
impl TaskSnapshot {
    /// Capture only an empty or closed, settled Session boundary. The host must
    /// supply confirmed durable history and the exact already-persisted budget
    /// image under exclusive ownership. This function performs no storage IO.
    pub fn capture(
        expected_revision: u64,
        compatibility: TaskCompatibility,
        payloads: BTreeMap<String, Value>,
        budget: &BudgetCheckpoint,
        history: &[SessionEvent],
    ) -> Result<Self, TaskStateError> {
        let assessment = assess_task_history(budget.identity(), history)?;
        if !assessment.unresolved.is_empty()
            || matches!(assessment.boundary, TaskHistoryBoundary::Open { .. })
        {
            return Err(TaskStateError::NeedsReview);
        }
        budget.verify_history(history)?;
        if budget.requires_recovery() {
            return Err(TaskStateError::NeedsReview);
        }
        let phase = phase(history)?;
        let snapshot = Self {
            version: TaskSnapshotVersion::V1,
            revision: expected_revision
                .checked_add(1)
                .ok_or(TaskStateError::RevisionExhausted)?,
            identity: budget.identity().clone(),
            compatibility,
            cursor: budget.anchor().cloned(),
            budget: TaskBudgetReference {
                revision: budget.revision(),
                charged: budget.report().charged,
            },
            phase,
            settled_steps: assessment.settled_steps,
            payloads,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn identity(&self) -> &TaskIdentity {
        &self.identity
    }
    pub fn compatibility(&self) -> &TaskCompatibility {
        &self.compatibility
    }
    pub fn cursor(&self) -> Option<&BudgetEventCursor> {
        self.cursor.as_ref()
    }
    pub fn budget(&self) -> &TaskBudgetReference {
        &self.budget
    }
    pub fn phase(&self) -> &TaskPhase {
        &self.phase
    }
    pub fn settled_steps(&self) -> &[StepId] {
        &self.settled_steps
    }
    pub fn payloads(&self) -> &BTreeMap<String, Value> {
        &self.payloads
    }
    /// Decode bounded data. Still requires verify_evidence before host use.
    pub fn from_json(bytes: &[u8]) -> Result<Self, TaskStateError> {
        if bytes.len() > MAX_TASK_SNAPSHOT_BYTES {
            return Err(TaskStateError::Limit);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| TaskStateError::Encoding)?;
        value.validate()?;
        Ok(value)
    }
    /// Structural checks only; not log confirmation or business payload validation.
    pub fn validate(&self) -> Result<(), TaskStateError> {
        if self.revision == 0 || self.budget.revision == 0 {
            return Err(TaskStateError::Invalid("zero revision"));
        }
        for text in [
            self.identity.task_id.as_str(),
            self.identity.session_id.as_str(),
            &self.identity.agent_key,
            &self.compatibility.agent_revision,
            &self.compatibility.policy_revision,
        ] {
            valid_text(text)?;
        }
        if self.compatibility.tool_revisions.len() > 1024
            || self.payloads.len() > 128
            || self.settled_steps.len() > MAX_TASK_STEPS
        {
            return Err(TaskStateError::Limit);
        }
        for (name, revision) in &self.compatibility.tool_revisions {
            valid_text(name)?;
            valid_text(revision)?;
        }
        if self
            .payloads
            .keys()
            .ne(self.compatibility.payload_schemas.keys())
        {
            return Err(TaskStateError::Incompatible);
        }
        for (name, version) in &self.compatibility.payload_schemas {
            valid_text(name)?;
            if *version == 0 {
                return Err(TaskStateError::Invalid("zero schema version"));
            }
        }
        if let Some(cursor) = &self.cursor {
            if cursor.session_id != self.identity.session_id {
                return Err(TaskStateError::IdentityMismatch);
            }
            valid_text(cursor.event_id.as_str())?;
            valid_text(cursor.turn_id.as_str())?;
        }
        if matches!(self.phase, TaskPhase::Created) != self.cursor.is_none() {
            return Err(TaskStateError::Invalid("phase boundary"));
        }
        if matches!(self.phase, TaskPhase::Created)
            && (self.budget.charged != BudgetAmounts::default() || !self.settled_steps.is_empty())
        {
            return Err(TaskStateError::Invalid("nonempty created state"));
        }
        if matches!(
            self.phase,
            TaskPhase::Stopped {
                reason: TaskRunStop::Completed | TaskRunStop::WaitingForInput
            }
        ) {
            return Err(TaskStateError::Invalid("inconsistent stopped phase"));
        }
        if let TaskPhase::WaitingForInput { question } = &self.phase
            && (question.trim().is_empty() || question.len() > 64 * 1024)
        {
            return Err(TaskStateError::Invalid("question"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for step in &self.settled_steps {
            valid_text(step.as_str())?;
            if !seen.insert(step) {
                return Err(TaskStateError::Invalid("duplicate step"));
            }
        }
        let mut nodes = 0;
        let mut stack: Vec<_> = self.payloads.values().map(|v| (v, 0)).collect();
        while let Some((value, depth)) = stack.pop() {
            nodes += 1;
            if depth > 64 || nodes > 65_536 {
                return Err(TaskStateError::Limit);
            }
            match value {
                Value::Array(values) => {
                    if values.len() > 65_536 {
                        return Err(TaskStateError::Limit);
                    }
                    stack.extend(values.iter().map(|v| (v, depth + 1)));
                }
                Value::Object(values) => {
                    if values.len() > 65_536 {
                        return Err(TaskStateError::Limit);
                    }
                    stack.extend(values.values().map(|v| (v, depth + 1)));
                }
                _ => {}
            }
            if stack.len() > 65_536 {
                return Err(TaskStateError::Limit);
            }
        }
        serde_json::to_writer(SizeLimit(MAX_TASK_SNAPSHOT_BYTES), self)
            .map_err(|_| TaskStateError::Limit)?;
        Ok(())
    }
    /// Expectations must come from the host's current configuration and storage,
    /// not be copied blindly from this snapshot. No permissions are restored.
    pub fn verify_evidence(
        &self,
        identity: &TaskIdentity,
        revision: u64,
        compatibility: &TaskCompatibility,
        budget: &BudgetCheckpoint,
        history: &[SessionEvent],
    ) -> Result<(), TaskStateError> {
        self.validate()?;
        if &self.identity != identity || budget.identity() != identity {
            return Err(TaskStateError::IdentityMismatch);
        }
        if self.revision != revision {
            return Err(TaskStateError::RevisionMismatch {
                expected: revision,
                actual: self.revision,
            });
        }
        if &self.compatibility != compatibility {
            return Err(TaskStateError::Incompatible);
        }
        let rebuilt = Self::capture(
            self.revision - 1,
            compatibility.clone(),
            self.payloads.clone(),
            budget,
            history,
        )?;
        if rebuilt != *self {
            return Err(TaskStateError::EvidenceMismatch);
        }
        Ok(())
    }
    /// Store-side monotonic/CAS checks, not evidence verification. Must run under
    /// the same lock/transaction that compares and replaces the stored value.
    pub fn validate_successor(&self, previous: Option<&Self>) -> Result<(), TaskStateError> {
        self.validate()?;
        let expected = previous
            .map_or(0, |p| p.revision)
            .checked_add(1)
            .ok_or(TaskStateError::RevisionExhausted)?;
        if self.revision != expected {
            return Err(TaskStateError::RevisionMismatch {
                expected,
                actual: self.revision,
            });
        }
        if let Some(old) = previous {
            old.validate()?;
            if old.identity != self.identity {
                return Err(TaskStateError::IdentityMismatch);
            }
            if old.compatibility != self.compatibility {
                return Err(TaskStateError::Incompatible);
            }
            if self.budget.revision < old.budget.revision
                || BudgetResource::ALL
                    .iter()
                    .any(|r| self.budget.charged.get(*r) < old.budget.charged.get(*r))
                || !self.settled_steps.starts_with(&old.settled_steps)
            {
                return Err(TaskStateError::EvidenceMismatch);
            }
            match (&old.cursor, &self.cursor) {
                (Some(a), Some(b)) if b.seq < a.seq || (b.seq == a.seq && b != a) => {
                    return Err(TaskStateError::EvidenceMismatch);
                }
                (Some(_), None) => return Err(TaskStateError::EvidenceMismatch),
                _ => {}
            }
            if self.budget.revision == old.budget.revision
                && (self.cursor != old.cursor || self.budget.charged != old.budget.charged)
            {
                return Err(TaskStateError::EvidenceMismatch);
            }
            if old.cursor == self.cursor
                && (old.phase != self.phase
                    || old.settled_steps != self.settled_steps
                    || old.budget.charged != self.budget.charged)
            {
                return Err(TaskStateError::EvidenceMismatch);
            }
        }
        Ok(())
    }
}
fn valid_text(value: &str) -> Result<(), TaskStateError> {
    if value.trim().is_empty() || value.len() > 4096 {
        Err(TaskStateError::Invalid(
            "empty or oversized identity/revision",
        ))
    } else {
        Ok(())
    }
}
fn phase(history: &[SessionEvent]) -> Result<TaskPhase, TaskStateError> {
    let Some(last) = history.last() else {
        return Ok(TaskPhase::Created);
    };
    match &last.kind {
        SessionEventKind::Done {
            status: DoneStatus::Completed,
            ..
        } => Ok(TaskPhase::Completed),
        SessionEventKind::Done {
            status: DoneStatus::WaitingForInput,
            ..
        } => {
            let question = history
                .iter()
                .rev()
                .take_while(|e| e.turn_id == last.turn_id)
                .find_map(|e| match &e.kind {
                    SessionEventKind::AssistantMessage { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .ok_or(TaskStateError::Invalid(
                    "waiting turn has no complete question",
                ))?;
            Ok(TaskPhase::WaitingForInput { question })
        }
        _ => match history
            .get(history.len().saturating_sub(2))
            .map(|e| &e.kind)
        {
            Some(SessionEventKind::TaskRunReport { report }) => Ok(TaskPhase::Stopped {
                reason: report.stop.clone(),
            }),
            _ => Err(TaskStateError::EvidenceMismatch),
        },
    }
}
struct SizeLimit(usize);
impl std::io::Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("task snapshot limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
