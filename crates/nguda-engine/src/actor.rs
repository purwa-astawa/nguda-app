//! The actor task: startup with crash recovery, then one message or deadline at a
//! time (AD-3).
//!
//! Logging (AD-14): events carry ids, revs, dates and kinds only. Titles, items,
//! days and messages are never logged or `Debug`-printed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nguda_core::{
    CalendarChange, CalendarEntry, CalendarError, CalendarErrorKind, CalendarSource, Clock, Date,
    Day, DayLogKind, ExportRange, FetchedEvent, FileSink, IdGen, IsoWeek, ItemId, LockDecision,
    OAuthClient, OpenSegmentRef, PlanItem, PromptId, PromptKind, Random, Running, Schedule,
    SessionStore, Settings, Store, StoreBatch, StoreError, StoreOp, SyncBatch, SyncCursor,
    TimeZone, Timestamp, WorkEnd, add_event, added_refs, export_csv, hide_calendar, in_plan, merge,
    merge_tag_names, reconcile, reflow, report, sort_tag_names, week_report,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::batch::diff;
use crate::commands::{self, Reject, domain_kind};
use crate::{
    AddedOutcome, BACKOFF_MAX, CalendarEvent, CalendarOutcome, DayReply, Effect, EngineDeps,
    EngineMsg, EventsReply, GoogleAccount, GoogleClient, HEARTBEAT_EVERY, Inbox, JITTER, NextItem,
    POLL_EVERY, QueryError, Snapshot, StatusEntry, StatusKey, SyncRound, WAKE_LATENESS, WeekReply,
};

