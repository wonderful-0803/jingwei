//! Bounded whole-turn selection. Counters are trusted host policy, not model input.
use std::collections::HashSet;

pub use jingwei_core::budget::{TokenBoundEvidence, TokenBudgetMode};
use jingwei_core::{
    EventId, GenerationConstraint, GenerationRequest, ModelMessage, ModelProtocolError, SessionId,
    TurnId,
};
use serde::{Deserialize, Serialize};

use crate::{ConversationProjection, ProjectedTurnState, ProjectionGroupKind};

/// Host-selected model and rendering/tokenizer revision. Neither may be blank.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextTarget {
    pub model: String,
    pub template_revision: String,
}

/// Disjoint contributions; template includes role delimiters and other framing.
/// A verified sum must bound the complete provider-rendered input, including
/// schema serialization. Exact counts are also verified upper bounds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextTokenBreakdown {
    pub system: u64,
    pub messages: u64,
    pub constraint: u64,
    pub template: u64,
}
impl ContextTokenBreakdown {
    pub fn total(self) -> Option<u64> {
        self.system
            .checked_add(self.messages)?
            .checked_add(self.constraint)?
            .checked_add(self.template)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextTokenCount {
    pub target: ContextTarget,
    /// Stable method/tokenizer version, supplied by the trusted implementation.
    pub method: String,
    pub evidence: TokenBoundEvidence,
    pub tokens: ContextTokenBreakdown,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("context counter failed: {0}")]
pub struct ContextCountError(pub String);

/// Synchronous, nonblocking host policy. Counts must be deterministic for a fixed
/// target and request. Verified evidence is a host assertion, not auto-verification.
/// The builder recounts whole requests and does not assume additive deletion costs.
pub trait ContextTokenCounter: Send + Sync {
    fn count(
        &self,
        target: &ContextTarget,
        request: &GenerationRequest,
    ) -> Result<ContextTokenCount, ContextCountError>;
}

/// UTF-8 serialized bytes / 4, rounded up per item, plus configured framing.
/// Deliberately only an estimate, including for CJK and unusual tokenizers.
#[derive(Clone, Copy, Debug)]
pub struct ByteHeuristicCounter {
    pub template_tokens: u64,
}
impl Default for ByteHeuristicCounter {
    fn default() -> Self {
        Self {
            template_tokens: 32,
        }
    }
}
impl ContextTokenCounter for ByteHeuristicCounter {
    fn count(
        &self,
        target: &ContextTarget,
        request: &GenerationRequest,
    ) -> Result<ContextTokenCount, ContextCountError> {
        let mut tokens = ContextTokenBreakdown {
            template: self.template_tokens,
            ..Default::default()
        };
        for message in &request.messages {
            let count = heuristic(message)?;
            let slot = if matches!(message, ModelMessage::System { .. }) {
                &mut tokens.system
            } else {
                &mut tokens.messages
            };
            *slot = slot
                .checked_add(count)
                .ok_or_else(|| ContextCountError("token overflow".into()))?;
        }
        tokens.constraint = heuristic(&request.constraint)?;
        Ok(ContextTokenCount {
            target: target.clone(),
            method: "serialized-utf8-ceil-div4-v1".into(),
            evidence: TokenBoundEvidence::Estimate,
            tokens,
        })
    }
}
fn heuristic(value: &impl Serialize) -> Result<u64, ContextCountError> {
    let mut sink = crate::CountBytes {
        remaining: usize::MAX,
    };
    serde_json::to_writer(&mut sink, value)
        .map_err(|_| ContextCountError("serialization limit".into()))?;
    Ok(((usize::MAX - sink.remaining) as u64).div_ceil(4))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub window_tokens: u64,
    pub output_reserve: u64,
    /// Hard mode requires the host to enforce this bound on actual generation.
    pub output_evidence: TokenBoundEvidence,
    pub safety_margin: u64,
    pub mode: TokenBudgetMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextBuildLimits {
    /// Serialized input including source metadata; not a process memory limit.
    pub max_input_bytes: usize,
    pub max_groups: usize,
    pub max_messages: usize,
    /// Each attempt counts the complete candidate. Bounds repeated selection work.
    pub max_count_calls: usize,
}
impl Default for ContextBuildLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_groups: 4096,
            max_messages: 8192,
            max_count_calls: 128,
        }
    }
}

