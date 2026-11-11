//! SQLite store adapter (AD-6, AD-12).
//!
//! One WAL connection, owned by one dedicated thread that serves every request in
//! order. Opening and migrating finish before [`SqliteStore::open`] returns.

mod schema;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use nguda_core::{
    Date, Day, DayLogEntry, DayLogKind, ItemId, ItemStatus, OpenSegmentRef, OrderKey, PlanItem,
    PlanItemRecord, Segment, Store, StoreBatch, StoreError, StoreFuture, StoreOp, Timestamp, Uuid,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Row, Transaction, params};
use rusqlite_migration::{Migrations, SchemaVersion};
use tokio::sync::oneshot;

/// Why the database could not be opened. Nothing is opened on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The database was written by a newer app. It is left untouched.
    NewerSchema {
        found: usize,
        supported: usize,
    },
    /// The pre-migration backup could not be written. No migration ran.
    Backup(String),
    Sqlite(String),
    Migration(String),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerSchema { found, supported } => write!(
                f,
                "database schema version {found} is newer than the supported version {supported}"
            ),
            Self::Backup(e) => write!(f, "pre-migration backup failed: {e}"),
            Self::Sqlite(e) => write!(f, "sqlite error: {e}"),
            Self::Migration(e) => write!(f, "migration failed: {e}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<rusqlite::Error> for OpenError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

enum Request {
    Commit(StoreBatch, oneshot::Sender<Result<(), StoreError>>),
    LoadDay(Date, oneshot::Sender<Result<Option<Day>, StoreError>>),
    LastAliveAt(oneshot::Sender<Result<Option<Timestamp>, StoreError>>),
    OpenSegment(oneshot::Sender<Result<Option<OpenSegmentRef>, StoreError>>),
    LoadDayLog(Date, oneshot::Sender<Result<Vec<DayLogEntry>, StoreError>>),
}

/// The SQLite implementation of the [`Store`] port.
pub struct SqliteStore {
    requests: Option<mpsc::Sender<Request>>,
    worker: Option<JoinHandle<()>>,
}

impl SqliteStore {
    /// Opens (creating if needed) and migrates the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        Self::open_with(path.as_ref(), &schema::migrations())
    }

    fn open_with(path: &Path, migrations: &Migrations<'_>) -> Result<Self, OpenError> {
        let conn = open_connection(path, migrations)?;
        let (requests, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("nguda-store".into())
            .spawn(move || serve(conn, rx))
            .map_err(|e| OpenError::Sqlite(format!("cannot start store thread: {e}")))?;
        Ok(Self {
            requests: Some(requests),
            worker: Some(worker),
        })
    }

    /// Queues `request` now, so requests are served in call order, and awaits the reply.
    fn call<T: Send + 'static>(
        &self,
        request: Request,
        reply: oneshot::Receiver<Result<T, StoreError>>,
    ) -> StoreFuture<T> {
        let sent = self
            .requests
            .as_ref()
            .is_some_and(|tx| tx.send(request).is_ok());
        Box::pin(async move {
            if !sent {
                return Err(worker_gone());
            }
            reply.await.unwrap_or_else(|_| Err(worker_gone()))
        })
    }
}

impl Drop for SqliteStore {
    /// Stops the worker after it serves queued requests, closing the connection.
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Store for SqliteStore {
    fn commit(&self, batch: StoreBatch) -> StoreFuture<()> {
        let (tx, rx) = oneshot::channel();
        self.call(Request::Commit(batch, tx), rx)
    }

    fn load_day(&self, date: Date) -> StoreFuture<Option<Day>> {
        let (tx, rx) = oneshot::channel();
        self.call(Request::LoadDay(date, tx), rx)
    }

    fn last_alive_at(&self) -> StoreFuture<Option<Timestamp>> {
        let (tx, rx) = oneshot::channel();
        self.call(Request::LastAliveAt(tx), rx)
    }

    fn open_segment(&self) -> StoreFuture<Option<OpenSegmentRef>> {
        let (tx, rx) = oneshot::channel();
        self.call(Request::OpenSegment(tx), rx)
    }

    fn load_day_log(&self, date: Date) -> StoreFuture<Vec<DayLogEntry>> {
        let (tx, rx) = oneshot::channel();
        self.call(Request::LoadDayLog(date, tx), rx)
    }
}

fn worker_gone() -> StoreError {
    StoreError::new("store worker stopped")
}

/// Opens the connection, refuses a newer schema before writing anything, then sets
/// the pragmas, backs up an existing schema that has pending migrations, and migrates.
fn open_connection(path: &Path, migrations: &Migrations<'_>) -> Result<Connection, OpenError> {
    // Probe an existing file read-only: closing a read-only connection never
    // checkpoints WAL frames into the main file, so a refusal changes no bytes.
    if path.exists() {
        let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        refuse_newer(&probe, migrations)?;
    }
    let mut conn = Connection::open(path)?;
    let found = refuse_newer(&conn, migrations)?;
    let pending = migrations
        .pending_migrations(&conn)
        .map_err(|e| OpenError::Sqlite(e.to_string()))?;

    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::Sqlite(format!(
            "journal_mode is {mode}, expected wal"
        )));
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    if pending > 0 {
        if found > 0 {
            backup(&conn, &backup_path(path, found))?;
        }
        migrations
            .to_latest(&mut conn)
            .map_err(|e| OpenError::Migration(e.to_string()))?;
    }
    Ok(conn)
}

