//! Calendar import (CAP-2, AD-9): the calendar source port and the merge of fetched
//! events into the current plan.
//!
//! Field ownership: Google owns an imported item's `title`, its pin (only while the
//! item is still fixed), its source length, and cancellation. `planned_secs` moves
//! by the change in the source length, so a local extension stays. The user owns
//! `fixed`, `tags`, and `order`, which the merge never touches; `status` changes
//! only when Google explicitly cancels a pending or paused item. An imported item
//! the user removed stays removed and is never re-added. An event missing from a
//! sync is never removed or changed. The merge changes the plan only, never a
//! baseline.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use jiff::Timestamp;
use jiff::civil::Date;

use crate::error::DomainError;
use crate::order::OrderKey;
use crate::plan::{CalendarRef, Day, ItemId, ItemStatus, PlanItem};

/// One timed event as fetched from a calendar. All-day, declined, and empty
/// events never become a `FetchedEvent`: the source skips them. A cancelled one
/// arrives as [`CalendarChange::Cancelled`] instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedEvent {
    pub calendar_id: String,
    pub source_id: String,
    /// A recurring instance's original start; `None` for a single event.
    pub instance_start: Option<Timestamp>,
    pub title: String,
    pub start: Timestamp,
    pub end: Timestamp,
}

impl FetchedEvent {
    /// The event's length in whole seconds, or `None` when it does not end after it
    /// starts.
    pub fn length_secs(&self) -> Option<u64> {
        let secs = self.end.as_second() - self.start.as_second();
        u64::try_from(secs).ok().filter(|s| *s > 0)
    }

    /// The event's merge key (AD-9).
    pub fn key(&self) -> EventKey {
        EventKey {
            calendar_id: self.calendar_id.clone(),
            source_id: self.source_id.clone(),
            instance_start: self.instance_start,
        }
    }
}

/// The merge key of an imported event (AD-9):
/// `(calendar_id, source_id, instance_start)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EventKey {
    pub calendar_id: String,
    pub source_id: String,
    /// A recurring instance's original start; `None` for a single event.
    pub instance_start: Option<Timestamp>,
}

impl EventKey {
    /// Whether `source` names this event instance.
    fn matches(&self, source: &CalendarRef) -> bool {
        source.calendar_id == self.calendar_id
            && source.source_id == self.source_id
            && source.instance_start == self.instance_start
    }
}

/// One change a calendar reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalendarChange {
    /// A timed event that exists, new or changed.
    Upsert(FetchedEvent),
    /// An event Google explicitly cancelled. Only its key is known.
    Cancelled(EventKey),
}

/// Where a calendar's incremental sync resumes (AD-17): Google's sync token, and
/// the local day it was issued for. A cursor is valid only on that day. `Debug`
/// never prints the token (AD-13, AD-14).
#[derive(Clone, PartialEq, Eq)]
pub struct SyncCursor {
    pub calendar_id: String,
    pub token: String,
    pub date: Date,
}

impl fmt::Debug for SyncCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncCursor")
            .field("calendar_id", &self.calendar_id)
            .field("token", &"<redacted>")
            .field("date", &self.date)
            .finish()
    }
}

/// The result of one sync of a calendar: its changes, in Google's order, and the
/// token the next incremental sync starts from (`None` when Google sent none).
/// `Debug` never prints the token (AD-13, AD-14).
#[derive(Clone, PartialEq, Eq)]
pub struct SyncBatch {
    /// The calendar's id, as Google reports it (never the alias `primary`).
    pub calendar_id: String,
    pub changes: Vec<CalendarChange>,
    pub next_token: Option<String>,
}

impl fmt::Debug for SyncBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyncBatch")
            .field("calendar_id", &self.calendar_id)
            .field("changes", &self.changes)
            .field(
                "next_token",
                &self.next_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// What kind of calendar failure happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CalendarErrorKind {
    /// No account is signed in (no refresh token stored).
    NotSignedIn,
    /// The build has no OAuth client to sign in with.
    NoClient,
    /// Signing in or refreshing the access token failed.
    Auth,
    /// Google is limiting requests (HTTP 403 or 429): back off.
    RateLimited,
    /// The sync token is no longer valid (HTTP 410): sync again from scratch.
    Gone,
    /// A network, HTTP, or parsing failure.
    Other,
}

/// A calendar failure. The message never carries tokens or event titles (AD-13,
/// AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarError {
    pub kind: CalendarErrorKind,
    pub message: String,
}

impl CalendarError {
    pub fn new(kind: CalendarErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for CalendarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CalendarError {}

/// The result of a calendar request. Std-only, so core needs no async runtime.
pub type CalendarFuture<T> = Pin<Box<dyn Future<Output = Result<T, CalendarError>> + Send>>;

/// One calendar of the account, as listed by the source. `Debug` prints the id
/// only: names are private (AD-14).
#[derive(Clone, PartialEq, Eq)]
pub struct CalendarInfo {
    pub id: String,
    /// The name to show: the user's override when set, else the calendar's own.
    pub name: String,
    /// Whether this is the account's primary calendar.
    pub primary: bool,
}

impl fmt::Debug for CalendarInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CalendarInfo")
            .field("id", &self.id)
            .field("primary", &self.primary)
            .finish_non_exhaustive()
    }
}

/// A stored calendar: a listed calendar and whether it feeds the plan (R2).
/// `Debug` never prints the name (AD-14).
#[derive(Clone, PartialEq, Eq)]
pub struct CalendarEntry {
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub included: bool,
}

impl fmt::Debug for CalendarEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CalendarEntry")
            .field("id", &self.id)
            .field("primary", &self.primary)
            .field("included", &self.included)
            .finish_non_exhaustive()
    }
}

