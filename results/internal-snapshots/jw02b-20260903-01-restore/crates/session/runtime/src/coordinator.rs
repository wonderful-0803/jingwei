use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use jingwei_core::{SessionEvent, SessionId, TurnId};
use jingwei_plugin::{
    EventObserverBinding, LifecycleFuture, RuntimeError, ServiceLifecycle, StopReason,
};
use jingwei_session::{
    PersistAppendOutcome, SessionEventDraft, SessionFuture, SessionPersistence, SessionRuntime,
    SessionRuntimeError, SessionTurn, TurnAdmission, TurnCommitSummary, validate_physical_log,
};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

pub(crate) struct MailboxRuntime {
    control: Arc<RuntimeControl>,
}

struct RuntimeControl {
    persistence: Arc<dyn SessionPersistence>,
    coordinators: Mutex<BTreeMap<SessionId, Arc<CoordinatorHandle>>>,
    stopping: Arc<AtomicBool>,
    observers: Arc<[EventObserverBinding]>,
}

impl MailboxRuntime {
    pub(crate) fn new(persistence: Arc<dyn SessionPersistence>) -> Self {
        Self::with_observers(persistence, Vec::new())
    }

    pub(crate) fn with_observers(
        persistence: Arc<dyn SessionPersistence>,
        observers: Vec<EventObserverBinding>,
    ) -> Self {
        Self {
            control: Arc::new(RuntimeControl {
                persistence,
                coordinators: Mutex::new(BTreeMap::new()),
                stopping: Arc::new(AtomicBool::new(false)),
                observers: Arc::from(observers),
            }),
        }
    }

    pub(crate) fn lifecycle(&self) -> MailboxLifecycle {
        MailboxLifecycle {
            control: Arc::clone(&self.control),
        }
    }

    async fn coordinator(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<CoordinatorHandle>, SessionRuntimeError> {
        let mut coordinators = self.control.coordinators.lock().await;
        if self.control.stopping.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Stopped);
        }
        if let Some(coordinator) = coordinators.get(session_id) {
            return Ok(Arc::clone(coordinator));
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        let coordinator = Arc::new(CoordinatorHandle {
            sender,
            join: StdMutex::new(None),
        });
        let weak = Arc::downgrade(&coordinator);
        let join = tokio::spawn(run_coordinator(
            session_id.clone(),
            Arc::clone(&self.control.persistence),
            Arc::clone(&self.control.observers),
            Arc::clone(&self.control.stopping),
            weak,
            receiver,
        ));
        *coordinator
            .join
            .lock()
            .expect("coordinator join lock should not be poisoned") = Some(join);
        coordinators.insert(session_id.clone(), Arc::clone(&coordinator));
        Ok(coordinator)
    }
}

pub(crate) struct MailboxLifecycle {
    control: Arc<RuntimeControl>,
}

