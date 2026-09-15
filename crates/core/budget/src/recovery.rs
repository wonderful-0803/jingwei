//! Conservative host recovery: only a retained exact final image can close a claim.
use super::*;
use jingwei_core::{BudgetExecutionId, BudgetOperationId};
use jingwei_session::SessionRecoveryOwnership;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_BUDGET_RECOVERIES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetRecoveryRequest {
    pub operation_id: BudgetOperationId,
    pub execution_id: BudgetExecutionId,
    pub source_revision: u64,
    pub actor: String,
    pub source: String,
    pub reason: String,
}

/// Exact candidate evidence, including the otherwise unrecoverable ID watermark.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetRecoveryRecord {
    pub request: BudgetRecoveryRequest,
    pub ownership_id: String,
    pub confirmed_anchor: Option<BudgetEventCursor>,
    pub candidate_report: BudgetReport,
    pub candidate_next_id: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetRecoveryError {
    #[error(transparent)]
    Checkpoint(#[from] BudgetCheckpointError),
    #[error(transparent)]
    Store(#[from] BudgetCheckpointStoreError),
    #[error(transparent)]
    History(#[from] jingwei_session::SessionPersistenceError),
    #[error(transparent)]
    Boundary(#[from] BudgetExecutionError),
    #[error("recovery refused: {0}")]
    Refused(&'static str),
    #[error("recovery commit failed: {source}")]
    Commit {
        checkpoint: Box<BudgetCheckpoint>,
        source: BudgetCheckpointStoreError,
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BudgetRecoveryOutcome {
    Applied(Box<BudgetCheckpoint>),
    AlreadyApplied(Box<BudgetRecoveryRecord>),
    /// The original exact final candidate was already persisted; no new mutation.
    OriginalCommitted(Box<BudgetCheckpoint>),
}

/// Requires independent exclusive ownership covering the entire future. The
/// retained candidate must come from the original Commit failure, never be
/// reconstructed from a report (which lacks the ledger's ID watermark).
/// Cancellation may commit the exact audited image; retry with unchanged inputs.
pub async fn recover_budget_candidate(
    store: Arc<dyn BudgetCheckpointStore>,
    ownership: Arc<dyn SessionRecoveryOwnership>,
    request: BudgetRecoveryRequest,
    candidate: &BudgetCheckpoint,
) -> Result<BudgetRecoveryOutcome, BudgetRecoveryError> {
    use BudgetRecoveryError::Refused;
    validate_request(&request).map_err(Refused)?;
    candidate.validate()?;
    if ownership.session_id() != &candidate.identity().session_id
        || ownership.ownership_id().trim().is_empty()
        || ownership.ownership_id().len() > MAX_BUDGET_AUDIT_TEXT_BYTES
    {
        return Err(Refused("ownership identity differs"));
    }
    if !candidate.is_quiescent()
        || request.source_revision.checked_add(1) != Some(candidate.revision())
    {
        return Err(Refused("candidate is not the exact ready successor"));
    }
    let current = store
        .load(candidate.identity())
        .await?
        .ok_or(Refused("checkpoint missing"))?;
    current.validate()?;
    if let Some((index, record)) = current
        .recoveries
        .iter()
        .enumerate()
        .find(|(_, r)| r.request.operation_id == request.operation_id)
    {
        if record.request != request
            || record.candidate_report != candidate.report
            || record.candidate_next_id != candidate.next_id
            || record.confirmed_anchor.as_ref() != candidate.anchor()
            || candidate.recoveries != current.recoveries[..index]
            || !current
                .grants
                .iter()
                .take_while(|grant| grant.source_revision < candidate.revision())
                .eq(candidate.grants.iter())
        {
            return Err(Refused("operation ID reused with different evidence"));
        }
        return Ok(BudgetRecoveryOutcome::AlreadyApplied(Box::new(
            record.clone(),
        )));
    }
    let history = ownership.history().await?;
    super::execution::verify_checkpoint_history(candidate, &history)?;
    if current == *candidate {
        return Ok(BudgetRecoveryOutcome::OriginalCommitted(Box::new(current)));
    }
    if current.revision() != request.source_revision
        || current.execution_id() != Some(&request.execution_id)
    {
        return Err(Refused("execution claim changed"));
    }
    if current.recoveries.len() >= MAX_BUDGET_RECOVERIES {
        return Err(Refused("recovery audit full"));
    }
    let mut recovered = candidate.clone();
    recovered.version = BudgetCheckpointVersion::V4;
    recovered.recoveries.push(BudgetRecoveryRecord {
        request,
        ownership_id: ownership.ownership_id().to_owned(),
        confirmed_anchor: candidate.anchor().cloned(),
        candidate_report: candidate.report.clone(),
        candidate_next_id: candidate.next_id,
    });
    recovered.validate_transition(Some(&current))?;
    store
        .compare_exchange_guarded(current.revision(), &recovered, ownership)
        .await
        .map_err(|source| BudgetRecoveryError::Commit {
            checkpoint: Box::new(recovered.clone()),
            source,
        })?;
    Ok(BudgetRecoveryOutcome::Applied(Box::new(recovered)))
}

fn validate_request(request: &BudgetRecoveryRequest) -> Result<(), &'static str> {
    for text in [
        request.operation_id.as_str(),
        request.execution_id.as_str(),
        &request.actor,
        &request.source,
        &request.reason,
    ] {
        if text.trim().is_empty() || text.len() > MAX_BUDGET_AUDIT_TEXT_BYTES {
            return Err("empty or oversized recovery audit");
        }
    }
    if request.source_revision == 0 {
        return Err("zero recovery revision");
    }
    Ok(())
}

impl BudgetCheckpoint {
    pub fn recoveries(&self) -> &[BudgetRecoveryRecord] {
        &self.recoveries
    }

    pub(crate) fn validate_recoveries(&self) -> Result<(), BudgetCheckpointError> {
        use BudgetCheckpointError::Invalid;
        if self.recoveries.len() > MAX_BUDGET_RECOVERIES
            || (!self.recoveries.is_empty() && self.version != BudgetCheckpointVersion::V4)
        {
            return Err(Invalid("invalid recovery version/count"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut revision = 0;
        for record in &self.recoveries {
            validate_request(&record.request).map_err(Invalid)?;
            if !ids.insert(&record.request.operation_id)
                || record.request.source_revision <= revision
                || record.request.source_revision >= self.revision
                || record.ownership_id.trim().is_empty()
                || record.ownership_id.len() > MAX_BUDGET_AUDIT_TEXT_BYTES
                || record.candidate_report.identity != self.report.identity
                || record.candidate_report.run.is_some()
                || !record.candidate_report.pending.is_empty()
                || record.candidate_next_id > self.next_id
            {
                return Err(Invalid("invalid recovery evidence"));
            }
            // Validate the retained evidence with the same ledger invariants as
            // a standalone candidate; do not recurse through historical audits.
            Self::validate_recovery_evidence(record)?;
            revision = record.request.source_revision;
        }
        Ok(())
    }

    pub(crate) fn validate_recovery_transition(
        &self,
        previous: Option<&Self>,
    ) -> Result<(), BudgetCheckpointError> {
        use BudgetCheckpointError::Invalid;
        let Some(previous) = previous else {
            return if self.recoveries.is_empty() {
                Ok(())
            } else {
                Err(Invalid("initial recovery audit"))
            };
        };
        if !self.recoveries.starts_with(&previous.recoveries) {
            return Err(Invalid("recovery audit rewritten"));
        }
        match &self.recoveries[previous.recoveries.len()..] {
            [] => Ok(()),
            [record] => {
                if previous.execution_id() != Some(&record.request.execution_id)
                    || previous.revision() != record.request.source_revision
                    || !self.is_quiescent()
                    || self.grants != previous.grants
                    || self.report != record.candidate_report
                    || self.next_id != record.candidate_next_id
                    || self.anchor() != record.confirmed_anchor.as_ref()
                    || self.next_id < previous.next_id
                    || self.report.limits != previous.report.limits
                    || self.report.token_mode != previous.report.token_mode
                    || self.report.active_time < previous.report.active_time
                    || self.report.cleanup_time < previous.report.cleanup_time
                    || self.report.reserved != BudgetAmounts::default()
                {
                    return Err(Invalid(
                        "recovery does not preserve original claim and candidate",
                    ));
                }
                for resource in BudgetResource::ALL {
                    if self.report.charged.get(resource) < previous.report.charged.get(resource) {
                        return Err(Invalid("recovery refunds consumed resources"));
                    }
                }
                for (next, old) in [
                    (
                        self.report.usage.input_tokens,
                        previous.report.usage.input_tokens,
                    ),
                    (
                        self.report.usage.output_tokens,
                        previous.report.usage.output_tokens,
                    ),
                    (
                        self.report.usage.tool_output_bytes,
                        previous.report.usage.tool_output_bytes,
                    ),
                ] {
                    if next.actual < old.actual
                        || next.estimated < old.estimated
                        || next.unknown < old.unknown
                    {
                        return Err(Invalid("recovery erases usage evidence"));
                    }
                }
                Ok(())
            }
            _ => Err(Invalid("multiple recoveries in one transition")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetRecoveryAssessment {
    Ready,
    /// A claim without new events is not proof of zero work or zero time.
    FrozenNoNewEvents,
    FrozenUnconfirmedWork,
    /// The report lacks an ID watermark; retain the original sealed candidate.
    FrozenClosedTailNeedsCandidate,
}

pub async fn inspect_budget_recovery(
    checkpoint: &BudgetCheckpoint,
    ownership: &dyn SessionRecoveryOwnership,
) -> Result<BudgetRecoveryAssessment, BudgetRecoveryError> {
    checkpoint.validate()?;
    if ownership.session_id() != &checkpoint.identity().session_id {
        return Err(BudgetRecoveryError::Refused("ownership identity differs"));
    }
    let history = ownership.history().await?;
    jingwei_session::validate_physical_log(ownership.session_id(), &history)
        .map_err(|_| BudgetRecoveryError::Refused("invalid physical history"))?;
    if checkpoint.is_quiescent() {
        super::execution::verify_checkpoint_history(checkpoint, &history)?;
        return Ok(BudgetRecoveryAssessment::Ready);
    }
    if history.last().map(BudgetEventCursor::from).as_ref() == checkpoint.anchor() {
        return Ok(BudgetRecoveryAssessment::FrozenNoNewEvents);
    }
    if let [.., report_event, terminal] = history.as_slice()
        && matches!(
            report_event.kind,
            jingwei_core::SessionEventKind::TaskRunReport { .. }
        )
        && matches!(
            terminal.kind,
            jingwei_core::SessionEventKind::Done { .. }
                | jingwei_core::SessionEventKind::Error { .. }
        )
    {
        return Ok(BudgetRecoveryAssessment::FrozenClosedTailNeedsCandidate);
    }
    Ok(BudgetRecoveryAssessment::FrozenUnconfirmedWork)
}