/// The calendar table after a listing: one entry per `listed` calendar, in list
/// order, with Google's name and primary flag. A stored `included` flag wins; a
/// calendar seen for the first time is included iff it is primary. A stored
/// calendar absent from `listed` is dropped.
pub fn reconcile(stored: &[CalendarEntry], listed: &[CalendarInfo]) -> Vec<CalendarEntry> {
    listed
        .iter()
        .map(|c| CalendarEntry {
            id: c.id.clone(),
            name: c.name.clone(),
            primary: c.primary,
            included: stored
                .iter()
                .find(|s| s.id == c.id)
                .map_or(c.primary, |s| s.included),
        })
        .collect()
}

/// Hides `day`'s items imported from `calendar_id` after it was un-included, at
/// `now`: each pending item with no segments becomes a hidden tombstone (see
/// [`Day::hide`]); every other item, and every item the user added on its own,
/// is left as is. Returns the hidden ids in plan order.
pub fn hide_calendar(
    day: &mut Day,
    calendar_id: &str,
    now: Timestamp,
) -> Result<Vec<ItemId>, DomainError> {
    let ids: Vec<ItemId> = day
        .items()
        .iter()
        .filter(|i| !i.added() && i.source().is_some_and(|s| s.calendar_id == calendar_id))
        .map(PlanItem::id)
        .collect();
    let mut hidden = Vec::new();
    for id in ids {
        if day.hide(id, now)? {
            hidden.push(id);
        }
    }
    Ok(hidden)
}

/// A calendar account. Every call does network or user-interactive I/O, so the
/// engine runs them off its actor (AD-17).
pub trait CalendarSource: Send + Sync + 'static {
    /// Signs in interactively (the system browser), storing the session.
    fn sign_in(&self) -> CalendarFuture<()>;

    /// The account's calendars the user can read, in Google's order.
    fn list_calendars(&self) -> CalendarFuture<Vec<CalendarInfo>>;

    /// Syncs the calendar `calendar_id`. With no `token` this is a full fetch: the
    /// timed events starting in `[window_start, window_end)`. With a sync token it
    /// is incremental: the changes since that token, with upserts starting outside
    /// the window skipped and every cancellation kept. Either way the batch carries
    /// the token for the next incremental sync.
    fn sync_calendar(
        &self,
        calendar_id: String,
        token: Option<String>,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<SyncBatch>;

    /// The timed events of calendar `calendar_id` starting in
    /// `[window_start, window_end)`, in Google's order: a read-only listing with
    /// no sync token and no cancellations.
    fn list_events(
        &self,
        calendar_id: String,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<Vec<FetchedEvent>>;

    /// The single event `event_id` of calendar `calendar_id`: an upsert when it
    /// is timed and starts in `[window_start, window_end)`, a cancellation when
    /// Google cancelled it, and `None` when it is not found, all-day, declined,
    /// or outside the window.
    fn get_event(
        &self,
        calendar_id: String,
        event_id: String,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<Option<CalendarChange>>;
}

/// Adds the single `event` the user picked to `day` (R4), minting an id with
/// `new_id` for a new item:
///
/// - an unknown event becomes a fixed, pending imported item marked `added`;
/// - an event whose item is live (not removed) changes nothing;
/// - an event whose item is removed but not cancelled by Google (hidden, or
///   removed by the user) is revived as pending, marked `added`, taken back into
///   the schedule if it was excluded, and given Google's fields;
/// - an event Google cancelled, or one with no positive length, changes nothing.
///
/// Returns the id of the item added or revived. On an error `day` may be
/// half-changed: callers add into a copy.
pub fn add_event(
    day: &mut Day,
    event: &FetchedEvent,
    mut new_id: impl FnMut() -> ItemId,
) -> Result<Option<ItemId>, DomainError> {
    let Some(len) = event.length_secs() else {
        return Ok(None);
    };
    let known = known_item(day, &event.key())
        .map(|i| (i.id(), i.status(), i.removed_by_source_at(), i.excluded()));
    match known {
        None => {
            let id = insert(day, event, len, &mut new_id)?;
            day.mark_added(id)?;
            Ok(Some(id))
        }
        Some((id, ItemStatus::Removed, None, excluded)) => {
            day.revive_removed(id)?;
            day.mark_added(id)?;
            if excluded {
                day.include(id)?;
            }
            update(day, id, event, len)?;
            Ok(Some(id))
        }
        Some(_) => Ok(None),
    }
}

/// Whether `day` already has the event `key` for the picker (R4): an item with
/// that merge key that is not removed, or that Google cancelled (an explicit
/// add never revives it). A hidden or user-removed item does not count: adding
/// it again revives it.
pub fn in_plan(day: &Day, key: &EventKey) -> bool {
    known_item(day, key)
        .is_some_and(|i| i.status() != ItemStatus::Removed || i.removed_by_source_at().is_some())
}

/// The events to refresh with [`CalendarSource::get_event`] in a sync round:
/// `(calendar_id, source_id)` of each non-removed item of `day` the user added
/// on its own whose calendar is not included in `calendars`, once each, in plan
/// order.
pub fn added_refs(day: &Day, calendars: &[CalendarEntry]) -> Vec<(String, String)> {
    let included = |id: &str| calendars.iter().any(|c| c.id == id && c.included);
    let mut refs: Vec<(String, String)> = Vec::new();
    for item in day.items() {
        if !item.added() || item.status() == ItemStatus::Removed {
            continue;
        }
        let Some(source) = item.source() else {
            continue;
        };
        let r = (source.calendar_id.clone(), source.source_id.clone());
        if !included(&r.0) && !refs.contains(&r) {
            refs.push(r);
        }
    }
    refs
}

/// Merges `changes` into `day` under the AD-9 ownership rules at `now`, minting
/// ids for new items with `new_id`.
///
/// - An upsert of a new event becomes a fixed, pending item pinned at its start,
///   planned for its length, and ordered after every item of the day. A known
///   event (same `(calendar_id, source_id, instance_start)`) gets Google's fields;
///   a removed one is left alone, except a hidden one (its calendar was
///   un-included), which is revived as pending first. Events with no positive
///   length are skipped.
/// - A cancellation of a known item applies [`Day::remove_by_source`]: a pending
///   or paused item becomes a tombstone, any other only records
///   `removed_by_source_at`. An unknown key, or an item already removed or
///   cancelled, is ignored.
///
/// An event absent from `changes` is never touched. Returns the ids of the items
/// added or changed, each once, in the order first touched. On an error `day`
/// may be half-changed: callers merge into a copy.
pub fn merge(
    day: &mut Day,
    changes: &[CalendarChange],
    now: Timestamp,
    mut new_id: impl FnMut() -> ItemId,
) -> Result<Vec<ItemId>, DomainError> {
    let mut touched: Vec<ItemId> = Vec::new();
    let mut touch = |id: ItemId| {
        if !touched.contains(&id) {
            touched.push(id);
        }
    };
    for change in changes {
        match change {
            CalendarChange::Upsert(event) => {
                if let Some(id) = upsert(day, event, &mut new_id)? {
                    touch(id);
                }
            }
            CalendarChange::Cancelled(key) => {
                if let Some(id) = known(day, key).map(|(id, _)| id)
                    && day.remove_by_source(id, now)?
                {
                    touch(id);
                }
            }
        }
    }
    Ok(touched)
}

/// The id and status of the item imported from `key`, if any.
fn known(day: &Day, key: &EventKey) -> Option<(ItemId, ItemStatus)> {
    known_item(day, key).map(|i| (i.id(), i.status()))
}

/// The item imported from `key`, if any.
fn known_item<'a>(day: &'a Day, key: &EventKey) -> Option<&'a PlanItem> {
    day.items()
        .iter()
        .find(|i| i.source().is_some_and(|s| key.matches(s)))
}

