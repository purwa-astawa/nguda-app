//! Plan items, the day, and the item timer state machine (AD-6, AD-15).
//!
//! Transitions take `&self` and return a new value, or mutate only on success, so a
//! rejected transition leaves its input unchanged.

use std::fmt;

use jiff::Timestamp;
use jiff::civil::Date;
use uuid::Uuid;

use crate::elapsed::Running;
use crate::error::DomainError;
use crate::order::OrderKey;
use crate::reflow::reflow;
use crate::segment::{Segment, actual_secs, closed_secs};

/// Identity of a plan item. Generation lives outside core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(pub Uuid);

impl fmt::Display for ItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemStatus {
    Pending,
    Running,
    Paused,
    Done,
    CarriedOver,
    Removed,
}

impl fmt::Display for ItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::CarriedOver => "carried over",
            Self::Removed => "removed",
        })
    }
}

/// A requested item transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Start,
    Pause,
    Resume,
    Finish,
    CarryOver,
    Remove,
    /// A plan edit: pin, unpin, reorder, retitle, or resize.
    Edit,
    /// Finish the running item and move on to the next one.
    Switch,
    /// Give the running item more planned time.
    Extend,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Finish => "finish",
            Self::CarryOver => "carry over",
            Self::Remove => "remove",
            Self::Edit => "edit",
            Self::Switch => "switch from",
            Self::Extend => "extend",
        })
    }
}

/// One item of a day's plan. Schedule times are never stored: fixed items sit at
/// `pinned_start`, and `order` ranks flexible items only.
///
/// All fields are private and an item is built only through [`PlanItem::new`], so a
/// fixed item always has a pin, status changes go through the transitions, and an
/// item's identity cannot be changed from outside. Once in a [`Day`], plan edits go
/// through the day's by-id methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    id: ItemId,
    title: String,
    planned_secs: u64,
    order: OrderKey,
    fixed: bool,
    pinned_start: Option<Timestamp>,
    status: ItemStatus,
    removed_at: Option<Timestamp>,
    segments: Vec<Segment>,
}

/// Every field of a [`PlanItem`], public, for rehydrating stored rows. Turn it into
/// an item only through [`PlanItem::restore`], which checks its consistency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItemRecord {
    pub id: ItemId,
    pub title: String,
    pub planned_secs: u64,
    pub order: OrderKey,
    pub fixed: bool,
    pub pinned_start: Option<Timestamp>,
    pub status: ItemStatus,
    pub removed_at: Option<Timestamp>,
    pub segments: Vec<Segment>,
}

impl PlanItem {
    /// Rebuilds a stored item, rejecting a record that breaks an item invariant:
    /// fixed exactly when pinned, removed exactly when tombstoned, no inverted closed
    /// segment, at most one open segment, and an open segment exactly when running.
    pub fn restore(record: PlanItemRecord) -> Result<Self, DomainError> {
        let invalid = |reason| DomainError::InvalidRecord {
            id: record.id,
            reason,
        };
        if record.fixed != record.pinned_start.is_some() {
            return Err(invalid("fixed must match having a pinned start"));
        }
        if (record.status == ItemStatus::Removed) != record.removed_at.is_some() {
            return Err(invalid("removed must match having a removed_at"));
        }
        if record
            .segments
            .iter()
            .any(|s| s.ended_at.is_some_and(|end| end < s.started_at))
        {
            return Err(invalid("segment ends before it starts"));
        }
        let open = record.segments.iter().filter(|s| s.is_open()).count();
        if open > 1 {
            return Err(invalid("more than one open segment"));
        }
        if (record.status == ItemStatus::Running) != (open == 1) {
            return Err(invalid("open segment must match running status"));
        }
        let PlanItemRecord {
            id,
            title,
            planned_secs,
            order,
            fixed,
            pinned_start,
            status,
            removed_at,
            segments,
        } = record;
        Ok(Self {
            id,
            title,
            planned_secs,
            order,
            fixed,
            pinned_start,
            status,
            removed_at,
            segments,
        })
    }

    /// A new flexible, pending item with no recorded time.
    pub fn new(id: ItemId, title: impl Into<String>, planned_secs: u64, order: OrderKey) -> Self {
        Self {
            id,
            title: title.into(),
            planned_secs,
            order,
            fixed: false,
            pinned_start: None,
            status: ItemStatus::Pending,
            removed_at: None,
            segments: Vec::new(),
        }
    }

    pub fn id(&self) -> ItemId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn planned_secs(&self) -> u64 {
        self.planned_secs
    }

    pub fn order(&self) -> &OrderKey {
        &self.order
    }

    pub fn fixed(&self) -> bool {
        self.fixed
    }

    pub fn pinned_start(&self) -> Option<Timestamp> {
        self.pinned_start
    }

    pub fn status(&self) -> ItemStatus {
        self.status
    }

