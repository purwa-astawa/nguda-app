//! nguda adapters: implementations of the `nguda-core` ports. Never depends on Tauri.

mod store;

use nguda_core::{Clock, IdGen, ItemId, SleepFuture, Timestamp, Uuid};

pub use store::{OpenError, SqliteStore};

/// The real wall clock behind the [`Clock`] port.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    /// Sleeps on the tokio timer for `deadline - now`, so it must be polled inside a
    /// tokio runtime. A deadline already passed completes at once.
    fn sleep_until(&self, deadline: Timestamp) -> SleepFuture {
        let wait =
            std::time::Duration::try_from(deadline.duration_since(self.now())).unwrap_or_default();
        Box::pin(async move { tokio::time::sleep(wait).await })
    }
}

/// UUID v7 ids, time-ordered (Consistency Conventions: IDs).
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7Ids;

impl IdGen for UuidV7Ids {
    fn new_item_id(&self) -> ItemId {
        ItemId(Uuid::now_v7())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleep_until_a_past_deadline_returns_at_once() {
        let past = Timestamp::now() - std::time::Duration::from_secs(5);
        SystemClock.sleep_until(past).await;
    }

    #[tokio::test]
    async fn sleep_until_waits_for_a_near_deadline() {
        let start = Timestamp::now();
        SystemClock
            .sleep_until(start + std::time::Duration::from_millis(30))
            .await;
        assert!(Timestamp::now().duration_since(start).as_millis() >= 25);
    }

    #[test]
    fn uuid_v7_ids_are_v7_and_distinct() {
        let (a, b) = (UuidV7Ids.new_item_id(), UuidV7Ids.new_item_id());
        assert_ne!(a, b);
        assert_eq!(a.0.get_version_num(), 7);
    }
}
