//! nguda engine: one actor task that owns live state (AD-2, AD-3).
//!
//! Inputs arrive as [`EngineMsg`] on a channel and are handled one at a time. Each
//! change is computed on a copy of the day, persisted as one [`nguda_core::StoreBatch`],
//! and swapped in only once that commit succeeds. After each change the engine bumps
//! `rev` and publishes a [`Snapshot`] on a `watch` channel, and one-time outputs go
//! out as [`Effect`]s. It holds no `Mutex` and runs no per-second tick: it waits on
//! one deadline through [`Clock::sleep_until`] (the earliest of the heartbeat, the
//! running item's planned end, and, while nothing runs, the next local midnight),
//! and rendering the ticking elapsed time is the subscriber's job (AD-11).
//!
//! Overflow: every publish re-flows the day and lists the flexible items that end
//! after the work-end setting ([`EngineMsg::SetWorkEnd`], persisted, default 17:00
//! in the engine's zone). A non-empty list sets the [`StatusKey::Overflow`] status
//! and opens an episode with one [`Effect::Notify`] of [`PromptKind::Overflow`];
//! when the list empties again the status clears and that prompt is withdrawn. The
//! heartbeat re-checks the list, so time passing alone can open an episode.
//!
//! Plan commands are targeted (AD-15): each item command names its `item_id`, and
//! those whose meaning depends on the current state also carry the `expected_rev`
//! of the snapshot the sender rendered. A stale rev, invalid input, or a domain
//! error applies nothing and sets the [`StatusKey::CommandRejected`] status.
//!
//! [`EngineMsg::InsertTask`] places a new item by [`InsertAnchor`], never by index.
//! Inserting "now" while an item runs pauses it onto the day's persisted interrupt
//! stack, and [`nguda_core::next_task`] (used by Switch and for `next_item`) resumes
//! the top of that stack before anything else.
//!
//! Day lock (AD-6, AD-7): each day's lock decision (on [`Day`], so in every
//! snapshot) starts undecided and is decided once, by [`EngineMsg::LockDay`], or
//! by a `Start` or insert-now carrying a [`LockChoice`]. Locking writes the day's
//! frozen baseline in the same batch; the engine never updates or reads it back.
//!
//! Carry-over (AD-6): [`EngineMsg::CarryOverToTomorrow`] moves the current overflow
//! list to tomorrow. Items never change days: each original turns `carried_over`,
//! and tomorrow's day (created with today's zone if missing) gains a fresh pending
//! item linked by `carried_from`, all with `CarriedOver` day-log entries in one
//! batch. The emptied list then ends the overflow episode.
//!
//! Tags (CAP-8, AD-9): [`EngineMsg::SetTags`] replaces an item's tags through the
//! normal plan path. The actor keeps every tag name ever stored (loaded at startup,
//! merged after each committed `SetTags`, never shrinking) and publishes them as
//! [`Snapshot::known_tags`]. Carry-over copies keep their original's tags.
//!
//! Rollover (AD-8): "today" is the day the engine last made current. While nothing
//! runs, the next local midnight after today is a deadline; when the local date has
//! moved past today, the engine loads (or creates) the new date's day and publishes
//! it. The check also runs before and after each command, so a command carrying a
//! pre-rollover rev is stale-rejected. While an item runs no rollover is armed and
//! its day stays current until it stops; segments are never split at midnight.
//!
//! Queries (AD-4, AD-16): [`EngineHandle::query_day`] and
//! [`EngineHandle::query_week`] are the only request/reply paths. They share the
//! inbox with commands, but their reply senders live outside [`EngineMsg`]. The
//! actor answers without waiting: it notes `now` and, when today is asked for,
//! clones its committed day, then spawns a task that loads what else it needs
//! (the day itself for any other date, the baseline of a locked day, and the day
//! log), builds [`nguda_core::report`] for each date, and replies. A week query
//! builds the reports of its dates up to today in order (a later date, such as a
//! carried-over tomorrow, is skipped, as is a date with no stored day; the first
//! store error fails the whole query), then folds them with
//! [`nguda_core::week_report`]. A query is read-only: no commit, no rev
//! bump, no publish, and no status change.
//!
//! Export (AD-16, CAP-9): [`EngineMsg::ExportCsv`] is fire-and-forget like any
//! command, but it never touches the plan, the committed state, or the
//! `CommandRejected` and `Store` statuses. The actor notes `now` and today, clones
//! its committed day, and spawns a task that resolves the range's dates (a week's
//! dates, or every stored date with today added, never past today, ascending),
//! builds each date's report the way the queries do (a date with no stored day
//! adds nothing), turns them into CSV with [`nguda_core::export_csv`], and writes
//! the file through the [`FileSink`] port on a blocking thread. The task posts the
//! outcome back to the actor through a weak inbox sender, so a pending export
//! never keeps the engine alive. Success clears [`StatusKey::Export`] (publishing
//! only if it was set) and emits [`Effect::ExportDone`]; a store or sink failure
//! sets [`StatusKey::Export`] and publishes. Logs carry the range kind, week, and
//! row count only, never the path, titles, or contents (AD-14).
//!
//! Calendar import (CAP-2, AD-9, AD-17): the engine never awaits the calendar in
//! a handler. [`EngineMsg::SignIn`] marks the account
//! [`GoogleAccount::SigningIn`], publishes, and spawns the interactive sign-in;
//! imports of today's primary-calendar events are spawned at startup, after a
//! successful sign-in, and at rollover. Each spawned task posts its result back
//! through a weak inbox sender under a request id, and the actor drops any result
//! whose id is not the latest it issued for that kind, and an import result whose
//! date is no longer today. An applied import merges the events into a copy of
//! today with [`nguda_core::merge`] and commits the diff like any plan change (an
//! unchanged plan commits nothing). Failures set [`StatusKey::Sync`], which the
//! next successful import or sign-in clears; "not signed in" sets no status, only
//! [`GoogleAccount::SignedOut`]. Tokens and event titles are never logged.

