//! User settings kept by the engine (Config convention): the work-end time of day.
//!
//! Core has no tzdb, so a zone always comes in as a [`TimeZone`], never by name.

use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;

const MINUTES_PER_DAY: u16 = 24 * 60;

/// The local time of day the user's work ends, in whole minutes after midnight,
/// `0..=1439`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkEnd(u16);

impl WorkEnd {
    /// 17:00, used when nothing is stored.
    pub const DEFAULT: Self = Self(17 * 60);

    /// `minutes` after midnight, or `None` outside `0..=1439`.
    pub fn from_minutes(minutes: u32) -> Option<Self> {
        u16::try_from(minutes)
            .ok()
            .filter(|m| *m < MINUTES_PER_DAY)
            .map(Self)
    }

    pub fn minutes(self) -> u32 {
        u32::from(self.0)
    }

    fn hour(self) -> i8 {
        // Below 24 by construction.
        i8::try_from(self.0 / 60).unwrap_or(0)
    }

    fn minute(self) -> i8 {
        i8::try_from(self.0 % 60).unwrap_or(0)
    }

    /// The instant this time of day falls on `date` in `tz`. A DST gap or fold is
    /// resolved compatibly (the later instant in a gap, the earlier in a fold), and
    /// an instant outside the supported range saturates.
    pub fn at(self, date: Date, tz: &TimeZone) -> Timestamp {
        let saturated = if date.year() < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        };
        let Ok(time) = Time::new(self.hour(), self.minute(), 0, 0) else {
            return saturated;
        };
        tz.to_ambiguous_timestamp(date.to_datetime(time))
            .compatible()
            .unwrap_or(saturated)
    }
}

impl Default for WorkEnd {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `HH:MM`, zero-padded.
impl fmt::Display for WorkEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// A string that is not a valid `HH:MM` work-end time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseWorkEndError;

impl fmt::Display for ParseWorkEndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("work-end must be HH:MM between 00:00 and 23:59")
    }
}

impl std::error::Error for ParseWorkEndError {}

/// Parses exactly `HH:MM` (two digits each), as [`Display`](fmt::Display) writes it.
impl FromStr for WorkEnd {
    type Err = ParseWorkEndError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (h, m) = s.split_once(':').ok_or(ParseWorkEndError)?;
        let two_digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
        if !two_digits(h) || !two_digits(m) {
            return Err(ParseWorkEndError);
        }
        let h: u32 = h.parse().map_err(|_| ParseWorkEndError)?;
        let m: u32 = m.parse().map_err(|_| ParseWorkEndError)?;
        if h > 23 || m > 59 {
            return Err(ParseWorkEndError);
        }
        Self::from_minutes(h * 60 + m).ok_or(ParseWorkEndError)
    }
}

/// Every user setting, each with its default when nothing is stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub work_end: WorkEnd,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ts;

    #[test]
    fn minutes_range_is_0_to_1439() {
        assert_eq!(WorkEnd::from_minutes(0).map(WorkEnd::minutes), Some(0));
        assert_eq!(
            WorkEnd::from_minutes(1439).map(WorkEnd::minutes),
            Some(1439)
        );
        assert_eq!(WorkEnd::from_minutes(1440), None);
        assert_eq!(WorkEnd::from_minutes(u32::MAX), None);
    }

    #[test]
    fn default_is_17_00() {
        assert_eq!(WorkEnd::default(), WorkEnd::DEFAULT);
        assert_eq!(WorkEnd::DEFAULT.minutes(), 17 * 60);
        assert_eq!(Settings::default().work_end, WorkEnd::DEFAULT);
    }

    #[test]
    fn displays_and_parses_hh_mm() {
        let we = |m| WorkEnd::from_minutes(m).unwrap();
        assert_eq!(we(0).to_string(), "00:00");
        assert_eq!(we(17 * 60).to_string(), "17:00");
        assert_eq!(we(18 * 60 + 5).to_string(), "18:05");
        assert_eq!(we(1439).to_string(), "23:59");
        for m in [0, 61, 17 * 60, 1439] {
            assert_eq!(we(m).to_string().parse::<WorkEnd>(), Ok(we(m)));
        }
        for bad in [
            "", "17", "7:00", "17:0", "24:00", "12:60", "ab:cd", "17:00:00", "+1:00",
        ] {
            assert_eq!(bad.parse::<WorkEnd>(), Err(ParseWorkEndError), "{bad:?}");
        }
    }

    #[test]
    fn at_gives_the_instant_on_the_date_in_the_zone() {
        let date = Date::constant(1970, 1, 1);
        assert_eq!(WorkEnd::DEFAULT.at(date, &TimeZone::UTC), ts(61_200));
        // UTC+2: 17:00 local is 15:00 UTC.
        let plus2 = TimeZone::fixed(jiff::tz::offset(2));
        assert_eq!(WorkEnd::DEFAULT.at(date, &plus2), ts(61_200 - 7_200));
        let next = Date::constant(1970, 1, 2);
        assert_eq!(
            WorkEnd::from_minutes(0).unwrap().at(next, &TimeZone::UTC),
            ts(86_400)
        );
    }

    #[test]
    fn at_saturates_at_the_edges_of_the_range() {
        let last = WorkEnd::from_minutes(1439).unwrap();
        // 23:59 on the last date, 25 h behind UTC, is past Timestamp::MAX.
        let east = TimeZone::fixed(jiff::tz::offset(-25));
        assert_eq!(last.at(Date::MAX, &east), Timestamp::MAX);
        let first = WorkEnd::from_minutes(0).unwrap();
        let west = TimeZone::fixed(jiff::tz::offset(25));
        assert_eq!(first.at(Date::MIN, &west), Timestamp::MIN);
    }
}
