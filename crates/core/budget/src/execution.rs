//! Explicit host-owned write-ahead execution claims over checkpoint CAS.

use std::collections::HashSet;
use std::sync::Arc;

use jingwei_core::{BudgetExecutionId, DoneStatus, SessionEvent, SessionEventKind};

use super::*;

#[derive(Debug, thiserror::Error)]
pub enum BudgetExecutionError {
    #[error(transparent)]
    Checkpoint(#[from] BudgetCheckpointError),
    #[error(transparent)]
    Store(#[from] BudgetCheckpointStoreError),
    /// The exact sealed attempt, retained for reconciliation without reconstruction.
    #[error("checkpoint transition from revision {expected_revision} failed: {source}")]
    Commit {
        expected_revision: u64,
        checkpoint: Box<BudgetCheckpoint>,
        source: BudgetCheckpointStoreError,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("checkpoint is missing; execution requires explicit initialization")]
    Missing,
    #[error("checkpoint has an unfinished execution or frozen state; audit required")]
    RecoveryRequired,
    #[error("canonical budget/log boundary is invalid: {0}")]
    Boundary(&'static str),
    #[error("canonical turn did not close with confirmed budget evidence")]
    IncompleteTurn,
}

/// Non-Clone ownership of one durably claimed execution. Only trusted host/runtime
/// code receives this object; Agents receive narrow BudgetScope authority instead.
/// Drop never clears the durable claim. There is no expiry, stealing, refund or replay.
#[must_use = "transfer to a controlled runtime; abandoned claims require audit"]
pub struct BudgetExecutionLease {
    task: TaskBudget,
    base: BudgetCheckpoint,
    claim: BudgetCheckpoint,
    store: Arc<dyn BudgetCheckpointStore>,
}

impl BudgetExecutionLease {
    /// Load the expected latest ready checkpoint and durably claim its successor
    /// before returning any execution authority. Missing state is not initialized.
    /// A cancelled/failed acquisition may leave a durable claim and must not be
    /// treated as permission to restore an older ready image.
    pub async fn acquire(
        store: Arc<dyn BudgetCheckpointStore>,
        context: BudgetRestoreContext,
        clock: Arc<dyn BudgetClock>,
    ) -> Result<Self, BudgetExecutionError> {
        let base = store
            .load(&context.identity)
            .await?
            .ok_or(BudgetExecutionError::Missing)?;
        base.validate()?;
        if !base.is_quiescent() {
            return Err(BudgetExecutionError::RecoveryRequired);
        }
        let task = TaskBudget::restore_checkpoint(base.clone(), context, clock)?;
        if let Some(reason) = task.report()?.stop {
            return Err(BudgetError::Stopped(reason).into());
        }
        // Never let two independent acquisitions produce an identical retry image.
        let claim = base.execution_claim()?;
        store
            .compare_exchange(base.revision(), &claim)
            .await
            .map_err(|source| BudgetExecutionError::Commit {
                expected_revision: base.revision(),
                checkpoint: Box::new(claim.clone()),
                source,
            })?;
        task.0.lock()?.checkpoint_revision = claim.revision();
        Ok(Self {
            task,
            base,
            claim,
            store,
        })
    }

    pub fn execution_id(&self) -> &BudgetExecutionId {
        self.claim
            .execution_id()
            .expect("an acquired lease has a claim ID")
    }

    pub fn claim(&self) -> &BudgetCheckpoint {
        &self.claim
    }

    /// Trusted runtime binding only. A clone does not extend the durable execution
    /// lifetime and must not start work before verify_history or after finalization.
    pub fn task(&self) -> &TaskBudget {
        &self.task
    }

    /// Verify actual canonical admission history before the first user/model/tool
    /// append. The provider must supply fresh, durable history under its Session
    /// ownership boundary; this pure check cannot enforce provider exclusivity.
    pub fn verify_history(&self, history: &[SessionEvent]) -> Result<(), BudgetExecutionError> {
        verify_checkpoint_history(&self.base, history)
    }

    /// After capability drain, report, terminal and successful Session settlement,
    /// seal all memory handles and replace this claim with a ready checkpoint.
    /// `events` must be the exact committed current-turn slice, not a projected log.
    /// Failed/cancelled futures leave a frozen claim or a completed ready image;
    /// reconcile storage, never restore the old base. No automatic retry is made.
    pub async fn commit_closed(
        self,
        events: &[SessionEvent],
    ) -> Result<BudgetCheckpoint, BudgetExecutionError> {
        let first_seq = match self.base.anchor() {
            Some(anchor) => anchor
                .seq
                .checked_add(1)
                .ok_or(BudgetExecutionError::Boundary("sequence exhausted"))?,
            None => 0,
        };
        validate_addresses(events, self.base.identity(), first_seq)?;
        let last = events.last().ok_or(BudgetExecutionError::IncompleteTurn)?;
        if events.iter().any(|event| event.turn_id != last.turn_id)
            || events.iter().filter(|event| is_terminal(event)).count() != 1
            || events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::TaskRunReport { .. }))
                .count()
                != 1
        {
            return Err(BudgetExecutionError::Boundary(
                "turn boundary or report cardinality differs",
            ));
        }
        // Seal before inspecting the final budget so no surviving clone can race
        // consumption or open a new run between validation and persistence.
        let checkpoint = self
            .task
            .seal_checkpoint(Some(BudgetEventCursor::from(last)))?;
        if !checkpoint.is_quiescent() {
            return Err(BudgetExecutionError::RecoveryRequired);
        }
        validate_closed_tail(events, checkpoint.report())?;
        self.store
            .compare_exchange(self.claim.revision(), &checkpoint)
            .await
            .map_err(|source| BudgetExecutionError::Commit {
                expected_revision: self.claim.revision(),
                checkpoint: Box::new(checkpoint.clone()),
                source,
            })?;
        Ok(checkpoint)
    }
}

pub(crate) fn verify_checkpoint_history(
    checkpoint: &BudgetCheckpoint,
    history: &[SessionEvent],
) -> Result<(), BudgetExecutionError> {
    checkpoint.validate()?;
    validate_addresses(history, checkpoint.identity(), 0)?;
    // Grants explain only limit/stop changes at this exact boundary. They cannot
    // explain changed charges, usage, identity, mode, or a newer/unclosed log tail.
    let report = checkpoint.boundary_report()?;
    match (checkpoint.anchor(), history.last()) {
        (None, None) => {
            if report.charged != BudgetAmounts::default()
                || report.reserved != BudgetAmounts::default()
                || report.active_time != std::time::Duration::ZERO
                || report.cleanup_time != std::time::Duration::ZERO
                || report.usage != BudgetUsageReport::default()
            {
                return Err(BudgetExecutionError::Boundary(
                    "nonempty budget without a log anchor",
                ));
            }
            Ok(())
        }
        (Some(anchor), Some(last)) if anchor == &BudgetEventCursor::from(last) => {
            validate_closed_tail(history, &report)
        }
        _ => Err(BudgetExecutionError::Boundary(
            "checkpoint is not at the confirmed log tail",
        )),
    }
}

impl Drop for BudgetExecutionLease {
    fn drop(&mut self) {
        // Revoke detached memory clones without inventing a storage commit. This
        // also applies to rejected runtime admission and cancelled acquisition users.
        let _ = self.task.seal_checkpoint(self.base.anchor().cloned());
    }
}

fn validate_addresses(
    events: &[SessionEvent],
    identity: &BudgetIdentity,
    first: u64,
) -> Result<(), BudgetExecutionError> {
    let mut ids = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        if event.session_id != identity.session_id
            || first.checked_add(index as u64) != Some(event.seq)
            || event.event_id.as_str().trim().is_empty()
            || event.turn_id.as_str().trim().is_empty()
            || !ids.insert(&event.event_id)
        {
            return Err(BudgetExecutionError::Boundary(
                "invalid physical event address",
            ));
        }
    }
    Ok(())
}

