//! Turning a day change into one store batch.

use nguda_core::{Day, StoreBatch, StoreOp};

/// The writes that take the stored `old` day to `new`, ordered so the store's
/// single-open-segment index never sees two open rows:
///
/// 1. `PutDay` (always; an idempotent upsert),
/// 2. `PutItem` for each new or changed item,
/// 3. `CloseSegment` for each segment open in `old` and closed in `new`,
/// 4. each segment new in `new`: closed ones first (open then close), then the open
///    one,
/// 5. `SetInterruptStack` when the interrupt stack changed.
///
/// Callers append their extra ops (day log, heartbeat) after these.
pub(crate) fn diff(old: &Day, new: &Day) -> StoreBatch {
    debug_assert_eq!(old.date(), new.date());
    // A day never deletes items.
    debug_assert!(old.items().iter().all(|o| new.item(o.id()).is_some()));

    let mut batch = StoreBatch::new();
    batch.push(StoreOp::PutDay {
        date: new.date(),
        tz: new.tz().to_owned(),
    });

    for item in new.items() {
        if old.item(item.id()) != Some(item) {
            batch.push(StoreOp::PutItem {
                date: new.date(),
                item: item.clone(),
            });
        }
    }

    for item in new.items() {
        let Some(before) = old.item(item.id()) else {
            continue;
        };
        for seg in before.segments().iter().filter(|s| s.is_open()) {
            let closed = item
                .segments()
                .iter()
                .find(|s| s.started_at == seg.started_at)
                .and_then(|s| s.ended_at);
            if let Some(ended_at) = closed {
                batch.push(StoreOp::CloseSegment {
                    item_id: item.id(),
                    started_at: seg.started_at,
                    ended_at,
                });
            }
        }
    }

    let mut open = None;
    for item in new.items() {
        let before = old.item(item.id()).map(|o| o.segments()).unwrap_or(&[]);
        for seg in item.segments() {
            if before.iter().any(|b| b.started_at == seg.started_at) {
                continue;
            }
            let op = StoreOp::OpenSegment {
                item_id: item.id(),
                started_at: seg.started_at,
            };
            match seg.ended_at {
                Some(ended_at) => {
                    batch.push(op);
                    batch.push(StoreOp::CloseSegment {
                        item_id: item.id(),
                        started_at: seg.started_at,
                        ended_at,
                    });
                }
                None => open = Some(op),
            }
        }
    }
    if let Some(op) = open {
        batch.push(op);
    }
    if old.interrupt_stack() != new.interrupt_stack() {
        batch.push(StoreOp::SetInterruptStack {
            date: new.date(),
            items: new.interrupt_stack().to_vec(),
        });
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{day_running_other, day1, id, item, ts};

    fn ops(batch: &StoreBatch) -> Vec<StoreOp> {
        batch.ops().to_vec()
    }

    fn put_day() -> StoreOp {
        StoreOp::PutDay {
            date: day1(),
            tz: "UTC".into(),
        }
    }

    fn put(day: &Day, n: u128) -> StoreOp {
        StoreOp::PutItem {
            date: day.date(),
            item: day.item(id(n)).unwrap().clone(),
        }
    }

    fn open_op(n: u128, at: i64) -> StoreOp {
        StoreOp::OpenSegment {
            item_id: id(n),
            started_at: ts(at),
        }
    }

    fn close_op(n: u128, from: i64, to: i64) -> StoreOp {
        StoreOp::CloseSegment {
            item_id: id(n),
            started_at: ts(from),
            ended_at: ts(to),
        }
    }

    #[test]
    fn diff_of_an_unchanged_day_is_only_put_day() {
        let day = day_running_other();
        assert_eq!(ops(&diff(&day, &day)), vec![put_day()]);
    }

    #[test]
    fn diff_puts_a_new_item_and_opens_its_segment() {
        let old = Day::new(day1(), "UTC");
        let mut pending = old.clone();
        pending.add(item(1, "a", "V")).unwrap();
        assert_eq!(
            ops(&diff(&old, &pending)),
            vec![put_day(), put(&pending, 1)]
        );

        let mut started = pending.clone();
        started.start(id(1), ts(10)).unwrap();
        assert_eq!(
            ops(&diff(&old, &started)),
            vec![put_day(), put(&started, 1), open_op(1, 10)]
        );
    }

    #[test]
    fn diff_closes_before_it_opens() {
        // Pause + resume of one item.
        let old = day_running_other();
        let mut new = old.clone();
        new.pause(id(100), ts(1_040)).unwrap();
        new.resume(id(100), ts(1_040)).unwrap();
        assert_eq!(
            ops(&diff(&old, &new)),
            vec![
                put_day(),
                put(&new, 100),
                close_op(100, 1_000, 1_040),
                open_op(100, 1_040)
            ]
        );

        // Pausing one item and starting a new one, listed first.
        let mut old = Day::new(day1(), "UTC");
        old.add(item(1, "first", "U")).unwrap();
        old.add(item(100, "other", "V")).unwrap();
        old.start(id(100), ts(1_000)).unwrap();
        let mut new = old.clone();
        new.pause(id(100), ts(1_050)).unwrap();
        new.start(id(1), ts(1_050)).unwrap();
        assert_eq!(
            ops(&diff(&old, &new)),
            vec![
                put_day(),
                put(&new, 1),
                put(&new, 100),
                close_op(100, 1_000, 1_050),
                open_op(1, 1_050)
            ]
        );
    }

    #[test]
    fn diff_sets_the_interrupt_stack_last_when_it_changed() {
        let old = day_running_other();
        let mut new = old.clone();
        new.interrupt(id(100), item(1, "n", "U"), ts(1_050))
            .unwrap();
        let stack = StoreOp::SetInterruptStack {
            date: day1(),
            items: vec![id(100)],
        };
        assert_eq!(
            ops(&diff(&old, &new)),
            vec![
                put_day(),
                put(&new, 100),
                put(&new, 1),
                close_op(100, 1_000, 1_050),
                open_op(1, 1_050),
                stack,
            ]
        );
        // Popping it writes an empty stack.
        let mut popped = new.clone();
        popped.switch(id(1), ts(1_100)).unwrap();
        assert_eq!(
            ops(&diff(&new, &popped)).last(),
            Some(&StoreOp::SetInterruptStack {
                date: day1(),
                items: vec![],
            })
        );
        // An unchanged stack writes nothing.
        let mut renamed = new.clone();
        renamed.set_title(id(100), "x").unwrap();
        assert_eq!(
            ops(&diff(&new, &renamed)),
            vec![put_day(), put(&renamed, 100)]
        );
    }

    #[test]
    fn diff_writes_new_closed_segments_before_the_new_open_one() {
        let old = Day::new(day1(), "UTC");
        let mut new = old.clone();
        new.add(item(1, "a", "V")).unwrap();
        new.add(item(2, "b", "W")).unwrap();
        new.start(id(2), ts(10)).unwrap();
        new.pause(id(2), ts(20)).unwrap();
        new.start(id(1), ts(30)).unwrap();
        new.pause(id(1), ts(40)).unwrap();
        new.resume(id(1), ts(50)).unwrap();
        assert_eq!(
            ops(&diff(&old, &new)),
            vec![
                put_day(),
                put(&new, 1),
                put(&new, 2),
                open_op(1, 30),
                close_op(1, 30, 40),
                open_op(2, 10),
                close_op(2, 10, 20),
                open_op(1, 50),
            ]
        );
    }
}
