//! Plan commands as pure functions on a copy of the day (AD-3, AD-15).
//!
//! Each command validates its input and applies domain transitions to `day`,
//! returning the store ops to append after [`crate::batch::diff`] (the extras), or a
//! [`Reject`]. On a reject `day` may be half-changed: callers pass a copy and drop it.

use std::fmt;

use nguda_core::{
    Day, DayLogEntry, DayLogKind, DomainError, IdGen, ItemId, ItemStatus, OrderKey, PlanItem,
    StoreOp, Timestamp,
};

use crate::{EngineMsg, ReorderAnchor};

/// Why a command was rejected. Its message holds ids, revs and kinds only, never
/// titles (AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reject {
    /// The command's `expected_rev` is not the engine's current rev.
    StaleRev { expected: u64, current: u64 },
    /// The input breaks a command rule (blank title, fixed anchor, ...).
    Invalid {
        command: &'static str,
        reason: String,
    },
    /// A domain transition refused the change.
    Domain {
        command: &'static str,
        error: DomainError,
    },
}

impl Reject {
    /// A short kind for logs.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::StaleRev { .. } => "stale_rev",
            Self::Invalid { .. } => "invalid_input",
            Self::Domain { error, .. } => domain_kind(error),
        }
    }

    fn invalid(command: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            command,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleRev { expected, current } => {
                write!(f, "stale rev: expected {expected}, current {current}")
            }
            Self::Invalid { command, reason } => write!(f, "{command}: {reason}"),
            // DomainError's Display holds ids, states and actions only.
            Self::Domain { command, error } => write!(f, "{command}: {error}"),
        }
    }
}

/// A domain error's kind, for logs: never its ids' titles or a `Debug` dump.
pub(crate) fn domain_kind(e: &DomainError) -> &'static str {
    match e {
        DomainError::AnotherItemRunning => "another_item_running",
        DomainError::InvalidTransition { .. } => "invalid_transition",
        DomainError::InvalidOrder => "invalid_order",
        DomainError::UnknownItem(_) => "unknown_item",
        DomainError::DuplicateItem(_) => "duplicate_item",
        DomainError::InvalidRecord { .. } => "invalid_record",
    }
}

/// The message's name, for logs and reject messages.
pub(crate) fn name(msg: &EngineMsg) -> &'static str {
    match msg {
        EngineMsg::CreateTask { .. } => "CreateTask",
        EngineMsg::EditTask { .. } => "EditTask",
        EngineMsg::ReorderTask { .. } => "ReorderTask",
        EngineMsg::ToggleFixed { .. } => "ToggleFixed",
        EngineMsg::RemoveTask { .. } => "RemoveTask",
        EngineMsg::Start { .. } => "Start",
        EngineMsg::Pause { .. } => "Pause",
    }
}

/// The item a message targets, for logs.
pub(crate) fn target(msg: &EngineMsg) -> Option<ItemId> {
    match msg {
        EngineMsg::CreateTask { .. } => None,
        EngineMsg::EditTask { item_id, .. }
        | EngineMsg::ReorderTask { item_id, .. }
        | EngineMsg::ToggleFixed { item_id, .. }
        | EngineMsg::RemoveTask { item_id }
        | EngineMsg::Start { item_id, .. }
        | EngineMsg::Pause { item_id } => Some(*item_id),
    }
}

