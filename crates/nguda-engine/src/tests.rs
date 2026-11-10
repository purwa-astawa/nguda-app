//! One test per row of the engine's I/O matrices (startup and recovery, and the
//! plan commands), the `diff` unit tests, and the exactly-once effect check. All run against `MemoryStore` in UTC.

use std::sync::Arc;
use std::time::Duration;

use nguda_core::{
    Date, Day, DayLogEntry, DayLogKind, ItemId, ItemStatus, OrderKey, PlanItem, Segment,
    StoreBatch, StoreOp, TimeZone, Uuid,
};
use tokio::sync::{mpsc, watch};

use crate::batch::diff;
use crate::test_support::{FakeClock, INJECTED, MemoryStore, SeqIds, ts};
use crate::{
    Effect, EngineClosed, EngineDeps, EngineHandle, EngineMsg, ReorderAnchor, Snapshot,
    StatusEntry, StatusKey, spawn,
};

const DAY_SECS: i64 = 86_400;

/// 1970-01-01 (UTC), the day of every instant below `DAY_SECS`.
fn day1() -> Date {
    Date::constant(1970, 1, 1)
}

fn day2() -> Date {
    Date::constant(1970, 1, 2)
}

fn id(n: u128) -> ItemId {
    ItemId(Uuid::from_u128(n))
}

/// Item ids from `SeqIds` start at 1, so seeded items use 100 and up.
fn item(n: u128, title: &str, order: &str) -> PlanItem {
    PlanItem::new(id(n), title, 600, OrderKey::new(order).unwrap())
}

fn closed(from: i64, to: i64) -> Segment {
    Segment {
        started_at: ts(from),
        ended_at: Some(ts(to)),
    }
}

fn store_status() -> Vec<StatusEntry> {
    vec![StatusEntry {
        key: StatusKey::Store,
        message: INJECTED.into(),
    }]
}

struct Harness {
    engine: EngineHandle,
    effects: mpsc::UnboundedReceiver<Effect>,
    rx: watch::Receiver<Snapshot>,
    clock: Arc<FakeClock>,
}

fn start(clock: &Arc<FakeClock>, store: &Arc<MemoryStore>) -> Harness {
    let (engine, effects) = spawn(EngineDeps {
        clock: clock.clone(),
        store: store.clone(),
        ids: Arc::new(SeqIds::default()),
        tz: TimeZone::UTC,
    });
    let rx = engine.subscribe();
    Harness {
        engine,
        effects,
        rx,
        clock: clock.clone(),
    }
}

impl Harness {
    /// Waits for the snapshot with exactly `rev`, failing on a skip or a timeout.
    async fn rev(&mut self, rev: u64) -> Snapshot {
        let rx = &mut self.rx;
        let snap = tokio::time::timeout(Duration::from_secs(5), async {
            rx.wait_for(|s| s.rev >= rev).await.unwrap().clone()
        })
        .await
        .unwrap_or_else(|_| panic!("no snapshot rev {rev}"));
        assert_eq!(snap.rev, rev, "skipped a rev");
        snap
    }

    fn send(&self, msg: EngineMsg) {
        self.engine.send(msg).unwrap();
    }

    /// Lets the engine run until `cond` holds.
    async fn until(&self, mut cond: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("condition never held");
    }
}

fn day_of(snap: &Snapshot) -> &Day {
    snap.day.as_ref().unwrap()
}

fn rejected(snap: &Snapshot) -> Option<&str> {
    snap.status
        .iter()
        .find(|e| e.key == StatusKey::CommandRejected)
        .map(|e| e.message.as_str())
}

fn create(title: &str, planned_secs: u64) -> EngineMsg {
    EngineMsg::CreateTask {
        title: title.into(),
        planned_secs,
    }
}

fn start_msg(n: u128, expected_rev: u64) -> EngineMsg {
    EngineMsg::Start {
        item_id: id(n),
        expected_rev,
    }
}

