//! The weekly estimate gap (AD-6, CAP-8): Σ actual ÷ Σ baseline planned.
//!
//! The gap folds [`DayReport`] rows, so "planned", "actual" and the exclusions
//! mean exactly what they mean in the day report (AD-16). A row counts only when
//! its planned value is the baseline ([`PlannedSource::Baseline`]) and it was
//! neither carried over nor removed. Unlocked days and unplanned rows add nothing.
//! An item counts once overall and once in each of its tags. Pure.
//!
//! A week is an ISO week of local civil dates, Monday to Sunday ([`IsoWeek`]), so
//! DST never changes which days it holds; durations are elapsed seconds.

use std::collections::BTreeMap;

use jiff::ToSpan;
use jiff::civil::{Date, ISOWeekDate, Weekday};

use crate::baseline::LockDecision;
use crate::plan::ItemStatus;
use crate::report::{DayReport, PlannedSource, ReportRow};
use crate::tags::sort_tag_names;

/// An ISO week (Monday to Sunday) whose seven dates all exist in jiff's range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IsoWeek {
    year: i16,
    week: i8,
}

impl IsoWeek {
    /// The week `week` of ISO year `year`, or `None` when that week does not exist
    /// (week 0, week 53 of a short year) or does not fit wholly in jiff's range.
    pub fn new(year: i16, week: i8) -> Option<Self> {
        ISOWeekDate::new(year, week, Weekday::Monday).ok()?;
        ISOWeekDate::new(year, week, Weekday::Sunday).ok()?;
        Some(Self { year, week })
    }

    /// The week holding `date`, or `None` for the last days of jiff's range,
    /// whose week runs past it.
    pub fn containing(date: Date) -> Option<Self> {
        let w = date.iso_week_date();
        Self::new(w.year(), w.week())
    }

    /// The ISO year, which can differ from the calendar year of some of its dates.
    pub fn year(self) -> i16 {
        self.year
    }

    /// The week number, `1..=53`.
    pub fn week(self) -> i8 {
        self.week
    }

    /// The Monday of the week.
    pub fn start(self) -> Date {
        self.date(Weekday::Monday)
    }

    /// The Sunday of the week.
    pub fn end(self) -> Date {
        self.date(Weekday::Sunday)
    }

    /// The seven dates of the week, Monday first.
    pub fn days(self) -> [Date; 7] {
        [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ]
        .map(|d| self.date(d))
    }

    /// Whether `date` is one of the week's dates.
    pub fn contains(self, date: Date) -> bool {
        (self.start()..=self.end()).contains(&date)
    }

    /// The week before, or `None` at the start of jiff's range.
    pub fn previous(self) -> Option<Self> {
        Self::containing(self.start().checked_sub(1.day()).ok()?)
    }

    /// The week after, or `None` at the end of jiff's range.
    pub fn next(self) -> Option<Self> {
        Self::containing(self.end().checked_add(1.day()).ok()?)
    }

    /// `new` checked the Monday and the Sunday exist, so every weekday between
    /// does too; this cannot fail, and falls back to the Monday rather than
    /// panic.
    fn date(self, weekday: Weekday) -> Date {
        ISOWeekDate::new(self.year, self.week, weekday)
            .or_else(|_| ISOWeekDate::new(self.year, self.week, Weekday::Monday))
            .map_or(Date::MIN, ISOWeekDate::date)
    }
}

/// The sums behind one gap figure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GapFigure {
    /// The rows counted.
    pub items: u32,
    pub actual_secs: u64,
    pub planned_secs: u64,
}

impl GapFigure {
    /// `actual_secs ÷ planned_secs`, or `None` when nothing was planned.
    pub fn ratio(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        (self.planned_secs > 0).then(|| self.actual_secs as f64 / self.planned_secs as f64)
    }

    fn add(&mut self, actual_secs: u64, planned_secs: u64) {
        self.items = self.items.saturating_add(1);
        self.actual_secs = self.actual_secs.saturating_add(actual_secs);
        self.planned_secs = self.planned_secs.saturating_add(planned_secs);
    }
}

/// One tag's gap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagGap {
    pub tag: String,
    pub figure: GapFigure,
}

/// A week's estimate gap (AD-16 `QueryWeek`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeekReport {
    pub week: IsoWeek,
    pub overall: GapFigure,
    /// Every tag of a counted row, sorted by [`sort_tag_names`].
    pub tags: Vec<TagGap>,
    /// The locked days of the week that have a report, ascending.
    pub counted_days: Vec<Date>,
    /// The undecided or `NoLock` days of the week that have a report, ascending.
    pub unlocked_days: Vec<Date>,
}

