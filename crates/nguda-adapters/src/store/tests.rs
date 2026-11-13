//! One test per row of the store's I/O matrix, plus WAL and worker checks.

use std::path::{Path, PathBuf};

use nguda_core::{
    Baseline, BaselineItem, Day, DayLogEntry, DayLogKind, ItemId, LockDecision, OpenSegmentRef,
    OrderKey, PlanItem, Settings, Span, Store, StoreBatch, StoreOp, Timestamp, Uuid, WorkEnd,
};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};
use tempfile::TempDir;

use super::schema::{V1_SQL, migrations_through};
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

/// Every stored plan item row, every column, sorted by id.
fn plan_item_rows(path: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn.prepare("SELECT * FROM plan_item ORDER BY id").unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |r| (0..columns).map(|c| r.get(c)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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
    b.extend(open);
    b
}

fn batch(ops: impl IntoIterator<Item = StoreOp>) -> StoreBatch {
    ops.into_iter().collect()
}

/// A store at `path` holding `sample_day`.
async fn seeded(path: &Path) -> SqliteStore {
    let store = SqliteStore::open(path).unwrap();
    store.commit(batch_for(&sample_day())).await.unwrap();
    store
}

/// Writes `sample_day` into a v1 database at `path`. The adapter writes columns v1
/// lacks, so the rows go through a current-schema scratch database and are copied
/// over with v1's columns only.
async fn seed_v1(dir: &TempDir, path: &Path) {
    drop(SqliteStore::open_with(path, &v1()).unwrap());
    let scratch = dir.path().join("scratch.sqlite");
    drop(seeded(&scratch).await);
    let conn = Connection::open(path).unwrap();
    conn.execute("ATTACH DATABASE ?1 AS src", [scratch.to_str().unwrap()])
        .unwrap();
    conn.execute_batch(
        r#"INSERT INTO day SELECT date, tz FROM src.day ORDER BY rowid;
           INSERT INTO plan_item
             SELECT id, day_date, title, planned_secs, "order", fixed, pinned_start,
                    status, removed_at
             FROM src.plan_item ORDER BY rowid;
           INSERT INTO time_segment SELECT id, item_id, started_at, ended_at
             FROM src.time_segment ORDER BY id;
           DETACH DATABASE src;"#,
    )
    .unwrap();
}

#[tokio::test]
async fn fresh_open_creates_the_latest_schema_without_a_backup() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    assert!(!path.exists());
    drop(SqliteStore::open(&path).unwrap());
    assert!(path.exists());
    assert_eq!(user_version(&path), 8);
    assert!(has_table(&path, "time_segment"));
    assert!(has_table(&path, "engine_meta"));
    assert!(has_table(&path, "day_log"));
    assert!(has_table(&path, "interrupt_entry"));
    assert!(has_table(&path, "settings"));
    assert!(has_table(&path, "baseline"));
    assert!(has_table(&path, "baseline_item"));
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
    seed_v1(&dir, &path).await;
    // A stale file at the backup path is replaced.
    let bak = backup_path(&path, 1);
    assert_eq!(bak, dir.path().join("nguda.db.v1.bak"));
    std::fs::write(&bak, b"stale").unwrap();

    let store = SqliteStore::open_with(&path, &v1_v2()).unwrap();

    // The backup holds the v1 data and predates the v2 migration.
    assert_eq!(user_version(&bak), 1);
    assert!(!has_table(&bak, "v2_marker"));
    // Both hold every item. (A whole-day load needs the current schema, so the
    // rows are compared directly, column by column.)
    let backed_up = plan_item_rows(&bak);
    // Checked against the seed too, not only against the migrated rows.
    let mut seeded: Vec<String> = sample_day()
        .items()
        .iter()
        .map(|i| i.id().to_string())
        .collect();
    seeded.sort();
    let backed_up_ids: Vec<String> = backed_up
        .iter()
        .map(|row| match &row[0] {
            rusqlite::types::Value::Text(id) => id.clone(),
            other => panic!("plan_item.id is not text: {other:?}"),
        })
        .collect();
    assert_eq!(backed_up_ids, seeded);
    // The database is at v2 with its data intact.
    assert_eq!(plan_item_rows(&path), backed_up);
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
    raw.pragma_update(None, "user_version", 9).unwrap();
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
            found: 9,
            supported: 8
        }
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read(wal(&path)).unwrap(), wal_before);
    assert!(backups(&dir).is_empty());
    assert_eq!(user_version(&path), 9);
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
    assert!(store.last_alive_at().await.is_err());
    assert!(store.open_segment().await.is_err());
    assert!(store.load_day_log(date()).await.is_err());
}