/// A started engine on `day` (seeded if given), at rev 1.
async fn running_on(at: i64, day: Option<&Day>) -> (Harness, Arc<MemoryStore>) {
    let (clock, store) = (FakeClock::at(at), MemoryStore::new());
    if let Some(day) = day {
        store.seed(day);
    }
    let mut h = start(&clock, &store);
    h.rev(1).await;
    (h, store)
}

/// Sends `msg` and expects its rejection at `rev`: day unchanged, no new batch.
async fn expect_rejected(
    h: &mut Harness,
    store: &MemoryStore,
    msg: EngineMsg,
    rev: u64,
) -> Snapshot {
    let before_day = h.rx.borrow().day.clone();
    let batches = store.batches().len();
    h.send(msg);
    let snap = h.rev(rev).await;
    assert_eq!(snap.day, before_day, "day unchanged");
    assert_eq!(store.batches().len(), batches, "no batch");
    assert!(rejected(&snap).is_some(), "CommandRejected set");
    snap
}

// --- Matrix rows ---

#[tokio::test]
async fn fresh_start_publishes_an_empty_today_at_rev_1() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    assert_eq!(*h.rx.borrow(), Snapshot::default());
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(Day::new(day1(), "UTC")));
    assert_eq!(snap.running, None);
    assert!(snap.status.is_empty());
    assert_eq!(store.last_alive(), Some(ts(1_000)));
}

#[tokio::test]
async fn existing_day_is_loaded_into_rev_1() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "write", "V")).unwrap();
    day.add(item(101, "read", "W")).unwrap();
    day.start(id(101), ts(10)).unwrap();
    day.pause(id(101), ts(20)).unwrap();
    store.seed(&day);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(day));
    assert_eq!(snap.running, None);
}

/// A day whose item 100 has been running since 1_000.
fn day_running_other() -> Day {
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "other", "V")).unwrap();
    day.start(id(100), ts(1_000)).unwrap();
    day
}

// --- Plan commands ---

#[tokio::test]
async fn create_task_adds_a_pending_flexible_item_and_persists_it() {
    let (mut h, store) = running_on(1_000, None).await;
    h.send(create("A", 600));
    let snap = h.rev(2).await;
    let item = day_of(&snap).item(id(1)).unwrap();
    assert_eq!(item.title(), "A");
    assert_eq!(item.planned_secs(), 600);
    assert_eq!(item.status(), ItemStatus::Pending);
    assert!(!item.fixed());
    assert_eq!(item.order(), &OrderKey::between(None, None).unwrap());
    assert!(snap.status.is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.batches().len(), 2);

    // The next one goes after it.
    h.send(create("B", 300));
    let snap = h.rev(3).await;
    let day = day_of(&snap);
    assert!(day.item(id(2)).unwrap().order() > day.item(id(1)).unwrap().order());
    let order: Vec<_> = day.in_order().into_iter().map(PlanItem::id).collect();
    assert_eq!(order, vec![id(1), id(2)]);

    // A reload gives the same day.
    let mut reloaded = start(&FakeClock::at(1_010), &store);
    assert_eq!(reloaded.rev(1).await.day, snap.day);
}

#[tokio::test]
async fn create_task_with_a_blank_title_or_zero_secs_is_rejected() {
    let (mut h, store) = running_on(1_000, None).await;
    let mut rev = 1;
    for msg in [create("", 600), create(" \t ", 600), create("A", 0)] {
        rev += 1;
        let snap = expect_rejected(&mut h, &store, msg, rev).await;
        assert!(rejected(&snap).unwrap().starts_with("CreateTask: "));
    }
    assert_eq!(store.stored_day(day1()), None);
}

fn abc_day() -> Day {
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "A", "U")).unwrap();
    day.add(item(101, "B", "V")).unwrap();
    day.add(item(102, "C", "W")).unwrap();
    day
}

fn edit(n: u128, title: Option<&str>, planned_secs: Option<u64>) -> EngineMsg {
    EngineMsg::EditTask {
        item_id: id(n),
        title: title.map(Into::into),
        planned_secs,
    }
}

