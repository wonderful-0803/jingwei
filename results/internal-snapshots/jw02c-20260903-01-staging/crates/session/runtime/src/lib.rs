//! Canonical implementation of Jingwei's Session runtime contract.

use std::sync::Arc;

use jingwei_core::{CapabilityId, SessionId};
use jingwei_plugin::{
    EventObserverBinding, FactoryContext, LifecycleFuture, ManagedService, MountContext,
    MountError, Plugin, PluginDescriptor, RuntimeError, ServiceFactory,
};
use jingwei_session::{
    SESSION_PERSISTENCE, SESSION_RUNTIME, SessionPersistence, SessionRuntime, SessionRuntimeError,
    SessionTurn,
};

mod coordinator;

const PERSISTENCE_DEPENDENCY: &[CapabilityId] = &[SESSION_PERSISTENCE];

/// Official provider plugin for Jingwei's canonical Session runtime.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalSessionRuntimePlugin;

impl CanonicalSessionRuntimePlugin {
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for CanonicalSessionRuntimePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::provider(
            "jingwei-session-runtime-canonical",
            SESSION_RUNTIME,
            "canonical",
        )
        .requires_capabilities(PERSISTENCE_DEPENDENCY)
    }

    fn mount(&self, ctx: &mut MountContext<'_>) -> Result<(), MountError> {
        ctx.provide_session_runtime_factory(Arc::new(CanonicalSessionRuntimeFactory))
    }
}

struct CanonicalSessionRuntimeFactory;

impl ServiceFactory<dyn SessionRuntime> for CanonicalSessionRuntimeFactory {
    fn construct<'a>(
        &'a self,
        ctx: FactoryContext<'a>,
    ) -> LifecycleFuture<'a, Result<ManagedService<dyn SessionRuntime>, RuntimeError>> {
        Box::pin(async move {
            let persistence = ctx.session_persistence().ok_or_else(|| {
                RuntimeError::new(
                    "canonical SessionRuntime requires its declared SessionPersistence dependency",
                )
            })?;
            let observers = ctx.event_observers().ok_or_else(|| {
                RuntimeError::new(
                    "Observer bindings are visible only to the selected SessionRuntime factory",
                )
            })?;
            let runtime = Arc::new(CanonicalSessionRuntime::with_observers(
                persistence,
                observers,
            ));
            let lifecycle = runtime.inner.lifecycle();
            let value: Arc<dyn SessionRuntime> = runtime;
            Ok(ManagedService::new(value, Box::new(lifecycle)))
        })
    }
}

/// Standard in-process authority for canonical Session logs.
pub struct CanonicalSessionRuntime {
    inner: coordinator::MailboxRuntime,
}

impl CanonicalSessionRuntime {
    pub fn new(persistence: Arc<dyn SessionPersistence>) -> Self {
        Self {
            inner: coordinator::MailboxRuntime::new(persistence),
        }
    }

    fn with_observers(
        persistence: Arc<dyn SessionPersistence>,
        observers: Vec<EventObserverBinding>,
    ) -> Self {
        Self {
            inner: coordinator::MailboxRuntime::with_observers(persistence, observers),
        }
    }
}

impl SessionRuntime for CanonicalSessionRuntime {
    fn begin_turn<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> jingwei_session::SessionFuture<'a, Result<Box<dyn SessionTurn>, SessionRuntimeError>> {
        self.inner.begin_turn(session_id)
    }
}
