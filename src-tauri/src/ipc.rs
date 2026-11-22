//! The typed IPC contract (AD-5). `src/bindings.ts` is generated from this module.
//!
//! specta-typescript refuses 64-bit integers, so engine `u64` counters cross IPC as
//! saturating `u32` (a TS `number`) and instants as RFC 3339 strings. `f64` was not
//! used because specta types it `number | null` (NaN serializes as null). Command
//! arguments follow the same convention: item ids are UUID strings, instants RFC
//! 3339 strings, revs and seconds `u32`, and the work-end time of day minutes
//! after midnight (`u32`) in and `HH:MM` out.
//!
//! Commands are fire-and-forget (AD-4). Each one only parses its arguments, failing
//! with `Err` on malformed input without sending anything; every semantic check
//! happens in the engine, which reports rejections through `Snapshot.status`.
//!
//! The exceptions are [`query_day`] and [`query_week`] (AD-16), read-only
//! request/replies. They are `async` commands, so Tauri runs them off the main
//! thread and nothing there waits on the engine (AD-4). Dates cross as
//! `YYYY-MM-DD`. A gap ratio crosses as `f64` (`number | null`): it is never NaN,
//! because core gives `None` instead of dividing by zero.
//!
//! [`export_csv`] (AD-16) is also `async`: it parses its range (an `Err` before
//! any dialog opens), asks for a path in a native save dialog opened from Rust
//! (`tauri-plugin-dialog`, so the webview needs no dialog permission), awaiting
//! the dialog's callback through a oneshot rather than blocking, and then sends
//! the fire-and-forget `ExportCsv`. Its `bool` only says whether a path was
//! chosen (`false`: cancelled, nothing sent). The outcome arrives later as an
//! [`ExportDoneEvent`] or an `export` status entry.
//!
//! [`sign_in`] is fire-and-forget like the plan commands: the engine runs the
//! Google sign-in off its actor, and the snapshot's `google` field shows its
//! progress (AD-17). So are [`sign_out`] and [`set_google_client`], whose
//! results show in the snapshot's `google` and `googleClient` fields (or a
//! status entry); the client secret is never sent back (AD-13). So is
//! [`add_calendar_event`]: the engine fetches the event off its actor, and it
//! shows up in the snapshot (or a `sync` status entry).
//!
//! [`query_calendar_events`] is another `async` read-only request/reply, for the
//! picker of today's events of one calendar (R4). A query superseded by a newer
//! one, or for a day that is no longer today, fails with the error string
//! `"stale"`, which the view ignores.

use nguda_core::{
    BaselineItem, CalendarEntry, CalendarErrorKind, Date, Day, DayLogEntry, DayLogKind, DayReport,
    ExportRange, GapFigure, IsoWeek, ItemId, ItemStatus, LockChoice, LockDecision, OAuthClient,
    PlanItem, PlannedSource, ReportRow, Span, Timestamp, Uuid, WeekReport, closed_secs,
};
use nguda_engine::{
    CalendarEvent, EngineHandle, EngineMsg, GoogleAccount, GoogleClient, InsertAnchor, QueryError,
    ReorderAnchor, Snapshot, StatusKey,
};
use serde::{Deserialize, Serialize};
#[cfg(any(debug_assertions, test))]
use specta_typescript::Typescript;
use tauri::{AppHandle, State, WebviewWindow, Wry};
use tauri_plugin_dialog::DialogExt;
use tauri_specta::{Builder, Event, collect_commands, collect_events};

/// Where debug builds write the bindings, relative to this crate.
#[cfg(any(debug_assertions, test))]
pub const BINDINGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../src/bindings.ts");

/// Which side of the anchor item a reordered item goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum AnchorPlace {
    Before,
    After,
}

/// A reorder target: next to another flexible item, never a numeric index (AD-15).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ReorderAnchorArg {
    pub place: AnchorPlace,
    /// UUID of the anchor item.
    pub item_id: String,
}

/// Where an inserted task goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum InsertPlace {
    /// Start it now, interrupting the running item.
    Now,
    Before,
    After,
    End,
}

/// An insert target, never a numeric index (AD-15). `before` and `after` need
/// `itemId`; `now` and `end` ignore it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct InsertAnchorArg {
    pub place: InsertPlace,
    /// UUID of the anchor item, for `before` and `after`.
    pub item_id: Option<String>,
}

/// The answer to "lock the day?" (AD-7), sent with a Start or an insert-now only
/// while the snapshot's day is undecided (`lockDecision` `none`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum LockChoiceArg {
    /// Lock & start: freeze the day's plan as the baseline.
    Lock,
    /// Start without lock: the day never gets a baseline.
    NoLock,
}

impl From<LockChoiceArg> for LockChoice {
    fn from(c: LockChoiceArg) -> Self {
        match c {
            LockChoiceArg::Lock => Self::Lock,
            LockChoiceArg::NoLock => Self::NoLock,
        }
    }
}

fn send(engine: &EngineHandle, msg: Result<EngineMsg, String>) -> Result<(), String> {
    engine.send(msg?).map_err(|e| e.to_string())
}

/// Adds a pending flexible item at the end of the plan.
#[tauri::command]
#[specta::specta]
pub fn create_task(
    engine: State<'_, EngineHandle>,
    title: String,
    planned_secs: u32,
) -> Result<(), String> {
    send(&engine, Ok(create_task_msg(title, planned_secs)))
}

/// Changes an item's title and/or planned length; `null` keeps the current value.
#[tauri::command]
#[specta::specta]
pub fn edit_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    title: Option<String>,
    planned_secs: Option<u32>,
) -> Result<(), String> {
    send(&engine, edit_task_msg(&item_id, title, planned_secs))
}

/// Moves a flexible item before or after another flexible item.
#[tauri::command]
#[specta::specta]
pub fn reorder_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    anchor: ReorderAnchorArg,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, reorder_task_msg(&item_id, &anchor, expected_rev))
}

/// Pins a flexible item at `pinnedStart` (RFC 3339, required), or unpins a fixed one.
#[tauri::command]
#[specta::specta]
pub fn toggle_fixed(
    engine: State<'_, EngineHandle>,
    item_id: String,
    pinned_start: Option<String>,
    expected_rev: u32,
) -> Result<(), String> {
    send(
        &engine,
        toggle_fixed_msg(&item_id, pinned_start.as_deref(), expected_rev),
    )
}

/// Removes an item that is not running. It is tombstoned, never deleted.
#[tauri::command]
#[specta::specta]
pub fn remove_task(engine: State<'_, EngineHandle>, item_id: String) -> Result<(), String> {
    send(&engine, remove_task_msg(&item_id))
}

/// Starts or resumes an item now, pausing whichever item is running. `lock` is
/// the lock choice while the day is undecided, else `null`.
#[tauri::command]
#[specta::specta]
pub fn start_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    expected_rev: u32,
    lock: Option<LockChoiceArg>,
) -> Result<(), String> {
    send(&engine, start_task_msg(&item_id, expected_rev, lock))
}

/// Locks the undecided day without starting anything, freezing its baseline.
#[tauri::command]
#[specta::specta]
pub fn lock_day(engine: State<'_, EngineHandle>, expected_rev: u32) -> Result<(), String> {
    send(&engine, Ok(lock_day_msg(expected_rev)))
}

/// Pauses the running item now.
#[tauri::command]
#[specta::specta]
pub fn pause_task(engine: State<'_, EngineHandle>, item_id: String) -> Result<(), String> {
    send(&engine, pause_task_msg(&item_id))
}

/// Finishes the running item now and starts or resumes the next one.
#[tauri::command]
#[specta::specta]
pub fn switch_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, switch_task_msg(&item_id, expected_rev))
}

/// Adds `secs` to the running item's planned length; later items re-flow.
#[tauri::command]
#[specta::specta]
pub fn extend_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    secs: u32,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, extend_task_msg(&item_id, secs, expected_rev))
}

/// Replaces an item's tags with `tags`. The engine trims them, drops blanks and
/// case-insensitive duplicates, and takes a known tag's spelling.
#[tauri::command]
#[specta::specta]
pub fn set_tags(
    engine: State<'_, EngineHandle>,
    item_id: String,
    tags: Vec<String>,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, set_tags_msg(&item_id, tags, expected_rev))
}

/// Inserts a new task at `anchor`; `now` starts it, pausing the running item.
/// `lock` is the lock choice for `now` on an undecided day, else `null`.
#[tauri::command]
#[specta::specta]
pub fn insert_task(
    engine: State<'_, EngineHandle>,
    title: String,
    planned_secs: u32,
    anchor: InsertAnchorArg,
    expected_rev: u32,
    lock: Option<LockChoiceArg>,
) -> Result<(), String> {
    send(
        &engine,
        insert_task_msg(title, planned_secs, &anchor, expected_rev, lock),
    )
}

/// Sets the work-end time of day, `minutes` after local midnight. The engine
/// rejects values outside `0..=1439`.
#[tauri::command]
#[specta::specta]
pub fn set_work_end(engine: State<'_, EngineHandle>, minutes: u32) -> Result<(), String> {
    send(&engine, Ok(set_work_end_msg(minutes)))
}

/// Turns launch at login on or off (the OS login item, AD-14), then reads it back
/// and reports it to the engine, so the snapshot's `autostart` shows what the OS
/// has. `Err` carries the plugin's error text when the change failed.
#[tauri::command]
#[specta::specta]
pub async fn set_launch_at_login(
    app: AppHandle,
    engine: State<'_, EngineHandle>,
    enabled: bool,
) -> Result<(), String> {
    crate::set_autostart(&app, &engine, enabled)
}

/// Carries the leftovers the snapshot at `expected_rev` lists as overflow over to
/// tomorrow (AD-6). The engine rejects a stale rev or an empty list.
#[tauri::command]
#[specta::specta]
pub fn carry_over_to_tomorrow(
    engine: State<'_, EngineHandle>,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, Ok(carry_over_to_tomorrow_msg(expected_rev)))
}

/// Signs in to Google in the system browser (CAP-2). Fire-and-forget: progress
/// shows in the snapshot's `google` field, and a failure as a `sync` status entry.
#[tauri::command]
#[specta::specta]
pub fn sign_in(engine: State<'_, EngineHandle>) -> Result<(), String> {
    send(&engine, Ok(sign_in_msg()))
}

/// Signs out of Google (R1): the stored sign-in is deleted and polling stops;
/// every plan item stays. Fire-and-forget: the snapshot's `google` field turns
/// `signedOut`, and a keyring failure shows as a `secretStore` status entry.
#[tauri::command]
#[specta::specta]
pub fn sign_out(engine: State<'_, EngineHandle>) -> Result<(), String> {
    send(&engine, Ok(sign_out_msg()))
}

