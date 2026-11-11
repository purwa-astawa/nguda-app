//! The actor task: startup with crash recovery, then one message or deadline at a
//! time (AD-3).
//!
//! Logging (AD-14): events carry ids, revs, dates and kinds only. Titles, items,
//! days and messages are never logged or `Debug`-printed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use nguda_core::{
    Clock, Date, Day, DayLogEntry, DayLogKind, IdGen, ItemId, OpenSegmentRef, PlanItem, PromptId,
    PromptKind, Running, Store, StoreBatch, StoreError, StoreOp, TimeZone, Timestamp, next_task,
    reflow,
};
use tokio::sync::{mpsc, watch};

use crate::batch::diff;
use crate::commands::{self, domain_kind};
use crate::{
    Effect, EngineDeps, EngineMsg, HEARTBEAT_EVERY, NextItem, Snapshot, StatusEntry, StatusKey,
};

pub(crate) async fn run(
    deps: EngineDeps,
    mut rx: mpsc::UnboundedReceiver<EngineMsg>,
    snapshots: watch::Sender<Snapshot>,
    effects: mpsc::UnboundedSender<Effect>,
) {
    let mut actor = Actor::new(deps, snapshots, effects);
    actor.startup().await;
    loop {
        let deadline = actor.clock.sleep_until(actor.next_deadline());
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(msg) => actor.handle(msg).await,
                None => break,
            },
            () = deadline => actor.on_deadline().await,
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
    /// When the next heartbeat is due.
    heartbeat_at: Timestamp,
    /// The `(item_id, planned_secs)` pairs already prompted for at their planned
    /// end this session, so a pause/resume never re-prompts and a changed planned
    /// length re-arms. Not persisted.
    prompted: BTreeSet<(ItemId, u64)>,
    /// The prompt last posted for each item and not yet withdrawn this session,
    /// so an extension can withdraw it. Not persisted.
    open_prompts: BTreeMap<ItemId, PromptId>,
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
            heartbeat_at: deadline_after(now),
            prompted: BTreeSet::new(),
            open_prompts: BTreeMap::new(),
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
                let batch = StoreBatch::from_iter([StoreOp::SetLastAlive { at: now }]);
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
        self.heartbeat_at = deadline_after(now);
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

    /// One command: rev check, validate and apply on a copy, commit `diff + extras`
    /// unless the day is unchanged, then swap. A rejection sets `CommandRejected`;
    /// anything that passes validation clears it. Publishes when the day or the
    /// status changed, and always on a rejection.
    async fn handle(&mut self, msg: EngineMsg) {
        let status_before = self.status.clone();
        let name = commands::name(&msg);
        let item_id = commands::target(&msg);
        // `msg` moves into `apply`, so note an extension's target first.
        let extend_target = match &msg {
            EngineMsg::ExtendTask { item_id, .. } => Some(*item_id),
            _ => None,
        };
        let now = self.clock.now();
        let mut next = self.day.clone();
        let changed = match commands::apply(msg, &mut next, now, self.ids.as_ref(), self.rev) {
            Err(reject) => {
                match item_id {
                    Some(item_id) => tracing::warn!(
                        command = name,
                        %item_id,
                        kind = reject.kind(),
                        "command rejected"
                    ),
                    None => {
                        tracing::warn!(command = name, kind = reject.kind(), "command rejected");
                    }
                }
                self.status
                    .insert(StatusKey::CommandRejected, reject.to_string());
                self.publish();
                return;
            }
            Ok(extras) => {
                self.status.remove(&StatusKey::CommandRejected);
                if next == self.day {
                    false
                } else {
                    let mut batch = diff(&self.day, &next);
                    batch.extend(extras);
                    self.swap_after_commit(next, batch).await
                }
            }
        };
        if changed || self.status != status_before {
            self.publish();
        }
        if changed {
            self.withdraw_prompt(extend_target);
        }
    }

    /// After a committed extension of `item_id`, withdraws its open prompt, if the
    /// engine posted one this session that is not withdrawn yet (AD-10).
    fn withdraw_prompt(&mut self, item_id: Option<ItemId>) {
        let Some(item_id) = item_id else { return };
        let Some(prompt_id) = self.open_prompts.remove(&item_id) else {
            return;
        };
        tracing::info!(%item_id, %prompt_id, "prompt withdrawn after extend");
        // A closed receiver only means nobody listens for effects.
        let _ = self
            .effects
            .send(Effect::WithdrawNotification { prompt_id });
    }

    /// Commits `batch`, then swaps `next` in. On failure the old state stays.
    async fn swap_after_commit(&mut self, next: Day, batch: StoreBatch) -> bool {
        if !self.commit(batch).await {
            return false;
        }
        self.day = next;
        true
    }

    /// The one deadline the loop waits on (AD-3): the earlier of the heartbeat and
    /// the running item's unprompted planned end.
    fn next_deadline(&self) -> Timestamp {
        match self.planned_end() {
            Some((_, end)) => end.min(self.heartbeat_at),
            None => self.heartbeat_at,
        }
    }

    /// The running item's planned end and its `(item_id, planned_secs)` key, unless
    /// that pair was already prompted: `segment_started_at + (planned_secs −
    /// accumulated_secs)`, saturating, so an item already over is due at once.
    fn planned_end(&self) -> Option<((ItemId, u64), Timestamp)> {
        let running = running_of(&self.day)?;
        let planned = self.day.item(running.item_id)?.planned_secs();
        let key = (running.item_id, planned);
        if self.prompted.contains(&key) {
            return None;
        }
        let left = Duration::from_secs(planned.saturating_sub(running.accumulated_secs));
        let end = running
            .segment_started_at
            .checked_add(left)
            .unwrap_or(Timestamp::MAX);
        Some((key, end))
    }

    /// Handles whatever is due: a planned-end prompt (an effect only: no state,
    /// commit or snapshot), and/or the heartbeat.
    async fn on_deadline(&mut self) {
        let now = self.clock.now();
        if let Some((key @ (item_id, _), end)) = self.planned_end()
            && now >= end
        {
            self.prompted.insert(key);
            let prompt_id = self.ids.new_prompt_id();
            // A still-open earlier prompt for this item (a re-arm after an edit) is
            // withdrawn first, so it is never left behind.
            if let Some(old) = self.open_prompts.insert(item_id, prompt_id) {
                tracing::info!(%item_id, prompt_id = %old, "prompt withdrawn: superseded");
                let _ = self
                    .effects
                    .send(Effect::WithdrawNotification { prompt_id: old });
            }
            tracing::info!(%item_id, %prompt_id, kind = "planned_end", "prompt due");
            // A closed receiver only means nobody listens for effects.
            let _ = self.effects.send(Effect::Notify {
                prompt_id,
                item_id,
                kind: PromptKind::PlannedEnd,
            });
        }
        if now >= self.heartbeat_at {
            self.heartbeat(now).await;
        }
    }

    /// Records `last_alive_at`. Publishes only if the store status changed.
    async fn heartbeat(&mut self, now: Timestamp) {
        let status_before = self.status.clone();
        let batch = StoreBatch::from_iter([StoreOp::SetLastAlive { at: now }]);
        self.commit(batch).await;
        self.heartbeat_at = deadline_after(now);
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
        let now = self.clock.now();
        let running = running_of(&self.day);
        let schedule = reflow(&self.day, now);
        let next_item = if running.is_none() {
            next_item_of(&self.day, now)
        } else {
            None
        };
        let snapshot = Snapshot {
            rev: self.rev,
            day: Some(self.day.clone()),
            running,
            next_item,
            schedule,
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

/// The first item that is running.
fn running_of(day: &Day) -> Option<Running> {
    day.items().iter().find_map(PlanItem::running_view)
}

/// The next item to show while nothing runs: [`next_task`], so the top of the
/// interrupt stack, else the schedule's first pending or paused item.
fn next_item_of(day: &Day, now: Timestamp) -> Option<NextItem> {
    next_task(day, now)
        .and_then(|id| day.item(id))
        .map(|i| NextItem {
            item_id: i.id(),
            title: i.title().to_owned(),
        })
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
