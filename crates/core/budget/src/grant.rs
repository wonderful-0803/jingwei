//! Host-only, audited limit increases at confirmed quiescent boundaries.

use std::collections::HashSet;

use jingwei_core::{BudgetOperationId, SessionEvent};
use serde::{Deserialize, Serialize};

use super::*;

/// Finite lifetime audit retention. Exhaustion fails closed; no implicit pruning.
pub const MAX_BUDGET_GRANTS: usize = 64;
/// UTF-8 byte bound for each operation ID, actor, source and reason.
pub const MAX_BUDGET_AUDIT_TEXT_BYTES: usize = 1024;

/// Supplied by an authenticated host, never by Agent/model/tool output. Actor and
/// source are provenance labels, not authentication or authorization by themselves.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetGrantRequest {
    pub operation_id: BudgetOperationId,
    pub actor: String,
    pub source: String,
    pub reason: String,
    /// Absolute new limits, not an additive delta (safe to retry unchanged).
    pub new_limits: BudgetLimits,
}

/// Committed in the same image as the limit change, then retained unchanged.
/// No execution claim is released by a grant: only already quiescent bases qualify.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetGrantRecord {
    pub request: BudgetGrantRequest,
    pub source_revision: u64,
    pub confirmed_anchor: Option<BudgetEventCursor>,
    pub old_limits: BudgetLimits,
    pub old_stop: Option<BudgetStopReason>,
}