/// The gap of `week` from its days' `reports`, in any order. Reports dated
/// outside the week are ignored. Each date must appear in at most one report:
/// a duplicate would be counted twice. Pure.
pub fn week_report(week: IsoWeek, reports: &[DayReport]) -> WeekReport {
    let mut overall = GapFigure::default();
    let mut tags: BTreeMap<&str, GapFigure> = BTreeMap::new();
    let mut counted_days = Vec::new();
    let mut unlocked_days = Vec::new();
    for report in reports.iter().filter(|r| week.contains(r.date)) {
        if report.lock_decision != LockDecision::Locked {
            unlocked_days.push(report.date);
            continue;
        }
        counted_days.push(report.date);
        for (row, planned) in report.rows.iter().filter_map(counted) {
            overall.add(row.actual_secs, planned);
            let mut seen: Vec<&str> = Vec::new();
            for tag in &row.tags {
                if !seen.contains(&tag.as_str()) {
                    seen.push(tag);
                    tags.entry(tag).or_default().add(row.actual_secs, planned);
                }
            }
        }
    }
    for days in [&mut counted_days, &mut unlocked_days] {
        days.sort_unstable();
    }
    let mut names: Vec<String> = tags.keys().map(|t| (*t).to_owned()).collect();
    sort_tag_names(&mut names);
    let tags = names
        .into_iter()
        .map(|tag| {
            let figure = tags[tag.as_str()];
            TagGap { tag, figure }
        })
        .collect();
    WeekReport {
        week,
        overall,
        tags,
        counted_days,
        unlocked_days,
    }
}