    pub fn removed_at(&self) -> Option<Timestamp> {
        self.removed_at
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Pins the item at `pinned_start`, making it fixed.
    pub(crate) fn set_fixed(&mut self, pinned_start: Timestamp) {
        self.fixed = true;
        self.pinned_start = Some(pinned_start);
    }

    /// Makes the item flexible again, dropping its pin.
    pub(crate) fn set_flexible(&mut self) {
        self.fixed = false;
        self.pinned_start = None;
    }

    pub(crate) fn set_order(&mut self, order: OrderKey) {
        self.order = order;
    }

    pub(crate) fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub(crate) fn set_planned_secs(&mut self, secs: u64) {
        self.planned_secs = secs;
    }

    /// Returns a copy with `f` applied. Every state except Removed allows plan edits.
    fn edit(&self, f: impl FnOnce(&mut Self)) -> Result<Self, DomainError> {
        if self.status == ItemStatus::Removed {
            return Err(self.reject(Action::Edit));
        }
        let mut next = self.clone();
        f(&mut next);
        Ok(next)
    }

    /// Actual time spent as of `now` (AD-6).
    pub fn actual_secs(&self, now: Timestamp) -> u64 {
        actual_secs(&self.segments, now)
    }

    pub fn has_open_segment(&self) -> bool {
        self.segments.iter().any(Segment::is_open)
    }

    /// The tray/view running state of this item while it runs (AD-11).
    pub fn running_view(&self) -> Option<Running> {
        if self.status != ItemStatus::Running {
            return None;
        }
        let open = self.segments.iter().find(|s| s.is_open())?;
        Some(Running {
            item_id: self.id,
            title: self.title.clone(),
            segment_started_at: open.started_at,
            accumulated_secs: closed_secs(&self.segments),
        })
    }

    fn reject(&self, action: Action) -> DomainError {
        DomainError::InvalidTransition {
            from: self.status,
            action,
        }
    }

    /// Opens a segment. Day-level callers enforce the single-open-segment rule.
    fn begin(&self, action: Action, now: Timestamp) -> Result<Self, DomainError> {
        let allowed = match action {
            Action::Start => ItemStatus::Pending,
            Action::Resume => ItemStatus::Paused,
            _ => return Err(self.reject(action)),
        };
        if self.status != allowed {
            return Err(self.reject(action));
        }
        let mut next = self.clone();
        next.status = ItemStatus::Running;
        next.segments.push(Segment::open(now));
        Ok(next)
    }

    /// Closes any open segment at `now`, clamped to its start so a clock running
    /// behind never yields an inverted segment.
    fn close_open_segment(&mut self, now: Timestamp) {
        for seg in self.segments.iter_mut().filter(|s| s.is_open()) {
            seg.ended_at = Some(now.max(seg.started_at));
        }
    }

    /// Running → Paused, closing the open segment at `now`.
    pub fn pause(&self, now: Timestamp) -> Result<Self, DomainError> {
        if self.status != ItemStatus::Running {
            return Err(self.reject(Action::Pause));
        }
        let mut next = self.clone();
        next.close_open_segment(now);
        next.status = ItemStatus::Paused;
        Ok(next)
    }

    /// Running or Paused → Done, closing any open segment at `now`.
    pub fn finish(&self, now: Timestamp) -> Result<Self, DomainError> {
        if !matches!(self.status, ItemStatus::Running | ItemStatus::Paused) {
            return Err(self.reject(Action::Finish));
        }
        let mut next = self.clone();
        next.close_open_segment(now);
        next.status = ItemStatus::Done;
        Ok(next)
    }

    /// Pending or Paused → CarriedOver.
    pub fn carry_over(&self) -> Result<Self, DomainError> {
        if !matches!(self.status, ItemStatus::Pending | ItemStatus::Paused) {
            return Err(self.reject(Action::CarryOver));
        }
        let mut next = self.clone();
        next.status = ItemStatus::CarriedOver;
        Ok(next)
    }

    /// Any state except Running and Removed → Removed, tombstoned at `now`.
    pub fn remove(&self, now: Timestamp) -> Result<Self, DomainError> {
        if matches!(self.status, ItemStatus::Running | ItemStatus::Removed) {
            return Err(self.reject(Action::Remove));
        }
        let mut next = self.clone();
        next.status = ItemStatus::Removed;
        next.removed_at = Some(now);
        Ok(next)
    }
}

/// A local day of plan items (AD-8): a civil date plus the IANA zone fixed for it.
///
/// `items` is private: the day is the only way to change its items, so item ids stay
/// unique and at most one segment is open across the day (AD-6). Every operation
/// leaves the day unchanged when it fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    date: Date,
    tz: String,
    items: Vec<PlanItem>,
}

impl Day {
    pub fn new(date: Date, tz: impl Into<String>) -> Self {
        Self {
            date,
            tz: tz.into(),
            items: Vec::new(),
        }
    }

    /// Rebuilds a stored day, adding each item through [`Day::add`] so duplicate ids
    /// and a second open segment are rejected.
    pub fn restore(
        date: Date,
        tz: impl Into<String>,
        items: impl IntoIterator<Item = PlanItem>,
    ) -> Result<Self, DomainError> {
        let mut day = Self::new(date, tz);
        for item in items {
            day.add(item)?;
        }
        Ok(day)
    }

    pub fn date(&self) -> Date {
        self.date
    }

    /// The IANA zone fixed for this day.
    pub fn tz(&self) -> &str {
        &self.tz
    }

    /// Adds an item. Rejects a duplicate id, and an item with an open segment while
    /// another segment is already open.
    pub fn add(&mut self, item: PlanItem) -> Result<(), DomainError> {
        if self.item(item.id).is_some() {
            return Err(DomainError::DuplicateItem(item.id));
        }
        if item.has_open_segment() && self.has_open_segment() {
            return Err(DomainError::AnotherItemRunning);
        }
        self.items.push(item);
        Ok(())
    }

    /// All items, removed ones included, in insertion order.
    pub fn items(&self) -> &[PlanItem] {
        &self.items
    }

    pub fn item(&self, id: ItemId) -> Option<&PlanItem> {
        self.items.iter().find(|i| i.id == id)
    }

    fn has_open_segment(&self) -> bool {
        self.items.iter().any(PlanItem::has_open_segment)
    }

    fn index(&self, id: ItemId) -> Result<usize, DomainError> {
        self.items
            .iter()
            .position(|i| i.id == id)
            .ok_or(DomainError::UnknownItem(id))
    }