#[derive(Debug, thiserror::Error)]
pub enum BudgetGrantError {
    #[error(transparent)]
    Checkpoint(#[from] BudgetCheckpointError),
    #[error(transparent)]
    Boundary(#[from] BudgetExecutionError),
    #[error(transparent)]
    Store(#[from] BudgetCheckpointStoreError),
    #[error("checkpoint missing; grants cannot initialize a task")]
    Missing,
    #[error("unfinished execution or frozen state requires independent recovery, not a grant")]
    RecoveryRequired,
    #[error("invalid host grant: {0}")]
    Invalid(&'static str),
    #[error("operation ID already exists with different content or source revision")]
    OperationConflict,
    #[error("bounded grant audit is full; explicit retention protocol required")]
    AuditFull,
    #[error("grant transition from revision {expected_revision} failed: {source}")]
    Commit {
        expected_revision: u64,
        checkpoint: Box<BudgetCheckpoint>,
        source: BudgetCheckpointStoreError,
    },
}

/// Neither variant grants execution authority. Acquire a new execution lease and
/// verify fresh Session history before work. AlreadyApplied may refer to an older
/// revision even while the latest checkpoint has an unfinished execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BudgetGrantOutcome {
    Applied {
        checkpoint: Box<BudgetCheckpoint>,
        commit: BudgetCheckpointCommit,
    },
    AlreadyApplied {
        record: Box<BudgetGrantRecord>,
    },
}

/// Load, verify and atomically commit one host-approved grant. The host must hold
/// independent Session writer ownership throughout this call, supply fresh durable
/// physical history, and authenticate the request against its own policy. The CAS
/// competes with execution acquisition; this function never creates a TaskBudget.
///
/// On cancellation or uncertainty retain the original request/context. Reconcile
/// the exact candidate (Commit error) or reissue the same operation, never invent
/// a replacement ID. An existing matching ID returns an audit receipt, not a lease.
pub async fn grant_budget(
    store: &dyn BudgetCheckpointStore,
    context: &BudgetRestoreContext,
    history: &[SessionEvent],
    request: &BudgetGrantRequest,
) -> Result<BudgetGrantOutcome, BudgetGrantError> {
    let base = store
        .load(&context.identity)
        .await?
        .ok_or(BudgetGrantError::Missing)?;
    base.validate()?;
    if base.identity() != &context.identity {
        return Err(BudgetCheckpointError::IdentityMismatch.into());
    }
    validate_request_text(request).map_err(BudgetGrantError::Invalid)?;
    // Retry detection precedes current-boundary checks: this is historical evidence
    // only, so it remains observable after later work or stricter host policy.
    if let Some(record) = base
        .grants
        .iter()
        .find(|r| r.request.operation_id == request.operation_id)
    {
        if &record.request != request
            || record.source_revision != context.revision
            || record.confirmed_anchor != context.confirmed_anchor
        {
            return Err(BudgetGrantError::OperationConflict);
        }
        return Ok(BudgetGrantOutcome::AlreadyApplied {
            record: Box::new(record.clone()),
        });
    }
    if base.revision() != context.revision {
        return Err(BudgetCheckpointError::RevisionMismatch {
            expected: context.revision,
            actual: base.revision(),
        }
        .into());
    }
    if base.anchor() != context.confirmed_anchor.as_ref() {
        return Err(BudgetCheckpointError::AnchorMismatch.into());
    }
    if !base.is_quiescent() {
        return Err(BudgetGrantError::RecoveryRequired);
    }
    execution::verify_checkpoint_history(&base, history)?;
    if base.grants.len() == MAX_BUDGET_GRANTS {
        return Err(BudgetGrantError::AuditFull);
    }
    validate_increase(base.report.limits, request.new_limits, base.report.stop)
        .map_err(BudgetGrantError::Invalid)?;
    if request.new_limits.tightened_by(context.host_limits) != request.new_limits {
        return Err(BudgetGrantError::Invalid(
            "new limits exceed the host ceiling",
        ));
    }
    // All consumed resources and time must fit, not just the first stop reason.
    if base.report.active_time >= request.new_limits.active_time
        || BudgetResource::ALL
            .iter()
            .any(|r| base.report.charged.get(*r) > request.new_limits.resources.get(*r))
    {
        return Err(BudgetGrantError::Invalid(
            "new limits do not cover retained consumption/time",
        ));
    }
    let record = BudgetGrantRecord {
        request: request.clone(),
        source_revision: base.revision(),
        confirmed_anchor: base.anchor().cloned(),
        old_limits: base.report.limits,
        old_stop: base.report.stop,
    };
    let mut checkpoint = base.clone();
    checkpoint.version = BudgetCheckpointVersion::V3;
    checkpoint.revision = base
        .revision()
        .checked_add(1)
        .ok_or(BudgetCheckpointError::RevisionExhausted)?;
    checkpoint.report.limits = request.new_limits;
    checkpoint.report.stop = None;
    checkpoint.grants.push(record);
    checkpoint.validate_transition(Some(&base))?;
    let commit = store
        .compare_exchange(base.revision(), &checkpoint)
        .await
        .map_err(|source| BudgetGrantError::Commit {
            expected_revision: base.revision(),
            checkpoint: Box::new(checkpoint.clone()),
            source,
        })?;
    Ok(BudgetGrantOutcome::Applied {
        checkpoint: Box::new(checkpoint),
        commit,
    })
}

impl BudgetCheckpoint {
    pub fn grants(&self) -> &[BudgetGrantRecord] {
        &self.grants
    }

    /// Storage must call this against its actual previous image under the CAS lock
    /// and when replaying an on-disk chain. Structural evidence, not host identity
    /// authentication, Session exclusivity, or canonical log freshness.
    pub fn validate_transition(
        &self,
        previous: Option<&Self>,
    ) -> Result<(), BudgetCheckpointError> {
        use BudgetCheckpointError::Invalid;
        self.validate_successor(previous.map_or(0, Self::revision))?;
        let Some(previous) = previous else {
            return if self.grants.is_empty() {
                Ok(())
            } else {
                Err(Invalid("initial image contains grants"))
            };
        };
        previous.validate()?;
        if previous.identity() != self.identity() {
            return Err(BudgetCheckpointError::IdentityMismatch);
        }
        if !self.grants.starts_with(&previous.grants) {
            return Err(Invalid("grant audit removed or rewritten"));
        }
        match &self.grants[previous.grants.len()..] {
            [] => {
                if self.report.limits.tightened_by(previous.report.limits) != self.report.limits {
                    return Err(Invalid("limits increased without an atomic grant record"));
                }
            }
            [record] => {
                if !previous.is_quiescent()
                    || record.source_revision != previous.revision()
                    || record.confirmed_anchor.as_ref() != previous.anchor()
                    || record.old_limits != previous.report.limits
                    || record.old_stop != previous.report.stop
                {
                    return Err(Invalid("grant does not describe the quiescent predecessor"));
                }
                let mut expected = previous.clone();
                expected.version = BudgetCheckpointVersion::V3;
                expected.revision = self.revision;
                expected.report.limits = record.request.new_limits;
                expected.report.stop = None;
                expected.grants.push(record.clone());
                if self != &expected {
                    return Err(Invalid(
                        "grant altered state other than limits and budget stop",
                    ));
                }
            }
            _ => return Err(Invalid("multiple grants in one transition")),
        }
        Ok(())
    }

    pub(crate) fn validate_grants(&self) -> Result<(), BudgetCheckpointError> {
        use BudgetCheckpointError::Invalid;
        if (!self.grants.is_empty() && self.version != BudgetCheckpointVersion::V3)
            || self.grants.len() > MAX_BUDGET_GRANTS
        {
            return Err(Invalid("invalid grant version or count"));
        }
        let mut ids = HashSet::new();
        let mut previous_revision = 0;
        let mut previous_anchor: Option<&BudgetEventCursor> = None;
        for record in &self.grants {
            validate_request_text(&record.request).map_err(Invalid)?;
            validate_increase(
                record.old_limits,
                record.request.new_limits,
                record.old_stop,
            )
            .map_err(Invalid)?;
            if record.source_revision <= previous_revision
                || record.source_revision >= self.revision()
                || !ids.insert(&record.request.operation_id)
            {
                return Err(Invalid("invalid grant revision or duplicate operation ID"));
            }
            if let Some(anchor) = &record.confirmed_anchor {
                if anchor.session_id != self.identity().session_id
                    || anchor.event_id.as_str().trim().is_empty()
                    || anchor.turn_id.as_str().trim().is_empty()
                    || self.anchor().is_none_or(|tail| {
                        anchor.seq > tail.seq || (anchor.seq == tail.seq && anchor != tail)
                    })
                    || previous_anchor.is_some_and(|old| {
                        anchor.seq < old.seq || (anchor.seq == old.seq && anchor != old)
                    })
                {
                    return Err(Invalid("invalid grant log anchor"));
                }
            } else if previous_anchor.is_some() {
                return Err(Invalid("grant log anchor moved backwards"));
            }
            previous_revision = record.source_revision;
            previous_anchor = record.confirmed_anchor.as_ref();
        }
        for pair in self.grants.windows(2) {
            if pair[0].confirmed_anchor == pair[1].confirmed_anchor
                && (pair[0].request.new_limits != pair[1].old_limits || pair[1].old_stop.is_some())
            {
                return Err(Invalid("discontinuous grants at the same boundary"));
            }
        }
        Ok(())
    }

    /// Reconstruct only the pre-grant limits/stop at this exact canonical boundary.
    /// Never refunds consumption or changes identity, usage, mode or pending work.
    pub(crate) fn boundary_report(&self) -> Result<BudgetReport, BudgetCheckpointError> {
        let mut report = self.report.clone();
        for record in self
            .grants
            .iter()
            .rev()
            .take_while(|r| r.confirmed_anchor.as_ref() == self.anchor())
        {
            if report.limits != record.request.new_limits || report.stop.is_some() {
                return Err(BudgetCheckpointError::Invalid(
                    "grant chain differs from boundary limits/stop",
                ));
            }
            report.limits = record.old_limits;
            report.stop = record.old_stop;
        }
        Ok(report)
    }
}

fn validate_request_text(request: &BudgetGrantRequest) -> Result<(), &'static str> {
    for text in [
        request.operation_id.as_str(),
        &request.actor,
        &request.source,
        &request.reason,
    ] {
        if text.trim().is_empty() || text.len() > MAX_BUDGET_AUDIT_TEXT_BYTES {
            return Err("empty or oversized audit text");
        }
    }
    Ok(())
}

fn validate_increase(
    old: BudgetLimits,
    new: BudgetLimits,
    stop: Option<BudgetStopReason>,
) -> Result<(), &'static str> {
    if old == new || new.tightened_by(old) != old {
        return Err("grant must increase at least one limit and decrease none");
    }
    match stop {
        None => Ok(()),
        Some(BudgetStopReason::ResourceLimit(resource))
            if new.resources.get(resource) > old.resources.get(resource) =>
        {
            Ok(())
        }
        Some(BudgetStopReason::ActiveTime) if new.active_time > old.active_time => Ok(()),
        _ => Err("grant cannot clear this stop or does not increase the stopped resource"),
    }
}
