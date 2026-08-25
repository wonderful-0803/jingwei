//! Durable append-only JSONL implementation of the Session persistence seam.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use jingwei_core::{SessionEvent, SessionId};
use jingwei_plugin::{
    FactoryContext, LifecycleFuture, ManagedService, MountContext, MountError, Plugin,
    PluginDescriptor, RuntimeError, ServiceFactory, ServiceLifecycle, StopReason,
};
use jingwei_session::{
    CommitCertainty, PersistAppendOutcome, SESSION_PERSISTENCE, SessionFuture, SessionPersistence,
    SessionPersistenceError, validate_physical_log,
};

/// Plugin that contributes the JSONL [`SessionPersistence`] provider.
#[derive(Clone)]
pub struct JsonlSessionPersistencePlugin {
    root: PathBuf,
    sync_data: Arc<dyn SyncData>,
}

impl JsonlSessionPersistencePlugin {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            sync_data: Arc::new(SystemSyncData),
        }
    }
}

impl Plugin for JsonlSessionPersistencePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider("session-jsonl", SESSION_PERSISTENCE, "jsonl")
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_persistence_factory(Arc::new(JsonlSessionPersistenceFactory {
            root: self.root.clone(),
            sync_data: Arc::clone(&self.sync_data),
        }))
    }
}

struct JsonlSessionPersistenceFactory {
    root: PathBuf,
    sync_data: Arc<dyn SyncData>,
}

impl ServiceFactory<dyn SessionPersistence> for JsonlSessionPersistenceFactory {
    fn construct<'a>(
        &'a self,
        _ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionPersistence>, RuntimeError>> {
        Box::pin(async move {
            let control = Arc::new(OperationControl::default());
            let persistence: Arc<dyn SessionPersistence> =
                Arc::new(JsonlSessionPersistence::with_control(
                    self.root.clone(),
                    Arc::clone(&self.sync_data),
                    Arc::clone(&control),
                ));
            Ok(ManagedService::new(
                persistence,
                Box::new(JsonlSessionPersistenceLifecycle { control }),
            ))
        })
    }
}

#[derive(Default)]
struct OperationControl {
    stopping: AtomicBool,
    inflight: AtomicUsize,
    drained: tokio::sync::Notify,
}

impl OperationControl {
    fn admit(self: &Arc<Self>) -> Result<OperationPermit, SessionPersistenceError> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(persistence_stopping());
        }
        self.inflight.fetch_add(1, Ordering::SeqCst);
        if self.stopping.load(Ordering::SeqCst) {
            self.release();
            return Err(persistence_stopping());
        }
        Ok(OperationPermit {
            control: Arc::clone(self),
        })
    }

    fn release(&self) {
        let previous = self.inflight.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "operation permit count underflowed");
        if previous == 1 {
            self.drained.notify_one();
        }
    }

    async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        while self.inflight.load(Ordering::SeqCst) != 0 {
            self.drained.notified().await;
        }
    }
}

struct OperationPermit {
    control: Arc<OperationControl>,
}

impl Drop for OperationPermit {
    fn drop(&mut self) {
        self.control.release();
    }
}

struct JsonlSessionPersistenceLifecycle {
    control: Arc<OperationControl>,
}

impl ServiceLifecycle for JsonlSessionPersistenceLifecycle {
    fn start(&mut self) -> LifecycleFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(
        self: Box<Self>,
        _reason: StopReason,
    ) -> LifecycleFuture<'static, Result<(), RuntimeError>> {
        Box::pin(async move {
            self.control.stop().await;
            Ok(())
        })
    }
}

fn persistence_stopping() -> SessionPersistenceError {
    SessionPersistenceError::Io {
        operation: "admit_operation",
        message: "JSONL persistence is stopping".to_string(),
        certainty: CommitCertainty::DefinitelyNotCommitted,
    }
}

/// Durable JSONL persistence with one physical file and one in-process writer lock per Session.
#[derive(Clone)]
pub struct JsonlSessionPersistence {
    root: PathBuf,
    session_gates: Arc<Mutex<HashMap<SessionId, Arc<Mutex<SessionState>>>>>,
    sync_data: Arc<dyn SyncData>,
    control: Arc<OperationControl>,
}

#[derive(Default)]
struct SessionState {
    uncertain: Option<SessionPersistenceError>,
}

trait SyncData: Send + Sync {
    fn sync_data(&self, file: &std::fs::File) -> std::io::Result<()>;
}

struct SystemSyncData;

impl SyncData for SystemSyncData {
    fn sync_data(&self, file: &std::fs::File) -> std::io::Result<()> {
        file.sync_data()
    }
}

