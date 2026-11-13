//! Test-only helpers. Not part of the public API.

use std::sync::atomic::{AtomicI64, Ordering};

use jiff::Timestamp;
use uuid::Uuid;

use crate::clock::{Clock, SleepFuture};
use crate::order::OrderKey;
use crate::plan::ItemId;
use crate::segment::Segment;

/// The instant `secs` whole seconds after the Unix epoch.
pub(crate) fn ts(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).unwrap()
}

/// The item id `n`, as `Uuid::from_u128`.
pub(crate) fn id(n: u128) -> ItemId {
    ItemId(Uuid::from_u128(n))
}

/// The order key `s`, which must be valid.
pub(crate) fn key(s: &str) -> OrderKey {
    OrderKey::new(s).unwrap()
}

/// A closed segment from `from` to `to`, in seconds since the Unix epoch.
pub(crate) fn closed(from: i64, to: i64) -> Segment {
    Segment {
        started_at: ts(from),
        ended_at: Some(ts(to)),
    }
}

/// A settable clock, in whole seconds since the Unix epoch.
pub(crate) struct FakeClock(AtomicI64);

impl FakeClock {
    pub(crate) fn at(secs: i64) -> Self {
        Self(AtomicI64::new(secs))
    }

    pub(crate) fn set(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        ts(self.0.load(Ordering::SeqCst))
    }

    /// Never completes: core tests have no deadlines.
    fn sleep_until(&self, _deadline: Timestamp) -> SleepFuture {
        Box::pin(std::future::pending())
    }
}
