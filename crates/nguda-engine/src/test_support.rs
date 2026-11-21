//! Test doubles: an in-memory store with the SQLite adapter's semantics, a clock
//! whose deadlines fire on `advance`, and sequential ids. Also the fixtures shared by
//! the engine's test modules (instants, ids, order keys, sample items and days).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nguda_core::{
    Baseline, CalendarChange, CalendarEntry, CalendarError, CalendarErrorKind, CalendarFuture,
    CalendarInfo, CalendarSource, Clock, Date, Day, DayLogEntry, FetchedEvent, FileSink,
    FileSinkError, IdGen, ItemId, LockChoice, LockDecision, OAuthClient, OpenSegmentRef, OrderKey,
    PlanItem, PlanItemRecord, PromptId, Random, Segment, SessionStore, Settings, SleepFuture,
    Store, StoreBatch, StoreError, StoreFuture, StoreOp, SyncBatch, SyncCursor, Timestamp, Uuid,
    WorkEnd,
};
use tokio::sync::{oneshot, watch};

pub(crate) fn ts(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).unwrap()
}

pub(crate) fn id(n: u128) -> ItemId {
    ItemId(Uuid::from_u128(n))
}

pub(crate) fn key(s: &str) -> OrderKey {
    OrderKey::new(s).unwrap()
}

/// 1970-01-01 (UTC), the day of every instant below one day of seconds.
pub(crate) fn day1() -> Date {
    Date::constant(1970, 1, 1)
}

/// 1970-01-02 (UTC), the day after [`day1`].
pub(crate) fn day2() -> Date {
    Date::constant(1970, 1, 2)
}

/// Item ids from `SeqIds` start at 1, so seeded items use 100 and up.
pub(crate) fn item(n: u128, title: &str, order: &str) -> PlanItem {
    PlanItem::new(id(n), title, 600, key(order))
}

/// A day whose item 100 has been running since 1_000, decided `NoLock` (a day
/// with time on it is always decided).
pub(crate) fn day_running_other() -> Day {
    let mut day = no_lock(Day::new(day1(), "UTC"));
    day.add(item(100, "other", "V")).unwrap();
    day.start(id(100), ts(1_000)).unwrap();
    day
}

/// `day`, decided `NoLock`, so a choiceless Start works on it as before the lock.
pub(crate) fn no_lock(mut day: Day) -> Day {
    day.decide_lock(LockChoice::NoLock).unwrap();
    day
}