/// Applies `msg` to `day` at `now`. A rev-checked command whose `expected_rev` is
/// not `rev` is rejected before anything else.
pub(crate) fn apply(
    msg: EngineMsg,
    day: &mut Day,
    now: Timestamp,
    ids: &dyn IdGen,
    rev: u64,
) -> Result<Vec<StoreOp>, Reject> {
    let check = |expected: u64| {
        if expected == rev {
            Ok(())
        } else {
            Err(Reject::StaleRev {
                expected,
                current: rev,
            })
        }
    };
    match msg {
        EngineMsg::CreateTask {
            title,
            planned_secs,
        } => create_task(day, ids, title, planned_secs),
        EngineMsg::EditTask {
            item_id,
            title,
            planned_secs,
        } => edit_task(day, item_id, title, planned_secs),
        EngineMsg::ReorderTask {
            item_id,
            anchor,
            expected_rev,
        } => {
            check(expected_rev)?;
            reorder_task(day, item_id, anchor)
        }
        EngineMsg::ToggleFixed {
            item_id,
            pinned_start,
            expected_rev,
        } => {
            check(expected_rev)?;
            toggle_fixed(day, item_id, pinned_start)
        }
        EngineMsg::RemoveTask { item_id } => remove_task(day, now, item_id),
        EngineMsg::Start {
            item_id,
            expected_rev,
        } => {
            check(expected_rev)?;
            start(day, now, item_id)
        }
        EngineMsg::Pause { item_id } => pause(day, now, item_id),
    }
}

fn domain(command: &'static str) -> impl Fn(DomainError) -> Reject {
    move |error| Reject::Domain { command, error }
}

/// Non-removed flexible items sorted by order key.
fn flexible_in_order(day: &Day) -> Vec<&PlanItem> {
    let mut items: Vec<&PlanItem> = day
        .items()
        .iter()
        .filter(|i| i.status() != ItemStatus::Removed && !i.fixed())
        .collect();
    items.sort_by(|a, b| a.order().cmp(b.order()));
    items
}

/// The item `id`, unless it is unknown or removed.
fn live_item<'d>(
    day: &'d Day,
    command: &'static str,
    id: ItemId,
    role: &str,
) -> Result<&'d PlanItem, Reject> {
    let item = day
        .item(id)
        .ok_or(DomainError::UnknownItem(id))
        .map_err(domain(command))?;
    if item.status() == ItemStatus::Removed {
        return Err(Reject::invalid(command, format!("{role} {id} is removed")));
    }
    Ok(item)
}

fn check_title(command: &'static str, title: &str) -> Result<(), Reject> {
    if title.trim().is_empty() {
        return Err(Reject::invalid(command, "blank title"));
    }
    Ok(())
}

fn check_secs(command: &'static str, secs: u64) -> Result<(), Reject> {
    if secs == 0 {
        return Err(Reject::invalid(command, "planned_secs must be positive"));
    }
    Ok(())
}

pub(crate) fn create_task(
    day: &mut Day,
    ids: &dyn IdGen,
    title: String,
    planned_secs: u64,
) -> Result<Vec<StoreOp>, Reject> {
    const CMD: &str = "CreateTask";
    check_title(CMD, &title)?;
    check_secs(CMD, planned_secs)?;
    let last = flexible_in_order(day).last().map(|i| i.order().clone());
    let order = OrderKey::between(last.as_ref(), None).map_err(domain(CMD))?;
    day.add(PlanItem::new(ids.new_item_id(), title, planned_secs, order))
        .map_err(domain(CMD))?;
    Ok(Vec::new())
}

pub(crate) fn edit_task(
    day: &mut Day,
    item_id: ItemId,
    title: Option<String>,
    planned_secs: Option<u64>,
) -> Result<Vec<StoreOp>, Reject> {
    const CMD: &str = "EditTask";
    live_item(day, CMD, item_id, "item")?;
    if let Some(title) = &title {
        check_title(CMD, title)?;
    }
    if let Some(secs) = planned_secs {
        check_secs(CMD, secs)?;
    }
    if let Some(title) = title {
        day.set_title(item_id, title).map_err(domain(CMD))?;
    }
    if let Some(secs) = planned_secs {
        day.set_planned_secs(item_id, secs).map_err(domain(CMD))?;
    }
    Ok(Vec::new())
}