mod actor;
mod batch;
mod commands;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nguda_core::{
    CalendarError, CalendarSource, Clock, Date, Day, DayReport, ExportRange, FetchedEvent,
    FileSink, IdGen, IsoWeek, ItemId, LockChoice, PromptId, PromptKind, Running, Schedule, Store,
    StoreError, TimeZone, Timestamp, WeekReport, WorkEnd,
};
use tokio::sync::{mpsc, oneshot, watch};

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
    ///
    /// `lock` decides the day's lock in the same batch (AD-7) and is sent only
    /// while the day is undecided: `Some(_)` on a decided day is rejected, and
    /// `None` on an undecided day is rejected with an [`Effect::AskLockOnStart`].
    /// `Some(Lock)` freezes the baseline of the day as it is after the start.
    Start {
        item_id: ItemId,
        expected_rev: u64,
        lock: Option<LockChoice>,
    },
    /// Pauses the running item now.
    Pause { item_id: ItemId },
    /// Finishes the running item `item_id` now and starts or resumes the next item,
    /// logging `Switched`. Rejected unless `item_id` is the running item. The engine
    /// never switches on its own.
    Switch { item_id: ItemId, expected_rev: u64 },
    /// Adds `secs` (positive) to the running item `item_id`'s planned length and logs
    /// `Extended`, in one batch. Later flexible items re-flow at publish, and the
    /// planned-end prompt re-arms at the new end; an open prompt for the item is
    /// withdrawn after the commit. Rejected unless `item_id` is the running item.
    ExtendTask {
        item_id: ItemId,
        secs: u64,
        expected_rev: u64,
    },
    /// Replaces a non-removed item's tags with `tags`, normalised in core (trimmed,
    /// blanks and case-insensitive duplicates dropped, a known name's spelling
    /// taken; see [`nguda_core::normalize_tags`]). Rev-checked. Once committed,
    /// new names join [`Snapshot::known_tags`].
    SetTags {
        item_id: ItemId,
        tags: Vec<String>,
        expected_rev: u64,
    },
    /// Adds a new flexible item at `anchor` and logs `Inserted`, in one batch.
    /// Always rev-checked. The title must not be blank and `planned_secs` must be
    /// positive. `Now` starts the new item, first pausing any running item and
    /// pushing it onto the interrupt stack.
    ///
    /// `lock` follows the [`EngineMsg::Start`] rules for `Now` (a choiceless
    /// insert-now on an undecided day is rejected, with no effect), and is invalid
    /// input with any other anchor. `Some(Lock)` puts the new item in the baseline.
    InsertTask {
        title: String,
        planned_secs: u64,
        anchor: InsertAnchor,
        expected_rev: u64,
        lock: Option<LockChoice>,
    },
    /// Locks the undecided day without starting anything: decides `Locked` and
    /// freezes the baseline of every item not removed or carried over, re-flowed
    /// now, in one batch (AD-6, AD-7). Rev-checked; a decided day is rejected.
    LockDay { expected_rev: u64 },
    /// Sets the work-end time of day to `minutes` after local midnight
    /// (`0..=1439`), persisted. Not rev-checked; the same value changes nothing.
    SetWorkEnd { minutes: u32 },
    /// Carries every item of the current overflow list over to tomorrow (AD-6):
    /// each turns `carried_over` and tomorrow gains a new pending flexible item with
    /// the same title and the remaining time (at least a minute), linked by
    /// `carried_from`, logged `CarriedOver` on today, in one batch. Rev-checked; an
    /// empty overflow list is rejected.
    CarryOverToTomorrow { expected_rev: u64 },
    /// Writes the CSV of `range` (days up to today; today as live state) to
    /// `path`, replacing any file there, off the actor (AD-16). Not rev-checked and
    /// read-only on plan state. The outcome comes back as [`Effect::ExportDone`]
    /// or the [`StatusKey::Export`] status.
    ExportCsv { path: PathBuf, range: ExportRange },
    /// Signs in to Google interactively, off the actor (AD-17). The account shows
    /// [`GoogleAccount::SigningIn`] until it finishes; success imports today's
    /// events, failure sets [`StatusKey::Sync`]. A newer `SignIn` supersedes an
    /// older one still waiting. Not rev-checked.
    SignIn,
}