/// The schema version, or `NewerSchema` if it is past the app's latest migration.
fn refuse_newer(conn: &Connection, migrations: &Migrations<'_>) -> Result<usize, OpenError> {
    let version = migrations
        .current_version(conn)
        .map_err(|e| OpenError::Sqlite(e.to_string()))?;
    let found = usize::from(version);
    if let SchemaVersion::Outside(_) = version {
        let pending = migrations
            .pending_migrations(conn)
            .map_err(|e| OpenError::Sqlite(e.to_string()))?;
        let supported = usize::try_from(found as i64 + i64::from(pending)).unwrap_or(0);
        return Err(OpenError::NewerSchema { found, supported });
    }
    Ok(found)
}

/// `<db file name>.v<from>.bak`, beside the database.
fn backup_path(path: &Path, from: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".v{from}.bak"));
    path.with_file_name(name)
}

/// A consistent copy via `VACUUM INTO`, which includes WAL content. A stale file at
/// `dest` is removed first, since `VACUUM INTO` refuses to overwrite.
fn backup(conn: &Connection, dest: &Path) -> Result<(), OpenError> {
    match std::fs::remove_file(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(OpenError::Backup(format!(
                "cannot remove stale backup: {e}"
            )));
        }
    }
    let dest = dest
        .to_str()
        .ok_or_else(|| OpenError::Backup("backup path is not valid UTF-8".into()))?;
    conn.execute("VACUUM INTO ?1", [dest])
        .map_err(|e| OpenError::Backup(e.to_string()))?;
    Ok(())
}

/// The worker loop: serves requests in order until every sender is dropped.
fn serve(mut conn: Connection, rx: mpsc::Receiver<Request>) {
    for request in rx {
        match request {
            Request::Commit(batch, reply) => {
                let _ = reply.send(commit(&mut conn, &batch));
            }
            Request::LoadDay(date, reply) => {
                let _ = reply.send(load_day(&conn, date));
            }
            Request::LastAliveAt(reply) => {
                let _ = reply.send(last_alive_at(&conn));
            }
            Request::OpenSegment(reply) => {
                let _ = reply.send(open_segment(&conn));
            }
            Request::LoadDayLog(date, reply) => {
                let _ = reply.send(load_day_log(&conn, date));
            }
        }
    }
}

fn sql_err(context: impl fmt::Display) -> impl FnOnce(rusqlite::Error) -> StoreError {
    move |e| StoreError::new(format!("{context}: {e}"))
}

/// Runs `sql` and maps every row with `f`. Any SQLite error is reported under `ctx`.
fn query_all<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    ctx: &str,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, StoreError> {
    let mut stmt = conn.prepare(sql).map_err(sql_err(ctx))?;
    stmt.query_map(params, f)
        .map_err(sql_err(ctx))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_err(ctx))
}

/// One transaction per batch. Dropping the transaction on error rolls it back.
fn commit(conn: &mut Connection, batch: &StoreBatch) -> Result<(), StoreError> {
    let tx = conn.transaction().map_err(sql_err("begin batch"))?;
    for op in batch.ops() {
        apply(&tx, op)?;
    }
    tx.commit().map_err(sql_err("commit batch"))
}

