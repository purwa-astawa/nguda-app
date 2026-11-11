//! nguda engine: one actor task that owns live state (AD-2, AD-3).
//!
//! Inputs arrive as [`EngineMsg`] on a channel and are handled one at a time. Each
//! change is computed on a copy of the day, persisted as one [`nguda_core::StoreBatch`],
//! and swapped in only once that commit succeeds. After each change the engine bumps
//! `rev` and publishes a [`Snapshot`] on a `watch` channel, and one-time outputs go
//! out as [`Effect`]s. It holds no `Mutex` and runs no per-second tick: it waits on
//! one deadline through [`Clock::sleep_until`], and rendering the ticking elapsed
//! time is the subscriber's job (AD-11).
//!
//! Plan commands are targeted (AD-15): each item command names its `item_id`, and
//! those whose meaning depends on the current state also carry the `expected_rev`
//! of the snapshot the sender rendered. A stale rev, invalid input, or a domain
//! error applies nothing and sets the [`StatusKey::CommandRejected`] status.

mod actor;
mod batch;
mod commands;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use nguda_core::{Clock, Day, IdGen, ItemId, Running, Store, TimeZone, Timestamp};
use tokio::sync::{mpsc, watch};

/// How often the engine records `last_alive_at` for crash recovery (AD-6).
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

/// Every input to the engine.
///
/// Messages may carry task titles, so they are never logged or `Debug`-printed
/// outside tests (AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineMsg {
    /// Adds a pending flexible item after the last flexible one. The title must not
    /// be blank and `planned_secs` must be positive.
    CreateTask { title: String, planned_secs: u64 },
    /// Changes the title and/or planned length of a non-removed item. `None` keeps
    /// the current value.
    EditTask {
        item_id: ItemId,
        title: Option<String>,
        planned_secs: Option<u64>,
    },
    /// Moves a flexible item next to another flexible item.
    ReorderTask {
        item_id: ItemId,
        anchor: ReorderAnchor,
        expected_rev: u64,
    },
    /// Pins a flexible item at `pinned_start` (required), or makes a fixed item
    /// flexible again (`pinned_start` is then ignored).
    ToggleFixed {
        item_id: ItemId,
        pinned_start: Option<Timestamp>,
        expected_rev: u64,
    },
    /// Tombstones a pending, paused, done, or carried-over item and logs it (AD-6).
    RemoveTask { item_id: ItemId },
    /// Starts a pending item or resumes a paused one now, pausing any other running
    /// item first. Already running: no change.
    Start { item_id: ItemId, expected_rev: u64 },
    /// Pauses the running item now.
    Pause { item_id: ItemId },
}

/// Where [`EngineMsg::ReorderTask`] puts the moved item, relative to another
/// flexible item. Never a numeric index (AD-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReorderAnchor {
    Before(ItemId),
    After(ItemId),
}

/// Who owns a status entry. Each owner sets or clears only its own key (AD-14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusKey {
    /// The last store request failed.
    Store,
    /// The last command was rejected (stale rev, invalid input, or a domain rule).
    /// Cleared by the next command that passes validation.
    CommandRejected,
}

/// One problem the user should see. At most one entry per key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub key: StatusKey,
    pub message: String,
}

/// Live state as published after each applied change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub rev: u64,
    /// Today's plan. `None` only before startup finishes (rev 0).
    pub day: Option<Day>,
    pub running: Option<Running>,
    /// What to do next (`Day::next_item`). Set only while nothing runs (AD-11).
    pub next_item: Option<NextItem>,
    /// Sorted by key.
    pub status: Vec<StatusEntry>,
}

/// The item the tray shows while nothing runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextItem {
    pub item_id: ItemId,
    pub title: String,
}

/// One-time outputs, each emitted exactly once (AD-3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A segment left open by a crash was closed at startup.
    Recovered { item_id: ItemId },
}

/// The ports and settings the engine runs on.
pub struct EngineDeps {
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn Store>,
    pub ids: Arc<dyn IdGen>,
    /// The zone that decides "today" and is fixed on each new day (AD-8).
    pub tz: TimeZone,
}

/// The engine task has stopped, so the message was not delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineClosed;

impl fmt::Display for EngineClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("engine is not running")
    }
}

impl std::error::Error for EngineClosed {}

/// Cheap to clone: a sender for inputs and a receiver for snapshots.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<EngineMsg>,
    snapshots: watch::Receiver<Snapshot>,
}

impl EngineHandle {
    /// Fire-and-forget: queues `msg` without waiting for the engine (AD-4).
    pub fn send(&self, msg: EngineMsg) -> Result<(), EngineClosed> {
        self.tx.send(msg).map_err(|_| EngineClosed)
    }

    /// A receiver that starts at the latest snapshot.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }
}

/// Starts the engine on the current tokio runtime. Must be called from within one.
///
/// Returns at once: startup (load, crash recovery, first snapshot at rev 1) runs on
/// the engine task, and messages sent meanwhile queue and are handled after it, in
/// order. Until then subscribers see `Snapshot::default()` (rev 0).
pub fn spawn(deps: EngineDeps) -> (EngineHandle, mpsc::UnboundedReceiver<Effect>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (effects_tx, effects) = mpsc::unbounded_channel();
    let (snap_tx, snapshots) = watch::channel(Snapshot::default());
    tokio::spawn(actor::run(deps, rx, snap_tx, effects_tx));
    (EngineHandle { tx, snapshots }, effects)
}
