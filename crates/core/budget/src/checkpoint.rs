//! Host-owned checkpoint transfer primitives. No persistence or log verification is implicit.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use jingwei_core::{EventId, SessionEvent, SessionId};
use serde::{Deserialize, Serialize};

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum BudgetCheckpointVersion {
    V1,
}

impl TryFrom<u16> for BudgetCheckpointVersion {
    type Error = &'static str;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::V1),
            _ => Err("unsupported budget checkpoint version"),
        }
    }
}

impl From<BudgetCheckpointVersion> for u16 {
    fn from(_: BudgetCheckpointVersion) -> Self {
        1
    }
}

/// An exact event address, not proof of durable commit or of the latest log tail.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetEventCursor {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub event_id: EventId,
    pub seq: u64,
}

impl From<&SessionEvent> for BudgetEventCursor {
    fn from(event: &SessionEvent) -> Self {
        Self {
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            event_id: event.event_id.clone(),
            seq: event.seq,
        }
    }
}

/// A sealed ledger image. Only trusted host code may restore it after checking
/// storage freshness, exclusive ownership and the canonical log boundary.
/// Serializing this object does not establish any of those guarantees.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetCheckpoint {
    version: BudgetCheckpointVersion,
    revision: u64,
    next_id: u64,
    anchor: Option<BudgetEventCursor>,
    report: BudgetReport,
    run_id: Option<u64>,
    recovery_frozen: bool,
}

/// Host expectations obtained independently from trusted storage and log checks.
/// Passing values copied blindly from the checkpoint does not verify freshness.
pub struct BudgetRestoreContext {
    pub identity: BudgetIdentity,
    pub revision: u64,
    pub confirmed_anchor: Option<BudgetEventCursor>,
    pub host_limits: BudgetLimits,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BudgetCheckpointError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("invalid budget checkpoint: {0}")]
    Invalid(&'static str),
    #[error("checkpoint identity differs from the host's expected task binding")]
    IdentityMismatch,
    #[error("checkpoint revision {actual} differs from expected {expected}")]
    RevisionMismatch { expected: u64, actual: u64 },
    #[error("checkpoint event cursor differs from the independently confirmed cursor")]
    AnchorMismatch,
    #[error("checkpoint revision space is exhausted")]
    RevisionExhausted,
}

impl BudgetCheckpoint {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn identity(&self) -> &BudgetIdentity {
        &self.report.identity
    }

    pub fn anchor(&self) -> Option<&BudgetEventCursor> {
        self.anchor.as_ref()
    }

    /// Evidence at sealing time, not live state and not execution authority.
    pub fn report(&self) -> &BudgetReport {
        &self.report
    }

    /// Storage implementors must additionally compare the actual stored revision
    /// atomically. This helper alone is not a CAS or a durability barrier.
    pub fn validate_successor(&self, expected_revision: u64) -> Result<(), BudgetCheckpointError> {
        self.validate()?;
        let expected = expected_revision
            .checked_add(1)
            .ok_or(BudgetCheckpointError::RevisionExhausted)?;
        if self.revision != expected {
            return Err(BudgetCheckpointError::RevisionMismatch {
                expected,
                actual: self.revision,
            });
        }
        Ok(())
    }