/// Sets the user's own Google OAuth client (AD-13); a missing or blank
/// `client_id` clears it, going back to the built-in client. A blank secret
/// means none. The engine rejects it unless signed out. Fire-and-forget: the
/// snapshot's `googleClient.customId` shows the id, never the secret.
#[tauri::command]
#[specta::specta]
pub fn set_google_client(
    engine: State<'_, EngineHandle>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<(), String> {
    send(&engine, Ok(set_google_client_msg(client_id, client_secret)))
}

/// Includes exactly `calendar_ids` in today's plan (R2), un-including every other
/// listed calendar. Fire-and-forget: the new flags show in the snapshot's
/// `calendars`. Unknown ids are ignored.
#[tauri::command]
#[specta::specta]
pub fn set_included_calendars(
    engine: State<'_, EngineHandle>,
    calendar_ids: Vec<String>,
) -> Result<(), String> {
    send(&engine, Ok(set_included_calendars_msg(calendar_ids)))
}

/// Leaves today's imported event `item_id` out of the schedule (R4): it stays in
/// the plan, greyed. The engine rejects a stale rev, a local item, or one that
/// has time recorded or is not pending.
#[tauri::command]
#[specta::specta]
pub fn exclude_event(
    engine: State<'_, EngineHandle>,
    item_id: String,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, exclude_event_msg(&item_id, expected_rev))
}

/// Takes the excluded event `item_id` back into the schedule. The engine rejects
/// a stale rev or an item that is not excluded.
#[tauri::command]
#[specta::specta]
pub fn include_event(
    engine: State<'_, EngineHandle>,
    item_id: String,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, include_event_msg(&item_id, expected_rev))
}

/// Adds the single event `event_id` of calendar `calendar_id` to today's plan
/// (R4). Fire-and-forget: it shows in the snapshot, or a failure as a `sync`
/// status entry.
#[tauri::command]
#[specta::specta]
pub fn add_calendar_event(
    engine: State<'_, EngineHandle>,
    calendar_id: String,
    event_id: String,
) -> Result<(), String> {
    send(&engine, Ok(add_calendar_event_msg(calendar_id, event_id)))
}

/// Today's timed events of calendar `calendar_id`, sorted by start, each with
/// whether today's plan already has it (R4). `date` (`YYYY-MM-DD`) must be
/// today: any other date, or a query superseded by a newer one, fails with
/// `"stale"`. Read-only. A malformed date is an `Err` and sends nothing.
#[tauri::command]
#[specta::specta]
pub async fn query_calendar_events(
    engine: State<'_, EngineHandle>,
    calendar_id: String,
    date: String,
) -> Result<Vec<CalendarEventView>, String> {
    let date = parse_date(&date)?;
    let events = engine
        .query_calendar_events(calendar_id, date)
        .await
        .map_err(|e| query_error_text(&e))?;
    Ok(events.iter().map(CalendarEventView::from).collect())
}

/// The error string of a failed query: `"stale"` for a stale one, which the
/// view ignores, else the error's message.
fn query_error_text(e: &QueryError) -> String {
    match e {
        QueryError::Stale => "stale".to_owned(),
        QueryError::Calendar(CalendarErrorKind::RateLimited) => {
            "Google is limiting requests; try again later".to_owned()
        }
        QueryError::Calendar(CalendarErrorKind::NotSignedIn) => {
            "not signed in to Google".to_owned()
        }
        QueryError::Calendar(_) => "could not list the calendar's events".to_owned(),
        other => other.to_string(),
    }
}

/// One of today's events of a calendar, for the picker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventView {
    /// Google's calendar id.
    pub calendar_id: String,
    /// Google's event (instance) id: what [`add_calendar_event`] takes.
    pub event_id: String,
    pub title: String,
    /// RFC 3339 UTC instant.
    pub start: String,
    /// RFC 3339 UTC instant.
    pub end: String,
    /// Today's plan already has it, so it cannot be added again.
    pub in_plan: bool,
}

impl From<&CalendarEvent> for CalendarEventView {
    fn from(e: &CalendarEvent) -> Self {
        Self {
            calendar_id: e.calendar_id.clone(),
            event_id: e.event_id.clone(),
            title: e.title.clone(),
            start: e.start.to_string(),
            end: e.end.to_string(),
            in_plan: e.in_plan,
        }
    }
}

/// The planned-versus-actual report of `date` (`YYYY-MM-DD`), or `null` when no
/// such day is stored. Read-only (AD-16). A malformed date is an `Err` and sends
/// nothing.
#[tauri::command]
#[specta::specta]
pub async fn query_day(
    engine: State<'_, EngineHandle>,
    date: String,
) -> Result<Option<DayReportView>, String> {
    let date = parse_date(&date)?;
    let report = engine.query_day(date).await.map_err(|e| e.to_string())?;
    Ok(report.as_ref().map(DayReportView::from))
}

/// The estimate gap of the ISO week (Monday to Sunday, local dates) holding
/// `date` (`YYYY-MM-DD`). Read-only (AD-16). A malformed date, or one in the last
/// days of the calendar whose week runs past it, is an `Err` and sends nothing.
#[tauri::command]
#[specta::specta]
pub async fn query_week(
    engine: State<'_, EngineHandle>,
    date: String,
) -> Result<WeekReportView, String> {
    let week = week_of(&date)?;
    let report = engine.query_week(week).await.map_err(|e| e.to_string())?;
    Ok(WeekReportView::from(&report))
}

/// Which days a CSV export covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ExportKind {
    /// The ISO week holding `date`.
    Week,
    /// Every stored day up to today.
    All,
}

/// A CSV export range. `week` needs `date` (`YYYY-MM-DD`, any day of the week);
/// `all` ignores it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportRangeArg {
    pub kind: ExportKind,
    pub date: Option<String>,
}

/// Exports `range` as CSV (AD-16): asks for a path in a native save dialog, then
/// sends the export to the engine. `Ok(false)` when the dialog was cancelled
/// (nothing sent), `Ok(true)` once the export is queued. A malformed range is an
/// `Err` before any dialog opens. The written row count arrives as an
/// [`ExportDoneEvent`]; a failure as an `export` status entry.
#[tauri::command]
#[specta::specta]
pub async fn export_csv(
    app: AppHandle,
    window: WebviewWindow,
    engine: State<'_, EngineHandle>,
    range: ExportRangeArg,
) -> Result<bool, String> {
    let range = export_range_of(&range)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_parent(&window)
        .add_filter("CSV", &["csv"])
        .set_file_name(default_file_name(range))
        .save_file(move |chosen| {
            // The command may have gone away; nothing to do then.
            let _ = tx.send(chosen);
        });
    let chosen = rx
        .await
        .map_err(|_| "the save dialog closed without an answer".to_owned())?;
    let path = chosen
        .map(|c| c.into_path())
        .transpose()
        .map_err(|_| "the chosen location is not a local file".to_owned())?;
    match export_msg(range, path) {
        Some(msg) => send(&engine, Ok(msg)).map(|()| true),
        None => Ok(false),
    }
}

/// The message for a save dialog's answer: `None` when it was cancelled, so
/// nothing is sent.
fn export_msg(range: ExportRange, chosen: Option<std::path::PathBuf>) -> Option<EngineMsg> {
    chosen.map(|path| EngineMsg::ExportCsv { path, range })
}

// --- Argument parsing: syntax only, no semantic checks ---

fn export_range_of(arg: &ExportRangeArg) -> Result<ExportRange, String> {
    match arg.kind {
        ExportKind::All => Ok(ExportRange::All),
        ExportKind::Week => {
            let date = arg
                .date
                .as_deref()
                .ok_or_else(|| "a week export needs a date".to_owned())?;
            week_of(date).map(ExportRange::Week)
        }
    }
}

/// The save dialog's suggested name: `nguda-<year>-W<ww>.csv` or `nguda-all.csv`.
fn default_file_name(range: ExportRange) -> String {
    match range {
        ExportRange::Week(w) => format!("nguda-{}-W{:02}.csv", w.year(), w.week()),
        ExportRange::All => "nguda-all.csv".to_owned(),
    }
}

fn week_of(s: &str) -> Result<IsoWeek, String> {
    IsoWeek::containing(parse_date(s)?).ok_or_else(|| "date out of range".to_owned())
}

fn parse_date(s: &str) -> Result<Date, String> {
    let bad = || "invalid date: expected YYYY-MM-DD".to_owned();
    let shaped = s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !shaped {
        return Err(bad());
    }
    s.parse::<Date>().map_err(|_| bad())
}

fn parse_item_id(s: &str) -> Result<ItemId, String> {
    Uuid::parse_str(s)
        .map(ItemId)
        .map_err(|_| "invalid item id: expected a UUID".to_owned())
}

fn parse_instant(s: &str) -> Result<Timestamp, String> {
    s.parse::<Timestamp>()
        .map_err(|_| "invalid instant: expected an RFC 3339 timestamp".to_owned())
}

fn create_task_msg(title: String, planned_secs: u32) -> EngineMsg {
    EngineMsg::CreateTask {
        title,
        planned_secs: planned_secs.into(),
    }
}

fn edit_task_msg(
    item_id: &str,
    title: Option<String>,
    planned_secs: Option<u32>,
) -> Result<EngineMsg, String> {
    Ok(EngineMsg::EditTask {
        item_id: parse_item_id(item_id)?,
        title,
        planned_secs: planned_secs.map(u64::from),
    })
}

fn reorder_task_msg(
    item_id: &str,
    anchor: &ReorderAnchorArg,
    expected_rev: u32,
) -> Result<EngineMsg, String> {
    let item_id = parse_item_id(item_id)?;
    let anchor_id = parse_item_id(&anchor.item_id)?;
    let anchor = match anchor.place {
        AnchorPlace::Before => ReorderAnchor::Before(anchor_id),
        AnchorPlace::After => ReorderAnchor::After(anchor_id),
    };
    Ok(EngineMsg::ReorderTask {
        item_id,
        anchor,
        expected_rev: expected_rev.into(),
    })
}

fn toggle_fixed_msg(
    item_id: &str,
    pinned_start: Option<&str>,
    expected_rev: u32,
) -> Result<EngineMsg, String> {
    Ok(EngineMsg::ToggleFixed {
        item_id: parse_item_id(item_id)?,
        pinned_start: pinned_start.map(parse_instant).transpose()?,
        expected_rev: expected_rev.into(),
    })
}

fn remove_task_msg(item_id: &str) -> Result<EngineMsg, String> {
    Ok(EngineMsg::RemoveTask {
        item_id: parse_item_id(item_id)?,
    })
}