fn apply(tx: &Transaction<'_>, op: &StoreOp) -> Result<(), StoreError> {
    match op {
        StoreOp::PutDay { date, tz } => {
            let ctx = format!("put day {date}");
            let changed = tx
                .execute(
                    "INSERT INTO day (date, tz) VALUES (?1, ?2)
                     ON CONFLICT (date) DO UPDATE SET tz = excluded.tz
                     WHERE tz = excluded.tz",
                    params![date.to_string(), tz],
                )
                .map_err(sql_err(&ctx))?;
            if changed != 1 {
                return Err(StoreError::new(format!(
                    "{ctx}: stored with a different time zone"
                )));
            }
        }
        StoreOp::PutItem { date, item } => {
            let id = item.id();
            let ctx = format!("put item {id}");
            let changed = tx
                .execute(
                    r#"INSERT INTO plan_item
                         (id, day_date, title, planned_secs, "order", fixed, pinned_start,
                          status, removed_at)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                       ON CONFLICT (id) DO UPDATE SET
                         title = excluded.title,
                         planned_secs = excluded.planned_secs,
                         "order" = excluded."order",
                         fixed = excluded.fixed,
                         pinned_start = excluded.pinned_start,
                         status = excluded.status,
                         removed_at = excluded.removed_at
                       WHERE day_date = excluded.day_date"#,
                    params![
                        id.to_string(),
                        date.to_string(),
                        item.title(),
                        i64::try_from(item.planned_secs()).map_err(|_| StoreError::new(
                            format!("{ctx}: planned_secs out of range")
                        ))?,
                        item.order().as_str(),
                        item.fixed(),
                        item.pinned_start().map(|t| to_nanos(t, &ctx)).transpose()?,
                        status_str(item.status()),
                        item.removed_at().map(|t| to_nanos(t, &ctx)).transpose()?,
                    ],
                )
                .map_err(sql_err(&ctx))?;
            if changed != 1 {
                return Err(StoreError::new(format!(
                    "{ctx}: item belongs to another day"
                )));
            }
        }
        StoreOp::OpenSegment {
            item_id,
            started_at,
        } => {
            let ctx = format!("open segment for item {item_id}");
            tx.execute(
                "INSERT INTO time_segment (item_id, started_at, ended_at) VALUES (?1, ?2, NULL)",
                params![item_id.to_string(), to_nanos(*started_at, &ctx)?],
            )
            .map_err(sql_err(&ctx))?;
        }
        StoreOp::CloseSegment {
            item_id,
            started_at,
            ended_at,
        } => {
            let ctx = format!("close segment for item {item_id}");
            let changed = tx
                .execute(
                    "UPDATE time_segment SET ended_at = ?3
                     WHERE item_id = ?1 AND started_at = ?2 AND ended_at IS NULL",
                    params![
                        item_id.to_string(),
                        to_nanos(*started_at, &ctx)?,
                        to_nanos(*ended_at, &ctx)?
                    ],
                )
                .map_err(sql_err(&ctx))?;
            if changed != 1 {
                return Err(StoreError::new(format!("{ctx}: no such open segment")));
            }
        }
        StoreOp::SetLastAlive { at } => {
            let ctx = "set last alive";
            tx.execute(
                "INSERT INTO engine_meta (id, last_alive_at) VALUES (1, ?1)
                 ON CONFLICT (id) DO UPDATE SET last_alive_at = excluded.last_alive_at",
                [to_nanos(*at, ctx)?],
            )
            .map_err(sql_err(ctx))?;
        }
        StoreOp::AppendDayLog { date, entry } => {
            let ctx = format!("append day log for {date}");
            let (kind, item_id, to_item_id, secs) = match &entry.kind {
                DayLogKind::Recovered { item_id } => (KIND_RECOVERED, item_id, None, None),
                DayLogKind::Removed { item_id } => (KIND_REMOVED, item_id, None, None),
                DayLogKind::Switched { from, to } => (KIND_SWITCHED, from, *to, None),
                DayLogKind::Extended { item_id, secs } => {
                    let secs = i64::try_from(*secs)
                        .map_err(|_| StoreError::new(format!("{ctx}: secs out of range")))?;
                    (KIND_EXTENDED, item_id, None, Some(secs))
                }
                DayLogKind::Inserted { item_id } => (KIND_INSERTED, item_id, None, None),
            };
            tx.execute(
                "INSERT INTO day_log (day_date, at, kind, item_id, to_item_id, secs)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    date.to_string(),
                    to_nanos(entry.at, &ctx)?,
                    kind,
                    item_id.to_string(),
                    to_item_id.map(|id| id.to_string()),
                    secs
                ],
            )
            .map_err(sql_err(&ctx))?;
        }
        StoreOp::SetInterruptStack { date, items } => {
            let ctx = format!("set interrupt stack for {date}");
            tx.execute(
                "DELETE FROM interrupt_entry WHERE day_date = ?1",
                [date.to_string()],
            )
            .map_err(sql_err(&ctx))?;
            for (position, item_id) in items.iter().enumerate() {
                let position = i64::try_from(position)
                    .map_err(|_| StoreError::new(format!("{ctx}: stack too deep")))?;
                tx.execute(
                    "INSERT INTO interrupt_entry (day_date, position, item_id)
                     VALUES (?1, ?2, ?3)",
                    params![date.to_string(), position, item_id.to_string()],
                )
                .map_err(sql_err(&ctx))?;
            }
        }
    }
    Ok(())
}

