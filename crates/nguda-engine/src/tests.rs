//! One test per row of the engine's I/O matrices (startup and recovery, and the
//! plan commands, the work-end setting and overflow), and the exactly-once effect
//! check. All run against `MemoryStore`
//! in UTC.

use std::sync::Arc;
use std::time::Duration;

use nguda_core::{
    Day, DayLogEntry, DayLogKind, ItemId, ItemStatus, OrderKey, PlanItem, PromptKind, Segment,
    SlotKind, StoreOp, TimeZone, WorkEnd,
};
use tokio::sync::{mpsc, watch};

use crate::test_support::{
    FakeClock, INJECTED, MemoryStore, SeqIds, day_running_other, day1, day2, id, item, key, prompt,
    ts,
};
use crate::{
    Effect, EngineClosed, EngineDeps, EngineHandle, EngineMsg, InsertAnchor, NextItem,
    ReorderAnchor, Snapshot, StatusEntry, StatusKey, spawn,
};

const DAY_SECS: i64 = 86_400;

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
    assert_eq!(ids_in_order(&snap), vec![id(1), id(2)]);

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
    snap.schedule.slots().iter().map(|s| s.item_id).collect()
}

#[tokio::test]
async fn reorder_before_the_first_item_moves_it_to_the_front() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(reorder(102, ReorderAnchor::Before(id(100)), 1));
    let snap = h.rev(2).await;
    assert_eq!(ids_in_order(&snap), vec![id(102), id(100), id(101)]);
    assert!(day_of(&snap).item(id(102)).unwrap().order() < &key("U"));
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
    assert!(day_of(&snap).item(id(100)).unwrap().order() > &key("W"));
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

#[tokio::test]
async fn reorder_of_or_next_to_an_interrupted_item_is_rejected() {
    // Stack [A]; N running; B, C pending.
    let (mut h, store) = n_interrupts_a().await;
    let cases = [
        (100, ReorderAnchor::After(id(102)), "item"),
        (101, ReorderAnchor::Before(id(100)), "anchor"),
    ];
    for (rev, (n, anchor, role)) in (3..).zip(cases) {
        let snap = expect_rejected(&mut h, &store, reorder(n, anchor, rev), rev + 1).await;
        let a = "00000000-0000-0000-0000-000000000064";
        assert_eq!(
            rejected(&snap),
            Some(format!("ReorderTask: {role} {a} is interrupted").as_str()),
            "{anchor:?}"
        );
    }
    let day = store.stored_day(day1()).unwrap();
    assert_eq!(day.interrupt_stack(), &[id(100)]);
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
async fn next_item_is_published_only_while_nothing_runs() {
    let (mut h, _store) = running_on(1_000, None).await;
    assert_eq!(h.rev(1).await.next_item, None);

    h.send(create("A", 600));
    let snap = h.rev(2).await;
    let next_a = Some(NextItem {
        item_id: id(1),
        title: "A".into(),
    });
    assert_eq!(snap.next_item, next_a);

    h.send(start_msg(1, 2));
    let snap = h.rev(3).await;
    assert!(snap.running.is_some());
    assert_eq!(snap.next_item, None);

    h.clock.set(1_020);
    h.send(EngineMsg::Pause { item_id: id(1) });
    let snap = h.rev(4).await;
    assert_eq!(snap.running, None);
    assert_eq!(snap.next_item, next_a);
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
    // The item still runs, past its planned end, so only its prompt may follow.
    tokio::time::sleep(Duration::from_millis(20)).await;
    while let Ok(effect) = h.effects.try_recv() {
        assert!(
            !matches!(effect, Effect::Recovered { .. }),
            "no Recovered without a commit"
        );
    }
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

// --- Planned-end prompt ---

fn notify(prompt_n: u128, item_n: u128) -> Effect {
    Effect::Notify {
        prompt_id: prompt(prompt_n),
        item_id: id(item_n),
        kind: PromptKind::PlannedEnd,
    }
}

impl Harness {
    async fn next_effect(&mut self) -> Effect {
        tokio::time::timeout(Duration::from_secs(5), self.effects.recv())
            .await
            .expect("no effect in time")
            .expect("effect channel open")
    }

    /// Gives the engine time to act, then checks it emitted nothing.
    async fn expect_no_effect(&mut self) {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(self.effects.try_recv().ok(), None);
    }
}

/// Item 100 "A" planned 60 s and item 101 "B" planned 600 s, both pending.
fn short_day() -> Day {
    let mut day = Day::new(day1(), "UTC");
    day.add(PlanItem::new(id(100), "A", 60, key("U"))).unwrap();
    day.add(item(101, "B", "V")).unwrap();
    day
}

/// An engine at 1_000 on `day` with item 100 started at rev 2.
async fn started_a(day: &Day) -> (Harness, Arc<MemoryStore>) {
    let (mut h, store) = running_on(1_000, Some(day)).await;
    h.send(start_msg(100, 1));
    assert!(h.rev(2).await.running.is_some());
    (h, store)
}

#[tokio::test]
async fn planned_end_emits_exactly_one_notify_and_keeps_the_item_running() {
    let (mut h, store) = started_a(&short_day()).await;
    let before = h.rx.borrow().clone();
    h.clock.set(1_059);
    h.expect_no_effect().await;
    h.clock.set(1_060);
    assert_eq!(h.next_effect().await, notify(1, 100));
    // An effect only: no state change, no snapshot, and only heartbeats committed.
    h.until(|| store.last_alive() == Some(ts(1_060))).await;
    assert_eq!(*h.rx.borrow(), before);
    assert!(!h.rx.has_changed().unwrap());
    assert!(
        store.batches()[2..]
            .iter()
            .all(|b| b.ops() == [StoreOp::SetLastAlive { at: ts(1_060) }])
    );
    // Long after, heartbeats pass but there is no second prompt and no switch.
    for at in [1_120, 1_180, 1_240, 1_300] {
        h.clock.set(at);
        h.until(|| store.last_alive() == Some(ts(at))).await;
    }
    h.expect_no_effect().await;
    let snap = h.rx.borrow().clone();
    assert_eq!(snap.rev, 2);
    assert_eq!(snap.running.unwrap().item_id, id(100));
}

#[tokio::test]
async fn a_planned_end_between_heartbeats_notifies_without_committing() {
    let mut day = Day::new(day1(), "UTC");
    day.add(PlanItem::new(id(100), "A", 30, key("U"))).unwrap();
    let (mut h, store) = started_a(&day).await;
    let before = h.rx.borrow().clone();
    let batches = store.batches().len();
    h.clock.set(1_029);
    h.expect_no_effect().await;
    h.clock.set(1_030);
    assert_eq!(h.next_effect().await, notify(1, 100));
    h.expect_no_effect().await;
    // The next heartbeat is due at 1_060: nothing was committed or published.
    assert_eq!(store.batches().len(), batches);
    assert_eq!(store.last_alive(), Some(ts(1_000)));
    assert_eq!(*h.rx.borrow(), before);
    assert!(!h.rx.has_changed().unwrap());
}

#[tokio::test]
async fn pause_and_resume_after_the_prompt_does_not_prompt_again() {
    let (mut h, _store) = started_a(&short_day()).await;
    h.clock.set(1_070);
    assert_eq!(h.next_effect().await, notify(1, 100));
    h.send(EngineMsg::Pause { item_id: id(100) });
    h.rev(3).await;
    h.clock.set(1_080);
    h.send(start_msg(100, 3));
    assert!(h.rev(4).await.running.is_some());
    h.clock.set(1_200);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn raising_planned_secs_after_the_prompt_re_arms_it() {
    let (mut h, _store) = started_a(&short_day()).await;
    h.clock.set(1_060);
    assert_eq!(h.next_effect().await, notify(1, 100));
    h.send(edit(100, None, Some(120)));
    h.rev(3).await;
    h.clock.set(1_119);
    h.expect_no_effect().await;
    h.clock.set(1_120);
    // The superseded prompt is withdrawn before the new one is posted.
    assert_eq!(h.next_effect().await, withdraw(1));
    assert_eq!(h.next_effect().await, notify(2, 100));
    h.clock.set(1_300);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn resuming_an_item_already_over_prompts_at_once() {
    let mut day = short_day();
    day.start(id(100), ts(0)).unwrap();
    day.pause(id(100), ts(90)).unwrap();
    let (mut h, _store) = running_on(1_000, Some(&day)).await;
    h.expect_no_effect().await;
    h.send(start_msg(100, 1));
    h.rev(2).await;
    assert_eq!(h.next_effect().await, notify(1, 100));
    h.expect_no_effect().await;
}

// --- Switch ---

fn switch_msg(n: u128, expected_rev: u64) -> EngineMsg {
    EngineMsg::Switch {
        item_id: id(n),
        expected_rev,
    }
}

fn switched_at(from: u128, to: Option<u128>, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Switched {
            from: id(from),
            to: to.map(id),
        },
    }
}

#[tokio::test]
async fn switch_finishes_the_running_item_and_starts_the_next_in_one_batch() {
    let (mut h, store) = started_a(&abc_day()).await;
    h.clock.set(1_030);
    h.send(switch_msg(100, 2));
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty());
    let day = day_of(&snap);
    let a = day.item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Done);
    assert_eq!(a.segments(), &[closed(1_000, 1_030)]);
    let b = day.item(id(101)).unwrap();
    assert_eq!(b.status(), ItemStatus::Running);
    assert_eq!(b.segments(), &[Segment::open(ts(1_030))]);
    assert_eq!(snap.running.unwrap().item_id, id(101));
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(
        store.day_log(day1()),
        vec![switched_at(100, Some(101), 1_030)]
    );
    let batches = store.batches();
    assert_eq!(batches.len(), 3, "startup, Start, Switch");
    let ops = batches[2].ops();
    assert!(
        ops.iter()
            .any(|op| matches!(op, StoreOp::CloseSegment { .. }))
    );
    assert!(
        ops.iter()
            .any(|op| matches!(op, StoreOp::OpenSegment { .. }))
    );
    assert_eq!(
        ops.last(),
        Some(&StoreOp::AppendDayLog {
            date: day1(),
            entry: switched_at(100, Some(101), 1_030)
        })
    );
}

#[tokio::test]
async fn switch_resumes_a_paused_next_item() {
    let mut day = abc_day();
    day.start(id(100), ts(10)).unwrap();
    day.pause(id(100), ts(40)).unwrap();
    let (mut h, store) = running_on(1_000, Some(&day)).await;
    h.send(start_msg(101, 1));
    h.rev(2).await;
    h.clock.set(1_050);
    h.send(switch_msg(101, 2));
    let snap = h.rev(3).await;
    let a = day_of(&snap).item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Running);
    assert_eq!(a.segments(), &[closed(10, 40), Segment::open(ts(1_050))]);
    assert_eq!(snap.running.as_ref().unwrap().accumulated_secs, 30);
    assert_eq!(
        day_of(&snap).item(id(101)).unwrap().status(),
        ItemStatus::Done
    );
    assert_eq!(
        store.day_log(day1()),
        vec![switched_at(101, Some(100), 1_050)]
    );
}

