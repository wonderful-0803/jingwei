//! Pure, deterministic projections of a complete physical Session snapshot.
//! No IO, model calls, log mutation, tool authorization or token budget is implicit.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

use jingwei_core::event::ToolCall;
use jingwei_core::{
    DoneStatus, EventId, ModelCallId, ModelMessage, ModelToolCall, ProviderToolCallId,
    SessionEvent, SessionEventKind, SessionId, TurnId,
};
use jingwei_session::{PhysicalLogError, validate_physical_log};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum LegacyReplyPolicy {
    #[default]
    Reject,
    /// Explicitly omit unknown legacy replies; never infer completeness from deltas.
    Omit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectionConfig {
    pub legacy_replies: LegacyReplyPolicy,
    pub max_events: usize,
    /// Bound on canonical serialized input, including metadata, not a token count.
    pub max_input_bytes: usize,
    pub max_messages: usize,
}
impl Default for ProjectionConfig {
    fn default() -> Self {
        Self {
            legacy_replies: LegacyReplyPolicy::Reject,
            max_events: 100_000,
            max_input_bytes: 16 * 1024 * 1024,
            max_messages: 50_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProjectedTurnState {
    Completed,
    WaitingForInput,
    Failed,
    Cancelled,
    Open,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProjectionGroupKind {
    User,
    Assistant,
    ToolExchange,
}

/// A future context selector must retain a tool exchange as an indivisible group.
/// Messages remain user/model/tool data; this projector never creates system text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectionGroup {
    pub turn_id: TurnId,
    pub kind: ProjectionGroupKind,
    pub source_events: Vec<EventId>,
    pub messages: Vec<ModelMessage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OmissionReason {
    Audit,
    ModelAudit,
    StreamDelta,
    UnsuccessfulReply,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OmittedEvent {
    pub event_id: EventId,
    pub seq: u64,
    pub reason: OmissionReason,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProjectedTurn {
    pub turn_id: TurnId,
    pub state: ProjectedTurnState,
    pub terminal_event: Option<EventId>,
    /// A successful old turn without AssistantMessage; only explicit Omit allows it.
    pub missing_complete_reply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationProjection {
    /// Canonical projection algorithm version; not a Session wire version.
    pub algorithm_version: u16,
    pub session_id: SessionId,
    pub source_tail: Option<EventId>,
    pub source_event_count: usize,
    pub config: ProjectionConfig,
    pub groups: Vec<ProjectionGroup>,
    pub turns: Vec<ProjectedTurn>,
    pub omitted: Vec<OmittedEvent>,
}
impl ConversationProjection {
    pub fn messages(&self) -> impl Iterator<Item = &ModelMessage> {
        self.groups.iter().flat_map(|group| &group.messages)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error(transparent)]
    Physical(#[from] PhysicalLogError),
    #[error("invalid projection limits or empty Session identity")]
    InvalidConfig,
    #[error("projection limit exceeded: {0}")]
    Limit(&'static str),
    #[error("event {event_id}: {reason}")]
    Malformed {
        event_id: EventId,
        reason: &'static str,
    },
    #[error("turn {0} has unconfirmed work; projection cannot discard it")]
    Unconfirmed(TurnId),
    #[error("turn {0} has no canonical complete reply; select an explicit legacy policy")]
    MissingReply(TurnId),
}

pub trait ConversationProjector: Send + Sync {
    fn project(
        &self,
        session: &SessionId,
        events: &[SessionEvent],
        config: ProjectionConfig,
    ) -> Result<ConversationProjection, ProjectionError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalConversationProjector;

impl ConversationProjector for CanonicalConversationProjector {
    fn project(
        &self,
        session: &SessionId,
        events: &[SessionEvent],
        config: ProjectionConfig,
    ) -> Result<ConversationProjection, ProjectionError> {
        if session.as_str().trim().is_empty()
            || config.max_events == 0
            || config.max_input_bytes == 0
            || config.max_messages == 0
        {
            return Err(ProjectionError::InvalidConfig);
        }
        if events.len() > config.max_events {
            return Err(ProjectionError::Limit("events"));
        }
        let mut counter = CountBytes {
            remaining: config.max_input_bytes,
        };
        for event in events {
            serde_json::to_writer(&mut counter, event)
                .map_err(|_| ProjectionError::Limit("input bytes"))?;
        }
        validate_physical_log(session, events)?;
        let mut projection = ConversationProjection {
            algorithm_version: 1,
            session_id: session.clone(),
            source_tail: events.last().map(|e| e.event_id.clone()),
            source_event_count: events.len(),
            config,
            groups: vec![],
            turns: vec![],
            omitted: vec![],
        };
        let mut seen = HashSet::new();
        let mut message_ids = HashSet::new();
        let mut start = 0;
        let mut message_count = 0;
        while start < events.len() {
            let turn = &events[start].turn_id;
            if turn.as_str().trim().is_empty() || !seen.insert(turn.clone()) {
                return Err(malformed(
                    &events[start],
                    "empty or noncontiguous Turn identity",
                ));
            }
            let end = start
                + events[start..]
                    .iter()
                    .take_while(|e| &e.turn_id == turn)
                    .count();
            project_turn(
                &events[start..end],
                &mut projection,
                &mut message_ids,
                &mut message_count,
            )?;
            if end < events.len()
                && projection.turns.last().unwrap().state == ProjectedTurnState::Open
            {
                return Err(ProjectionError::Unconfirmed(turn.clone()));
            }
            start = end;
        }
        projection.omitted.sort_by_key(|event| event.seq);
        Ok(projection)
    }
}

fn malformed(event: &SessionEvent, reason: &'static str) -> ProjectionError {
    ProjectionError::Malformed {
        event_id: event.event_id.clone(),
        reason,
    }
}

fn project_turn(
    events: &[SessionEvent],
    out: &mut ConversationProjection,
    message_ids: &mut HashSet<jingwei_core::MessageId>,
    count: &mut usize,
) -> Result<(), ProjectionError> {
    let turn = events[0].turn_id.clone();
    let mut user = None;
    let mut complete = None;
    let mut terminal = None;
    let mut state = ProjectedTurnState::Open;
    let mut calls: Vec<(&SessionEvent, &ToolCall, Option<&SessionEvent>)> = vec![];
    let mut call_index = HashMap::new();
    let mut model_calls: HashMap<&ModelCallId, bool> = HashMap::new();
    for event in events {
        if event.event_id.as_str().trim().is_empty() {
            return Err(malformed(event, "empty event ID"));
        }
        if terminal.is_some() {
            return Err(malformed(event, "event follows terminal"));
        }
        if complete.is_some()
            && matches!(
                event.kind,
                SessionEventKind::UserMessage { .. }
                    | SessionEventKind::AssistantDelta { .. }
                    | SessionEventKind::ModelRequest { .. }
                    | SessionEventKind::ModelResult { .. }
                    | SessionEventKind::ToolCall { .. }
                    | SessionEventKind::ToolResult { .. }
            )
        {
            return Err(malformed(event, "conversation work follows complete reply"));
        }
        let mut omitted = None;
        match &event.kind {
            SessionEventKind::UserMessage { .. } => {
                if user.replace(event).is_some() {
                    return Err(malformed(event, "multiple user messages in a Turn"));
                }
            }
            SessionEventKind::AssistantMessage { .. } => {
                let Some(id) = &event.message_id else {
                    return Err(malformed(event, "complete reply lacks MessageId"));
                };
                if id.as_str().trim().is_empty()
                    || !message_ids.insert(id.clone())
                    || complete.replace(event).is_some()
                {
                    return Err(malformed(event, "duplicate complete reply or MessageId"));
                }
            }
            SessionEventKind::AssistantDelta { .. } => omitted = Some(OmissionReason::StreamDelta),
            SessionEventKind::ToolCall { call } => {
                if call.id.trim().is_empty()
                    || call.name.trim().is_empty()
                    || call_index.insert(call.id.as_str(), calls.len()).is_some()
                {
                    return Err(malformed(event, "empty or duplicate tool call identity"));
                }
                calls.push((event, call, None));
            }
            SessionEventKind::ToolResult { result } => {
                let Some(&index) = call_index.get(result.call_id.as_str()) else {
                    return Err(malformed(event, "orphan or cross-Turn tool result"));
                };
                if calls[index].2.replace(event).is_some() {
                    return Err(malformed(event, "duplicate tool result"));
                }
            }
            SessionEventKind::ModelRequest { request } => {
                if request.call_id.as_str().trim().is_empty()
                    || model_calls.insert(&request.call_id, false).is_some()
                {
                    return Err(malformed(event, "empty or duplicate model call"));
                }
                omitted = Some(OmissionReason::ModelAudit);
            }
            SessionEventKind::ModelResult { result } => {
                let Some(done) = model_calls.get_mut(&result.call_id) else {
                    return Err(malformed(event, "orphan or cross-Turn model result"));
                };
                if *done {
                    return Err(malformed(event, "duplicate model result"));
                }
                *done = true;
                // Candidate calls (including refused/invalid proposals) are audit,
                // not execution. Only canonical ToolCall/ToolResult form exchanges.
                omitted = Some(OmissionReason::ModelAudit);
            }
            SessionEventKind::Done { status, .. } => {
                state = match status {
                    DoneStatus::Completed => ProjectedTurnState::Completed,
                    DoneStatus::WaitingForInput => ProjectedTurnState::WaitingForInput,
                    DoneStatus::Cancelled => ProjectedTurnState::Cancelled,
                };
                terminal = Some(event.event_id.clone());
                omitted = Some(OmissionReason::Audit);
            }
            SessionEventKind::Error { .. } => {
                state = ProjectedTurnState::Failed;
                terminal = Some(event.event_id.clone());
                omitted = Some(OmissionReason::Audit);
            }
            _ => omitted = Some(OmissionReason::Audit),
        }
        if user.is_none()
            && matches!(
                event.kind,
                SessionEventKind::AssistantMessage { .. }
                    | SessionEventKind::AssistantDelta { .. }
                    | SessionEventKind::ToolCall { .. }
                    | SessionEventKind::ModelRequest { .. }
            )
        {
            return Err(malformed(event, "conversation work precedes user message"));
        }
        if let Some(reason) = omitted {
            out.omitted.push(OmittedEvent {
                event_id: event.event_id.clone(),
                seq: event.seq,
                reason,
            });
        }
    }
    if calls.iter().any(|(_, _, result)| result.is_none())
        || model_calls.values().any(|done| !done)
        || (state == ProjectedTurnState::Open
            && events
                .iter()
                .any(|e| !matches!(e.kind, SessionEventKind::UserMessage { .. })))
    {
        return Err(ProjectionError::Unconfirmed(turn));
    }
    let successful = matches!(
        state,
        ProjectedTurnState::Completed | ProjectedTurnState::WaitingForInput
    );
    let missing = successful && complete.is_none();
    if missing && out.config.legacy_replies == LegacyReplyPolicy::Reject {
        return Err(ProjectionError::MissingReply(turn));
    }
    if let Some(event) = user {
        let SessionEventKind::UserMessage { text } = &event.kind else {
            unreachable!()
        };
        push(
            out,
            count,
            ProjectionGroup {
                turn_id: turn.clone(),
                kind: ProjectionGroupKind::User,
                source_events: vec![event.event_id.clone()],
                messages: vec![ModelMessage::user(text)],
            },
        )?;
    }
    // Stable call order, even when parallel results completed in reverse order.
    for (event, call, result) in calls {
        let result = result.unwrap();
        let SessionEventKind::ToolResult { result: value } = &result.kind else {
            unreachable!()
        };
        let id = ProviderToolCallId::new(format!("jw_tool_{}", event.seq))
            .expect("generated ID is nonempty");
        push(
            out,
            count,
            ProjectionGroup {
                turn_id: turn.clone(),
                kind: ProjectionGroupKind::ToolExchange,
                source_events: vec![event.event_id.clone(), result.event_id.clone()],
                messages: vec![
                    ModelMessage::Assistant {
                        content: None,
                        tool_calls: vec![ModelToolCall {
                            id: id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        }],
                    },
                    ModelMessage::Tool {
                        call_id: id,
                        content: serde_json::to_string(&value.outcome)
                            .expect("tool outcome is serializable"),
                    },
                ],
            },
        )?;
    }
    if let Some(event) = complete {
        if successful {
            let SessionEventKind::AssistantMessage { text, .. } = &event.kind else {
                unreachable!()
            };
            push(
                out,
                count,
                ProjectionGroup {
                    turn_id: turn.clone(),
                    kind: ProjectionGroupKind::Assistant,
                    source_events: vec![event.event_id.clone()],
                    messages: vec![ModelMessage::assistant(text)],
                },
            )?;
        } else {
            out.omitted.push(OmittedEvent {
                event_id: event.event_id.clone(),
                seq: event.seq,
                reason: OmissionReason::UnsuccessfulReply,
            });
        }
    }
    out.turns.push(ProjectedTurn {
        turn_id: turn,
        state,
        terminal_event: terminal,
        missing_complete_reply: missing,
    });
    Ok(())
}

fn push(
    out: &mut ConversationProjection,
    count: &mut usize,
    group: ProjectionGroup,
) -> Result<(), ProjectionError> {
    *count = count
        .checked_add(group.messages.len())
        .ok_or(ProjectionError::Limit("messages"))?;
    if *count > out.config.max_messages {
        return Err(ProjectionError::Limit("messages"));
    }
    out.groups.push(group);
    Ok(())
}

struct CountBytes {
    remaining: usize,
}
impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("input limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