fn is_terminal(event: &SessionEvent) -> bool {
    matches!(
        event.kind,
        SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
    )
}

fn validate_closed_tail(
    events: &[SessionEvent],
    budget: &BudgetReport,
) -> Result<(), BudgetExecutionError> {
    let [.., report_event, terminal] = events else {
        return Err(BudgetExecutionError::IncompleteTurn);
    };
    let SessionEventKind::TaskRunReport { report } = &report_event.kind else {
        return Err(BudgetExecutionError::IncompleteTurn);
    };
    let Some(run) = &report.budget.run else {
        return Err(BudgetExecutionError::IncompleteTurn);
    };
    if !report.capabilities_drained
        || run.metrics.unconfirmed != BudgetEventCounts::default()
        || run.metrics.confirmed.model_requests != run.metrics.confirmed.model_results
        || run.metrics.confirmed.tool_calls != run.metrics.confirmed.tool_results
        || !is_terminal(terminal)
        || report_event.turn_id != terminal.turn_id
        || run.turn_id.as_ref() != Some(&terminal.turn_id)
        || run.open
        || budget.run.is_some()
        || !budget.pending.is_empty()
        || !report.budget.pending.is_empty()
        || report.budget.identity != budget.identity
        || report.budget.limits != budget.limits
        || report.budget.token_mode != budget.token_mode
        || report.budget.charged != budget.charged
        || report.budget.reserved != budget.reserved
        || report.budget.usage != budget.usage
        || report.budget.active_time != budget.active_time
        || report.budget.cleanup_time > budget.cleanup_time
        || report.budget.stop != budget.stop
    {
        return Err(BudgetExecutionError::Boundary(
            "budget report does not confirm checkpoint",
        ));
    }
    let consistent = matches!(
        (&report.stop, &terminal.kind),
        (
            TaskRunStop::Completed,
            SessionEventKind::Done {
                status: DoneStatus::Completed,
                ..
            }
        ) | (
            TaskRunStop::WaitingForInput,
            SessionEventKind::Done {
                status: DoneStatus::WaitingForInput,
                ..
            }
        ) | (
            TaskRunStop::CallerCancelled | TaskRunStop::RuntimeStopping,
            SessionEventKind::Done {
                status: DoneStatus::Cancelled,
                ..
            }
        ) | (
            TaskRunStop::Budget(_) | TaskRunStop::Failed,
            SessionEventKind::Error { .. }
        )
    );
    if !consistent {
        return Err(BudgetExecutionError::Boundary(
            "report and terminal dispositions disagree",
        ));
    }
    Ok(())
}