#[tokio::test]
async fn switch_with_nothing_left_finishes_and_logs_no_target() {
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "A", "U")).unwrap();
    day.add(item(101, "B", "V")).unwrap();
    day.remove(id(101), ts(5)).unwrap();
    let (mut h, store) = started_a(&day).await;
    h.clock.set(1_020);
    h.send(switch_msg(100, 2));
    let snap = h.rev(3).await;
    assert_eq!(
        day_of(&snap).item(id(100)).unwrap().status(),
        ItemStatus::Done
    );
    assert_eq!(snap.running, None);
    assert_eq!(snap.next_item, None);
    assert_eq!(store.day_log(day1()), vec![switched_at(100, None, 1_020)]);
}

#[tokio::test]
async fn a_stale_switch_applies_nothing() {
    let (mut h, store) = started_a(&abc_day()).await;
    let snap = expect_rejected(&mut h, &store, switch_msg(100, 1), 3).await;
    assert_eq!(rejected(&snap), Some("stale rev: expected 1, current 2"));
    assert_eq!(snap.running.unwrap().item_id, id(100));
    assert_eq!(store.day_log(day1()), vec![]);
}

#[tokio::test]
async fn switch_from_a_non_running_item_is_rejected() {
    let mut day = abc_day();
    day.add(item(103, "D", "X")).unwrap();
    day.start(id(103), ts(10)).unwrap();
    day.finish(id(103), ts(20)).unwrap();
    day.start(id(102), ts(30)).unwrap();
    day.pause(id(102), ts(40)).unwrap();
    let (mut h, store) = started_a(&day).await;
    // Pending, paused, done, unknown; each with the current rev.
    for (rev, n) in (2..).zip([101, 102, 103, 999]) {
        let snap = expect_rejected(&mut h, &store, switch_msg(n, rev), rev + 1).await;
        assert!(rejected(&snap).unwrap().starts_with("Switch: "), "{n}");
    }
    assert_eq!(store.day_log(day1()), vec![]);
    assert_eq!(h.rx.borrow().running.as_ref().unwrap().item_id, id(100));
}

/// A slot as `(item, kind, (start, end))` in whole seconds.
type SlotSecs = (ItemId, SlotKind, Option<(i64, i64)>);

fn spans(snap: &Snapshot) -> Vec<SlotSecs> {
    snap.schedule
        .slots()
        .iter()
        .map(|s| {
            let span = s.span.map(|sp| (sp.start.as_second(), sp.end.as_second()));
            (s.item_id, s.kind, span)
        })
        .collect()
}

