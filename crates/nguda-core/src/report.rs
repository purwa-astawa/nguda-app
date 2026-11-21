//! The day report: what a day was planned to be next to what happened (AD-6,
//! AD-16).
//!
//! "Planned" follows AD-6. On a [`LockDecision::Locked`] day it is the baseline
//! value ([`PlannedSource::Baseline`]), or none for an item missing from the
//! baseline ([`PlannedSource::Unplanned`]). On an undecided or `NoLock` day it is
//! the item's current planned length ([`PlannedSource::Unlocked`]). Actual time is
//! [`actual_secs`], the one formula. Pure: `now` comes in as an argument.

use std::collections::BTreeMap;

use jiff::Timestamp;
use jiff::civil::Date;

use crate::baseline::{Baseline, BaselineItem, LockDecision};
use crate::day_log::DayLogEntry;
use crate::plan::{Day, ItemId, ItemStatus, PlanItem};
use crate::reflow::reflow;
use crate::segment::actual_secs;

/// Where a row's planned value comes from (AD-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlannedSource {
    /// The frozen baseline value of a locked day.
    Baseline,
    /// The current plan of a day that is not locked.
    Unlocked,
    /// A locked day's item that is not in the baseline: no planned value.
    Unplanned,
}

/// One item of the day, with its planned, actual and difference figures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRow {
    pub item_id: ItemId,
    pub title: String,
    pub status: ItemStatus,
    /// The item this one was carried over from, on an earlier day.
    pub carried_from: Option<ItemId>,
    /// The item's tags, in order.
    pub tags: Vec<String>,
    /// `None` exactly when `planned_source` is `Unplanned`.
    pub planned_secs: Option<u64>,
    pub planned_source: PlannedSource,
    pub actual_secs: u64,
    /// `actual − planned`, signed; `None` when there is no planned value.
    pub difference_secs: Option<i64>,
}

/// One stretch of actual work. An open segment ends at `max(start, now)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActualSegment {
    pub item_id: ItemId,
    pub start: Timestamp,
    pub end: Timestamp,
    pub open: bool,
}

/// A day's planned-versus-actual report (AD-16 `QueryDay`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayReport {
    pub date: Date,
    pub lock_decision: LockDecision,
    /// When the day was locked; `None` unless it is locked with a baseline.
    pub locked_at: Option<Timestamp>,
    /// Every item of the day: schedule order, then removed items.
    pub rows: Vec<ReportRow>,
    /// The frozen baseline in schedule order at lock; empty unless locked.
    pub baseline: Vec<BaselineItem>,
    /// Every segment of the day's items, by start.
    pub segments: Vec<ActualSegment>,
    /// The day log in append order.
    pub markers: Vec<DayLogEntry>,
}

