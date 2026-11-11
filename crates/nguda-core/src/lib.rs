//! nguda domain: pure rules and port traits.
//!
//! No I/O, no Tauri, no SQLite, and no system clock: "now" always comes in through
//! the [`Clock`] port (AD-1, AD-8).

mod clock;
mod day_log;
mod elapsed;
mod error;
mod ids;
mod next_task;
mod notifier;
mod order;
mod plan;
mod reflow;
mod segment;
mod store;
#[cfg(test)]
mod test_support;

pub use clock::{Clock, SleepFuture};
pub use day_log::{DayLogEntry, DayLogKind};
pub use elapsed::{Running, elapsed_secs, format_elapsed, until_next_second};
pub use error::DomainError;
pub use ids::{IdGen, PromptId};
pub use jiff::Timestamp;
pub use jiff::civil::Date;
pub use jiff::tz::TimeZone;
pub use next_task::next_task;
pub use notifier::{Notification, Notifier, NotifierError, PromptKind};
pub use order::OrderKey;
pub use plan::{Action, Day, ItemId, ItemStatus, PlanItem, PlanItemRecord};
pub use reflow::{Schedule, Slot, SlotKind, Span, reflow};
pub use segment::{Segment, actual_secs, closed_secs};
pub use store::{OpenSegmentRef, Store, StoreBatch, StoreError, StoreFuture, StoreOp};
pub use uuid::Uuid;