impl ServiceLifecycle for MailboxLifecycle {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(
        self: Box<Self>,
        _reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>> {
        Box::pin(async move { self.control.shutdown().await })
    }
}

impl RuntimeControl {
    async fn shutdown(&self) -> Result<(), RuntimeError> {
        self.stopping.store(true, Ordering::Release);
        let coordinators = self
            .coordinators
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut stopped = Vec::with_capacity(coordinators.len());
        for coordinator in &coordinators {
            let (response, waiter) = oneshot::channel();
            if coordinator
                .sender
                .send(Command::Shutdown { response })
                .is_ok()
            {
                stopped.push(waiter);
            }
        }
        for waiter in stopped {
            let _ = waiter.await;
        }

        let mut first_failure = None;
        for coordinator in coordinators {
            let join = coordinator
                .join
                .lock()
                .expect("coordinator join lock should not be poisoned")
                .take();
            if let Some(join) = join
                && let Err(error) = join.await
                && first_failure.is_none()
            {
                first_failure = Some(error.to_string());
            }
        }
        match first_failure {
            Some(message) => Err(RuntimeError::new(format!(
                "Session coordinator failed during shutdown: {message}"
            ))),
            None => Ok(()),
        }
    }
}

impl SessionRuntime for MailboxRuntime {
    fn begin_turn<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Box<dyn SessionTurn>, SessionRuntimeError>> {
        Box::pin(async move {
            if self.control.stopping.load(Ordering::Acquire) {
                return Err(SessionRuntimeError::Stopped);
            }
            let coordinator = self.coordinator(session_id).await?;
            if self.control.stopping.load(Ordering::Acquire) {
                return Err(SessionRuntimeError::Stopped);
            }
            let (response, waiter) = oneshot::channel();
            coordinator
                .sender
                .send(Command::Begin { response })
                .map_err(|_| SessionRuntimeError::Stopped)?;
            let grant = waiter.await.map_err(|_| SessionRuntimeError::Stopped)??;
            Ok(Box::new(grant.into_turn()) as Box<dyn SessionTurn>)
        })
    }
}

struct CoordinatorHandle {
    sender: mpsc::UnboundedSender<Command>,
    join: StdMutex<Option<JoinHandle<()>>>,
}

enum Command {
    Begin {
        response: oneshot::Sender<Result<AdmissionGrant, SessionRuntimeError>>,
    },
    Append(Box<AppendCommand>),
    Settle {
        token: u64,
        turn_id: TurnId,
        response: oneshot::Sender<Result<TurnCommitSummary, SessionRuntimeError>>,
    },
    Abandon {
        token: u64,
    },
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

struct AppendCommand {
    token: u64,
    turn_id: TurnId,
    draft: SessionEventDraft,
    response: oneshot::Sender<Result<Arc<SessionEvent>, SessionRuntimeError>>,
}

struct AdmissionGrant {
    admission: TurnAdmission,
    release: LeaseRelease,
}

impl AdmissionGrant {
    fn into_turn(self) -> MailboxTurn {
        MailboxTurn {
            admission: self.admission,
            release: Some(self.release),
        }
    }
}

struct LeaseRelease {
    coordinator: Arc<CoordinatorHandle>,
    token: u64,
    armed: bool,
}

impl LeaseRelease {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LeaseRelease {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .coordinator
                .sender
                .send(Command::Abandon { token: self.token });
        }
    }
}

struct MailboxTurn {
    admission: TurnAdmission,
    release: Option<LeaseRelease>,
}

impl MailboxTurn {
    fn coordinator(&self) -> &Arc<CoordinatorHandle> {
        &self
            .release
            .as_ref()
            .expect("an exposed Session turn always owns its release guard")
            .coordinator
    }

    fn token(&self) -> u64 {
        self.release
            .as_ref()
            .expect("an exposed Session turn always owns its release guard")
            .token
    }
}

impl SessionTurn for MailboxTurn {
    fn admission(&self) -> &TurnAdmission {
        &self.admission
    }

    fn append<'a>(
        &'a self,
        draft: &'a SessionEventDraft,
    ) -> SessionFuture<'a, Result<Arc<SessionEvent>, SessionRuntimeError>> {
        let coordinator = Arc::clone(self.coordinator());
        let token = self.token();
        let turn_id = self.admission.turn_id().clone();
        let draft = draft.clone();
        Box::pin(async move {
            let (response, waiter) = oneshot::channel();
            coordinator
                .sender
                .send(Command::Append(Box::new(AppendCommand {
                    token,
                    turn_id,
                    draft,
                    response,
                })))
                .map_err(|_| SessionRuntimeError::Stopped)?;
            waiter.await.map_err(|_| SessionRuntimeError::Stopped)?
        })
    }

    fn settle(
        self: Box<Self>,
    ) -> SessionFuture<'static, Result<TurnCommitSummary, SessionRuntimeError>> {
        Box::pin(async move {
            let mut turn = *self;
            let coordinator = Arc::clone(turn.coordinator());
            let token = turn.token();
            let turn_id = turn.admission.turn_id().clone();
            let (response, waiter) = oneshot::channel();
            coordinator
                .sender
                .send(Command::Settle {
                    token,
                    turn_id,
                    response,
                })
                .map_err(|_| SessionRuntimeError::Stopped)?;
            let result = waiter.await.map_err(|_| SessionRuntimeError::Stopped)?;
            if result.is_ok() {
                turn.release
                    .as_mut()
                    .expect("settling turn still owns its release guard")
                    .disarm();
            }
            result
        })
    }
}

struct CoordinatorState {
    loaded: bool,
    events: Vec<SessionEvent>,
    active: Option<ActiveTurn>,
    next_token: u64,
    pending: VecDeque<oneshot::Sender<Result<AdmissionGrant, SessionRuntimeError>>>,
    poisoned: Option<PoisonedFailure>,
}

#[derive(Clone)]
enum PoisonedFailure {
    Persistence(jingwei_session::SessionPersistenceError),
    InvalidHistory(jingwei_session::PhysicalLogError),
}

impl PoisonedFailure {
    fn runtime_error(&self) -> SessionRuntimeError {
        match self {
            Self::Persistence(error) => SessionRuntimeError::Persistence(error.clone()),
            Self::InvalidHistory(error) => SessionRuntimeError::InvalidHistory(error.clone()),
        }
    }
}

