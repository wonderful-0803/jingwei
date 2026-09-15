use crate::{MAX_TASK_HISTORY_EVENTS, MAX_TASK_STEPS, TaskIdentity, TaskStateError, valid_text};
use jingwei_budget::{BudgetEventCounts, BudgetEventCursor};
use jingwei_core::{SessionEvent, SessionEventKind, StepId, TurnId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TaskOperationKind {
    Model,
    Tool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedTaskOperation {
    pub kind: TaskOperationKind,
    pub call_id: String,
    pub intent: BudgetEventCursor,
    pub step_id: Option<StepId>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskHistoryBoundary {
    Empty,
    Closed,
    Open { turn_id: TurnId },
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskHistoryAssessment {
    pub boundary: TaskHistoryBoundary,
    pub settled_steps: Vec<StepId>,
    /// Missing results mean unknown outcome, never permission to re-execute.
    pub unresolved: Vec<UnresolvedTaskOperation>,
}
/// Read-only assessment of the full, independently loaded Session history. It
/// neither recovers a Task nor invokes models/tools. Incomplete turns are retained.
pub fn assess_task_history(
    identity: &TaskIdentity,
    history: &[SessionEvent],
) -> Result<TaskHistoryAssessment, TaskStateError> {
    if history.len() > MAX_TASK_HISTORY_EVENTS {
        return Err(TaskStateError::Limit);
    }
    valid_text(identity.task_id.as_str())?;
    valid_text(identity.session_id.as_str())?;
    valid_text(&identity.agent_key)?;
    let mut event_ids = BTreeSet::new();
    let mut turn_ids = BTreeSet::new();
    let mut current_turn = None;
    let mut closed = true;
    let mut models = BTreeMap::<String, UnresolvedTaskOperation>::new();
    let mut tools = BTreeMap::<String, UnresolvedTaskOperation>::new();
    let mut calls_seen = BTreeSet::new();
    let mut steps_seen = BTreeSet::new();
    let mut steps = vec![];
    let mut counts = BudgetEventCounts::default();
    let mut report_seen = false;
    for (index, event) in history.iter().enumerate() {
        if event.session_id != identity.session_id {
            return Err(TaskStateError::IdentityMismatch);
        }
        valid_text(event.event_id.as_str())?;
        valid_text(event.turn_id.as_str())?;
        if event.seq != index as u64 || !event_ids.insert(&event.event_id) {
            return Err(TaskStateError::Invalid("physical event address"));
        }
        if current_turn != Some(&event.turn_id) {
            if !closed || !turn_ids.insert(&event.turn_id) {
                return Err(TaskStateError::Invalid("interleaved or reopened turn"));
            }
            if !matches!(event.kind, SessionEventKind::UserMessage { .. }) {
                return Err(TaskStateError::Invalid("turn lacks user envelope"));
            }
            current_turn = Some(&event.turn_id);
            closed = false;
            counts = BudgetEventCounts::default();
            report_seen = false;
        } else if matches!(event.kind, SessionEventKind::UserMessage { .. }) {
            return Err(TaskStateError::Invalid("duplicate user envelope"));
        } else if closed {
            return Err(TaskStateError::Invalid("event after terminal"));
        }
        if report_seen
            && !matches!(
                event.kind,
                SessionEventKind::Done { .. } | SessionEventKind::Error { .. }
            )
        {
            return Err(TaskStateError::Invalid("event after final report"));
        }
        let pending = match &event.kind {
            SessionEventKind::ModelRequest { request } => {
                counts.model_requests += 1;
                let step = request
                    .options
                    .context
                    .as_ref()
                    .filter(|c| c.task_id == identity.task_id)
                    .map(|c| c.step_id.clone());
                Some((TaskOperationKind::Model, request.call_id.to_string(), step))
            }
            SessionEventKind::ToolCall { call } => {
                counts.tool_calls += 1;
                let step = call
                    .action
                    .as_ref()
                    .filter(|c| c.decision.task_id == identity.task_id)
                    .map(|c| c.decision.step_id.clone());
                Some((TaskOperationKind::Tool, call.id.clone(), step))
            }
            SessionEventKind::ModelResult { result } => {
                finish(&mut models, result.call_id.as_str(), &event.turn_id)?;
                counts.model_results += 1;
                None
            }
            SessionEventKind::ToolResult { result } => {
                finish(&mut tools, &result.call_id, &event.turn_id)?;
                counts.tool_results += 1;
                None
            }
            SessionEventKind::TaskRunReport { report } => {
                report_seen = true;
                if report.budget.identity.session_id != identity.session_id {
                    return Err(TaskStateError::IdentityMismatch);
                }
                if report.budget.run.as_ref().is_none_or(|r| {
                    r.turn_id.as_ref() != Some(&event.turn_id) || r.metrics.confirmed != counts
                }) {
                    return Err(TaskStateError::Invalid("report counts or turn mismatch"));
                }
                None
            }
            SessionEventKind::Done { .. } | SessionEventKind::Error { .. } => {
                closed = true;
                None
            }
            _ => None,
        };
        if let Some((kind, call_id, step_id)) = pending {
            valid_text(&call_id)?;
            if !calls_seen.insert((matches!(kind, TaskOperationKind::Tool), call_id.clone())) {
                return Err(TaskStateError::Invalid("duplicate call ID"));
            }
            if let Some(step) = &step_id {
                valid_text(step.as_str())?;
                if steps_seen.insert(step.clone()) {
                    steps.push(step.clone());
                }
                if steps.len() > MAX_TASK_STEPS {
                    return Err(TaskStateError::Limit);
                }
            }
            let operation = UnresolvedTaskOperation {
                kind,
                call_id: call_id.clone(),
                intent: BudgetEventCursor::from(event),
                step_id,
            };
            match kind {
                TaskOperationKind::Model => {
                    models.insert(call_id, operation);
                }
                TaskOperationKind::Tool => {
                    tools.insert(call_id, operation);
                }
            }
        }
    }
    let mut unresolved: Vec<_> = models.into_values().chain(tools.into_values()).collect();
    unresolved.sort_by_key(|p| p.intent.seq);
    let pending_steps: BTreeSet<_> = unresolved
        .iter()
        .filter_map(|p| p.step_id.as_ref())
        .collect();
    steps.retain(|step| !pending_steps.contains(step));
    Ok(TaskHistoryAssessment {
        boundary: match current_turn {
            None => TaskHistoryBoundary::Empty,
            Some(_) if closed => TaskHistoryBoundary::Closed,
            Some(id) => TaskHistoryBoundary::Open {
                turn_id: id.clone(),
            },
        },
        settled_steps: steps,
        unresolved,
    })
}
fn finish(
    pending: &mut BTreeMap<String, UnresolvedTaskOperation>,
    id: &str,
    turn: &TurnId,
) -> Result<(), TaskStateError> {
    let operation = pending
        .remove(id)
        .ok_or(TaskStateError::Invalid("unmatched or repeated result"))?;
    if &operation.intent.turn_id != turn {
        return Err(TaskStateError::Invalid("result crossed turn"));
    }
    Ok(())
}