/// Stored `day_log.kind` values.
const KIND_RECOVERED: &str = "recovered";
const KIND_REMOVED: &str = "removed";
const KIND_SWITCHED: &str = "switched";
const KIND_EXTENDED: &str = "extended";
const KIND_INSERTED: &str = "inserted";

fn last_alive_at(conn: &Connection) -> Result<Option<Timestamp>, StoreError> {
    let ctx = "load last alive";
    conn.query_row(
        "SELECT last_alive_at FROM engine_meta WHERE id = 1",
        [],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(sql_err(ctx))?
    .map(|n| from_nanos(n, ctx))
    .transpose()
}

fn open_segment(conn: &Connection) -> Result<Option<OpenSegmentRef>, StoreError> {
    let ctx = "load open segment";
    let Some((date, item_id, started_at)) = conn
        .query_row(
            "SELECT p.day_date, s.item_id, s.started_at
             FROM time_segment s JOIN plan_item p ON p.id = s.item_id
             WHERE s.ended_at IS NULL",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_err(ctx))?
    else {
        return Ok(None);
    };
    let item_id = parse_id(&item_id)?;
    let ctx = format!("open segment for item {item_id}");
    let date = date
        .parse::<Date>()
        .map_err(|_| StoreError::new(format!("{ctx}: invalid stored date")))?;
    Ok(Some(OpenSegmentRef {
        date,
        item_id,
        started_at: from_nanos(started_at, &ctx)?,
    }))
}

fn load_day_log(conn: &Connection, date: Date) -> Result<Vec<DayLogEntry>, StoreError> {
    let ctx = format!("load day log {date}");
    let rows = query_all(
        conn,
        "SELECT id, at, kind, item_id, to_item_id, secs FROM day_log
         WHERE day_date = ?1 ORDER BY id",
        [date.to_string()],
        &ctx,
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        },
    )?;
    rows.into_iter()
        .map(|(row_id, at, kind, item_id, to_item_id, secs)| {
            let ctx = format!("day log entry {row_id}");
            let kind = match (kind.as_str(), item_id) {
                (KIND_RECOVERED, Some(item_id)) => DayLogKind::Recovered {
                    item_id: parse_id(&item_id)?,
                },
                (KIND_REMOVED, Some(item_id)) => DayLogKind::Removed {
                    item_id: parse_id(&item_id)?,
                },
                (KIND_SWITCHED, Some(from)) => DayLogKind::Switched {
                    from: parse_id(&from)?,
                    to: to_item_id.as_deref().map(parse_id).transpose()?,
                },
                (KIND_EXTENDED, Some(item_id)) => DayLogKind::Extended {
                    item_id: parse_id(&item_id)?,
                    secs: secs
                        .and_then(|s| u64::try_from(s).ok())
                        .filter(|s| *s > 0)
                        .ok_or_else(|| {
                            StoreError::new(format!("{ctx}: missing or invalid secs"))
                        })?,
                },
                (KIND_INSERTED, Some(item_id)) => DayLogKind::Inserted {
                    item_id: parse_id(&item_id)?,
                },
                (
                    KIND_RECOVERED | KIND_REMOVED | KIND_SWITCHED | KIND_EXTENDED | KIND_INSERTED,
                    None,
                ) => {
                    return Err(StoreError::new(format!("{ctx}: missing item id")));
                }
                _ => return Err(StoreError::new(format!("{ctx}: unknown kind"))),
            };
            Ok(DayLogEntry {
                at: from_nanos(at, &ctx)?,
                kind,
            })
        })
        .collect()
}

/// One plan-item row before its segments are attached.
struct ItemRow {
    id: String,
    title: String,
    planned_secs: i64,
    order: String,
    fixed: bool,
    pinned_start: Option<i64>,
    status: String,
    removed_at: Option<i64>,
}

fn load_day(conn: &Connection, date: Date) -> Result<Option<Day>, StoreError> {
    let ctx = format!("load day {date}");
    let date_text = date.to_string();
    let Some(tz) = conn
        .query_row("SELECT tz FROM day WHERE date = ?1", [&date_text], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .map_err(sql_err(&ctx))?
    else {
        return Ok(None);
    };

    let rows = query_all(
        conn,
        r#"SELECT id, title, planned_secs, "order", fixed, pinned_start, status, removed_at
           FROM plan_item WHERE day_date = ?1 ORDER BY rowid"#,
        [&date_text],
        &ctx,
        |r| {
            Ok(ItemRow {
                id: r.get(0)?,
                title: r.get(1)?,
                planned_secs: r.get(2)?,
                order: r.get(3)?,
                fixed: r.get(4)?,
                pinned_start: r.get(5)?,
                status: r.get(6)?,
                removed_at: r.get(7)?,
            })
        },
    )?;

    let mut segments: HashMap<ItemId, Vec<Segment>> = HashMap::new();
    let segment_rows = query_all(
        conn,
        "SELECT s.item_id, s.started_at, s.ended_at
         FROM time_segment s JOIN plan_item p ON p.id = s.item_id
         WHERE p.day_date = ?1 ORDER BY s.started_at, s.id",
        [&date_text],
        &ctx,
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        },
    )?;
    for (item_id, started_at, ended_at) in segment_rows {
        let id = parse_id(&item_id)?;
        let ctx = format!("item {id}");
        segments.entry(id).or_default().push(Segment {
            started_at: from_nanos(started_at, &ctx)?,
            ended_at: ended_at.map(|n| from_nanos(n, &ctx)).transpose()?,
        });
    }

    let items = rows
        .into_iter()
        .map(|row| {
            let id = parse_id(&row.id)?;
            let segments = segments.remove(&id).unwrap_or_default();
            restore_item(id, row, segments)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stack = query_all(
        conn,
        "SELECT item_id FROM interrupt_entry WHERE day_date = ?1 ORDER BY position",
        [&date_text],
        &ctx,
        |r| r.get::<_, String>(0),
    )?
    .iter()
    .map(|s| parse_id(s))
    .collect::<Result<Vec<_>, _>>()?;
    Day::restore(date, tz, items)
        .and_then(|mut day| day.restore_interrupt_stack(stack).map(|()| day))
        .map(Some)
        .map_err(|e| StoreError::new(format!("{ctx}: {e}")))
}

fn restore_item(id: ItemId, row: ItemRow, segments: Vec<Segment>) -> Result<PlanItem, StoreError> {
    let bad = |what: &str| StoreError::new(format!("item {id}: invalid {what}"));
    let ctx = format!("item {id}");
    let record = PlanItemRecord {
        id,
        title: row.title,
        planned_secs: u64::try_from(row.planned_secs).map_err(|_| bad("planned_secs"))?,
        order: OrderKey::new(row.order).map_err(|_| bad("order"))?,
        fixed: row.fixed,
        pinned_start: row.pinned_start.map(|n| from_nanos(n, &ctx)).transpose()?,
        status: parse_status(&row.status).ok_or_else(|| bad("status"))?,
        removed_at: row.removed_at.map(|n| from_nanos(n, &ctx)).transpose()?,
        segments,
    };
    PlanItem::restore(record).map_err(|e| StoreError::new(e.to_string()))
}

fn parse_id(text: &str) -> Result<ItemId, StoreError> {
    Uuid::try_parse(text)
        .map(ItemId)
        .map_err(|_| StoreError::new(format!("invalid stored item id {text:?}")))
}

fn to_nanos(t: Timestamp, ctx: &str) -> Result<i64, StoreError> {
    i64::try_from(t.as_nanosecond())
        .map_err(|_| StoreError::new(format!("{ctx}: instant out of storable range")))
}

fn from_nanos(n: i64, ctx: &str) -> Result<Timestamp, StoreError> {
    Timestamp::from_nanosecond(i128::from(n))
        .map_err(|_| StoreError::new(format!("{ctx}: invalid stored instant")))
}

fn status_str(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::Pending => "pending",
        ItemStatus::Running => "running",
        ItemStatus::Paused => "paused",
        ItemStatus::Done => "done",
        ItemStatus::CarriedOver => "carried_over",
        ItemStatus::Removed => "removed",
    }
}

fn parse_status(text: &str) -> Option<ItemStatus> {
    Some(match text {
        "pending" => ItemStatus::Pending,
        "running" => ItemStatus::Running,
        "paused" => ItemStatus::Paused,
        "done" => ItemStatus::Done,
        "carried_over" => ItemStatus::CarriedOver,
        "removed" => ItemStatus::Removed,
        _ => return None,
    })
}