#[tokio::test]
async fn the_snapshot_carries_the_schedule_reflowed_at_publish_time() {
    // A and B flexible, C fixed at 1_900; all 600 s.
    let mut day = abc_day();
    day.set_fixed(id(102), ts(1_900)).unwrap();
    let (mut h, _store) = running_on(1_000, Some(&day)).await;

    h.send(start_msg(100, 1));
    let snap = h.rev(2).await;
    // A runs to 1_600; B would overlap C, so it moves past it.
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Running, Some((1_000, 1_600))),
            (id(102), SlotKind::Fixed, Some((1_900, 2_500))),
            (id(101), SlotKind::Flexible, Some((2_500, 3_100))),
        ]
    );
    assert_eq!(snap.next_item, None);

    // Paused at 1_300, A keeps its remaining 300 s and flows from now.
    h.clock.set(1_300);
    h.send(EngineMsg::Pause { item_id: id(100) });
    let snap = h.rev(3).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Flexible, Some((1_300, 1_600))),
            (id(102), SlotKind::Fixed, Some((1_900, 2_500))),
            (id(101), SlotKind::Flexible, Some((2_500, 3_100))),
        ]
    );
    assert_eq!(snap.next_item.as_ref().map(|n| n.item_id), Some(id(100)));
    assert_eq!(snap.schedule.overflow(ts(2_500)), vec![id(101)]);
}

#[tokio::test]
async fn next_item_and_switch_follow_the_schedule_past_a_due_pin() {
    // C is pinned at 500, before now, so it comes first and Switch starts it.
    let mut day = abc_day();
    day.set_fixed(id(102), ts(500)).unwrap();
    let (mut h, _store) = running_on(1_000, Some(&day)).await;
    let next = h.rx.borrow().next_item.as_ref().map(|n| n.item_id);
    assert_eq!(next, Some(id(102)));

    h.send(start_msg(100, 1));
    h.rev(2).await;
    h.clock.set(1_100);
    h.send(switch_msg(100, 2));
    let snap = h.rev(3).await;
    assert_eq!(snap.running.as_ref().map(|r| r.item_id), Some(id(102)));
}

// --- Extend ---

fn extend_msg(n: u128, secs: u64, expected_rev: u64) -> EngineMsg {
    EngineMsg::ExtendTask {
        item_id: id(n),
        secs,
        expected_rev,
    }
}

fn extended_at(n: u128, secs: u64, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Extended {
            item_id: id(n),
            secs,
        },
    }
}

fn withdraw(prompt_n: u128) -> Effect {
    Effect::WithdrawNotification {
        prompt_id: prompt(prompt_n),
    }
}

/// R (100) running from 1_000 at rev 2, planned 600 s, so it is due at 1_600.
async fn r_running() -> (Harness, Arc<MemoryStore>) {
    started_a(&abc_day()).await
}

/// R prompted at its planned end 1_600 (prompt 1).
async fn r_prompted() -> (Harness, Arc<MemoryStore>) {
    let (mut h, store) = r_running().await;
    h.clock.set(1_600);
    assert_eq!(h.next_effect().await, notify(1, 100));
    (h, store)
}

#[tokio::test]
async fn extend_shifts_later_flexible_items_and_logs_in_one_batch() {
    // R and A flexible, F fixed at 5_000; all 600 s.
    let mut day = abc_day();
    day.set_fixed(id(102), ts(5_000)).unwrap();
    let (mut h, store) = started_a(&day).await;
    h.clock.set(1_100);
    h.send(extend_msg(100, 300, 2));
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty());
    assert_eq!(day_of(&snap).item(id(100)).unwrap().planned_secs(), 900);
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Running, Some((1_000, 1_900))),
            (id(101), SlotKind::Flexible, Some((1_900, 2_500))),
            (id(102), SlotKind::Fixed, Some((5_000, 5_600))),
        ]
    );
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.day_log(day1()), vec![extended_at(100, 300, 1_100)]);
    // The heartbeat due at 1_060 may commit before or after the extension (the
    // loop's select picks either), so find the extension's batch by its entry: the
    // planned-length change and the log entry share that one batch.
    let entry = StoreOp::AppendDayLog {
        date: day1(),
        entry: extended_at(100, 300, 1_100),
    };
    let batches = store.batches();
    let with_entry: Vec<_> = batches
        .iter()
        .filter(|b| b.ops().contains(&entry))
        .collect();
    assert_eq!(with_entry.len(), 1);
    let ops = with_entry[0].ops();
    assert!(ops.iter().any(|op| matches!(op, StoreOp::PutItem { .. })));
    assert_eq!(ops.last(), Some(&entry));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn extend_keeps_a_fixed_item_in_place_and_flows_flexible_past_it() {
    // R 600 s, A flexible 300 s, F fixed at 1_900 for 300 s.
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "R", "U")).unwrap();
    day.add(PlanItem::new(id(101), "A", 300, key("V"))).unwrap();
    day.add(PlanItem::new(id(102), "F", 300, key("W"))).unwrap();
    day.set_fixed(id(102), ts(1_900)).unwrap();
    let (mut h, _store) = started_a(&day).await;
    assert_eq!(
        spans(&h.rx.borrow()),
        vec![
            (id(100), SlotKind::Running, Some((1_000, 1_600))),
            (id(101), SlotKind::Flexible, Some((1_600, 1_900))),
            (id(102), SlotKind::Fixed, Some((1_900, 2_200))),
        ]
    );
    h.clock.set(1_100);
    h.send(extend_msg(100, 300, 2));
    let snap = h.rev(3).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Running, Some((1_000, 1_900))),
            (id(102), SlotKind::Fixed, Some((1_900, 2_200))),
            (id(101), SlotKind::Flexible, Some((2_200, 2_500))),
        ]
    );
}

