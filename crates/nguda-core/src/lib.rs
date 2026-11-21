//! nguda domain: pure rules and port traits.
//!
//! No I/O, no Tauri, no SQLite, and no system clock: "now" always comes in through
//! the [`Clock`] port (AD-1, AD-8).

mod baseline;
mod calendar;
mod clock;
mod day_log;
mod elapsed;
mod error;
mod export;
mod gap;
mod ids;
mod next_task;
mod notifier;
mod order;
mod plan;
mod random;
mod reflow;
mod report;
mod secrets;
mod segment;
mod settings;
mod store;
mod tags;
#[cfg(test)]
mod test_support;

pub use baseline::{Baseline, BaselineItem, LockChoice, LockDecision, baseline};
pub use calendar::{
    CalendarChange, CalendarEntry, CalendarError, CalendarErrorKind, CalendarFuture, CalendarInfo,
    CalendarSource, EventKey, FetchedEvent, OAuthClient, SessionStore, SyncBatch, SyncCursor,
    add_event, added_refs, hide_calendar, in_plan, merge, reconcile,
};
pub use clock::{Clock, SleepFuture};
pub use day_log::{DayLogEntry, DayLogKind};
pub use elapsed::{Running, elapsed_secs, format_elapsed, until_next_second};
pub use error::DomainError;
pub use export::{CsvExport, ExportRange, FileSink, FileSinkError, export_csv};
pub use gap::{GapFigure, IsoWeek, TagGap, WeekReport, week_report};
pub use ids::{IdGen, PromptId};
pub use jiff::Timestamp;
pub use jiff::civil::Date;
pub use jiff::tz::TimeZone;
pub use next_task::next_task;
pub use notifier::{Notification, Notifier, NotifierError, PromptAction, PromptKind};
pub use order::OrderKey;
pub use plan::{Action, CalendarRef, Day, ItemId, ItemStatus, PlanItem, PlanItemRecord};
pub use random::Random;
pub use reflow::{Schedule, Slot, SlotKind, Span, reflow};
pub use report::{ActualSegment, DayReport, PlannedSource, ReportRow, report};
pub use secrets::{SecretStore, SecretStoreError};
pub use segment::{Segment, actual_secs, closed_secs};
pub use settings::{ParseWorkEndError, Settings, WorkEnd};
pub use store::{OpenSegmentRef, Store, StoreBatch, StoreError, StoreFuture, StoreOp};
pub use tags::{merge_tag_names, normalize_tags, sort_tag_names};
pub use uuid::Uuid;