impl Default for CoordinatorState {
    fn default() -> Self {
        Self {
            loaded: false,
            events: Vec::new(),
            active: None,
            next_token: 1,
            pending: VecDeque::new(),
            poisoned: None,
        }
    }
}

struct ActiveTurn {
    token: u64,
    turn_id: TurnId,
    start_seq: usize,
    terminal_seq: Option<u64>,
}

async fn run_coordinator(
    session_id: SessionId,
    persistence: Arc<dyn SessionPersistence>,
    observers: Arc<[EventObserverBinding]>,
    stopping: Arc<AtomicBool>,
    coordinator: Weak<CoordinatorHandle>,
    mut receiver: mpsc::UnboundedReceiver<Command>,
) {
    let mut state = CoordinatorState::default();
    let mut shutdown_responses = Vec::new();
    while let Some(command) = receiver.recv().await {
        match command {
            Command::Begin { response } => {
                if stopping.load(Ordering::Acquire) {
                    let _ = response.send(Err(SessionRuntimeError::Stopped));
                    continue;
                }
                if response.is_closed() {
                    continue;
                }
                if state.active.is_some() {
                    state.pending.push_back(response);
                } else {
                    let _ = admit(
                        &session_id,
                        persistence.as_ref(),
                        stopping.as_ref(),
                        &coordinator,
                        &mut state,
                        response,
                    )
                    .await;
                }
            }
            Command::Append(command) => {
                append(
                    &session_id,
                    persistence.as_ref(),
                    observers.as_ref(),
                    &mut state,
                    *command,
                )
                .await;
            }
            Command::Settle {
                token,
                turn_id,
                response,
            } => {
                settle(&session_id, &mut state, token, turn_id, response);
                grant_pending(
                    &session_id,
                    persistence.as_ref(),
                    stopping.as_ref(),
                    &coordinator,
                    &mut state,
                )
                .await;
            }
            Command::Abandon { token } => {
                if state
                    .active
                    .as_ref()
                    .is_some_and(|turn| turn.token == token)
                {
                    state.active = None;
                    grant_pending(
                        &session_id,
                        persistence.as_ref(),
                        stopping.as_ref(),
                        &coordinator,
                        &mut state,
                    )
                    .await;
                }
            }
            Command::Shutdown { response } => {
                receiver.close();
                reject_pending(&mut state);
                shutdown_responses.push(response);
            }
        }
    }
    state.active = None;
    reject_pending(&mut state);
    for response in shutdown_responses {
        let _ = response.send(());
    }
}

async fn grant_pending(
    session_id: &SessionId,
    persistence: &dyn SessionPersistence,
    stopping: &AtomicBool,
    coordinator: &Weak<CoordinatorHandle>,
    state: &mut CoordinatorState,
) {
    if stopping.load(Ordering::Acquire) {
        reject_pending(state);
        return;
    }
    while state.active.is_none() {
        let Some(response) = state.pending.pop_front() else {
            break;
        };
        if response.is_closed() {
            continue;
        }
        if admit(
            session_id,
            persistence,
            stopping,
            coordinator,
            state,
            response,
        )
        .await
        {
            break;
        }
    }
}

async fn admit(
    session_id: &SessionId,
    persistence: &dyn SessionPersistence,
    stopping: &AtomicBool,
    coordinator: &Weak<CoordinatorHandle>,
    state: &mut CoordinatorState,
    response: oneshot::Sender<Result<AdmissionGrant, SessionRuntimeError>>,
) -> bool {
    if response.is_closed() {
        return false;
    }
    if stopping.load(Ordering::Acquire) {
        let _ = response.send(Err(SessionRuntimeError::Stopped));
        return false;
    }
    if let Some(failure) = &state.poisoned {
        let _ = response.send(Err(failure.runtime_error()));
        return false;
    }
    if !state.loaded {
        let events = match persistence.load(session_id).await {
            Ok(events) => events,
            Err(error) => {
                let _ = response.send(Err(error.into()));
                return false;
            }
        };
        if let Err(error) = validate_physical_log(session_id, &events) {
            let _ = response.send(Err(error.into()));
            return false;
        }
        state.events = events;
        state.loaded = true;
    }

    if stopping.load(Ordering::Acquire) {
        let _ = response.send(Err(SessionRuntimeError::Stopped));
        return false;
    }

    let Some(handle) = coordinator.upgrade() else {
        let _ = response.send(Err(SessionRuntimeError::Stopped));
        return false;
    };
    let token = state.next_token;
    let Some(next_token) = token.checked_add(1) else {
        let _ = response.send(Err(SessionRuntimeError::SequenceExhausted));
        return false;
    };
    state.next_token = next_token;
    let turn_id = TurnId::new();
    let admission = TurnAdmission::new(
        session_id.clone(),
        turn_id.clone(),
        Arc::from(state.events.clone()),
    );
    state.active = Some(ActiveTurn {
        token,
        turn_id,
        start_seq: state.events.len(),
        terminal_seq: None,
    });
    let grant = AdmissionGrant {
        admission,
        release: LeaseRelease {
            coordinator: handle,
            token,
            armed: true,
        },
    };
    if response.send(Ok(grant)).is_err() {
        state.active = None;
        return false;
    }
    true
}

