//! One test per row of the engine's I/O matrix, the `diff` unit tests, and the
//! exactly-once effect check. All run against `MemoryStore` in UTC.

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
    DEMO_TITLE, Effect, EngineClosed, EngineDeps, EngineHandle, EngineMsg, Snapshot, StatusEntry,
    StatusKey, spawn,
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

fn demo(snap: &Snapshot) -> &PlanItem {
    let day = snap.day.as_ref().unwrap();
    day.items()
        .iter()
        .find(|i| i.title() == DEMO_TITLE)
        .expect("demo item")
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

#[tokio::test]
async fn start_demo_creates_and_starts_the_demo_and_persists_it() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    h.rev(1).await;
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(2).await;
    let demo = demo(&snap);
    assert_eq!(demo.id(), id(1));
    assert_eq!(demo.status(), ItemStatus::Running);
    assert_eq!(demo.segments(), &[Segment::open(ts(1_000))]);
    let running = snap.running.clone().unwrap();
    assert_eq!(running.title, DEMO_TITLE);
    assert_eq!(running.segment_started_at, ts(1_000));
    assert_eq!(running.accumulated_secs, 0);
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn start_demo_again_restarts_the_segment() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    h.rev(1).await;
    h.send(EngineMsg::StartDemo);
    h.rev(2).await;
    h.clock.set(1_042);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(3).await;
    assert_eq!(
        demo(&snap).segments(),
        &[closed(1_000, 1_042), Segment::open(ts(1_042))]
    );
    let running = snap.running.clone().unwrap();
    assert_eq!(running.segment_started_at, ts(1_042));
    assert_eq!(running.accumulated_secs, 42);
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn start_demo_at_the_same_instant_is_a_no_op() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    h.rev(1).await;
    h.send(EngineMsg::StartDemo);
    h.send(EngineMsg::StartDemo);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(2).await;
    // A later message would bump rev past 2 if the repeats had published.
    h.clock.set(1_010);
    h.send(EngineMsg::StartDemo);
    let after = h.rev(3).await;
    assert_eq!(demo(&snap).segments(), &[Segment::open(ts(1_000))]);
    assert_eq!(demo(&after).segments().len(), 2);
    assert!(after.status.is_empty());
}

/// A day whose item 100 has been running since 1_000.
fn day_running_other() -> Day {
    let mut day = Day::new(day1(), "UTC");
    day.add(item(100, "other", "V")).unwrap();
    day.start(id(100), ts(1_000)).unwrap();
    day
}

#[tokio::test]
async fn start_demo_pauses_another_running_item_first() {
    // The only way another item runs in this story: its recovery commit failed, so
    // the engine kept the loaded state with it running.
    let (clock, store) = (FakeClock::at(2_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.fail_commits(true);
    let mut h = start(&clock, &store);
    let snap = h.rev(1).await;
    assert_eq!(snap.running.unwrap().title, "other");
    store.fail_commits(false);

    // Before the 2_060 heartbeat, whose success would clear the status and publish.
    h.clock.set(2_050);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(2).await;
    let day = snap.day.as_ref().unwrap();
    let other = day.item(id(100)).unwrap();
    assert_eq!(other.status(), ItemStatus::Paused);
    assert_eq!(other.segments(), &[closed(1_000, 2_050)]);
    assert_eq!(demo(&snap).segments(), &[Segment::open(ts(2_050))]);
    assert_eq!(snap.running.as_ref().unwrap().title, DEMO_TITLE);
    assert!(snap.status.is_empty());
    assert_eq!(store.stored_day(day1()), snap.day);
}

#[tokio::test]
async fn a_failing_commit_keeps_the_state_and_sets_store_status_until_a_success() {
    let (clock, store) = (FakeClock::at(1_000), MemoryStore::new());
    let mut h = start(&clock, &store);
    let before = h.rev(1).await;
    store.fail_commits(true);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(2).await;
    assert_eq!(snap.day, before.day);
    assert_eq!(snap.running, None);
    assert_eq!(snap.status, store_status());
    assert_eq!(store.stored_day(day1()), None);

    store.fail_commits(false);
    h.clock.set(1_005);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(3).await;
    assert!(snap.status.is_empty());
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
    assert_eq!(h.engine.send(EngineMsg::StartDemo), Err(EngineClosed));
}

// --- Startup ordering and effects ---

#[tokio::test]
async fn messages_sent_before_startup_queue_and_run_after_it_in_order() {
    let (clock, store) = (FakeClock::at(5_000), MemoryStore::new());
    store.seed(&day_running_other());
    store.seed_last_alive(Some(ts(1_030)));
    // Sent before the engine task first runs, so before recovery.
    let mut h = start(&clock, &store);
    h.send(EngineMsg::StartDemo);
    let snap = h.rev(2).await;
    let day = snap.day.as_ref().unwrap();
    // Recovery ran first: the other item closed at the heartbeat, not at 5_000.
    assert_eq!(
        day.item(id(100)).unwrap().segments(),
        &[closed(1_000, 1_030)]
    );
    assert_eq!(demo(&snap).segments(), &[Segment::open(ts(5_000))]);
    let batches = store.batches();
    assert_eq!(batches.len(), 2);
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
    h.send(EngineMsg::StartDemo);
    h.rev(2).await;
    h.clock.advance(60);
    h.until(|| store.last_alive() == Some(ts(5_060))).await;
    h.clock.advance(1);
    h.send(EngineMsg::StartDemo);
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