pub(crate) fn reorder_task(
    day: &mut Day,
    item_id: ItemId,
    anchor: ReorderAnchor,
) -> Result<Vec<StoreOp>, Reject> {
    const CMD: &str = "ReorderTask";
    let anchor_id = match anchor {
        ReorderAnchor::Before(id) | ReorderAnchor::After(id) => id,
    };
    if anchor_id == item_id {
        return Err(Reject::invalid(
            CMD,
            format!("item {item_id} is its own anchor"),
        ));
    }
    for (id, role) in [(item_id, "item"), (anchor_id, "anchor")] {
        if live_item(day, CMD, id, role)?.fixed() {
            return Err(Reject::invalid(CMD, format!("{role} {id} is fixed")));
        }
    }
    let all = flexible_in_order(day);
    let pos = |id: ItemId| all.iter().position(|i| i.id() == id);
    let (from, to) = (pos(item_id), pos(anchor_id));
    let in_place = match anchor {
        ReorderAnchor::Before(_) => from.map(|f| f + 1) == to,
        ReorderAnchor::After(_) => to.map(|t| t + 1) == from,
    };
    if in_place {
        // Already there: a new key would commit and bump rev for nothing.
        return Ok(Vec::new());
    }
    let others: Vec<&PlanItem> = all.into_iter().filter(|i| i.id() != item_id).collect();
    let at = others
        .iter()
        .position(|i| i.id() == anchor_id)
        .expect("anchor is a live flexible item");
    let key = |p: usize| others.get(p).map(|i| i.order());
    let (low, high) = match anchor {
        ReorderAnchor::Before(_) => (at.checked_sub(1).and_then(key), key(at)),
        ReorderAnchor::After(_) => (key(at), key(at + 1)),
    };
    let order = OrderKey::between(low, high).map_err(domain(CMD))?;
    day.set_order(item_id, order).map_err(domain(CMD))?;
    Ok(Vec::new())
}

pub(crate) fn toggle_fixed(
    day: &mut Day,
    item_id: ItemId,
    pinned_start: Option<Timestamp>,
) -> Result<Vec<StoreOp>, Reject> {
    const CMD: &str = "ToggleFixed";
    let item = live_item(day, CMD, item_id, "item")?;
    if item.fixed() {
        let key = item.order().clone();
        day.set_flexible(item_id).map_err(domain(CMD))?;
        // The old key may now be shared with an item created meanwhile; equal keys
        // would make every reorder between the two fail, so move it to the end.
        let others: Vec<&PlanItem> = flexible_in_order(day)
            .into_iter()
            .filter(|i| i.id() != item_id)
            .collect();
        if others.iter().any(|i| i.order() == &key) {
            let last = others.last().map(|i| i.order());
            let order = OrderKey::between(last, None).map_err(domain(CMD))?;
            day.set_order(item_id, order).map_err(domain(CMD))?;
        }
    } else {
        let Some(at) = pinned_start else {
            return Err(Reject::invalid(
                CMD,
                format!("item {item_id} needs a pinned_start to become fixed"),
            ));
        };
        day.set_fixed(item_id, at).map_err(domain(CMD))?;
    }
    Ok(Vec::new())
}

pub(crate) fn remove_task(
    day: &mut Day,
    now: Timestamp,
    item_id: ItemId,
) -> Result<Vec<StoreOp>, Reject> {
    day.remove(item_id, now).map_err(domain("RemoveTask"))?;
    Ok(vec![StoreOp::AppendDayLog {
        date: day.date(),
        entry: DayLogEntry {
            at: now,
            kind: DayLogKind::Removed { item_id },
        },
    }])
}

