//! Best-effort projections of already accepted canonical Session events.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::event::SessionEvent;

/// A boxed asynchronous Observer operation.
pub type ObserverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An owner-free projection failure. The plugin kernel retains Observer ownership metadata.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ObserverError {
    #[error("Observer failed: {0}")]
    Failed(String),
}

/// A post-commit projection. Returning an error cannot roll back canonical acceptance.
pub trait EventObserver: Send + Sync {
    fn observe(&self, event: Arc<SessionEvent>) -> ObserverFuture<'_, Result<(), ObserverError>>;
}