#[tokio::test]
async fn a_real_v1_database_upgrades_to_v2_with_a_backup_and_its_data() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    seed_v1(&dir, &path).await;
    assert_eq!(user_version(&path), 1);

    let store = SqliteStore::open(&path).unwrap();
    // The sample day has time on it, so v8 decides it `no_lock`.
    let mut expected = sample_day();
    expected.restore_lock_decision(LockDecision::NoLock);
    assert_eq!(store.load_day(date()).await.unwrap(), Some(expected));
    assert_eq!(store.last_alive_at().await.unwrap(), None);
    assert_eq!(store.load_day_log(date()).await.unwrap(), vec![]);
    drop(store);

    let bak = dir.path().join("nguda.db.v1.bak");
    assert_eq!(backups(&dir), vec![bak.clone()]);
    assert_eq!(user_version(&bak), 1);
    assert!(!has_table(&bak, "day_log"));
    assert_eq!(user_version(&path), 8);
    assert!(has_table(&path, "engine_meta"));
}

#[tokio::test]
async fn settings_default_when_no_row_is_stored() {
    let dir = TempDir::new().unwrap();
    let store = SqliteStore::open(db(&dir)).unwrap();
    assert_eq!(store.load_settings().await.unwrap(), Settings::default());
    assert_eq!(
        store.load_settings().await.unwrap().work_end,
        WorkEnd::from_minutes(17 * 60).unwrap()
    );
}

#[tokio::test]
async fn work_end_round_trips_across_a_reopen_and_is_overwritten() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    let late = WorkEnd::from_minutes(18 * 60 + 30).unwrap();
    store
        .commit(StoreBatch::from_iter([StoreOp::SetWorkEnd {
            work_end: late,
        }]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_settings().await.unwrap().work_end, late);
    let early = WorkEnd::from_minutes(9 * 60).unwrap();
    store
        .commit(StoreBatch::from_iter([StoreOp::SetWorkEnd {
            work_end: early,
        }]))
        .await
        .unwrap();
    assert_eq!(store.load_settings().await.unwrap().work_end, early);
    drop(store);
    let rows: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT count(*) FROM settings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn an_unparseable_stored_work_end_is_a_store_error() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(SqliteStore::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO settings (key, value) VALUES ('work_end', '25:00')",
            [],
        )
        .unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_settings().await.unwrap_err();
    assert_eq!(err.message, "load settings: invalid stored work_end");
}

#[tokio::test]
async fn last_alive_round_trips_and_is_overwritten() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.last_alive_at().await.unwrap(), None);
    store
        .commit(batch([StoreOp::SetLastAlive { at: ts(1_000) }]))
        .await
        .unwrap();
    assert_eq!(store.last_alive_at().await.unwrap(), Some(ts(1_000)));
    store
        .commit(batch([StoreOp::SetLastAlive { at: ts(1_060) }]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.last_alive_at().await.unwrap(), Some(ts(1_060)));
}

fn recovered(n: u128, at: i64) -> DayLogEntry {
    DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Recovered { item_id: id(n) },
    }
}

#[tokio::test]
async fn day_log_appends_load_in_order_per_day() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    // Appended out of time order: load order is append order.
    store
        .commit(batch([
            StoreOp::AppendDayLog {
                date: date(),
                entry: recovered(2, 900),
            },
            StoreOp::AppendDayLog {
                date: date(),
                entry: recovered(1, 800),
            },
        ]))
        .await
        .unwrap();
    store
        .commit(batch([StoreOp::AppendDayLog {
            date: date(),
            entry: recovered(4, 950),
        }]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store.load_day_log(date()).await.unwrap(),
        vec![recovered(2, 900), recovered(1, 800), recovered(4, 950)]
    );
    assert_eq!(store.load_day_log(other_date()).await.unwrap(), vec![]);
}

