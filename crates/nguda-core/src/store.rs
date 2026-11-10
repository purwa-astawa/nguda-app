//! The store port: atomic write batches, day loads, and the engine's recovery reads
//! (AD-2, AD-3, AD-6).
//!
//! The future type is std-only so core stays free of any async runtime (AD-1).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use jiff::Timestamp;
use jiff::civil::Date;

use crate::day_log::DayLogEntry;
use crate::plan::{Day, ItemId, PlanItem};

/// One write in a [`StoreBatch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOp {
    /// Creates the day, or confirms it exists. A day's zone never changes once stored.
    PutDay { date: Date, tz: String },
    /// Inserts or updates the item's plan fields. Its segments are ignored: they
    /// change only through [`StoreOp::OpenSegment`] and [`StoreOp::CloseSegment`]. An
    /// item never moves to another day.
    PutItem { date: Date, item: PlanItem },
    /// Opens a segment for `item_id` at `started_at`. At most one segment is open.
    OpenSegment {
        item_id: ItemId,
        started_at: Timestamp,
    },
    /// Closes the open segment identified by `(item_id, started_at)`, exactly once.
    CloseSegment {
        item_id: ItemId,
        started_at: Timestamp,
        ended_at: Timestamp,
    },
    /// Records the engine heartbeat, replacing any earlier value (AD-6).
    SetLastAlive { at: Timestamp },
    /// Appends `entry` to the stored day `date`'s log.
    AppendDayLog { date: Date, entry: DayLogEntry },
}

/// Where the one open segment is: its item's day, the item, and its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSegmentRef {
    pub date: Date,
    pub item_id: ItemId,
    pub started_at: Timestamp,
}

/// Writes committed together, in order, as one transaction: all or nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreBatch {
    ops: Vec<StoreOp>,
}

impl StoreBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, op: StoreOp) {
        self.ops.push(op);
    }

    pub fn ops(&self) -> &[StoreOp] {
        &self.ops
    }
}

/// A store failure. The message carries ids and storage errors only, never titles
/// (AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    pub message: String,
}

impl StoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

/// The result of a store request.
pub type StoreFuture<T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send>>;

/// Persistence for the plan and its time segments.
pub trait Store: Send + Sync + 'static {
    /// Applies every op of `batch` in one transaction. Any failing op rolls back the
    /// whole batch.
    fn commit(&self, batch: StoreBatch) -> StoreFuture<()>;

    /// The stored day with its items and segments, or `None` if no such day exists.
    fn load_day(&self, date: Date) -> StoreFuture<Option<Day>>;

    /// The last recorded engine heartbeat, or `None` if none was ever written.
    fn last_alive_at(&self) -> StoreFuture<Option<Timestamp>>;

    /// The open segment, on any day, or `None` if every segment is closed.
    fn open_segment(&self) -> StoreFuture<Option<OpenSegmentRef>>;

    /// The day's log entries in append order. Empty for an unknown day.
    fn load_day_log(&self, date: Date) -> StoreFuture<Vec<DayLogEntry>>;
}