/// `day`, decided `Lock`.
pub(crate) fn locked(mut day: Day) -> Day {
    day.decide_lock(LockChoice::Lock).unwrap();
    day
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

/// Item ids 1, 2, 3, ... as `Uuid::from_u128`; prompt ids count separately from
/// [`PROMPT_BASE`] + 1.
#[derive(Default)]
pub(crate) struct SeqIds {
    items: AtomicU64,
    prompts: AtomicU64,
}

/// Prompt ids start above this, so they never equal an item id in a test.
const PROMPT_BASE: u128 = 1 << 64;

/// The `n`th prompt id `SeqIds` mints (1-based).
pub(crate) fn prompt(n: u128) -> PromptId {
    PromptId(Uuid::from_u128(PROMPT_BASE + n))
}

impl IdGen for SeqIds {
    fn new_item_id(&self) -> ItemId {
        ItemId(Uuid::from_u128(u128::from(
            self.items.fetch_add(1, Ordering::SeqCst) + 1,
        )))
    }

    fn new_prompt_id(&self) -> PromptId {
        prompt(u128::from(self.prompts.fetch_add(1, Ordering::SeqCst) + 1))
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
    /// date → interrupt stack, bottom first.
    stacks: BTreeMap<Date, Vec<ItemId>>,
    /// `None` until a work-end is stored.
    work_end: Option<WorkEnd>,
    /// The user's own OAuth client, when stored.
    google_client: Option<OAuthClient>,
    /// date → lock decision; a missing date is undecided.
    decisions: BTreeMap<Date, LockDecision>,
    /// date → its write-once baseline.
    baselines: BTreeMap<Date, Baseline>,
    /// Every tag name ever stored, in first-stored order, unique ASCII
    /// case-insensitively like the SQLite `NOCASE` column.
    tag_names: Vec<String>,
    /// calendar id → its sync cursor.
    cursors: BTreeMap<String, SyncCursor>,
    /// The calendar table, in stored order.
    calendars: Vec<CalendarEntry>,
}

impl Data {
    /// The day log of `date`, in insertion order.
    fn day_log(&self, date: Date) -> Vec<DayLogEntry> {
        self.day_log
            .iter()
            .filter(|(d, _)| *d == date)
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// Stores every name of `tags` not yet stored (ASCII case-insensitively).
    fn add_tag_names(&mut self, tags: &[String]) {
        for tag in tags {
            if !self.tag_names.iter().any(|n| n.eq_ignore_ascii_case(tag)) {
                self.tag_names.push(tag.clone());
            }
        }
    }

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
                self.add_tag_names(item.tags());
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
            StoreOp::SetInterruptStack { date, items } => {
                if !self.days.contains_key(date) {
                    return Err(StoreError::new(format!(
                        "set interrupt stack: no day {date}"
                    )));
                }
                for id in items {
                    if self.item_date(*id) != Some(*date) {
                        return Err(StoreError::new(format!(
                            "set interrupt stack: item {id} is not on day {date}"
                        )));
                    }
                }
                self.stacks.insert(*date, items.clone());
            }
            StoreOp::SetWorkEnd { work_end } => self.work_end = Some(*work_end),
            StoreOp::SetGoogleClient { client } => self.google_client = client.clone(),
            StoreOp::SetLockDecision { date, decision } => {
                if !self.days.contains_key(date) {
                    return Err(StoreError::new(format!("set lock decision: no day {date}")));
                }
                if self
                    .decisions
                    .get(date)
                    .is_some_and(|d| *d != LockDecision::None)
                {
                    return Err(StoreError::new(format!(
                        "set lock decision for {date}: already decided"
                    )));
                }
                self.decisions.insert(*date, *decision);
            }
            StoreOp::PutBaseline { baseline } => {
                let date = baseline.date;
                let ctx = format!("put baseline for {date}");
                if !self.days.contains_key(&date) {
                    return Err(StoreError::new(format!("{ctx}: no day")));
                }
                if self.baselines.contains_key(&date) {
                    return Err(StoreError::new(format!("{ctx}: already stored")));
                }
                for item in &baseline.items {
                    if self.item_date(item.item_id).is_none() {
                        return Err(StoreError::new(format!(
                            "{ctx}: unknown item {}",
                            item.item_id
                        )));
                    }
                }
                self.baselines.insert(date, baseline.clone());
            }
            StoreOp::PutSyncCursor { cursor } => {
                self.cursors
                    .insert(cursor.calendar_id.clone(), cursor.clone());
            }
            StoreOp::ClearSyncCursor { calendar_id } => {
                self.cursors.remove(calendar_id);
            }
            StoreOp::SetCalendars { calendars } => {
                let mut seen = std::collections::BTreeSet::new();
                if !calendars.iter().all(|c| seen.insert(c.id.as_str())) {
                    return Err(StoreError::new("set calendars: duplicate calendar id"));
                }
                self.calendars = calendars.clone();
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
        let stack = self.stacks.get(&date).cloned().unwrap_or_default();
        let decision = self.decisions.get(&date).copied().unwrap_or_default();
        Day::restore(date, tz.clone(), items)
            .and_then(|mut day| day.restore_interrupt_stack(stack).map(|()| day))
            .map(|mut day| {
                day.restore_lock_decision(decision);
                Some(day)
            })
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
        segments: Vec::new(),
        ..full_record_of(item)
    }
}

/// Every field of `item`, segments included.
pub(crate) fn full_record_of(item: &PlanItem) -> PlanItemRecord {
    PlanItemRecord {
        id: item.id(),
        title: item.title().to_owned(),
        planned_secs: item.planned_secs(),
        order: item.order().clone(),
        fixed: item.fixed(),
        pinned_start: item.pinned_start(),
        status: item.status(),
        removed_at: item.removed_at(),
        segments: item.segments().to_vec(),
        carried_from: item.carried_from(),
        tags: item.tags().to_vec(),
        source: item.source().cloned(),
        removed_by_source_at: item.removed_by_source_at(),
        hidden_at: item.hidden_at(),
        excluded: item.excluded(),
        added: item.added(),
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
            data.add_tag_names(item.tags());
            data.items.push((day.date(), record_of(item)));
            for seg in item.segments() {
                data.segments.push((item.id(), seg.clone()));
            }
        }
        data.stacks
            .insert(day.date(), day.interrupt_stack().to_vec());
        data.decisions.insert(day.date(), day.lock_decision());
    }

    /// Stores `items` as `date`'s interrupt stack as is, bypassing batches.
    pub(crate) fn seed_interrupt_stack(&self, date: Date, items: Vec<ItemId>) {
        self.data.lock().unwrap().stacks.insert(date, items);
    }

    /// The stored OAuth client.
    pub(crate) fn google_client(&self) -> Option<OAuthClient> {
        self.data.lock().unwrap().google_client.clone()
    }

    /// Stores `work_end` as is, bypassing batches.
    pub(crate) fn seed_work_end(&self, work_end: WorkEnd) {
        self.data.lock().unwrap().work_end = Some(work_end);
    }

    /// Stores `baseline` as its date's baseline as is, bypassing batches.
    pub(crate) fn seed_baseline(&self, baseline: Baseline) {
        self.data
            .lock()
            .unwrap()
            .baselines
            .insert(baseline.date, baseline);
    }

    /// Appends `entries` to `date`'s day log as is, bypassing batches.
    pub(crate) fn seed_day_log(&self, date: Date, entries: Vec<DayLogEntry>) {
        let mut data = self.data.lock().unwrap();
        data.day_log.extend(entries.into_iter().map(|e| (date, e)));
    }

    /// Stores `cursor` as its calendar's sync cursor as is, bypassing batches.
    pub(crate) fn seed_cursor(&self, cursor: SyncCursor) {
        self.data
            .lock()
            .unwrap()
            .cursors
            .insert(cursor.calendar_id.clone(), cursor);
    }

    /// Every stored sync cursor, by calendar id.
    pub(crate) fn stored_cursors(&self) -> Vec<SyncCursor> {
        self.data
            .lock()
            .unwrap()
            .cursors
            .values()
            .cloned()
            .collect()
    }

    /// Stores `calendars` as the calendar table as is, bypassing batches.
    pub(crate) fn seed_calendars(&self, calendars: Vec<CalendarEntry>) {
        self.data.lock().unwrap().calendars = calendars;
    }

    /// The stored calendar table, in stored order.
    pub(crate) fn stored_calendars(&self) -> Vec<CalendarEntry> {
        self.data.lock().unwrap().calendars.clone()
    }

    pub(crate) fn seed_last_alive(&self, at: Option<Timestamp>) {
        self.data.lock().unwrap().last_alive = at;
    }

    pub(crate) fn last_alive(&self) -> Option<Timestamp> {
        self.data.lock().unwrap().last_alive
    }

    pub(crate) fn day_log(&self, date: Date) -> Vec<DayLogEntry> {
        self.data.lock().unwrap().day_log(date)
    }

    pub(crate) fn stored_day(&self, date: Date) -> Option<Day> {
        self.data.lock().unwrap().load_day(date).unwrap()
    }

    /// The stored baseline of `date`, as committed.
    pub(crate) fn stored_baseline(&self, date: Date) -> Option<Baseline> {
        self.data.lock().unwrap().baselines.get(&date).cloned()
    }

    /// Every stored tag name, in first-stored order.
    pub(crate) fn tag_names(&self) -> Vec<String> {
        self.data.lock().unwrap().tag_names.clone()
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
        self.read(|d| Ok(d.day_log(date)))
    }

    fn load_settings(&self) -> StoreFuture<Settings> {
        self.read(|d| {
            Ok(Settings {
                work_end: d.work_end.unwrap_or_default(),
                google_client: d.google_client.clone(),
            })
        })
    }

    fn load_baseline(&self, date: Date) -> StoreFuture<Option<Baseline>> {
        self.read(|d| Ok(d.baselines.get(&date).cloned()))
    }

    fn load_tag_names(&self) -> StoreFuture<Vec<String>> {
        self.read(|d| Ok(d.tag_names.clone()))
    }

    fn stored_dates(&self) -> StoreFuture<Vec<Date>> {
        self.read(|d| Ok(d.days.keys().copied().collect()))
    }

    fn load_sync_cursors(&self) -> StoreFuture<Vec<SyncCursor>> {
        self.read(|d| Ok(d.cursors.values().cloned().collect()))
    }

    fn load_calendars(&self) -> StoreFuture<Vec<CalendarEntry>> {
        self.read(|d| Ok(d.calendars.clone()))
    }
}

pub(crate) const SINK_FAILURE: &str = "injected sink failure";

/// A [`FileSink`] that records each write as `(path, bytes)`, or fails while
/// `fail` is set (recording nothing).
#[derive(Default)]
pub(crate) struct FakeFileSink {
    writes: Mutex<Vec<(PathBuf, Vec<u8>)>>,
    fail: AtomicBool,
}

impl FakeFileSink {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// Every successful write, in order.
    pub(crate) fn writes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.writes.lock().unwrap().clone()
    }
}

impl FileSink for FakeFileSink {
    fn write(&self, path: &Path, contents: &[u8]) -> Result<(), FileSinkError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(FileSinkError::new(SINK_FAILURE));
        }
        self.writes
            .lock()
            .unwrap()
            .push((path.to_owned(), contents.to_vec()));
        Ok(())
    }
}

