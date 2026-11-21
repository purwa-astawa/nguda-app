//! The randomness port: the engine's poll jitter (AD-17) draws from it, so core
//! and the engine need no RNG crate (AD-1) and tests can fix the value.

/// A source of uniform random numbers.
pub trait Random: Send + Sync + 'static {
    /// A uniform value in `[0, 1)`.
    fn unit(&self) -> f64;
}