    /// Structural checks, not authenticity, authorization, freshness or log validation.
    pub fn validate(&self) -> Result<(), BudgetCheckpointError> {
        use BudgetCheckpointError::Invalid;
        let report = &self.report;
        let identity = &report.identity;
        if self.revision == 0 {
            return Err(Invalid("revision zero is reserved for absent storage"));
        }
        if identity.task_id.as_str().trim().is_empty()
            || identity.session_id.as_str().trim().is_empty()
            || identity.agent_key.trim().is_empty()
        {
            return Err(Invalid("empty identity"));
        }
        if let Some(anchor) = &self.anchor
            && (anchor.session_id != identity.session_id
                || anchor.turn_id.as_str().trim().is_empty()
                || anchor.event_id.as_str().trim().is_empty())
        {
            return Err(Invalid("invalid event cursor identity"));
        }
        if self.run_id.is_some() != report.run.is_some() {
            return Err(Invalid("run identity and run evidence disagree"));
        }
        if report.run.is_none() && !report.pending.is_empty() {
            return Err(Invalid("pending reservations without a run"));
        }
        let mut ids = HashSet::new();
        if let Some(id) = self.run_id {
            if id == 0 || id > self.next_id {
                return Err(Invalid("invalid run ID watermark"));
            }
            ids.insert(id);
        }
        let mut reserved = BudgetAmounts::default();
        let mut pending_attempts = BudgetAmounts::default();
        for pending in &report.pending {
            if pending.id == 0 || pending.id > self.next_id || !ids.insert(pending.id) {
                return Err(Invalid("invalid or duplicate reservation ID"));
            }
            if pending.failed_usage.is_some() && !pending.started {
                return Err(Invalid("usage evidence on an unstarted reservation"));
            }
            validate_request(pending.request, report.token_mode)
                .map_err(|_| Invalid("invalid pending resource request"))?;
            for resource in BudgetResource::ALL {
                let totals = if resource.is_attempt() {
                    &mut pending_attempts
                } else {
                    &mut reserved
                };
                totals.set(
                    resource,
                    totals
                        .get(resource)
                        .checked_add(pending.request.amounts.get(resource))
                        .ok_or(Invalid("pending totals overflow"))?,
                );
            }
        }
        if reserved != report.reserved {
            return Err(Invalid(
                "reserved totals do not equal pending variable resources",
            ));
        }
        if self.next_id == 0
            && (report.charged != BudgetAmounts::default() || report.active_time != Duration::ZERO)
        {
            return Err(Invalid("consumption without an allocated run"));
        }
        for resource in BudgetResource::ALL {
            if pending_attempts.get(resource) > report.charged.get(resource) {
                return Err(Invalid("pending attempts exceed task charges"));
            }
            if !fits(
                report.charged.get(resource),
                report.reserved.get(resource),
                0,
                report.limits.resources.get(resource),
            ) && report.stop.is_none()
            {
                return Err(Invalid("over-limit task without stop evidence"));
            }
        }
        if report.active_time > report.limits.active_time && report.stop.is_none() {
            return Err(Invalid("over-limit task time without stop evidence"));
        }
        for (resource, usage) in [
            (BudgetResource::InputTokens, report.usage.input_tokens),
            (BudgetResource::OutputTokens, report.usage.output_tokens),
            (
                BudgetResource::ToolOutputBytes,
                report.usage.tool_output_bytes,
            ),
        ] {
            if usage
                .actual
                .checked_add(usage.estimated)
                .is_none_or(|total| total > report.charged.get(resource))
            {
                return Err(Invalid("usage evidence exceeds conservative charges"));
            }
        }
        if let Some(run) = &report.run {
            if run
                .turn_id
                .as_ref()
                .is_some_and(|id| id.as_str().trim().is_empty())
            {
                return Err(Invalid("empty run TurnId"));
            }
            if run.reserved != report.reserved
                || run.active_time > report.active_time
                || run.cleanup_time > report.cleanup_time
            {
                return Err(Invalid("run totals disagree with task totals"));
            }
            for resource in BudgetResource::ALL {
                if run.charged.get(resource) > report.charged.get(resource)
                    || pending_attempts.get(resource) > run.charged.get(resource)
                    || run.limits.resources.get(resource) > report.limits.resources.get(resource)
                {
                    return Err(Invalid("run resource totals or limits exceed task values"));
                }
                if !fits(
                    run.charged.get(resource),
                    run.reserved.get(resource),
                    0,
                    run.limits.resources.get(resource),
                ) && run.stop.or(report.stop).is_none()
                {
                    return Err(Invalid("over-limit run without stop evidence"));
                }
            }
            if run.limits.active_time > report.limits.active_time
                || (run.active_time > run.limits.active_time && run.stop.or(report.stop).is_none())
            {
                return Err(Invalid("invalid run time limit"));
            }
        }
        Ok(())
    }
}

impl TaskBudget {
    /// Irreversibly seal every clone/scope/reservation of this ledger and transfer
    /// its image. Drain work first for a resumable image. This does not cancel IO,
    /// persist bytes, commit an event, or establish a crash-safe checkpoint.
    pub fn seal_checkpoint(
        &self,
        anchor: Option<BudgetEventCursor>,
    ) -> Result<BudgetCheckpoint, BudgetCheckpointError> {
        let mut state = self.0.lock()?;
        if state.sealed {
            return Err(BudgetError::CheckpointSealed.into());
        }
        self.0.tick(&mut state);
        let checkpoint = BudgetCheckpoint {
            version: BudgetCheckpointVersion::V1,
            revision: state
                .checkpoint_revision
                .checked_add(1)
                .ok_or(BudgetCheckpointError::RevisionExhausted)?,
            next_id: state.next_id,
            anchor,
            report: self.0.snapshot(&state),
            run_id: state.run.as_ref().map(|run| run.id),
            recovery_frozen: state.recovery_frozen,
        };
        checkpoint.validate()?;
        state.sealed = true;
        Ok(checkpoint)
    }

