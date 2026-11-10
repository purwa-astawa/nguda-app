//! Append-only day-log entries (AD-6). No current state is derived from them.

use jiff::Timestamp;

use crate::plan::ItemId;

/// One change recorded against a day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayLogEntry {
    pub at: Timestamp,
    pub kind: DayLogKind,
}

/// What happened. Later kinds (extended, inserted, carried over, ...) join here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DayLogKind {
    /// An open segment left by a crash was closed at the last heartbeat.
    Recovered { item_id: ItemId },
    /// The user removed the item: it is tombstoned, never deleted.
    Removed { item_id: ItemId },
}