/// The report of `day` as of `now`, with its `baseline` (if it has one) and its
/// day `log`. A locked day without a baseline marks every row unplanned. Pure.
pub fn report(
    day: &Day,
    baseline: Option<&Baseline>,
    log: &[DayLogEntry],
    now: Timestamp,
) -> DayReport {
    let locked = day.lock_decision() == LockDecision::Locked;
    let baseline = if locked { baseline } else { None };
    let frozen: BTreeMap<ItemId, u64> = baseline
        .map(|b| {
            b.items
                .iter()
                .map(|i| (i.item_id, i.planned_secs))
                .collect()
        })
        .unwrap_or_default();
    let row = |item: &PlanItem| {
        let (planned_secs, planned_source) = if locked {
            match frozen.get(&item.id()) {
                Some(&secs) => (Some(secs), PlannedSource::Baseline),
                None => (None, PlannedSource::Unplanned),
            }
        } else {
            (Some(item.planned_secs()), PlannedSource::Unlocked)
        };
        let actual = actual_secs(item.segments(), now);
        ReportRow {
            item_id: item.id(),
            title: item.title().to_owned(),
            status: item.status(),
            carried_from: item.carried_from(),
            tags: item.tags().to_vec(),
            planned_secs,
            planned_source,
            actual_secs: actual,
            difference_secs: planned_secs.map(|p| signed(actual).saturating_sub(signed(p))),
        }
    };
    let scheduled = reflow(day, now)
        .slots()
        .iter()
        .filter_map(|s| day.item(s.item_id))
        .collect::<Vec<_>>();
    // Excluded items stay in the plan but out of the schedule: they keep a row.
    let excluded = day
        .items()
        .iter()
        .filter(|i| i.excluded() && i.status() != ItemStatus::Removed)
        .filter(|i| !scheduled.iter().any(|s| s.id() == i.id()));
    let removed = day
        .items()
        .iter()
        .filter(|i| i.status() == ItemStatus::Removed);
    let rows = scheduled
        .iter()
        .copied()
        .chain(excluded)
        .chain(removed)
        .map(row)
        .collect();

    let mut segments: Vec<ActualSegment> = day
        .items()
        .iter()
        .flat_map(|item| {
            item.segments().iter().map(move |s| ActualSegment {
                item_id: item.id(),
                start: s.started_at,
                end: s.ended_at.unwrap_or_else(|| s.started_at.max(now)),
                open: s.is_open(),
            })
        })
        .collect();
    segments.sort_by_key(|s| s.start);

    DayReport {
        date: day.date(),
        lock_decision: day.lock_decision(),
        locked_at: baseline.map(|b| b.locked_at),
        rows,
        baseline: baseline.map(|b| b.items.clone()).unwrap_or_default(),
        segments,
        markers: log.to_vec(),
    }
}

