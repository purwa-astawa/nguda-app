//! nguda domain: pure rules and port traits.
//!
//! No I/O, no Tauri, no SQLite, and no system clock: "now" always comes in through
//! the [`Clock`] port (AD-1, AD-8).

mod clock;
mod elapsed;

pub use clock::Clock;
pub use elapsed::{Running, elapsed_secs, format_elapsed, until_next_second};
pub use jiff::Timestamp;