fn reject_pending(state: &mut CoordinatorState) {
    for response in state.pending.drain(..) {
        let _ = response.send(Err(SessionRuntimeError::Stopped));
    }
}

async fn append(
    session_id: &SessionId,
    persistence: &dyn SessionPersistence,
    observers: &[EventObserverBinding],
    state: &mut CoordinatorState,
    command: AppendCommand,
) {
    let AppendCommand {
        token,
        turn_id,
        draft,
        response,
    } = command;
    let Some(active) = state
        .active
        .as_ref()
        .filter(|active| active.token == token && active.turn_id == turn_id)
    else {
        let _ = response.send(Err(SessionRuntimeError::TurnNotActive {
            session_id: session_id.clone(),
            turn_id,
        }));
        return;
    };
    let canonical_turn_id = active.turn_id.clone();
    if let Some(failure) = &state.poisoned {
        let _ = response.send(Err(failure.runtime_error()));
        return;
    }
    if let Some(existing) = state
        .events
        .iter()
        .find(|event| event.event_id == *draft.event_id())
    {
        if existing.turn_id == canonical_turn_id
            && existing.generation_id.as_ref() == draft.generation_id()
            && existing.message_id.as_ref() == draft.message_id()
            && &existing.kind == draft.kind()
        {
            let _ = response.send(Ok(Arc::new(existing.clone())));
        } else {
            let _ = response.send(Err(jingwei_session::SessionPersistenceError::Conflict {
                seq: existing.seq,
                event_id: draft.event_id().clone(),
                message: format!(
                    "event ID {} was reused with different content",
                    draft.event_id()
                ),
            }
            .into()));
        }
        return;
    }
    if let Some(terminal_seq) = state.active.as_ref().and_then(|turn| turn.terminal_seq) {
        let _ = response.send(Err(SessionRuntimeError::TurnClosed {
            session_id: session_id.clone(),
            turn_id: canonical_turn_id,
            terminal_seq,
        }));
        return;
    }
    let seq = match u64::try_from(state.events.len()) {
        Ok(seq) => seq,
        Err(_) => {
            let _ = response.send(Err(SessionRuntimeError::SequenceExhausted));
            return;
        }
    };
    let event = SessionEvent {
        event_id: draft.event_id().clone(),
        session_id: session_id.clone(),
        turn_id: canonical_turn_id,
        generation_id: draft.generation_id().cloned(),
        message_id: draft.message_id().cloned(),
        seq,
        kind: draft.kind().clone(),
    };
    let result = persistence.commit_durable(&event).await;
    match result {
        Ok(PersistAppendOutcome::Appended) => {
            state.events.push(event.clone());
            mark_terminal(state, &event);
            let committed = Arc::new(event);
            observe_all(observers, Arc::clone(&committed)).await;
            let _ = response.send(Ok(committed));
        }
        Ok(PersistAppendOutcome::ReplayedExact) => {
            state.events.push(event.clone());
            mark_terminal(state, &event);
            let _ = response.send(Ok(Arc::new(event)));
        }
        Err(
            error @ jingwei_session::SessionPersistenceError::Io {
                certainty: jingwei_session::CommitCertainty::Indeterminate,
                ..
            },
        ) => {
            reconcile_indeterminate(
                session_id,
                persistence,
                observers,
                state,
                event,
                error,
                response,
            )
            .await;
        }
        Err(
            error @ (jingwei_session::SessionPersistenceError::Conflict { .. }
            | jingwei_session::SessionPersistenceError::CorruptLog { .. }
            | jingwei_session::SessionPersistenceError::InvalidHistory(_)),
        ) => {
            state.poisoned = Some(PoisonedFailure::Persistence(error.clone()));
            let _ = response.send(Err(error.into()));
        }
        Err(error) => {
            let _ = response.send(Err(error.into()));
        }
    }
}

