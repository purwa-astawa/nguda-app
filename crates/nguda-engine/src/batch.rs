//! Turning a day change into one store batch.

use nguda_core::{Day, StoreBatch, StoreOp};

/// The writes that take the stored `old` day to `new`, ordered so the store's
/// single-open-segment index never sees two open rows:
///
/// 1. `PutDay` (always; an idempotent upsert),
/// 2. `PutItem` for each new or changed item,
/// 3. `CloseSegment` for each segment open in `old` and closed in `new`,
/// 4. each segment new in `new`: closed ones first (open then close), then the open
///    one.
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
    batch
}
