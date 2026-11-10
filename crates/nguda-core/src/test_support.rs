//! Test-only helpers. Not part of the public API.

use std::sync::atomic::{AtomicI64, Ordering};

use jiff::Timestamp;

use crate::clock::{Clock, SleepFuture};

/// A settable clock, in whole seconds since the Unix epoch.
pub(crate) struct FakeClock(AtomicI64);

impl FakeClock {
    pub(crate) fn at(secs: i64) -> Self {
        Self(AtomicI64::new(secs))
    }

    pub(crate) fn set(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }

    pub(crate) fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Self::ts(self.0.load(Ordering::SeqCst))
    }

    /// Never completes: core tests have no deadlines.
    fn sleep_until(&self, _deadline: Timestamp) -> SleepFuture {
        Box::pin(std::future::pending())
    }
}