async fn reconcile_indeterminate(
    session_id: &SessionId,
    persistence: &dyn SessionPersistence,
    observers: &[EventObserverBinding],
    state: &mut CoordinatorState,
    candidate: SessionEvent,
    original_error: jingwei_session::SessionPersistenceError,
    response: oneshot::Sender<Result<Arc<SessionEvent>, SessionRuntimeError>>,
) {
    let restored = match persistence.load(session_id).await {
        Ok(events) => events,
        Err(error) => {
            state.poisoned = Some(PoisonedFailure::Persistence(error.clone()));
            let _ = response.send(Err(error.into()));
            return;
        }
    };
    if let Err(error) = validate_physical_log(session_id, &restored) {
        state.poisoned = Some(PoisonedFailure::InvalidHistory(error.clone()));
        let _ = response.send(Err(error.into()));
        return;
    }

    let head = state.events.len();
    if restored == state.events {
        let proven_absent = match original_error {
            jingwei_session::SessionPersistenceError::Io {
                operation,
                message,
                certainty: _,
            } => jingwei_session::SessionPersistenceError::Io {
                operation,
                message: format!(
                    "{message}; physical reconciliation proved the candidate was not committed"
                ),
                certainty: jingwei_session::CommitCertainty::DefinitelyNotCommitted,
            },
            error => error,
        };
        let _ = response.send(Err(proven_absent.into()));
        return;
    }
    if restored.len() == head + 1 && restored[..head] == state.events && restored[head] == candidate
    {
        let committed = restored[head].clone();
        state.events = restored;
        mark_terminal(state, &committed);
        let committed = Arc::new(committed);
        observe_all(observers, Arc::clone(&committed)).await;
        let _ = response.send(Ok(committed));
        return;
    }

    let conflict = jingwei_session::SessionPersistenceError::Conflict {
        seq: candidate.seq,
        event_id: candidate.event_id,
        message: "durable log diverged while reconciling an indeterminate commit".to_string(),
    };
    state.poisoned = Some(PoisonedFailure::Persistence(conflict.clone()));
    let _ = response.send(Err(conflict.into()));
}

fn mark_terminal(state: &mut CoordinatorState, event: &SessionEvent) {
    if matches!(
        event.kind,
        jingwei_core::SessionEventKind::Done { .. } | jingwei_core::SessionEventKind::Error { .. }
    ) && let Some(active) = state.active.as_mut()
        && active.turn_id == event.turn_id
    {
        active.terminal_seq = Some(event.seq);
    }
}

async fn observe_all(observers: &[EventObserverBinding], event: Arc<SessionEvent>) {
    for binding in observers {
        if let Err(error) = binding.observer().observe(Arc::clone(&event)).await {
            tracing::warn!(
                observer_key = binding.key(),
                observer_owner = binding.owner().as_str(),
                event_id = %event.event_id,
                error = %error,
                "post-commit EventObserver failed"
            );
        }
    }
}

fn settle(
    session_id: &SessionId,
    state: &mut CoordinatorState,
    token: u64,
    turn_id: TurnId,
    response: oneshot::Sender<Result<TurnCommitSummary, SessionRuntimeError>>,
) {
    let Some(active) = state
        .active
        .as_ref()
        .filter(|active| active.token == token && active.turn_id == turn_id)
    else {
        let _ = response.send(Err(SessionRuntimeError::TurnNotActive {
            session_id: session_id.clone(),
            turn_id,
        }));
        return;
    };
    if let Some(failure) = &state.poisoned {
        state.active = None;
        let _ = response.send(Err(failure.runtime_error()));
        return;
    }
    let summary = TurnCommitSummary::new(
        active.turn_id.clone(),
        state.events[active.start_seq..].to_vec(),
    );
    state.active = None;
    let _ = response.send(Ok(summary));
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use jingwei_session::{PersistAppendOutcome, SessionPersistenceError};

    use super::*;

    struct EmptyPersistence;

    impl SessionPersistence for EmptyPersistence {
        fn load<'a>(
            &'a self,
            _session_id: &'a SessionId,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<SessionEvent>, SessionPersistenceError>> + Send + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn commit_durable<'a>(
            &'a self,
            _event: &'a SessionEvent,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<PersistAppendOutcome, SessionPersistenceError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(PersistAppendOutcome::Appended) })
        }
    }

    #[tokio::test]
    async fn stopping_runtime_never_creates_a_new_coordinator() {
        let runtime = MailboxRuntime::new(Arc::new(EmptyPersistence));
        runtime.control.stopping.store(true, Ordering::Release);

        drop(
            runtime
                .coordinator(&SessionId::from("sess_stop_create_race"))
                .await,
        );

        assert!(
            runtime.control.coordinators.lock().await.is_empty(),
            "stopping and coordinator creation must share the map-lock linearization boundary"
        );
    }
}