/// The calendar id the fake's batches use: the fake account's primary calendar.
pub(crate) const CALENDAR: &str = "me@example.com";

type FetchReply = oneshot::Sender<Result<SyncBatch, CalendarError>>;
type ListReply = oneshot::Sender<Result<Vec<FetchedEvent>, CalendarError>>;
type GetReply = oneshot::Sender<Result<Option<CalendarChange>, CalendarError>>;
/// What one `list_events` call was given: the calendar id and the window.
pub(crate) type ListCall = (String, (Timestamp, Timestamp));
/// What one `get_event` call was given: the calendar id, the event id, and the
/// window.
pub(crate) type GetCall = (String, String, (Timestamp, Timestamp));
type SignInReply = oneshot::Sender<Result<SessionStore, CalendarError>>;
/// What one sync call was given: the calendar id, the sync token (`None` for a
/// full fetch), and the window.
pub(crate) type SyncCall = (String, Option<String>, (Timestamp, Timestamp));
/// A sync's inputs and its pending reply (taken once resolved).
type PendingFetch = (SyncCall, Option<FetchReply>);

/// The fake account's primary calendar, as listed.
pub(crate) fn primary_info() -> CalendarInfo {
    CalendarInfo {
        id: CALENDAR.into(),
        name: "Me".into(),
        primary: true,
    }
}

