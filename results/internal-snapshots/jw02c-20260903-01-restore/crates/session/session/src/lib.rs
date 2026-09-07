//! Public contracts for Jingwei's canonical Session event log.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use jingwei_core::{
    CapabilityId, EventId, GenerationId, MessageId, SessionEvent, SessionEventKind, SessionId,
    TurnId,
};

/// A boxed asynchronous Session operation.
pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The singular capability implemented by Session persistence providers.
pub const SESSION_PERSISTENCE: CapabilityId = CapabilityId::new("jingwei.session.persistence");

/// The singular capability implemented by Session runtime providers.
pub const SESSION_RUNTIME: CapabilityId = CapabilityId::new("jingwei.session.runtime");

/// Whether an I/O failure is known to have left the durable log unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitCertainty {
    DefinitelyNotCommitted,
    Indeterminate,
}

/// A durable append result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistAppendOutcome {
    Appended,
    ReplayedExact,
}

/// A structured persistence-provider failure.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionPersistenceError {
    #[error("persistence I/O operation {operation} failed ({certainty:?}): {message}")]
    Io {
        operation: &'static str,
        message: String,
        certainty: CommitCertainty,
    },
    #[error("event serialization failed: {message}")]
    Serialization { message: String },
    #[error("physical Session log is corrupt at index {physical_index}: {message}")]
    CorruptLog {
        physical_index: usize,
        message: String,
    },
    #[error(transparent)]
    InvalidHistory(#[from] PhysicalLogError),
    #[error("durable event {event_id} conflicts at sequence {seq}: {message}")]
    Conflict {
        seq: u64,
        event_id: EventId,
        message: String,
    },
}

/// The durable storage seam behind a [`SessionRuntime`].
pub trait SessionPersistence: Send + Sync {
    /// Load events in their physical/native order. Implementations must not sort the result.
    fn load<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Vec<SessionEvent>, SessionPersistenceError>>;

    /// Commit one expected-next event and return only after the provider's durability barrier.
    fn commit_durable<'a>(
        &'a self,
        event: &'a SessionEvent,
    ) -> SessionFuture<'a, Result<PersistAppendOutcome, SessionPersistenceError>>;
}

/// One proposed event. Its stable ID supports exact retry, while canonical address fields are
/// deliberately absent.
#[derive(Clone, Debug)]
pub struct SessionEventDraft {
    event_id: EventId,
    generation_id: Option<GenerationId>,
    message_id: Option<MessageId>,
    kind: SessionEventKind,
}

impl SessionEventDraft {
    pub fn new(kind: SessionEventKind) -> Self {
        Self {
            event_id: EventId::new(),
            generation_id: None,
            message_id: None,
            kind,
        }
    }

    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    pub fn generation_id(&self) -> Option<&GenerationId> {
        self.generation_id.as_ref()
    }

    pub fn message_id(&self) -> Option<&MessageId> {
        self.message_id.as_ref()
    }

    pub fn kind(&self) -> &SessionEventKind {
        &self.kind
    }

    #[must_use]
    pub fn with_generation_id(mut self, generation_id: GenerationId) -> Self {
        self.generation_id = Some(generation_id);
        self
    }

    #[must_use]
    pub fn with_message_id(mut self, message_id: MessageId) -> Self {
        self.message_id = Some(message_id);
        self
    }
}

/// Immutable facts captured when a turn obtains the Session lease.
#[derive(Clone, Debug)]
pub struct TurnAdmission {
    session_id: SessionId,
    turn_id: TurnId,
    history: Arc<[SessionEvent]>,
}

impl TurnAdmission {
    pub fn new(session_id: SessionId, turn_id: TurnId, history: Arc<[SessionEvent]>) -> Self {
        Self {
            session_id,
            turn_id,
            history,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn history(&self) -> Arc<[SessionEvent]> {
        Arc::clone(&self.history)
    }
}

/// Stable summary returned when a Session turn lease is explicitly settled.
#[derive(Clone, Debug)]
pub struct TurnCommitSummary {
    turn_id: TurnId,
    events: Vec<SessionEvent>,
}

impl TurnCommitSummary {
    pub fn new(turn_id: TurnId, events: Vec<SessionEvent>) -> Self {
        Self { turn_id, events }
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    pub fn into_events(self) -> Vec<SessionEvent> {
        self.events
    }
}

/// A structured failure from the canonical Session authority.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionRuntimeError {
    #[error(transparent)]
    Persistence(#[from] SessionPersistenceError),
    #[error(transparent)]
    InvalidHistory(#[from] PhysicalLogError),
    #[error("Session runtime is stopped")]
    Stopped,
    #[error("turn {turn_id} is no longer active for Session {session_id}")]
    TurnNotActive {
        session_id: SessionId,
        turn_id: TurnId,
    },
    #[error(
        "turn {turn_id} in Session {session_id} is closed by terminal event at sequence {terminal_seq}"
    )]
    TurnClosed {
        session_id: SessionId,
        turn_id: TurnId,
        terminal_seq: u64,
    },
    #[error("Session event sequence is exhausted")]
    SequenceExhausted,
}

/// A behavior-rich Session service. Implementations own admission, history, and canonical append.
pub trait SessionRuntime: Send + Sync {
    fn begin_turn<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Box<dyn SessionTurn>, SessionRuntimeError>>;
}

/// A non-Clone lease for one admitted turn.
pub trait SessionTurn: Send + Sync {
    fn admission(&self) -> &TurnAdmission;

    fn append<'a>(
        &'a self,
        draft: &'a SessionEventDraft,
    ) -> SessionFuture<'a, Result<Arc<SessionEvent>, SessionRuntimeError>>;

    fn settle(
        self: Box<Self>,
    ) -> SessionFuture<'static, Result<TurnCommitSummary, SessionRuntimeError>>;
}

/// A violation of the canonical physical-log address contract.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PhysicalLogError {
    #[error(
        "event at physical index {physical_index} belongs to Session {actual}, expected {expected}"
    )]
    WrongSession {
        physical_index: usize,
        expected: SessionId,
        actual: SessionId,
    },
    #[error("event at physical index {physical_index} has sequence {actual}, expected {expected}")]
    WrongSequence {
        physical_index: usize,
        expected: u64,
        actual: u64,
    },
    #[error("event ID {event_id} is duplicated at physical index {physical_index}")]
    DuplicateEventId {
        physical_index: usize,
        event_id: EventId,
    },
}

/// Validate without reordering or repairing a persistence provider's physical result.
pub fn validate_physical_log(
    session_id: &SessionId,
    events: &[SessionEvent],
) -> Result<(), PhysicalLogError> {
    let mut event_ids = HashSet::with_capacity(events.len());
    for (physical_index, event) in events.iter().enumerate() {
        if &event.session_id != session_id {
            return Err(PhysicalLogError::WrongSession {
                physical_index,
                expected: session_id.clone(),
                actual: event.session_id.clone(),
            });
        }
        let expected = u64::try_from(physical_index).unwrap_or(u64::MAX);
        if event.seq != expected {
            return Err(PhysicalLogError::WrongSequence {
                physical_index,
                expected,
                actual: event.seq,
            });
        }
        if !event_ids.insert(event.event_id.clone()) {
            return Err(PhysicalLogError::DuplicateEventId {
                physical_index,
                event_id: event.event_id.clone(),
            });
        }
    }
    Ok(())
}