#[tokio::test]
async fn extend_after_the_prompt_withdraws_it_and_re_arms_at_the_new_end() {
    let (mut h, _store) = r_prompted().await;
    h.clock.set(1_610);
    h.send(extend_msg(100, 300, 2));
    h.rev(3).await;
    assert_eq!(h.next_effect().await, withdraw(1));
    h.clock.set(1_899);
    h.expect_no_effect().await;
    h.clock.set(1_900);
    assert_eq!(h.next_effect().await, notify(2, 100));
    h.clock.set(2_100);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn extend_before_the_prompt_withdraws_nothing_and_prompts_at_the_new_end() {
    let (mut h, _store) = r_running().await;
    h.clock.set(1_100);
    h.send(extend_msg(100, 300, 2));
    h.rev(3).await;
    h.expect_no_effect().await;
    h.clock.set(1_600);
    h.expect_no_effect().await;
    h.clock.set(1_899);
    h.expect_no_effect().await;
    h.clock.set(1_900);
    assert_eq!(h.next_effect().await, notify(1, 100));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn extend_that_leaves_the_item_over_withdraws_then_prompts_at_once() {
    let (mut h, _store) = r_prompted().await;
    h.clock.set(2_500);
    h.send(extend_msg(100, 300, 2));
    h.rev(3).await;
    assert_eq!(h.next_effect().await, withdraw(1));
    assert_eq!(h.next_effect().await, notify(2, 100));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn a_stale_extend_applies_nothing_and_emits_nothing() {
    let (mut h, store) = r_prompted().await;
    let snap = expect_rejected(&mut h, &store, extend_msg(100, 300, 1), 3).await;
    assert_eq!(rejected(&snap), Some("stale rev: expected 1, current 2"));
    assert_eq!(day_of(&snap).item(id(100)).unwrap().planned_secs(), 600);
    assert_eq!(store.day_log(day1()), vec![]);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn extend_of_a_non_running_or_unknown_item_is_rejected() {
    let mut day = abc_day();
    day.add(item(103, "D", "X")).unwrap();
    day.start(id(103), ts(10)).unwrap();
    day.finish(id(103), ts(20)).unwrap();
    day.start(id(102), ts(30)).unwrap();
    day.pause(id(102), ts(40)).unwrap();
    let (mut h, store) = started_a(&day).await;
    // Pending, paused, done, unknown; each with the current rev.
    for (rev, n) in (2..).zip([101, 102, 103, 999]) {
        let snap = expect_rejected(&mut h, &store, extend_msg(n, 300, rev), rev + 1).await;
        assert!(rejected(&snap).unwrap().starts_with("ExtendTask: "), "{n}");
    }
    // A huge extension of a non-running item is a transition reject, not overflow.
    let snap = expect_rejected(&mut h, &store, extend_msg(101, u64::MAX, 6), 7).await;
    assert_eq!(
        rejected(&snap),
        Some("ExtendTask: cannot extend an item that is pending")
    );
    assert_eq!(store.day_log(day1()), vec![]);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn extend_by_zero_or_past_u64_is_invalid_input() {
    let (mut h, store) = r_prompted().await;
    let snap = expect_rejected(&mut h, &store, extend_msg(100, 0, 2), 3).await;
    assert_eq!(rejected(&snap), Some("ExtendTask: secs must be positive"));
    let snap = expect_rejected(&mut h, &store, extend_msg(100, u64::MAX, 3), 4).await;
    assert_eq!(
        rejected(&snap),
        Some(format!("ExtendTask: item {} planned_secs would overflow", id(100)).as_str())
    );
    assert_eq!(day_of(&snap).item(id(100)).unwrap().planned_secs(), 600);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn a_failed_extend_commit_keeps_the_plan_and_withdraws_nothing() {
    let (mut h, store) = r_prompted().await;
    store.fail_commits(true);
    h.clock.set(1_610);
    h.send(extend_msg(100, 300, 2));
    let snap = h.rev(3).await;
    assert_eq!(snap.status, store_status());
    assert_eq!(day_of(&snap).item(id(100)).unwrap().planned_secs(), 600);
    assert_eq!(store.day_log(day1()), vec![]);
    h.expect_no_effect().await;
    // Not re-armed: the old end was already prompted.
    h.clock.set(1_900);
    h.expect_no_effect().await;
}

// --- Insert ---

fn insert_msg(
    title: &str,
    planned_secs: u64,
    anchor: InsertAnchor,
    expected_rev: u64,
) -> EngineMsg {
    EngineMsg::InsertTask {
        title: title.into(),
        planned_secs,
        anchor,
        expected_rev,
    }
}

fn inserted_at(n: u128, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Inserted { item_id: id(n) },
    }
}

/// The one committed batch holding `entry`. The heartbeat may commit on its own
/// around it, so batches are found by their entry, not their index.
fn batch_with(store: &MemoryStore, entry: DayLogEntry) -> Vec<StoreOp> {
    let op = StoreOp::AppendDayLog {
        date: day1(),
        entry,
    };
    let batches: Vec<_> = store
        .batches()
        .into_iter()
        .filter(|b| b.ops().contains(&op))
        .collect();
    assert_eq!(batches.len(), 1);
    let ops = batches[0].ops().to_vec();
    assert_eq!(ops.last(), Some(&op), "the entry comes last");
    ops
}

/// A (100) running from 1_000 at rev 2, B (101) and C (102) pending, all 600 s;
/// N (1, 300 s) inserted with `Now` at 1_100, at rev 3.
async fn n_interrupts_a() -> (Harness, Arc<MemoryStore>) {
    let (mut h, store) = started_a(&abc_day()).await;
    h.clock.set(1_100);
    h.send(insert_msg("N", 300, InsertAnchor::Now, 2));
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty(), "{:?}", snap.status);
    (h, store)
}

#[tokio::test]
async fn insert_now_pauses_the_running_item_onto_the_stack_and_starts_the_new_one() {
    let (h, store) = n_interrupts_a().await;
    let snap = h.rx.borrow().clone();
    let day = day_of(&snap);
    let a = day.item(id(100)).unwrap();
    assert_eq!(a.status(), ItemStatus::Paused);
    assert_eq!(a.segments(), &[closed(1_000, 1_100)]);
    let n = day.item(id(1)).unwrap();
    assert_eq!((n.title(), n.planned_secs(), n.fixed()), ("N", 300, false));
    assert_eq!(n.segments(), &[Segment::open(ts(1_100))]);
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(1));
    assert_eq!(day.interrupt_stack(), &[id(100)]);
    assert_eq!(
        spans(&snap),
        vec![
            (id(1), SlotKind::Running, Some((1_100, 1_400))),
            (id(100), SlotKind::Flexible, Some((1_400, 1_900))),
            (id(101), SlotKind::Flexible, Some((1_900, 2_500))),
            (id(102), SlotKind::Flexible, Some((2_500, 3_100))),
        ]
    );
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(store.day_log(day1()), vec![inserted_at(1, 1_100)]);
    let ops = batch_with(&store, inserted_at(1, 1_100));
    assert!(ops.contains(&StoreOp::CloseSegment {
        item_id: id(100),
        started_at: ts(1_000),
        ended_at: ts(1_100),
    }));
    assert!(ops.contains(&StoreOp::OpenSegment {
        item_id: id(1),
        started_at: ts(1_100),
    }));
    assert!(ops.contains(&StoreOp::SetInterruptStack {
        date: day1(),
        items: vec![id(100)],
    }));
}

#[tokio::test]
async fn switch_from_the_inserted_item_resumes_the_interrupted_one() {
    let (mut h, store) = n_interrupts_a().await;
    h.clock.set(1_200);
    h.send(switch_msg(1, 3));
    let snap = h.rev(4).await;
    let day = day_of(&snap);
    assert_eq!(day.item(id(1)).unwrap().status(), ItemStatus::Done);
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(100), "A, not B");
    assert_eq!(day.item(id(101)).unwrap().status(), ItemStatus::Pending);
    assert!(day.interrupt_stack().is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
    assert_eq!(
        store.day_log(day1()),
        vec![inserted_at(1, 1_100), switched_at(1, Some(100), 1_200)]
    );
}

#[tokio::test]
async fn nested_insert_now_resumes_in_reverse_order() {
    let (mut h, store) = n_interrupts_a().await;
    h.clock.set(1_150);
    h.send(insert_msg("M", 300, InsertAnchor::Now, 3));
    let snap = h.rev(4).await;
    assert_eq!(day_of(&snap).interrupt_stack(), &[id(100), id(1)]);
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(2));
    // N first, then A: the stacked items flow top first.
    let flow: Vec<ItemId> = spans(&snap).into_iter().map(|(i, ..)| i).collect();
    assert_eq!(flow, vec![id(2), id(1), id(100), id(101), id(102)]);

    h.send(switch_msg(2, 4));
    let snap = h.rev(5).await;
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(1));
    assert_eq!(day_of(&snap).interrupt_stack(), &[id(100)]);
    h.send(switch_msg(1, 5));
    let snap = h.rev(6).await;
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(100));
    assert!(day_of(&snap).interrupt_stack().is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn insert_now_while_idle_starts_the_new_item_and_pushes_nothing() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(insert_msg("N", 300, InsertAnchor::Now, 1));
    let snap = h.rev(2).await;
    let day = day_of(&snap);
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(1));
    assert!(day.interrupt_stack().is_empty());
    assert!(day.item(id(1)).unwrap().order() > day.item(id(102)).unwrap().order());
    let ops = batch_with(&store, inserted_at(1, 1_000));
    assert!(
        !ops.iter()
            .any(|op| matches!(op, StoreOp::SetInterruptStack { .. }))
    );
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn insert_after_the_current_item_flows_right_after_it() {
    let (mut h, store) = started_a(&abc_day()).await;
    h.send(insert_msg("N", 300, InsertAnchor::After(id(100)), 2));
    let snap = h.rev(3).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Running, Some((1_000, 1_600))),
            (id(1), SlotKind::Flexible, Some((1_600, 1_900))),
            (id(101), SlotKind::Flexible, Some((1_900, 2_500))),
            (id(102), SlotKind::Flexible, Some((2_500, 3_100))),
        ]
    );
    let day = day_of(&snap);
    assert_eq!(day.item(id(1)).unwrap().status(), ItemStatus::Pending);
    assert!(day.interrupt_stack().is_empty());
    assert_eq!(store.day_log(day1()), vec![inserted_at(1, 1_000)]);
}