/// Where [`EngineMsg::ReorderTask`] puts the moved item, relative to another
/// flexible item. Never a numeric index (AD-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReorderAnchor {
    Before(ItemId),
    After(ItemId),
}

/// Where [`EngineMsg::InsertTask`] puts the new item. Never a numeric index (AD-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertAnchor {
    /// Start it now: first in the flow, interrupting any running item.
    Now,
    /// Right after this item; after the running item means first in the flow.
    After(ItemId),
    /// Right before this non-running flexible item.
    Before(ItemId),
    /// After the last flexible item.
    End,
}

/// Who owns a status entry. Each owner sets or clears only its own key (AD-14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusKey {
    /// The last store request failed.
    Store,
    /// The last command was rejected (stale rev, invalid input, or a domain rule).
    /// Cleared by the next command that passes validation.
    CommandRejected,
    /// Re-flow ends flexible items after work-end. Owned by the overflow check
    /// alone: set while the list is non-empty, removed when it is empty.
    Overflow,
    /// The last CSV export failed. Cleared by the next successful export.
    Export,
    /// The last Google sign-in or calendar import failed. Cleared by the next
    /// successful sign-in or import.
    Sync,
}

/// What the engine knows about the Google account. Never carries an address or a
/// token (AD-13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GoogleAccount {
    /// Not known yet: the startup import has not answered.
    #[default]
    Unknown,
    /// No account is signed in.
    SignedOut,
    /// An interactive sign-in is waiting for the browser.
    SigningIn,
    /// An account is signed in.
    SignedIn,
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
    /// What to do next (`nguda_core::next_task`): the top of the interrupt stack,
    /// else the first pending or paused item of `schedule`. Set only while nothing
    /// runs (AD-11).
    pub next_item: Option<NextItem>,
    /// Today's schedule, re-flowed at publish time (`nguda_core::reflow`). Empty
    /// only before startup finishes (rev 0).
    pub schedule: Schedule,
    /// The work-end setting the overflow check used.
    pub work_end: WorkEnd,
    /// Pending or paused flexible items of `schedule` that end after today's
    /// work-end, in schedule order (`Schedule::overflow`).
    pub overflow: Vec<ItemId>,
    /// Sorted by key.
    pub status: Vec<StatusEntry>,
    /// Every tag name ever stored, sorted case-insensitively. Never shrinks.
    pub known_tags: Vec<String>,
    /// The Google account state.
    pub google: GoogleAccount,
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
    /// Show a prompt about `item_id`. Emitted once per `prompt_id`; it changes no
    /// state, so no snapshot accompanies it.
    Notify {
        prompt_id: PromptId,
        item_id: ItemId,
        kind: PromptKind,
    },
    /// Remove the prompt `prompt_id`, posted earlier this session: a planned-end
    /// prompt whose item was extended (emitted only after the extension committed)
    /// or was superseded, or the overflow prompt of an episode that just ended.
    WithdrawNotification { prompt_id: PromptId },
    /// A choiceless `Start` of `item_id` hit an undecided day and was rejected:
    /// surfaces that can should ask "lock the day?" and resend with a choice (AD-7).
    AskLockOnStart { item_id: ItemId },
    /// A CSV export finished writing `rows` records (header not counted).
    ExportDone { rows: usize },
}

