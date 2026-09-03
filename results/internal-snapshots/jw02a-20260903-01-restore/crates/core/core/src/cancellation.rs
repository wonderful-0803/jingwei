use std::future::Future;
use std::pin::Pin;

/// A borrowed wait for a sticky cancellation signal.
pub type CancellationFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Read-only cancellation observed by turn-scoped framework capabilities.
pub trait CancellationSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;

    fn cancelled(&self) -> CancellationFuture<'_>;
}
