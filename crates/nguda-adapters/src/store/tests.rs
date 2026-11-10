//! One test per row of the store's I/O matrix, plus WAL and worker checks.

use std::path::{Path, PathBuf};

use nguda_core::{Day, ItemId, OrderKey, PlanItem, Store, StoreBatch, StoreOp, Timestamp, Uuid};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use tempfile::TempDir;

use super::schema::V1_SQL;
use super::{OpenError, SqliteStore, backup_path};

const TITLE: &str = "private title";

fn date() -> nguda_core::Date {
    nguda_core::Date::constant(2026, 11, 11)
}

fn other_date() -> nguda_core::Date {
    nguda_core::Date::constant(2026, 11, 12)
}

fn id(n: u128) -> ItemId {
    ItemId(Uuid::from_u128(n))
}

/// Sub-second instants, to prove nanosecond round trips.
fn ts(secs: i64) -> Timestamp {
    Timestamp::from_nanosecond(i128::from(secs) * 1_000_000_000 + 123_456_789).unwrap()
}

fn item(n: u128, order: &str) -> PlanItem {
    PlanItem::new(
        id(n),
        format!("{TITLE} {n}"),
        1500,
        OrderKey::new(order).unwrap(),
    )
}

fn db(dir: &TempDir) -> PathBuf {
    dir.path().join("nguda.db")
}

fn v1() -> Migrations<'static> {
    Migrations::new(vec![M::up(V1_SQL)])
}

fn v1_v2() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(V1_SQL),
        M::up("CREATE TABLE v2_marker (x INTEGER) STRICT;"),
    ])
}

fn user_version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

fn has_table(path: &Path, table: &str) -> bool {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        == 1
}

fn backups(dir: &TempDir) -> Vec<PathBuf> {
    std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "bak"))
        .collect()
}

/// A day with a running flexible item (one closed and one open segment), a pending
/// fixed item, a removed item, and a done item.
fn sample_day() -> Day {
    let mut d = Day::new(date(), "Europe/Berlin");
    for (n, order) in [(1, "V"), (2, "W"), (3, "X"), (4, "Y")] {
        d.add(item(n, order)).unwrap();
    }
    d.start(id(4), ts(100)).unwrap();
    d.finish(id(4), ts(200)).unwrap();
    d.start(id(1), ts(300)).unwrap();
    d.pause(id(1), ts(400)).unwrap();
    d.resume(id(1), ts(500)).unwrap();
    d.set_fixed(id(2), ts(9_000)).unwrap();
    d.set_planned_secs(id(2), 3_600).unwrap();
    d.remove(id(3), ts(600)).unwrap();
    d
}

/// The batch that stores `day`: the day, every item, then closed segments before the
/// open one so the single-open rule holds throughout.
fn batch_for(day: &Day) -> StoreBatch {
    let mut b = StoreBatch::new();
    b.push(StoreOp::PutDay {
        date: day.date(),
        tz: day.tz().into(),
    });
    for i in day.items() {
        b.push(StoreOp::PutItem {
            date: day.date(),
            item: i.clone(),
        });
    }
    let mut open = Vec::new();
    for i in day.items() {
        for s in i.segments() {
            let op = StoreOp::OpenSegment {
                item_id: i.id(),
                started_at: s.started_at,
            };
            match s.ended_at {
                Some(ended_at) => {
                    b.push(op);
                    b.push(StoreOp::CloseSegment {
                        item_id: i.id(),
                        started_at: s.started_at,
                        ended_at,
                    });
                }
                None => open.push(op),
            }
        }
    }
    for op in open {
        b.push(op);
    }
    b
}

fn batch(ops: impl IntoIterator<Item = StoreOp>) -> StoreBatch {
    let mut b = StoreBatch::new();
    for op in ops {
        b.push(op);
    }
    b
}

/// A store at `path` holding `sample_day`.
async fn seeded(path: &Path) -> SqliteStore {
    let store = SqliteStore::open(path).unwrap();
    store.commit(batch_for(&sample_day())).await.unwrap();
    store
}