/// Completed historical projection and a separate, mandatory current conversation.
/// Current messages must start with User, contain no System, and have all tool
/// results confirmed. An unfinished tool group is rejected before any trimming.
pub struct ContextBuildInput<'a> {
    pub history: &'a ConversationProjection,
    pub system: &'a [String],
    pub current: &'a [ModelMessage],
    pub constraint: &'a GenerationConstraint,
    pub pinned_turns: &'a [TurnId],
    /// Opaque host state revision for provenance; no task state is inferred.
    pub state_version: Option<&'a str>,
    pub target: &'a ContextTarget,
    pub budget: ContextBudget,
    pub limits: ContextBuildLimits,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextTurnSelection {
    pub turn_id: TurnId,
    pub source_events: Vec<EventId>,
    pub retained: bool,
    pub protected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextCountAttempt {
    pub removed_turns: usize,
    pub count: ContextTokenCount,
    /// Complete input + reserved output + safety margin, checked for overflow.
    pub total_reserved: u64,
}

/// A serializable derived report for the host to save. No implicit journal IO.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextBuildReport {
    pub algorithm_version: u16,
    pub projection_version: u16,
    pub session_id: SessionId,
    pub source_tail: Option<EventId>,
    pub source_event_count: usize,
    pub state_version: Option<String>,
    pub target: ContextTarget,
    pub budget: ContextBudget,
    pub limits: ContextBuildLimits,
    pub system_messages: usize,
    pub current_messages: usize,
    pub turns: Vec<ContextTurnSelection>,
    pub attempts: Vec<ContextCountAttempt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuiltContext {
    pub request: GenerationRequest,
    pub report: ContextBuildReport,
}

#[derive(Debug, thiserror::Error)]
pub enum ContextBuildError {
    #[error("invalid context configuration: {0}")]
    Invalid(&'static str),
    #[error("context resource limit: {0}")]
    Limit(&'static str),
    #[error("invalid context message shape: {0}")]
    Shape(#[from] ModelProtocolError),
    #[error(transparent)]
    Counter(#[from] ContextCountError),
    #[error("counter evidence is unverified or bound to another target")]
    UnverifiedCount,
    #[error("context token arithmetic overflow")]
    Overflow,
    #[error("minimum necessary context exceeds the budget")]
    DoesNotFit(Box<ContextBuildReport>),
    #[error("context counting attempt limit reached")]
    CountLimit(Box<ContextBuildReport>),
}

pub trait ContextBuilder: Send + Sync {
    fn build(
        &self,
        input: ContextBuildInput<'_>,
        counter: &dyn ContextTokenCounter,
    ) -> Result<BuiltContext, ContextBuildError>;
}

/// Drops oldest unpinned historical turns. Never edits text, schemas or roles.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalContextBuilder;
impl ContextBuilder for CanonicalContextBuilder {
    fn build(
        &self,
        input: ContextBuildInput<'_>,
        counter: &dyn ContextTokenCounter,
    ) -> Result<BuiltContext, ContextBuildError> {
        validate_input(&input)?;
        let mut report = ContextBuildReport {
            algorithm_version: 1,
            projection_version: input.history.algorithm_version,
            session_id: input.history.session_id.clone(),
            source_tail: input.history.source_tail.clone(),
            source_event_count: input.history.source_event_count,
            state_version: input.state_version.map(str::to_owned),
            target: input.target.clone(),
            budget: input.budget,
            limits: input.limits,
            system_messages: input.system.len(),
            current_messages: input.current.len(),
            turns: input
                .history
                .turns
                .iter()
                .map(|turn| ContextTurnSelection {
                    turn_id: turn.turn_id.clone(),
                    source_events: input
                        .history
                        .groups
                        .iter()
                        .filter(|group| group.turn_id == turn.turn_id)
                        .flat_map(|group| group.source_events.clone())
                        .collect(),
                    retained: true,
                    protected: input.pinned_turns.contains(&turn.turn_id),
                })
                .collect(),
            attempts: vec![],
        };
        let reserve = input
            .budget
            .output_reserve
            .checked_add(input.budget.safety_margin)
            .ok_or(ContextBuildError::Overflow)?;
        // Build and validate the full request before any deletion. Invalid history
        // must not disappear as a side effect of a tight budget.
        let mut request = assemble(&input, &report);
        request.validate_shape()?;
        let mut removed = 0;
        loop {
            let count = counter.count(input.target, &request)?;
            if count.target != *input.target
                || count.method.trim().is_empty()
                || (input.budget.mode == TokenBudgetMode::Hard
                    && count.evidence != TokenBoundEvidence::VerifiedUpperBound)
            {
                return Err(ContextBuildError::UnverifiedCount);
            }
            let input_tokens = count.tokens.total().ok_or(ContextBuildError::Overflow)?;
            if input_tokens == 0 {
                return Err(ContextBuildError::Invalid("zero input count"));
            }
            let total_reserved = input_tokens
                .checked_add(reserve)
                .ok_or(ContextBuildError::Overflow)?;
            report.attempts.push(ContextCountAttempt {
                removed_turns: removed,
                count,
                total_reserved,
            });
            if total_reserved <= input.budget.window_tokens {
                return Ok(BuiltContext { request, report });
            }
            let Some(index) = report
                .turns
                .iter()
                .position(|turn| turn.retained && !turn.protected)
            else {
                return Err(ContextBuildError::DoesNotFit(Box::new(report)));
            };
            if report.attempts.len() == input.limits.max_count_calls {
                return Err(ContextBuildError::CountLimit(Box::new(report)));
            }
            report.turns[index].retained = false;
            removed += 1;
            request = assemble(&input, &report);
            request.validate_shape()?;
        }
    }
}

fn assemble(input: &ContextBuildInput<'_>, report: &ContextBuildReport) -> GenerationRequest {
    let retained: HashSet<_> = report
        .turns
        .iter()
        .filter(|turn| turn.retained)
        .map(|turn| &turn.turn_id)
        .collect();
    let messages = input
        .system
        .iter()
        .map(ModelMessage::system)
        .chain(
            input
                .history
                .groups
                .iter()
                .filter(|group| retained.contains(&group.turn_id))
                .flat_map(|group| group.messages.iter().cloned()),
        )
        .chain(input.current.iter().cloned())
        .collect();
    GenerationRequest {
        messages,
        constraint: input.constraint.clone(),
    }
}

fn validate_input(input: &ContextBuildInput<'_>) -> Result<(), ContextBuildError> {
    let limits = input.limits;
    if limits.max_input_bytes == 0
        || limits.max_groups == 0
        || limits.max_messages == 0
        || limits.max_count_calls == 0
        || input.budget.window_tokens == 0
        || input.budget.output_reserve == 0
        || input.target.model.trim().is_empty()
        || input.target.template_revision.trim().is_empty()
    {
        return Err(ContextBuildError::Invalid("zero limit or blank target"));
    }
    if input.budget.mode == TokenBudgetMode::Hard
        && input.budget.output_evidence != TokenBoundEvidence::VerifiedUpperBound
    {
        return Err(ContextBuildError::UnverifiedCount);
    }
    if input.history.groups.len() > limits.max_groups
        || input.history.turns.len() > limits.max_groups
    {
        return Err(ContextBuildError::Limit("groups"));
    }
    let mut sink = crate::CountBytes {
        remaining: limits.max_input_bytes,
    };
    serde_json::to_writer(
        &mut sink,
        &(
            input.history,
            input.system,
            input.current,
            input.constraint,
            input.pinned_turns,
            input.state_version,
            input.target,
        ),
    )
    .map_err(|_| ContextBuildError::Limit("input bytes"))?;
    let message_count = input.history.groups.iter().try_fold(
        input
            .system
            .len()
            .checked_add(input.current.len())
            .ok_or(ContextBuildError::Overflow)?,
        |sum, group| {
            sum.checked_add(group.messages.len())
                .ok_or(ContextBuildError::Overflow)
        },
    )?;
    if message_count > limits.max_messages {
        return Err(ContextBuildError::Limit("messages"));
    }
    if input.history.algorithm_version != 1 {
        return Err(ContextBuildError::Invalid("unsupported projection version"));
    }
    if !matches!(input.current.first(), Some(ModelMessage::User { .. }))
        || input
            .current
            .iter()
            .any(|m| matches!(m, ModelMessage::System { .. }))
    {
        return Err(ContextBuildError::Invalid(
            "current context must start with user and contain no system",
        ));
    }
    GenerationRequest::text(input.current.to_vec()).validate_shape()?;
    let mut turns = HashSet::new();
    for turn in &input.history.turns {
        if turn.turn_id.as_str().trim().is_empty()
            || !turns.insert(&turn.turn_id)
            || turn.state == ProjectedTurnState::Open
        {
            return Err(ContextBuildError::Invalid(
                "duplicate, blank or open historical turn",
            ));
        }
    }
    let pins: HashSet<_> = input.pinned_turns.iter().collect();
    if pins.len() != input.pinned_turns.len() || pins.iter().any(|pin| !turns.contains(pin)) {
        return Err(ContextBuildError::Invalid(
            "duplicate or unknown pinned turn",
        ));
    }
    let mut last_index = 0;
    for group in &input.history.groups {
        let Some(index) = input
            .history
            .turns
            .iter()
            .position(|turn| turn.turn_id == group.turn_id)
        else {
            return Err(ContextBuildError::Invalid("unknown group turn"));
        };
        if index < last_index || group.source_events.is_empty() {
            return Err(ContextBuildError::Invalid("unordered or unsourced group"));
        }
        last_index = index;
        let valid = match (group.kind, group.messages.as_slice()) {
            (ProjectionGroupKind::User, [ModelMessage::User { .. }]) => true,
            (
                ProjectionGroupKind::Assistant,
                [
                    ModelMessage::Assistant {
                        content: Some(_),
                        tool_calls,
                    },
                ],
            ) => tool_calls.is_empty(),
            (
                ProjectionGroupKind::ToolExchange,
                [
                    ModelMessage::Assistant { tool_calls, .. },
                    ModelMessage::Tool { call_id, .. },
                ],
            ) => tool_calls.len() == 1 && tool_calls[0].id == *call_id,
            _ => false,
        };
        if !valid {
            return Err(ContextBuildError::Invalid("invalid projection group shape"));
        }
    }
    Ok(())
}