fn signed(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::day_log::DayLogKind;
    use crate::plan::PlanItemRecord;
    use crate::reflow::Span;
    use crate::segment::Segment;
    use crate::test_support::{closed, id, record, ts};

    fn day(decision: LockDecision, records: Vec<PlanItemRecord>) -> Day {
        let items = records.into_iter().map(|r| PlanItem::restore(r).unwrap());
        let mut d = Day::restore(jiff::civil::date(2026, 11, 21), "UTC", items).unwrap();
        d.restore_lock_decision(decision);
        d
    }

    fn frozen(n: u128, planned_secs: u64) -> BaselineItem {
        BaselineItem {
            item_id: id(n),
            planned_secs,
            span: Some(Span {
                start: ts(0),
                end: ts(1),
            }),
        }
    }

    fn baseline_of(d: &Day, items: Vec<BaselineItem>) -> Baseline {
        Baseline {
            date: d.date(),
            locked_at: ts(50),
            items,
        }
    }

    fn row(r: &DayReport, n: u128) -> &ReportRow {
        r.rows.iter().find(|row| row.item_id == id(n)).unwrap()
    }

    #[test]
    fn locked_and_extended_item_is_planned_from_the_baseline() {
        let d = day(
            LockDecision::Locked,
            vec![PlanItemRecord {
                status: ItemStatus::Done,
                segments: vec![closed(100, 2_000)],
                tags: vec!["deep".into(), "Work".into()],
                ..record(1, 1_800, "a")
            }],
        );
        let b = baseline_of(&d, vec![frozen(1, 1_500)]);
        let log = vec![DayLogEntry {
            at: ts(1_600),
            kind: DayLogKind::Extended {
                item_id: id(1),
                secs: 300,
            },
        }];
        let r = report(&d, Some(&b), &log, ts(3_000));
        assert_eq!(r.lock_decision, LockDecision::Locked);
        assert_eq!(r.locked_at, Some(ts(50)));
        assert_eq!(r.baseline, b.items);
        assert_eq!(r.markers, log);
        assert_eq!(
            r.rows,
            vec![ReportRow {
                item_id: id(1),
                title: "item 1".into(),
                status: ItemStatus::Done,
                carried_from: None,
                tags: vec!["deep".into(), "Work".into()],
                planned_secs: Some(1_500),
                planned_source: PlannedSource::Baseline,
                actual_secs: 1_900,
                difference_secs: Some(400),
            }]
        );
    }

    #[test]
    fn an_item_inserted_after_lock_is_unplanned() {
        let d = day(
            LockDecision::Locked,
            vec![
                record(1, 600, "a"),
                PlanItemRecord {
                    status: ItemStatus::Done,
                    segments: vec![closed(100, 700)],
                    ..record(2, 300, "b")
                },
            ],
        );
        let b = baseline_of(&d, vec![frozen(1, 600)]);
        let log = vec![DayLogEntry {
            at: ts(100),
            kind: DayLogKind::Inserted { item_id: id(2) },
        }];
        let r = report(&d, Some(&b), &log, ts(1_000));
        let b_row = row(&r, 2);
        assert_eq!(b_row.planned_secs, None);
        assert_eq!(b_row.planned_source, PlannedSource::Unplanned);
        assert_eq!(b_row.actual_secs, 600);
        assert_eq!(b_row.difference_secs, None);
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Baseline);
        assert_eq!(r.markers, log);
    }

    #[test]
    fn an_unlocked_day_plans_from_the_current_plan_with_no_baseline() {
        for decision in [LockDecision::NoLock, LockDecision::None] {
            let d = day(
                decision,
                vec![PlanItemRecord {
                    status: ItemStatus::Done,
                    segments: vec![closed(0, 1_000)],
                    ..record(1, 1_200, "a")
                }],
            );
            // A stray baseline is ignored on a day that is not locked.
            let b = baseline_of(&d, vec![frozen(1, 99)]);
            let r = report(&d, Some(&b), &[], ts(2_000));
            assert_eq!(r.lock_decision, decision);
            assert_eq!(r.locked_at, None);
            assert!(r.baseline.is_empty());
            let a = row(&r, 1);
            assert_eq!(a.planned_secs, Some(1_200));
            assert_eq!(a.planned_source, PlannedSource::Unlocked);
            assert_eq!(a.difference_secs, Some(-200));
        }
    }

    #[test]
    fn a_carried_over_item_is_a_row_planned_from_the_baseline() {
        let d = day(
            LockDecision::Locked,
            vec![PlanItemRecord {
                status: ItemStatus::CarriedOver,
                ..record(1, 600, "a")
            }],
        );
        let b = baseline_of(&d, vec![frozen(1, 900)]);
        let log = vec![DayLogEntry {
            at: ts(500),
            kind: DayLogKind::CarriedOver {
                item_id: id(1),
                to: id(9),
            },
        }];
        let r = report(&d, Some(&b), &log, ts(1_000));
        let a = row(&r, 1);
        assert_eq!(a.status, ItemStatus::CarriedOver);
        assert_eq!(a.planned_secs, Some(900));
        assert_eq!(a.planned_source, PlannedSource::Baseline);
        assert_eq!(a.actual_secs, 0);
        assert_eq!(a.difference_secs, Some(-900));
        assert_eq!(r.markers, log);

        // Tomorrow's copy is a row on tomorrow, linked back.
        let tomorrow = day(
            LockDecision::None,
            vec![PlanItemRecord {
                carried_from: Some(id(1)),
                ..record(9, 600, "a")
            }],
        );
        let r = report(&tomorrow, None, &[], ts(1_000));
        assert_eq!(row(&r, 9).carried_from, Some(id(1)));
        assert_eq!(row(&r, 9).planned_source, PlannedSource::Unlocked);
    }

    #[test]
    fn an_excluded_item_keeps_its_row_with_its_planned_value_on_either_kind_of_day() {
        let excluded = |n, order| PlanItemRecord {
            source: Some(crate::plan::CalendarRef {
                calendar_id: "cal".into(),
                source_id: format!("ev{n}"),
                instance_start: None,
                source_secs: 600,
            }),
            excluded: true,
            ..record(n, 600, order)
        };
        let removed = PlanItemRecord {
            status: ItemStatus::Removed,
            removed_at: Some(ts(10)),
            ..record(3, 600, "c")
        };
        let records = || vec![excluded(1, "a"), record(2, 300, "b"), removed.clone()];

        let d = day(LockDecision::Locked, records());
        let b = baseline_of(&d, vec![frozen(1, 700), frozen(2, 300)]);
        let r = report(&d, Some(&b), &[], ts(100));
        let ids: Vec<ItemId> = r.rows.iter().map(|r| r.item_id).collect();
        assert_eq!(
            ids,
            vec![id(2), id(1), id(3)],
            "after the schedule, before removed"
        );
        assert_eq!(row(&r, 1).status, ItemStatus::Pending);
        assert_eq!(row(&r, 1).planned_secs, Some(700));
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Baseline);

        let d = day(LockDecision::NoLock, records());
        let r = report(&d, None, &[], ts(100));
        assert_eq!(r.rows.len(), 3);
        assert_eq!(row(&r, 1).planned_secs, Some(600));
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Unlocked);
    }

    #[test]
    fn a_removed_item_is_a_row_after_the_schedule_on_either_kind_of_day() {
        let removed = |n, order| PlanItemRecord {
            status: ItemStatus::Removed,
            removed_at: Some(ts(10)),
            ..record(n, 600, order)
        };
        let records = || vec![removed(1, "a"), record(2, 300, "b"), removed(3, "c")];

        let d = day(LockDecision::Locked, records());
        let b = baseline_of(&d, vec![frozen(1, 700), frozen(2, 300)]);
        let r = report(&d, Some(&b), &[], ts(100));
        let ids: Vec<ItemId> = r.rows.iter().map(|r| r.item_id).collect();
        assert_eq!(ids, vec![id(2), id(1), id(3)]);
        assert_eq!(row(&r, 1).status, ItemStatus::Removed);
        assert_eq!(row(&r, 1).planned_secs, Some(700));
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Baseline);
        assert_eq!(row(&r, 3).planned_source, PlannedSource::Unplanned);

        let d = day(LockDecision::NoLock, records());
        let r = report(&d, None, &[], ts(100));
        assert_eq!(row(&r, 1).status, ItemStatus::Removed);
        assert_eq!(row(&r, 1).planned_secs, Some(600));
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Unlocked);
    }

    #[test]
    fn a_running_item_counts_up_to_now_and_its_segment_ends_at_now() {
        let d = day(
            LockDecision::NoLock,
            vec![
                PlanItemRecord {
                    status: ItemStatus::Running,
                    segments: vec![closed(0, 100), Segment::open(ts(300))],
                    ..record(1, 600, "b")
                },
                PlanItemRecord {
                    status: ItemStatus::Done,
                    segments: vec![closed(150, 250)],
                    ..record(2, 600, "a")
                },
            ],
        );
        let r = report(&d, None, &[], ts(400));
        assert_eq!(row(&r, 1).actual_secs, 200);
        assert_eq!(row(&r, 1).difference_secs, Some(-400));
        assert_eq!(
            r.segments,
            vec![
                ActualSegment {
                    item_id: id(1),
                    start: ts(0),
                    end: ts(100),
                    open: false,
                },
                ActualSegment {
                    item_id: id(2),
                    start: ts(150),
                    end: ts(250),
                    open: false,
                },
                ActualSegment {
                    item_id: id(1),
                    start: ts(300),
                    end: ts(400),
                    open: true,
                },
            ]
        );
        // A clock behind the open segment's start ends it at its start.
        let r = report(&d, None, &[], ts(200));
        assert_eq!(r.segments[2].end, ts(300));
    }

    #[test]
    fn a_locked_day_without_a_baseline_marks_every_row_unplanned() {
        let d = day(LockDecision::Locked, vec![record(1, 600, "a")]);
        let r = report(&d, None, &[], ts(0));
        assert_eq!(r.locked_at, None);
        assert!(r.baseline.is_empty());
        assert_eq!(row(&r, 1).planned_source, PlannedSource::Unplanned);
        assert_eq!(row(&r, 1).planned_secs, None);
    }
}