#[tokio::test]
async fn insert_after_or_before_an_item_lands_between_its_neighbours() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    h.send(insert_msg("N", 300, InsertAnchor::After(id(101)), 1));
    let snap = h.rev(2).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Flexible, Some((1_000, 1_600))),
            (id(101), SlotKind::Flexible, Some((1_600, 2_200))),
            (id(1), SlotKind::Flexible, Some((2_200, 2_500))),
            (id(102), SlotKind::Flexible, Some((2_500, 3_100))),
        ]
    );
    h.send(insert_msg("M", 300, InsertAnchor::Before(id(102)), 2));
    let snap = h.rev(3).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Flexible, Some((1_000, 1_600))),
            (id(101), SlotKind::Flexible, Some((1_600, 2_200))),
            (id(1), SlotKind::Flexible, Some((2_200, 2_500))),
            (id(2), SlotKind::Flexible, Some((2_500, 2_800))),
            (id(102), SlotKind::Flexible, Some((2_800, 3_400))),
        ]
    );
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn insert_at_end_goes_last_among_flexible_items_and_fixed_items_never_move() {
    // C fixed at 5_000; A, B flexible.
    let mut day = abc_day();
    day.set_fixed(id(102), ts(5_000)).unwrap();
    let (mut h, _store) = running_on(1_000, Some(&day)).await;
    let fixed_span = (id(102), SlotKind::Fixed, Some((5_000, 5_600)));
    h.send(insert_msg("N", 300, InsertAnchor::End, 1));
    let snap = h.rev(2).await;
    assert_eq!(
        spans(&snap),
        vec![
            (id(100), SlotKind::Flexible, Some((1_000, 1_600))),
            (id(101), SlotKind::Flexible, Some((1_600, 2_200))),
            (id(1), SlotKind::Flexible, Some((2_200, 2_500))),
            fixed_span,
        ]
    );
    for (rev, anchor) in (2..).zip([
        InsertAnchor::After(id(100)),
        InsertAnchor::Before(id(101)),
        InsertAnchor::Now,
        InsertAnchor::Now,
        InsertAnchor::After(id(1)),
    ]) {
        h.send(insert_msg("X", 300, anchor, rev));
        let snap = h.rev(rev + 1).await;
        assert!(snap.status.is_empty(), "{anchor:?}: {:?}", snap.status);
        assert!(spans(&snap).contains(&fixed_span), "{anchor:?}");
    }
}

