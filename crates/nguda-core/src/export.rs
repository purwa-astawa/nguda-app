//! CSV export (AD-16, CAP-9): day reports as RFC 4180 CSV, and the file sink port
//! the engine writes it through.
//!
//! One record per [`ReportRow`], carried-over and removed rows included, in the
//! order the reports are given and then report row order. The planned and actual
//! figures are the report's own, so they mean what they mean in the Summary and
//! Week views. The text is UTF-8 with a leading BOM (so spreadsheet apps read
//! non-ASCII titles correctly), records end in CRLF, and a field holding `,`,
//! `"`, CR or LF is quoted with each `"` doubled. Titles are written as they are:
//! no spreadsheet formula escaping. Pure.

use std::fmt;
use std::path::Path;

use crate::gap::IsoWeek;
use crate::report::{DayReport, PlannedSource};

/// Which days an export covers. Days after today never count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportRange {
    /// The dates of one ISO week.
    Week(IsoWeek),
    /// Every stored date.
    All,
}

/// A built CSV file: its whole text, BOM included, and its record count without
/// the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvExport {
    pub contents: String,
    pub rows: usize,
}

/// The header record's fields, in column order.
const HEADER: [&str; 6] = [
    "date",
    "title",
    "tags",
    "planned_secs",
    "planned_source",
    "actual_secs",
];

/// The byte order mark the file starts with.
const BOM: char = '\u{feff}';

/// The CSV of every row of `reports`, in the given order. Pure.
pub fn export_csv(reports: &[DayReport]) -> CsvExport {
    let mut contents = String::new();
    contents.push(BOM);
    push_record(&mut contents, HEADER.iter().copied());
    let mut rows = 0;
    for report in reports {
        let date = report.date.to_string();
        for row in &report.rows {
            let tags = row.tags.join("; ");
            let planned = row.planned_secs.map(|s| s.to_string()).unwrap_or_default();
            let actual = row.actual_secs.to_string();
            push_record(
                &mut contents,
                [
                    date.as_str(),
                    row.title.as_str(),
                    tags.as_str(),
                    planned.as_str(),
                    source_name(row.planned_source),
                    actual.as_str(),
                ],
            );
            rows += 1;
        }
    }
    CsvExport { contents, rows }
}

fn source_name(source: PlannedSource) -> &'static str {
    match source {
        PlannedSource::Baseline => "baseline",
        PlannedSource::Unlocked => "unlocked",
        PlannedSource::Unplanned => "unplanned",
    }
}

fn push_record<'a>(out: &mut String, fields: impl IntoIterator<Item = &'a str>) {
    for (i, field) in fields.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_field(out, field);
    }
    out.push_str("\r\n");
}

fn push_field(out: &mut String, field: &str) {
    if field.contains([',', '"', '\r', '\n']) {
        out.push('"');
        out.push_str(&field.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(field);
    }
}

/// A file sink failure. The message is the OS error or a fixed reason, never
/// the file's contents (AD-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSinkError {
    pub message: String,
}

impl FileSinkError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for FileSinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FileSinkError {}