    /// Restore only after the host has established exclusive ownership of the
    /// latest committed image and reconciled the event log. This method does no IO
    /// and does not prevent another process from restoring the same bytes.
    pub fn restore_checkpoint(
        checkpoint: BudgetCheckpoint,
        context: BudgetRestoreContext,
        clock: Arc<dyn BudgetClock>,
    ) -> Result<Self, BudgetCheckpointError> {
        checkpoint.validate()?;
        if checkpoint.identity() != &context.identity {
            return Err(BudgetCheckpointError::IdentityMismatch);
        }
        if checkpoint.revision != context.revision {
            return Err(BudgetCheckpointError::RevisionMismatch {
                expected: context.revision,
                actual: checkpoint.revision,
            });
        }
        if checkpoint.anchor != context.confirmed_anchor {
            return Err(BudgetCheckpointError::AnchorMismatch);
        }
        let report = checkpoint.report;
        let limits = report.limits.tightened_by(context.host_limits);
        let frozen =
            checkpoint.recovery_frozen || report.run.is_some() || !report.pending.is_empty();
        let mut stop = report.stop;
        if frozen {
            stop.get_or_insert(BudgetStopReason::RecoveryRequired);
        } else if report.active_time >= limits.active_time {
            stop.get_or_insert(BudgetStopReason::ActiveTime);
        }
        for resource in BudgetResource::ALL {
            if !fits(
                report.charged.get(resource),
                report.reserved.get(resource),
                0,
                limits.resources.get(resource),
            ) {
                stop.get_or_insert(BudgetStopReason::ResourceLimit(resource));
            }
        }
        let run = report.run.zip(checkpoint.run_id).map(|(mut report, id)| {
            report.open = false;
            report.limits = report.limits.tightened_by(limits);
            RunState {
                id,
                report,
                cleaning: true,
            }
        });
        let last_clock = clock.now();
        Ok(Self(Arc::new(Inner {
            identity: report.identity,
            limits,
            token_mode: report.token_mode,
            clock,
            waiters: Mutex::new(Vec::new()),
            state: Mutex::new(State {
                sealed: false,
                recovery_frozen: frozen,
                checkpoint_revision: checkpoint.revision,
                charged: report.charged,
                reserved: report.reserved,
                usage: report.usage,
                active_time: report.active_time,
                cleanup_time: report.cleanup_time,
                stop,
                run,
                pending: report.pending.into_iter().map(|p| (p.id, p)).collect(),
                next_id: checkpoint.next_id,
                last_clock,
            }),
        })))
    }
}

pub type BudgetCheckpointFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetCheckpointCommit {
    Committed,
    ReplayedExact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetCheckpointCommitCertainty {
    DefinitelyNotCommitted,
    Indeterminate,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum BudgetCheckpointStoreError {
    #[error("checkpoint compare-and-exchange conflict: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error(transparent)]
    Invalid(#[from] BudgetCheckpointError),
    #[error("checkpoint storage failed ({certainty:?}): {message}")]
    Storage {
        certainty: BudgetCheckpointCommitCertainty,
        message: String,
    },
}

/// Host-selected storage, not a global provider. Implementors own durable atomic
/// CAS and read consistency across all their writers. No default adapter is supplied.
pub trait BudgetCheckpointStore: Send + Sync {
    /// Latest committed image for the exact binding; absence is not permission to
    /// recreate a task whose history indicates prior execution.
    fn load<'a>(
        &'a self,
        identity: &'a BudgetIdentity,
    ) -> BudgetCheckpointFuture<'a, Result<Option<BudgetCheckpoint>, BudgetCheckpointStoreError>>;

    /// Atomically replace expected_revision (0 = absent) with its next revision.
    /// Validate the image, reject wrong identity, and return only after durability.
    /// Exact retries of the same transition may return ReplayedExact. For an
    /// indeterminate result, reconcile this same image before any new transition.
    fn compare_exchange<'a>(
        &'a self,
        expected_revision: u64,
        checkpoint: &'a BudgetCheckpoint,
    ) -> BudgetCheckpointFuture<'a, Result<BudgetCheckpointCommit, BudgetCheckpointStoreError>>;
}