    /// Replaces item `id` with `f(item)`, only if `f` succeeds.
    fn apply(
        &mut self,
        id: ItemId,
        f: impl FnOnce(&PlanItem) -> Result<PlanItem, DomainError>,
    ) -> Result<(), DomainError> {
        let idx = self.index(id)?;
        self.items[idx] = f(&self.items[idx])?;
        Ok(())
    }

    /// Pending → Running, unless another item in the day has an open segment.
    pub fn start(&mut self, id: ItemId, now: Timestamp) -> Result<(), DomainError> {
        self.begin(id, Action::Start, now)
    }

    /// Paused → Running, under the same single-open-segment rule as `start`.
    pub fn resume(&mut self, id: ItemId, now: Timestamp) -> Result<(), DomainError> {
        self.begin(id, Action::Resume, now)
    }

    fn begin(&mut self, id: ItemId, action: Action, now: Timestamp) -> Result<(), DomainError> {
        let idx = self.index(id)?;
        let next = self.items[idx].begin(action, now)?;
        if self.has_open_segment() {
            return Err(DomainError::AnotherItemRunning);
        }
        self.items[idx] = next;
        Ok(())
    }

    /// Running → Paused. See [`PlanItem::pause`].
    pub fn pause(&mut self, id: ItemId, now: Timestamp) -> Result<(), DomainError> {
        self.apply(id, |i| i.pause(now))
    }

    /// Running or Paused → Done. See [`PlanItem::finish`].
    pub fn finish(&mut self, id: ItemId, now: Timestamp) -> Result<(), DomainError> {
        self.apply(id, |i| i.finish(now))
    }

    /// Pending or Paused → CarriedOver. See [`PlanItem::carry_over`].
    pub fn carry_over(&mut self, id: ItemId) -> Result<(), DomainError> {
        self.apply(id, PlanItem::carry_over)
    }

    /// Finishes the running item `current` and starts or resumes [`Day::next_item`]
    /// (as chosen after the finish, as of `now`) at `now`, all or nothing. Returns the item now
    /// running, or `None` when nothing was left. Only a running item can be switched
    /// from (AD-15).
    pub fn switch(
        &mut self,
        current: ItemId,
        now: Timestamp,
    ) -> Result<Option<ItemId>, DomainError> {
        let from = &self.items[self.index(current)?];
        if from.status != ItemStatus::Running {
            return Err(from.reject(Action::Switch));
        }
        let mut next = self.clone();
        next.finish(current, now)?;
        let to = next.next_item(now).map(|i| (i.id, i.status));
        match to {
            Some((id, ItemStatus::Paused)) => next.resume(id, now)?,
            Some((id, _)) => next.start(id, now)?,
            None => {}
        }
        *self = next;
        Ok(to.map(|(id, _)| id))
    }

    /// Any state except Running and Removed → Removed. See [`PlanItem::remove`].
    pub fn remove(&mut self, id: ItemId, now: Timestamp) -> Result<(), DomainError> {
        self.apply(id, |i| i.remove(now))
    }

    /// Pins item `id` at `pinned_start`, making it fixed.
    pub fn set_fixed(&mut self, id: ItemId, pinned_start: Timestamp) -> Result<(), DomainError> {
        self.apply(id, |i| i.edit(|n| n.set_fixed(pinned_start)))
    }

    /// Makes item `id` flexible, dropping its pin.
    pub fn set_flexible(&mut self, id: ItemId) -> Result<(), DomainError> {
        self.apply(id, |i| i.edit(PlanItem::set_flexible))
    }

    /// Sets item `id`'s lexorank key (meaningful for flexible items only).
    pub fn set_order(&mut self, id: ItemId, order: OrderKey) -> Result<(), DomainError> {
        self.apply(id, |i| i.edit(|n| n.set_order(order)))
    }

    pub fn set_title(&mut self, id: ItemId, title: impl Into<String>) -> Result<(), DomainError> {
        let title = title.into();
        self.apply(id, |i| i.edit(|n| n.set_title(title)))
    }

    /// Sets item `id`'s current planned length in whole seconds.
    pub fn set_planned_secs(&mut self, id: ItemId, secs: u64) -> Result<(), DomainError> {
        self.apply(id, |i| i.edit(|n| n.set_planned_secs(secs)))
    }

    /// Adds `secs` to the running item `id`'s planned length (AD-6: the plan grows,
    /// not "now"), returning the new planned length. Only a running item can be
    /// extended (AD-15). An extension that would overflow `u64` is rejected as an
    /// [`Action::Extend`] transition; either way the day is unchanged on a reject.
    pub fn extend(&mut self, id: ItemId, secs: u64) -> Result<u64, DomainError> {
        let idx = self.index(id)?;
        let item = &self.items[idx];
        if item.status != ItemStatus::Running {
            return Err(item.reject(Action::Extend));
        }
        let planned = item
            .planned_secs
            .checked_add(secs)
            .ok_or_else(|| item.reject(Action::Extend))?;
        self.items[idx].planned_secs = planned;
        Ok(planned)
    }