#[tokio::test]
async fn fresh_open_creates_the_latest_schema_without_a_backup() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    assert!(!path.exists());
    drop(SqliteStore::open(&path).unwrap());
    assert!(path.exists());
    assert_eq!(user_version(&path), 1);
    assert!(has_table(&path, "time_segment"));
    assert!(backups(&dir).is_empty());
}

#[tokio::test]
async fn journal_mode_is_wal() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    // WAL is persistent, so a second handle sees the mode the store set.
    let mode: String = Connection::open(&path)
        .unwrap()
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(store);
}

#[tokio::test]
async fn upgrade_writes_a_backup_of_the_old_schema_then_migrates() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open_with(&path, &v1()).unwrap();
    store.commit(batch_for(&sample_day())).await.unwrap();
    drop(store);
    // A stale file at the backup path is replaced.
    let bak = backup_path(&path, 1);
    assert_eq!(bak, dir.path().join("nguda.db.v1.bak"));
    std::fs::write(&bak, b"stale").unwrap();

    let store = SqliteStore::open_with(&path, &v1_v2()).unwrap();

    // The backup holds the v1 data and predates the v2 migration.
    assert_eq!(user_version(&bak), 1);
    assert!(!has_table(&bak, "v2_marker"));
    let backed_up = SqliteStore::open_with(&bak, &v1()).unwrap();
    assert_eq!(
        backed_up.load_day(date()).await.unwrap(),
        Some(sample_day())
    );
    drop(backed_up);
    // The database is at v2 with its data intact.
    assert_eq!(store.load_day(date()).await.unwrap(), Some(sample_day()));
    drop(store);
    assert_eq!(user_version(&path), 2);
    assert!(has_table(&path, "v2_marker"));
    assert_eq!(backups(&dir), vec![bak]);
}

#[tokio::test]
async fn up_to_date_open_writes_no_backup() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day(date()).await.unwrap(), Some(sample_day()));
    drop(store);
    assert!(backups(&dir).is_empty());
}

#[tokio::test]
async fn newer_schema_is_refused_without_touching_the_file() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    // Bump the version in WAL frames only, then snapshot the files while that
    // connection is still open, so the copy holds uncheckpointed frames and no
    // connection: a read-write open closing it would checkpoint them.
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    raw.pragma_update(None, "user_version", 5).unwrap();
    let copy = dir.path().join("newer.db");
    let wal = |p: &Path| PathBuf::from(format!("{}-wal", p.display()));
    std::fs::copy(&path, &copy).unwrap();
    std::fs::copy(wal(&path), wal(&copy)).unwrap();
    drop(raw);
    let path = copy;
    let before = std::fs::read(&path).unwrap();
    let wal_before = std::fs::read(wal(&path)).unwrap();
    assert!(!wal_before.is_empty(), "the test needs real WAL frames");

    let err = SqliteStore::open(&path).err().unwrap();
    assert_eq!(
        err,
        OpenError::NewerSchema {
            found: 5,
            supported: 1
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read(wal(&path)).unwrap(), wal_before);
    assert!(backups(&dir).is_empty());
    assert_eq!(user_version(&path), 5);
}

#[tokio::test]
async fn a_committed_day_round_trips_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(loaded, sample_day());
    // Spot-check the variety the sample covers.
    assert!(loaded.item(id(2)).unwrap().fixed());
    assert_eq!(loaded.item(id(3)).unwrap().removed_at(), Some(ts(600)));
    assert_eq!(loaded.item(id(1)).unwrap().segments().len(), 2);
}

