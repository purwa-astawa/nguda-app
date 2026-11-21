//! Calendar import (CAP-2, AD-9): the calendar source port and the merge of fetched
//! events into the current plan.
//!
//! Field ownership: Google owns an imported item's `title`, its pin (only while the
//! item is still fixed), and its source length. `planned_secs` moves by the change
//! in the source length, so a local extension stays. The user owns `fixed`, `tags`,
//! `order`, and `status`, which the merge never touches. An imported item the user
//! removed stays removed and is never re-added. The merge changes the plan only,
//! never a baseline.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use jiff::Timestamp;

use crate::error::DomainError;
use crate::order::OrderKey;
use crate::plan::{CalendarRef, Day, ItemId, ItemStatus, PlanItem};

/// One timed event as fetched from a calendar. All-day, declined, cancelled, and
/// empty events never become a `FetchedEvent`: the source skips them.
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

    /// The timed events of the primary calendar that overlap
    /// `[window_start, window_end)`.
    fn fetch_primary(
        &self,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<Vec<FetchedEvent>>;
}

/// Merges `events` into `day` under the AD-9 ownership rules, minting ids for new
/// items with `new_id`. A new event becomes a fixed, pending item pinned at its
/// start, planned for its length, and ordered after every item of the day. A known
/// event (same `(calendar_id, source_id, instance_start)`) gets Google's fields; a
/// removed one is left alone. Events with no positive length are skipped. Returns
/// how many items were added or changed. On an error `day` may be half-changed:
/// callers merge into a copy.
pub fn merge(
    day: &mut Day,
    events: &[FetchedEvent],
    mut new_id: impl FnMut() -> ItemId,
) -> Result<usize, DomainError> {
    let mut changed = 0;
    for event in events {
        let Some(len) = event.length_secs() else {
            continue;
        };
        let key = CalendarRef {
            calendar_id: event.calendar_id.clone(),
            source_id: event.source_id.clone(),
            instance_start: event.instance_start,
            source_secs: len,
        };
        let known = day
            .items()
            .iter()
            .find(|i| i.source().is_some_and(|s| s.same_key(&key)))
            .map(|i| (i.id(), i.status()));
        match known {
            None => {
                let order = OrderKey::between(last_order(day), None)?;
                day.add(PlanItem::imported(
                    new_id(),
                    event.title.clone(),
                    event.start,
                    order,
                    key,
                ))?;
                changed += 1;
            }
            Some((_, ItemStatus::Removed)) => {}
            Some((item_id, _)) => {
                if update(day, item_id, event, len)? {
                    changed += 1;
                }
            }
        }
    }
    Ok(changed)
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
        let n = merge(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
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
        merge(
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
        merge(&mut d, &events, ids()).unwrap();
        let before = d.clone();
        assert_eq!(merge(&mut d, &events, ids()).unwrap(), 0);
        assert_eq!(d, before);
    }

    #[test]
    fn a_moved_and_retitled_event_updates_pin_and_title() {
        let mut d = empty_day();
        merge(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        let n = merge(&mut d, &[event("e1", "Daily", 7_200, 9_000)], ids()).unwrap();
        assert_eq!(n, 1);
        let i = only(&d);
        assert_eq!(i.title(), "Daily");
        assert_eq!(i.pinned_start(), Some(ts(7_200)));
        assert_eq!(i.planned_secs(), 1_800);
    }

    #[test]
    fn a_moved_event_leaves_a_flexible_item_flexible() {
        let mut d = empty_day();
        merge(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        d.set_flexible(id(1000)).unwrap();
        d.set_tags(id(1000), vec!["meet".into()]).unwrap();
        d.set_order(id(1000), key("a")).unwrap();
        merge(&mut d, &[event("e1", "Daily", 7_200, 9_000)], ids()).unwrap();
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
        merge(&mut d, &[event("e1", "Review", 0, 1_800)], ids()).unwrap();
        d.set_planned_secs(id(1000), 2_400).unwrap();
        merge(&mut d, &[event("e1", "Review", 0, 3_600)], ids()).unwrap();
        let i = only(&d);
        assert_eq!(i.planned_secs(), 4_200);
        assert_eq!(i.source().unwrap().source_secs, 3_600);
    }

    #[test]
    fn a_shortened_event_never_drops_below_the_minimum() {
        let mut d = empty_day();
        merge(&mut d, &[event("e1", "Review", 0, 3_600)], ids()).unwrap();
        d.set_planned_secs(id(1000), 600).unwrap();
        merge(&mut d, &[event("e1", "Review", 0, 60)], ids()).unwrap();
        let i = only(&d);
        assert_eq!(i.planned_secs(), MIN_PLANNED_SECS);
        assert_eq!(i.source().unwrap().source_secs, 60);
    }

    #[test]
    fn a_removed_imported_item_stays_removed_and_is_not_re_added() {
        let mut d = empty_day();
        let events = [event("e1", "Standup", 3_600, 5_400)];
        merge(&mut d, &events, ids()).unwrap();
        d.remove(id(1000), ts(100)).unwrap();
        let before = d.clone();
        let moved = [event("e1", "Daily", 7_200, 9_000)];
        assert_eq!(merge(&mut d, &moved, ids()).unwrap(), 0);
        assert_eq!(d, before);
    }

    #[test]
    fn recurring_instances_stay_distinct_by_instance_start() {
        let mut d = empty_day();
        let mut a = event("r1", "Weekly", 3_600, 5_400);
        a.instance_start = Some(ts(3_600));
        let mut b = event("r1", "Weekly", 90_000, 91_800);
        b.instance_start = Some(ts(90_000));
        assert_eq!(merge(&mut d, &[a.clone(), b.clone()], ids()).unwrap(), 2);
        assert_eq!(merge(&mut d, &[a, b], ids()).unwrap(), 0);
        assert_eq!(d.items().len(), 2);
    }

    #[test]
    fn an_event_with_no_positive_length_is_skipped() {
        let mut d = empty_day();
        let events = [
            event("e1", "zero", 3_600, 3_600),
            event("e2", "neg", 3_600, 0),
        ];
        assert_eq!(merge(&mut d, &events, ids()).unwrap(), 0);
        assert!(d.items().is_empty());
    }

    #[test]
    fn local_items_are_never_matched() {
        let mut d = empty_day();
        d.add(PlanItem::new(id(1), "Standup", 600, key("V")))
            .unwrap();
        merge(&mut d, &[event("e1", "Standup", 3_600, 5_400)], ids()).unwrap();
        assert_eq!(d.items().len(), 2);
        assert_eq!(d.item(id(1)).unwrap().source(), None);
    }

    #[test]
    fn error_messages_carry_the_message_only() {
        let e = CalendarError::new(CalendarErrorKind::Other, "HTTP 503");
        assert_eq!(e.to_string(), "HTTP 503");
    }
}