    /// The first pending or paused item in schedule order as of `now` (see
    /// [`crate::reflow`]), or `None` when nothing is left. Paused counts, because
    /// resuming it is a likely next step.
    pub fn next_item(&self, now: Timestamp) -> Option<&PlanItem> {
        reflow(self, now).next().and_then(|id| self.item(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Clock;
    use crate::elapsed::elapsed_secs;
    use crate::test_support::{FakeClock, closed, ts};

    fn id(n: u128) -> ItemId {
        ItemId(Uuid::from_u128(n))
    }

    fn key(s: &str) -> OrderKey {
        OrderKey::new(s).unwrap()
    }

    fn item(n: u128, order: &str) -> PlanItem {
        PlanItem::new(id(n), format!("item {n}"), 1500, key(order))
    }

    fn day(items: Vec<PlanItem>) -> Day {
        let mut d = Day::new(jiff::civil::date(2026, 11, 10), "Europe/Berlin");
        for i in items {
            d.add(i).unwrap();
        }
        d
    }

    fn open_segments(d: &Day) -> usize {
        d.items()
            .iter()
            .flat_map(|i| i.segments())
            .filter(|s| s.is_open())
            .count()
    }

    /// A one-item day with item 1 driven into `status` at the clock's time.
    fn day_with(status: ItemStatus, clock: &FakeClock) -> Day {
        let mut d = day(vec![item(1, "V")]);
        let now = clock.now();
        match status {
            ItemStatus::Pending => {}
            ItemStatus::Running => d.start(id(1), now).unwrap(),
            ItemStatus::Paused => {
                d.start(id(1), now).unwrap();
                d.pause(id(1), now).unwrap();
            }
            ItemStatus::Done => {
                d.start(id(1), now).unwrap();
                d.finish(id(1), now).unwrap();
            }
            ItemStatus::CarriedOver => d.carry_over(id(1)).unwrap(),
            ItemStatus::Removed => d.remove(id(1), now).unwrap(),
        }
        assert_eq!(d.item(id(1)).unwrap().status(), status);
        d
    }

    fn with_status(status: ItemStatus, clock: &FakeClock) -> PlanItem {
        day_with(status, clock).item(id(1)).unwrap().clone()
    }

    const ALL: [ItemStatus; 6] = [
        ItemStatus::Pending,
        ItemStatus::Running,
        ItemStatus::Paused,
        ItemStatus::Done,
        ItemStatus::CarriedOver,
        ItemStatus::Removed,
    ];

    const ACTIONS: [Action; 6] = [
        Action::Start,
        Action::Pause,
        Action::Resume,
        Action::Finish,
        Action::CarryOver,
        Action::Remove,
    ];

    fn legal(s: ItemStatus, a: Action) -> bool {
        use ItemStatus::*;
        matches!(
            (s, a),
            (Pending, Action::Start)
                | (Paused, Action::Resume)
                | (Running, Action::Pause)
                | (Running | Paused, Action::Finish)
                | (Pending | Paused, Action::CarryOver)
                | (Pending | Paused | Done | CarriedOver, Action::Remove)
        )
    }

    fn day_action(d: &mut Day, id: ItemId, a: Action, now: Timestamp) -> Result<(), DomainError> {
        match a {
            Action::Start => d.start(id, now),
            Action::Pause => d.pause(id, now),
            Action::Resume => d.resume(id, now),
            Action::Finish => d.finish(id, now),
            Action::CarryOver => d.carry_over(id),
            Action::Remove => d.remove(id, now),
            Action::Edit => d.set_title(id, "edited"),
            Action::Switch => d.switch(id, now).map(|_| ()),
            Action::Extend => d.extend(id, 60).map(|_| ()),
        }
    }

    #[test]
    fn new_item_is_flexible_and_pending() {
        let i = item(1, "V");
        assert!(!i.fixed());
        assert_eq!(i.pinned_start(), None);
        assert_eq!(i.status(), ItemStatus::Pending);
        assert_eq!(i.removed_at(), None);
        assert!(i.segments().is_empty());
    }

    #[test]
    fn start_opens_one_segment_at_now() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        let i = d.item(id(1)).unwrap();
        assert_eq!(i.status(), ItemStatus::Running);
        assert_eq!(i.segments(), &[Segment::open(clock.now())]);
    }

    #[test]
    fn start_while_another_runs_is_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V"), item(2, "W")]);
        d.start(id(1), clock.now()).unwrap();
        clock.set(1_010);
        let before = d.clone();
        assert_eq!(
            d.start(id(2), clock.now()),
            Err(DomainError::AnotherItemRunning)
        );
        assert_eq!(d, before);
        assert_eq!(open_segments(&d), 1);
    }

    #[test]
    fn pause_closes_the_segment_at_now() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        clock.set(1_010);
        let running = d.item(id(1)).unwrap().clone();
        d.pause(id(1), clock.now()).unwrap();
        let paused = d.item(id(1)).unwrap();
        assert_eq!(paused.status(), ItemStatus::Paused);
        assert_eq!(
            paused.segments(),
            &[Segment {
                started_at: ts(1_000),
                ended_at: Some(ts(1_010)),
            }]
        );
        // The value-returning transition leaves its input untouched.
        let _ = running.pause(clock.now()).unwrap();
        assert_eq!(running.status(), ItemStatus::Running);
    }