impl JsonlSessionPersistence {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_sync_data(root, Arc::new(SystemSyncData))
    }

    fn with_sync_data(root: impl Into<PathBuf>, sync_data: Arc<dyn SyncData>) -> Self {
        Self::with_control(root, sync_data, Arc::new(OperationControl::default()))
    }

    fn with_control(
        root: impl Into<PathBuf>,
        sync_data: Arc<dyn SyncData>,
        control: Arc<OperationControl>,
    ) -> Self {
        Self {
            root: root.into(),
            session_gates: Arc::new(Mutex::new(HashMap::new())),
            sync_data,
            control,
        }
    }

    fn gate_for(&self, session_id: &SessionId) -> Arc<Mutex<SessionState>> {
        let mut gates = self
            .session_gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            gates
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(SessionState::default()))),
        )
    }

    fn load_sync(
        &self,
        session_id: &SessionId,
        gate: &Mutex<SessionState>,
    ) -> Result<Vec<SessionEvent>, SessionPersistenceError> {
        let state = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.uncertain {
            return Err(error.clone());
        }
        read_physical_log(&session_file_path(&self.root, session_id), session_id)
    }

    fn commit_sync(
        &self,
        event: &SessionEvent,
        gate: &Mutex<SessionState>,
    ) -> Result<PersistAppendOutcome, SessionPersistenceError> {
        let mut line =
            serde_json::to_vec(event).map_err(|error| SessionPersistenceError::Serialization {
                message: error.to_string(),
            })?;
        line.push(b'\n');

        let mut state = gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(error) = &state.uncertain {
            return Err(error.clone());
        }
        let path = session_file_path(&self.root, &event.session_id);
        let events = read_physical_log(&path, &event.session_id)?;

        if let Some(existing) = events
            .iter()
            .find(|existing| existing.event_id == event.event_id)
        {
            if existing == event {
                return Ok(PersistAppendOutcome::ReplayedExact);
            }
            return Err(SessionPersistenceError::Conflict {
                seq: event.seq,
                event_id: event.event_id.clone(),
                message: format!(
                    "event ID is already durable at sequence {} with different content",
                    existing.seq
                ),
            });
        }

        let requested_index =
            usize::try_from(event.seq).map_err(|_| SessionPersistenceError::Conflict {
                seq: event.seq,
                event_id: event.event_id.clone(),
                message: format!("sequence cannot address durable head {}", events.len()),
            })?;
        if requested_index != events.len() {
            let message = if let Some(existing) = events.get(requested_index) {
                format!(
                    "durable sequence {} is occupied by event {}",
                    event.seq, existing.event_id
                )
            } else {
                format!("sequence is not the expected durable head {}", events.len())
            };
            return Err(SessionPersistenceError::Conflict {
                seq: event.seq,
                event_id: event.event_id.clone(),
                message,
            });
        }

        fs::create_dir_all(&self.root).map_err(|error| {
            io_error(
                "create_dir_all",
                error,
                CommitCertainty::DefinitelyNotCommitted,
            )
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                io_error(
                    "open_append",
                    error,
                    CommitCertainty::DefinitelyNotCommitted,
                )
            })?;
        state.uncertain = Some(SessionPersistenceError::Io {
            operation: "commit_in_progress",
            message: "durable outcome is unknown until sync_data succeeds".to_string(),
            certainty: CommitCertainty::Indeterminate,
        });
        if let Err(error) = file.write_all(&line) {
            return Err(latch_indeterminate(&mut state, "write_all", error));
        }
        if let Err(error) = file.flush() {
            return Err(latch_indeterminate(&mut state, "flush", error));
        }
        if let Err(error) = self.sync_data.sync_data(&file) {
            return Err(latch_indeterminate(&mut state, "sync_data", error));
        }
        state.uncertain = None;

        Ok(PersistAppendOutcome::Appended)
    }
}

impl SessionPersistence for JsonlSessionPersistence {
    fn load<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> SessionFuture<'a, Result<Vec<SessionEvent>, SessionPersistenceError>> {
        let persistence = self.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let permit = persistence.control.admit()?;
            let gate = persistence.gate_for(&session_id);
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                persistence.load_sync(&session_id, &gate)
            })
            .await
            .map_err(|error| SessionPersistenceError::Io {
                operation: "load_blocking_task",
                message: error.to_string(),
                certainty: CommitCertainty::DefinitelyNotCommitted,
            })?
        })
    }

    fn commit_durable<'a>(
        &'a self,
        event: &'a SessionEvent,
    ) -> SessionFuture<'a, Result<PersistAppendOutcome, SessionPersistenceError>> {
        let persistence = self.clone();
        let event = event.clone();
        Box::pin(async move {
            let permit = persistence.control.admit()?;
            let gate = persistence.gate_for(&event.session_id);
            let worker_gate = Arc::clone(&gate);
            match tokio::task::spawn_blocking(move || {
                let _permit = permit;
                persistence.commit_sync(&event, &worker_gate)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    let failure = SessionPersistenceError::Io {
                        operation: "commit_blocking_task",
                        message: error.to_string(),
                        certainty: CommitCertainty::Indeterminate,
                    };
                    gate.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .uncertain = Some(failure.clone());
                    Err(failure)
                }
            }
        })
    }
}