#[tokio::test]
async fn starting_a_stacked_item_by_hand_drops_its_entry() {
    let (mut h, store) = n_interrupts_a().await;
    h.send(start_msg(100, 3));
    let snap = h.rev(4).await;
    let day = day_of(&snap);
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(100));
    assert_eq!(day.item(id(1)).unwrap().status(), ItemStatus::Paused);
    assert!(day.interrupt_stack().is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn the_interrupt_stack_survives_an_engine_restart() {
    let (h, store) = n_interrupts_a().await;
    drop(h);
    // N's open segment is recovered (paused) on restart; the stack is kept.
    let mut h = start(&FakeClock::at(1_300), &store);
    let snap = h.rev(1).await;
    let day = day_of(&snap);
    assert_eq!(day.interrupt_stack(), &[id(100)]);
    assert_eq!(day.item(id(1)).unwrap().status(), ItemStatus::Paused);
    assert_eq!(snap.next_item.as_ref().map(|n| n.item_id), Some(id(100)));
    h.send(start_msg(1, 1));
    h.rev(2).await;
    h.send(switch_msg(1, 2));
    let snap = h.rev(3).await;
    assert_eq!(snap.running.as_ref().unwrap().item_id, id(100));
    assert!(day_of(&snap).interrupt_stack().is_empty());
}

#[tokio::test]
async fn a_stale_insert_applies_nothing() {
    let (mut h, store) = started_a(&abc_day()).await;
    for (rev, anchor) in (3..).zip([InsertAnchor::Now, InsertAnchor::After(id(101))]) {
        let snap = expect_rejected(&mut h, &store, insert_msg("N", 300, anchor, 1), rev).await;
        assert_eq!(
            rejected(&snap),
            Some(format!("stale rev: expected 1, current {}", rev - 1).as_str())
        );
    }
    assert_eq!(store.day_log(day1()), vec![]);
}

#[tokio::test]
async fn insert_with_bad_input_or_a_bad_anchor_is_rejected() {
    let mut day = abc_day();
    day.add(item(103, "F", "X")).unwrap();
    day.set_fixed(id(103), ts(5_000)).unwrap();
    day.add(item(104, "R", "Y")).unwrap();
    day.remove(id(104), ts(5)).unwrap();
    let (mut h, store) = started_a(&day).await;
    let msgs = [
        ("", 300, InsertAnchor::End),
        ("  ", 300, InsertAnchor::Now),
        ("N", 0, InsertAnchor::End),
        ("N", 300, InsertAnchor::After(id(104))),
        ("N", 300, InsertAnchor::Before(id(999))),
        ("N", 300, InsertAnchor::After(id(103))),
        ("N", 300, InsertAnchor::Before(id(103))),
        ("N", 300, InsertAnchor::Before(id(100))),
    ];
    for (rev, (title, secs, anchor)) in (2..).zip(msgs) {
        let snap = expect_rejected(
            &mut h,
            &store,
            insert_msg(title, secs, anchor, rev),
            rev + 1,
        )
        .await;
        assert!(
            rejected(&snap).unwrap().starts_with("InsertTask: "),
            "{anchor:?}: {:?}",
            rejected(&snap)
        );
    }
    assert_eq!(store.day_log(day1()), vec![]);
}

#[tokio::test]
async fn insert_next_to_an_interrupted_item_is_rejected() {
    let (mut h, store) = n_interrupts_a().await;
    for (rev, anchor) in (4..).zip([InsertAnchor::Before(id(100)), InsertAnchor::After(id(100))]) {
        let snap =
            expect_rejected(&mut h, &store, insert_msg("X", 300, anchor, rev - 1), rev).await;
        assert_eq!(
            rejected(&snap),
            Some("InsertTask: anchor 00000000-0000-0000-0000-000000000064 is interrupted"),
            "{anchor:?}"
        );
    }
    assert_eq!(store.day_log(day1()), vec![inserted_at(1, 1_100)]);
}

#[tokio::test]
async fn a_bad_stored_interrupt_stack_fails_the_load_with_store_status() {
    let mut paused = abc_day();
    paused.start(id(100), ts(10)).unwrap();
    paused.pause(id(100), ts(20)).unwrap();
    for (day, stack) in [
        (abc_day(), vec![id(101)]),
        (paused.clone(), vec![id(100), id(100)]),
    ] {
        let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
        store.seed(&day);
        store.seed_interrupt_stack(day1(), stack.clone());
        let mut h = start(&clock, &store);
        let snap = h.rev(1).await;
        assert_eq!(snap.day, Some(Day::new(day1(), "UTC")), "{stack:?}");
        let status = snap.status.iter().find(|e| e.key == StatusKey::Store);
        assert!(status.is_some(), "{stack:?}");
    }
}

// --- Work-end and overflow ---

/// 17:00 on day 1 (UTC), the default work-end.
const WORK_END: i64 = 61_200;
/// 16:00 on day 1.
const FOUR_PM: i64 = 57_600;

fn work_end(minutes: u32) -> WorkEnd {
    WorkEnd::from_minutes(minutes).unwrap()
}

fn set_work_end(minutes: u32) -> EngineMsg {
    EngineMsg::SetWorkEnd { minutes }
}

fn notify_overflow(prompt_n: u128, item_n: u128) -> Effect {
    Effect::Notify {
        prompt_id: prompt(prompt_n),
        item_id: id(item_n),
        kind: PromptKind::Overflow,
    }
}

fn overflow_status(snap: &Snapshot) -> Option<&str> {
    snap.status
        .iter()
        .find(|e| e.key == StatusKey::Overflow)
        .map(|e| e.message.as_str())
}

/// A (100, 15 min) running from 16:00 at rev 2 and B (101, 40 min) pending, so B
/// ends at 16:55, just inside work-end.
async fn a_running_at_four() -> (Harness, Arc<MemoryStore>) {
    let mut day = Day::new(day1(), "UTC");
    day.add(PlanItem::new(id(100), "A", 900, key("U"))).unwrap();
    day.add(PlanItem::new(id(101), "B", 2_400, key("V")))
        .unwrap();
    let (mut h, store) = running_on(FOUR_PM, Some(&day)).await;
    h.send(start_msg(100, 1));
    let snap = h.rev(2).await;
    assert_eq!(
        snap.schedule.slot(id(101)).unwrap().span.unwrap().end,
        ts(60_900)
    );
    assert!(snap.overflow.is_empty());
    (h, store)
}

/// `a_running_at_four` with A extended by 15 min at rev 3: B ends 17:10, so the
/// episode opened with prompt 1.
async fn b_overflowing() -> (Harness, Arc<MemoryStore>) {
    let (mut h, store) = a_running_at_four().await;
    h.send(extend_msg(100, 900, 2));
    let snap = h.rev(3).await;
    assert_eq!(snap.overflow, vec![id(101)]);
    assert_eq!(h.next_effect().await, notify_overflow(1, 101));
    (h, store)
}

#[tokio::test]
async fn no_overflow_publishes_an_empty_list_and_no_status_or_notify() {
    let (mut h, _store) = running_on(1_000, Some(&abc_day())).await;
    let snap = h.rx.borrow().clone();
    assert!(snap.overflow.is_empty());
    assert_eq!(snap.work_end, WorkEnd::DEFAULT);
    assert_eq!(overflow_status(&snap), None);
    h.expect_no_effect().await;
}

#[tokio::test]
async fn reflow_past_work_end_lists_the_item_sets_status_and_notifies_once() {
    let (mut h, _store) = b_overflowing().await;
    let snap = h.rx.borrow().clone();
    assert_eq!(
        snap.schedule.slot(id(101)).unwrap().span.unwrap().end,
        ts(61_800)
    );
    assert_eq!(
        overflow_status(&snap),
        Some("1 item runs past work-end 17:00")
    );
    // Heartbeats pass while still overflowing: no second prompt, no new snapshot.
    for at in [FOUR_PM + 60, FOUR_PM + 120] {
        h.clock.set(at);
    }
    h.expect_no_effect().await;
    assert_eq!(h.rx.borrow().rev, 3);
}

#[tokio::test]
async fn a_publish_within_an_episode_updates_the_list_without_a_new_notify() {
    let (mut h, _store) = b_overflowing().await;
    h.send(create("C", 600));
    let snap = h.rev(4).await;
    assert_eq!(snap.overflow, vec![id(101), id(1)]);
    assert_eq!(
        overflow_status(&snap),
        Some("2 items run past work-end 17:00")
    );
    h.expect_no_effect().await;
}

#[tokio::test]
async fn removing_the_leftover_clears_status_and_withdraws_the_prompt() {
    let (mut h, _store) = b_overflowing().await;
    h.send(EngineMsg::RemoveTask { item_id: id(101) });
    let snap = h.rev(4).await;
    assert!(snap.overflow.is_empty());
    assert_eq!(overflow_status(&snap), None);
    assert_eq!(h.next_effect().await, withdraw(1));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn a_later_work_end_clears_and_an_earlier_one_opens_a_new_episode() {
    let (mut h, _store) = b_overflowing().await;
    h.send(set_work_end(18 * 60));
    let snap = h.rev(4).await;
    assert_eq!(snap.work_end, work_end(18 * 60));
    assert!(snap.overflow.is_empty());
    assert_eq!(overflow_status(&snap), None);
    assert_eq!(h.next_effect().await, withdraw(1));

    h.send(set_work_end(17 * 60));
    let snap = h.rev(5).await;
    assert_eq!(snap.overflow, vec![id(101)]);
    assert_eq!(
        overflow_status(&snap),
        Some("1 item runs past work-end 17:00")
    );
    assert_eq!(h.next_effect().await, notify_overflow(2, 101));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn time_passing_alone_opens_an_episode_at_a_heartbeat() {
    // Nothing runs: A flows from now and ends 16:59.
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "A", "U")).unwrap();
    let start_at = WORK_END - 660;
    let (mut h, _store) = running_on(start_at, Some(&day)).await;
    assert!(h.rx.borrow().overflow.is_empty());
    // Ending exactly at work-end is not overflow: no publish.
    h.clock.set(start_at + 60);
    h.expect_no_effect().await;
    assert_eq!(h.rx.borrow().rev, 1);
    h.clock.set(start_at + 120);
    let snap = h.rev(2).await;
    assert_eq!(snap.overflow, vec![id(100)]);
    assert_eq!(
        overflow_status(&snap),
        Some("1 item runs past work-end 17:00")
    );
    assert_eq!(h.next_effect().await, notify_overflow(1, 100));
}

#[tokio::test]
async fn set_work_end_commits_one_batch_and_the_same_value_commits_nothing() {
    let (mut h, store) = running_on(1_000, None).await;
    h.send(set_work_end(18 * 60));
    let snap = h.rev(2).await;
    assert_eq!(snap.work_end, work_end(18 * 60));
    assert!(snap.status.is_empty());
    let batches = store.batches();
    let with_op: Vec<_> = batches
        .iter()
        .filter(|b| {
            b.ops()
                .iter()
                .any(|op| matches!(op, StoreOp::SetWorkEnd { .. }))
        })
        .collect();
    assert_eq!(with_op.len(), 1);
    assert_eq!(
        with_op[0].ops(),
        [StoreOp::SetWorkEnd {
            work_end: work_end(18 * 60)
        }]
    );

    let count = store.batches().len();
    h.send(set_work_end(18 * 60));
    h.send(create("A", 600));
    let snap = h.rev(3).await;
    assert_eq!(snap.day.unwrap().items().len(), 1, "rev 3 is the create");
    assert_eq!(store.batches().len(), count + 1);
}

#[tokio::test]
async fn set_work_end_out_of_range_is_rejected() {
    let (mut h, store) = running_on(1_000, None).await;
    let batches = store.batches().len();
    h.send(set_work_end(1_440));
    let snap = h.rev(2).await;
    assert_eq!(
        rejected(&snap),
        Some("SetWorkEnd: minutes 1440 is not between 0 and 1439")
    );
    assert_eq!(snap.work_end, WorkEnd::DEFAULT);
    assert_eq!(store.batches().len(), batches);
}

#[tokio::test]
async fn a_failed_set_work_end_commit_keeps_the_old_value() {
    let (mut h, store) = running_on(1_000, None).await;
    store.fail_commits(true);
    h.send(set_work_end(18 * 60));
    let snap = h.rev(2).await;
    assert_eq!(snap.work_end, WorkEnd::DEFAULT);
    assert_eq!(snap.status, store_status());
}

#[tokio::test]
async fn the_work_end_survives_an_engine_restart() {
    let (mut h, store) = running_on(1_000, None).await;
    h.send(set_work_end(18 * 60 + 30));
    h.rev(2).await;
    drop(h);
    let mut h = start(&FakeClock::at(1_100), &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.work_end, work_end(18 * 60 + 30));
    assert!(snap.status.is_empty());
}

#[tokio::test]
async fn a_failed_settings_load_uses_the_default_with_store_status() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    store.seed_work_end(work_end(18 * 60 + 30));
    store.fail_loads(true);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.work_end, WorkEnd::DEFAULT);
    assert_eq!(snap.status, store_status());
}