#[tokio::test]
async fn a_failing_batch_persists_nothing() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    // Item 1 already has an open segment, so a second one must fail.
    let err = store
        .commit(batch([
            StoreOp::PutItem {
                date: date(),
                item: item(9, "Z"),
            },
            StoreOp::OpenSegment {
                item_id: id(9),
                started_at: ts(700),
            },
        ]))
        .await
        .unwrap_err();
    assert!(err.message.contains(&id(9).to_string()), "{err}");
    assert!(!err.message.contains(TITLE), "{err}");
    let loaded = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(loaded, sample_day());
    assert!(loaded.item(id(9)).is_none());
}

#[tokio::test]
async fn closing_a_closed_segment_rolls_back_the_batch() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let mut renamed = sample_day();
    renamed.set_title(id(4), "renamed").unwrap();
    let err = store
        .commit(batch([
            StoreOp::PutItem {
                date: date(),
                item: renamed.item(id(4)).unwrap().clone(),
            },
            StoreOp::CloseSegment {
                item_id: id(4),
                started_at: ts(100),
                ended_at: ts(250),
            },
        ]))
        .await
        .unwrap_err();
    assert!(err.message.contains(&id(4).to_string()), "{err}");
    assert_eq!(store.load_day(date()).await.unwrap(), Some(sample_day()));
}

#[tokio::test]
async fn changing_a_days_time_zone_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    // Re-putting the same zone is fine.
    store
        .commit(batch([StoreOp::PutDay {
            date: date(),
            tz: "Europe/Berlin".into(),
        }]))
        .await
        .unwrap();
    store
        .commit(batch([
            StoreOp::PutItem {
                date: date(),
                item: item(9, "Z"),
            },
            StoreOp::PutDay {
                date: date(),
                tz: "Asia/Makassar".into(),
            },
        ]))
        .await
        .unwrap_err();
    assert_eq!(store.load_day(date()).await.unwrap(), Some(sample_day()));
}

#[tokio::test]
async fn moving_an_item_to_another_day_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let err = store
        .commit(batch([
            StoreOp::PutDay {
                date: other_date(),
                tz: "Europe/Berlin".into(),
            },
            StoreOp::PutItem {
                date: other_date(),
                item: sample_day().item(id(2)).unwrap().clone(),
            },
        ]))
        .await
        .unwrap_err();
    assert!(err.message.contains(&id(2).to_string()), "{err}");
    assert_eq!(store.load_day(date()).await.unwrap(), Some(sample_day()));
    assert_eq!(store.load_day(other_date()).await.unwrap(), None);
}

#[tokio::test]
async fn an_unknown_day_loads_as_none() {
    let dir = TempDir::new().unwrap();
    let store = seeded(&db(&dir)).await;
    assert_eq!(store.load_day(other_date()).await.unwrap(), None);
}

#[tokio::test]
async fn an_inconsistent_stored_row_fails_the_load_naming_the_item() {
    // Running without an open segment: the schema allows it, restore does not.
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "UPDATE plan_item SET status = 'running' WHERE id = ?1",
        [id(4).to_string()],
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_day(date()).await.unwrap_err();
    assert!(err.message.contains(&id(4).to_string()), "{err}");
    assert!(!err.message.contains(TITLE), "{err}");
    drop(store);

    // Fixed with no pin, sneaked past the CHECK constraint.
    let raw = Connection::open(&path).unwrap();
    raw.pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    raw.execute(
        "UPDATE plan_item SET pinned_start = NULL WHERE id = ?1",
        [id(2).to_string()],
    )
    .unwrap();
    raw.execute(
        "UPDATE plan_item SET status = 'done' WHERE id = ?1",
        [id(4).to_string()],
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_day(date()).await.unwrap_err();
    assert!(err.message.contains(&id(2).to_string()), "{err}");
    assert!(err.message.contains("pinned"), "{err}");
}

#[tokio::test]
async fn a_stopped_worker_yields_a_store_error() {
    let (requests, rx) = std::sync::mpsc::channel();
    drop(rx);
    let store = SqliteStore {
        requests: Some(requests),
        worker: None,
    };
    assert!(store.commit(StoreBatch::new()).await.is_err());
    assert!(store.load_day(date()).await.is_err());
}
