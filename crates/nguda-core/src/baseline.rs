//! The day lock and its frozen baseline (AD-6, AD-7).
//!
//! A day's [`LockDecision`] is made at most once. Locking freezes a [`Baseline`]:
//! one [`BaselineItem`] per item that is not removed or carried over, with its
//! planned length and its re-flowed span at the lock instant. The baseline is
//! written once and never updated, so later plan edits never change "planned".

use jiff::Timestamp;
use jiff::civil::Date;

use crate::plan::{Day, ItemId};
use crate::reflow::{SlotKind, Span, reflow};

/// Whether a day's plan was frozen. Starts `None` and is decided at most once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LockDecision {
    /// Not decided yet.
    #[default]
    None,
    /// Locked: the day has a baseline.
    Locked,
    /// Decided against locking: the day never gets a baseline.
    NoLock,
}

/// The user's answer to "lock the day?".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LockChoice {
    Lock,
    NoLock,
}

impl LockChoice {
    /// The decision this choice makes.
    pub fn decision(self) -> LockDecision {
        match self {
            Self::Lock => LockDecision::Locked,
            Self::NoLock => LockDecision::NoLock,
        }
    }
}

/// One frozen item of a baseline: its planned length and scheduled span at lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaselineItem {
    pub item_id: ItemId,
    pub planned_secs: u64,
    /// The re-flowed span at lock, `None` for a done item without segments.
    pub span: Option<Span>,
}

/// A day's plan frozen at lock (AD-6). Written once, never updated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Baseline {
    pub date: Date,
    pub locked_at: Timestamp,
    /// In schedule order at lock.
    pub items: Vec<BaselineItem>,
}

/// The baseline of `day` locked at `now`: every slot of `reflow(day, now)` in
/// schedule order, except carried-over ones (re-flow already leaves removed items
/// out). Pure.
pub fn baseline(day: &Day, now: Timestamp) -> Baseline {
    let items = reflow(day, now)
        .slots()
        .iter()
        .filter(|s| s.kind != SlotKind::CarriedOver)
        .filter_map(|s| {
            day.item(s.item_id).map(|item| BaselineItem {
                item_id: s.item_id,
                planned_secs: item.planned_secs(),
                span: s.span,
            })
        })
        .collect();
    Baseline {
        date: day.date(),
        locked_at: now,
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DomainError;
    use crate::plan::{ItemStatus, PlanItem, PlanItemRecord};
    use crate::test_support::{closed, id, key, ts};

    fn record(n: u128, planned_secs: u64, order: &str) -> PlanItemRecord {
        PlanItemRecord {
            id: id(n),
            title: format!("item {n}"),
            planned_secs,
            order: key(order),
            fixed: false,
            pinned_start: None,
            status: ItemStatus::Pending,
            removed_at: None,
            segments: Vec::new(),
            carried_from: None,
        }
    }

    fn day(records: Vec<PlanItemRecord>) -> Day {
        let items = records.into_iter().map(|r| PlanItem::restore(r).unwrap());
        Day::restore(jiff::civil::date(2026, 11, 14), "UTC", items).unwrap()
    }

    fn span(start: i64, end: i64) -> Option<Span> {
        Some(Span {
            start: ts(start),
            end: ts(end),
        })
    }

    #[test]
    fn baseline_follows_the_schedule_and_skips_removed_and_carried_over() {
        let d = day(vec![
            record(1, 600, "b"),
            PlanItemRecord {
                status: ItemStatus::Removed,
                removed_at: Some(ts(5)),
                ..record(2, 600, "a")
            },
            PlanItemRecord {
                status: ItemStatus::CarriedOver,
                ..record(3, 600, "c")
            },
            PlanItemRecord {
                status: ItemStatus::Done,
                segments: vec![closed(10, 50)],
                ..record(4, 300, "d")
            },
            PlanItemRecord {
                fixed: true,
                pinned_start: Some(ts(200)),
                ..record(5, 100, "V")
            },
        ]);
        let b = baseline(&d, ts(100));
        assert_eq!(b.date, d.date());
        assert_eq!(b.locked_at, ts(100));
        assert_eq!(
            b.items,
            vec![
                BaselineItem {
                    item_id: id(4),
                    planned_secs: 300,
                    span: span(10, 50),
                },
                BaselineItem {
                    item_id: id(5),
                    planned_secs: 100,
                    span: span(200, 300),
                },
                BaselineItem {
                    item_id: id(1),
                    planned_secs: 600,
                    span: span(300, 900),
                },
            ]
        );
    }

    #[test]
    fn baseline_of_an_empty_day_is_empty() {
        let b = baseline(&day(vec![]), ts(0));
        assert!(b.items.is_empty());
    }

    #[test]
    fn a_day_is_decided_at_most_once() {
        let mut d = day(vec![]);
        assert_eq!(d.lock_decision(), LockDecision::None);
        d.decide_lock(LockChoice::Lock).unwrap();
        assert_eq!(d.lock_decision(), LockDecision::Locked);
        assert_eq!(
            d.decide_lock(LockChoice::NoLock),
            Err(DomainError::LockAlreadyDecided)
        );
        assert_eq!(d.lock_decision(), LockDecision::Locked);

        let mut d = day(vec![]);
        d.decide_lock(LockChoice::NoLock).unwrap();
        assert_eq!(d.lock_decision(), LockDecision::NoLock);
        assert_eq!(
            d.decide_lock(LockChoice::Lock),
            Err(DomainError::LockAlreadyDecided)
        );

        let mut d = day(vec![]);
        d.restore_lock_decision(LockDecision::Locked);
        assert_eq!(d.lock_decision(), LockDecision::Locked);
    }
}