#[tokio::test]
async fn edit_task_changes_title_and_secs_and_persists_them() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(edit(100, Some("renamed"), None));
    let snap = h.rev(2).await;
    let a = day_of(&snap).item(id(100)).unwrap();
    assert_eq!(a.title(), "renamed");
    assert_eq!(a.planned_secs(), 600);
    assert_eq!(store.stored_day(day1()), snap.day);

    h.send(edit(100, None, Some(900)));
    let snap = h.rev(3).await;
    assert_eq!(day_of(&snap).item(id(100)).unwrap().planned_secs(), 900);
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.batches().len(), 3);
}

#[tokio::test]
async fn edit_task_with_nothing_new_commits_and_publishes_nothing() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(edit(100, None, None));
    h.send(edit(100, Some("A"), Some(600)));
    // A later message would bump rev past 2 if the no-ops had published.
    h.send(edit(100, Some("A2"), None));
    let snap = h.rev(2).await;
    assert_eq!(day_of(&snap).item(id(100)).unwrap().title(), "A2");
    assert_eq!(store.batches().len(), 2);
}

#[tokio::test]
async fn edit_task_rejects_unknown_removed_blank_and_zero() {
    let mut day = abc_day();
    day.remove(id(102), ts(10)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    let cases = [
        edit(999, Some("x"), None),
        edit(102, Some("x"), None),
        edit(102, None, None),
        edit(100, Some("  "), None),
        edit(100, None, Some(0)),
        // One bad field rejects the whole edit.
        edit(100, Some("ok"), Some(0)),
    ];
    for (i, msg) in cases.into_iter().enumerate() {
        expect_rejected(&mut h, &store, msg, i as u64 + 2).await;
    }
    assert_eq!(store.stored_day(day1()), Some(day));
}

fn reorder(n: u128, anchor: ReorderAnchor, expected_rev: u64) -> EngineMsg {
    EngineMsg::ReorderTask {
        item_id: id(n),
        anchor,
        expected_rev,
    }
}

fn ids_in_order(snap: &Snapshot) -> Vec<ItemId> {
    day_of(snap)
        .in_order()
        .into_iter()
        .map(PlanItem::id)
        .collect()
}

#[tokio::test]
async fn reorder_before_the_first_item_moves_it_to_the_front() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(reorder(102, ReorderAnchor::Before(id(100)), 1));
    let snap = h.rev(2).await;
    assert_eq!(ids_in_order(&snap), vec![id(102), id(100), id(101)]);
    assert!(day_of(&snap).item(id(102)).unwrap().order() < &OrderKey::new("U").unwrap());
    assert!(snap.status.is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.batches().len(), 2);
}

#[tokio::test]
async fn reorder_after_the_last_item_keys_it_after() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(reorder(100, ReorderAnchor::After(id(102)), 1));
    let snap = h.rev(2).await;
    assert_eq!(ids_in_order(&snap), vec![id(101), id(102), id(100)]);
    assert!(day_of(&snap).item(id(100)).unwrap().order() > &OrderKey::new("W").unwrap());
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn reorder_into_the_current_place_commits_and_publishes_nothing() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(reorder(102, ReorderAnchor::After(id(101)), 1));
    h.send(reorder(100, ReorderAnchor::Before(id(101)), 1));
    // Still rev 1 for this one: a later message would bump rev past 2 if the
    // no-ops had published.
    h.send(reorder(102, ReorderAnchor::Before(id(100)), 1));
    let snap = h.rev(2).await;
    assert!(snap.status.is_empty(), "no-ops kept rev 1");
    assert_eq!(ids_in_order(&snap), vec![id(102), id(100), id(101)]);
    assert_eq!(store.batches().len(), 2);
}