#[tokio::test]
async fn a_removed_day_log_entry_round_trips_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let removed = DayLogEntry {
        at: ts(1_200),
        kind: DayLogKind::Removed { item_id: id(3) },
    };
    store
        .commit(batch([
            StoreOp::AppendDayLog {
                date: date(),
                entry: recovered(1, 1_100),
            },
            StoreOp::AppendDayLog {
                date: date(),
                entry: removed.clone(),
            },
        ]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store.load_day_log(date()).await.unwrap(),
        vec![recovered(1, 1_100), removed]
    );
}

#[tokio::test]
async fn switched_day_log_entries_round_trip_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let switched = |at, from, to: Option<u128>| DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Switched {
            from: id(from),
            to: to.map(id),
        },
    };
    let entries = [switched(1_300, 4, Some(1)), switched(1_400, 1, None)];
    store
        .commit(batch(entries.iter().map(|entry| StoreOp::AppendDayLog {
            date: date(),
            entry: entry.clone(),
        })))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day_log(date()).await.unwrap(), entries);
}

#[tokio::test]
async fn extended_day_log_entries_round_trip_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let extended = |at, n, secs| DayLogEntry {
        at: ts(at),
        kind: DayLogKind::Extended {
            item_id: id(n),
            secs,
        },
    };
    let entries = [extended(1_300, 1, 300), extended(1_400, 1, 900)];
    store
        .commit(batch(entries.iter().map(|entry| StoreOp::AppendDayLog {
            date: date(),
            entry: entry.clone(),
        })))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day_log(date()).await.unwrap(), entries);
}

#[tokio::test]
async fn an_extended_entry_without_secs_fails_the_load() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO day_log (day_date, at, kind, item_id) VALUES (?1, 0, 'extended', ?2)",
        [date().to_string(), id(1).to_string()],
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_day_log(date()).await.unwrap_err();
    assert!(err.message.contains("missing or invalid secs"), "{err}");
}

#[tokio::test]
async fn an_extended_entry_past_i64_rolls_back_the_batch() {
    let dir = TempDir::new().unwrap();
    let store = seeded(&db(&dir)).await;
    store
        .commit(batch([StoreOp::AppendDayLog {
            date: date(),
            entry: DayLogEntry {
                at: ts(1_300),
                kind: DayLogKind::Extended {
                    item_id: id(1),
                    secs: u64::MAX,
                },
            },
        }]))
        .await
        .unwrap_err();
    assert_eq!(store.load_day_log(date()).await.unwrap(), vec![]);
}

#[tokio::test]
async fn a_switched_entry_to_an_unknown_item_rolls_back_the_batch() {
    let dir = TempDir::new().unwrap();
    let store = seeded(&db(&dir)).await;
    store
        .commit(batch([StoreOp::AppendDayLog {
            date: date(),
            entry: DayLogEntry {
                at: ts(1_300),
                kind: DayLogKind::Switched {
                    from: id(1),
                    to: Some(id(99)),
                },
            },
        }]))
        .await
        .unwrap_err();
    assert_eq!(store.load_day_log(date()).await.unwrap(), vec![]);
}

#[tokio::test]
async fn day_log_on_an_unknown_day_rolls_back_the_batch() {
    let dir = TempDir::new().unwrap();
    let store = seeded(&db(&dir)).await;
    store
        .commit(batch([
            StoreOp::SetLastAlive { at: ts(1_000) },
            StoreOp::AppendDayLog {
                date: other_date(),
                entry: recovered(1, 900),
            },
        ]))
        .await
        .unwrap_err();
    assert_eq!(store.last_alive_at().await.unwrap(), None);
    assert_eq!(store.load_day_log(other_date()).await.unwrap(), vec![]);
}

