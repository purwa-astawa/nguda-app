//! Test doubles: an in-memory store with the SQLite adapter's semantics, a clock
//! whose deadlines fire on `advance`, and sequential ids.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nguda_core::{
    Clock, Date, Day, DayLogEntry, IdGen, ItemId, OpenSegmentRef, PlanItem, PlanItemRecord,
    Segment, SleepFuture, Store, StoreBatch, StoreError, StoreFuture, StoreOp, Timestamp, Uuid,
};
use tokio::sync::watch;

pub(crate) fn ts(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).unwrap()
}

/// A settable clock in whole seconds. `sleep_until` wakes when `advance` or `set`
/// reaches its deadline.
pub(crate) struct FakeClock(watch::Sender<i64>);

impl FakeClock {
    pub(crate) fn at(secs: i64) -> Arc<Self> {
        Arc::new(Self(watch::Sender::new(secs)))
    }

    pub(crate) fn set(&self, secs: i64) {
        self.0.send_replace(secs);
    }

    pub(crate) fn advance(&self, secs: i64) {
        self.0.send_modify(|now| *now += secs);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        ts(*self.0.borrow())
    }

    fn sleep_until(&self, deadline: Timestamp) -> SleepFuture {
        let mut rx = self.0.subscribe();
        Box::pin(async move {
            loop {
                if ts(*rx.borrow_and_update()) >= deadline {
                    return;
                }
                if rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// Ids 1, 2, 3, ... as `Uuid::from_u128`.
#[derive(Default)]
pub(crate) struct SeqIds(AtomicU64);

impl IdGen for SeqIds {
    fn new_item_id(&self) -> ItemId {
        ItemId(Uuid::from_u128(u128::from(
            self.0.fetch_add(1, Ordering::SeqCst) + 1,
        )))
    }
}

#[derive(Clone, Default)]
struct Data {
    /// date → tz.
    days: BTreeMap<Date, String>,
    /// Items in insertion order, stored without segments.
    items: Vec<(Date, PlanItemRecord)>,
    /// Segments in insertion order.
    segments: Vec<(ItemId, Segment)>,
    last_alive: Option<Timestamp>,
    day_log: Vec<(Date, DayLogEntry)>,
}

impl Data {
    fn item_date(&self, id: ItemId) -> Option<Date> {
        self.items.iter().find(|(_, r)| r.id == id).map(|(d, _)| *d)
    }

    fn apply(&mut self, op: &StoreOp) -> Result<(), StoreError> {
        match op {
            StoreOp::PutDay { date, tz } => match self.days.get(date) {
                Some(stored) if stored != tz => {
                    return Err(StoreError::new(format!(
                        "put day {date}: stored with a different time zone"
                    )));
                }
                Some(_) => {}
                None => {
                    self.days.insert(*date, tz.clone());
                }
            },
            StoreOp::PutItem { date, item } => {
                let id = item.id();
                if !self.days.contains_key(date) {
                    return Err(StoreError::new(format!("put item {id}: no day {date}")));
                }
                let record = record_of(item);
                match self.items.iter_mut().find(|(_, r)| r.id == id) {
                    Some((stored, _)) if stored != date => {
                        return Err(StoreError::new(format!(
                            "put item {id}: item belongs to another day"
                        )));
                    }
                    Some((_, r)) => *r = record,
                    None => self.items.push((*date, record)),
                }
            }
            StoreOp::OpenSegment {
                item_id,
                started_at,
            } => {
                let ctx = format!("open segment for item {item_id}");
                if self.item_date(*item_id).is_none() {
                    return Err(StoreError::new(format!("{ctx}: unknown item")));
                }
                if self.segments.iter().any(|(_, s)| s.is_open()) {
                    return Err(StoreError::new(format!("{ctx}: another segment is open")));
                }
                if self
                    .segments
                    .iter()
                    .any(|(i, s)| i == item_id && s.started_at == *started_at)
                {
                    return Err(StoreError::new(format!("{ctx}: duplicate start")));
                }
                self.segments.push((*item_id, Segment::open(*started_at)));
            }
            StoreOp::CloseSegment {
                item_id,
                started_at,
                ended_at,
            } => {
                let seg = self
                    .segments
                    .iter_mut()
                    .find(|(i, s)| i == item_id && s.started_at == *started_at && s.is_open())
                    .ok_or_else(|| {
                        StoreError::new(format!(
                            "close segment for item {item_id}: no such open segment"
                        ))
                    })?;
                if ended_at < started_at {
                    return Err(StoreError::new(format!(
                        "close segment for item {item_id}: ends before it starts"
                    )));
                }
                seg.1.ended_at = Some(*ended_at);
            }
            StoreOp::SetLastAlive { at } => self.last_alive = Some(*at),
            StoreOp::AppendDayLog { date, entry } => {
                if !self.days.contains_key(date) {
                    return Err(StoreError::new(format!("append day log: no day {date}")));
                }
                self.day_log.push((*date, entry.clone()));
            }
        }
        Ok(())
    }

    fn load_day(&self, date: Date) -> Result<Option<Day>, StoreError> {
        let Some(tz) = self.days.get(&date) else {
            return Ok(None);
        };
        let items = self
            .items
            .iter()
            .filter(|(d, _)| *d == date)
            .map(|(_, r)| {
                let mut record = r.clone();
                record.segments = self
                    .segments
                    .iter()
                    .filter(|(i, _)| *i == r.id)
                    .map(|(_, s)| s.clone())
                    .collect();
                record.segments.sort_by_key(|s| s.started_at);
                PlanItem::restore(record).map_err(|e| StoreError::new(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Day::restore(date, tz.clone(), items)
            .map(Some)
            .map_err(|e| StoreError::new(format!("load day {date}: {e}")))
    }

    fn open_segment(&self) -> Option<OpenSegmentRef> {
        self.segments
            .iter()
            .find(|(_, s)| s.is_open())
            .map(|(id, s)| OpenSegmentRef {
                date: self.item_date(*id).expect("segment of a stored item"),
                item_id: *id,
                started_at: s.started_at,
            })
    }
}

fn record_of(item: &PlanItem) -> PlanItemRecord {
    PlanItemRecord {
        id: item.id(),
        title: item.title().to_owned(),
        planned_secs: item.planned_secs(),
        order: item.order().clone(),
        fixed: item.fixed(),
        pinned_start: item.pinned_start(),
        status: item.status(),
        removed_at: item.removed_at(),
        segments: Vec::new(),
    }
}

/// An in-memory [`Store`]: a whole batch applies or none of it.
#[derive(Default)]
pub(crate) struct MemoryStore {
    data: Mutex<Data>,
    batches: Mutex<Vec<StoreBatch>>,
    fail_commits: AtomicBool,
    fail_loads: AtomicBool,
    fail_load_day: AtomicBool,
}

pub(crate) const INJECTED: &str = "injected store failure";

impl MemoryStore {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Makes every commit fail (and record nothing).
    pub(crate) fn fail_commits(&self, fail: bool) {
        self.fail_commits.store(fail, Ordering::SeqCst);
    }

    /// Makes every read fail.
    pub(crate) fn fail_loads(&self, fail: bool) {
        self.fail_loads.store(fail, Ordering::SeqCst);
    }

    /// Makes only `load_day` fail; the other reads still succeed.
    pub(crate) fn fail_load_day(&self, fail: bool) {
        self.fail_load_day.store(fail, Ordering::SeqCst);
    }

    /// Stores `day` with its segments as is, bypassing batches.
    pub(crate) fn seed(&self, day: &Day) {
        let mut data = self.data.lock().unwrap();
        data.days.insert(day.date(), day.tz().to_owned());
        for item in day.items() {
            data.items.push((day.date(), record_of(item)));
            for seg in item.segments() {
                data.segments.push((item.id(), seg.clone()));
            }
        }
    }

    pub(crate) fn seed_last_alive(&self, at: Option<Timestamp>) {
        self.data.lock().unwrap().last_alive = at;
    }

    pub(crate) fn last_alive(&self) -> Option<Timestamp> {
        self.data.lock().unwrap().last_alive
    }

    pub(crate) fn day_log(&self, date: Date) -> Vec<DayLogEntry> {
        let data = self.data.lock().unwrap();
        data.day_log
            .iter()
            .filter(|(d, _)| *d == date)
            .map(|(_, e)| e.clone())
            .collect()
    }

    pub(crate) fn stored_day(&self, date: Date) -> Option<Day> {
        self.data.lock().unwrap().load_day(date).unwrap()
    }

    /// Every successfully committed batch, in order.
    pub(crate) fn batches(&self) -> Vec<StoreBatch> {
        self.batches.lock().unwrap().clone()
    }

    fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Data) -> Result<T, StoreError>,
    ) -> StoreFuture<T> {
        let result = if self.fail_loads.load(Ordering::SeqCst) {
            Err(StoreError::new(INJECTED))
        } else {
            f(&self.data.lock().unwrap())
        };
        Box::pin(async move { result })
    }
}

impl Store for MemoryStore {
    fn commit(&self, batch: StoreBatch) -> StoreFuture<()> {
        let result = if self.fail_commits.load(Ordering::SeqCst) {
            Err(StoreError::new(INJECTED))
        } else {
            let mut data = self.data.lock().unwrap();
            let mut next = data.clone();
            batch
                .ops()
                .iter()
                .try_for_each(|op| next.apply(op))
                .map(|()| {
                    *data = next;
                    self.batches.lock().unwrap().push(batch);
                })
        };
        Box::pin(async move { result })
    }

    fn load_day(&self, date: Date) -> StoreFuture<Option<Day>> {
        if self.fail_load_day.load(Ordering::SeqCst) {
            return Box::pin(async { Err(StoreError::new(INJECTED)) });
        }
        self.read(|d| d.load_day(date))
    }

    fn last_alive_at(&self) -> StoreFuture<Option<Timestamp>> {
        self.read(|d| Ok(d.last_alive))
    }

    fn open_segment(&self) -> StoreFuture<Option<OpenSegmentRef>> {
        self.read(|d| Ok(d.open_segment()))
    }

    fn load_day_log(&self, date: Date) -> StoreFuture<Vec<DayLogEntry>> {
        self.read(|d| {
            Ok(d.day_log
                .iter()
                .filter(|(day, _)| *day == date)
                .map(|(_, e)| e.clone())
                .collect())
        })
    }
}
