//! Re-flow: the day's schedule computed from its plan at an instant (AD-1, AD-6).
//!
//! Scheduled times are never stored. [`reflow`] derives them from the items and
//! "now", which always comes in as a parameter:
//!
//! - a done item spans its actual time, first segment start to last segment end;
//! - the running item ends at its segment start plus its remaining planned time;
//! - pending and paused fixed items sit at `[pinned_start, pinned_start + planned]`;
//! - pending and paused flexible items flow from `max(now, running end)` in
//!   order-key order, each with its remaining time, moved past any fixed span they
//!   would overlap and never split;
//! - carried-over items have no span and come last; removed items are left out.

use std::time::Duration;

use jiff::Timestamp;

use crate::plan::{Day, ItemId, ItemStatus, PlanItem};
use crate::segment::closed_secs;

/// A scheduled stretch of time, `start` inclusive and `end` exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: Timestamp,
    pub end: Timestamp,
}

/// Why an item sits where it does in the schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotKind {
    /// Finished: its actual span.
    Done,
    /// Running: from its open segment's start to its planned end.
    Running,
    /// Pending or paused and fixed: at its pin.
    Fixed,
    /// Pending or paused and flexible: flowed around the fixed items.
    Flexible,
    /// Moved to another day: no span.
    CarriedOver,
}

/// One item's place in the schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub item_id: ItemId,
    pub kind: SlotKind,
    pub span: Option<Span>,
}

/// The day's non-removed items in schedule order, each with its span if it has one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    slots: Vec<Slot>,
}

impl Schedule {
    /// Every slot in schedule order.
    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn slot(&self, id: ItemId) -> Option<&Slot> {
        self.slots.iter().find(|s| s.item_id == id)
    }

    /// The first pending or paused item in schedule order, or `None` when nothing
    /// is left.
    pub fn next(&self) -> Option<ItemId> {
        self.slots
            .iter()
            .find(|s| matches!(s.kind, SlotKind::Fixed | SlotKind::Flexible))
            .map(|s| s.item_id)
    }

    /// Pending or paused flexible items that end after `work_end`, in schedule
    /// order. Ending exactly at `work_end` is not overflow.
    pub fn overflow(&self, work_end: Timestamp) -> Vec<ItemId> {
        self.slots
            .iter()
            .filter(|s| s.kind == SlotKind::Flexible)
            .filter(|s| s.span.is_some_and(|span| span.end > work_end))
            .map(|s| s.item_id)
            .collect()
    }
}

/// `start + secs`, saturating to [`Timestamp::MAX`].
fn add_secs(start: Timestamp, secs: u64) -> Timestamp {
    start
        .checked_add(Duration::from_secs(secs))
        .unwrap_or(Timestamp::MAX)
}

/// First segment start to last segment end, or `None` without segments.
fn actual_span(item: &PlanItem) -> Option<Span> {
    let segs = item.segments();
    let start = segs.iter().map(|s| s.started_at).min()?;
    let end = segs
        .iter()
        .map(|s| s.ended_at.unwrap_or(s.started_at))
        .max()
        .unwrap_or(start)
        .max(start);
    Some(Span { start, end })
}

fn slot(item: &PlanItem, kind: SlotKind, span: Option<Span>) -> Slot {
    Slot {
        item_id: item.id(),
        kind,
        span,
    }
}