#[tokio::test]
async fn an_unknown_stored_day_log_kind_fails_the_load_without_its_text() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO day_log (day_date, at, kind, item_id) VALUES (?1, 0, 'from-the-future', NULL)",
        [date().to_string()],
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_day_log(date()).await.unwrap_err();
    assert!(err.message.contains("unknown kind"), "{err}");
    assert!(!err.message.contains("from-the-future"), "{err}");
}

#[tokio::test]
async fn open_segment_is_none_open_or_none_after_close() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.open_segment().await.unwrap(), None);

    // sample_day leaves item 1 running since 500.
    store.commit(batch_for(&sample_day())).await.unwrap();
    assert_eq!(
        store.open_segment().await.unwrap(),
        Some(OpenSegmentRef {
            date: date(),
            item_id: id(1),
            started_at: ts(500),
        })
    );

    store
        .commit(batch([StoreOp::CloseSegment {
            item_id: id(1),
            started_at: ts(500),
            ended_at: ts(560),
        }]))
        .await
        .unwrap();
    assert_eq!(store.open_segment().await.unwrap(), None);
}

/// Item 10 interrupted by 11, which was interrupted by 12 (running): stack [10, 11].
fn stacked_day() -> Day {
    let mut d = Day::new(date(), "UTC");
    d.add(item(10, "V")).unwrap();
    d.start(id(10), ts(100)).unwrap();
    d.interrupt(id(10), item(11, "W"), ts(200)).unwrap();
    d.interrupt(id(11), item(12, "X"), ts(300)).unwrap();
    assert_eq!(d.interrupt_stack(), &[id(10), id(11)]);
    d
}

fn set_stack(items: Vec<ItemId>) -> StoreOp {
    StoreOp::SetInterruptStack {
        date: date(),
        items,
    }
}

#[tokio::test]
async fn the_interrupt_stack_round_trips_and_is_replaced() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let day = stacked_day();
    let store = SqliteStore::open(&path).unwrap();
    let mut b = batch_for(&day);
    b.push(set_stack(vec![id(10), id(11)]));
    store.commit(b).await.unwrap();
    drop(store);

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day(date()).await.unwrap(), Some(day));
    // Replacing keeps only the new entries, in order.
    store
        .commit(batch([set_stack(vec![id(11)])]))
        .await
        .unwrap();
    let loaded = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(loaded.interrupt_stack(), &[id(11)]);
    store.commit(batch([set_stack(vec![])])).await.unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.load_day(date()).await.unwrap().unwrap();
    assert!(loaded.interrupt_stack().is_empty());
}

#[tokio::test]
async fn a_stored_non_paused_stack_entry_fails_the_load() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    store.commit(batch_for(&stacked_day())).await.unwrap();
    // The running item 12 is not paused.
    store
        .commit(batch([set_stack(vec![id(10), id(12)])]))
        .await
        .unwrap();
    let err = store.load_day(date()).await.unwrap_err();
    assert!(err.message.contains(&id(12).to_string()), "{err}");
    assert!(!err.message.contains(TITLE), "{err}");
}

/// A duplicate or unknown entry never reaches a load: the schema refuses it at
/// commit (`UNIQUE (item_id)`, the foreign key) and the batch rolls back.
#[tokio::test]
async fn a_duplicate_or_unknown_stack_entry_is_refused_at_commit() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = SqliteStore::open(&path).unwrap();
    let day = stacked_day();
    let mut b = batch_for(&day);
    b.push(set_stack(vec![id(10), id(11)]));
    store.commit(b).await.unwrap();
    for bad in [vec![id(10), id(10)], vec![id(99)]] {
        store
            .commit(batch([set_stack(bad.clone())]))
            .await
            .unwrap_err();
        assert_eq!(
            store.load_day(date()).await.unwrap(),
            Some(day.clone()),
            "unchanged after {bad:?}"
        );
    }
}

