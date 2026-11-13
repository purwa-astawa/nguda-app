//! The one elapsed-time formula and its display format (AD-11).

use std::time::Duration;

use jiff::Timestamp;

use crate::plan::ItemId;

/// The running task: time already banked plus the open segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub item_id: ItemId,
    pub title: String,
    pub segment_started_at: Timestamp,
    pub accumulated_secs: u64,
}

/// Whole seconds spent on `running` as of `now`. A clock that reads earlier than the
/// segment start counts the open segment as zero.
pub fn elapsed_secs(running: &Running, now: Timestamp) -> u64 {
    let open = now.duration_since(running.segment_started_at).as_secs();
    running.accumulated_secs + u64::try_from(open).unwrap_or(0)
}

/// `MM:SS` below one hour, `H:MM:SS` from one hour on.
pub fn format_elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Time from `now` until the elapsed value next increments, aligned to the segment's
/// start second, so a renderer that waits this long sees each second exactly once.
pub fn until_next_second(running: &Running, now: Timestamp) -> Duration {
    let open_ms = now.duration_since(running.segment_started_at).as_millis();
    if open_ms < 0 {
        return Duration::from_millis(u64::try_from(-open_ms).unwrap_or(u64::MAX));
    }
    Duration::from_millis(1000 - (open_ms % 1000) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::id;

    fn at(secs: i64, ms: i64) -> Timestamp {
        Timestamp::from_millisecond(secs * 1000 + ms).unwrap()
    }

    fn running(started: Timestamp, accumulated_secs: u64) -> Running {
        Running {
            item_id: id(1),
            title: "Demo".into(),
            segment_started_at: started,
            accumulated_secs,
        }
    }

    #[test]
    fn elapsed_counts_whole_seconds_of_the_open_segment() {
        let r = running(at(1_000, 0), 0);
        assert_eq!(elapsed_secs(&r, at(1_000, 0)), 0);
        assert_eq!(elapsed_secs(&r, at(1_000, 999)), 0);
        assert_eq!(elapsed_secs(&r, at(1_005, 0)), 5);
        assert_eq!(elapsed_secs(&r, at(4_600, 0)), 3600);
    }

    #[test]
    fn elapsed_adds_accumulated_time() {
        let r = running(at(1_000, 0), 90);
        assert_eq!(elapsed_secs(&r, at(1_010, 500)), 100);
    }

    #[test]
    fn elapsed_ignores_a_clock_behind_the_segment_start() {
        let r = running(at(1_000, 0), 7);
        assert_eq!(elapsed_secs(&r, at(990, 0)), 7);
    }

    #[test]
    fn format_switches_to_hours_at_one_hour() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(5), "00:05");
        assert_eq!(format_elapsed(59 * 60 + 59), "59:59");
        assert_eq!(format_elapsed(3600), "1:00:00");
        assert_eq!(format_elapsed(10 * 3600 + 61), "10:01:01");
    }

    #[test]
    fn next_second_is_aligned_to_the_segment_start() {
        let r = running(at(1_000, 250), 0);
        assert_eq!(
            until_next_second(&r, at(1_000, 250)),
            Duration::from_millis(1000)
        );
        assert_eq!(
            until_next_second(&r, at(1_003, 0)),
            Duration::from_millis(250)
        );
        assert_eq!(
            until_next_second(&r, at(1_003, 249)),
            Duration::from_millis(1)
        );
        assert_eq!(
            until_next_second(&r, at(999, 250)),
            Duration::from_millis(1000)
        );
    }

    /// One case of the Rust/TS conformance fixture shared with `test/elapsed.test.ts`.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        name: String,
        segment_started_at: String,
        accumulated_secs: u64,
        now: String,
        elapsed_secs: u64,
        formatted: String,
    }

    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/elapsed.json"
    ));

    #[test]
    fn conformance_fixture_matches() {
        let cases: Vec<Case> = serde_json::from_str(FIXTURE).unwrap();
        assert!(cases.len() >= 12, "fixture has too few cases");
        for c in cases {
            let r = running(c.segment_started_at.parse().unwrap(), c.accumulated_secs);
            let now: Timestamp = c.now.parse().unwrap();
            let secs = elapsed_secs(&r, now);
            assert_eq!(secs, c.elapsed_secs, "{}", c.name);
            assert_eq!(format_elapsed(secs), c.formatted, "{}", c.name);
        }
    }
}