/// The schedule of `day` as of `now`. Pure: the same day and instant always give
/// the same schedule.
pub fn reflow(day: &Day, now: Timestamp) -> Schedule {
    let live = |s: ItemStatus| day.items().iter().filter(move |i| i.status() == s);
    let open = |i: &&PlanItem| matches!(i.status(), ItemStatus::Pending | ItemStatus::Paused);

    // Build order, which also breaks ties of the sort below: done, running, fixed
    // by pin, flexible by order key.
    let mut slots: Vec<Slot> = live(ItemStatus::Done)
        .map(|i| slot(i, SlotKind::Done, actual_span(i)))
        .collect();

    let mut cursor = now;
    for item in live(ItemStatus::Running) {
        let span = item.running_view().map(|r| {
            let left = item.planned_secs().saturating_sub(r.accumulated_secs);
            Span {
                start: r.segment_started_at,
                end: add_secs(r.segment_started_at, left),
            }
        });
        if let Some(span) = span {
            cursor = cursor.max(span.end);
        }
        slots.push(slot(item, SlotKind::Running, span));
    }

    let mut fixed: Vec<(&PlanItem, Span)> = day
        .items()
        .iter()
        .filter(open)
        .filter_map(|i| {
            let start = i.pinned_start()?;
            Some((
                i,
                Span {
                    start,
                    end: add_secs(start, i.planned_secs()),
                },
            ))
        })
        .collect();
    fixed.sort_by_key(|(_, span)| span.start);
    slots.extend(
        fixed
            .iter()
            .map(|(i, span)| slot(i, SlotKind::Fixed, Some(*span))),
    );

    let mut flexible: Vec<&PlanItem> = day
        .items()
        .iter()
        .filter(open)
        .filter(|i| !i.fixed())
        .collect();
    flexible.sort_by(|a, b| a.order().cmp(b.order()));
    for item in flexible {
        let len = item
            .planned_secs()
            .saturating_sub(closed_secs(item.segments()));
        let mut start = cursor;
        // Fixed spans are sorted by start, so one pass suffices: a span earlier
        // than the moved start cannot overlap it.
        for (_, f) in &fixed {
            if f.start < add_secs(start, len) && f.end > start {
                start = f.end;
            }
        }
        let end = add_secs(start, len);
        cursor = end;
        slots.push(slot(item, SlotKind::Flexible, Some(Span { start, end })));
    }

    // Stable: equal starts keep the build order. Spanless slots (a done item
    // without segments) follow the spanned ones.
    slots.sort_by_key(|s| match s.span {
        Some(span) => (false, span.start),
        None => (true, Timestamp::MIN),
    });

    let mut carried: Vec<&PlanItem> = live(ItemStatus::CarriedOver).collect();
    carried.sort_by(|a, b| a.order().cmp(b.order()));
    slots.extend(
        carried
            .into_iter()
            .map(|i| slot(i, SlotKind::CarriedOver, None)),
    );

    Schedule { slots }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::order::OrderKey;
    use crate::plan::PlanItemRecord;
    use crate::segment::Segment;
    use crate::test_support::{FakeClock, closed, ts};
    use uuid::Uuid;

    fn id(n: u128) -> ItemId {
        ItemId(Uuid::from_u128(n))
    }

    fn record(n: u128, planned_secs: u64, order: &str) -> PlanItemRecord {
        PlanItemRecord {
            id: id(n),
            title: format!("item {n}"),
            planned_secs,
            order: OrderKey::new(order).unwrap(),
            fixed: false,
            pinned_start: None,
            status: ItemStatus::Pending,
            removed_at: None,
            segments: Vec::new(),
        }
    }

    fn flexible(n: u128, planned_secs: u64, order: &str) -> PlanItemRecord {
        record(n, planned_secs, order)
    }

    fn fixed(n: u128, pinned: i64, planned_secs: u64) -> PlanItemRecord {
        PlanItemRecord {
            fixed: true,
            pinned_start: Some(ts(pinned)),
            ..record(n, planned_secs, "V")
        }
    }

    fn running(n: u128, planned_secs: u64, segments: Vec<Segment>) -> PlanItemRecord {
        PlanItemRecord {
            status: ItemStatus::Running,
            segments,
            ..record(n, planned_secs, "V")
        }
    }

    fn day(records: Vec<PlanItemRecord>) -> Day {
        let items = records.into_iter().map(|r| PlanItem::restore(r).unwrap());
        Day::restore(jiff::civil::date(2026, 11, 11), "UTC", items).unwrap()
    }

    fn span(s: &Schedule, n: u128) -> Option<(i64, i64)> {
        s.slot(id(n))
            .unwrap()
            .span
            .map(|sp| (sp.start.as_second(), sp.end.as_second()))
    }

    fn order(s: &Schedule) -> Vec<ItemId> {
        s.slots().iter().map(|s| s.item_id).collect()
    }

    #[test]
    fn flexible_items_flow_around_a_fixed_item() {
        let clock = FakeClock::at(0);
        let d = day(vec![
            flexible(1, 600, "a"),
            flexible(2, 600, "b"),
            fixed(3, 900, 300),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((0, 600)));
        assert_eq!(span(&s, 3), Some((900, 1200)));
        assert_eq!(span(&s, 2), Some((1200, 1800)), "moved past the fixed item");
        assert_eq!(order(&s), vec![id(1), id(3), id(2)]);
        assert_eq!(s.slot(id(3)).unwrap().kind, SlotKind::Fixed);
        assert_eq!(s.slot(id(2)).unwrap().kind, SlotKind::Flexible);
    }

    #[test]
    fn a_flexible_item_that_touches_a_fixed_one_fits_before_it() {
        let clock = FakeClock::at(0);
        let d = day(vec![flexible(1, 600, "a"), fixed(2, 600, 300)]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((0, 600)));
        assert_eq!(span(&s, 2), Some((600, 900)));
        assert_eq!(order(&s), vec![id(1), id(2)]);
    }

    #[test]
    fn a_flexible_item_skips_several_overlapping_fixed_items() {
        let clock = FakeClock::at(0);
        let d = day(vec![
            flexible(1, 200, "a"),
            fixed(2, 0, 100),
            fixed(3, 50, 1950),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((2000, 2200)));
    }

    #[test]
    fn the_running_item_ends_at_its_planned_end_and_flow_follows() {
        let clock = FakeClock::at(300);
        let d = day(vec![
            running(1, 600, vec![Segment::open(ts(100))]),
            flexible(2, 600, "a"),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((100, 700)));
        assert_eq!(s.slot(id(1)).unwrap().kind, SlotKind::Running);
        assert_eq!(span(&s, 2), Some((700, 1300)));
        assert_eq!(s.next(), Some(id(2)));
    }

    #[test]
    fn running_end_subtracts_banked_time() {
        let clock = FakeClock::at(1_000);
        let d = day(vec![running(
            1,
            600,
            vec![closed(0, 200), Segment::open(ts(1_000))],
        )]);
        assert_eq!(span(&reflow(&d, clock.now()), 1), Some((1_000, 1_400)));
    }

    #[test]
    fn a_running_item_past_its_planned_end_lets_the_flow_start_now() {
        let clock = FakeClock::at(1_000);
        let d = day(vec![
            running(1, 600, vec![Segment::open(ts(0))]),
            flexible(2, 600, "a"),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((0, 600)));
        assert_eq!(span(&s, 2), Some((1_000, 1_600)));
    }

    #[test]
    fn a_paused_flexible_item_keeps_only_its_remaining_time() {
        let clock = FakeClock::at(0);
        let d = day(vec![PlanItemRecord {
            status: ItemStatus::Paused,
            segments: vec![closed(-1_000, -800)],
            ..flexible(1, 600, "a")
        }]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 1), Some((0, 400)));
        assert_eq!(s.slot(id(1)).unwrap().kind, SlotKind::Flexible);
    }

    #[test]
    fn a_past_pin_keeps_its_time_and_is_next() {
        let clock = FakeClock::at(5_000);
        let d = day(vec![flexible(1, 600, "a"), fixed(2, 0, 300)]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 2), Some((0, 300)));
        assert_eq!(span(&s, 1), Some((5_000, 5_600)));
        assert_eq!(order(&s), vec![id(2), id(1)]);
        assert_eq!(s.next(), Some(id(2)));
    }

    #[test]
    fn done_spans_its_actual_time_carried_over_comes_last_and_removed_is_absent() {
        let clock = FakeClock::at(5_000);
        let d = day(vec![
            PlanItemRecord {
                status: ItemStatus::CarriedOver,
                ..flexible(1, 600, "a")
            },
            PlanItemRecord {
                status: ItemStatus::Done,
                segments: vec![closed(100, 200), closed(300, 450)],
                ..flexible(2, 600, "b")
            },
            PlanItemRecord {
                status: ItemStatus::Removed,
                removed_at: Some(ts(10)),
                ..flexible(3, 600, "c")
            },
            flexible(4, 600, "d"),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 2), Some((100, 450)));
        assert_eq!(s.slot(id(2)).unwrap().kind, SlotKind::Done);
        assert_eq!(span(&s, 1), None);
        assert_eq!(s.slot(id(1)).unwrap().kind, SlotKind::CarriedOver);
        assert!(s.slot(id(3)).is_none());
        assert_eq!(order(&s), vec![id(2), id(4), id(1)]);
        assert_eq!(s.next(), Some(id(4)));
    }

    #[test]
    fn a_done_item_without_segments_has_no_span() {
        let d = day(vec![PlanItemRecord {
            status: ItemStatus::Done,
            ..flexible(1, 600, "a")
        }]);
        let s = reflow(&d, ts(0));
        assert_eq!(s.slot(id(1)).unwrap().span, None);
        assert_eq!(s.next(), None);
    }

    #[test]
    fn overflow_lists_flexible_items_ending_after_work_end() {
        let clock = FakeClock::at(0);
        let d = day(vec![
            flexible(1, 600, "a"),
            flexible(2, 600, "b"),
            flexible(3, 600, "c"),
        ]);
        let s = reflow(&d, clock.now());
        assert_eq!(span(&s, 3), Some((1_200, 1_800)));
        assert_eq!(s.overflow(ts(1_200)), vec![id(3)]);
        assert_eq!(s.overflow(ts(0)), vec![id(1), id(2), id(3)]);
        assert!(s.overflow(ts(1_800)).is_empty());
    }

    #[test]
    fn overflow_never_lists_running_fixed_or_done_items() {
        let clock = FakeClock::at(1_000);
        let d = day(vec![
            running(1, 6_000, vec![Segment::open(ts(500))]),
            fixed(2, 9_000, 600),
            PlanItemRecord {
                status: ItemStatus::Done,
                segments: vec![closed(0, 400)],
                ..flexible(3, 600, "a")
            },
        ]);
        assert!(reflow(&d, clock.now()).overflow(ts(0)).is_empty());
    }

    #[test]
    fn equal_starts_keep_the_build_order() {
        // A done item, a fixed item and a flexible item all starting at 0.
        let clock = FakeClock::at(0);
        let d = day(vec![
            flexible(1, 0, "a"),
            fixed(2, 0, 0),
            PlanItemRecord {
                status: ItemStatus::Done,
                segments: vec![closed(0, 0)],
                ..flexible(3, 600, "b")
            },
        ]);
        assert_eq!(order(&reflow(&d, clock.now())), vec![id(3), id(2), id(1)]);
    }

    #[test]
    fn timestamps_saturate_instead_of_panicking() {
        let d = day(vec![fixed(1, 0, u64::MAX), flexible(2, u64::MAX, "a")]);
        let s = reflow(&d, ts(0));
        let end = |n| s.slot(id(n)).unwrap().span.unwrap().end;
        assert_eq!(end(1), Timestamp::MAX);
        assert_eq!(end(2), Timestamp::MAX);
        let near_max = Timestamp::MAX.as_second() - 10;
        let s = reflow(&day(vec![flexible(3, 600, "a")]), ts(near_max));
        assert_eq!(s.slot(id(3)).unwrap().span.unwrap().end, Timestamp::MAX);
    }

    #[test]
    fn the_empty_day_has_an_empty_schedule() {
        let s = reflow(&day(vec![]), ts(0));
        assert!(s.slots().is_empty());
        assert_eq!(s, Schedule::default());
        assert_eq!(s.next(), None);
    }
}