/// A [`CalendarSource`] whose syncs and sign-ins the test answers explicitly,
/// one by one and in any order: each call waits on its own oneshot until
/// resolved. Listing answers at once with a settable list (by default the one
/// primary calendar [`CALENDAR`]).
pub(crate) struct FakeCalendar {
    fetches: Mutex<Vec<PendingFetch>>,
    lists_of_events: Mutex<Vec<(ListCall, Option<ListReply>)>>,
    gets: Mutex<Vec<(GetCall, Option<GetReply>)>>,
    sign_ins: Mutex<Vec<Option<SignInReply>>>,
    listing: Mutex<Result<Vec<CalendarInfo>, CalendarError>>,
    lists: AtomicU64,
    /// Whether a client is built in.
    built_in: AtomicBool,
    /// Every `set_client` call, in order.
    clients: Mutex<Vec<Option<OAuthClient>>>,
    /// How many `sign_out` calls were made.
    sign_outs: AtomicU64,
    /// What every `sign_out` answers.
    sign_out_result: Mutex<Result<(), CalendarError>>,
}

impl Default for FakeCalendar {
    fn default() -> Self {
        Self {
            fetches: Mutex::default(),
            lists_of_events: Mutex::default(),
            gets: Mutex::default(),
            sign_ins: Mutex::default(),
            listing: Mutex::new(Ok(vec![primary_info()])),
            lists: AtomicU64::new(0),
            built_in: AtomicBool::new(true),
            clients: Mutex::default(),
            sign_outs: AtomicU64::new(0),
            sign_out_result: Mutex::new(Ok(())),
        }
    }
}