#[tokio::test]
async fn inserted_day_log_entries_round_trip_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let entry = DayLogEntry {
        at: ts(1_500),
        kind: DayLogKind::Inserted { item_id: id(2) },
    };
    store
        .commit(batch([StoreOp::AppendDayLog {
            date: date(),
            entry: entry.clone(),
        }]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day_log(date()).await.unwrap(), vec![entry]);
}

#[tokio::test]
async fn a_carried_item_round_trips_its_link_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let mut tomorrow = Day::new(other_date(), "Europe/Berlin");
    tomorrow
        .add(PlanItem::carried(
            id(20),
            format!("{TITLE} 1"),
            900,
            OrderKey::new("V").unwrap(),
            id(1),
        ))
        .unwrap();
    store.commit(batch_for(&tomorrow)).await.unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.load_day(other_date()).await.unwrap().unwrap();
    assert_eq!(loaded, tomorrow);
    assert_eq!(loaded.item(id(20)).unwrap().carried_from(), Some(id(1)));
    // Items that were not carried keep no link.
    let today = store.load_day(date()).await.unwrap().unwrap();
    assert!(today.items().iter().all(|i| i.carried_from().is_none()));
}

#[tokio::test]
async fn carried_over_day_log_entries_round_trip_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let entry = DayLogEntry {
        at: ts(1_600),
        kind: DayLogKind::CarriedOver {
            item_id: id(2),
            to: id(4),
        },
    };
    store
        .commit(batch([StoreOp::AppendDayLog {
            date: date(),
            entry: entry.clone(),
        }]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_day_log(date()).await.unwrap(), vec![entry]);
}

#[tokio::test]
async fn a_carried_over_entry_without_a_target_fails_the_load() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    drop(seeded(&path).await);
    let raw = Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO day_log (day_date, at, kind, item_id) VALUES (?1, 0, 'carried_over', ?2)",
        [date().to_string(), id(1).to_string()],
    )
    .unwrap();
    drop(raw);
    let store = SqliteStore::open(&path).unwrap();
    let err = store.load_day_log(date()).await.unwrap_err();
    assert!(err.message.contains("missing to_item_id"), "{err}");
}

// --- Day lock and baseline ---

fn set_decision(decision: LockDecision) -> StoreOp {
    StoreOp::SetLockDecision {
        date: date(),
        decision,
    }
}

/// A baseline of `sample_day` at `locked_at`: the running item with a span, then
/// the done one without.
fn sample_baseline(locked_at: i64) -> Baseline {
    Baseline {
        date: date(),
        locked_at: ts(locked_at),
        items: vec![
            BaselineItem {
                item_id: id(1),
                planned_secs: 1500,
                span: Some(Span {
                    start: ts(500),
                    end: ts(1_900),
                }),
            },
            BaselineItem {
                item_id: id(4),
                planned_secs: 1500,
                span: None,
            },
        ],
    }
}

#[tokio::test]
async fn the_lock_decision_and_baseline_round_trip_across_a_reopen() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    assert_eq!(store.load_baseline(date()).await.unwrap(), None);
    store
        .commit(batch([
            set_decision(LockDecision::Locked),
            StoreOp::PutBaseline {
                baseline: sample_baseline(700),
            },
        ]))
        .await
        .unwrap();
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    let day = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(day.lock_decision(), LockDecision::Locked);
    assert_eq!(
        store.load_baseline(date()).await.unwrap(),
        Some(sample_baseline(700))
    );
    assert_eq!(store.load_baseline(other_date()).await.unwrap(), None);
    // An undecided day loads `None`, and NoLock round-trips too.
    store
        .commit(batch([StoreOp::PutDay {
            date: other_date(),
            tz: "UTC".into(),
        }]))
        .await
        .unwrap();
    let other = store.load_day(other_date()).await.unwrap().unwrap();
    assert_eq!(other.lock_decision(), LockDecision::None);
    store
        .commit(batch([StoreOp::SetLockDecision {
            date: other_date(),
            decision: LockDecision::NoLock,
        }]))
        .await
        .unwrap();
    let other = store.load_day(other_date()).await.unwrap().unwrap();
    assert_eq!(other.lock_decision(), LockDecision::NoLock);
}

