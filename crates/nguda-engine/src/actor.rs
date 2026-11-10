//! The actor task: startup with crash recovery, then one message or deadline at a
//! time (AD-3).
//!
//! Logging (AD-14): events carry ids, revs, dates and kinds only. Titles, items,
//! days and messages are never logged or `Debug`-printed.

use std::collections::BTreeMap;
use std::sync::Arc;

use nguda_core::{
    Clock, Date, Day, DayLogEntry, DayLogKind, DomainError, IdGen, ItemId, ItemStatus,
    OpenSegmentRef, OrderKey, PlanItem, Running, Store, StoreBatch, StoreError, StoreOp, TimeZone,
    Timestamp,
};
use tokio::sync::{mpsc, watch};

use crate::batch::diff;
use crate::{
    DEMO_TITLE, Effect, EngineDeps, EngineMsg, HEARTBEAT_EVERY, Snapshot, StatusEntry, StatusKey,
};

/// The demo item's planned length.
const DEMO_PLANNED_SECS: u64 = 25 * 60;

pub(crate) async fn run(
    deps: EngineDeps,
    mut rx: mpsc::UnboundedReceiver<EngineMsg>,
    snapshots: watch::Sender<Snapshot>,
    effects: mpsc::UnboundedSender<Effect>,
) {
    let mut actor = Actor::new(deps, snapshots, effects);
    actor.startup().await;
    loop {
        let deadline = actor.clock.sleep_until(actor.next_deadline);
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => actor.handle(msg).await,
                None => break,
            },
            () = deadline => actor.heartbeat().await,
        }
    }
    tracing::debug!("engine stopped: every handle dropped");
}

/// The engine's state. Owned by the one task; no `Mutex` (AD-3).
struct Actor {
    clock: Arc<dyn Clock>,
    store: Arc<dyn Store>,
    ids: Arc<dyn IdGen>,
    tz: TimeZone,
    /// Today's plan, as last committed.
    day: Day,
    status: BTreeMap<StatusKey, String>,
    rev: u64,
    /// The only deadline in this story: the next heartbeat.
    next_deadline: Timestamp,
    snapshots: watch::Sender<Snapshot>,
    effects: mpsc::UnboundedSender<Effect>,
}

/// A crash-left open segment, with what recovery needs.
struct Crash {
    segment: OpenSegmentRef,
    last_alive: Option<Timestamp>,
    day: Day,
}

impl Actor {
    fn new(
        deps: EngineDeps,
        snapshots: watch::Sender<Snapshot>,
        effects: mpsc::UnboundedSender<Effect>,
    ) -> Self {
        let now = deps.clock.now();
        let day = Day::new(today(now, &deps.tz), tz_name(&deps.tz));
        Self {
            clock: deps.clock,
            store: deps.store,
            ids: deps.ids,
            tz: deps.tz,
            day,
            status: BTreeMap::new(),
            rev: 0,
            next_deadline: deadline_after(now),
            snapshots,
            effects,
        }
    }

    /// Recovers a crash-left open segment, loads today, and publishes rev 1. Runs
    /// before any message is handled.
    async fn startup(&mut self) {
        let now = self.clock.now();
        let today = today(now, &self.tz);
        let mut today_day = None;
        match self.find_crash().await {
            Err(e) => self.store_failed("recovery read", &e),
            Ok(None) => {
                let mut batch = StoreBatch::new();
                batch.push(StoreOp::SetLastAlive { at: now });
                self.commit(batch).await;
            }
            Ok(Some(crash)) => {
                let date = crash.segment.date;
                let kept = self.recover(crash, now).await;
                if date == today {
                    today_day = Some(kept);
                }
            }
        }
        self.day = match today_day {
            Some(day) => day,
            None => match self.store.load_day(today).await {
                Ok(Some(day)) => day,
                Ok(None) => Day::new(today, tz_name(&self.tz)),
                Err(e) => {
                    self.store_failed("load today", &e);
                    Day::new(today, tz_name(&self.tz))
                }
            },
        };
        self.next_deadline = deadline_after(now);
        self.publish();
    }

    async fn find_crash(&self) -> Result<Option<Crash>, StoreError> {
        let Some(segment) = self.store.open_segment().await? else {
            return Ok(None);
        };
        let last_alive = self.store.last_alive_at().await?;
        let day = self.store.load_day(segment.date).await?.ok_or_else(|| {
            StoreError::new(format!(
                "open segment for item {} has no day {}",
                segment.item_id, segment.date
            ))
        })?;
        Ok(Some(Crash {
            segment,
            last_alive,
            day,
        }))
    }

    /// Closes the crash-left segment at the last heartbeat (or its start), pausing
    /// the item and logging `Recovered` on the segment's own day, in one batch with
    /// the heartbeat. Returns the segment's day: recovered, or as loaded on failure.
    async fn recover(&mut self, crash: Crash, now: Timestamp) -> Day {
        let Crash {
            segment,
            last_alive,
            day,
        } = crash;
        let end = last_alive.map_or(segment.started_at, |at| at.max(segment.started_at));
        let mut next = day.clone();
        if let Err(e) = next.pause(segment.item_id, end) {
            tracing::warn!(
                item_id = %segment.item_id,
                kind = domain_kind(&e),
                "crash recovery rejected by the domain"
            );
            return day;
        }
        let mut batch = diff(&day, &next);
        batch.push(StoreOp::AppendDayLog {
            date: segment.date,
            entry: DayLogEntry {
                at: now,
                kind: DayLogKind::Recovered {
                    item_id: segment.item_id,
                },
            },
        });
        batch.push(StoreOp::SetLastAlive { at: now });
        if !self.commit(batch).await {
            return day;
        }
        tracing::info!(
            item_id = %segment.item_id,
            date = %segment.date,
            "closed a crash-left open segment"
        );
        // A closed receiver only means nobody listens for effects yet.
        let _ = self.effects.send(Effect::Recovered {
            item_id: segment.item_id,
        });
        next
    }