impl FakeCalendar {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets whether a client is built in (`true` by default).
    pub(crate) fn set_built_in(&self, built_in: bool) {
        self.built_in.store(built_in, Ordering::SeqCst);
    }

    /// Every `set_client` call, in order.
    pub(crate) fn clients(&self) -> Vec<Option<OAuthClient>> {
        self.clients.lock().unwrap().clone()
    }

    /// How many sign-outs were made.
    pub(crate) fn sign_out_count(&self) -> u64 {
        self.sign_outs.load(Ordering::SeqCst)
    }

    /// Sets what every later sign-out answers.
    pub(crate) fn set_sign_out_result(&self, result: Result<(), CalendarError>) {
        *self.sign_out_result.lock().unwrap() = result;
    }

    /// Sets what every later listing answers.
    pub(crate) fn set_listing(&self, listing: Result<Vec<CalendarInfo>, CalendarError>) {
        *self.listing.lock().unwrap() = listing;
    }

    /// How many listings were made.
    pub(crate) fn list_count(&self) -> u64 {
        self.lists.load(Ordering::SeqCst)
    }

    pub(crate) fn fetch_count(&self) -> usize {
        self.fetches.lock().unwrap().len()
    }

    pub(crate) fn sign_in_count(&self) -> usize {
        self.sign_ins.lock().unwrap().len()
    }

    /// The window of sync `n` (0-based).
    pub(crate) fn fetch_window(&self, n: usize) -> (Timestamp, Timestamp) {
        self.fetches.lock().unwrap()[n].0.2
    }

    /// The calendar id sync `n` (0-based) was given.
    pub(crate) fn fetch_calendar(&self, n: usize) -> String {
        self.fetches.lock().unwrap()[n].0.0.clone()
    }

    /// The `(calendar_id, token)` sync `n` (0-based) resumed from, `None` for a
    /// full fetch.
    pub(crate) fn fetch_cursor(&self, n: usize) -> Option<(String, String)> {
        let calls = self.fetches.lock().unwrap();
        let (calendar_id, token, _) = &calls[n].0;
        token.clone().map(|t| (calendar_id.clone(), t))
    }

    /// Answers sync `n` (0-based) with `result`.
    pub(crate) fn resolve_fetch(&self, n: usize, result: Result<SyncBatch, CalendarError>) {
        let reply = self.fetches.lock().unwrap()[n]
            .1
            .take()
            .expect("fetch already resolved");
        let _ = reply.send(result);
    }

    /// Answers sync `n` (0-based) with `events` as upserts of [`CALENDAR`]
    /// and no next token, or with the error.
    pub(crate) fn resolve_events(
        &self,
        n: usize,
        result: Result<Vec<FetchedEvent>, CalendarError>,
    ) {
        self.resolve_fetch(
            n,
            result.map(|events| SyncBatch {
                calendar_id: CALENDAR.into(),
                changes: events.into_iter().map(CalendarChange::Upsert).collect(),
                next_token: None,
            }),
        );
    }

    /// How many `list_events` calls were made.
    pub(crate) fn list_events_count(&self) -> usize {
        self.lists_of_events.lock().unwrap().len()
    }

    /// What `list_events` call `n` (0-based) was given.
    pub(crate) fn list_events_call(&self, n: usize) -> ListCall {
        self.lists_of_events.lock().unwrap()[n].0.clone()
    }