    #[test]
    fn resume_opens_a_new_segment() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        clock.set(1_010);
        d.pause(id(1), clock.now()).unwrap();
        clock.set(1_020);
        d.resume(id(1), clock.now()).unwrap();
        let i = d.item(id(1)).unwrap();
        assert_eq!(i.status(), ItemStatus::Running);
        assert_eq!(i.segments().len(), 2);
        assert_eq!(i.segments()[1], Segment::open(ts(1_020)));
    }

    #[test]
    fn resume_while_another_runs_is_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V"), item(2, "W")]);
        d.start(id(1), clock.now()).unwrap();
        d.pause(id(1), clock.now()).unwrap();
        d.start(id(2), clock.now()).unwrap();
        let before = d.clone();
        assert_eq!(
            d.resume(id(1), clock.now()),
            Err(DomainError::AnotherItemRunning)
        );
        assert_eq!(d, before);
    }

    #[test]
    fn finish_from_running_or_paused_closes_any_open_segment() {
        let clock = FakeClock::at(1_000);
        for s in [ItemStatus::Running, ItemStatus::Paused] {
            clock.set(1_000);
            let mut d = day_with(s, &clock);
            clock.set(1_030);
            d.finish(id(1), clock.now()).unwrap();
            let done = d.item(id(1)).unwrap();
            assert_eq!(done.status(), ItemStatus::Done);
            assert_eq!(open_segments(&d), 0);
            let expected_end = if s == ItemStatus::Running {
                1_030
            } else {
                1_000
            };
            assert_eq!(done.segments()[0].ended_at, Some(ts(expected_end)));
        }
    }

    #[test]
    fn carry_over_from_pending_or_paused() {
        let clock = FakeClock::at(1_000);
        for s in [ItemStatus::Pending, ItemStatus::Paused] {
            let mut d = day_with(s, &clock);
            d.carry_over(id(1)).unwrap();
            assert_eq!(d.item(id(1)).unwrap().status(), ItemStatus::CarriedOver);
        }
    }

    #[test]
    fn remove_tombstones_at_now() {
        let clock = FakeClock::at(1_000);
        for s in [
            ItemStatus::Pending,
            ItemStatus::Paused,
            ItemStatus::Done,
            ItemStatus::CarriedOver,
        ] {
            clock.set(1_000);
            let mut d = day_with(s, &clock);
            clock.set(2_000);
            d.remove(id(1), clock.now()).unwrap();
            let removed = d.item(id(1)).unwrap();
            assert_eq!(removed.status(), ItemStatus::Removed);
            assert_eq!(removed.removed_at(), Some(ts(2_000)));
        }
    }

    #[test]
    fn illegal_transitions_are_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        let now = clock.now();
        for s in ALL {
            for a in ACTIONS {
                // By id on the day.
                let mut d = day_with(s, &clock);
                let before = d.clone();
                let result = day_action(&mut d, id(1), a, now);
                // And value-returning on the item, where it exists.
                let i = with_status(s, &clock);
                let item_result = match a {
                    Action::Pause => Some(i.pause(now).map(|_| ())),
                    Action::Finish => Some(i.finish(now).map(|_| ())),
                    Action::CarryOver => Some(i.carry_over().map(|_| ())),
                    Action::Remove => Some(i.remove(now).map(|_| ())),
                    Action::Start
                    | Action::Resume
                    | Action::Edit
                    | Action::Switch
                    | Action::Extend => None,
                };
                if let Some(r) = &item_result {
                    assert_eq!(r, &result, "{s} -> {a}: item and day agree");
                }
                if legal(s, a) {
                    assert!(result.is_ok(), "{s} -> {a} should be allowed");
                } else {
                    assert_eq!(
                        result,
                        Err(DomainError::InvalidTransition { from: s, action: a }),
                        "{s} -> {a}"
                    );
                    assert_eq!(d, before);
                    assert_eq!(i, with_status(s, &clock));
                }
            }
        }
    }

    #[test]
    fn every_day_transition_rejects_an_unknown_item_unchanged() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        for a in ACTIONS {
            let before = d.clone();
            assert_eq!(
                day_action(&mut d, id(9), a, clock.now()),
                Err(DomainError::UnknownItem(id(9))),
                "{a}"
            );
            assert_eq!(d, before);
        }
    }

    #[test]
    fn add_rejects_a_duplicate_id_unchanged() {
        let mut d = day(vec![item(1, "V")]);
        let before = d.clone();
        assert_eq!(d.add(item(1, "W")), Err(DomainError::DuplicateItem(id(1))));
        assert_eq!(d, before);
        assert_eq!(d.items().len(), 1);
    }

    #[test]
    fn add_rejects_a_second_open_segment_unchanged() {
        let clock = FakeClock::at(1_000);
        let mut other = day(vec![item(2, "W")]);
        other.start(id(2), clock.now()).unwrap();
        let running = other.item(id(2)).unwrap().clone();
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        let before = d.clone();
        assert_eq!(d.add(running.clone()), Err(DomainError::AnotherItemRunning));
        assert_eq!(d, before);
        // Without another open segment, it is accepted.
        let mut empty = day(vec![]);
        empty.add(running).unwrap();
        assert_eq!(open_segments(&empty), 1);
    }

    #[test]
    fn pause_and_finish_with_clock_behind_never_invert_a_segment() {
        for finish in [false, true] {
            let clock = FakeClock::at(1_000);
            let mut d = day_with(ItemStatus::Running, &clock);
            clock.set(990);
            if finish {
                d.finish(id(1), clock.now()).unwrap();
            } else {
                d.pause(id(1), clock.now()).unwrap();
            }
            let seg = &d.item(id(1)).unwrap().segments()[0];
            assert_eq!(seg.ended_at, Some(seg.started_at));
            assert_eq!(d.item(id(1)).unwrap().actual_secs(ts(2_000)), 0);
        }
    }

    #[test]
    fn elapsed_across_pause_and_resume_matches_elapsed_secs() {
        let clock = FakeClock::at(0);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        clock.set(10);
        d.pause(id(1), clock.now()).unwrap();
        clock.set(20);
        d.resume(id(1), clock.now()).unwrap();
        clock.set(25);
        d.pause(id(1), clock.now()).unwrap();
        clock.set(30);
        d.resume(id(1), clock.now()).unwrap();
        clock.set(33);
        let i = d.item(id(1)).unwrap();
        assert_eq!(i.actual_secs(clock.now()), 18);

        let running = i.running_view().unwrap();
        assert_eq!(running.item_id, id(1));
        assert_eq!(running.title, "item 1");
        assert_eq!(running.segment_started_at, ts(30));
        assert_eq!(running.accumulated_secs, 15);
        for t in [25, 30, 33, 100, 3_700] {
            assert_eq!(
                elapsed_secs(&running, ts(t)),
                i.actual_secs(ts(t)),
                "at {t}"
            );
        }
    }

    #[test]
    fn clock_behind_open_segment_counts_zero() {
        let clock = FakeClock::at(1_000);
        let mut d = day(vec![item(1, "V")]);
        d.start(id(1), clock.now()).unwrap();
        clock.set(990);
        assert_eq!(d.item(id(1)).unwrap().actual_secs(clock.now()), 0);
    }

    #[test]
    fn running_view_only_while_running() {
        let clock = FakeClock::at(1_000);
        for s in ALL {
            let i = with_status(s, &clock);
            assert_eq!(i.running_view().is_some(), s == ItemStatus::Running, "{s}");
        }
    }

    /// Every by-id edit, applied to item 1 of `d`.
    fn edit_all(d: &mut Day, id: ItemId) -> Vec<Result<(), DomainError>> {
        vec![
            d.set_fixed(id, ts(5_000)),
            d.set_flexible(id),
            d.set_order(id, key("k")),
            d.set_title(id, "renamed"),
            d.set_planned_secs(id, 600),
        ]
    }

    #[test]
    fn fixed_requires_a_pin_and_flexible_drops_it() {
        let mut d = day(vec![item(1, "V")]);
        d.set_fixed(id(1), ts(5_000)).unwrap();
        let i = d.item(id(1)).unwrap();
        assert!(i.fixed());
        assert_eq!(i.pinned_start(), Some(ts(5_000)));
        d.set_flexible(id(1)).unwrap();
        let i = d.item(id(1)).unwrap();
        assert!(!i.fixed());
        assert_eq!(i.pinned_start(), None);
    }

    #[test]
    fn edits_change_order_title_and_planned_length() {
        let mut d = day(vec![item(1, "V")]);
        d.set_order(id(1), key("k")).unwrap();
        d.set_title(id(1), "renamed").unwrap();
        d.set_planned_secs(id(1), 600).unwrap();
        let i = d.item(id(1)).unwrap();
        assert_eq!(i.order(), &key("k"));
        assert_eq!(i.title(), "renamed");
        assert_eq!(i.planned_secs(), 600);
        assert_eq!(i.id(), id(1));
    }

    #[test]
    fn edits_are_allowed_in_every_state_but_removed() {
        let clock = FakeClock::at(1_000);
        for s in ALL {
            let mut d = day_with(s, &clock);
            let before = d.clone();
            let results = edit_all(&mut d, id(1));
            if s == ItemStatus::Removed {
                for r in results {
                    assert_eq!(
                        r,
                        Err(DomainError::InvalidTransition {
                            from: ItemStatus::Removed,
                            action: Action::Edit,
                        })
                    );
                }
                assert_eq!(d, before);
            } else {
                assert!(results.iter().all(Result::is_ok), "{s}: {results:?}");
                let i = d.item(id(1)).unwrap();
                assert_eq!(i.status(), s, "edits never change status");
                assert_eq!(i.segments(), before.item(id(1)).unwrap().segments());
                assert_eq!(i.title(), "renamed");
                assert_eq!(i.planned_secs(), 600);
                assert_eq!(i.order(), &key("k"));
            }
        }
    }

    #[test]
    fn edits_on_a_running_item_keep_it_running() {
        let clock = FakeClock::at(1_000);
        let mut d = day_with(ItemStatus::Running, &clock);
        d.set_fixed(id(1), ts(900)).unwrap();
        d.set_title(id(1), "renamed").unwrap();
        d.set_planned_secs(id(1), 2_400).unwrap();
        d.set_order(id(1), key("k")).unwrap();
        let i = d.item(id(1)).unwrap();
        assert_eq!(i.status(), ItemStatus::Running);
        assert!(i.fixed());
        assert_eq!(open_segments(&d), 1);
        assert_eq!(i.running_view().unwrap().title, "renamed");
    }

    #[test]
    fn every_day_edit_rejects_an_unknown_item_unchanged() {
        let mut d = day(vec![item(1, "V")]);
        let before = d.clone();
        for r in edit_all(&mut d, id(9)) {
            assert_eq!(r, Err(DomainError::UnknownItem(id(9))));
        }
        assert_eq!(d, before);
    }

    #[test]
    fn schedule_puts_fixed_at_their_pins_flows_flexible_by_order_and_skips_removed() {
        let mut d = day(vec![
            item(1, "1"),
            item(3, "b"),
            item(5, "0V"),
            item(2, "2"),
            item(4, "a"),
        ]);
        d.set_fixed(id(1), ts(9_000)).unwrap();
        d.set_fixed(id(2), ts(3_000)).unwrap();
        d.remove(id(5), ts(0)).unwrap();
        let ids: Vec<ItemId> = reflow(&d, ts(0))
            .slots()
            .iter()
            .map(|s| s.item_id)
            .collect();
        assert_eq!(ids, vec![id(4), id(3), id(2), id(1)]);
        assert_eq!(d.items().len(), 5);
    }

    fn record(status: ItemStatus) -> PlanItemRecord {
        PlanItemRecord {
            id: id(1),
            title: "item 1".into(),
            planned_secs: 1500,
            order: key("V"),
            fixed: false,
            pinned_start: None,
            status,
            removed_at: None,
            segments: Vec::new(),
        }
    }

    fn assert_invalid(r: PlanItemRecord) {
        assert!(
            matches!(
                PlanItem::restore(r.clone()),
                Err(DomainError::InvalidRecord { id: i, .. }) if i == id(1)
            ),
            "{r:?}"
        );
    }

    #[test]
    fn restore_accepts_every_state_reached_by_transitions() {
        let clock = FakeClock::at(1_000);
        for s in ALL {
            let mut d = day_with(s, &clock);
            d.set_title(id(1), "t").ok();
            if s != ItemStatus::Removed {
                d.set_fixed(id(1), ts(5_000)).unwrap();
            }
            let i = d.item(id(1)).unwrap().clone();
            let r = PlanItemRecord {
                id: i.id(),
                title: i.title().into(),
                planned_secs: i.planned_secs(),
                order: i.order().clone(),
                fixed: i.fixed(),
                pinned_start: i.pinned_start(),
                status: i.status(),
                removed_at: i.removed_at(),
                segments: i.segments().to_vec(),
            };
            assert_eq!(PlanItem::restore(r), Ok(i), "{s}");
        }
    }

    #[test]
    fn restore_accepts_a_zero_length_closed_segment() {
        let mut r = record(ItemStatus::Paused);
        r.segments = vec![closed(10, 10)];
        assert!(PlanItem::restore(r).is_ok());
    }

    #[test]
    fn restore_rejects_fixed_without_a_pin_and_a_pin_without_fixed() {
        let mut r = record(ItemStatus::Pending);
        r.fixed = true;
        assert_invalid(r);
        let mut r = record(ItemStatus::Pending);
        r.pinned_start = Some(ts(5_000));
        assert_invalid(r);
    }

    #[test]
    fn restore_rejects_removed_status_and_tombstone_mismatch() {
        assert_invalid(record(ItemStatus::Removed));
        let mut r = record(ItemStatus::Done);
        r.removed_at = Some(ts(5_000));
        assert_invalid(r);
    }

    #[test]
    fn restore_rejects_an_inverted_closed_segment() {
        let mut r = record(ItemStatus::Done);
        r.segments = vec![closed(20, 10)];
        assert_invalid(r);
    }

    #[test]
    fn restore_rejects_two_open_segments() {
        let mut r = record(ItemStatus::Running);
        r.segments = vec![Segment::open(ts(10)), Segment::open(ts(20))];
        assert_invalid(r);
    }

    #[test]
    fn restore_requires_an_open_segment_exactly_when_running() {
        assert_invalid(record(ItemStatus::Running));
        let mut r = record(ItemStatus::Running);
        r.segments = vec![closed(0, 10)];
        assert_invalid(r);
        for s in [ItemStatus::Pending, ItemStatus::Paused, ItemStatus::Done] {
            let mut r = record(s);
            r.segments = vec![Segment::open(ts(10))];
            assert_invalid(r);
        }
    }

    #[test]
    fn invalid_record_display_names_the_id_not_the_title() {
        let mut r = record(ItemStatus::Running);
        r.title = "secret title".into();
        let msg = PlanItem::restore(r).unwrap_err().to_string();
        assert!(msg.contains(&id(1).to_string()), "{msg}");
        assert!(!msg.contains("secret"), "{msg}");
    }

    #[test]
    fn day_restore_builds_through_add() {
        let date = jiff::civil::date(2026, 11, 10);
        let d = Day::restore(date, "Europe/Berlin", vec![item(1, "V"), item(2, "W")]).unwrap();
        assert_eq!(d.date(), date);
        assert_eq!(d.tz(), "Europe/Berlin");
        assert_eq!(d.items().len(), 2);
        assert_eq!(
            Day::restore(date, "UTC", vec![item(1, "V"), item(1, "W")]),
            Err(DomainError::DuplicateItem(id(1)))
        );
        let clock = FakeClock::at(1_000);
        let mut a = day(vec![item(1, "V")]);
        a.start(id(1), clock.now()).unwrap();
        let mut b = day(vec![item(2, "W")]);
        b.start(id(2), clock.now()).unwrap();
        let running = [
            a.item(id(1)).unwrap().clone(),
            b.item(id(2)).unwrap().clone(),
        ];
        assert_eq!(
            Day::restore(date, "UTC", running),
            Err(DomainError::AnotherItemRunning)
        );
    }

    fn next_id(d: &Day) -> Option<ItemId> {
        d.next_item(ts(0)).map(PlanItem::id)
    }

    #[test]
    fn next_item_is_the_first_pending_after_done_fixed() {
        let t = ts(0);
        let mut d = day(vec![item(1, "a"), item(2, "b"), item(3, "c")]);
        d.set_fixed(id(1), ts(100)).unwrap();
        d.start(id(1), t).unwrap();
        d.finish(id(1), t).unwrap();
        assert_eq!(next_id(&d), Some(id(2)));
        assert_eq!(d.next_item(ts(0)).unwrap().title(), "item 2");
    }

    #[test]
    fn next_item_counts_a_paused_item() {
        let t = ts(0);
        let mut d = day(vec![item(1, "a"), item(2, "b")]);
        d.start(id(1), t).unwrap();
        d.pause(id(1), t).unwrap();
        assert_eq!(next_id(&d), Some(id(1)));
    }

    #[test]
    fn next_item_skips_done_carried_over_removed_and_running() {
        let t = ts(0);
        let mut d = day(vec![
            item(1, "a"),
            item(2, "b"),
            item(3, "c"),
            item(4, "d"),
            item(5, "e"),
        ]);
        d.start(id(1), t).unwrap();
        d.finish(id(1), t).unwrap();
        d.carry_over(id(2)).unwrap();
        d.remove(id(3), t).unwrap();
        d.start(id(4), t).unwrap();
        assert_eq!(next_id(&d), Some(id(5)));
    }

    #[test]
    fn next_item_puts_a_past_pin_first_and_a_future_pin_after_a_flexible_item_that_fits() {
        let mut d = day(vec![item(1, "a"), item(2, "z")]);
        d.set_fixed(id(2), ts(5_000)).unwrap();
        // Item 1 (1500 s) fits before the pin at 5000.
        assert_eq!(next_id(&d), Some(id(1)));
        assert_eq!(d.next_item(ts(6_000)).map(PlanItem::id), Some(id(2)));
    }

    #[test]
    fn next_item_is_none_when_nothing_is_left() {
        assert_eq!(next_id(&day(vec![])), None);
        let t = ts(0);
        let mut d = day(vec![item(1, "a"), item(2, "b")]);
        d.start(id(1), t).unwrap();
        d.finish(id(1), t).unwrap();
        d.remove(id(2), t).unwrap();
        assert_eq!(next_id(&d), None);
    }

    #[test]
    fn switch_finishes_the_running_item_and_starts_the_next() {
        let mut d = day(vec![item(1, "a"), item(2, "b"), item(3, "c")]);
        d.start(id(1), ts(0)).unwrap();
        assert_eq!(d.switch(id(1), ts(60)), Ok(Some(id(2))));
        let a = d.item(id(1)).unwrap();
        assert_eq!(a.status(), ItemStatus::Done);
        assert_eq!(a.segments(), &[closed(0, 60)]);
        let b = d.item(id(2)).unwrap();
        assert_eq!(b.status(), ItemStatus::Running);
        assert_eq!(b.segments(), &[Segment::open(ts(60))]);
        assert_eq!(d.item(id(3)).unwrap().status(), ItemStatus::Pending);
        assert_eq!(open_segments(&d), 1);
    }

    #[test]
    fn switch_resumes_a_paused_next_item() {
        let mut d = day(vec![item(1, "a"), item(2, "b")]);
        d.start(id(1), ts(0)).unwrap();
        d.pause(id(1), ts(10)).unwrap();
        d.start(id(2), ts(20)).unwrap();
        assert_eq!(d.switch(id(2), ts(30)), Ok(Some(id(1))));
        let a = d.item(id(1)).unwrap();
        assert_eq!(a.status(), ItemStatus::Running);
        assert_eq!(a.segments(), &[closed(0, 10), Segment::open(ts(30))]);
        assert_eq!(d.item(id(2)).unwrap().status(), ItemStatus::Done);
    }

    #[test]
    fn switch_with_nothing_left_only_finishes() {
        let mut d = day(vec![item(1, "a"), item(2, "b")]);
        d.remove(id(2), ts(0)).unwrap();
        d.start(id(1), ts(0)).unwrap();
        assert_eq!(d.switch(id(1), ts(5)), Ok(None));
        assert_eq!(d.item(id(1)).unwrap().status(), ItemStatus::Done);
        assert_eq!(open_segments(&d), 0);
    }

    #[test]
    fn switch_from_a_non_running_or_unknown_item_is_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        for s in ALL.into_iter().filter(|s| *s != ItemStatus::Running) {
            let mut d = day_with(s, &clock);
            d.add(item(2, "b")).unwrap();
            let before = d.clone();
            assert_eq!(
                d.switch(id(1), clock.now()),
                Err(DomainError::InvalidTransition {
                    from: s,
                    action: Action::Switch
                }),
                "{s}"
            );
            assert_eq!(d, before);
        }
        let mut d = day_with(ItemStatus::Running, &clock);
        let before = d.clone();
        assert_eq!(
            d.switch(id(9), clock.now()),
            Err(DomainError::UnknownItem(id(9)))
        );
        assert_eq!(d, before);
    }

    #[test]
    fn extend_adds_to_the_running_items_planned_length() {
        let clock = FakeClock::at(1_000);
        let mut d = day_with(ItemStatus::Running, &clock);
        d.add(item(2, "b")).unwrap();
        let before = d.item(id(1)).unwrap().clone();
        assert_eq!(d.extend(id(1), 300), Ok(1800));
        let after = d.item(id(1)).unwrap();
        assert_eq!(after.planned_secs(), 1800);
        assert_eq!(after.status(), ItemStatus::Running);
        assert_eq!(after.segments(), before.segments());
        assert_eq!(d.item(id(2)).unwrap().planned_secs(), 1500);
    }

    #[test]
    fn extend_a_non_running_or_unknown_item_is_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        for s in ALL.into_iter().filter(|s| *s != ItemStatus::Running) {
            let mut d = day_with(s, &clock);
            let before = d.clone();
            assert_eq!(
                d.extend(id(1), 300),
                Err(DomainError::InvalidTransition {
                    from: s,
                    action: Action::Extend
                }),
                "{s}"
            );
            assert_eq!(d, before);
        }
        let mut d = day_with(ItemStatus::Running, &clock);
        let before = d.clone();
        assert_eq!(d.extend(id(9), 300), Err(DomainError::UnknownItem(id(9))));
        assert_eq!(d, before);
    }

    #[test]
    fn extend_past_u64_is_rejected_unchanged() {
        let clock = FakeClock::at(1_000);
        let mut d = day_with(ItemStatus::Running, &clock);
        let before = d.clone();
        assert_eq!(
            d.extend(id(1), u64::MAX),
            Err(DomainError::InvalidTransition {
                from: ItemStatus::Running,
                action: Action::Extend
            })
        );
        assert_eq!(d, before);
        assert_eq!(Action::Extend.to_string(), "extend");
    }
}
