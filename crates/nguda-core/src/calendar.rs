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

/// A calendar account. Both calls do network or user-interactive I/O, so the
/// engine runs them off its actor (AD-17).
pub trait CalendarSource: Send + Sync + 'static {
    /// Signs in interactively (the system browser), storing the session.
    fn sign_in(&self) -> CalendarFuture<()>;

    /// Syncs the primary calendar. With no `cursor` this is a full fetch: the
    /// timed events starting in `[window_start, window_end)`. With a cursor
    /// `(calendar_id, sync_token)` it is incremental: the changes since that token,
    /// with upserts starting outside the window skipped and every cancellation
    /// kept. Either way the batch carries the token for the next incremental sync.
    fn sync_primary(
        &self,
        cursor: Option<(String, String)>,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<SyncBatch>;
}

/// Merges `changes` into `day` under the AD-9 ownership rules at `now`, minting
/// ids for new items with `new_id`.
///
/// - An upsert of a new event becomes a fixed, pending item pinned at its start,
///   planned for its length, and ordered after every item of the day. A known
///   event (same `(calendar_id, source_id, instance_start)`) gets Google's fields;
///   a removed one is left alone. Events with no positive length are skipped.
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
    day.items()
        .iter()
        .find(|i| i.source().is_some_and(|s| key.matches(s)))
        .map(|i| (i.id(), i.status()))
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
    match known(day, &event.key()) {
        None => {
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
            Ok(Some(id))
        }
        Some((_, ItemStatus::Removed)) => Ok(None),
        Some((id, _)) => Ok(update(day, id, event, len)?.then_some(id)),
    }
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