    /// Answers `list_events` call `n` (0-based) with `result`.
    pub(crate) fn resolve_list_events(
        &self,
        n: usize,
        result: Result<Vec<FetchedEvent>, CalendarError>,
    ) {
        let reply = self.lists_of_events.lock().unwrap()[n]
            .1
            .take()
            .expect("list already resolved");
        let _ = reply.send(result);
    }

    /// How many `get_event` calls were made.
    pub(crate) fn get_count(&self) -> usize {
        self.gets.lock().unwrap().len()
    }

    /// What `get_event` call `n` (0-based) was given.
    pub(crate) fn get_call(&self, n: usize) -> GetCall {
        self.gets.lock().unwrap()[n].0.clone()
    }

    /// Answers `get_event` call `n` (0-based) with `result`.
    pub(crate) fn resolve_get(
        &self,
        n: usize,
        result: Result<Option<CalendarChange>, CalendarError>,
    ) {
        let reply = self.gets.lock().unwrap()[n]
            .1
            .take()
            .expect("get already resolved");
        let _ = reply.send(result);
    }

    /// Answers sign-in `n` (0-based) with `result`.
    pub(crate) fn resolve_sign_in(&self, n: usize, result: Result<SessionStore, CalendarError>) {
        let reply = self.sign_ins.lock().unwrap()[n]
            .take()
            .expect("sign-in already resolved");
        let _ = reply.send(result);
    }
}

fn dropped() -> CalendarError {
    CalendarError::new(CalendarErrorKind::Other, "fake call dropped")
}

impl CalendarSource for FakeCalendar {
    fn sign_in(&self) -> CalendarFuture<SessionStore> {
        let (tx, rx) = oneshot::channel();
        self.sign_ins.lock().unwrap().push(Some(tx));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(dropped())) })
    }

    fn sign_out(&self) -> CalendarFuture<()> {
        self.sign_outs.fetch_add(1, Ordering::SeqCst);
        let result = self.sign_out_result.lock().unwrap().clone();
        Box::pin(async move { result })
    }

    fn set_client(&self, client: Option<OAuthClient>) {
        self.clients.lock().unwrap().push(client);
    }

    fn has_client(&self) -> bool {
        self.built_in.load(Ordering::SeqCst)
            || self
                .clients
                .lock()
                .unwrap()
                .last()
                .is_some_and(Option::is_some)
    }

    fn list_calendars(&self) -> CalendarFuture<Vec<CalendarInfo>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let listing = self.listing.lock().unwrap().clone();
        Box::pin(async move { listing })
    }

    fn sync_calendar(
        &self,
        calendar_id: String,
        token: Option<String>,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<SyncBatch> {
        let (tx, rx) = oneshot::channel();
        self.fetches
            .lock()
            .unwrap()
            .push(((calendar_id, token, (window_start, window_end)), Some(tx)));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(dropped())) })
    }

    fn list_events(
        &self,
        calendar_id: String,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<Vec<FetchedEvent>> {
        let (tx, rx) = oneshot::channel();
        self.lists_of_events
            .lock()
            .unwrap()
            .push(((calendar_id, (window_start, window_end)), Some(tx)));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(dropped())) })
    }

    fn get_event(
        &self,
        calendar_id: String,
        event_id: String,
        window_start: Timestamp,
        window_end: Timestamp,
    ) -> CalendarFuture<Option<CalendarChange>> {
        let (tx, rx) = oneshot::channel();
        self.gets.lock().unwrap().push((
            (calendar_id, event_id, (window_start, window_end)),
            Some(tx),
        ));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(dropped())) })
    }
}

/// A [`Random`] answering a settable value, `0.5` (no jitter) by default.
pub(crate) struct FixedRandom(Mutex<f64>);

impl FixedRandom {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(0.5)))
    }

    pub(crate) fn set(&self, unit: f64) {
        *self.0.lock().unwrap() = unit;
    }
}

impl Random for FixedRandom {
    fn unit(&self) -> f64 {
        *self.0.lock().unwrap()
    }
}