    async fn handle(&mut self, msg: EngineMsg) {
        let status_before = self.status.clone();
        let changed = match msg {
            EngineMsg::StartDemo => self.start_demo().await,
        };
        if changed || self.status != status_before {
            self.publish();
        }
    }

    async fn start_demo(&mut self) -> bool {
        let now = self.clock.now();
        let mut next = self.day.clone();
        if let Err(e) = start_demo_on(&mut next, self.ids.as_ref(), now) {
            tracing::warn!(
                msg = "StartDemo",
                kind = domain_kind(&e),
                "message rejected"
            );
            return false;
        }
        if next == self.day {
            return false;
        }
        let batch = diff(&self.day, &next);
        self.swap_after_commit(next, batch).await
    }

    /// Commits `batch`, then swaps `next` in. On failure the old state stays.
    async fn swap_after_commit(&mut self, next: Day, batch: StoreBatch) -> bool {
        if !self.commit(batch).await {
            return false;
        }
        self.day = next;
        true
    }

    /// Records `last_alive_at`. Publishes only if the store status changed.
    async fn heartbeat(&mut self) {
        let now = self.clock.now();
        let status_before = self.status.clone();
        let mut batch = StoreBatch::new();
        batch.push(StoreOp::SetLastAlive { at: now });
        self.commit(batch).await;
        self.next_deadline = deadline_after(now);
        if self.status != status_before {
            self.publish();
        }
    }

    /// The handler's one await. Success clears the `Store` status; failure sets it.
    async fn commit(&mut self, batch: StoreBatch) -> bool {
        let ops = batch.ops().len();
        match self.store.commit(batch).await {
            Ok(()) => {
                self.status.remove(&StatusKey::Store);
                true
            }
            Err(e) => {
                tracing::warn!(ops, "store commit failed");
                self.store_failed("commit", &e);
                false
            }
        }
    }

    /// Sets the `Store` status. Store errors carry ids only, never titles.
    fn store_failed(&mut self, what: &'static str, e: &StoreError) {
        tracing::warn!(what, error = %e, "store request failed");
        self.status.insert(StatusKey::Store, e.message.clone());
    }

    fn publish(&mut self) {
        self.rev += 1;
        let snapshot = Snapshot {
            rev: self.rev,
            day: Some(self.day.clone()),
            running: running_of(&self.day),
            status: self
                .status
                .iter()
                .map(|(key, message)| StatusEntry {
                    key: *key,
                    message: message.clone(),
                })
                .collect(),
        };
        tracing::debug!(rev = self.rev, date = %self.day.date(), "snapshot published");
        // Every receiver may be gone; the engine keeps serving senders regardless.
        self.snapshots.send_replace(snapshot);
    }
}

/// StartDemo on `day`: find or add the demo item, pause any other running item,
/// then restart the demo's segment at `now` (pause+resume from Running, resume from
/// Paused, start from Pending). A demo already restarted at `now` is left as is. On
/// error `day` may be half-changed: callers pass a
/// copy and drop it.
pub(crate) fn start_demo_on(
    day: &mut Day,
    ids: &dyn IdGen,
    now: Timestamp,
) -> Result<(), DomainError> {
    let demo = match day.items().iter().find(|i| i.title() == DEMO_TITLE) {
        Some(item) => item.id(),
        None => {
            let id = ids.new_item_id();
            let order = OrderKey::between(None, None)?;
            day.add(PlanItem::new(id, DEMO_TITLE, DEMO_PLANNED_SECS, order))?;
            id
        }
    };
    let others: Vec<ItemId> = day
        .items()
        .iter()
        .filter(|i| i.id() != demo && i.status() == ItemStatus::Running)
        .map(PlanItem::id)
        .collect();
    for id in others {
        day.pause(id, now)?;
    }
    let item = day.item(demo);
    match item.map(PlanItem::status) {
        // Restarting at the instant it started changes nothing, and a zero-length
        // segment beside a new one at the same start would collide in the store.
        Some(ItemStatus::Running)
            if item
                .and_then(PlanItem::running_view)
                .map(|r| r.segment_started_at)
                == Some(now) =>
        {
            Ok(())
        }
        Some(ItemStatus::Running) => {
            day.pause(demo, now)?;
            day.resume(demo, now)
        }
        Some(ItemStatus::Paused) => day.resume(demo, now),
        _ => day.start(demo, now),
    }
}

/// The first item that is running.
fn running_of(day: &Day) -> Option<Running> {
    day.items().iter().find_map(PlanItem::running_view)
}

fn today(now: Timestamp, tz: &TimeZone) -> Date {
    now.to_zoned(tz.clone()).date()
}

/// The IANA name fixed on new days, or `UTC` for a zone without one.
fn tz_name(tz: &TimeZone) -> String {
    tz.iana_name().unwrap_or("UTC").to_owned()
}

fn deadline_after(now: Timestamp) -> Timestamp {
    now.checked_add(HEARTBEAT_EVERY).unwrap_or(Timestamp::MAX)
}

/// A domain error's kind, for logs: never its ids' titles or a `Debug` dump.
fn domain_kind(e: &DomainError) -> &'static str {
    match e {
        DomainError::AnotherItemRunning => "another_item_running",
        DomainError::InvalidTransition { .. } => "invalid_transition",
        DomainError::InvalidOrder => "invalid_order",
        DomainError::UnknownItem(_) => "unknown_item",
        DomainError::DuplicateItem(_) => "duplicate_item",
        DomainError::InvalidRecord { .. } => "invalid_record",
    }
}