fn start_task_msg(
    item_id: &str,
    expected_rev: u32,
    lock: Option<LockChoiceArg>,
) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Start {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
        lock: lock.map(Into::into),
    })
}

fn lock_day_msg(expected_rev: u32) -> EngineMsg {
    EngineMsg::LockDay {
        expected_rev: expected_rev.into(),
    }
}

fn pause_task_msg(item_id: &str) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Pause {
        item_id: parse_item_id(item_id)?,
    })
}

fn switch_task_msg(item_id: &str, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Switch {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
    })
}

fn extend_task_msg(item_id: &str, secs: u32, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::ExtendTask {
        item_id: parse_item_id(item_id)?,
        secs: secs.into(),
        expected_rev: expected_rev.into(),
    })
}

fn set_tags_msg(item_id: &str, tags: Vec<String>, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::SetTags {
        item_id: parse_item_id(item_id)?,
        tags,
        expected_rev: expected_rev.into(),
    })
}

fn exclude_event_msg(item_id: &str, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::ExcludeEvent {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
    })
}

fn include_event_msg(item_id: &str, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::IncludeEvent {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
    })
}

fn add_calendar_event_msg(calendar_id: String, event_id: String) -> EngineMsg {
    EngineMsg::AddCalendarEvent {
        calendar_id,
        event_id,
    }
}

fn set_work_end_msg(minutes: u32) -> EngineMsg {
    EngineMsg::SetWorkEnd { minutes }
}

/// What the shell reports after reading the OS login item.
pub(crate) fn autostart_observed_msg(enabled: bool) -> EngineMsg {
    EngineMsg::AutostartObserved { enabled }
}

fn sign_in_msg() -> EngineMsg {
    EngineMsg::SignIn
}

fn sign_out_msg() -> EngineMsg {
    EngineMsg::SignOut
}

fn set_google_client_msg(client_id: Option<String>, client_secret: Option<String>) -> EngineMsg {
    let client = client_id
        .filter(|id| !id.trim().is_empty())
        .map(|id| OAuthClient {
            id,
            secret: client_secret.filter(|s| !s.trim().is_empty()),
        });
    EngineMsg::SetGoogleClient { client }
}

fn set_included_calendars_msg(calendar_ids: Vec<String>) -> EngineMsg {
    EngineMsg::SetIncludedCalendars { calendar_ids }
}

fn carry_over_to_tomorrow_msg(expected_rev: u32) -> EngineMsg {
    EngineMsg::CarryOverToTomorrow {
        expected_rev: expected_rev.into(),
    }
}

fn insert_task_msg(
    title: String,
    planned_secs: u32,
    anchor: &InsertAnchorArg,
    expected_rev: u32,
    lock: Option<LockChoiceArg>,
) -> Result<EngineMsg, String> {
    let anchor_id = || {
        anchor
            .item_id
            .as_deref()
            .ok_or_else(|| "missing anchor item id".to_owned())
            .and_then(parse_item_id)
    };
    let anchor = match anchor.place {
        InsertPlace::Now => InsertAnchor::Now,
        InsertPlace::End => InsertAnchor::End,
        InsertPlace::Before => InsertAnchor::Before(anchor_id()?),
        InsertPlace::After => InsertAnchor::After(anchor_id()?),
    };
    Ok(EngineMsg::InsertTask {
        title,
        planned_secs: planned_secs.into(),
        anchor,
        expected_rev: expected_rev.into(),
        lock: lock.map(Into::into),
    })
}

/// The tray's Extend… item was picked: the view focuses the running row's
/// extend-minutes field. Carries nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type, Event)]
pub struct FocusExtendEvent;

/// A CSV export finished: `rows` records (the header not counted) were written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct ExportDoneEvent {
    pub rows: u32,
}

/// The tray's "Today's summary" was picked: the view shows the Summary tab for
/// the snapshot's today. Carries nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type, Event)]
pub struct OpenSummaryEvent;

/// The last engine snapshot, as the view sees it. Also the reply of
/// [`current_snapshot`], so a window that loads late still gets the latest state.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    pub rev: u32,
    /// Today. `null` only before startup finishes (rev 0).
    pub day: Option<DayView>,
    /// Today's non-removed items in schedule order (`Snapshot.schedule`), then
    /// today's excluded events (which the schedule leaves out) by pinned start,
    /// with no scheduled times. The view shows them as given and never re-sorts.
    pub items: Vec<ItemView>,
    pub running: Option<RunningView>,
    /// The work-end time of day, `HH:MM` local.
    pub work_end: String,
    /// UUIDs of the flexible items that end after work-end, in schedule order. The
    /// view renders them as given and never computes overflow itself.
    pub overflow: Vec<String>,
    /// One entry per status key, sorted by key.
    pub status: Vec<StatusView>,
    /// Every tag name ever stored, sorted case-insensitively: the suggestions for
    /// an item's tag editor.
    pub known_tags: Vec<String>,
    /// The Google account state. Never an address or a token.
    pub google: GoogleAccountView,
    /// The account's calendars as last listed, in Google's order. Empty before
    /// the first listing.
    pub calendars: Vec<CalendarView>,
    /// The OAuth client sign-in uses. Never the secret.
    pub google_client: GoogleClientView,
    /// Whether nguda starts at login, as last read from the OS. `null` until
    /// the first successful read; a failed read keeps the last value.
    pub autostart: Option<bool>,
}

/// The OAuth client sign-in uses (AD-13). Never carries the secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct GoogleClientView {
    /// The user's own client id, when one is set.
    pub custom_id: Option<String>,
    /// Whether any client (the user's or a built-in one) can sign in.
    pub available: bool,
}

impl From<&GoogleClient> for GoogleClientView {
    fn from(c: &GoogleClient) -> Self {
        Self {
            custom_id: c.custom_id.clone(),
            available: c.available,
        }
    }
}

/// One calendar of the account and whether it feeds today's plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CalendarView {
    /// Google's calendar id.
    pub id: String,
    /// The name to show.
    pub name: String,
    /// The account's primary calendar.
    pub primary: bool,
    /// Its events feed today's plan.
    pub included: bool,
}

impl From<&CalendarEntry> for CalendarView {
    fn from(c: &CalendarEntry) -> Self {
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            primary: c.primary,
            included: c.included,
        }
    }
}

/// What the app knows about the Google account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum GoogleAccountView {
    /// Still checking at startup.
    Unknown,
    SignedOut,
    /// Waiting for the browser sign-in.
    SigningIn,
    SignedIn,
}