#[tokio::test]
async fn reorder_rejects_fixed_removed_unknown_and_self_anchors() {
    let mut day = abc_day();
    day.add(item(103, "F", "X")).unwrap();
    day.set_fixed(id(103), ts(50_000)).unwrap();
    day.add(item(104, "R", "Y")).unwrap();
    day.remove(id(104), ts(10)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    let cases = [
        (103, ReorderAnchor::Before(id(100))),
        (100, ReorderAnchor::After(id(103))),
        (104, ReorderAnchor::Before(id(100))),
        (100, ReorderAnchor::Before(id(104))),
        (999, ReorderAnchor::Before(id(100))),
        (100, ReorderAnchor::After(id(999))),
        (100, ReorderAnchor::Before(id(100))),
    ];
    // Each carries the current rev, so only the target is wrong.
    for (rev, (n, anchor)) in (1..).zip(cases) {
        expect_rejected(&mut h, &store, reorder(n, anchor, rev), rev + 1).await;
    }
    assert_eq!(store.stored_day(day1()), Some(day));
}

fn toggle(n: u128, pinned_start: Option<i64>, expected_rev: u64) -> EngineMsg {
    EngineMsg::ToggleFixed {
        item_id: id(n),
        pinned_start: pinned_start.map(ts),
        expected_rev,
    }
}

#[tokio::test]
async fn toggle_fixed_pins_a_flexible_item() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(toggle(101, Some(30_000), 1));
    let snap = h.rev(2).await;
    let b = day_of(&snap).item(id(101)).unwrap();
    assert!(b.fixed());
    assert_eq!(b.pinned_start(), Some(ts(30_000)));
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn toggle_fixed_without_a_pin_is_rejected() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    expect_rejected(&mut h, &store, toggle(101, None, 1), 2).await;
}

#[tokio::test]
async fn toggle_fixed_unpins_a_fixed_item_ignoring_pinned_start() {
    let mut day = abc_day();
    day.set_fixed(id(101), ts(30_000)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    h.send(toggle(101, Some(40_000), 1));
    let snap = h.rev(2).await;
    let b = day_of(&snap).item(id(101)).unwrap();
    assert!(!b.fixed());
    assert_eq!(b.pinned_start(), None);
    assert_eq!(store.stored_day(day1()), snap.day);
}

fn removed_at(n: u128, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Removed { item_id: id(n) },
    }
}

#[tokio::test]
async fn remove_task_tombstones_and_logs_in_one_batch() {
    let mut day = abc_day();
    day.start(id(101), ts(10)).unwrap();
    day.pause(id(101), ts(20)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    h.send(EngineMsg::RemoveTask { item_id: id(100) });
    let snap = h.rev(2).await;
    let a = day_of(&snap).item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Removed);
    assert_eq!(a.removed_at(), Some(ts(1_000)));
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.day_log(day1()), vec![removed_at(100, 1_000)]);
    let batches = store.batches();
    assert_eq!(batches.len(), 2);
    let last = batches[1].ops();
    assert!(last.iter().any(|op| matches!(op, StoreOp::PutItem { .. })));
    assert_eq!(
        last.last(),
        Some(&StoreOp::AppendDayLog {
            date: day1(),
            entry: removed_at(100, 1_000)
        })
    );

    // Paused works too.
    h.clock.set(1_005);
    h.send(EngineMsg::RemoveTask { item_id: id(101) });
    let snap = h.rev(3).await;
    assert_eq!(
        day_of(&snap).item(id(101)).unwrap().status(),
        ItemStatus::Removed
    );
    assert_eq!(
        store.day_log(day1()),
        vec![removed_at(100, 1_000), removed_at(101, 1_005)]
    );
}

#[tokio::test]
async fn remove_task_rejects_running_and_removed_items() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(EngineMsg::RemoveTask { item_id: id(102) });
    h.send(start_msg(100, 2));
    assert!(h.rev(3).await.status.is_empty(), "Start saw rev 2");
    let batches = store.batches().len();
    expect_rejected(
        &mut h,
        &store,
        EngineMsg::RemoveTask { item_id: id(100) },
        4,
    )
    .await;
    expect_rejected(
        &mut h,
        &store,
        EngineMsg::RemoveTask { item_id: id(102) },
        5,
    )
    .await;
    assert_eq!(store.batches().len(), batches);
    assert_eq!(store.day_log(day1()).len(), 1);
}

