//! nguda domain: pure rules and port traits.
//!
//! No I/O, no Tauri, no SQLite, and no system clock: "now" always comes in through
//! the [`Clock`] port (AD-1, AD-8).

mod clock;
mod elapsed;
mod error;
mod order;
mod plan;
mod segment;
#[cfg(test)]
mod test_support;

pub use clock::Clock;
pub use elapsed::{Running, elapsed_secs, format_elapsed, until_next_second};
pub use error::DomainError;
pub use jiff::Timestamp;
pub use jiff::civil::Date;
pub use order::OrderKey;
pub use plan::{Action, Day, ItemId, ItemStatus, PlanItem, display_order};
pub use segment::{Segment, actual_secs, closed_secs};
pub use uuid::Uuid;