// --- Carry-over and midnight rollover ---

/// Midnight between day 1 and day 2 (UTC).
const MIDNIGHT: i64 = DAY_SECS;

fn carry(expected_rev: u64) -> EngineMsg {
    EngineMsg::CarryOverToTomorrow { expected_rev }
}

fn carried_at(n: u128, to: u128, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::CarriedOver {
            item_id: id(n),
            to: id(to),
        },
    }
}

/// `day` (A 100 then `rest`) seeded, A started at 16:00 at rev 2.
async fn a_started_at_four(day: &Day) -> (Harness, Arc<MemoryStore>) {
    let (mut h, store) = running_on(FOUR_PM, Some(day)).await;
    h.send(start_msg(100, 1));
    h.rev(2).await;
    (h, store)
}

/// A (100, 45 min) running from 16:00 at rev 2, then B (101, 40 min) and C (102,
/// 30 min) pending, ending 17:25 and 17:55: overflow [B, C], episode prompt 1.
/// Today's zone is `tz`.
async fn leftovers(tz: &str) -> (Harness, Arc<MemoryStore>) {
    let mut day = Day::new(day1(), tz);
    day.add(PlanItem::new(id(100), "A", 2_700, key("U")))
        .unwrap();
    day.add(PlanItem::new(id(101), "B", 2_400, key("V")))
        .unwrap();
    day.add(PlanItem::new(id(102), "C", 1_800, key("W")))
        .unwrap();
    let (mut h, store) = a_started_at_four(&day).await;
    assert_eq!(h.rx.borrow().overflow, vec![id(101), id(102)]);
    assert_eq!(h.next_effect().await, notify_overflow(1, 101));
    (h, store)
}

#[tokio::test]
async fn carry_over_moves_the_leftovers_to_a_new_tomorrow_in_one_batch() {
    let (mut h, store) = leftovers("Europe/Berlin").await;
    let batches = store.batches().len();
    h.send(carry(2));
    let snap = h.rev(3).await;
    let today = day_of(&snap);
    for n in [101, 102] {
        assert_eq!(today.item(id(n)).unwrap().status(), ItemStatus::CarriedOver);
    }
    assert_eq!(today.item(id(100)).unwrap().status(), ItemStatus::Running);
    assert_eq!(rejected(&snap), None);

    let tomorrow = store.stored_day(day2()).unwrap();
    assert_eq!(tomorrow.tz(), "Europe/Berlin");
    let new: Vec<_> = [1, 2].map(|n| tomorrow.item(id(n)).unwrap().clone()).into();
    assert_eq!(tomorrow.items().len(), 2);
    for (item, (title, secs, from)) in new.iter().zip([("B", 2_400, 101), ("C", 1_800, 102)]) {
        assert_eq!(item.title(), title);
        assert_eq!(item.planned_secs(), secs);
        assert_eq!(item.carried_from(), Some(id(from)));
        assert_eq!(item.status(), ItemStatus::Pending);
        assert!(!item.fixed());
    }
    assert!(new[0].order() < new[1].order());
    assert_eq!(
        store.day_log(day1()),
        vec![carried_at(101, 1, FOUR_PM), carried_at(102, 2, FOUR_PM)]
    );
    assert!(store.day_log(day2()).is_empty());
    assert_eq!(store.batches().len(), batches + 1, "one batch");

    assert!(snap.overflow.is_empty());
    assert_eq!(overflow_status(&snap), None);
    assert_eq!(h.next_effect().await, withdraw(1));
    h.expect_no_effect().await;
}

