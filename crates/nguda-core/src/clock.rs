use std::future::Future;
use std::pin::Pin;

use jiff::Timestamp;

/// A wait that completes at a deadline. Std-only so core needs no async runtime (AD-1).
pub type SleepFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The only source of "now" for the domain and the engine (AD-8), and the only way
/// the engine waits for a deadline (AD-3).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;

    /// Completes once [`Clock::now`] has reached `deadline`, or at once if it already
    /// has.
    fn sleep_until(&self, deadline: Timestamp) -> SleepFuture;
}
