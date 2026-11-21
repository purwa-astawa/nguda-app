//! nguda adapters: implementations of the `nguda-core` ports. Never depends on Tauri.

mod file_sink;
mod gcal;
mod notifier;
mod secrets;
mod store;

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use nguda_core::{Clock, IdGen, ItemId, PromptId, Random, SleepFuture, Timestamp, Uuid};

pub use file_sink::FsFileSink;
pub use gcal::{GcalConfig, GcalSource, OpenUrl, REFRESH_TOKEN_KEY, SCOPES};
pub use notifier::{ActionCallback, SystemNotifier};
pub use secrets::KeyringSecretStore;
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

    fn new_prompt_id(&self) -> PromptId {
        PromptId(Uuid::now_v7())
    }
}

/// Uniform randomness for the engine's poll jitter, from std's randomly keyed
/// SipHash (`RandomState`) over a counter: no RNG crate. Not for secrets.
#[derive(Debug, Default)]
pub struct StdRandom {
    counter: AtomicU64,
}

impl Random for StdRandom {
    fn unit(&self) -> f64 {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(n);
        // The top 53 bits fill an f64 mantissa exactly, so the result is in [0, 1).
        let bits = hasher.finish() >> 11;
        #[allow(clippy::cast_precision_loss)]
        let unit = bits as f64 / (1u64 << 53) as f64;
        unit
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
    fn std_random_stays_in_the_unit_interval_and_varies() {
        let random = StdRandom::default();
        let draws: Vec<f64> = (0..1_000).map(|_| random.unit()).collect();
        assert!(draws.iter().all(|u| (0.0..1.0).contains(u)));
        let low = draws.iter().filter(|u| **u < 0.5).count();
        assert!((300..700).contains(&low), "{low}");
    }

    #[test]
    fn uuid_v7_ids_are_v7_and_distinct() {
        let (a, b) = (UuidV7Ids.new_item_id(), UuidV7Ids.new_item_id());
        assert_ne!(a, b);
        assert_eq!(a.0.get_version_num(), 7);
        let (p, q) = (UuidV7Ids.new_prompt_id(), UuidV7Ids.new_prompt_id());
        assert_ne!(p, q);
        assert_eq!(p.0.get_version_num(), 7);
    }
}
