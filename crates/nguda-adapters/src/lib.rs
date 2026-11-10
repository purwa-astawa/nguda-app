//! nguda adapters: implementations of the `nguda-core` ports. Never depends on Tauri.

use nguda_core::{Clock, Timestamp};

/// The real wall clock behind the [`Clock`] port.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}