#[tokio::test]
async fn start_pauses_the_running_item_and_starts_the_target_in_one_batch() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(start_msg(101, 1));
    let snap = h.rev(2).await;
    assert_eq!(snap.running.as_ref().unwrap().title, "B");
    h.clock.set(1_030);
    h.send(start_msg(100, 2));
    let snap = h.rev(3).await;
    let day = day_of(&snap);
    let b = day.item(id(101)).unwrap();
    assert_eq!(b.status(), ItemStatus::Paused);
    assert_eq!(b.segments(), &[closed(1_000, 1_030)]);
    let a = day.item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Running);
    assert_eq!(a.segments(), &[Segment::open(ts(1_030))]);
    let running = snap.running.clone().unwrap();
    assert_eq!(running.title, "A");
    assert_eq!(running.segment_started_at, ts(1_030));
    assert_eq!(running.accumulated_secs, 0);
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.batches().len(), 3);
}

#[tokio::test]
async fn start_resumes_a_paused_item() {
    let mut day = abc_day();
    day.start(id(100), ts(10)).unwrap();
    day.pause(id(100), ts(52)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    h.send(start_msg(100, 1));
    let snap = h.rev(2).await;
    let a = day_of(&snap).item(id(100)).unwrap();
    assert_eq!(a.segments(), &[closed(10, 52), Segment::open(ts(1_000))]);
    assert_eq!(snap.running.as_ref().unwrap().accumulated_secs, 42);
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn start_on_the_running_item_changes_nothing() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(start_msg(100, 1));
    let snap = h.rev(2).await;
    h.clock.set(1_010);
    h.send(start_msg(100, 2));
    // A later message would bump rev past 3 if the no-op had published.
    h.send(edit(101, Some("B2"), None));
    let after = h.rev(3).await;
    assert_eq!(
        day_of(&after).item(id(100)),
        day_of(&snap).item(id(100)),
        "segment untouched"
    );
    assert_eq!(store.batches().len(), 3);
}

#[tokio::test]
async fn start_rejects_done_removed_and_unknown_items() {
    let mut day = abc_day();
    day.start(id(100), ts(10)).unwrap();
    day.finish(id(100), ts(20)).unwrap();
    day.remove(id(101), ts(30)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    for (rev, n) in (1..).zip([100, 101, 999]) {
        expect_rejected(&mut h, &store, start_msg(n, rev), rev + 1).await;
    }
    assert_eq!(store.stored_day(day1()), Some(day));
}

#[tokio::test]
async fn pause_pauses_the_running_item_and_rejects_others() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(start_msg(100, 1));
    h.rev(2).await;
    h.clock.set(1_020);
    h.send(EngineMsg::Pause { item_id: id(100) });
    let snap = h.rev(3).await;
    let a = day_of(&snap).item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Paused);
    assert_eq!(a.segments(), &[closed(1_000, 1_020)]);
    assert_eq!(snap.running, None);
    assert_eq!(store.stored_day(day1()), snap.day);

    expect_rejected(&mut h, &store, EngineMsg::Pause { item_id: id(100) }, 4).await;
    expect_rejected(&mut h, &store, EngineMsg::Pause { item_id: id(101) }, 5).await;
}

#[tokio::test]
async fn a_stale_rev_applies_nothing_and_commits_no_batch() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    let stale = [
        start_msg(100, 0),
        reorder(102, ReorderAnchor::Before(id(100)), 0),
        toggle(101, Some(30_000), 7),
    ];
    for (rev, msg) in (1u64..).zip(stale) {
        let expected = match &msg {
            EngineMsg::Start { expected_rev, .. }
            | EngineMsg::ReorderTask { expected_rev, .. }
            | EngineMsg::ToggleFixed { expected_rev, .. } => *expected_rev,
            _ => unreachable!(),
        };
        let snap = expect_rejected(&mut h, &store, msg, rev + 1).await;
        assert_eq!(
            snap.status,
            vec![StatusEntry {
                key: StatusKey::CommandRejected,
                message: format!("stale rev: expected {expected}, current {rev}"),
            }]
        );
    }
    assert_eq!(store.batches().len(), 1);
}

