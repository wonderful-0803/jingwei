//! Conservative host retry planning. No IO, model calls, permissions or refunds.
use crate::*;
use jingwei_core::{EventId, SessionId, TaskId};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ToolRetryError {
    #[error("tool recovery evidence is missing, malformed, or exceeds limits")]
    InvalidEvidence,
    #[error("legacy call has no stable operation identity")]
    LegacyCall,
    #[error("operation already completed; do not retry")]
    Completed,
    #[error("external outcome requires host review")]
    NeedsReview,
    #[error("a newer attempt exists for this operation")]
    NotLatestAttempt,
    #[error("task, session, tool, arguments or effect contract changed")]
    BindingChanged,
    #[error("this retry plan was already consumed; inspect current evidence again")]
    Consumed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolRecoveryStatus {
    Completed,
    NotExecuted,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRecoveryAssessment {
    pub call: ToolCall,
    pub result: Option<ToolResult>,
    pub status: ToolRecoveryStatus,
}
fn text_valid(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1024
}
pub fn validate_tool_contract(contract: &ToolEffectContract) -> Result<(), ToolRetryError> {
    if !text_valid(&contract.revision) {
        return Err(ToolRetryError::InvalidEvidence);
    }
    Ok(())
}
fn validate_operation(operation: &ToolOperation) -> Result<(), ToolRetryError> {
    validate_tool_contract(&operation.contract)?;
    if !text_valid(operation.task_id.as_str()) || !text_valid(&operation.key) {
        return Err(ToolRetryError::InvalidEvidence);
    }
    if let Some(retry) = &operation.retry {
        if !text_valid(&retry.call_id) || !text_valid(retry.event_id.as_str()) {
            return Err(ToolRetryError::InvalidEvidence);
        }
        if let Some(review) = &retry.review {
            validate_review(review)?;
        }
    }
    Ok(())
}
fn validate_review(review: &ToolRetryReview) -> Result<(), ToolRetryError> {
    if !text_valid(&review.actor) || !text_valid(&review.evidence) {
        return Err(ToolRetryError::InvalidEvidence);
    }
    Ok(())
}
/// Full confirmed physical history under host Session ownership is required.
/// A failed result's retryable flag is never evidence that no side effect occurred.
pub fn inspect_tool_recovery(
    history: &[SessionEvent],
    call_id: &str,
) -> Result<ToolRecoveryAssessment, ToolRetryError> {
    let (call, result, _) = evidence(history, call_id)?;
    let status = match result.map(|r| &r.outcome) {
        Some(ToolRecordedOutcome::Succeeded { .. }) => ToolRecoveryStatus::Completed,
        Some(ToolRecordedOutcome::Failed {
            category:
                ToolFailureCategory::Unavailable
                | ToolFailureCategory::InvalidArguments
                | ToolFailureCategory::Denied
                | ToolFailureCategory::GuardFault
                | ToolFailureCategory::ApprovalUnavailable
                | ToolFailureCategory::ApprovalDenied
                | ToolFailureCategory::ApprovalFault,
            ..
        }) => ToolRecoveryStatus::NotExecuted,
        _ => ToolRecoveryStatus::OutcomeUnknown,
    };
    Ok(ToolRecoveryAssessment {
        call: call.clone(),
        result: result.cloned(),
        status,
    })
}
fn evidence<'a>(
    history: &'a [SessionEvent],
    call_id: &str,
) -> Result<(&'a ToolCall, Option<&'a ToolResult>, &'a SessionEvent), ToolRetryError> {
    if history.is_empty() || history.len() > 100_000 || !text_valid(call_id) {
        return Err(ToolRetryError::InvalidEvidence);
    }
    serde_json::to_writer(HistoryLimit(16 * 1024 * 1024), history)
        .map_err(|_| ToolRetryError::InvalidEvidence)?;
    jingwei_session::validate_physical_log(&history[0].session_id, history)
        .map_err(|_| ToolRetryError::InvalidEvidence)?;
    let mut calls = BTreeMap::<&str, (&ToolCall, &SessionEvent, Option<&ToolResult>)>::new();
    let mut operations = BTreeMap::new();
    for event in history {
        match &event.kind {
            SessionEventKind::ToolCall { call } => {
                if !text_valid(&call.id)
                    || call.name.is_empty()
                    || call.name.len() > MAX_TOOL_NAME_BYTES
                    || calls.contains_key(call.id.as_str())
                {
                    return Err(ToolRetryError::InvalidEvidence);
                }
                if let Some(op) = &call.operation {
                    validate_operation(op)?;
                    let key = (&op.task_id, op.key.as_str());
                    if let Some((old, old_event)) = operations.get(&key).copied() {
                        let old: &ToolCall = old;
                        let old_event: &SessionEvent = old_event;
                        if old.name != call.name
                            || old.arguments != call.arguments
                            || old.operation.as_ref().unwrap().contract != op.contract
                        {
                            return Err(ToolRetryError::BindingChanged);
                        }
                        if op
                            .retry
                            .as_ref()
                            .is_none_or(|r| r.call_id != old.id || r.event_id != old_event.event_id)
                        {
                            return Err(ToolRetryError::InvalidEvidence);
                        }
                    } else if op.retry.is_some() {
                        return Err(ToolRetryError::InvalidEvidence);
                    }
                    operations.insert(key, (call, event));
                }
                calls.insert(&call.id, (call, event, None));
            }
            SessionEventKind::ToolResult { result } => {
                let Some((_, call_event, stored)) = calls.get_mut(result.call_id.as_str()) else {
                    return Err(ToolRetryError::InvalidEvidence);
                };
                if call_event.turn_id != event.turn_id || stored.replace(result).is_some() {
                    return Err(ToolRetryError::InvalidEvidence);
                }
            }
            _ => {}
        }
    }
    let (call, event, result) = calls
        .get(call_id)
        .copied()
        .ok_or(ToolRetryError::InvalidEvidence)?;
    if let Some(op) = &call.operation
        && operations
            .get(&(&op.task_id, op.key.as_str()))
            .is_some_and(|(latest, _)| latest.id != call_id)
    {
        return Err(ToolRetryError::NotLatestAttempt);
    }
    if let Some(op) = &call.operation {
        for (previous, _, result) in calls.values() {
            if previous.id != call.id
                && previous
                    .operation
                    .as_ref()
                    .is_some_and(|p| p.task_id == op.task_id && p.key == op.key)
                && result
                    .is_some_and(|r| matches!(r.outcome, ToolRecordedOutcome::Succeeded { .. }))
            {
                return Err(ToolRetryError::Completed);
            }
        }
    }
    Ok((call, result, event))
}
/// Ephemeral host evidence, not execution authority. Clones share a single-use
/// latch; no serialization or global replay lock. Across processes keep exclusive
/// Session ownership and inspect fresh history. Gateways must revalidate grants.
#[derive(Clone, Debug)]
pub struct ToolRetryPlan {
    source: ToolCall,
    session: SessionId,
    source_event: EventId,
    review: Option<ToolRetryReview>,
    consumed: Arc<AtomicBool>,
}
impl PartialEq for ToolRetryPlan {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.consumed, &other.consumed)
    }
}
impl Eq for ToolRetryPlan {}
impl ToolRetryPlan {
    /// Runtime-side preflight before recording or invoking a body. Consumes the
    /// plan even if later admission fails; re-inspection is explicit and safe.
    pub fn take_operation(
        &self,
        session: &SessionId,
        task: &TaskId,
        name: &str,
        arguments: &Value,
        contract: &ToolEffectContract,
    ) -> Result<ToolOperation, ToolRetryError> {
        let op = self
            .source
            .operation
            .as_deref()
            .ok_or(ToolRetryError::LegacyCall)?;
        if &self.session != session
            || &op.task_id != task
            || self.source.name != name
            || &self.source.arguments != arguments
            || &op.contract != contract
        {
            return Err(ToolRetryError::BindingChanged);
        }
        self.consumed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| ToolRetryError::Consumed)?;
        let mut op = op.clone();
        op.retry = Some(ToolRetryAudit {
            call_id: self.source.id.clone(),
            event_id: self.source_event.clone(),
            review: self.review.clone(),
        });
        Ok(op)
    }
}
use serde_json::Value;
/// Prepare one explicitly requested attempt from independently confirmed history.
/// Read-only/idempotent uncertainty permits an attempt; Unknown/NonIdempotent
/// requires trusted evidence of non-execution. Completed evidence always wins.
pub fn prepare_tool_retry(
    history: &[SessionEvent],
    call_id: &str,
    review: Option<ToolRetryReview>,
) -> Result<ToolRetryPlan, ToolRetryError> {
    if let Some(review) = &review {
        validate_review(review)?;
    }
    let assessment = inspect_tool_recovery(history, call_id)?;
    if assessment.status == ToolRecoveryStatus::Completed
        || review
            .as_ref()
            .is_some_and(|r| r.outcome == ToolReviewOutcome::Completed)
    {
        return Err(ToolRetryError::Completed);
    }
    let op = assessment
        .call
        .operation
        .as_ref()
        .ok_or(ToolRetryError::LegacyCall)?;
    if assessment.status != ToolRecoveryStatus::NotExecuted
        && !review
            .as_ref()
            .is_some_and(|r| r.outcome == ToolReviewOutcome::NotExecuted)
        && !matches!(
            op.contract.effect,
            ToolEffect::ReadOnly | ToolEffect::Idempotent
        )
    {
        return Err(ToolRetryError::NeedsReview);
    }
    let (_, _, event) = evidence(history, call_id)?;
    Ok(ToolRetryPlan {
        source: assessment.call,
        session: event.session_id.clone(),
        source_event: event.event_id.clone(),
        review,
        consumed: Arc::new(AtomicBool::new(false)),
    })
}

struct HistoryLimit(usize);
impl std::io::Write for HistoryLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("history limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
