//! nguda adapters: implementations of the `nguda-core` ports. Never depends on Tauri.

mod store;

use nguda_core::{Clock, Timestamp};

pub use store::{OpenError, SqliteStore};

/// The real wall clock behind the [`Clock`] port.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}