impl From<GoogleAccount> for GoogleAccountView {
    fn from(g: GoogleAccount) -> Self {
        match g {
            GoogleAccount::Unknown => Self::Unknown,
            GoogleAccount::SignedOut => Self::SignedOut,
            GoogleAccount::SigningIn => Self::SigningIn,
            GoogleAccount::SignedIn => Self::SignedIn,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DayView {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    /// The IANA zone fixed for this day.
    pub tz: String,
    /// Whether the day is locked (AD-7). While `none`, Start and insert-now carry
    /// a lock choice; once decided, they carry none.
    pub lock_decision: LockDecisionView,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum LockDecisionView {
    None,
    Locked,
    NoLock,
}

impl From<LockDecision> for LockDecisionView {
    fn from(d: LockDecision) -> Self {
        match d {
            LockDecision::None => Self::None,
            LockDecision::Locked => Self::Locked,
            LockDecision::NoLock => Self::NoLock,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ItemView {
    /// UUID of the item.
    pub id: String,
    pub title: String,
    pub planned_secs: u32,
    pub fixed: bool,
    /// RFC 3339 UTC instant; set exactly when `fixed`.
    pub pinned_start: Option<String>,
    pub status: ItemStatusView,
    /// Closed segments only. A running item's live time comes from `running`.
    pub actual_secs: u32,
    /// RFC 3339 UTC instant the schedule puts this item's start at; `null` when it
    /// has no span (carried over, or done without time).
    pub scheduled_start: Option<String>,
    /// RFC 3339 UTC instant the schedule puts this item's end at; set exactly when
    /// `scheduled_start` is.
    pub scheduled_end: Option<String>,
    /// The item is paused on the interrupt stack: `Switch` resumes it before the
    /// schedule, so it is never an insert anchor.
    pub interrupted: bool,
    /// The item's tags, in order.
    pub tags: Vec<String>,
    /// The item was imported from a calendar event.
    pub imported: bool,
    /// The user left this imported event out of the schedule: shown greyed,
    /// with no scheduled times.
    pub excluded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatusView {
    Pending,
    Running,
    Paused,
    Done,
    CarriedOver,
    Removed,
}

impl From<ItemStatus> for ItemStatusView {
    fn from(s: ItemStatus) -> Self {
        match s {
            ItemStatus::Pending => Self::Pending,
            ItemStatus::Running => Self::Running,
            ItemStatus::Paused => Self::Paused,
            ItemStatus::Done => Self::Done,
            ItemStatus::CarriedOver => Self::CarriedOver,
            ItemStatus::Removed => Self::Removed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub key: StatusKeyView,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum StatusKeyView {
    Store,
    CommandRejected,
    Overflow,
    /// The last CSV export failed; cleared by the next successful one.
    Export,
    /// The last Google sign-in or calendar import failed, or the sign-in
    /// expired; cleared by the next successful one or a sign-out.
    Sync,
    /// The sign-in lasts only until the app quits (no keyring), or a sign-out
    /// could not delete it from the keyring.
    SecretStore,
}

impl From<StatusKey> for StatusKeyView {
    fn from(k: StatusKey) -> Self {
        match k {
            StatusKey::Store => Self::Store,
            StatusKey::CommandRejected => Self::CommandRejected,
            StatusKey::Overflow => Self::Overflow,
            StatusKey::Export => Self::Export,
            StatusKey::Sync => Self::Sync,
            StatusKey::SecretStore => Self::SecretStore,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RunningView {
    /// UUID of the running item.
    pub item_id: String,
    pub title: String,
    /// RFC 3339 UTC instant, e.g. `2026-11-10T09:00:00Z`.
    pub segment_started_at: String,
    pub accumulated_secs: u32,
}

/// A day's planned-versus-actual report (AD-16), the reply of [`query_day`].
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DayReportView {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    /// Anything but `locked` means "planned" is the current plan (`unlocked`).
    pub lock_decision: LockDecisionView,
    /// RFC 3339 UTC instant the day was locked; `null` unless locked with a baseline.
    pub locked_at: Option<String>,
    /// Every item of the day, removed and carried-over ones included: schedule
    /// order, then removed items. The view shows them as given.
    pub rows: Vec<ReportRowView>,
    /// The frozen baseline in schedule order at lock; empty unless locked.
    pub baseline: Vec<BaselineSlotView>,
    /// Every stretch of actual work, by start.
    pub segments: Vec<SegmentView>,
    /// The day log in append order.
    pub markers: Vec<MarkerView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ReportRowView {
    /// UUID of the item.
    pub item_id: String,
    pub title: String,
    pub status: ItemStatusView,
    /// UUID of the item on an earlier day this one was carried over from.
    pub carried_from: Option<String>,
    /// The item's tags, in order.
    pub tags: Vec<String>,
    /// `null` exactly when `plannedSource` is `unplanned`.
    pub planned_secs: Option<u32>,
    pub planned_source: PlannedSourceView,
    /// Closed segments plus the open one up to the engine's `now`.
    pub actual_secs: u32,
    /// `actualSecs − plannedSecs`, signed and saturating; `null` with no planned value.
    pub difference_secs: Option<i32>,
}

/// Where a row's planned value comes from (AD-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum PlannedSourceView {
    /// The frozen baseline of a locked day.
    Baseline,
    /// The current plan of a day that is not locked.
    Unlocked,
    /// Added to a locked day after lock: no planned value.
    Unplanned,
}

impl From<PlannedSource> for PlannedSourceView {
    fn from(p: PlannedSource) -> Self {
        match p {
            PlannedSource::Baseline => Self::Baseline,
            PlannedSource::Unlocked => Self::Unlocked,
            PlannedSource::Unplanned => Self::Unplanned,
        }
    }
}

/// One frozen baseline item: its planned length and scheduled span at lock.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BaselineSlotView {
    /// UUID of the item.
    pub item_id: String,
    pub planned_secs: u32,
    /// RFC 3339 UTC instant; `null` for a done item without time at lock.
    pub scheduled_start: Option<String>,
    /// RFC 3339 UTC instant; set exactly when `scheduledStart` is.
    pub scheduled_end: Option<String>,
}

/// One stretch of actual work. An open one ends at the engine's `now`.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SegmentView {
    /// UUID of the item.
    pub item_id: String,
    /// RFC 3339 UTC instant.
    pub start: String,
    /// RFC 3339 UTC instant.
    pub end: String,
    pub open: bool,
}

/// One day-log entry.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkerView {
    /// RFC 3339 UTC instant.
    pub at: String,
    pub kind: MarkerKindView,
    /// UUID of the item the entry is about (for `switched`, the item switched from).
    pub item_id: String,
    /// UUID of the other item: the switch target (`null` when nothing was left) or
    /// the carried-over copy. `null` for every other kind.
    pub to_item_id: Option<String>,
    /// The seconds an `extended` entry added; `null` for every other kind.
    pub secs: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum MarkerKindView {
    Recovered,
    Removed,
    Switched,
    Extended,
    Inserted,
    CarriedOver,
    CalendarChanged,
    Excluded,
    Included,
}

/// A week's estimate gap (AD-6, AD-16), the reply of [`query_week`].
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WeekReportView {
    /// The ISO year, which can differ from the calendar year of some dates.
    pub year: i32,
    /// The ISO week number, `1..=53`.
    pub week: u32,
    /// The Monday, `YYYY-MM-DD`.
    pub start: String,
    /// The Sunday, `YYYY-MM-DD`.
    pub end: String,
    /// The previous week's Monday; `null` at the start of the calendar.
    pub previous_start: Option<String>,
    /// The next week's Monday; `null` at the end of the calendar.
    pub next_start: Option<String>,
    /// Σ actual ÷ Σ baseline planned over every counted item.
    pub overall: GapView,
    /// Each tag of a counted item, sorted case-insensitively.
    pub tags: Vec<TagGapView>,
    /// The locked days of the week, ascending.
    pub counted_days: Vec<String>,
    /// The days excluded as unlocked (undecided or no-lock), ascending.
    pub unlocked_days: Vec<String>,
}

/// One gap figure and the counts behind it.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct GapView {
    /// `actualSecs ÷ plannedSecs`, from core; `null` when nothing was planned.
    pub ratio: Option<f64>,
    pub items: u32,
    pub actual_secs: u32,
    pub planned_secs: u32,
}

/// One tag's gap.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TagGapView {
    pub tag: String,
    pub gap: GapView,
}

impl From<&GapFigure> for GapView {
    fn from(g: &GapFigure) -> Self {
        Self {
            ratio: g.ratio(),
            items: g.items,
            actual_secs: saturating_u32(g.actual_secs),
            planned_secs: saturating_u32(g.planned_secs),
        }
    }
}

impl From<&WeekReport> for WeekReportView {
    fn from(r: &WeekReport) -> Self {
        let dates = |days: &[Date]| days.iter().map(ToString::to_string).collect();
        Self {
            year: i32::from(r.week.year()),
            week: u32::try_from(r.week.week()).unwrap_or(0),
            start: r.week.start().to_string(),
            end: r.week.end().to_string(),
            previous_start: r.week.previous().map(|w| w.start().to_string()),
            next_start: r.week.next().map(|w| w.start().to_string()),
            overall: GapView::from(&r.overall),
            tags: r
                .tags
                .iter()
                .map(|t| TagGapView {
                    tag: t.tag.clone(),
                    gap: GapView::from(&t.figure),
                })
                .collect(),
            counted_days: dates(&r.counted_days),
            unlocked_days: dates(&r.unlocked_days),
        }
    }
}

impl From<&DayReport> for DayReportView {
    fn from(r: &DayReport) -> Self {
        Self {
            date: r.date.to_string(),
            lock_decision: r.lock_decision.into(),
            locked_at: r.locked_at.map(|t| t.to_string()),
            rows: r.rows.iter().map(row_view).collect(),
            baseline: r.baseline.iter().map(baseline_view).collect(),
            segments: r
                .segments
                .iter()
                .map(|s| SegmentView {
                    item_id: s.item_id.to_string(),
                    start: s.start.to_string(),
                    end: s.end.to_string(),
                    open: s.open,
                })
                .collect(),
            markers: r.markers.iter().map(marker_view).collect(),
        }
    }
}

fn row_view(r: &ReportRow) -> ReportRowView {
    ReportRowView {
        item_id: r.item_id.to_string(),
        title: r.title.clone(),
        status: r.status.into(),
        carried_from: r.carried_from.map(|id| id.to_string()),
        tags: r.tags.clone(),
        planned_secs: r.planned_secs.map(saturating_u32),
        planned_source: r.planned_source.into(),
        actual_secs: saturating_u32(r.actual_secs),
        difference_secs: r.difference_secs.map(saturating_i32),
    }
}

fn baseline_view(b: &BaselineItem) -> BaselineSlotView {
    BaselineSlotView {
        item_id: b.item_id.to_string(),
        planned_secs: saturating_u32(b.planned_secs),
        scheduled_start: b.span.map(|sp| sp.start.to_string()),
        scheduled_end: b.span.map(|sp| sp.end.to_string()),
    }
}

fn marker_view(e: &DayLogEntry) -> MarkerView {
    let (kind, item_id, to, secs) = match &e.kind {
        DayLogKind::Recovered { item_id } => (MarkerKindView::Recovered, item_id, None, None),
        DayLogKind::Removed { item_id } => (MarkerKindView::Removed, item_id, None, None),
        DayLogKind::Switched { from, to } => (MarkerKindView::Switched, from, *to, None),
        DayLogKind::Extended { item_id, secs } => {
            (MarkerKindView::Extended, item_id, None, Some(*secs))
        }
        DayLogKind::Inserted { item_id } => (MarkerKindView::Inserted, item_id, None, None),
        DayLogKind::CarriedOver { item_id, to } => {
            (MarkerKindView::CarriedOver, item_id, Some(*to), None)
        }
        DayLogKind::CalendarChanged { item_id } => {
            (MarkerKindView::CalendarChanged, item_id, None, None)
        }
        DayLogKind::Excluded { item_id } => (MarkerKindView::Excluded, item_id, None, None),
        DayLogKind::Included { item_id } => (MarkerKindView::Included, item_id, None, None),
    };
    MarkerView {
        at: e.at.to_string(),
        kind,
        item_id: item_id.to_string(),
        to_item_id: to.map(|id| id.to_string()),
        secs: secs.map(saturating_u32),
    }
}

impl From<&Snapshot> for SnapshotEvent {
    fn from(s: &Snapshot) -> Self {
        let items: Vec<ItemView> = s
            .day
            .as_ref()
            .map(|day| {
                let mut items: Vec<ItemView> = s
                    .schedule
                    .slots()
                    .iter()
                    .filter_map(|slot| day.item(slot.item_id).map(|i| item_view(day, i, slot.span)))
                    .collect();
                // Excluded events have no slot; they follow, by pin, unscheduled.
                let mut excluded: Vec<&PlanItem> = day
                    .items()
                    .iter()
                    .filter(|i| {
                        i.excluded()
                            && i.status() != ItemStatus::Removed
                            && s.schedule.slot(i.id()).is_none()
                    })
                    .collect();
                excluded.sort_by_key(|i| i.pinned_start());
                items.extend(excluded.into_iter().map(|i| item_view(day, i, None)));
                items
            })
            .unwrap_or_default();
        Self {
            rev: saturating_u32(s.rev),
            day: s.day.as_ref().map(|d| DayView {
                date: d.date().to_string(),
                tz: d.tz().to_owned(),
                lock_decision: d.lock_decision().into(),
            }),
            items,
            running: s.running.as_ref().map(|r| RunningView {
                item_id: r.item_id.to_string(),
                title: r.title.clone(),
                segment_started_at: r.segment_started_at.to_string(),
                accumulated_secs: saturating_u32(r.accumulated_secs),
            }),
            work_end: s.work_end.to_string(),
            overflow: s.overflow.iter().map(ToString::to_string).collect(),
            known_tags: s.known_tags.clone(),
            google: s.google.into(),
            google_client: (&s.google_client).into(),
            autostart: s.autostart,
            calendars: s.calendars.iter().map(CalendarView::from).collect(),
            status: s
                .status
                .iter()
                .map(|e| StatusView {
                    key: e.key.into(),
                    message: e.message.clone(),
                })
                .collect(),
        }
    }
}

fn item_view(day: &Day, i: &PlanItem, span: Option<Span>) -> ItemView {
    ItemView {
        id: i.id().to_string(),
        title: i.title().to_owned(),
        planned_secs: saturating_u32(i.planned_secs()),
        fixed: i.fixed(),
        pinned_start: i.pinned_start().map(|t| t.to_string()),
        status: i.status().into(),
        actual_secs: saturating_u32(closed_secs(i.segments())),
        scheduled_start: span.map(|sp| sp.start.to_string()),
        scheduled_end: span.map(|sp| sp.end.to_string()),
        interrupted: day.interrupt_stack().contains(&i.id()),
        tags: i.tags().to_vec(),
        imported: i.source().is_some(),
        excluded: i.excluded(),
    }
}

/// The latest snapshot, read from the engine's `watch` receiver. Sends nothing to
/// the engine (AD-4), so a window that loads after the last event still renders.
#[tauri::command]
#[specta::specta]
pub fn current_snapshot(engine: State<'_, EngineHandle>) -> SnapshotEvent {
    SnapshotEvent::from(&*engine.subscribe().borrow())
}

fn saturating_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn saturating_i32(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX })
}

pub fn specta_builder() -> Builder<Wry> {
    Builder::<Wry>::new()
        .commands(collect_commands![
            create_task,
            edit_task,
            reorder_task,
            toggle_fixed,
            remove_task,
            start_task,
            lock_day,
            pause_task,
            switch_task,
            extend_task,
            set_tags,
            insert_task,
            set_work_end,
            set_launch_at_login,
            carry_over_to_tomorrow,
            sign_in,
            sign_out,
            set_google_client,
            set_included_calendars,
            exclude_event,
            include_event,
            add_calendar_event,
            current_snapshot,
            query_day,
            query_calendar_events,
            query_week,
            export_csv
        ])
        .events(collect_events![
            SnapshotEvent,
            FocusExtendEvent,
            OpenSummaryEvent,
            ExportDoneEvent
        ])
}

#[cfg(any(debug_assertions, test))]
pub fn export_bindings(builder: &Builder<Wry>, path: &str) -> Result<(), specta_typescript::Error> {
    builder.export(Typescript::default(), path)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nguda_adapters::SqliteStore;
    use nguda_core::{CalendarRef, OrderKey, PlanItemRecord, Running, Segment, WorkEnd};
    use nguda_engine::StatusEntry;
    use tokio::time::timeout;

    use super::*;

    /// Same export the debug app runs at startup; fails if the committed file drifted.
    /// Run with `NGUDA_WRITE_BINDINGS=1` to regenerate `src/bindings.ts`.
    #[test]
    fn committed_bindings_match_the_generated_ones() {
        let builder = specta_builder();
        if std::env::var_os("NGUDA_WRITE_BINDINGS").is_some() {
            export_bindings(&builder, BINDINGS_PATH).unwrap();
        }
        let out = std::env::temp_dir().join(format!("nguda-bindings-{}.ts", std::process::id()));
        export_bindings(&builder, out.to_str().unwrap()).unwrap();
        let generated = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        let committed = std::fs::read_to_string(BINDINGS_PATH).unwrap_or_default();
        assert!(
            generated == committed,
            "src/bindings.ts is stale; run `NGUDA_WRITE_BINDINGS=1 cargo test -p nguda`"
        );
    }

    const A: &str = "0192f3a0-0000-7000-8000-000000000001";
    const B: &str = "0192f3a0-0000-7000-8000-000000000002";

    fn item(s: &str) -> ItemId {
        ItemId(Uuid::parse_str(s).unwrap())
    }

    #[test]
    fn committed_bindings_export_the_plan_commands_and_current_snapshot() {
        let committed = std::fs::read_to_string(BINDINGS_PATH).unwrap();
        for name in [
            "createTask",
            "editTask",
            "reorderTask",
            "toggleFixed",
            "removeTask",
            "startTask",
            "lockDay",
            "pauseTask",
            "switchTask",
            "extendTask",
            "setTags",
            "insertTask",
            "setWorkEnd",
            "setLaunchAtLogin",
            "carryOverToTomorrow",
            "currentSnapshot",
            "queryDay",
            "queryWeek",
        ] {
            assert!(committed.contains(&format!("\t{name}: ")), "{name}");
        }
        assert!(!committed.contains("startDemo"));
        assert!(committed.contains("focusExtendEvent"));
        assert!(committed.contains("openSummaryEvent"));
    }

    #[test]
    fn valid_arguments_parse_into_the_expected_messages() {
        assert_eq!(
            create_task_msg("Write".into(), 600),
            EngineMsg::CreateTask {
                title: "Write".into(),
                planned_secs: 600
            }
        );
        assert_eq!(
            edit_task_msg(A, Some("t".into()), None),
            Ok(EngineMsg::EditTask {
                item_id: item(A),
                title: Some("t".into()),
                planned_secs: None
            })
        );
        assert_eq!(
            edit_task_msg(A, None, Some(90)),
            Ok(EngineMsg::EditTask {
                item_id: item(A),
                title: None,
                planned_secs: Some(90)
            })
        );
        let before = ReorderAnchorArg {
            place: AnchorPlace::Before,
            item_id: B.into(),
        };
        assert_eq!(
            reorder_task_msg(A, &before, 7),
            Ok(EngineMsg::ReorderTask {
                item_id: item(A),
                anchor: ReorderAnchor::Before(item(B)),
                expected_rev: 7
            })
        );
        let after = ReorderAnchorArg {
            place: AnchorPlace::After,
            item_id: B.into(),
        };
        assert_eq!(
            reorder_task_msg(A, &after, 7),
            Ok(EngineMsg::ReorderTask {
                item_id: item(A),
                anchor: ReorderAnchor::After(item(B)),
                expected_rev: 7
            })
        );
        assert_eq!(
            toggle_fixed_msg(A, Some("2026-11-11T09:30:00+01:00"), 3),
            Ok(EngineMsg::ToggleFixed {
                item_id: item(A),
                pinned_start: Some("2026-11-11T08:30:00Z".parse().unwrap()),
                expected_rev: 3
            })
        );
        assert_eq!(
            toggle_fixed_msg(A, None, 3),
            Ok(EngineMsg::ToggleFixed {
                item_id: item(A),
                pinned_start: None,
                expected_rev: 3
            })
        );
        assert_eq!(
            remove_task_msg(A),
            Ok(EngineMsg::RemoveTask { item_id: item(A) })
        );
        assert_eq!(
            start_task_msg(A, 4, None),
            Ok(EngineMsg::Start {
                item_id: item(A),
                expected_rev: 4,
                lock: None
            })
        );
        for (arg, choice) in [
            (LockChoiceArg::Lock, LockChoice::Lock),
            (LockChoiceArg::NoLock, LockChoice::NoLock),
        ] {
            assert_eq!(
                start_task_msg(A, 4, Some(arg)),
                Ok(EngineMsg::Start {
                    item_id: item(A),
                    expected_rev: 4,
                    lock: Some(choice)
                })
            );
            assert_eq!(
                insert_task_msg("Call".into(), 900, &now_anchor(), 4, Some(arg)),
                Ok(EngineMsg::InsertTask {
                    title: "Call".into(),
                    planned_secs: 900,
                    anchor: InsertAnchor::Now,
                    expected_rev: 4,
                    lock: Some(choice)
                })
            );
        }
        assert_eq!(lock_day_msg(6), EngineMsg::LockDay { expected_rev: 6 });
        let choice: LockChoiceArg = serde_json::from_str(r#""noLock""#).unwrap();
        assert_eq!(choice, LockChoiceArg::NoLock);
        let choice: LockChoiceArg = serde_json::from_str(r#""lock""#).unwrap();
        assert_eq!(choice, LockChoiceArg::Lock);
        assert_eq!(pause_task_msg(A), Ok(EngineMsg::Pause { item_id: item(A) }));
        assert_eq!(
            switch_task_msg(A, 9),
            Ok(EngineMsg::Switch {
                item_id: item(A),
                expected_rev: 9
            })
        );
        assert_eq!(
            extend_task_msg(A, 300, 9),
            Ok(EngineMsg::ExtendTask {
                item_id: item(A),
                secs: 300,
                expected_rev: 9
            })
        );
        assert_eq!(
            set_tags_msg(A, vec!["deep".into(), " Work ".into()], 9),
            Ok(EngineMsg::SetTags {
                item_id: item(A),
                tags: vec!["deep".into(), " Work ".into()],
                expected_rev: 9
            })
        );
        assert_eq!(
            set_work_end_msg(1080),
            EngineMsg::SetWorkEnd { minutes: 1080 }
        );
        assert_eq!(
            carry_over_to_tomorrow_msg(7),
            EngineMsg::CarryOverToTomorrow { expected_rev: 7 }
        );
        assert_eq!(sign_in_msg(), EngineMsg::SignIn);
        assert_eq!(sign_out_msg(), EngineMsg::SignOut);
        assert_eq!(
            set_google_client_msg(Some("abc".into()), Some("shh".into())),
            EngineMsg::SetGoogleClient {
                client: Some(OAuthClient {
                    id: "abc".into(),
                    secret: Some("shh".into())
                })
            }
        );
        assert_eq!(
            set_google_client_msg(Some("abc".into()), Some("  ".into())),
            EngineMsg::SetGoogleClient {
                client: Some(OAuthClient {
                    id: "abc".into(),
                    secret: None
                })
            }
        );
        for blank in [None, Some(String::new()), Some("  ".into())] {
            assert_eq!(
                set_google_client_msg(blank, Some("shh".into())),
                EngineMsg::SetGoogleClient { client: None }
            );
        }
        assert_eq!(
            exclude_event_msg(A, 9),
            Ok(EngineMsg::ExcludeEvent {
                item_id: item(A),
                expected_rev: 9
            })
        );
        assert_eq!(
            include_event_msg(A, 9),
            Ok(EngineMsg::IncludeEvent {
                item_id: item(A),
                expected_rev: 9
            })
        );
        assert_eq!(
            add_calendar_event_msg("b#c@y".into(), "ev_1".into()),
            EngineMsg::AddCalendarEvent {
                calendar_id: "b#c@y".into(),
                event_id: "ev_1".into()
            }
        );
        assert_eq!(
            set_included_calendars_msg(vec!["a@x".into(), "b#c@y".into()]),
            EngineMsg::SetIncludedCalendars {
                calendar_ids: vec!["a@x".into(), "b#c@y".into()]
            }
        );
        let anchor = |place, id: Option<&str>| InsertAnchorArg {
            place,
            item_id: id.map(Into::into),
        };
        for (arg, expected) in [
            (anchor(InsertPlace::Now, None), InsertAnchor::Now),
            (anchor(InsertPlace::End, None), InsertAnchor::End),
            (anchor(InsertPlace::Now, Some(B)), InsertAnchor::Now),
            (
                anchor(InsertPlace::Before, Some(B)),
                InsertAnchor::Before(item(B)),
            ),
            (
                anchor(InsertPlace::After, Some(B)),
                InsertAnchor::After(item(B)),
            ),
        ] {
            assert_eq!(
                insert_task_msg("Call".into(), 900, &arg, 4, None),
                Ok(EngineMsg::InsertTask {
                    title: "Call".into(),
                    planned_secs: 900,
                    anchor: expected,
                    expected_rev: 4,
                    lock: None
                }),
                "{arg:?}"
            );
        }
        let arg: InsertAnchorArg =
            serde_json::from_str(&format!(r#"{{"place":"after","itemId":"{B}"}}"#)).unwrap();
        assert_eq!(arg, anchor(InsertPlace::After, Some(B)));
        let arg: InsertAnchorArg =
            serde_json::from_str(r#"{"place":"now","itemId":null}"#).unwrap();
        assert_eq!(arg, anchor(InsertPlace::Now, None));
    }

    #[test]
    fn malformed_ids_and_instants_are_errors() {
        for bad in ["", "not-a-uuid", "0192f3a0-0000-7000-8000"] {
            assert!(edit_task_msg(bad, None, None).is_err(), "{bad}");
            assert!(remove_task_msg(bad).is_err(), "{bad}");
            assert!(start_task_msg(bad, 1, None).is_err(), "{bad}");
            assert!(pause_task_msg(bad).is_err(), "{bad}");
            assert!(switch_task_msg(bad, 1).is_err(), "{bad}");
            assert!(extend_task_msg(bad, 300, 1).is_err(), "{bad}");
            assert!(exclude_event_msg(bad, 1).is_err(), "{bad}");
            assert!(include_event_msg(bad, 1).is_err(), "{bad}");
            assert!(set_tags_msg(bad, vec!["t".into()], 1).is_err(), "{bad}");
            assert!(toggle_fixed_msg(bad, None, 1).is_err(), "{bad}");
            let anchor = ReorderAnchorArg {
                place: AnchorPlace::After,
                item_id: B.into(),
            };
            assert!(reorder_task_msg(bad, &anchor, 1).is_err(), "{bad}");
            let bad_anchor = ReorderAnchorArg {
                place: AnchorPlace::After,
                item_id: bad.into(),
            };
            assert!(reorder_task_msg(A, &bad_anchor, 1).is_err(), "{bad}");
            for place in [InsertPlace::Before, InsertPlace::After] {
                let arg = InsertAnchorArg {
                    place,
                    item_id: Some(bad.into()),
                };
                assert!(
                    insert_task_msg("t".into(), 60, &arg, 1, None).is_err(),
                    "{bad}"
                );
            }
        }
        for place in [InsertPlace::Before, InsertPlace::After] {
            let arg = InsertAnchorArg {
                place,
                item_id: None,
            };
            assert!(
                insert_task_msg("t".into(), 60, &arg, 1, None).is_err(),
                "{place:?}"
            );
        }
        for bad in ["", "tomorrow", "2026-11-11", "2026-11-11T09:30:00"] {
            assert!(toggle_fixed_msg(A, Some(bad), 1).is_err(), "{bad}");
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn now_anchor() -> InsertAnchorArg {
        InsertAnchorArg {
            place: InsertPlace::Now,
            item_id: None,
        }
    }

    fn record(id: &str, title: &str, order: &str) -> PlanItemRecord {
        PlanItemRecord {
            id: item(id),
            title: title.into(),
            planned_secs: 1500,
            order: OrderKey::new(order).unwrap(),
            fixed: false,
            pinned_start: None,
            status: ItemStatus::Pending,
            removed_at: None,
            segments: Vec::new(),
            carried_from: None,
            tags: Vec::new(),
            source: None,
            removed_by_source_at: None,
            hidden_at: None,
            excluded: false,
            added: false,
        }
    }

    const F: &str = "0192f3a0-0000-7000-8000-00000000000f";
    const R: &str = "0192f3a0-0000-7000-8000-000000000009";

    /// An event `source_id` of "me@example.com", 1500 s long.
    fn me_ref(source_id: &str) -> CalendarRef {
        CalendarRef {
            calendar_id: "me@example.com".into(),
            source_id: source_id.into(),
            instance_start: None,
            source_secs: 1500,
        }
    }

    /// Fixed F (pinned), flexible A (running, 60 s banked) and B, and removed R.
    fn sample_day() -> Day {
        let f = PlanItemRecord {
            fixed: true,
            pinned_start: Some(ts("2026-11-11T08:00:00Z")),
            tags: vec!["meeting".into(), "Team".into()],
            source: Some(me_ref("standup")),
            ..record(F, "Standup", "V")
        };
        let a = PlanItemRecord {
            status: ItemStatus::Running,
            segments: vec![
                Segment {
                    started_at: ts("2026-11-11T09:00:00Z"),
                    ended_at: Some(ts("2026-11-11T09:01:00.5Z")),
                },
                Segment::open(ts("2026-11-11T09:30:00Z")),
            ],
            ..record(A, "Write", "V")
        };
        let b = record(B, "Review", "k");
        let r = PlanItemRecord {
            status: ItemStatus::Removed,
            removed_at: Some(ts("2026-11-11T07:00:00Z")),
            ..record(R, "Gone", "a")
        };
        let items = [b, r, a, f].map(|rec| PlanItem::restore(rec).unwrap());
        Day::restore("2026-11-11".parse().unwrap(), "Europe/Berlin", items).unwrap()
    }

    #[test]
    fn snapshot_event_carries_the_day_in_schedule_order() {
        let day = sample_day();
        let running = day.item(item(A)).unwrap().running_view();
        let schedule = nguda_core::reflow(&day, ts("2026-11-11T09:40:00Z"));
        let snap = Snapshot {
            rev: 5,
            day: Some(day),
            running,
            next_item: None,
            schedule,
            work_end: WorkEnd::from_minutes(18 * 60 + 5).unwrap(),
            overflow: vec![item(B)],
            status: vec![StatusEntry {
                key: StatusKey::CommandRejected,
                message: "stale revision".into(),
            }],
            known_tags: vec!["meeting".into(), "old".into(), "Team".into()],
            google: GoogleAccount::SignedIn,
            google_client: GoogleClient {
                custom_id: Some("my-client".into()),
                available: true,
            },
            calendars: vec![
                CalendarEntry {
                    id: "me@example.com".into(),
                    name: "Me".into(),
                    primary: true,
                    included: true,
                },
                CalendarEntry {
                    id: "team@group".into(),
                    name: "Team".into(),
                    primary: false,
                    included: false,
                },
            ],
            autostart: Some(true),
        };
        let ev = SnapshotEvent::from(&snap);
        assert_eq!(ev.rev, 5);
        assert_eq!(ev.autostart, Some(true));
        assert_eq!(ev.google, GoogleAccountView::SignedIn);
        assert_eq!(
            ev.google_client,
            GoogleClientView {
                custom_id: Some("my-client".into()),
                available: true,
            }
        );
        assert_eq!(
            ev.calendars,
            vec![
                CalendarView {
                    id: "me@example.com".into(),
                    name: "Me".into(),
                    primary: true,
                    included: true,
                },
                CalendarView {
                    id: "team@group".into(),
                    name: "Team".into(),
                    primary: false,
                    included: false,
                },
            ]
        );
        assert_eq!(ev.known_tags, ["meeting", "old", "Team"]);
        assert_eq!(ev.work_end, "18:05");
        assert_eq!(ev.overflow, [B]);
        assert_eq!(
            ev.day,
            Some(DayView {
                date: "2026-11-11".into(),
                tz: "Europe/Berlin".into(),
                lock_decision: LockDecisionView::None,
            })
        );
        let ids: Vec<&str> = ev.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, [F, A, B], "schedule order, removed R excluded");
        assert_eq!(
            ev.items[0],
            ItemView {
                id: F.into(),
                title: "Standup".into(),
                planned_secs: 1500,
                fixed: true,
                pinned_start: Some("2026-11-11T08:00:00Z".into()),
                status: ItemStatusView::Pending,
                actual_secs: 0,
                scheduled_start: Some("2026-11-11T08:00:00Z".into()),
                scheduled_end: Some("2026-11-11T08:25:00Z".into()),
                interrupted: false,
                tags: vec!["meeting".into(), "Team".into()],
                imported: true,
                excluded: false,
            }
        );
        assert!(ev.items[1].tags.is_empty());
        assert!(!ev.items[1].imported, "a local item");
        // A ends at its planned end (1500 s less 60 banked); B flows after it.
        let times = |n: usize| {
            let i = &ev.items[n];
            (i.scheduled_start.as_deref(), i.scheduled_end.as_deref())
        };
        assert_eq!(
            times(1),
            (Some("2026-11-11T09:30:00Z"), Some("2026-11-11T09:54:00Z"))
        );
        assert_eq!(
            times(2),
            (Some("2026-11-11T09:54:00Z"), Some("2026-11-11T10:19:00Z"))
        );
        assert_eq!(ev.items[1].status, ItemStatusView::Running);
        assert_eq!(ev.items[1].actual_secs, 60, "closed segments only");
        assert_eq!(ev.items[1].pinned_start, None);
        assert_eq!(
            ev.running,
            Some(RunningView {
                item_id: A.into(),
                title: "Write".into(),
                segment_started_at: "2026-11-11T09:30:00Z".into(),
                accumulated_secs: 60,
            })
        );
        assert_eq!(
            ev.status,
            [StatusView {
                key: StatusKeyView::CommandRejected,
                message: "stale revision".into()
            }]
        );
    }

    #[test]
    fn snapshot_event_marks_interrupted_items() {
        const N: &str = "0192f3a0-0000-7000-8000-00000000000e";
        let mut day = sample_day();
        let now = ts("2026-11-11T09:40:00Z");
        let n = PlanItem::restore(record(N, "Call", "U")).unwrap();
        day.interrupt(item(A), n, now).unwrap();
        let schedule = nguda_core::reflow(&day, now);
        let snap = Snapshot {
            rev: 6,
            day: Some(day),
            schedule,
            ..Default::default()
        };
        let ev = SnapshotEvent::from(&snap);
        let flags: Vec<(&str, ItemStatusView, bool)> = ev
            .items
            .iter()
            .map(|i| (i.id.as_str(), i.status, i.interrupted))
            .collect();
        assert_eq!(
            flags,
            [
                (F, ItemStatusView::Pending, false),
                (N, ItemStatusView::Running, false),
                (A, ItemStatusView::Paused, true),
                (B, ItemStatusView::Pending, false),
            ]
        );
    }

    #[test]
    fn status_keys_and_item_statuses_serialize_as_camel_case() {
        let key = |k| serde_json::to_string(&StatusKeyView::from(k)).unwrap();
        assert_eq!(key(StatusKey::Store), "\"store\"");
        assert_eq!(key(StatusKey::CommandRejected), "\"commandRejected\"");
        assert_eq!(key(StatusKey::Overflow), "\"overflow\"");
        assert_eq!(key(StatusKey::Export), "\"export\"");
        assert_eq!(key(StatusKey::Sync), "\"sync\"");
        assert_eq!(key(StatusKey::SecretStore), "\"secretStore\"");
        let client = serde_json::to_string(&GoogleClientView {
            custom_id: Some("abc".into()),
            available: false,
        })
        .unwrap();
        assert_eq!(client, r#"{"customId":"abc","available":false}"#);
        let google = |g| serde_json::to_string(&GoogleAccountView::from(g)).unwrap();
        assert_eq!(google(GoogleAccount::Unknown), "\"unknown\"");
        assert_eq!(google(GoogleAccount::SignedOut), "\"signedOut\"");
        assert_eq!(google(GoogleAccount::SigningIn), "\"signingIn\"");
        assert_eq!(google(GoogleAccount::SignedIn), "\"signedIn\"");
        let status = |s| serde_json::to_string(&ItemStatusView::from(s)).unwrap();
        assert_eq!(status(ItemStatus::CarriedOver), "\"carriedOver\"");
        assert_eq!(status(ItemStatus::Paused), "\"paused\"");
        let decision = |d| serde_json::to_string(&LockDecisionView::from(d)).unwrap();
        assert_eq!(decision(LockDecision::None), "\"none\"");
        assert_eq!(decision(LockDecision::Locked), "\"locked\"");
        assert_eq!(decision(LockDecision::NoLock), "\"noLock\"");
    }

    #[test]
    fn snapshot_event_carries_the_lock_decision() {
        let mut day = sample_day();
        day.decide_lock(LockChoice::Lock).unwrap();
        let snap = Snapshot {
            day: Some(day),
            ..Default::default()
        };
        let ev = SnapshotEvent::from(&snap);
        assert_eq!(ev.day.unwrap().lock_decision, LockDecisionView::Locked);
    }

    #[test]
    fn pre_startup_snapshot_has_no_day_and_no_items() {
        let ev = SnapshotEvent::from(&Snapshot::default());
        assert_eq!(
            ev,
            SnapshotEvent {
                rev: 0,
                day: None,
                items: vec![],
                running: None,
                work_end: "17:00".into(),
                overflow: vec![],
                status: vec![],
                known_tags: vec![],
                google: GoogleAccountView::Unknown,
                calendars: vec![],
                google_client: GoogleClientView {
                    custom_id: None,
                    available: false,
                },
                autostart: None,
            }
        );
    }

    #[test]
    fn autostart_maps_into_the_view_and_the_observed_message() {
        for autostart in [None, Some(false), Some(true)] {
            let snap = Snapshot {
                autostart,
                ..Default::default()
            };
            assert_eq!(SnapshotEvent::from(&snap).autostart, autostart);
        }
        assert_eq!(
            autostart_observed_msg(true),
            EngineMsg::AutostartObserved { enabled: true }
        );
        assert_eq!(
            autostart_observed_msg(false),
            EngineMsg::AutostartObserved { enabled: false }
        );
    }

    #[test]
    fn snapshot_event_formats_sub_second_instants() {
        let mut snap = Snapshot {
            rev: 3,
            day: Some(sample_day()),
            ..Default::default()
        };
        snap.running = Some(Running {
            item_id: item(A),
            title: "Write".into(),
            segment_started_at: Timestamp::from_millisecond(1_500).unwrap(),
            accumulated_secs: 0,
        });
        let r = SnapshotEvent::from(&snap).running.unwrap();
        assert_eq!(r.item_id, A);
        assert_eq!(r.segment_started_at, "1970-01-01T00:00:01.5Z");
    }

    #[test]
    fn query_day_dates_parse_only_as_yyyy_mm_dd() {
        assert_eq!(parse_date("2026-11-21"), Ok(Date::constant(2026, 11, 21)));
        for bad in [
            "",
            "2026-13-40",
            "2026-02-30",
            "2026-1-01",
            "20261121",
            "2026-11-21T00:00",
            " 2026-11-21",
            "2026/11/21",
            "+002026-11",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn query_week_dates_parse_into_the_week_holding_them() {
        assert_eq!(week_of("2027-01-01"), Ok(IsoWeek::new(2026, 53).unwrap()));
        assert!(week_of("2026-13-40").is_err());
        assert_eq!(week_of("9999-12-31"), Err("date out of range".to_owned()));
    }

    fn range_arg(kind: ExportKind, date: Option<&str>) -> ExportRangeArg {
        ExportRangeArg {
            kind,
            date: date.map(str::to_owned),
        }
    }

    #[test]
    fn export_ranges_parse_before_any_dialog() {
        let w47 = ExportRange::Week(IsoWeek::new(2026, 47).unwrap());
        assert_eq!(
            export_range_of(&range_arg(ExportKind::Week, Some("2026-11-21"))),
            Ok(w47)
        );
        assert_eq!(
            export_range_of(&range_arg(ExportKind::All, None)),
            Ok(ExportRange::All)
        );
        assert_eq!(
            export_range_of(&range_arg(ExportKind::All, Some("junk"))),
            Ok(ExportRange::All),
            "all ignores the date"
        );
        assert!(export_range_of(&range_arg(ExportKind::Week, Some("2026-13-40"))).is_err());
        assert_eq!(
            export_range_of(&range_arg(ExportKind::Week, None)),
            Err("a week export needs a date".to_owned())
        );
        let arg: ExportRangeArg =
            serde_json::from_str(r#"{"kind":"week","date":"2026-11-21"}"#).unwrap();
        assert_eq!(export_range_of(&arg), Ok(w47));
        let arg: ExportRangeArg = serde_json::from_str(r#"{"kind":"all","date":null}"#).unwrap();
        assert_eq!(arg.kind, ExportKind::All);
    }

    #[test]
    fn the_default_export_file_name_names_the_week_or_all() {
        let week = |y, w| ExportRange::Week(IsoWeek::new(y, w).unwrap());
        assert_eq!(default_file_name(week(2026, 3)), "nguda-2026-W03.csv");
        assert_eq!(default_file_name(week(2026, 53)), "nguda-2026-W53.csv");
        assert_eq!(default_file_name(ExportRange::All), "nguda-all.csv");
    }

    #[test]
    fn a_cancelled_save_dialog_sends_nothing() {
        assert_eq!(export_msg(ExportRange::All, None), None);
        let path = std::path::PathBuf::from("/tmp/x.csv");
        assert_eq!(
            export_msg(ExportRange::All, Some(path.clone())),
            Some(EngineMsg::ExportCsv {
                path,
                range: ExportRange::All
            })
        );
    }

    #[test]
    fn export_done_carries_its_row_count() {
        let json = serde_json::to_value(ExportDoneEvent { rows: 7 }).unwrap();
        assert_eq!(json, serde_json::json!({ "rows": 7 }));
    }

    fn week_report(year: i16, week: i8, overall: GapFigure) -> WeekReport {
        WeekReport {
            week: IsoWeek::new(year, week).unwrap(),
            overall,
            tags: vec![nguda_core::TagGap {
                tag: "proposals".into(),
                figure: overall,
            }],
            counted_days: vec![Date::constant(2026, 11, 16)],
            unlocked_days: vec![Date::constant(2026, 11, 17), Date::constant(2026, 11, 18)],
        }
    }

    #[test]
    fn week_report_view_converts_the_ratio_counts_and_neighbours() {
        let figure = GapFigure {
            items: 2,
            actual_secs: 1600,
            planned_secs: 1000,
        };
        let v = WeekReportView::from(&week_report(2026, 47, figure));
        let gap = GapView {
            ratio: Some(1.6),
            items: 2,
            actual_secs: 1600,
            planned_secs: 1000,
        };
        assert_eq!(
            v,
            WeekReportView {
                year: 2026,
                week: 47,
                start: "2026-11-16".into(),
                end: "2026-11-22".into(),
                previous_start: Some("2026-11-09".into()),
                next_start: Some("2026-11-23".into()),
                overall: gap.clone(),
                tags: vec![TagGapView {
                    tag: "proposals".into(),
                    gap,
                }],
                counted_days: vec!["2026-11-16".into()],
                unlocked_days: vec!["2026-11-17".into(), "2026-11-18".into()],
            }
        );
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["overall"]["ratio"], 1.6);
        assert_eq!(json["overall"]["actualSecs"], 1600);
        assert_eq!(json["previousStart"], "2026-11-09");
        assert_eq!(json["unlockedDays"][1], "2026-11-18");
    }

    #[test]
    fn week_report_view_has_a_null_ratio_without_planned_time_and_crosses_years() {
        let v = WeekReportView::from(&week_report(2026, 53, GapFigure::default()));
        assert_eq!(v.overall.ratio, None);
        assert_eq!(
            serde_json::to_value(&v).unwrap()["overall"]["ratio"],
            serde_json::Value::Null
        );
        assert_eq!(
            (v.start.as_str(), v.end.as_str()),
            ("2026-12-28", "2027-01-03")
        );
        assert_eq!(v.previous_start.as_deref(), Some("2026-12-21"));
        assert_eq!(v.next_start.as_deref(), Some("2027-01-04"));
        let last = WeekReportView::from(&week_report(9999, 51, GapFigure::default()));
        assert_eq!(last.next_start, None);
    }

    #[test]
    fn day_report_view_converts_every_field() {
        use nguda_core::{ActualSegment, Span};
        let report = DayReport {
            date: Date::constant(2026, 11, 21),
            lock_decision: LockDecision::Locked,
            locked_at: Some(ts("2026-11-21T08:00:00Z")),
            rows: vec![
                ReportRow {
                    item_id: item(A),
                    title: "Write".into(),
                    status: ItemStatus::CarriedOver,
                    carried_from: Some(item(B)),
                    tags: vec!["deep".into(), "Work".into()],
                    planned_secs: Some(1500),
                    planned_source: PlannedSource::Baseline,
                    actual_secs: 1900,
                    difference_secs: Some(400),
                },
                ReportRow {
                    item_id: item(B),
                    title: "Call".into(),
                    status: ItemStatus::Done,
                    carried_from: None,
                    tags: Vec::new(),
                    planned_secs: None,
                    planned_source: PlannedSource::Unplanned,
                    actual_secs: u64::MAX,
                    difference_secs: None,
                },
                ReportRow {
                    item_id: item(F),
                    title: "Huge".into(),
                    status: ItemStatus::Removed,
                    carried_from: None,
                    tags: Vec::new(),
                    planned_secs: Some(u64::MAX),
                    planned_source: PlannedSource::Unlocked,
                    actual_secs: 0,
                    difference_secs: Some(i64::MIN),
                },
            ],
            baseline: vec![
                BaselineItem {
                    item_id: item(A),
                    planned_secs: 1500,
                    span: Some(Span {
                        start: ts("2026-11-21T09:00:00Z"),
                        end: ts("2026-11-21T09:25:00Z"),
                    }),
                },
                BaselineItem {
                    item_id: item(F),
                    planned_secs: 60,
                    span: None,
                },
            ],
            segments: vec![ActualSegment {
                item_id: item(A),
                start: ts("2026-11-21T09:00:00Z"),
                end: ts("2026-11-21T09:31:40Z"),
                open: true,
            }],
            markers: [
                DayLogKind::Recovered { item_id: item(A) },
                DayLogKind::Removed { item_id: item(A) },
                DayLogKind::Switched {
                    from: item(A),
                    to: Some(item(B)),
                },
                DayLogKind::Switched {
                    from: item(A),
                    to: None,
                },
                DayLogKind::Extended {
                    item_id: item(A),
                    secs: 300,
                },
                DayLogKind::Inserted { item_id: item(B) },
                DayLogKind::CarriedOver {
                    item_id: item(A),
                    to: item(B),
                },
                DayLogKind::CalendarChanged { item_id: item(B) },
            ]
            .into_iter()
            .map(|kind| DayLogEntry {
                at: ts("2026-11-21T10:00:00Z"),
                kind,
            })
            .collect(),
        };
        let v = DayReportView::from(&report);
        assert_eq!(v.date, "2026-11-21");
        assert_eq!(v.lock_decision, LockDecisionView::Locked);
        assert_eq!(v.locked_at.as_deref(), Some("2026-11-21T08:00:00Z"));
        assert_eq!(
            v.rows[0],
            ReportRowView {
                item_id: A.into(),
                title: "Write".into(),
                status: ItemStatusView::CarriedOver,
                carried_from: Some(B.into()),
                tags: vec!["deep".into(), "Work".into()],
                planned_secs: Some(1500),
                planned_source: PlannedSourceView::Baseline,
                actual_secs: 1900,
                difference_secs: Some(400),
            }
        );
        assert_eq!(v.rows[1].planned_secs, None);
        assert_eq!(v.rows[1].planned_source, PlannedSourceView::Unplanned);
        assert_eq!(v.rows[1].actual_secs, u32::MAX, "saturating");
        assert_eq!(v.rows[1].difference_secs, None);
        assert_eq!(v.rows[2].planned_secs, Some(u32::MAX));
        assert_eq!(v.rows[2].planned_source, PlannedSourceView::Unlocked);
        assert_eq!(v.rows[2].difference_secs, Some(i32::MIN), "saturating");
        assert_eq!(saturating_i32(i64::MAX), i32::MAX);
        assert_eq!(
            v.baseline,
            [
                BaselineSlotView {
                    item_id: A.into(),
                    planned_secs: 1500,
                    scheduled_start: Some("2026-11-21T09:00:00Z".into()),
                    scheduled_end: Some("2026-11-21T09:25:00Z".into()),
                },
                BaselineSlotView {
                    item_id: F.into(),
                    planned_secs: 60,
                    scheduled_start: None,
                    scheduled_end: None,
                },
            ]
        );
        assert_eq!(
            v.segments,
            [SegmentView {
                item_id: A.into(),
                start: "2026-11-21T09:00:00Z".into(),
                end: "2026-11-21T09:31:40Z".into(),
                open: true,
            }]
        );
        let markers: Vec<(MarkerKindView, &str, Option<&str>, Option<u32>)> = v
            .markers
            .iter()
            .map(|m| {
                assert_eq!(m.at, "2026-11-21T10:00:00Z");
                (m.kind, m.item_id.as_str(), m.to_item_id.as_deref(), m.secs)
            })
            .collect();
        assert_eq!(
            markers,
            [
                (MarkerKindView::Recovered, A, None, None),
                (MarkerKindView::Removed, A, None, None),
                (MarkerKindView::Switched, A, Some(B), None),
                (MarkerKindView::Switched, A, None, None),
                (MarkerKindView::Extended, A, None, Some(300)),
                (MarkerKindView::Inserted, B, None, None),
                (MarkerKindView::CarriedOver, A, Some(B), None),
                (MarkerKindView::CalendarChanged, B, None, None),
            ]
        );
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["lockDecision"], "locked");
        assert_eq!(json["rows"][0]["plannedSource"], "baseline");
        assert_eq!(json["rows"][0]["differenceSecs"], 400);
        assert_eq!(json["rows"][0]["carriedFrom"], B);
        assert_eq!(json["markers"][6]["kind"], "carriedOver");
        assert_eq!(json["markers"][6]["toItemId"], B);
        assert_eq!(json["markers"][7]["kind"], "calendarChanged");
    }

    const E1: &str = "0192f3a0-0000-7000-8000-0000000000e1";
    const E2: &str = "0192f3a0-0000-7000-8000-0000000000e2";

    /// An imported, excluded pending record pinned at `pin`.
    fn excluded_record(id: &str, source_id: &str, pin: &str) -> PlanItemRecord {
        PlanItemRecord {
            fixed: true,
            pinned_start: Some(ts(pin)),
            source: Some(me_ref(source_id)),
            excluded: true,
            ..record(id, "Skipped", "V")
        }
    }

    #[test]
    fn excluded_events_follow_the_schedule_by_pin_with_no_scheduled_times() {
        let mut items: Vec<PlanItem> = sample_day().items().to_vec();
        items.push(PlanItem::restore(excluded_record(E2, "late", "2026-11-11T15:00:00Z")).unwrap());
        items
            .push(PlanItem::restore(excluded_record(E1, "early", "2026-11-11T07:00:00Z")).unwrap());
        let gone = PlanItemRecord {
            status: ItemStatus::Removed,
            removed_at: Some(ts("2026-11-11T07:30:00Z")),
            ..excluded_record(
                "0192f3a0-0000-7000-8000-0000000000e3",
                "gone",
                "2026-11-11T06:00:00Z",
            )
        };
        items.push(PlanItem::restore(gone).unwrap());
        let day = Day::restore("2026-11-11".parse().unwrap(), "Europe/Berlin", items).unwrap();
        let schedule = nguda_core::reflow(&day, ts("2026-11-11T09:40:00Z"));
        let snap = Snapshot {
            rev: 2,
            day: Some(day),
            schedule,
            ..Snapshot::default()
        };
        let ev = SnapshotEvent::from(&snap);
        let ids: Vec<&str> = ev.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            [F, A, B, E1, E2],
            "excluded last, by pin; removed left out"
        );
        for i in &ev.items[3..] {
            assert!(i.excluded && i.imported);
            assert_eq!(i.scheduled_start, None);
            assert_eq!(i.scheduled_end, None);
            assert_eq!(i.status, ItemStatusView::Pending);
        }
        assert_eq!(
            ev.items[3].pinned_start.as_deref(),
            Some("2026-11-11T07:00:00Z")
        );
        assert!(ev.items[..3].iter().all(|i| !i.excluded));
    }

    #[test]
    fn calendar_events_and_query_errors_cross_as_views() {
        let e = CalendarEvent {
            calendar_id: "b#c@y".into(),
            event_id: "ev_1".into(),
            title: "Review".into(),
            start: ts("2026-11-21T09:00:00Z"),
            end: ts("2026-11-21T09:30:00Z"),
            in_plan: true,
        };
        assert_eq!(
            CalendarEventView::from(&e),
            CalendarEventView {
                calendar_id: "b#c@y".into(),
                event_id: "ev_1".into(),
                title: "Review".into(),
                start: "2026-11-21T09:00:00Z".into(),
                end: "2026-11-21T09:30:00Z".into(),
                in_plan: true,
            }
        );
        let json = serde_json::to_value(CalendarEventView::from(&e)).unwrap();
        assert_eq!(json["eventId"], "ev_1");
        assert_eq!(json["inPlan"], true);
        assert_eq!(query_error_text(&QueryError::Stale), "stale");
        assert_eq!(
            query_error_text(&QueryError::EngineClosed),
            "engine is not running"
        );
        let other = query_error_text(&QueryError::Calendar(CalendarErrorKind::Other));
        assert_ne!(other, "stale");
        let marker = |kind| {
            marker_view(&DayLogEntry {
                at: ts("2026-11-21T10:00:00Z"),
                kind,
            })
            .kind
        };
        assert_eq!(
            marker(DayLogKind::Excluded { item_id: item(A) }),
            MarkerKindView::Excluded
        );
        assert_eq!(
            marker(DayLogKind::Included { item_id: item(A) }),
            MarkerKindView::Included
        );
    }

    /// The item without its scheduled times, which follow the system clock.
    fn unscheduled(i: ItemView) -> ItemView {
        assert!(i.scheduled_start.is_some() && i.scheduled_end.is_some());
        ItemView {
            scheduled_start: None,
            scheduled_end: None,
            ..i
        }
    }

    /// The Restart rows: a task created and a work-end set through the command path
    /// survive a new engine on the same SQLite file.
    #[tokio::test(flavor = "multi_thread")]
    async fn created_task_survives_an_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nguda.sqlite3");
        let spawn = || {
            let store = SqliteStore::open(&path).unwrap();
            nguda_engine::spawn(crate::test_deps(store)).0
        };
        let wait = Duration::from_secs(10);

        let engine = spawn();
        engine.send(set_work_end_msg(18 * 60)).unwrap();
        engine.send(create_task_msg("Write".into(), 1500)).unwrap();
        let mut rx = engine.subscribe();
        // Handled in order: once the task shows, the work-end is committed too.
        let created = timeout(
            wait,
            rx.wait_for(|s| !SnapshotEvent::from(s).items.is_empty()),
        )
        .await
        .expect("task created in time")
        .map(|s| unscheduled(SnapshotEvent::from(&*s).items[0].clone()))
        .unwrap();
        assert_eq!(
            (created.title.as_str(), created.planned_secs),
            ("Write", 1500)
        );
        drop(rx);
        drop(engine);

        // The old engine task exits once its handle is gone and drops its store,
        // whose worker closes the connection; opening a second one is fine either way.
        let engine = spawn();
        let mut rx = engine.subscribe();
        let restored = timeout(wait, rx.wait_for(|s| s.rev >= 1))
            .await
            .expect("engine started in time")
            .map(|s| SnapshotEvent::from(&*s))
            .unwrap();
        assert_eq!(restored.work_end, "18:00");
        let restored: Vec<ItemView> = restored.items.into_iter().map(unscheduled).collect();
        assert_eq!(restored, [created]);
    }
}
