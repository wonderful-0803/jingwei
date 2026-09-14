//! Explicit in-process reply handoff. Serialized state alone grants no execution.
use jingwei_agent::{AgentTurnReport, AgentTurnRequest, TurnDisposition};
use jingwei_budget::{BudgetLimits, TaskBudget, TaskRunStop};
use jingwei_core::{SessionId, TaskId, TurnId};
use serde::{Deserialize, Serialize};

use crate::ReferenceConfigError;

/// Persistable data, not a live budget handle or a recovery checkpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingQuestion {
    pub version: u16,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: TurnId,
    pub agent_key: String,
    pub pending_question: String,
}
impl PendingQuestion {
    pub(crate) fn valid(&self) -> bool {
        self.version == 1
            && !self.agent_key.trim().is_empty()
            && self.agent_key.len() <= 4096
            && !self.pending_question.trim().is_empty()
            && self.pending_question.len() <= 64 * 1024
    }
}
/// Non-cloneable convenience handoff bound to the supplied live Task ledger.
/// The host must retain a single owner; this is not a global replay lock.
pub struct ReferenceWaiting {
    pending: PendingQuestion,
    task: TaskBudget,
    charged: jingwei_budget::BudgetAmounts,
}
impl ReferenceWaiting {
    /// Accept only a successful, drained waiting report and the matching idle
    /// ledger with its original cumulative consumption. A new same-ID ledger fails.
    /// Durable execution leases must use the host's durable resume path instead.
    pub fn from_report(
        report: &AgentTurnReport,
        task: TaskBudget,
    ) -> Result<Self, ReferenceConfigError> {
        let receipt = report.task_run_report().ok_or(ReferenceConfigError)?;
        let artifact = report.artifact().ok_or(ReferenceConfigError)?;
        let pending: PendingQuestion =
            serde_json::from_value(artifact.get("pending").ok_or(ReferenceConfigError)?.clone())
                .map_err(|_| ReferenceConfigError)?;
        if report.disposition() != TurnDisposition::WaitingForInput
            || report.budget_checkpoint().is_some()
            || receipt.stop != TaskRunStop::WaitingForInput
            || !receipt.capabilities_drained
            || artifact.get("type").and_then(|v| v.as_str()) != Some("reference_waiting_v1")
            || !pending.valid()
            || &pending.session_id != report.session_id()
            || &pending.turn_id != report.turn_id()
            || pending.pending_question != report.final_text()
            || pending.session_id != task.identity().session_id
            || pending.task_id != task.identity().task_id
            || pending.agent_key != task.identity().agent_key
            || receipt.budget.identity != *task.identity()
            || receipt
                .budget
                .run
                .as_ref()
                .is_none_or(|run| run.open || run.turn_id.as_ref() != Some(report.turn_id()))
        {
            return Err(ReferenceConfigError);
        }
        let waiting = Self {
            pending,
            task,
            charged: receipt.budget.charged,
        };
        waiting.validate_ledger()?;
        Ok(waiting)
    }
    pub fn pending(&self) -> &PendingQuestion {
        &self.pending
    }
    fn validate_ledger(&self) -> Result<(), ReferenceConfigError> {
        let live = self.task.report().map_err(|_| ReferenceConfigError)?;
        if live.run.is_some()
            || live.stop.is_some()
            || !live.pending.is_empty()
            || live.charged != self.charged
            || self.charged.model_requests == 0
        {
            return Err(ReferenceConfigError);
        }
        Ok(())
    }
    /// Consume the handoff and explicitly correlate this reply to the question.
    /// Caller authenticates the user and decides whether to resume; no input is
    /// automatically interpreted as consent to a previously denied operation.
    pub fn resume(
        self,
        reply_to: &TurnId,
        answer: &str,
        run_limits: BudgetLimits,
    ) -> Result<AgentTurnRequest, ReferenceConfigError> {
        if reply_to != &self.pending.turn_id || answer.trim().is_empty() || answer.len() > 64 * 1024
        {
            return Err(ReferenceConfigError);
        }
        self.validate_ledger()?;
        let message = serde_json::json!({
            "type":"reference_reply_v1", "reply_to":self.pending.turn_id,
            "task_id":self.pending.task_id, "answer":answer
        })
        .to_string();
        Ok(
            AgentTurnRequest::new(self.pending.session_id, self.pending.agent_key, message)
                .with_budget(self.task, run_limits),
        )
    }
}