/// A row that counts in the gap, with its baseline planned seconds.
fn counted(row: &ReportRow) -> Option<(&ReportRow, u64)> {
    let excluded = matches!(row.status, ItemStatus::CarriedOver | ItemStatus::Removed);
    match (row.planned_source, row.planned_secs) {
        (PlannedSource::Baseline, Some(planned)) if !excluded => Some((row, planned)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::baseline::{Baseline, BaselineItem};
    use crate::plan::{Day, PlanItem, PlanItemRecord};
    use crate::report::report;
    use crate::segment::Segment;
    use crate::test_support::{closed, id, key, ts};

    fn record(n: u128, planned_secs: u64, tags: &[&str]) -> PlanItemRecord {
        PlanItemRecord {
            id: id(n),
            title: format!("item {n}"),
            planned_secs,
            order: key(&format!("a{n}")),
            fixed: false,
            pinned_start: None,
            status: ItemStatus::Done,
            removed_at: None,
            segments: Vec::new(),
            carried_from: None,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            source: None,
            removed_by_source_at: None,
        }
    }

    /// A record of item `n`, done, whose actual time is `actual` seconds.
    fn worked(n: u128, planned_secs: u64, actual: i64, tags: &[&str]) -> PlanItemRecord {
        PlanItemRecord {
            segments: vec![closed(1_000, 1_000 + actual)],
            ..record(n, planned_secs, tags)
        }
    }

    /// The report of `date` with `records`, `baselined` the `(item, planned)` pairs
    /// frozen at lock (used only when `decision` is `Locked`).
    fn day_report(
        date: Date,
        decision: LockDecision,
        records: Vec<PlanItemRecord>,
        baselined: &[(u128, u64)],
        now: i64,
    ) -> DayReport {
        let items = records.into_iter().map(|r| PlanItem::restore(r).unwrap());
        let mut d = Day::restore(date, "UTC", items).unwrap();
        d.restore_lock_decision(decision);
        let b = Baseline {
            date,
            locked_at: ts(0),
            items: baselined
                .iter()
                .map(|&(n, planned_secs)| BaselineItem {
                    item_id: id(n),
                    planned_secs,
                    span: None,
                })
                .collect(),
        };
        report(&d, Some(&b), &[], ts(now))
    }

    fn week(year: i16, week: i8) -> IsoWeek {
        IsoWeek::new(year, week).unwrap()
    }

    /// 2026-W47: Monday 16 to Sunday 22 November.
    fn w47() -> IsoWeek {
        week(2026, 47)
    }

    #[test]
    fn a_locked_day_counts_actual_over_baseline_planned_overall_and_per_tag() {
        let mon = day_report(
            date(2026, 11, 16),
            LockDecision::Locked,
            vec![worked(1, 1_200, 1_600, &["proposals"])],
            &[(1, 1_000)],
            10_000,
        );
        let r = week_report(w47(), &[mon]);
        let expected = GapFigure {
            items: 1,
            actual_secs: 1_600,
            planned_secs: 1_000,
        };
        assert_eq!(r.overall, expected);
        assert_eq!(r.overall.ratio(), Some(1.6));
        assert_eq!(
            r.tags,
            vec![TagGap {
                tag: "proposals".into(),
                figure: expected
            }]
        );
        assert_eq!(r.counted_days, vec![date(2026, 11, 16)]);
        assert!(r.unlocked_days.is_empty());
        assert_eq!(r.week, w47());
    }

    #[test]
    fn carried_over_removed_and_unplanned_rows_add_nothing() {
        let mon = day_report(
            date(2026, 11, 16),
            LockDecision::Locked,
            vec![
                worked(1, 600, 900, &["x"]),
                PlanItemRecord {
                    status: ItemStatus::CarriedOver,
                    ..worked(2, 600, 300, &["x"])
                },
                PlanItemRecord {
                    status: ItemStatus::Removed,
                    removed_at: Some(ts(50)),
                    ..worked(3, 600, 200, &["x"])
                },
                worked(4, 600, 700, &["x"]),
            ],
            &[(1, 600), (2, 600), (3, 600)],
            10_000,
        );
        let r = week_report(w47(), &[mon]);
        let only_a = GapFigure {
            items: 1,
            actual_secs: 900,
            planned_secs: 600,
        };
        assert_eq!(r.overall, only_a);
        assert_eq!(r.tags[0].figure, only_a);
    }

    #[test]
    fn an_unlocked_day_adds_nothing_and_is_listed() {
        for decision in [LockDecision::NoLock, LockDecision::None] {
            let tue = day_report(
                date(2026, 11, 17),
                decision,
                vec![worked(1, 600, 900, &["x"])],
                &[(1, 600)],
                10_000,
            );
            let r = week_report(w47(), &[tue]);
            assert_eq!(r.overall, GapFigure::default());
            assert!(r.tags.is_empty());
            assert!(r.counted_days.is_empty());
            assert_eq!(r.unlocked_days, vec![date(2026, 11, 17)]);
        }
    }

    #[test]
    fn an_item_with_two_tags_counts_once_overall_and_once_in_each_tag() {
        let wed = day_report(
            date(2026, 11, 18),
            LockDecision::Locked,
            vec![worked(1, 600, 900, &["b", "a"]), worked(2, 300, 300, &[])],
            &[(1, 600), (2, 300)],
            10_000,
        );
        let r = week_report(w47(), &[wed]);
        assert_eq!(
            r.overall,
            GapFigure {
                items: 2,
                actual_secs: 1_200,
                planned_secs: 900,
            }
        );
        let e = GapFigure {
            items: 1,
            actual_secs: 900,
            planned_secs: 600,
        };
        assert_eq!(
            r.tags,
            vec![
                TagGap {
                    tag: "a".into(),
                    figure: e
                },
                TagGap {
                    tag: "b".into(),
                    figure: e
                },
            ]
        );
        assert_eq!(e.ratio(), Some(1.5));
    }

    #[test]
    fn tags_sort_case_insensitively_and_sum_across_days() {
        let mon = day_report(
            date(2026, 11, 16),
            LockDecision::Locked,
            vec![worked(1, 100, 200, &["work"])],
            &[(1, 100)],
            10_000,
        );
        let fri = day_report(
            date(2026, 11, 20),
            LockDecision::Locked,
            vec![worked(2, 300, 100, &["work", "Admin"])],
            &[(2, 300)],
            10_000,
        );
        let r = week_report(w47(), &[fri, mon]);
        let names: Vec<&str> = r.tags.iter().map(|t| t.tag.as_str()).collect();
        assert_eq!(names, vec!["Admin", "work"]);
        assert_eq!(
            r.tags[1].figure,
            GapFigure {
                items: 2,
                actual_secs: 300,
                planned_secs: 400,
            }
        );
        assert_eq!(r.counted_days, vec![date(2026, 11, 16), date(2026, 11, 20)]);
    }

    #[test]
    fn no_locked_days_gives_no_ratio() {
        let r = week_report(w47(), &[]);
        assert_eq!(r.overall, GapFigure::default());
        assert_eq!(r.overall.ratio(), None);
        assert!(r.tags.is_empty() && r.counted_days.is_empty());

        // A locked day without a baseline is counted but adds nothing.
        let mon = day_report(
            date(2026, 11, 16),
            LockDecision::Locked,
            vec![worked(1, 600, 900, &["x"])],
            &[],
            10_000,
        );
        let r = week_report(w47(), &[mon]);
        assert_eq!(r.overall.ratio(), None);
        assert_eq!(r.counted_days, vec![date(2026, 11, 16)]);
    }

    #[test]
    fn reports_outside_the_week_are_ignored() {
        let next_mon = day_report(
            date(2026, 11, 23),
            LockDecision::Locked,
            vec![worked(1, 600, 900, &[])],
            &[(1, 600)],
            10_000,
        );
        let r = week_report(w47(), &[next_mon]);
        assert_eq!(r.overall, GapFigure::default());
        assert!(r.counted_days.is_empty());
    }

    #[test]
    fn a_running_item_counts_up_to_now() {
        let sat = day_report(
            date(2026, 11, 21),
            LockDecision::Locked,
            vec![PlanItemRecord {
                status: ItemStatus::Running,
                segments: vec![closed(0, 100), Segment::open(ts(500))],
                ..record(1, 1_000, &[])
            }],
            &[(1, 1_000)],
            800,
        );
        let r = week_report(w47(), &[sat]);
        assert_eq!(r.overall.actual_secs, 400);
        assert_eq!(r.overall.ratio(), Some(0.4));
    }

    #[test]
    fn a_dst_week_holds_seven_civil_days_and_counts_elapsed_seconds() {
        let w = week(2026, 43);
        assert_eq!(
            w.days(),
            [19, 20, 21, 22, 23, 24, 25].map(|d| date(2026, 10, d))
        );
        // Europe/Berlin falls back at 03:00 CEST on Sunday 25 October. Core has no
        // tz database, so the instants are fixed: 01:30 CEST (23:30 UTC on the
        // 24th) to 03:30 CET (02:30 UTC) reads two hours on the wall clock but is
        // three hours elapsed.
        let (from, to) = (1_792_884_600, 1_792_895_400);
        let sun = day_report(
            date(2026, 10, 25),
            LockDecision::Locked,
            vec![PlanItemRecord {
                segments: vec![closed(from, to)],
                ..record(1, 3_600, &[])
            }],
            &[(1, 3_600)],
            to + 3_600,
        );
        let r = week_report(w, &[sun]);
        assert_eq!(r.overall.actual_secs, 3 * 3_600);
        assert_eq!(r.counted_days, vec![date(2026, 10, 25)]);
    }

    #[test]
    fn the_year_edge_week_spans_two_calendar_years() {
        let w = IsoWeek::containing(date(2027, 1, 1)).unwrap();
        assert_eq!((w.year(), w.week()), (2026, 53));
        assert_eq!(w.start(), date(2026, 12, 28));
        assert_eq!(w.end(), date(2027, 1, 3));
        assert_eq!(w.next(), Some(week(2027, 1)));
        assert_eq!(w.previous(), Some(week(2026, 52)));
        assert_eq!(week(2027, 1).previous(), Some(w));
    }

    #[test]
    fn iso_week_maths_rejects_invalid_weeks_and_never_panics_at_the_range_ends() {
        assert_eq!(IsoWeek::new(2026, 0), None);
        assert_eq!(IsoWeek::new(2025, 53), None);
        assert!(IsoWeek::new(2026, 53).is_some());
        assert_eq!(IsoWeek::new(2026, 54), None);
        let w = w47();
        assert_eq!(w.days()[0], w.start());
        assert_eq!(w.days()[6], w.end());
        assert_eq!(w.days()[0].weekday(), Weekday::Monday);
        assert!(w.contains(date(2026, 11, 22)) && !w.contains(date(2026, 11, 23)));
        assert_eq!(IsoWeek::containing(date(2026, 11, 22)), Some(w));

        // The first week of jiff's range starts on its first day; nothing before.
        let first = IsoWeek::containing(Date::MIN).unwrap();
        assert_eq!(first.start(), Date::MIN);
        assert_eq!(first.previous(), None);
        // The last week runs past jiff's range, so it is no week at all.
        assert_eq!(IsoWeek::containing(Date::MAX), None);
        assert_eq!(IsoWeek::new(9999, 52), None);
        let last = week(9999, 51);
        assert_eq!(last.next(), None);
        assert!(last.previous().is_some());
    }
}