/// The ports and settings the engine runs on.
pub struct EngineDeps {
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn Store>,
    pub ids: Arc<dyn IdGen>,
    /// Where CSV exports are written (AD-16).
    pub files: Arc<dyn FileSink>,
    /// The calendar account events are imported from (CAP-2).
    pub calendar: Arc<dyn CalendarSource>,
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

/// A [`EngineHandle::query_day`] or [`EngineHandle::query_week`] that got no
/// report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    /// The engine task has stopped, or dropped the request.
    EngineClosed,
    /// A store read failed. Engine state is untouched.
    Store(StoreError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EngineClosed => EngineClosed.fmt(f),
            Self::Store(e) => write!(f, "store read failed: {e}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// The reply of a day query: the report, `None` for a day with no stored data.
pub(crate) type DayReply = Result<Option<DayReport>, StoreError>;

/// The reply of a week query.
pub(crate) type WeekReply = Result<WeekReport, StoreError>;

/// What arrives on the engine's channel. Kept apart from [`EngineMsg`] so the
/// reply sender never needs its derives.
pub(crate) enum Inbox {
    Command(EngineMsg),
    QueryDay {
        date: Date,
        reply: oneshot::Sender<DayReply>,
    },
    QueryWeek {
        week: IsoWeek,
        reply: oneshot::Sender<WeekReply>,
    },
    /// An export task's outcome, tagged with the request id the actor gave it:
    /// the row count, or the failure message. Outcomes older than the newest one
    /// already applied are dropped (AD-17).
    ExportFinished {
        id: u64,
        result: Result<usize, String>,
    },
    /// A sign-in task's outcome, tagged with its request id. Only the latest
    /// sign-in's outcome applies (AD-17).
    SignInFinished {
        id: u64,
        result: Result<(), CalendarError>,
    },
    /// An import task's fetched events for `date`, tagged with its request id.
    /// Only the latest import's result applies, and only while `date` is still
    /// today (AD-17).
    ImportFinished {
        id: u64,
        date: Date,
        result: Result<Vec<FetchedEvent>, CalendarError>,
    },
}

/// Cheap to clone: a sender for inputs and a receiver for snapshots.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<Inbox>,
    snapshots: watch::Receiver<Snapshot>,
}

impl EngineHandle {
    /// Fire-and-forget: queues `msg` without waiting for the engine (AD-4).
    pub fn send(&self, msg: EngineMsg) -> Result<(), EngineClosed> {
        self.tx.send(Inbox::Command(msg)).map_err(|_| EngineClosed)
    }

    /// The report of `date` (AD-16): `Ok(None)` when no such day is stored. Today
    /// reflects the live state as of the engine's `now`. Read-only. Await it only
    /// off the main thread (AD-4).
    pub async fn query_day(&self, date: Date) -> Result<Option<DayReport>, QueryError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Inbox::QueryDay { date, reply })
            .map_err(|_| QueryError::EngineClosed)?;
        rx.await
            .map_err(|_| QueryError::EngineClosed)?
            .map_err(QueryError::Store)
    }

    /// The estimate gap of `week` (AD-6, AD-16), from the reports of its days.
    /// Today reflects the live state as of the engine's `now`. Read-only. Await
    /// it only off the main thread (AD-4).
    pub async fn query_week(&self, week: IsoWeek) -> Result<WeekReport, QueryError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Inbox::QueryWeek { week, reply })
            .map_err(|_| QueryError::EngineClosed)?;
        rx.await
            .map_err(|_| QueryError::EngineClosed)?
            .map_err(QueryError::Store)
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
    // The actor holds only a weak sender, so dropping every handle still stops it.
    tokio::spawn(actor::run(deps, rx, tx.downgrade(), snap_tx, effects_tx));
    (EngineHandle { tx, snapshots }, effects)
}