#[tokio::test]
async fn a_valid_command_clears_command_rejected() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    expect_rejected(&mut h, &store, start_msg(100, 0), 2).await;
    h.send(start_msg(100, 2));
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty());
    assert!(snap.running.is_some());

    // A valid no-op clears it too, publishing only the status change.
    expect_rejected(&mut h, &store, create("", 1), 4).await;
    h.send(start_msg(100, 4));
    let snap = h.rev(5).await;
    assert!(snap.status.is_empty());
    assert_eq!(store.batches().len(), 2);
}

#[tokio::test]
async fn a_failing_commit_keeps_the_state_and_sets_store_status_until_a_success() {
    let (mut h, store) = running_on(1_000, None).await;
    expect_rejected(&mut h, &store, create("", 600), 2).await;
    let before = h.rx.borrow().clone();
    store.fail_commits(true);
    h.send(create("A", 600));
    let snap = h.rev(3).await;
    assert_eq!(snap.day, before.day);
    // The prior rejection is cleared: the command passed validation.
    assert_eq!(snap.status, store_status());
    assert_eq!(store.stored_day(day1()), None);

    store.fail_commits(false);
    h.clock.set(1_005);
    h.send(create("A", 600));
    let snap = h.rev(4).await;
    assert!(snap.status.is_empty());
    // The failed attempt used id 1.
    h.send(start_msg(2, 4));
    let snap = h.rev(5).await;
    assert_eq!(snap.running.unwrap().segment_started_at, ts(1_005));
}

#[tokio::test]
async fn heartbeat_records_last_alive_and_publishes_only_status_changes() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    h.rev(1).await;
    assert_eq!(store.last_alive(), Some(ts(1_000)));

    h.clock.advance(60);
    h.until(|| store.last_alive() == Some(ts(1_060))).await;
    h.clock.advance(60);
    h.until(|| store.last_alive() == Some(ts(1_120))).await;
    assert!(!h.rx.has_changed().unwrap(), "nothing visible changed");
    assert_eq!(h.rx.borrow().rev, 1);
    // No tick: a clock short of the deadline writes nothing.
    h.clock.advance(59);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(store.last_alive(), Some(ts(1_120)));

    store.fail_commits(true);
    h.clock.advance(1);
    let snap = h.rev(2).await;
    assert_eq!(snap.status, store_status());
    assert_eq!(store.last_alive(), Some(ts(1_120)));

    store.fail_commits(false);
    h.clock.advance(60);
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty());
    assert_eq!(store.last_alive(), Some(ts(1_240)));
}

fn recovered_at(n: u128, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Recovered { item_id: id(n) },
    }
}

#[tokio::test]
async fn crash_with_a_heartbeat_closes_the_segment_at_last_alive() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.seed_last_alive(Some(ts(1_030)));
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    let other = snap.day.as_ref().unwrap().item(id(100)).unwrap();
    assert_eq!(other.status(), ItemStatus::Paused);
    assert_eq!(other.segments(), &[closed(1_000, 1_030)]);
    assert_eq!(snap.running, None);
    assert!(snap.status.is_empty());
    assert_eq!(store.day_log(day1()), vec![recovered_at(100, 5_000)]);
    assert_eq!(store.last_alive(), Some(ts(5_000)));
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(
        h.effects.recv().await,
        Some(Effect::Recovered { item_id: id(100) })
    );
    // Recovery, day log and heartbeat are one batch.
    let batches = store.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0].ops().last(),
        Some(&StoreOp::SetLastAlive { at: ts(5_000) })
    );
}

#[tokio::test]
async fn crash_without_a_newer_heartbeat_closes_the_segment_at_its_start() {
    for last_alive in [None, Some(ts(900))] {
        let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
        store.seed(&day_running_other());
        store.seed_last_alive(last_alive);
        let mut h = start(&clock, &store);
        let snap = h.rev(1).await;
        let other = snap.day.as_ref().unwrap().item(id(100)).unwrap();
        assert_eq!(other.segments(), &[closed(1_000, 1_000)], "{last_alive:?}");
        assert_eq!(other.status(), ItemStatus::Paused);
        assert_eq!(store.day_log(day1()), vec![recovered_at(100, 5_000)]);
        assert_eq!(store.last_alive(), Some(ts(5_000)));
    }
}