pub(crate) async fn run(
    deps: EngineDeps,
    mut rx: mpsc::UnboundedReceiver<Inbox>,
    inbox: mpsc::WeakUnboundedSender<Inbox>,
    snapshots: watch::Sender<Snapshot>,
    effects: mpsc::UnboundedSender<Effect>,
) {
    let mut actor = Actor::new(deps, inbox, snapshots, effects);
    actor.startup().await;
    loop {
        let deadline = actor.clock.sleep_until(actor.next_deadline());
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Inbox::Command(msg)) => actor.handle(msg).await,
                Some(Inbox::QueryDay { date, reply }) => actor.query_day(date, reply),
                Some(Inbox::QueryWeek { week, reply }) => actor.query_week(week, reply),
                Some(Inbox::ExportFinished { id, result }) => actor.export_finished(id, result),
                Some(Inbox::SignInFinished { id, result }) => actor.sign_in_finished(id, result),
                Some(Inbox::SignOutFinished { id, result }) => actor.sign_out_finished(id, result),
                Some(Inbox::ImportFinished { id, date, result }) => {
                    actor.import_finished(id, date, result).await;
                }
                Some(Inbox::QueryCalendarEvents { calendar_id, date, reply }) => {
                    actor.query_calendar_events(calendar_id, date, reply);
                }
                Some(Inbox::CalendarEventsFetched { id, date, result, reply }) => {
                    actor.calendar_events_fetched(id, date, result, reply);
                }
                Some(Inbox::EventAdded { date, result }) => {
                    actor.event_added(date, result).await;
                }
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
    files: Arc<dyn FileSink>,
    calendar: Arc<dyn CalendarSource>,
    random: Arc<dyn Random>,
    /// The last sign-in request id handed out; only its outcome applies.
    sign_in_issued: u64,
    /// The task of the latest sign-in issued, aborted by a sign-out.
    sign_in_task: Option<tokio::task::JoinHandle<()>>,
    /// The user's own OAuth client, as last committed or loaded. Never logged
    /// or published: it may hold a secret.
    google_client: Option<OAuthClient>,
    /// Whether the calendar source has any client to sign in with.
    client_available: bool,
    /// The OS login item as the shell last reported it; `None` until it does.
    /// Never persisted (AD-14).
    autostart: Option<bool>,
    /// The last sync request id handed out; only its result applies.
    import_issued: u64,
    /// The last calendar-events query id handed out; only its result is
    /// answered with events, older ones `Stale`.
    events_query_issued: u64,
    /// Each calendar's sync cursor, by calendar id, as last committed or loaded
    /// (AD-17). Never logged: it holds tokens.
    cursors: BTreeMap<String, SyncCursor>,
    /// The calendar table, in Google's listing order, as last committed or
    /// loaded (R2).
    calendars: Vec<CalendarEntry>,
    /// When the next calendar poll is due: `None` while a sync is in flight or
    /// while signed out.
    poll_at: Option<Timestamp>,
    /// Whether the latest sync issued has not answered yet.
    in_flight: bool,
    /// The task of the latest round issued, aborted when a newer one supersedes
    /// it so a dropped round makes no more requests.
    round_task: Option<tokio::task::JoinHandle<()>>,
    /// How many syncs in a row Google answered with 403 or 429.
    rate_limits: u32,
    /// The Google account state, as last learned from the calendar source.
    google: GoogleAccount,
    /// The actor's own inbox, weakly held: spawned tasks (exports, sign-in and
    /// sign-out, sync rounds, events queries, event adds) post their outcome here
    /// without keeping the engine alive.
    inbox: mpsc::WeakUnboundedSender<Inbox>,
    /// The last export request id handed out; ids start at 1.
    export_issued: u64,
    /// The newest export id whose outcome was applied; older outcomes are stale.
    export_applied: u64,
    tz: TimeZone,
    /// Today's plan, as last committed.
    day: Day,
    /// The work-end setting, as last committed or loaded.
    work_end: WorkEnd,
    /// Every tag name ever stored, sorted case-insensitively: loaded at startup and
    /// merged after each committed `SetTags`, so a failed commit adds nothing.
    known_tags: Vec<String>,
    /// The overflow list of the last publish, in schedule order.
    overflow: Vec<ItemId>,
    /// The prompt of the open overflow episode, if one is open: set when the list
    /// turns non-empty, withdrawn when it empties. Not persisted.
    overflow_prompt: Option<PromptId>,
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
        inbox: mpsc::WeakUnboundedSender<Inbox>,
        snapshots: watch::Sender<Snapshot>,
        effects: mpsc::UnboundedSender<Effect>,
    ) -> Self {
        let now = deps.clock.now();
        let day = Day::new(today(now, &deps.tz), tz_name(&deps.tz));
        Self {
            clock: deps.clock,
            store: deps.store,
            ids: deps.ids,
            files: deps.files,
            calendar: deps.calendar,
            random: deps.random,
            sign_in_issued: 0,
            sign_in_task: None,
            google_client: None,
            client_available: false,
            autostart: None,
            import_issued: 0,
            events_query_issued: 0,
            cursors: BTreeMap::new(),
            calendars: Vec::new(),
            poll_at: None,
            in_flight: false,
            round_task: None,
            rate_limits: 0,
            google: GoogleAccount::Unknown,
            inbox,
            export_issued: 0,
            export_applied: 0,
            tz: deps.tz,
            day,
            work_end: WorkEnd::DEFAULT,
            known_tags: Vec::new(),
            overflow: Vec::new(),
            overflow_prompt: None,
            status: BTreeMap::new(),
            rev: 0,
            heartbeat_at: deadline_after(now),
            prompted: BTreeSet::new(),
            open_prompts: BTreeMap::new(),
            snapshots,
            effects,
        }
    }

    /// Recovers a crash-left open segment, loads today, the settings, the known
    /// tag names, the sync cursors and the calendar table, publishes rev 1, and
    /// starts syncing today's calendar events.
    /// Runs before any message is handled.
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
            None => self.load_or_new(today, "load today").await,
        };
        let settings = match self.store.load_settings().await {
            Ok(settings) => settings,
            Err(e) => {
                self.store_failed("load settings", &e);
                Settings::default()
            }
        };
        self.work_end = settings.work_end;
        self.google_client = settings.google_client;
        self.calendar.set_client(self.google_client.clone());
        self.client_available = self.calendar.has_client();
        self.known_tags = match self.store.load_tag_names().await {
            Ok(mut names) => {
                sort_tag_names(&mut names);
                names
            }
            Err(e) => {
                self.store_failed("load tag names", &e);
                Vec::new()
            }
        };
        self.cursors = match self.store.load_sync_cursors().await {
            Ok(cursors) => cursors
                .into_iter()
                .map(|c| (c.calendar_id.clone(), c))
                .collect(),
            Err(e) => {
                self.store_failed("load sync cursors", &e);
                BTreeMap::new()
            }
        };
        self.calendars = match self.store.load_calendars().await {
            Ok(calendars) => calendars,
            Err(e) => {
                self.store_failed("load calendars", &e);
                Vec::new()
            }
        };
        self.heartbeat_at = deadline_after(now);
        self.publish();
        self.start_import();
    }

    /// The stored day `date`, or a new one in the engine's zone when it is missing
    /// or the load fails (which sets the `Store` status as `what`).
    async fn load_or_new(&mut self, date: Date, what: &'static str) -> Day {
        match self.store.load_day(date).await {
            Ok(Some(day)) => day,
            Ok(None) => Day::new(date, tz_name(&self.tz)),
            Err(e) => {
                self.store_failed(what, &e);
                Day::new(date, tz_name(&self.tz))
            }
        }
    }

    /// Makes the local date of `now` current when it is past today and nothing runs
    /// (AD-8): loads that day (or starts a new one), publishes it, and starts
    /// importing its calendar events. Only moves forward, possibly over several
    /// days. A running item keeps its day current.
    async fn roll_over_if_due(&mut self, now: Timestamp) {
        if running_of(&self.day).is_some() {
            return;
        }
        let date = today(now, &self.tz);
        if date <= self.day.date() {
            return;
        }
        let day = self.load_or_new(date, "load new day").await;
        tracing::info!(from = %self.day.date(), to = %date, "rolled over to a new day");
        self.day = day;
        self.publish();
        self.start_import();
    }

    /// Starts a sync round off the actor (AD-17) under a fresh request id and
    /// today's date, superseding any round still running. The task lists the
    /// calendars, reconciles them with the stored table, then syncs each included
    /// calendar in turn: incrementally from a cursor issued for today, else a
    /// full fetch of today's local day. It posts the round back as
    /// [`Inbox::ImportFinished`]. The superseded round's task is aborted. No poll
    /// is due while it is in flight.
    fn start_import(&mut self) {
        self.import_issued += 1;
        let id = self.import_issued;
        let date = self.day.date();
        self.poll_at = None;
        if let Some(task) = self.round_task.take() {
            task.abort();
        }
        let Some((from, to)) = day_window(date, &self.tz) else {
            self.in_flight = false;
            tracing::warn!(%date, "no import window for the day");
            return;
        };
        self.in_flight = true;
        // A cursor from another day never resumes: a new day starts with full fetches.
        let tokens: BTreeMap<String, String> = self
            .cursors
            .values()
            .filter(|c| c.date == date)
            .map(|c| (c.calendar_id.clone(), c.token.clone()))
            .collect();
        let stored = self.calendars.clone();
        // The added items to refresh are chosen after the listing reconciles the
        // table, so the task gets today's plan as of now.
        let day = self.day.clone();
        let calendar = self.calendar.clone();
        let inbox = self.inbox.clone();
        tracing::debug!(id, %date, cursors = tokens.len(), "calendar sync round started");
        self.round_task = Some(tokio::spawn(async move {
            let result = sync_round(calendar.as_ref(), &stored, &day, &tokens, from, to).await;
            // The engine may have stopped meanwhile; nothing to report to then.
            if let Some(inbox) = inbox.upgrade() {
                let _ = inbox.send(Inbox::ImportFinished { id, date, result });
            }
        }));
    }

    /// Applies a sync round (AD-9, AD-17), unless it is stale: an id other than
    /// the latest issued, or a date that is no longer today; a stale round
    /// changes nothing at all.
    ///
    /// A listing failure fails the whole round: an expired session signs out
    /// (see [`Self::session_expired`]), "not signed in" (or no client) marks the
    /// account signed out and schedules no poll, a 403/429 grows the
    /// backoff and sets `Sync`, any other failure sets `Sync`; the table and the
    /// plan stay as they are. Otherwise the account is signed in, the successful
    /// calendars are applied as one batch (see [`Self::apply_round`]), and then, in
    /// precedence: an expired session signs out; a `410` on an incremental sync
    /// starts a new round at once
    /// (that calendar's cursor already cleared); else a 403/429 grows the backoff
    /// and sets `Sync`; else any other failure sets `Sync`; else `Sync` and the
    /// backoff are cleared. Every outcome but a new round or a sign-out schedules
    /// the next poll. Publishes only on a visible change. Logs ids, dates, kinds
    /// and counts only (AD-14).
    async fn import_finished(
        &mut self,
        id: u64,
        date: Date,
        result: Result<SyncRound, CalendarError>,
    ) {
        if id != self.import_issued || date != self.day.date() {
            tracing::debug!(id, latest = self.import_issued, %date, "stale import dropped");
            return;
        }
        self.in_flight = false;
        let status_before = self.status.clone();
        let google_before = self.google;
        let mut changed = false;
        match result {
            Ok(round) => {
                self.set_google(GoogleAccount::SignedIn);
                let failures = self.apply_round(id, date, round).await;
                changed = failures.changed;
                if failures.expired {
                    self.session_expired(id);
                } else if failures.gone {
                    tracing::info!(id, "calendar sync token expired: fetching again");
                    self.start_import();
                } else if failures.rate_limited {
                    self.rate_limited(id);
                } else if let Some(message) = failures.other {
                    self.status.insert(
                        StatusKey::Sync,
                        format!("Calendar import failed: {message}"),
                    );
                    self.schedule_poll();
                } else {
                    self.status.remove(&StatusKey::Sync);
                    self.rate_limits = 0;
                    self.schedule_poll();
                }
            }
            Err(e) if e.kind == CalendarErrorKind::SessionExpired => self.session_expired(id),
            Err(e) if e.kind == CalendarErrorKind::NotSignedIn => {
                tracing::debug!(id, "calendar import: not signed in");
                self.rate_limits = 0;
                self.set_google(GoogleAccount::SignedOut);
            }
            Err(e) if e.kind == CalendarErrorKind::RateLimited => {
                if self.google == GoogleAccount::Unknown {
                    self.set_google(GoogleAccount::SignedIn);
                }
                self.rate_limited(id);
            }
            Err(e) => {
                tracing::warn!(id, kind = error_kind(&e), "calendar listing failed");
                self.status.insert(
                    StatusKey::Sync,
                    format!("Calendar import failed: {}", e.message),
                );
                if e.kind == CalendarErrorKind::NoClient {
                    self.set_google(GoogleAccount::SignedOut);
                } else {
                    if self.google == GoogleAccount::Unknown {
                        // A stored session exists; only this fetch failed.
                        self.set_google(GoogleAccount::SignedIn);
                    }
                    self.schedule_poll();
                }
            }
        }
        if changed || self.status != status_before || self.google != google_before {
            self.publish();
        }
    }

    /// Handles a session Google no longer honours (the source already forgot and
    /// deleted it): unless already signed out (a sign-out came first), marks
    /// the account signed out, asks to sign in again in `Sync`, and resets the
    /// backoff. Schedules no poll. Publishing is the caller's.
    fn session_expired(&mut self, id: u64) {
        self.poll_at = None;
        if self.google == GoogleAccount::SignedOut {
            return;
        }
        tracing::warn!(id, "google sign-in expired or revoked");
        self.rate_limits = 0;
        self.set_google(GoogleAccount::SignedOut);
        self.status.insert(
            StatusKey::Sync,
            "Your Google sign-in expired or was revoked. Sign in again.".to_owned(),
        );
    }

    /// Expires the session ([`Self::session_expired`]) and publishes when that
    /// changed the account or the status.
    fn expire_and_publish(&mut self, id: u64) {
        let before = (self.google, self.status.clone());
        self.session_expired(id);
        if before != (self.google, self.status.clone()) {
            self.publish();
        }
    }

    /// Grows the rate-limit backoff, sets `Sync`, and schedules the next poll.
    fn rate_limited(&mut self, id: u64) {
        self.rate_limits = self.rate_limits.saturating_add(1);
        tracing::warn!(id, limits = self.rate_limits, "calendar sync rate limited");
        self.status.insert(
            StatusKey::Sync,
            "Google is limiting requests; syncing again later".to_owned(),
        );
        self.schedule_poll();
    }

    /// Applies a listed round to a copy of today at `now` and commits, as one
    /// batch, the calendar table (when it changed), the plan diff, a
    /// `CalendarChanged` entry per touched item, and every cursor change: each
    /// successful calendar's new token for today (or its cursor removed when
    /// Google sent none), the cursor of each calendar that left the list, and the
    /// cursor of each calendar whose incremental sync hit a `410`. A calendar
    /// whose merge the domain rejects is skipped and counts as a failure. Swaps
    /// the day, the table and the cursors in only after the commit; a dead
    /// token is forgotten even if the commit failed. Nothing changed commits
    /// nothing.
    async fn apply_round(&mut self, id: u64, date: Date, round: SyncRound) -> RoundFailures {
        let SyncRound {
            calendars,
            outcomes,
            added,
        } = round;
        let now = self.clock.now();
        let mut failures = RoundFailures::default();
        let mut next = self.day.clone();
        let mut cursors = self.cursors.clone();
        let mut touched: Vec<ItemId> = Vec::new();
        let mut fetched = 0;
        let mut dead: Vec<String> = Vec::new();
        let ids = self.ids.clone();
        for CalendarOutcome {
            calendar_id,
            incremental,
            result,
        } in outcomes
        {
            match result {
                Ok(SyncBatch {
                    calendar_id: _,
                    changes,
                    next_token,
                }) => {
                    let mut merged = next.clone();
                    match merge(&mut merged, &changes, now, || ids.new_item_id()) {
                        Ok(t) => {
                            next = merged;
                            fetched += changes.len();
                            push_new(&mut touched, t);
                            match next_token {
                                Some(token) => {
                                    let cursor = SyncCursor {
                                        calendar_id: calendar_id.clone(),
                                        token,
                                        date,
                                    };
                                    cursors.insert(calendar_id, cursor);
                                }
                                None => {
                                    cursors.remove(&calendar_id);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                id,
                                %date,
                                kind = domain_kind(&e),
                                "calendar merge rejected"
                            );
                            failures.other.get_or_insert_with(|| e.to_string());
                        }
                    }
                }
                Err(e) if e.kind == CalendarErrorKind::SessionExpired => {
                    failures.expired = true;
                }
                Err(e) if e.kind == CalendarErrorKind::Gone && incremental => {
                    cursors.remove(&calendar_id);
                    dead.push(calendar_id);
                    failures.gone = true;
                }
                Err(e) if e.kind == CalendarErrorKind::RateLimited => {
                    failures.rate_limited = true;
                }
                Err(e) => {
                    tracing::warn!(id, kind = error_kind(&e), "calendar sync failed");
                    failures.other.get_or_insert(e.message);
                }
            }
        }
        // Added events of calendars not included: merged like any change (R4).
        // `None` (not found, skipped, or not today) never deletes (AD-9).
        let refreshed = added.len();
        for AddedOutcome {
            calendar_id,
            event_id,
            result,
        } in added
        {
            match result {
                // Only the event asked for: anything else is not this item's.
                Ok(Some(change)) if names(&change, &calendar_id, &event_id) => {
                    let mut merged = next.clone();
                    match merge(&mut merged, &[change], now, || ids.new_item_id()) {
                        Ok(t) => {
                            next = merged;
                            push_new(&mut touched, t);
                        }
                        Err(e) => {
                            tracing::warn!(id, %date, kind = domain_kind(&e), "added event merge rejected");
                            failures.other.get_or_insert_with(|| e.to_string());
                        }
                    }
                }
                Ok(_) => {}
                Err(e) if e.kind == CalendarErrorKind::SessionExpired => {
                    failures.expired = true;
                }
                Err(e) if e.kind == CalendarErrorKind::RateLimited => {
                    failures.rate_limited = true;
                }
                Err(e) => {
                    tracing::warn!(id, kind = error_kind(&e), "added event refresh failed");
                    failures.other.get_or_insert(e.message);
                }
            }
        }
        // A calendar no longer listed keeps no cursor; its items stay (AD-9).
        cursors.retain(|calendar_id, _| calendars.iter().any(|c| &c.id == calendar_id));
        tracing::info!(
            id,
            %date,
            calendars = calendars.len(),
            fetched,
            refreshed,
            count = touched.len(),
            "calendar sync round merged"
        );
        let day_changed = next != self.day;
        let table_changed = calendars != self.calendars;
        let mut ops = if day_changed {
            diff(&self.day, &next)
        } else {
            StoreBatch::new()
        };
        if table_changed {
            ops.push(StoreOp::SetCalendars {
                calendars: calendars.clone(),
            });
        }
        ops.extend(
            touched.iter().map(|&item_id| {
                commands::log_op(date, now, DayLogKind::CalendarChanged { item_id })
            }),
        );
        ops.extend(cursor_ops(&self.cursors, &cursors));
        let committed =
            !ops.ops().is_empty() && self.swap_after_commit(next, self.work_end, ops).await;
        if committed {
            self.calendars = calendars;
            self.cursors = cursors;
            failures.changed = day_changed || table_changed;
        } else {
            // Even if the clear failed to commit, never reuse a dead token.
            for calendar_id in dead {
                self.cursors.remove(&calendar_id);
            }
        }
        failures
    }

    /// Includes exactly `calendar_ids` of the table (R2), ignoring unknown ids.
    /// The same selection changes nothing: no commit, round, or publish.
    /// Otherwise commits in one batch the new table, today's untracked pending
    /// items of each newly un-included calendar hidden (a `CalendarChanged`
    /// entry each), and those calendars' cursors cleared, so a re-include always
    /// fetches in full; then publishes and starts a new round at once,
    /// superseding any in flight. A failed commit changes nothing and publishes
    /// the `Store` status. Logs counts only (AD-14).
    async fn set_included_calendars(&mut self, calendar_ids: &[String]) {
        let calendars: Vec<CalendarEntry> = self
            .calendars
            .iter()
            .map(|c| CalendarEntry {
                included: calendar_ids.contains(&c.id),
                ..c.clone()
            })
            .collect();
        if calendars == self.calendars {
            tracing::debug!("calendar selection unchanged");
            return;
        }
        let now = self.clock.now();
        let date = self.day.date();
        let mut next = self.day.clone();
        let mut cursors = self.cursors.clone();
        let mut hidden: Vec<ItemId> = Vec::new();
        let dropped = self
            .calendars
            .iter()
            .zip(&calendars)
            .filter(|(old, new)| old.included && !new.included)
            .map(|(old, _)| old.id.clone());
        for calendar_id in dropped {
            match hide_calendar(&mut next, &calendar_id, now) {
                Ok(ids) => hidden.extend(ids),
                Err(e) => {
                    tracing::warn!(kind = domain_kind(&e), "calendar hide rejected");
                    self.status
                        .insert(StatusKey::Sync, format!("Calendar selection failed: {e}"));
                    self.publish();
                    return;
                }
            }
            cursors.remove(&calendar_id);
        }
        let mut ops = if next == self.day {
            StoreBatch::new()
        } else {
            diff(&self.day, &next)
        };
        ops.push(StoreOp::SetCalendars {
            calendars: calendars.clone(),
        });
        ops.extend(
            hidden.iter().map(|&item_id| {
                commands::log_op(date, now, DayLogKind::CalendarChanged { item_id })
            }),
        );
        ops.extend(cursor_ops(&self.cursors, &cursors));
        if !self.swap_after_commit(next, self.work_end, ops).await {
            self.publish();
            return;
        }
        let included = calendars.iter().filter(|c| c.included).count();
        tracing::info!(
            included,
            hidden = hidden.len(),
            "calendar selection changed"
        );
        self.calendars = calendars;
        self.cursors = cursors;
        self.publish();
        self.start_import();
    }

    /// Schedules the next poll after a sync result: `POLL_EVERY`, or after `n`
    /// consecutive rate limits `min(POLL_EVERY·2^n, BACKOFF_MAX)`, each jittered
    /// by ±`JITTER` from the [`Random`] port.
    fn schedule_poll(&mut self) {
        let base = backoff_base(self.rate_limits);
        let delay = jittered(base, self.random.unit());
        self.poll_at = Some(
            self.clock
                .now()
                .checked_add(delay)
                .unwrap_or(Timestamp::MAX),
        );
        tracing::debug!(delay_secs = delay.as_secs(), "calendar poll scheduled");
    }

    /// Sets the account state, except that a sign-in in progress stays shown until
    /// its own outcome arrives.
    fn set_google(&mut self, state: GoogleAccount) {
        if self.google != GoogleAccount::SigningIn {
            self.google = state;
        }
    }

    /// Starts an interactive sign-in off the actor (AD-17): marks the account
    /// signing in, publishes, and spawns the sign-in, whose outcome comes back as
    /// [`Inbox::SignInFinished`] under a fresh request id.
    fn sign_in(&mut self) {
        self.sign_in_issued += 1;
        let id = self.sign_in_issued;
        self.google = GoogleAccount::SigningIn;
        self.publish();
        let calendar = self.calendar.clone();
        let inbox = self.inbox.clone();
        tracing::info!(id, "google sign-in started");
        if let Some(task) = self.sign_in_task.take() {
            task.abort();
        }
        self.sign_in_task = Some(tokio::spawn(async move {
            let result = calendar.sign_in().await;
            // The engine may have stopped meanwhile; nothing to report to then.
            if let Some(inbox) = inbox.upgrade() {
                let _ = inbox.send(Inbox::SignInFinished { id, result });
            }
        }));
    }

    /// Signs out (R1): drops any sign-in or round in flight (aborting their
    /// tasks, so their outcomes never apply), schedules no poll, resets the
    /// backoff, marks the account signed out, clears `Sync` and `SecretStore`,
    /// and publishes. The source forgets the session at once; deleting the
    /// stored one runs off the actor (AD-17) and comes back as
    /// [`Inbox::SignOutFinished`]. Plan, calendar table, cursors and day log are
    /// untouched.
    fn sign_out(&mut self) {
        self.sign_in_issued += 1;
        if let Some(task) = self.sign_in_task.take() {
            task.abort();
        }
        self.import_issued += 1;
        if let Some(task) = self.round_task.take() {
            task.abort();
        }
        self.in_flight = false;
        self.poll_at = None;
        self.rate_limits = 0;
        self.google = GoogleAccount::SignedOut;
        self.status.remove(&StatusKey::Sync);
        self.status.remove(&StatusKey::SecretStore);
        self.publish();
        let id = self.sign_in_issued;
        // Memory only: the source forgets the session before the future runs.
        let delete = self.calendar.sign_out();
        let inbox = self.inbox.clone();
        tracing::info!(id, "google sign-out started");
        tokio::spawn(async move {
            let result = delete.await;
            // The engine may have stopped meanwhile; nothing to report to then.
            if let Some(inbox) = inbox.upgrade() {
                let _ = inbox.send(Inbox::SignOutFinished { id, result });
            }
        });
    }

    /// Applies a sign-out's deletion: a failure sets `SecretStore`, unless a
    /// newer sign-in was issued since. Logs the kind only (AD-14).
    fn sign_out_finished(&mut self, id: u64, result: Result<(), CalendarError>) {
        if id != self.sign_in_issued {
            tracing::debug!(id, latest = self.sign_in_issued, "stale sign-out dropped");
            return;
        }
        match result {
            Ok(()) => tracing::info!(id, "google sign-out finished"),
            Err(e) => {
                tracing::warn!(
                    id,
                    kind = error_kind(&e),
                    "google sign-out could not delete"
                );
                self.status.insert(
                    StatusKey::SecretStore,
                    format!(
                        "Could not remove the Google sign-in from the keyring: {}",
                        e.message
                    ),
                );
                self.publish();
            }
        }
    }

    /// Sets the user's own OAuth client (R1, AD-13), only while signed out: the
    /// id trimmed and non-blank, a blank secret dropped. The same client changes
    /// nothing. Otherwise commits it, hands it to the source, and publishes. A
    /// rejection sets `CommandRejected`. Never logs the client.
    async fn set_google_client(&mut self, client: Option<OAuthClient>) {
        const CMD: &str = commands::SET_GOOGLE_CLIENT;
        if self.google != GoogleAccount::SignedOut {
            let reject = Reject::invalid(CMD, "sign out before changing the Google client");
            self.reject(CMD, None, &reject);
            return;
        }
        let client = match client {
            None => None,
            Some(OAuthClient { id, secret }) => {
                let id = id.trim().to_owned();
                if id.is_empty() {
                    let reject = Reject::invalid(CMD, "the client ID must not be blank");
                    self.reject(CMD, None, &reject);
                    return;
                }
                let secret = secret
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                Some(OAuthClient { id, secret })
            }
        };
        let status_before = self.status.clone();
        self.status.remove(&StatusKey::CommandRejected);
        if client == self.google_client {
            tracing::debug!("google client unchanged");
            if self.status != status_before {
                self.publish();
            }
            return;
        }
        let batch = StoreBatch::from_iter([StoreOp::SetGoogleClient {
            client: client.clone(),
        }]);
        if self.commit(batch).await {
            tracing::info!(custom = client.is_some(), "google client changed");
            self.calendar.set_client(client.clone());
            self.google_client = client;
            self.client_available = self.calendar.has_client();
        }
        self.publish();
    }

    /// Applies the latest sign-in's outcome; an older one is dropped (AD-17).
    /// Success marks the account signed in, clears `Sync`, sets `SecretStore`
    /// when the session lives in memory only (else clears it), and imports
    /// today's events; failure marks it signed out and sets `Sync` with the
    /// message only.
    fn sign_in_finished(&mut self, id: u64, result: Result<SessionStore, CalendarError>) {
        if id != self.sign_in_issued {
            tracing::debug!(id, latest = self.sign_in_issued, "stale sign-in dropped");
            return;
        }
        self.sign_in_task = None;
        match result {
            Ok(kept) => {
                tracing::info!(
                    id,
                    session_only = kept == SessionStore::SessionOnly,
                    "google sign-in succeeded"
                );
                self.google = GoogleAccount::SignedIn;
                self.status.remove(&StatusKey::Sync);
                match kept {
                    SessionStore::Stored => {
                        self.status.remove(&StatusKey::SecretStore);
                    }
                    SessionStore::SessionOnly => {
                        self.status.insert(
                            StatusKey::SecretStore,
                            "No keyring is available: the Google sign-in lasts until nguda quits."
                                .to_owned(),
                        );
                    }
                }
                self.publish();
                self.start_import();
            }
            Err(e) => {
                tracing::warn!(id, kind = error_kind(&e), "google sign-in failed");
                self.google = GoogleAccount::SignedOut;
                let message = match e.kind {
                    CalendarErrorKind::NoClient => e.message,
                    _ => format!("Google sign-in failed: {}", e.message),
                };
                self.status.insert(StatusKey::Sync, message);
                self.publish();
            }
        }
    }

    /// Answers a calendar-events query (R4, AD-16, AD-17) under a fresh request
    /// id: a `date` other than today is answered `Stale` at once; otherwise the
    /// listing of today's window is spawned, and its result comes back as
    /// [`Inbox::CalendarEventsFetched`] with the reply. Read-only. Logs the id
    /// and date only (AD-14).
    fn query_calendar_events(
        &mut self,
        calendar_id: String,
        date: Date,
        reply: oneshot::Sender<EventsReply>,
    ) {
        self.events_query_issued += 1;
        let id = self.events_query_issued;
        let window = day_window(date, &self.tz);
        let (true, Some((from, to))) = (date == self.day.date(), window) else {
            tracing::debug!(id, %date, "calendar events query stale");
            // The asker may have gone away; nothing to do then.
            let _ = reply.send(Err(QueryError::Stale));
            return;
        };
        let calendar = self.calendar.clone();
        let inbox = self.inbox.clone();
        tracing::debug!(id, %date, "calendar events queried");
        tokio::spawn(async move {
            let result = calendar.list_events(calendar_id, from, to).await;
            match inbox.upgrade() {
                Some(inbox) => {
                    let _ = inbox.send(Inbox::CalendarEventsFetched {
                        id,
                        date,
                        result,
                        reply,
                    });
                }
                // The engine stopped meanwhile: the asker sees it closed.
                None => drop(reply),
            }
        });
    }

    /// Answers a calendar-events query with its listing: `Stale` when a newer
    /// query was issued or `date` is no longer today, the failure's kind on an
    /// error, else the events sorted by start, each marked whether today's plan
    /// has it ([`in_plan`]). Changes nothing, except that an expired session
    /// (even in a stale answer) signs out and publishes.
    fn calendar_events_fetched(
        &mut self,
        id: u64,
        date: Date,
        result: Result<Vec<FetchedEvent>, CalendarError>,
        reply: oneshot::Sender<EventsReply>,
    ) {
        if let Err(e) = &result
            && e.kind == CalendarErrorKind::SessionExpired
        {
            self.expire_and_publish(id);
        }
        let answer = if id != self.events_query_issued || date != self.day.date() {
            tracing::debug!(id, latest = self.events_query_issued, %date, "stale events dropped");
            Err(QueryError::Stale)
        } else {
            match result {
                Ok(events) => {
                    tracing::debug!(id, count = events.len(), "calendar events listed");
                    Ok(calendar_events(&self.day, events))
                }
                Err(e) => {
                    tracing::warn!(id, kind = error_kind(&e), "calendar events query failed");
                    Err(QueryError::Calendar(e.kind))
                }
            }
        };
        // The asker may have gone away; nothing to do then.
        let _ = reply.send(answer);
    }

    /// Starts adding one event to today (R4) off the actor (AD-17): fetches it
    /// with `events.get` in today's window and posts the result back as
    /// [`Inbox::EventAdded`] for today's date. Touches no state here. Logs the
    /// date only (AD-14).
    fn add_calendar_event(&mut self, calendar_id: String, event_id: String) {
        let date = self.day.date();
        let Some((from, to)) = day_window(date, &self.tz) else {
            tracing::warn!(%date, "no add window for the day");
            return;
        };
        let calendar = self.calendar.clone();
        let inbox = self.inbox.clone();
        tracing::info!(%date, "calendar event add started");
        tokio::spawn(async move {
            let result = calendar.get_event(calendar_id, event_id, from, to).await;
            // The engine may have stopped meanwhile; nothing to report to then.
            if let Some(inbox) = inbox.upgrade() {
                let _ = inbox.send(Inbox::EventAdded { date, result });
            }
        });
    }

    /// Applies an added event's fetch (R4), unless `date` is no longer today. A
    /// timed event of today is added to a copy of today ([`add_event`]) and, when
    /// that changes the plan, committed with a `CalendarChanged` entry in one
    /// batch; a key already live changes nothing. A missing, skipped, or
    /// cancelled event, a fetch failure, or a domain rejection sets `Sync`
    /// ("Calendar event not added: ...") and changes nothing. Publishes on a
    /// change. Logs kinds only (AD-14).
    async fn event_added(
        &mut self,
        date: Date,
        result: Result<Option<CalendarChange>, CalendarError>,
    ) {
        if date != self.day.date() {
            tracing::debug!(%date, "stale event add dropped");
            return;
        }
        let event = match result {
            Ok(Some(CalendarChange::Upsert(event))) => event,
            Ok(Some(CalendarChange::Cancelled(_))) => {
                return self.add_failed("the event is cancelled");
            }
            Ok(None) => {
                return self.add_failed("not found, all-day, declined, or not today");
            }
            Err(e) if e.kind == CalendarErrorKind::SessionExpired => {
                self.expire_and_publish(0);
                return;
            }
            Err(e) => {
                tracing::warn!(kind = error_kind(&e), "calendar event fetch failed");
                return self.add_failed(&e.message);
            }
        };
        let now = self.clock.now();
        let mut next = self.day.clone();
        let ids = self.ids.clone();
        let item_id = match add_event(&mut next, &event, || ids.new_item_id()) {
            Ok(Some(item_id)) => item_id,
            Ok(None) => {
                tracing::debug!("calendar event already in the plan");
                return;
            }
            Err(e) => {
                tracing::warn!(kind = domain_kind(&e), "calendar event add rejected");
                return self.add_failed(&e.to_string());
            }
        };
        let mut batch = diff(&self.day, &next);
        batch.push(commands::log_op(
            date,
            now,
            DayLogKind::CalendarChanged { item_id },
        ));
        if self.swap_after_commit(next, self.work_end, batch).await {
            tracing::info!(%item_id, "calendar event added");
        }
        self.publish();
    }

    /// Sets `Sync` for an add that changed nothing, and publishes.
    fn add_failed(&mut self, reason: &str) {
        self.status.insert(
            StatusKey::Sync,
            format!("Calendar event not added: {reason}"),
        );
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
        batch.push(commands::log_op(
            segment.date,
            now,
            DayLogKind::Recovered {
                item_id: segment.item_id,
            },
        ));
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

    /// Answers a day query without waiting (AD-4, AD-16): notes `now`, clones
    /// today's committed day when `date` is today, and spawns the loads and the
    /// report. Read-only: no commit, rev bump, publish, or status change. A dropped
    /// reply is ignored. Logs the date only (AD-14).
    fn query_day(&self, date: Date, reply: oneshot::Sender<DayReply>) {
        let now = self.clock.now();
        let today = (date == self.day.date()).then(|| self.day.clone());
        let store = self.store.clone();
        tracing::debug!(%date, live = today.is_some(), "day queried");
        tokio::spawn(async move {
            let result = build_report(store.as_ref(), date, today, now).await;
            if let Err(e) = &result {
                tracing::warn!(%date, error = %e, "day query failed");
            }
            // The asker may have gone away; nothing to do then.
            let _ = reply.send(result);
        });
    }

    /// Answers a week query without waiting (AD-4, AD-16): notes `now`, clones
    /// today's committed day when today is in `week`, and spawns the reports of
    /// the week's dates up to today and the gap. Dates after today are skipped: a
    /// stored future day (such as tomorrow's, made by carry-over) has not happened
    /// yet, so it is neither counted nor excluded as unlocked. Read-only, like
    /// [`Self::query_day`]. A dropped reply is ignored. Logs the week only (AD-14).
    fn query_week(&self, week: IsoWeek, reply: oneshot::Sender<WeekReply>) {
        let now = self.clock.now();
        let today_date = self.day.date();
        let mut today = week.contains(today_date).then(|| self.day.clone());
        let store = self.store.clone();
        let (year, number) = (week.year(), week.week());
        tracing::debug!(year, week = number, live = today.is_some(), "week queried");
        tokio::spawn(async move {
            let result = async {
                let mut reports = Vec::with_capacity(7);
                for date in week.days().into_iter().filter(|d| *d <= today_date) {
                    let live = today.take_if(|d| d.date() == date);
                    if let Some(r) = build_report(store.as_ref(), date, live, now).await? {
                        reports.push(r);
                    }
                }
                Ok(week_report(week, &reports))
            }
            .await;
            if let Err(e) = &result {
                tracing::warn!(year, week = number, error = %e, "week query failed");
            }
            // The asker may have gone away; nothing to do then.
            let _ = reply.send(result);
        });
    }

    /// Starts a CSV export without waiting (AD-4, AD-16, AD-17): notes `now` and
    /// today, clones today's committed day, and spawns a task that resolves the
    /// range's dates up to today, builds their reports, turns them into CSV, writes
    /// it through the file sink on a blocking thread, and posts the outcome back
    /// as [`Inbox::ExportFinished`] under a fresh request id. Touches no other
    /// state here. Logs the range kind and week only, never the path (AD-14).
    fn export_csv(&mut self, path: PathBuf, range: ExportRange) {
        self.export_issued += 1;
        let id = self.export_issued;
        let now = self.clock.now();
        let today_date = self.day.date();
        let mut today = Some(self.day.clone());
        let store = self.store.clone();
        let files = self.files.clone();
        let inbox = self.inbox.clone();
        let (kind, year, number) = range_fields(range);
        tracing::info!(range = kind, year, week = number, "export started");
        tokio::spawn(async move {
            let result = async {
                let dates = export_dates(store.as_ref(), range, today_date).await?;
                let mut reports = Vec::with_capacity(dates.len());
                for date in dates {
                    let live = today.take_if(|d| d.date() == date);
                    let built = build_report(store.as_ref(), date, live, now).await;
                    if let Some(r) = built.map_err(|e| e.message)? {
                        reports.push(r);
                    }
                }
                let csv = export_csv(&reports);
                let rows = csv.rows;
                tokio::task::spawn_blocking(move || files.write(&path, csv.contents.as_bytes()))
                    .await
                    .map_err(|_| "the file write task failed".to_owned())?
                    .map_err(|e| e.message)?;
                Ok::<_, String>(rows)
            }
            .await;
            match &result {
                Ok(rows) => {
                    tracing::info!(range = kind, year, week = number, rows, "export written");
                }
                Err(_) => tracing::warn!(range = kind, year, week = number, "export failed"),
            }
            // The engine may have stopped meanwhile; nothing to report to then.
            if let Some(inbox) = inbox.upgrade() {
                let _ = inbox.send(Inbox::ExportFinished { id, result });
            }
        });
    }

    /// Applies an export's outcome (AD-14): success clears the `Export` status
    /// (publishing only if it was set) and emits `ExportDone`; failure sets it and
    /// publishes. No other status, the plan, or the committed state changes. An
    /// outcome older than the newest one applied is stale and dropped (AD-17).
    fn export_finished(&mut self, id: u64, result: Result<usize, String>) {
        if id < self.export_applied {
            tracing::debug!(
                id,
                newest = self.export_applied,
                "stale export outcome dropped"
            );
            return;
        }
        self.export_applied = id;
        match result {
            Ok(rows) => {
                if self.status.remove(&StatusKey::Export).is_some() {
                    self.publish();
                }
                // A closed receiver only means nobody listens for effects.
                let _ = self.effects.send(Effect::ExportDone { rows });
            }
            Err(message) => {
                self.status
                    .insert(StatusKey::Export, format!("Export failed: {message}"));
                self.publish();
            }
        }
    }

    /// One command. Rollover runs first, so a command carrying a pre-rollover rev
    /// is stale, and again after a committed change (a Pause after midnight ends
    /// the old day). Carry-over needs tomorrow's day, so it has its own path.
    async fn handle(&mut self, msg: EngineMsg) {
        // Changes nothing but the mirrored login item, so no rollover either.
        if let EngineMsg::AutostartObserved { enabled } = msg {
            self.autostart_observed(enabled);
            return;
        }
        self.roll_over_if_due(self.clock.now()).await;
        let changed = match msg {
            EngineMsg::CarryOverToTomorrow { expected_rev } => self.carry_over(expected_rev).await,
            EngineMsg::ExportCsv { path, range } => {
                self.export_csv(path, range);
                false
            }
            EngineMsg::SignIn => {
                self.sign_in();
                false
            }
            EngineMsg::SignOut => {
                self.sign_out();
                false
            }
            EngineMsg::SetGoogleClient { client } => {
                self.set_google_client(client).await;
                false
            }
            EngineMsg::SetIncludedCalendars { calendar_ids } => {
                self.set_included_calendars(&calendar_ids).await;
                false
            }
            EngineMsg::AddCalendarEvent {
                calendar_id,
                event_id,
            } => {
                self.add_calendar_event(calendar_id, event_id);
                false
            }
            msg => self.handle_plan(msg).await,
        };
        if changed {
            self.roll_over_if_due(self.clock.now()).await;
        }
    }

    /// Mirrors the shell's read of the OS login item (AD-14): publishes only when
    /// it changed. No store op, no effect, no status change.
    fn autostart_observed(&mut self, enabled: bool) {
        if self.autostart == Some(enabled) {
            return;
        }
        self.autostart = Some(enabled);
        self.publish();
    }

    /// A command on today's day and the work-end: rev check, validate and apply on
    /// copies of the day and the work-end, commit `diff + extras` unless both are
    /// unchanged, then swap. A rejection sets `CommandRejected`; anything that
    /// passes validation clears it. Publishes when the day, the work-end, or the
    /// status changed, and always on a rejection. Returns whether a change was
    /// committed.
    async fn handle_plan(&mut self, msg: EngineMsg) -> bool {
        let status_before = self.status.clone();
        let name = commands::name(&msg);
        let item_id = commands::target(&msg);
        // `msg` moves into `apply`, so note an extension's and a retag's target first.
        let extend_target = match &msg {
            EngineMsg::ExtendTask { item_id, .. } => Some(*item_id),
            _ => None,
        };
        let tags_target = match &msg {
            EngineMsg::SetTags { item_id, .. } => Some(*item_id),
            _ => None,
        };
        let now = self.clock.now();
        let mut next = self.day.clone();
        let mut work_end = self.work_end;
        let applied = commands::apply(
            msg,
            &mut next,
            &mut work_end,
            now,
            self.ids.as_ref(),
            self.rev,
            &self.known_tags,
        );
        let changed = match applied {
            Err(reject) => {
                self.reject(name, item_id, &reject);
                self.ask_lock(name, item_id, &reject);
                return false;
            }
            Ok(extras) => {
                self.status.remove(&StatusKey::CommandRejected);
                if next == self.day && work_end == self.work_end {
                    false
                } else {
                    // A settings-only change touches no day row.
                    let mut batch = if next == self.day {
                        StoreBatch::new()
                    } else {
                        diff(&self.day, &next)
                    };
                    batch.extend(extras);
                    self.swap_after_commit(next, work_end, batch).await
                }
            }
        };
        if changed && let Some(item) = tags_target.and_then(|id| self.day.item(id)) {
            merge_tag_names(&mut self.known_tags, item.tags());
        }
        if changed || self.status != status_before {
            self.publish();
        }
        if changed {
            self.withdraw_prompt(extend_target);
        }
        changed
    }

    /// Logs a rejected command, sets `CommandRejected`, and publishes.
    fn reject(&mut self, name: &'static str, item_id: Option<ItemId>, reject: &Reject) {
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
    }

    /// After a choiceless `Start` was rejected on an undecided day, asks surfaces
    /// to get a lock choice for its item (AD-7). Insert-now has no item to name.
    fn ask_lock(&self, name: &'static str, item_id: Option<ItemId>, reject: &Reject) {
        let (commands::START, Some(item_id), Reject::LockUndecided { .. }) =
            (name, item_id, reject)
        else {
            return;
        };
        tracing::info!(%item_id, "asking for a lock choice");
        // A closed receiver only means nobody listens for effects.
        let _ = self.effects.send(Effect::AskLockOnStart { item_id });
    }

    /// Carries the current overflow list over to tomorrow (AD-6), in one batch with
    /// tomorrow's day (loaded, or new in today's zone) and the `CarriedOver`
    /// entries. A stale rev or an empty list is rejected; a failed load of tomorrow
    /// sets the `Store` status and applies nothing. Always publishes. Returns
    /// whether a change was committed.
    async fn carry_over(&mut self, expected_rev: u64) -> bool {
        const CMD: &str = commands::CARRY_OVER;
        if expected_rev != self.rev {
            let reject = Reject::StaleRev {
                expected: expected_rev,
                current: self.rev,
            };
            self.reject(CMD, None, &reject);
            return false;
        }
        if self.overflow.is_empty() {
            self.reject(
                CMD,
                None,
                &Reject::invalid(CMD, commands::NOTHING_TO_CARRY_OVER),
            );
            return false;
        }
        let Ok(date) = self.day.date().tomorrow() else {
            self.reject(CMD, None, &Reject::invalid(CMD, "today has no tomorrow"));
            return false;
        };
        self.status.remove(&StatusKey::CommandRejected);
        let tomorrow = match self.store.load_day(date).await {
            Ok(Some(day)) => day,
            Ok(None) => Day::new(date, self.day.tz()),
            Err(e) => {
                self.store_failed("load tomorrow", &e);
                self.publish();
                return false;
            }
        };
        let now = self.clock.now();
        let mut today_next = self.day.clone();
        let mut tomorrow_next = tomorrow.clone();
        let items = self.overflow.clone();
        let extras = match commands::carry_over(
            &mut today_next,
            &mut tomorrow_next,
            &items,
            self.ids.as_ref(),
            now,
        ) {
            Ok(extras) => extras,
            Err(reject) => {
                self.reject(CMD, None, &reject);
                return false;
            }
        };
        let mut batch = diff(&self.day, &today_next);
        batch.extend(diff(&tomorrow, &tomorrow_next).ops().iter().cloned());
        batch.extend(extras);
        let changed = self
            .swap_after_commit(today_next, self.work_end, batch)
            .await;
        if changed {
            tracing::info!(count = items.len(), to = %date, "carried over to tomorrow");
        }
        self.publish();
        changed
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

    /// Commits `batch`, then swaps `next` and `work_end` in. On failure the old
    /// state stays.
    async fn swap_after_commit(&mut self, next: Day, work_end: WorkEnd, batch: StoreBatch) -> bool {
        if !self.commit(batch).await {
            return false;
        }
        self.day = next;
        self.work_end = work_end;
        true
    }

    /// The one deadline the loop waits on (AD-3): the earliest of the heartbeat,
    /// the running item's unprompted planned end, the local-midnight rollover, and
    /// the calendar poll.
    fn next_deadline(&self) -> Timestamp {
        [
            Some(self.heartbeat_at),
            self.planned_end().map(|(_, end)| end),
            self.rollover_at(),
            self.poll_at,
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(Timestamp::MAX)
    }

    /// The next local midnight after today, in the engine's zone (AD-8), or `None`
    /// while an item runs: its day stays current until it stops, and a past
    /// deadline would only spin the loop.
    fn rollover_at(&self) -> Option<Timestamp> {
        if running_of(&self.day).is_some() {
            return None;
        }
        Some(next_midnight(self.day.date(), &self.tz))
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
    /// commit or snapshot), the heartbeat, the midnight rollover, and/or the
    /// calendar poll. A heartbeat firing `WAKE_LATENESS` late means the machine
    /// slept, so a scheduled poll starts at once, unless it is backing off from
    /// rate limits.
    async fn on_deadline(&mut self) {
        let now = self.clock.now();
        // Checked before the heartbeat moves `heartbeat_at` on.
        let woke = self
            .heartbeat_at
            .checked_add(WAKE_LATENESS)
            .is_ok_and(|late| now >= late);
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
        // Rollover first, so the heartbeat's overflow check sees the new day.
        self.roll_over_if_due(now).await;
        if now >= self.heartbeat_at {
            self.heartbeat(now).await;
        }
        // A rollover just started a sync, which clears `poll_at`.
        if let Some(due) = self.poll_at
            && !self.in_flight
            // A wake never cuts a rate-limit backoff short.
            && (now >= due || (woke && self.rate_limits == 0))
        {
            tracing::debug!(woke, "calendar poll due");
            self.start_import();
        }
    }

    /// Records `last_alive_at` and re-checks overflow, since time passing alone
    /// re-flows pending work. Publishes only if the store status or the overflow
    /// list changed.
    async fn heartbeat(&mut self, now: Timestamp) {
        let status_before = self.status.clone();
        let batch = StoreBatch::from_iter([StoreOp::SetLastAlive { at: now }]);
        self.commit(batch).await;
        self.heartbeat_at = deadline_after(now);
        let overflow = self.overflow_of(&reflow(&self.day, now));
        if self.status != status_before || overflow != self.overflow {
            self.publish();
        }
    }

    /// The flexible items of `schedule` that end after today's work-end.
    fn overflow_of(&self, schedule: &Schedule) -> Vec<ItemId> {
        schedule.overflow(self.work_end.at(self.day.date(), &self.tz))
    }

    /// Sets or clears the `Overflow` status for the current list. The message holds
    /// the count and the work-end time, never titles (AD-14).
    fn set_overflow_status(&mut self) {
        let message = match self.overflow.len() {
            0 => {
                self.status.remove(&StatusKey::Overflow);
                return;
            }
            1 => format!("1 item runs past work-end {}", self.work_end),
            n => format!("{n} items run past work-end {}", self.work_end),
        };
        self.status.insert(StatusKey::Overflow, message);
    }

    /// Opens an overflow episode (one `Notify`) when the list turned non-empty, or
    /// ends it (withdrawing its prompt) when the list emptied. Runs after the
    /// snapshot is sent, so the renderer finds the leftovers in it.
    fn overflow_effects(&mut self) {
        match (self.overflow.first(), self.overflow_prompt) {
            (Some(&item_id), None) => {
                let prompt_id = self.ids.new_prompt_id();
                self.overflow_prompt = Some(prompt_id);
                tracing::info!(%item_id, %prompt_id, kind = "overflow", "prompt due");
                // A closed receiver only means nobody listens for effects.
                let _ = self.effects.send(Effect::Notify {
                    prompt_id,
                    item_id,
                    kind: PromptKind::Overflow,
                });
            }
            (None, Some(prompt_id)) => {
                self.overflow_prompt = None;
                tracing::info!(%prompt_id, "prompt withdrawn: overflow cleared");
                let _ = self
                    .effects
                    .send(Effect::WithdrawNotification { prompt_id });
            }
            _ => {}
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
        self.overflow = self.overflow_of(&schedule);
        self.set_overflow_status();
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
            work_end: self.work_end,
            overflow: self.overflow.clone(),
            known_tags: self.known_tags.clone(),
            google: self.google,
            calendars: self.calendars.clone(),
            google_client: GoogleClient {
                custom_id: self.google_client.as_ref().map(|c| c.id.clone()),
                available: self.client_available,
            },
            autostart: self.autostart,
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
        self.overflow_effects();
    }
}

/// What went wrong in a round's per-calendar syncs, and whether the applied
/// part changed anything visible.
#[derive(Default)]
struct RoundFailures {
    /// The day or the calendar table changed and was committed.
    changed: bool,
    /// An incremental sync hit a `410`.
    gone: bool,
    /// A sync was rate limited (403/429).
    rate_limited: bool,
    /// The first other failure's message.
    other: Option<String>,
    /// A sync or refresh found the session expired or revoked.
    expired: bool,
}

/// One sync round (AD-17), run off the actor: lists the calendars (a failure
/// fails the round), reconciles them with `stored`, then syncs each included
/// calendar in table order, resuming from its token in `tokens` when it has one,
/// and then re-fetches each of `day`'s live added events whose calendar is not
/// included ([`added_refs`]).
async fn sync_round(
    calendar: &dyn CalendarSource,
    stored: &[CalendarEntry],
    day: &Day,
    tokens: &BTreeMap<String, String>,
    from: Timestamp,
    to: Timestamp,
) -> Result<SyncRound, CalendarError> {
    let listed = calendar.list_calendars().await?;
    let calendars = reconcile(stored, &listed);
    let mut outcomes = Vec::new();
    for c in calendars.iter().filter(|c| c.included) {
        let token = tokens.get(&c.id).cloned();
        let incremental = token.is_some();
        let result = calendar.sync_calendar(c.id.clone(), token, from, to).await;
        outcomes.push(CalendarOutcome {
            calendar_id: c.id.clone(),
            incremental,
            result,
        });
    }
    let mut added = Vec::new();
    for (calendar_id, event_id) in added_refs(day, &calendars) {
        let result = calendar
            .get_event(calendar_id.clone(), event_id.clone(), from, to)
            .await;
        added.push(AddedOutcome {
            calendar_id,
            event_id,
            result,
        });
    }
    Ok(SyncRound {
        calendars,
        outcomes,
        added,
    })
}

/// Appends the `ids` not already in `into`, in order.
fn push_new(into: &mut Vec<ItemId>, ids: Vec<ItemId>) {
    for id in ids {
        if !into.contains(&id) {
            into.push(id);
        }
    }
}

/// Whether `change` is of event `event_id` of calendar `calendar_id`.
fn names(change: &CalendarChange, calendar_id: &str, event_id: &str) -> bool {
    let (c, e) = match change {
        CalendarChange::Upsert(ev) => (&ev.calendar_id, &ev.source_id),
        CalendarChange::Cancelled(key) => (&key.calendar_id, &key.source_id),
    };
    c == calendar_id && e == event_id
}

/// The picker's view of `events` against `day` (R4): sorted by start (ties
/// keep Google's order), each marked whether the plan has it.
fn calendar_events(day: &Day, mut events: Vec<FetchedEvent>) -> Vec<CalendarEvent> {
    events.sort_by_key(|e| e.start);
    events
        .into_iter()
        .map(|e| {
            let in_plan = in_plan(day, &e.key());
            CalendarEvent {
                calendar_id: e.calendar_id,
                event_id: e.source_id,
                title: e.title,
                start: e.start,
                end: e.end,
                in_plan,
            }
        })
        .collect()
}

/// The store ops that turn the cursors `before` into `after`: a put for each new
/// or changed cursor and a clear for each one gone.
fn cursor_ops(
    before: &BTreeMap<String, SyncCursor>,
    after: &BTreeMap<String, SyncCursor>,
) -> Vec<StoreOp> {
    let cleared = before
        .keys()
        .filter(|id| !after.contains_key(*id))
        .map(|calendar_id| StoreOp::ClearSyncCursor {
            calendar_id: calendar_id.clone(),
        });
    let put = after
        .values()
        .filter(|c| before.get(&c.calendar_id) != Some(*c))
        .map(|c| StoreOp::PutSyncCursor { cursor: c.clone() });
    cleared.chain(put).collect()
}

/// The report of `date`: from `today` when given, else from the stored day
/// (`None` when there is none), with its baseline (locked days only) and log.
async fn build_report(
    store: &dyn Store,
    date: Date,
    today: Option<Day>,
    now: Timestamp,
) -> DayReply {
    let day = match today {
        Some(day) => day,
        None => match store.load_day(date).await? {
            Some(day) => day,
            None => return Ok(None),
        },
    };
    let baseline = if day.lock_decision() == LockDecision::Locked {
        store.load_baseline(date).await?
    } else {
        None
    };
    let log = store.load_day_log(date).await?;
    Ok(Some(report(&day, baseline.as_ref(), &log, now)))
}

/// The dates an export of `range` covers, ascending and never after `today`: the
/// week's dates, or every stored date with `today` added when it is missing.
async fn export_dates(
    store: &dyn Store,
    range: ExportRange,
    today: Date,
) -> Result<Vec<Date>, String> {
    let mut dates = match range {
        ExportRange::Week(week) => week.days().to_vec(),
        ExportRange::All => {
            let mut dates = store.stored_dates().await.map_err(|e| e.message)?;
            if !dates.contains(&today) {
                dates.push(today);
            }
            dates
        }
    };
    dates.retain(|d| *d <= today);
    dates.sort();
    dates.dedup();
    Ok(dates)
}

/// An export range's log fields: its kind, and the week's year and number.
fn range_fields(range: ExportRange) -> (&'static str, Option<i16>, Option<i8>) {
    match range {
        ExportRange::Week(w) => ("week", Some(w.year()), Some(w.week())),
        ExportRange::All => ("all", None, None),
    }
}

/// The first item that is running.
fn running_of(day: &Day) -> Option<Running> {
    day.items().iter().find_map(PlanItem::running_view)
}

/// The next item to show while nothing runs: [`Day::next_item`], so the top of the
/// interrupt stack, else the schedule's first pending or paused item.
fn next_item_of(day: &Day, now: Timestamp) -> Option<NextItem> {
    day.next_item(now).map(|i| NextItem {
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

/// The first instant of the day after `date` in `tz`, saturating to
/// `Timestamp::MAX` at the end of the calendar. Never panics.
fn next_midnight(date: Date, tz: &TimeZone) -> Timestamp {
    date.tomorrow()
        .and_then(|d| d.to_zoned(tz.clone()))
        .and_then(|z| z.start_of_day())
        .map_or(Timestamp::MAX, |z| z.timestamp())
}

/// The local day `date` in `tz` as `[start, next midnight)`, or `None` at the
/// edge of the calendar.
fn day_window(date: Date, tz: &TimeZone) -> Option<(Timestamp, Timestamp)> {
    let start = date
        .to_zoned(tz.clone())
        .ok()?
        .start_of_day()
        .ok()?
        .timestamp();
    Some((start, next_midnight(date, tz)))
}

/// A calendar error's kind, for logs: never its message's details.
fn error_kind(e: &CalendarError) -> &'static str {
    match e.kind {
        CalendarErrorKind::NotSignedIn => "not_signed_in",
        CalendarErrorKind::NoClient => "no_client",
        CalendarErrorKind::Auth => "auth",
        CalendarErrorKind::RateLimited => "rate_limited",
        CalendarErrorKind::Gone => "gone",
        CalendarErrorKind::SessionExpired => "session_expired",
        CalendarErrorKind::Other => "other",
    }
}

/// The poll base after `limits` consecutive rate limits:
/// `min(POLL_EVERY·2^limits, BACKOFF_MAX)`.
fn backoff_base(limits: u32) -> Duration {
    POLL_EVERY
        .checked_mul(2u32.checked_pow(limits).unwrap_or(u32::MAX))
        .map_or(BACKOFF_MAX, |d| d.min(BACKOFF_MAX))
}

/// `round(base · (1 − JITTER + 2·JITTER·unit))` in whole seconds, with `unit`
/// clamped into `[0, 1]`.
fn jittered(base: Duration, unit: f64) -> Duration {
    let factor = 1.0 - JITTER + 2.0 * JITTER * unit.clamp(0.0, 1.0);
    Duration::from_secs_f64((base.as_secs_f64() * factor).round())
}

fn deadline_after(now: Timestamp) -> Timestamp {
    now.checked_add(HEARTBEAT_EVERY).unwrap_or(Timestamp::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_base_doubles_up_to_the_cap() {
        let secs = |n| backoff_base(n).as_secs();
        assert_eq!(secs(0), 300);
        assert_eq!(secs(1), 600);
        assert_eq!(secs(2), 1_200);
        assert_eq!(secs(3), 2_400);
        assert_eq!(secs(4), 3_600);
        assert_eq!(secs(40), 3_600);
        assert_eq!(secs(u32::MAX), 3_600);
    }

    #[test]
    fn the_jitter_is_a_quarter_either_way_rounded_to_seconds() {
        let secs = |base: u64, unit| jittered(Duration::from_secs(base), unit).as_secs();
        assert_eq!(secs(300, 0.0), 225);
        assert_eq!(secs(300, 0.2), 255);
        assert_eq!(secs(300, 0.5), 300);
        assert_eq!(secs(300, 1.0), 375);
        assert_eq!(secs(3_600, 0.0), 2_700);
        assert_eq!(secs(300, 7.0), 375, "clamped");
    }
}