fn latch_indeterminate(
    state: &mut SessionState,
    operation: &'static str,
    error: std::io::Error,
) -> SessionPersistenceError {
    let failure = io_error(operation, error, CommitCertainty::Indeterminate);
    state.uncertain = Some(failure.clone());
    failure
}

fn read_physical_log(
    path: &Path,
    session_id: &SessionId,
) -> Result<Vec<SessionEvent>, SessionPersistenceError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io_error(
                "read",
                error,
                CommitCertainty::DefinitelyNotCommitted,
            ));
        }
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&b'\n') {
        return Err(SessionPersistenceError::CorruptLog {
            physical_index: bytes.iter().filter(|byte| **byte == b'\n').count(),
            message: "physical JSONL tail is not terminated by a newline".to_string(),
        });
    }

    let mut events = Vec::new();
    for (physical_index, record) in bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        let event = serde_json::from_slice(record).map_err(|error| {
            SessionPersistenceError::CorruptLog {
                physical_index,
                message: format!("JSON record cannot be decoded: {error}"),
            }
        })?;
        events.push(event);
    }
    validate_physical_log(session_id, &events).map_err(SessionPersistenceError::InvalidHistory)?;
    Ok(events)
}

fn io_error(
    operation: &'static str,
    error: std::io::Error,
    certainty: CommitCertainty,
) -> SessionPersistenceError {
    SessionPersistenceError::Io {
        operation,
        message: error.to_string(),
        certainty,
    }
}