#[tokio::test]
async fn a_paused_leftover_carries_only_its_remaining_time() {
    let mut day = Day::new(day1(), "UTC");
    day.add(PlanItem::new(id(100), "A", 2_700, key("U")))
        .unwrap();
    day.add(PlanItem::new(id(101), "P", 2_400, key("V")))
        .unwrap();
    // P: 10 of 40 min done, so it flows 16:45–17:15.
    day.start(id(101), ts(1_000)).unwrap();
    day.pause(id(101), ts(1_600)).unwrap();
    let (mut h, store) = a_started_at_four(&day).await;
    assert_eq!(h.rx.borrow().overflow, vec![id(101)]);
    h.send(carry(2));
    let snap = h.rev(3).await;
    assert_eq!(
        day_of(&snap).item(id(101)).unwrap().status(),
        ItemStatus::CarriedOver
    );
    let carried = store
        .stored_day(day2())
        .unwrap()
        .item(id(1))
        .unwrap()
        .clone();
    assert_eq!(carried.planned_secs(), 1_800);
    assert!(carried.segments().is_empty());
}

#[tokio::test]
async fn carry_over_appends_after_an_existing_tomorrow_and_keeps_it() {
    let mut existing = Day::new(day2(), "UTC");
    existing.add(item(200, "X", "Y")).unwrap();
    // Today's zone differs, so a tomorrow rebuilt with it would show.
    let (mut h, store) = leftovers("Europe/Berlin").await;
    store.seed(&existing);
    h.send(carry(2));
    h.rev(3).await;
    let tomorrow = store.stored_day(day2()).unwrap();
    assert_eq!(tomorrow.tz(), "UTC");
    assert_eq!(tomorrow.item(id(200)), existing.item(id(200)));
    let x = tomorrow.item(id(200)).unwrap().order();
    let (b, c) = (
        tomorrow.item(id(1)).unwrap().order(),
        tomorrow.item(id(2)).unwrap().order(),
    );
    assert!(x < b && b < c);
}

#[tokio::test]
async fn carry_over_with_nothing_overflowing_is_rejected() {
    let (mut h, store) = running_on(1_000, Some(&abc_day())).await;
    let snap = expect_rejected(&mut h, &store, carry(1), 2).await;
    assert_eq!(
        rejected(&snap),
        Some("CarryOverToTomorrow: nothing to carry over")
    );
    assert_eq!(store.stored_day(day2()), None);
}

#[tokio::test]
async fn carry_over_with_a_stale_rev_is_rejected() {
    let (mut h, store) = leftovers("UTC").await;
    let snap = expect_rejected(&mut h, &store, carry(1), 3).await;
    assert_eq!(rejected(&snap), Some("stale rev: expected 1, current 2"));
    assert_eq!(snap.overflow, vec![id(101), id(102)]);
    assert_eq!(store.stored_day(day2()), None);
}

#[tokio::test]
async fn a_failed_tomorrow_load_applies_nothing_and_sets_store_status() {
    let (mut h, store) = leftovers("UTC").await;
    let batches = store.batches().len();
    let before = h.rx.borrow().day.clone();
    store.fail_load_day(true);
    h.send(carry(2));
    let snap = h.rev(3).await;
    assert_eq!(snap.day, before);
    assert_eq!(snap.overflow, vec![id(101), id(102)]);
    assert_eq!(store.batches().len(), batches, "no commit");
    assert!(snap.status.iter().any(|e| e.key == StatusKey::Store));
    store.fail_load_day(false);
    assert_eq!(store.stored_day(day2()), None);
}

#[tokio::test]
async fn midnight_rolls_over_to_the_day_holding_the_carried_items() {
    let (mut h, store) = leftovers("UTC").await;
    h.send(carry(2));
    h.rev(3).await;
    h.clock.advance(60);
    h.send(EngineMsg::Pause { item_id: id(100) });
    let snap = h.rev(4).await;
    assert_eq!(day_of(&snap).date(), day1(), "16:01 is still day 1");
    assert_eq!(snap.running, None);

    h.clock.set(MIDNIGHT + 30);
    let snap = h.rev(5).await;
    let today = day_of(&snap);
    assert_eq!(today.date(), day2());
    assert_eq!(today, &store.stored_day(day2()).unwrap());
    for (n, from) in [(1, 101), (2, 102)] {
        let item = today.item(id(n)).unwrap();
        assert_eq!(item.status(), ItemStatus::Pending);
        assert_eq!(item.carried_from(), Some(id(from)));
    }
    assert_eq!(
        snap.next_item.map(|n| n.item_id),
        Some(id(1)),
        "the carried items are next"
    );
    assert!(snap.overflow.is_empty());
}

#[tokio::test]
async fn an_item_running_at_midnight_keeps_its_day_until_it_stops() {
    let mut day = Day::new(day1(), "UTC");
    day.add(PlanItem::new(id(100), "A", 3_600, key("U")))
        .unwrap();
    let (mut h, store) = running_on(MIDNIGHT - 1_800, Some(&day)).await;
    h.send(start_msg(100, 1));
    h.rev(2).await;

    h.clock.set(MIDNIGHT + 60);
    h.until(|| store.last_alive() == Some(ts(MIDNIGHT + 60)))
        .await;
    let snap = h.rx.borrow().clone();
    assert_eq!(snap.rev, 2);
    assert_eq!(day_of(&snap).date(), day1());
    assert_eq!(snap.running.as_ref().map(|r| r.item_id), Some(id(100)));

    h.clock.set(MIDNIGHT + 600);
    h.send(EngineMsg::Pause { item_id: id(100) });
    // Rev 3 is the pause on day 1, rev 4 the rollover right after it.
    let snap = h.rev(4).await;
    assert_eq!(day_of(&snap), &Day::new(day2(), "UTC"));
    let old = store.stored_day(day1()).unwrap();
    let paused = old.item(id(100)).unwrap();
    assert_eq!(paused.status(), ItemStatus::Paused);
    // The segment stays whole on day 1, across midnight.
    assert_eq!(
        paused.segments(),
        &[closed(MIDNIGHT - 1_800, MIDNIGHT + 600)]
    );
}

#[tokio::test]
async fn a_command_with_a_pre_rollover_rev_is_rejected_after_rolling_over() {
    let (mut h, store) = running_on(MIDNIGHT - 60, Some(&abc_day())).await;
    let batches = store.batches().len();
    h.clock.set(MIDNIGHT + 10);
    h.send(start_msg(100, 1));
    // Rev 2 is the rollover (it may be overwritten before it is seen), so the
    // stale rev is 1 against 2.
    let snap = h.rev(3).await;
    assert_eq!(rejected(&snap), Some("stale rev: expected 1, current 2"));
    assert_eq!(snap.running, None);
    assert_eq!(day_of(&snap).date(), day2());
    assert!(
        store.batches()[batches..].iter().all(|b| b
            .ops()
            .iter()
            .all(|op| matches!(op, StoreOp::SetLastAlive { .. }))),
        "only heartbeats were committed"
    );
}

#[tokio::test]
async fn carried_items_load_with_their_link_after_a_restart() {
    let (mut h, store) = leftovers("UTC").await;
    h.send(carry(2));
    h.rev(3).await;
    drop(h);
    let mut h = start(&FakeClock::at(MIDNIGHT + 100), &store);
    let snap = h.rev(1).await;
    let today = day_of(&snap);
    assert_eq!(today.date(), day2());
    assert_eq!(today.item(id(1)).unwrap().carried_from(), Some(id(101)));
    assert_eq!(today.item(id(2)).unwrap().carried_from(), Some(id(102)));
}