#[tokio::test]
async fn crash_on_an_earlier_day_is_recovered_on_that_day() {
    let now = DAY_SECS + 100;
    let (clock, store) = (FakeClock::at(now), MemoryStore::new());
    store.seed(&day_running_other());
    store.seed_last_alive(Some(ts(1_030)));
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(Day::new(day2(), "UTC")));
    assert_eq!(snap.running, None);
    let yesterday = store.stored_day(day1()).unwrap();
    let other = yesterday.item(id(100)).unwrap();
    assert_eq!(other.status(), ItemStatus::Paused);
    assert_eq!(other.segments(), &[closed(1_000, 1_030)]);
    assert_eq!(store.day_log(day1()), vec![recovered_at(100, now)]);
    assert_eq!(store.day_log(day2()), vec![]);
    assert_eq!(
        h.effects.recv().await,
        Some(Effect::Recovered { item_id: id(100) })
    );
}

#[tokio::test]
async fn a_failed_recovery_commit_keeps_the_loaded_state_with_store_status() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.seed_last_alive(Some(ts(1_030)));
    store.fail_commits(true);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(day_running_other()));
    assert_eq!(snap.running.unwrap().segment_started_at, ts(1_000));
    assert_eq!(snap.status, store_status());
    assert_eq!(store.day_log(day1()), vec![]);
    assert_eq!(store.last_alive(), Some(ts(1_030)));
    assert!(h.effects.try_recv().is_err(), "no effect without a commit");
}

#[tokio::test]
async fn a_failed_startup_load_yields_an_empty_today_with_store_status() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.fail_loads(true);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(Day::new(day1(), "UTC")));
    assert_eq!(snap.running, None);
    assert_eq!(snap.status, store_status());
}

#[tokio::test]
async fn a_failed_load_of_today_alone_yields_an_empty_today_with_store_status() {
    // No open segment, so the recovery reads succeed; only `load_day(today)` fails.
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "stored", "V")).unwrap();
    store.seed(&day);
    store.fail_load_day(true);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.day, Some(Day::new(day1(), "UTC")));
    assert_eq!(snap.running, None);
    assert_eq!(snap.status, store_status());
    // The startup heartbeat still committed.
    assert_eq!(store.last_alive(), Some(ts(5_000)));
}

#[test]
fn send_after_engine_stopped_returns_engine_closed() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let h = rt.block_on(async { start(&clock, &store) });
    drop(rt); // drops the engine task and its receiver
    assert_eq!(h.engine.send(create("a", 60)), Err(EngineClosed));
}

// --- Startup ordering and effects ---

#[tokio::test]
async fn messages_sent_before_startup_queue_and_run_after_it_in_order() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.seed_last_alive(Some(ts(1_030)));
    // Sent before the engine task first runs, so before recovery.
    let mut h = start(&clock, &store);
    h.send(create("new", 60));
    h.send(start_msg(1, 2));
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty(), "Start saw rev 2");
    let day = snap.day.as_ref().unwrap();
    // Recovery ran first: the other item closed at the heartbeat, not at 5_000.
    assert_eq!(
        day.item(id(100)).unwrap().segments(),
        &[closed(1_000, 1_030)]
    );
    assert_eq!(
        day.item(id(1)).unwrap().segments(),
        &[Segment::open(ts(5_000))]
    );
    let batches = store.batches();
    assert_eq!(batches.len(), 3);
    assert!(
        batches[0]
            .ops()
            .iter()
            .any(|op| matches!(op, StoreOp::AppendDayLog { .. }))
    );
}

#[tokio::test]
async fn the_recovered_effect_is_emitted_exactly_once() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    let mut h = start(&clock, &store);
    h.rev(1).await;
    h.send(create("new", 60));
    h.rev(2).await;
    h.clock.advance(60);
    h.until(|| store.last_alive() == Some(ts(5_060))).await;
    h.clock.advance(1);
    h.send(start_msg(1, 2));
    h.rev(3).await;
    assert_eq!(
        h.effects.try_recv(),
        Ok(Effect::Recovered { item_id: id(100) })
    );
    assert!(h.effects.try_recv().is_err());
}

// --- diff ---

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