fn encode_session_id(session_id: &SessionId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = session_id.as_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn decode_session_id(encoded: &str) -> Option<SessionId> {
    if encoded.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok().map(SessionId::from)
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Return the path-contained, reversible filename for a Session ID.
pub fn session_file_path(root: &Path, session_id: &SessionId) -> PathBuf {
    root.join(format!("session-{}.jsonl", encode_session_id(session_id)))
}

/// Decode a filename previously returned by [`session_file_path`].
pub fn session_id_from_file_path(path: &Path) -> Option<SessionId> {
    let file_name = path.file_name()?.to_str()?;
    let encoded = file_name.strip_prefix("session-")?.strip_suffix(".jsonl")?;
    decode_session_id(encoded)
}

#[cfg(test)]
mod tests {
    use std::future::{Future, poll_fn};
    use std::io;
    use std::sync::Condvar;
    use std::task::Poll;

    use jingwei_core::{CapabilityId, EventId, SessionEventKind, TurnId};
    use jingwei_plugin::Registrar;
    use jingwei_session::{SESSION_RUNTIME, SessionRuntime, SessionRuntimeError, SessionTurn};

    use super::*;

    struct FailingSyncData;

    impl SyncData for FailingSyncData {
        fn sync_data(&self, _file: &std::fs::File) -> io::Result<()> {
            Err(io::Error::other("injected sync_data failure"))
        }
    }

    struct BlockingSyncData {
        entered: tokio::sync::Notify,
        release: (Mutex<bool>, Condvar),
    }

    impl BlockingSyncData {
        fn new() -> Self {
            Self {
                entered: tokio::sync::Notify::new(),
                release: (Mutex::new(false), Condvar::new()),
            }
        }

        async fn wait_until_entered(&self) {
            self.entered.notified().await;
        }

        fn release(&self) {
            let (released, ready) = &self.release;
            *released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            ready.notify_all();
        }
    }

    impl SyncData for BlockingSyncData {
        fn sync_data(&self, _file: &std::fs::File) -> io::Result<()> {
            self.entered.notify_one();
            let (released, ready) = &self.release;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = ready
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(())
        }
    }

    struct CapturedRuntime;

    impl SessionRuntime for CapturedRuntime {
        fn begin_turn<'a>(
            &'a self,
            _session_id: &'a SessionId,
        ) -> SessionFuture<'a, Result<Box<dyn SessionTurn>, SessionRuntimeError>> {
            Box::pin(async { Err(SessionRuntimeError::Stopped) })
        }
    }

    struct CapturePersistenceFactory {
        captured: Arc<Mutex<Option<Arc<dyn SessionPersistence>>>>,
    }

    impl ServiceFactory<dyn SessionRuntime> for CapturePersistenceFactory {
        fn construct<'a>(
            &'a self,
            ctx: FactoryContext<'a>,
        ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionRuntime>, RuntimeError>> {
            Box::pin(async move {
                let persistence = ctx
                    .session_persistence()
                    .ok_or_else(|| RuntimeError::new("test runtime requires persistence"))?;
                *self
                    .captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(persistence);
                let runtime: Arc<dyn SessionRuntime> = Arc::new(CapturedRuntime);
                Ok(ManagedService::ready(runtime))
            })
        }
    }

    const PERSISTENCE_REQUIREMENT: &[CapabilityId] = &[SESSION_PERSISTENCE];

    struct CapturePersistencePlugin {
        captured: Arc<Mutex<Option<Arc<dyn SessionPersistence>>>>,
    }

    impl Plugin for CapturePersistencePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor::provider("capture-runtime", SESSION_RUNTIME, "capture")
                .requires_capabilities(PERSISTENCE_REQUIREMENT)
        }

        fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
            ctx.provide_session_runtime_factory(Arc::new(CapturePersistenceFactory {
                captured: Arc::clone(&self.captured),
            }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_accepted_blocking_commit_after_waiter_cancellation() {
        let root = std::env::temp_dir().join(format!("jingwei-jsonl-{}", uuid::Uuid::new_v4()));
        let sync_data = Arc::new(BlockingSyncData::new());
        let captured = Arc::new(Mutex::new(None));
        let mut registrar = Registrar::default();
        registrar.add(JsonlSessionPersistencePlugin {
            root: root.clone(),
            sync_data: sync_data.clone(),
        });
        registrar.add(CapturePersistencePlugin {
            captured: Arc::clone(&captured),
        });
        registrar.require(SESSION_RUNTIME);
        let registry = registrar.finish().await.unwrap();
        let persistence = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap();
        let event = SessionEvent {
            event_id: EventId::new(),
            session_id: SessionId::from("jsonl-cancelled-waiter"),
            turn_id: TurnId::from("turn-cancelled-waiter"),
            generation_id: None,
            message_id: None,
            seq: 0,
            kind: SessionEventKind::AssistantDelta {
                text: "accepted before cancellation".to_string(),
            },
        };

        let worker_persistence = Arc::clone(&persistence);
        let worker_event = event.clone();
        let waiter =
            tokio::spawn(async move { worker_persistence.commit_durable(&worker_event).await });
        sync_data.wait_until_entered().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());

        let mut shutdown = Box::pin(registry.shutdown());
        let was_pending = poll_fn(|context| match shutdown.as_mut().poll(context) {
            Poll::Pending => Poll::Ready(true),
            Poll::Ready(result) => {
                result.unwrap();
                Poll::Ready(false)
            }
        })
        .await;
        assert!(matches!(
            persistence.load(&event.session_id).await,
            Err(SessionPersistenceError::Io {
                operation: "admit_operation",
                certainty: CommitCertainty::DefinitelyNotCommitted,
                message: _
            })
        ));
        sync_data.release();
        if was_pending {
            shutdown.await.unwrap();
        }

        assert!(
            was_pending,
            "shutdown returned while an accepted blocking commit was still running"
        );
        assert_eq!(
            JsonlSessionPersistence::new(&root)
                .load(&event.session_id)
                .await
                .unwrap(),
            [event]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn visible_complete_line_after_sync_failure_remains_indeterminate_until_restart() {
        let root = std::env::temp_dir().join(format!("jingwei-jsonl-{}", uuid::Uuid::new_v4()));
        let persistence = JsonlSessionPersistence::with_sync_data(&root, Arc::new(FailingSyncData));
        let session_id = SessionId::from("jsonl-sync-failure");
        let expected = SessionEvent {
            event_id: EventId::new(),
            session_id: session_id.clone(),
            turn_id: TurnId::from("turn-sync-failure"),
            generation_id: None,
            message_id: None,
            seq: 0,
            kind: SessionEventKind::AssistantDelta {
                text: "visible but not proven durable".to_string(),
            },
        };

        assert!(matches!(
            persistence.commit_durable(&expected).await,
            Err(SessionPersistenceError::Io {
                operation: "sync_data",
                certainty: CommitCertainty::Indeterminate,
                message: _
            })
        ));
        let visible: SessionEvent =
            serde_json::from_slice(&std::fs::read(session_file_path(&root, &session_id)).unwrap())
                .unwrap();
        assert_eq!(visible, expected);

        assert!(matches!(
            persistence.load(&session_id).await,
            Err(SessionPersistenceError::Io {
                operation: "sync_data",
                certainty: CommitCertainty::Indeterminate,
                message: _
            })
        ));
        assert!(matches!(
            persistence.commit_durable(&expected).await,
            Err(SessionPersistenceError::Io {
                operation: "sync_data",
                certainty: CommitCertainty::Indeterminate,
                message: _
            })
        ));

        let restarted = JsonlSessionPersistence::new(&root);
        assert_eq!(restarted.load(&session_id).await.unwrap(), [expected]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