/// Adds or updates the item of `event`. Returns its id when it was added or
/// changed.
fn upsert(
    day: &mut Day,
    event: &FetchedEvent,
    new_id: &mut impl FnMut() -> ItemId,
) -> Result<Option<ItemId>, DomainError> {
    let Some(len) = event.length_secs() else {
        return Ok(None);
    };
    match known_item(day, &event.key()).map(|i| (i.id(), i.status(), i.hidden_at())) {
        None => insert(day, event, len, new_id).map(Some),
        Some((id, ItemStatus::Removed, Some(_))) => {
            day.revive(id)?;
            update(day, id, event, len)?;
            Ok(Some(id))
        }
        Some((_, ItemStatus::Removed, None)) => Ok(None),
        Some((id, _, _)) => Ok(update(day, id, event, len)?.then_some(id)),
    }
}

/// Adds `event` (length `len`) as a new fixed, pending imported item ordered after
/// every item of the day, returning its id.
fn insert(
    day: &mut Day,
    event: &FetchedEvent,
    len: u64,
    new_id: &mut impl FnMut() -> ItemId,
) -> Result<ItemId, DomainError> {
    let order = OrderKey::between(last_order(day), None)?;
    let id = new_id();
    let source = CalendarRef {
        calendar_id: event.calendar_id.clone(),
        source_id: event.source_id.clone(),
        instance_start: event.instance_start,
        source_secs: len,
    };
    day.add(PlanItem::imported(
        id,
        event.title.clone(),
        event.start,
        order,
        source,
    ))?;
    Ok(id)
}

/// Applies Google's fields of `event` (length `len`) to the known item `id`.
/// Returns whether anything changed.
fn update(day: &mut Day, id: ItemId, event: &FetchedEvent, len: u64) -> Result<bool, DomainError> {
    let item = day.item(id).ok_or(DomainError::UnknownItem(id))?;
    let title_changed = item.title() != event.title;
    let pin_changed = item.fixed() && item.pinned_start() != Some(event.start);
    let source_secs = item.source().map_or(len, |s| s.source_secs);
    let planned = item.planned_secs();
    if title_changed {
        day.set_title(id, event.title.clone())?;
    }
    if pin_changed {
        day.set_fixed(id, event.start)?;
    }
    let len_changed = source_secs != len;
    if len_changed {
        day.set_planned_secs(id, resized(planned, source_secs, len))?;
        day.set_source_secs(id, len)?;
    }
    Ok(title_changed || pin_changed || len_changed)
}

/// The shortest planned length a resize can leave, in seconds: planned lengths are
/// positive.
const MIN_PLANNED_SECS: u64 = 1;

/// `planned` moved by `new_len - old_len`, so a local extension stays, and never
/// below [`MIN_PLANNED_SECS`].
fn resized(planned: u64, old_len: u64, new_len: u64) -> u64 {
    let next = if new_len >= old_len {
        planned.saturating_add(new_len - old_len)
    } else {
        planned.saturating_sub(old_len - new_len)
    };
    next.max(MIN_PLANNED_SECS)
}