/// Writes whole files. Blocking: the engine calls it off its actor.
pub trait FileSink: Send + Sync + 'static {
    /// Replaces the file at `path` with `contents`.
    fn write(&self, path: &Path, contents: &[u8]) -> Result<(), FileSinkError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::LockDecision;
    use crate::plan::ItemStatus;
    use crate::report::ReportRow;
    use crate::test_support::id;
    use jiff::civil::date;

    fn row(
        n: u128,
        title: &str,
        tags: &[&str],
        planned_secs: Option<u64>,
        planned_source: PlannedSource,
        actual_secs: u64,
    ) -> ReportRow {
        ReportRow {
            item_id: id(n),
            title: title.to_string(),
            status: ItemStatus::Done,
            carried_from: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            planned_secs,
            planned_source,
            actual_secs,
            difference_secs: None,
        }
    }

    fn report(d: jiff::civil::Date, rows: Vec<ReportRow>) -> DayReport {
        DayReport {
            date: d,
            lock_decision: LockDecision::Locked,
            locked_at: None,
            rows,
            baseline: Vec::new(),
            segments: Vec::new(),
            markers: Vec::new(),
        }
    }

    const HEAD: &str = "\u{feff}date,title,tags,planned_secs,planned_source,actual_secs\r\n";

    fn body(csv: &CsvExport) -> &str {
        csv.contents.strip_prefix(HEAD).expect("BOM and header")
    }

    #[test]
    fn plain_row_joins_tags_and_names_the_baseline() {
        let csv = export_csv(&[report(
            date(2026, 11, 16),
            vec![row(
                1,
                "A",
                &["x", "y"],
                Some(1800),
                PlannedSource::Baseline,
                2400,
            )],
        )]);
        assert_eq!(body(&csv), "2026-11-16,A,x; y,1800,baseline,2400\r\n");
        assert_eq!(csv.rows, 1);
    }

    #[test]
    fn unplanned_row_has_an_empty_planned_value() {
        let csv = export_csv(&[report(
            date(2026, 11, 16),
            vec![row(1, "B", &[], None, PlannedSource::Unplanned, 300)],
        )]);
        assert_eq!(body(&csv), "2026-11-16,B,,,unplanned,300\r\n");
    }

    #[test]
    fn unlocked_row_keeps_its_current_plan() {
        let csv = export_csv(&[report(
            date(2026, 11, 16),
            vec![row(1, "C", &[], Some(600), PlannedSource::Unlocked, 0)],
        )]);
        assert_eq!(body(&csv), "2026-11-16,C,,600,unlocked,0\r\n");
    }

    #[test]
    fn fields_with_separators_quotes_or_newlines_are_quoted() {
        let csv = export_csv(&[report(
            date(2026, 11, 16),
            vec![
                row(1, "a,b", &[], Some(1), PlannedSource::Baseline, 1),
                row(2, "say \"hi\"", &[], Some(1), PlannedSource::Baseline, 1),
                row(3, "line1\nline2", &[], Some(1), PlannedSource::Baseline, 1),
                row(4, "cr\rhere", &["t,1"], Some(1), PlannedSource::Baseline, 1),
            ],
        )]);
        assert_eq!(
            body(&csv),
            "2026-11-16,\"a,b\",,1,baseline,1\r\n\
             2026-11-16,\"say \"\"hi\"\"\",,1,baseline,1\r\n\
             2026-11-16,\"line1\nline2\",,1,baseline,1\r\n\
             2026-11-16,\"cr\rhere\",\"t,1\",1,baseline,1\r\n"
        );
        assert_eq!(csv.rows, 4);
    }

    #[test]
    fn non_ascii_is_written_raw_after_the_bom() {
        let csv = export_csv(&[report(
            date(2026, 11, 16),
            vec![row(
                1,
                "Café 日本",
                &[],
                Some(5),
                PlannedSource::Baseline,
                6,
            )],
        )]);
        assert!(csv.contents.as_bytes().starts_with(&[0xEF, 0xBB, 0xBF]));
        assert_eq!(body(&csv), "2026-11-16,Café 日本,,5,baseline,6\r\n");
    }

    #[test]
    fn no_rows_is_the_bom_and_header_only() {
        let csv = export_csv(&[]);
        assert_eq!(csv.contents, HEAD);
        assert_eq!(csv.rows, 0);
        let empty_day = export_csv(&[report(date(2026, 11, 16), Vec::new())]);
        assert_eq!(empty_day.contents, HEAD);
        assert_eq!(empty_day.rows, 0);
    }

    #[test]
    fn mixed_rows_over_two_days_keep_day_then_row_order() {
        let mut carried = row(2, "moved", &[], Some(60), PlannedSource::Baseline, 10);
        carried.status = ItemStatus::CarriedOver;
        let mut removed = row(3, "gone", &["z"], Some(30), PlannedSource::Baseline, 0);
        removed.status = ItemStatus::Removed;
        let csv = export_csv(&[
            report(
                date(2026, 11, 16),
                vec![
                    row(1, "first", &["a"], Some(100), PlannedSource::Baseline, 120),
                    carried,
                    removed,
                ],
            ),
            report(
                date(2026, 11, 17),
                vec![
                    row(4, "next", &[], Some(200), PlannedSource::Unlocked, 50),
                    row(5, "extra", &[], None, PlannedSource::Unplanned, 70),
                ],
            ),
        ]);
        assert_eq!(
            body(&csv),
            "2026-11-16,first,a,100,baseline,120\r\n\
             2026-11-16,moved,,60,baseline,10\r\n\
             2026-11-16,gone,z,30,baseline,0\r\n\
             2026-11-17,next,,200,unlocked,50\r\n\
             2026-11-17,extra,,,unplanned,70\r\n"
        );
        assert_eq!(csv.rows, 5);
    }
}
