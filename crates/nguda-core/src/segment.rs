//! Time segments and the one actual-time function (AD-6).

use jiff::Timestamp;

/// A stretch of work on one item. Opened with `ended_at = None`, closed exactly once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

impl Segment {
    pub fn open(started_at: Timestamp) -> Self {
        Self {
            started_at,
            ended_at: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// Whole seconds from `from` to `to`, or 0 when `to` is earlier. Truncates like
/// [`crate::elapsed_secs`] so both formulas agree.
fn whole_secs(from: Timestamp, to: Timestamp) -> u64 {
    u64::try_from(to.duration_since(from).as_secs()).unwrap_or(0)
}

/// Sum of the closed segments only.
pub fn closed_secs(segments: &[Segment]) -> u64 {
    segments
        .iter()
        .filter_map(|s| s.ended_at.map(|end| whole_secs(s.started_at, end)))
        .sum()
}

/// Actual time as of `now`: closed segments plus any open one. A clock behind the
/// open segment's start counts that segment as 0.
pub fn actual_secs(segments: &[Segment], now: Timestamp) -> u64 {
    let open: u64 = segments
        .iter()
        .filter(|s| s.is_open())
        .map(|s| whole_secs(s.started_at, now))
        .sum();
    closed_secs(segments) + open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{closed, ts};

    #[test]
    fn elapsed_across_pause_and_resume() {
        let segs = [closed(0, 10), closed(20, 25), Segment::open(ts(30))];
        assert_eq!(actual_secs(&segs, ts(33)), 18);
        assert_eq!(closed_secs(&segs), 15);
    }

    #[test]
    fn clock_behind_open_segment_counts_zero() {
        let segs = [closed(0, 10), Segment::open(ts(100))];
        assert_eq!(actual_secs(&segs, ts(90)), 10);
    }

    #[test]
    fn no_segments_is_zero() {
        assert_eq!(actual_secs(&[], ts(5)), 0);
    }
}