/// The highest order key of the day, removed items included.
fn last_order(day: &Day) -> Option<&OrderKey> {
    day.items().iter().map(PlanItem::order).max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{id, key, ts};
    use jiff::civil::date;

    fn event(source_id: &str, title: &str, start: i64, end: i64) -> FetchedEvent {
        FetchedEvent {
            calendar_id: "me@example.com".into(),
            source_id: source_id.into(),
            instance_start: None,
            title: title.into(),
            start: ts(start),
            end: ts(end),
        }
    }

    /// Merges `events` as upserts at time 0, returning how many items changed.
    fn merge_events(
        d: &mut Day,
        events: &[FetchedEvent],
        new_id: impl FnMut() -> ItemId,
    ) -> Result<usize, DomainError> {
        let changes: Vec<CalendarChange> =
            events.iter().cloned().map(CalendarChange::Upsert).collect();
        merge(d, &changes, ts(0), new_id).map(|ids| ids.len())
    }

    fn empty_day() -> Day {
        Day::new(date(2026, 11, 21), "UTC")
    }

    /// Ids 1000, 1001, ... for new items.
    fn ids() -> impl FnMut() -> ItemId {
        let mut n = 1000;
        move || {
            n += 1;
            id(n - 1)
        }
    }

    fn only(day: &Day) -> &PlanItem {
        assert_eq!(day.items().len(), 1);
        &day.items()[0]
    }

    #[test]
    fn a_new_timed_event_becomes_a_fixed_pending_item_at_its_start() {
        let mut d = empty_day();
        let n = merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        assert_eq!(n, 1);
        let i = only(&d);
        assert_eq!(i.id(), id(1000));
        assert_eq!(i.title(), "Standup");
        assert!(i.fixed());
        assert_eq!(i.pinned_start(), Some(ts(3_600)));
        assert_eq!(i.status(), ItemStatus::Pending);
        assert_eq!(i.planned_secs(), 1_800);
        let source = i.source().unwrap();
        assert_eq!(source.source_secs, 1_800);
        assert_eq!(source.calendar_id, "me@example.com");
        assert_eq!(source.source_id, "e1");
        assert_eq!(source.instance_start, None);
        assert_eq!(i.removed_by_source_at(), None);
    }

    #[test]
    fn new_items_are_ordered_after_every_existing_item() {
        let mut d = empty_day();
        d.add(PlanItem::new(id(1), "local", 600, key("V"))).unwrap();
        merge_events(
            &mut d,
            &[
                event("e1", "a", 3_600, 5_400),
                event("e2", "b", 7_200, 9_000),
            ],
            ids(),
        )
        .unwrap();
        let orders: Vec<&OrderKey> = d.items().iter().map(PlanItem::order).collect();
        assert!(orders[0] < orders[1] && orders[1] < orders[2], "{orders:?}");
    }

    #[test]
    fn re_import_unchanged_changes_nothing() {
        let mut d = empty_day();
        let events = [event("e1", "Standup", 3_600, 5_400)];
        merge_events(&mut d, &events, ids()).unwrap();
        let before = d.clone();
        assert_eq!(merge_events(&mut d, &events, ids()).unwrap(), 0);
        assert_eq!(d, before);
    }

    #[test]
    fn a_moved_and_retitled_event_updates_pin_and_title() {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        let n = merge_events(&mut d, &[event("e1", "Daily", 7_200, 9_000)], ids()).unwrap();
        assert_eq!(n, 1);
        let i = only(&d);
        assert_eq!(i.title(), "Daily");
        assert_eq!(i.pinned_start(), Some(ts(7_200)));
        assert_eq!(i.planned_secs(), 1_800);
    }

    #[test]
    fn a_moved_event_leaves_a_flexible_item_flexible() {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        d.set_flexible(id(1000)).unwrap();
        d.set_tags(id(1000), vec!["meet".into()]).unwrap();
        d.set_order(id(1000), key("a")).unwrap();
        merge_events(&mut d, &[event("e1", "Daily", 7_200, 9_000)], ids()).unwrap();
        let i = only(&d);
        assert!(!i.fixed());
        assert_eq!(i.pinned_start(), None);
        assert_eq!(i.tags(), ["meet".to_owned()]);
        assert_eq!(i.order(), &key("a"));
        assert_eq!(i.status(), ItemStatus::Pending);
        assert_eq!(i.title(), "Daily");
    }

    #[test]
    fn a_length_change_keeps_a_local_extension() {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Review", 0, 1_800)], ids()).unwrap();
        d.set_planned_secs(id(1000), 2_400).unwrap();
        merge_events(&mut d, &[event("e1", "Review", 0, 3_600)], ids()).unwrap();
        let i = only(&d);
        assert_eq!(i.planned_secs(), 4_200);
        assert_eq!(i.source().unwrap().source_secs, 3_600);
    }

    #[test]
    fn a_shortened_event_never_drops_below_the_minimum() {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Review", 0, 3_600)], ids()).unwrap();
        d.set_planned_secs(id(1000), 600).unwrap();
        merge_events(&mut d, &[event("e1", "Review", 0, 60)], ids()).unwrap();
        let i = only(&d);
        assert_eq!(i.planned_secs(), MIN_PLANNED_SECS);
        assert_eq!(i.source().unwrap().source_secs, 60);
    }

    #[test]
    fn a_removed_imported_item_stays_removed_and_is_not_re_added() {
        let mut d = empty_day();
        let events = [event("e1", "Standup", 3_600, 5_400)];
        merge_events(&mut d, &events, ids()).unwrap();
        d.remove(id(1000), ts(100)).unwrap();
        let before = d.clone();
        let moved = [event("e1", "Daily", 7_200, 9_000)];
        assert_eq!(merge_events(&mut d, &moved, ids()).unwrap(), 0);
        assert_eq!(d, before);
    }

    #[test]
    fn recurring_instances_stay_distinct_by_instance_start() {
        let mut d = empty_day();
        let mut a = event("r1", "Weekly", 3_600, 5_400);
        a.instance_start = Some(ts(3_600));
        let mut b = event("r1", "Weekly", 90_000, 91_800);
        b.instance_start = Some(ts(90_000));
        assert_eq!(
            merge_events(&mut d, &[a.clone(), b.clone()], ids()).unwrap(),
            2
        );
        assert_eq!(merge_events(&mut d, &[a, b], ids()).unwrap(), 0);
        assert_eq!(d.items().len(), 2);
    }

    #[test]
    fn an_event_with_no_positive_length_is_skipped() {
        let mut d = empty_day();
        let events = [
            event("e1", "zero", 3_600, 3_600),
            event("e2", "neg", 3_600, 0),
        ];
        assert_eq!(merge_events(&mut d, &events, ids()).unwrap(), 0);
        assert!(d.items().is_empty());
    }

    #[test]
    fn local_items_are_never_matched() {
        let mut d = empty_day();
        d.add(PlanItem::new(id(1), "Standup", 600, key("V")))
            .unwrap();
        merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        assert_eq!(d.items().len(), 2);
        assert_eq!(d.item(id(1)).unwrap().source(), None);
    }

    fn cancel(source_id: &str) -> CalendarChange {
        CalendarChange::Cancelled(EventKey {
            calendar_id: "me@example.com".into(),
            source_id: source_id.into(),
            instance_start: None,
        })
    }

    /// A day with event "e1" imported as item 1000, tagged, with a closed segment
    /// and then driven into `status`.
    fn imported_with(status: ItemStatus) -> Day {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        d.set_tags(id(1000), vec!["meet".into()]).unwrap();
        d.start(id(1000), ts(10)).unwrap();
        d.pause(id(1000), ts(20)).unwrap();
        match status {
            ItemStatus::Pending => {
                d = Day::restore(
                    d.date(),
                    d.tz(),
                    d.items().iter().map(|i| {
                        let mut r = record_of(i);
                        r.status = ItemStatus::Pending;
                        PlanItem::restore(r).unwrap()
                    }),
                )
                .unwrap();
            }
            ItemStatus::Paused => {}
            ItemStatus::Running => d.resume(id(1000), ts(30)).unwrap(),
            ItemStatus::Done => d.finish(id(1000), ts(30)).unwrap(),
            ItemStatus::CarriedOver => d.carry_over(id(1000)).unwrap(),
            ItemStatus::Removed => d.remove(id(1000), ts(30)).unwrap(),
        }
        assert_eq!(d.item(id(1000)).unwrap().status(), status);
        d
    }

    fn record_of(i: &PlanItem) -> crate::plan::PlanItemRecord {
        crate::plan::PlanItemRecord {
            id: i.id(),
            title: i.title().to_owned(),
            planned_secs: i.planned_secs(),
            order: i.order().clone(),
            fixed: i.fixed(),
            pinned_start: i.pinned_start(),
            status: i.status(),
            removed_at: i.removed_at(),
            segments: i.segments().to_vec(),
            carried_from: i.carried_from(),
            tags: i.tags().to_vec(),
            source: i.source().cloned(),
            removed_by_source_at: i.removed_by_source_at(),
            hidden_at: i.hidden_at(),
            excluded: i.excluded(),
            added: i.added(),
        }
    }

    /// Checks that only the cancellation fields differ between `before` and
    /// `after` for item 1000: user fields and segments are kept.
    fn assert_user_fields_kept(before: &PlanItem, after: &PlanItem) {
        assert_eq!(after.tags(), before.tags());
        assert_eq!(after.segments(), before.segments());
        assert_eq!(after.fixed(), before.fixed());
        assert_eq!(after.pinned_start(), before.pinned_start());
        assert_eq!(after.order(), before.order());
        assert_eq!(after.title(), before.title());
        assert_eq!(after.planned_secs(), before.planned_secs());
    }

    #[test]
    fn a_cancelled_pending_or_paused_item_becomes_a_source_tombstone() {
        for status in [ItemStatus::Pending, ItemStatus::Paused] {
            let mut d = imported_with(status);
            let before = d.item(id(1000)).unwrap().clone();
            let touched = merge(&mut d, &[cancel("e1")], ts(500), ids()).unwrap();
            assert_eq!(touched, vec![id(1000)], "{status:?}");
            let i = d.item(id(1000)).unwrap();
            assert_eq!(i.status(), ItemStatus::Removed, "{status:?}");
            assert_eq!(i.removed_at(), Some(ts(500)));
            assert_eq!(i.removed_by_source_at(), Some(ts(500)));
            assert!(!i.segments().is_empty());
            assert_user_fields_kept(&before, i);
            assert_eq!(d.items().len(), 1);
        }
    }

    #[test]
    fn a_cancelled_paused_item_leaves_the_interrupt_stack() {
        let mut d = imported_with(ItemStatus::Paused);
        d.restore_interrupt_stack([id(1000)]).unwrap();
        merge(&mut d, &[cancel("e1")], ts(500), ids()).unwrap();
        assert!(d.interrupt_stack().is_empty());
    }

    #[test]
    fn a_cancelled_running_done_or_carried_item_keeps_its_status() {
        for status in [
            ItemStatus::Running,
            ItemStatus::Done,
            ItemStatus::CarriedOver,
        ] {
            let mut d = imported_with(status);
            let before = d.item(id(1000)).unwrap().clone();
            let touched = merge(&mut d, &[cancel("e1")], ts(500), ids()).unwrap();
            assert_eq!(touched, vec![id(1000)], "{status:?}");
            let i = d.item(id(1000)).unwrap();
            assert_eq!(i.status(), status);
            assert_eq!(i.removed_at(), None);
            assert_eq!(i.removed_by_source_at(), Some(ts(500)));
            assert_user_fields_kept(&before, i);
        }
    }

    #[test]
    fn cancelling_a_removed_or_already_cancelled_item_changes_nothing() {
        let mut removed = imported_with(ItemStatus::Removed);
        let before = removed.clone();
        assert!(
            merge(&mut removed, &[cancel("e1")], ts(500), ids())
                .unwrap()
                .is_empty()
        );
        assert_eq!(removed, before);

        let mut done = imported_with(ItemStatus::Done);
        merge(&mut done, &[cancel("e1")], ts(500), ids()).unwrap();
        let before = done.clone();
        assert!(
            merge(&mut done, &[cancel("e1")], ts(900), ids())
                .unwrap()
                .is_empty()
        );
        assert_eq!(done, before);
    }

    #[test]
    fn a_cancelled_source_tombstone_is_not_re_added_by_an_upsert() {
        let mut d = imported_with(ItemStatus::Pending);
        merge(&mut d, &[cancel("e1")], ts(500), ids()).unwrap();
        let before = d.clone();
        assert_eq!(
            merge_events(&mut d, &[event("e1", "Back", 7_200, 9_000)], ids()).unwrap(),
            0
        );
        assert_eq!(d, before);
    }

    #[test]
    fn cancelling_an_unknown_key_is_ignored() {
        let mut d = imported_with(ItemStatus::Pending);
        let before = d.clone();
        let mut other_instance = EventKey {
            calendar_id: "me@example.com".into(),
            source_id: "e1".into(),
            instance_start: Some(ts(3_600)),
        };
        let touched = merge(
            &mut d,
            &[
                cancel("nope"),
                CalendarChange::Cancelled(other_instance.clone()),
            ],
            ts(500),
            ids(),
        )
        .unwrap();
        assert!(touched.is_empty());
        assert_eq!(d, before);
        other_instance.calendar_id = "other@example.com".into();
        other_instance.instance_start = None;
        merge(
            &mut d,
            &[CalendarChange::Cancelled(other_instance)],
            ts(500),
            ids(),
        )
        .unwrap();
        assert_eq!(d, before);
    }

    #[test]
    fn a_cancelled_recurring_instance_matches_only_that_instance() {
        let mut d = empty_day();
        let mut a = event("r1", "Weekly", 3_600, 5_400);
        a.instance_start = Some(ts(3_600));
        let mut b = event("r1", "Weekly", 90_000, 91_800);
        b.instance_start = Some(ts(90_000));
        merge_events(&mut d, &[a, b], ids()).unwrap();
        let key = EventKey {
            calendar_id: "me@example.com".into(),
            source_id: "r1".into(),
            instance_start: Some(ts(90_000)),
        };
        let touched = merge(&mut d, &[CalendarChange::Cancelled(key)], ts(500), ids()).unwrap();
        assert_eq!(touched, vec![id(1001)]);
        assert_eq!(d.item(id(1000)).unwrap().status(), ItemStatus::Pending);
        assert_eq!(d.item(id(1001)).unwrap().status(), ItemStatus::Removed);
    }

    #[test]
    fn an_event_absent_from_the_changes_is_never_touched() {
        let mut d = empty_day();
        merge_events(
            &mut d,
            &[
                event("e1", "Standup", 3_600, 5_400),
                event("e2", "Review", 7_200, 9_000),
            ],
            ids(),
        )
        .unwrap();
        let kept = d.item(id(1001)).unwrap().clone();
        let touched = merge(
            &mut d,
            &[CalendarChange::Upsert(event("e1", "Daily", 3_600, 5_400))],
            ts(500),
            ids(),
        )
        .unwrap();
        assert_eq!(touched, vec![id(1000)]);
        assert_eq!(d.item(id(1001)).unwrap(), &kept);
        assert!(merge(&mut d, &[], ts(600), ids()).unwrap().is_empty());
        assert_eq!(d.item(id(1001)).unwrap(), &kept);
    }

    #[test]
    fn touched_ids_are_listed_once_in_first_touched_order() {
        let mut d = empty_day();
        merge_events(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        let touched = merge(
            &mut d,
            &[
                CalendarChange::Upsert(event("e2", "New", 7_200, 9_000)),
                CalendarChange::Upsert(event("e1", "Daily", 3_600, 5_400)),
                CalendarChange::Upsert(event("e1", "Daily", 4_000, 5_800)),
                CalendarChange::Upsert(event("e2", "New", 7_200, 9_000)),
                cancel("e1"),
            ],
            ts(500),
            || id(2000),
        )
        .unwrap();
        assert_eq!(touched, vec![id(2000), id(1000)]);
        assert_eq!(d.item(id(1000)).unwrap().status(), ItemStatus::Removed);
    }

    fn info(id: &str, primary: bool) -> CalendarInfo {
        CalendarInfo {
            id: id.into(),
            name: format!("name {id}"),
            primary,
        }
    }

    fn entry(id: &str, primary: bool, included: bool) -> CalendarEntry {
        CalendarEntry {
            id: id.into(),
            name: format!("old {id}"),
            primary,
            included,
        }
    }

    #[test]
    fn reconcile_defaults_new_calendars_to_primary_only() {
        let got = reconcile(&[], &[info("o", false), info("p", true)]);
        assert_eq!(
            got,
            vec![
                CalendarEntry {
                    id: "o".into(),
                    name: "name o".into(),
                    primary: false,
                    included: false,
                },
                CalendarEntry {
                    id: "p".into(),
                    name: "name p".into(),
                    primary: true,
                    included: true,
                },
            ]
        );
    }

    #[test]
    fn reconcile_keeps_stored_flags_drops_absent_and_follows_list_order() {
        let stored = [
            entry("p", true, false),
            entry("o", false, true),
            entry("gone", false, true),
        ];
        let got = reconcile(
            &stored,
            &[info("n", false), info("o", false), info("p", true)],
        );
        let ids: Vec<(&str, bool)> = got.iter().map(|c| (c.id.as_str(), c.included)).collect();
        assert_eq!(ids, vec![("n", false), ("o", true), ("p", false)]);
        assert!(got.iter().all(|c| c.name.starts_with("name ")));
    }

    #[test]
    fn calendar_debug_never_prints_the_name() {
        let printed = format!(
            "{:?} {:?}",
            info("cal-1", true),
            entry("cal-2", false, true)
        );
        assert!(printed.contains("cal-1") && printed.contains("cal-2"));
        assert!(!printed.contains("name") && !printed.contains("old"));
    }

    fn other_event(source_id: &str, start: i64) -> FetchedEvent {
        FetchedEvent {
            calendar_id: "other@example.com".into(),
            ..event(source_id, source_id, start, start + 1_800)
        }
    }

    #[test]
    fn hide_calendar_hides_only_pending_untracked_items_of_that_calendar() {
        let mut d = empty_day();
        // 1000: other/o1 pending; 1001: other/o2 done; 1002: other/o3 pending with
        // a segment; 1003: other/o4 removed by the user; 1004: primary e1 pending.
        merge_events(
            &mut d,
            &[
                other_event("o1", 3_600),
                other_event("o2", 7_200),
                other_event("o3", 10_800),
                other_event("o4", 14_400),
                event("e1", "Standup", 18_000, 19_800),
            ],
            ids(),
        )
        .unwrap();
        d.start(id(1001), ts(10)).unwrap();
        d.finish(id(1001), ts(20)).unwrap();
        d.start(id(1002), ts(30)).unwrap();
        d.pause(id(1002), ts(40)).unwrap();
        d = Day::restore(
            d.date(),
            d.tz(),
            d.items().iter().map(|i| {
                let mut r = record_of(i);
                if r.id == id(1002) {
                    r.status = ItemStatus::Pending;
                }
                PlanItem::restore(r).unwrap()
            }),
        )
        .unwrap();
        d.remove(id(1003), ts(50)).unwrap();
        let before = d.clone();
        let hidden = hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
        assert_eq!(hidden, vec![id(1000)]);
        let o1 = d.item(id(1000)).unwrap();
        assert_eq!(o1.status(), ItemStatus::Removed);
        assert_eq!(o1.removed_at(), Some(ts(500)));
        assert_eq!(o1.hidden_at(), Some(ts(500)));
        for n in [1001, 1002, 1003, 1004] {
            assert_eq!(d.item(id(n)), before.item(id(n)), "item {n}");
        }
        assert_eq!(d.items().len(), 5);
    }

    #[test]
    fn a_re_import_revives_a_hidden_item_keeping_user_fields() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        d.set_tags(id(1000), vec!["meet".into()]).unwrap();
        d.set_order(id(1000), key("a")).unwrap();
        hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
        let mut moved = other_event("o1", 5_400);
        moved.title = "Renamed".into();
        let touched = merge(&mut d, &[CalendarChange::Upsert(moved)], ts(600), ids()).unwrap();
        assert_eq!(touched, vec![id(1000)]);
        let i = only(&d);
        assert_eq!(i.status(), ItemStatus::Pending);
        assert_eq!(i.removed_at(), None);
        assert_eq!(i.hidden_at(), None);
        assert_eq!(i.tags(), ["meet".to_owned()]);
        assert_eq!(i.order(), &key("a"));
        assert!(i.fixed());
        assert_eq!(i.pinned_start(), Some(ts(5_400)));
        assert_eq!(i.title(), "Renamed");
    }

    #[test]
    fn an_unchanged_re_import_still_revives_and_counts_as_touched() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
        let touched = merge(
            &mut d,
            &[CalendarChange::Upsert(other_event("o1", 3_600))],
            ts(600),
            ids(),
        )
        .unwrap();
        assert_eq!(touched, vec![id(1000)]);
        assert_eq!(only(&d).status(), ItemStatus::Pending);
    }

    #[test]
    fn a_hidden_item_can_still_not_be_cancelled_by_source() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
        let before = d.clone();
        let cancel = CalendarChange::Cancelled(other_event("o1", 3_600).key());
        assert!(merge(&mut d, &[cancel], ts(600), ids()).unwrap().is_empty());
        assert_eq!(d, before);
    }

    #[test]
    fn merge_keeps_excluded_through_updates_cancellation_hide_and_revive() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        d.exclude(id(1000)).unwrap();
        let mut moved = other_event("o1", 5_400);
        moved.title = "Renamed".into();
        moved.end = ts(9_000);
        merge(
            &mut d,
            &[CalendarChange::Upsert(moved.clone())],
            ts(10),
            ids(),
        )
        .unwrap();
        let i = only(&d);
        assert!(i.excluded());
        assert_eq!(i.title(), "Renamed");
        assert_eq!(i.pinned_start(), Some(ts(5_400)));
        assert_eq!(i.planned_secs(), 3_600);
        hide_calendar(&mut d, "other@example.com", ts(20)).unwrap();
        assert!(only(&d).excluded());
        assert_eq!(only(&d).status(), ItemStatus::Removed);
        merge(
            &mut d,
            &[CalendarChange::Upsert(moved.clone())],
            ts(30),
            ids(),
        )
        .unwrap();
        assert!(only(&d).excluded());
        assert_eq!(only(&d).status(), ItemStatus::Pending);
        let cancel = CalendarChange::Cancelled(moved.key());
        merge(&mut d, &[cancel], ts(40), ids()).unwrap();
        assert!(only(&d).excluded());
        assert_eq!(only(&d).removed_by_source_at(), Some(ts(40)));
    }

    #[test]
    fn add_event_imports_a_new_event_as_an_added_fixed_item() {
        let mut d = empty_day();
        let got = add_event(&mut d, &other_event("o1", 3_600), ids()).unwrap();
        assert_eq!(got, Some(id(1000)));
        let i = only(&d);
        assert!(i.added() && i.fixed() && !i.excluded());
        assert_eq!(i.pinned_start(), Some(ts(3_600)));
        assert_eq!(i.status(), ItemStatus::Pending);
        assert_eq!(i.planned_secs(), 1_800);
        let mut zero = other_event("o2", 3_600);
        zero.end = zero.start;
        assert_eq!(add_event(&mut d, &zero, ids()).unwrap(), None);
        assert_eq!(d.items().len(), 1);
    }

    #[test]
    fn add_event_of_a_live_item_changes_nothing() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        let before = d.clone();
        let mut moved = other_event("o1", 5_400);
        moved.title = "Renamed".into();
        assert_eq!(add_event(&mut d, &moved, ids()).unwrap(), None);
        assert_eq!(d, before);
    }

    #[test]
    fn add_event_revives_a_hidden_or_user_removed_item_with_googles_fields() {
        for hidden in [true, false] {
            let mut d = empty_day();
            merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
            d.set_tags(id(1000), vec!["meet".into()]).unwrap();
            if hidden {
                hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
            } else {
                d.exclude(id(1000)).unwrap();
                d.remove(id(1000), ts(500)).unwrap();
            }
            let mut moved = other_event("o1", 5_400);
            moved.title = "Renamed".into();
            assert_eq!(add_event(&mut d, &moved, ids()).unwrap(), Some(id(1000)));
            let i = only(&d);
            assert_eq!(i.status(), ItemStatus::Pending, "hidden={hidden}");
            assert_eq!(i.removed_at(), None);
            assert_eq!(i.hidden_at(), None);
            assert!(i.added() && !i.excluded());
            assert_eq!(i.title(), "Renamed");
            assert_eq!(i.pinned_start(), Some(ts(5_400)));
            assert_eq!(i.tags(), ["meet".to_owned()]);
        }
    }

    #[test]
    fn add_event_never_revives_a_source_cancelled_item() {
        let mut d = empty_day();
        merge_events(&mut d, &[other_event("o1", 3_600)], ids()).unwrap();
        merge(
            &mut d,
            &[CalendarChange::Cancelled(other_event("o1", 3_600).key())],
            ts(500),
            ids(),
        )
        .unwrap();
        let before = d.clone();
        assert_eq!(
            add_event(&mut d, &other_event("o1", 3_600), ids()).unwrap(),
            None
        );
        assert_eq!(d, before);
    }

    #[test]
    fn in_plan_counts_live_and_source_cancelled_items_only() {
        let mut d = empty_day();
        let mut next = ids();
        merge_events(
            &mut d,
            &[
                other_event("o1", 3_600),
                other_event("o2", 7_200),
                other_event("o3", 10_800),
                other_event("o4", 14_400),
            ],
            &mut next,
        )
        .unwrap();
        d.remove(id(1001), ts(10)).unwrap();
        merge(
            &mut d,
            &[CalendarChange::Cancelled(other_event("o3", 10_800).key())],
            ts(20),
            &mut next,
        )
        .unwrap();
        d.exclude(id(1003)).unwrap();
        let has = |d: &Day, s: &str, start| in_plan(d, &other_event(s, start).key());
        assert!(has(&d, "o1", 3_600), "live");
        assert!(!has(&d, "o2", 7_200), "removed by the user");
        assert!(has(&d, "o3", 10_800), "cancelled by Google");
        assert!(has(&d, "o4", 14_400), "excluded");
        assert!(!has(&d, "o5", 0), "unknown");
        hide_calendar(&mut d, "other@example.com", ts(30)).unwrap();
        assert!(!has(&d, "o1", 3_600), "hidden");
    }

    #[test]
    fn hide_calendar_skips_added_items() {
        let mut d = empty_day();
        let mut next = ids();
        add_event(&mut d, &other_event("o1", 3_600), &mut next).unwrap();
        merge_events(&mut d, &[other_event("o2", 7_200)], &mut next).unwrap();
        let hidden = hide_calendar(&mut d, "other@example.com", ts(500)).unwrap();
        assert_eq!(hidden, vec![id(1001)]);
        assert_eq!(d.item(id(1000)).unwrap().status(), ItemStatus::Pending);
    }

    #[test]
    fn added_refs_lists_live_added_items_of_non_included_calendars_once() {
        let mut d = empty_day();
        let mut next = ids();
        add_event(&mut d, &other_event("o1", 3_600), &mut next).unwrap();
        add_event(&mut d, &other_event("o2", 7_200), &mut next).unwrap();
        let mut inst = other_event("o1", 90_000);
        inst.instance_start = Some(ts(90_000));
        add_event(&mut d, &inst, &mut next).unwrap();
        add_event(&mut d, &event("e1", "Mine", 3_600, 5_400), &mut next).unwrap();
        merge_events(&mut d, &[other_event("o3", 10_800)], &mut next).unwrap();
        d.remove(id(1001), ts(10)).unwrap();
        let calendars = [
            entry("other@example.com", false, false),
            entry("me@example.com", true, true),
        ];
        assert_eq!(
            added_refs(&d, &calendars),
            vec![("other@example.com".to_owned(), "o1".to_owned())]
        );
        assert_eq!(
            added_refs(&d, &[]),
            vec![
                ("other@example.com".to_owned(), "o1".to_owned()),
                ("me@example.com".to_owned(), "e1".to_owned()),
            ]
        );
    }

    #[test]
    fn a_sync_batch_debug_never_prints_the_token() {
        let batch = SyncBatch {
            calendar_id: "me@example.com".into(),
            changes: Vec::new(),
            next_token: Some("secret-sync-token".into()),
        };
        let printed = format!("{batch:?}");
        assert!(printed.contains("me@example.com"));
        assert!(!printed.contains("secret-sync-token"));
    }

    #[test]
    fn a_sync_cursor_debug_never_prints_the_token() {
        let cursor = SyncCursor {
            calendar_id: "me@example.com".into(),
            token: "secret-sync-token".into(),
            date: date(2026, 11, 21),
        };
        let printed = format!("{cursor:?}");
        assert!(printed.contains("me@example.com"));
        assert!(printed.contains("2026-11-21"));
        assert!(!printed.contains("secret-sync-token"));
        let op = format!("{:?}", crate::store::StoreOp::PutSyncCursor { cursor });
        assert!(op.contains("me@example.com"));
        assert!(!op.contains("secret-sync-token"));
    }

    #[test]
    fn error_messages_carry_the_message_only() {
        let e = CalendarError::new(CalendarErrorKind::Other, "HTTP 503");
        assert_eq!(e.to_string(), "HTTP 503");
    }
}