pub(crate) fn start(
    day: &mut Day,
    now: Timestamp,
    item_id: ItemId,
) -> Result<Vec<StoreOp>, Reject> {
    const CMD: &str = "Start";
    let status = day
        .item(item_id)
        .map(PlanItem::status)
        .ok_or(Reject::Domain {
            command: CMD,
            error: DomainError::UnknownItem(item_id),
        })?;
    if status == ItemStatus::Running {
        return Ok(Vec::new());
    }
    let running: Vec<ItemId> = day
        .items()
        .iter()
        .filter(|i| i.status() == ItemStatus::Running)
        .map(PlanItem::id)
        .collect();
    for id in running {
        day.pause(id, now).map_err(domain(CMD))?;
    }
    match status {
        ItemStatus::Paused => day.resume(item_id, now),
        _ => day.start(item_id, now),
    }
    .map_err(domain(CMD))?;
    Ok(Vec::new())
}

pub(crate) fn pause(
    day: &mut Day,
    now: Timestamp,
    item_id: ItemId,
) -> Result<Vec<StoreOp>, Reject> {
    day.pause(item_id, now).map_err(domain("Pause"))?;
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use nguda_core::{Date, Uuid};

    use super::*;
    use crate::test_support::{SeqIds, ts};

    fn id(n: u128) -> ItemId {
        ItemId(Uuid::from_u128(n))
    }

    fn key(s: &str) -> OrderKey {
        OrderKey::new(s).unwrap()
    }

    /// Flexible items 100 (A, "U"), 101 (B, "V"), 102 (C, "W").
    fn abc() -> Day {
        let mut day = Day::new(Date::constant(1970, 1, 1), "UTC");
        for (n, k) in [(100, "U"), (101, "V"), (102, "W")] {
            day.add(PlanItem::new(id(n), format!("t{n}"), 600, key(k)))
                .unwrap();
        }
        day
    }

    fn order_of(day: &Day) -> Vec<ItemId> {
        day.in_order().into_iter().map(PlanItem::id).collect()
    }

    #[test]
    fn create_keys_after_the_last_live_flexible_item() {
        let ids = SeqIds::default();
        let mut empty = Day::new(Date::constant(1970, 1, 1), "UTC");
        create_task(&mut empty, &ids, "a".into(), 60).unwrap();
        assert_eq!(
            empty.item(id(1)).unwrap().order(),
            &OrderKey::between(None, None).unwrap()
        );

        let mut day = abc();
        // A fixed item with a later key and a removed one are ignored.
        day.add(PlanItem::new(id(103), "f", 60, key("y"))).unwrap();
        day.set_fixed(id(103), ts(5)).unwrap();
        day.add(PlanItem::new(id(104), "r", 60, key("z"))).unwrap();
        day.remove(id(104), ts(5)).unwrap();
        create_task(&mut day, &ids, "b".into(), 60).unwrap();
        let new = day.item(id(2)).unwrap();
        assert_eq!(
            new.order(),
            &OrderKey::between(Some(&key("W")), None).unwrap()
        );
        assert!(new.order() < &key("y"));
        assert_eq!(new.status(), ItemStatus::Pending);
        assert!(!new.fixed());
    }

    #[test]
    fn reorder_before_and_after_place_keys_between_neighbours() {
        let mut day = abc();
        reorder_task(&mut day, id(102), ReorderAnchor::Before(id(100))).unwrap();
        assert!(day.item(id(102)).unwrap().order() < &key("U"));
        assert_eq!(order_of(&day), vec![id(102), id(100), id(101)]);

        let mut day = abc();
        reorder_task(&mut day, id(100), ReorderAnchor::After(id(101))).unwrap();
        let k = day.item(id(100)).unwrap().order().clone();
        assert!(key("V") < k && k < key("W"));
        assert_eq!(order_of(&day), vec![id(101), id(100), id(102)]);

        let mut day = abc();
        reorder_task(&mut day, id(100), ReorderAnchor::After(id(102))).unwrap();
        assert!(day.item(id(100)).unwrap().order() > &key("W"));
        assert_eq!(order_of(&day), vec![id(101), id(102), id(100)]);
    }

    #[test]
    fn reorder_rejects_bad_targets_without_change() {
        let base = {
            let mut d = abc();
            d.add(PlanItem::new(id(103), "f", 60, key("X"))).unwrap();
            d.set_fixed(id(103), ts(5)).unwrap();
            d.add(PlanItem::new(id(104), "r", 60, key("Y"))).unwrap();
            d.remove(id(104), ts(5)).unwrap();
            d
        };
        let cases = [
            (id(100), ReorderAnchor::Before(id(100))),
            (id(103), ReorderAnchor::Before(id(100))),
            (id(100), ReorderAnchor::After(id(103))),
            (id(104), ReorderAnchor::Before(id(100))),
            (id(100), ReorderAnchor::After(id(104))),
            (id(999), ReorderAnchor::Before(id(100))),
            (id(100), ReorderAnchor::Before(id(999))),
        ];
        for (item, anchor) in cases {
            let mut day = base.clone();
            assert!(
                reorder_task(&mut day, item, anchor).is_err(),
                "{item} {anchor:?}"
            );
            assert_eq!(day, base);
        }
    }

    #[test]
    fn reorder_between_equal_keys_is_invalid_order() {
        let mut day = abc();
        day.add(PlanItem::new(id(103), "dup", 60, key("U")))
            .unwrap();
        // Sorted stably: 100 "U", 103 "U", 101 "V". Nothing fits after 100.
        let before = day.clone();
        let err = reorder_task(&mut day, id(102), ReorderAnchor::After(id(100))).unwrap_err();
        assert_eq!(day, before);
        assert_eq!(err.kind(), "invalid_order");
    }

    #[test]
    fn reorder_into_its_current_place_leaves_the_day_unchanged() {
        for (item, anchor) in [
            (102, ReorderAnchor::After(id(101))),
            (100, ReorderAnchor::Before(id(101))),
        ] {
            let mut day = abc();
            reorder_task(&mut day, id(item), anchor).unwrap();
            assert_eq!(day, abc(), "{item} {anchor:?}");
        }
    }

    #[test]
    fn unpinning_onto_a_shared_key_gives_a_fresh_key() {
        let ids = SeqIds::default();
        let mut day = Day::new(Date::constant(1970, 1, 1), "UTC");
        create_task(&mut day, &ids, "a".into(), 60).unwrap();
        toggle_fixed(&mut day, id(1), Some(ts(5))).unwrap();
        create_task(&mut day, &ids, "b".into(), 60).unwrap();
        assert_eq!(
            day.item(id(1)).unwrap().order(),
            day.item(id(2)).unwrap().order()
        );
        toggle_fixed(&mut day, id(1), None).unwrap();
        let (a, b) = (day.item(id(1)).unwrap(), day.item(id(2)).unwrap());
        assert!(!a.fixed());
        assert!(a.order() > b.order());
        reorder_task(&mut day, id(1), ReorderAnchor::Before(id(2))).unwrap();
        assert_eq!(order_of(&day), vec![id(1), id(2)]);
    }

    #[test]
    fn unpinning_keeps_a_key_nobody_shares() {
        let mut day = abc();
        day.set_fixed(id(101), ts(5)).unwrap();
        toggle_fixed(&mut day, id(101), None).unwrap();
        assert_eq!(day, abc());
    }

    #[test]
    fn reject_messages_hold_no_titles() {
        let mut day = abc();
        let stale = apply(
            EngineMsg::Start {
                item_id: id(100),
                expected_rev: 3,
            },
            &mut day,
            ts(0),
            &SeqIds::default(),
            5,
        )
        .unwrap_err();
        assert_eq!(stale.to_string(), "stale rev: expected 3, current 5");
        assert_eq!(stale.kind(), "stale_rev");
        let blank = create_task(&mut day, &SeqIds::default(), "  ".into(), 60).unwrap_err();
        assert_eq!(blank.to_string(), "CreateTask: blank title");
        let err = pause(&mut day, ts(0), id(100)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Pause: cannot pause an item that is pending"
        );
        assert!(!err.to_string().contains("t100"));
    }
}