#[tokio::test]
async fn a_second_baseline_for_a_date_rolls_back_the_whole_batch() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    store
        .commit(batch([StoreOp::PutBaseline {
            baseline: sample_baseline(700),
        }]))
        .await
        .unwrap();
    let err = store
        .commit(batch([
            set_decision(LockDecision::Locked),
            StoreOp::PutBaseline {
                baseline: sample_baseline(900),
            },
        ]))
        .await
        .unwrap_err();
    assert!(err.message.contains("put baseline"), "{err}");
    assert!(!err.message.contains(TITLE));
    assert_eq!(
        store.load_baseline(date()).await.unwrap(),
        Some(sample_baseline(700))
    );
    let day = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(day.lock_decision(), LockDecision::None, "rolled back");
}

#[tokio::test]
async fn a_baseline_of_an_unknown_day_or_item_rolls_back() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    let mut unknown_item = sample_baseline(700);
    unknown_item.items[1].item_id = id(99);
    let mut unknown_day = sample_baseline(700);
    unknown_day.date = other_date();
    for baseline in [unknown_item, unknown_day] {
        assert!(
            store
                .commit(batch([StoreOp::PutBaseline { baseline }]))
                .await
                .is_err()
        );
    }
    assert_eq!(store.load_baseline(date()).await.unwrap(), None);
    // A decision for an unknown day is refused too.
    let err = store
        .commit(batch([StoreOp::SetLockDecision {
            date: other_date(),
            decision: LockDecision::Locked,
        }]))
        .await
        .unwrap_err();
    assert!(err.message.contains("no such day"), "{err}");
}

#[tokio::test]
async fn the_stored_baseline_refuses_updates_and_deletes() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    store
        .commit(batch([StoreOp::PutBaseline {
            baseline: sample_baseline(700),
        }]))
        .await
        .unwrap();
    drop(store);
    let conn = Connection::open(&path).unwrap();
    for sql in [
        "UPDATE baseline SET locked_at = 0",
        "DELETE FROM baseline",
        "UPDATE baseline_item SET planned_secs = 1",
        "DELETE FROM baseline_item",
    ] {
        let err = conn.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("write-once"), "{sql}: {err}");
    }
}

#[tokio::test]
async fn v8_decides_days_with_time_no_lock_and_leaves_the_rest_undecided() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let v7 = SqliteStore::open_with(&path, &migrations_through(7)).unwrap();
    v7.commit(batch_for(&sample_day())).await.unwrap();
    let mut idle = Day::new(other_date(), "UTC");
    idle.add(item(10, "V")).unwrap();
    v7.commit(batch_for(&idle)).await.unwrap();
    drop(v7);
    assert_eq!(user_version(&path), 7);

    let store = SqliteStore::open(&path).unwrap();
    let started = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(started.lock_decision(), LockDecision::NoLock);
    let untouched = store.load_day(other_date()).await.unwrap().unwrap();
    assert_eq!(untouched.lock_decision(), LockDecision::None);
    assert_eq!(store.load_baseline(date()).await.unwrap(), None);
    drop(store);
    assert_eq!(user_version(&path), 8);
    assert_eq!(backups(&dir), vec![backup_path(&path, 7)]);
}

#[tokio::test]
async fn a_second_lock_decision_fails_and_rolls_back_its_batch() {
    let dir = TempDir::new().unwrap();
    let path = db(&dir);
    let store = seeded(&path).await;
    store
        .commit(batch([set_decision(LockDecision::NoLock)]))
        .await
        .unwrap();
    let err = store
        .commit(batch([
            set_decision(LockDecision::Locked),
            StoreOp::PutBaseline {
                baseline: sample_baseline(700),
            },
        ]))
        .await
        .unwrap_err();
    assert!(err.message.contains("already decided"), "{err}");
    let day = store.load_day(date()).await.unwrap().unwrap();
    assert_eq!(day.lock_decision(), LockDecision::NoLock);
    assert_eq!(
        store.load_baseline(date()).await.unwrap(),
        None,
        "rolled back"
    );
}
