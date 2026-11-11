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

use nguda_core::{Day, ItemId, ItemStatus, PlanItem, Slot, Timestamp, Uuid, closed_secs};
use nguda_engine::{EngineHandle, EngineMsg, InsertAnchor, ReorderAnchor, Snapshot, StatusKey};
use serde::{Deserialize, Serialize};
#[cfg(any(debug_assertions, test))]
use specta_typescript::Typescript;
use tauri::{State, Wry};
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

/// Starts or resumes an item now, pausing whichever item is running.
#[tauri::command]
#[specta::specta]
pub fn start_task(
    engine: State<'_, EngineHandle>,
    item_id: String,
    expected_rev: u32,
) -> Result<(), String> {
    send(&engine, start_task_msg(&item_id, expected_rev))
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

/// Inserts a new task at `anchor`; `now` starts it, pausing the running item.
#[tauri::command]
#[specta::specta]
pub fn insert_task(
    engine: State<'_, EngineHandle>,
    title: String,
    planned_secs: u32,
    anchor: InsertAnchorArg,
    expected_rev: u32,
) -> Result<(), String> {
    send(
        &engine,
        insert_task_msg(title, planned_secs, &anchor, expected_rev),
    )
}

/// Sets the work-end time of day, `minutes` after local midnight. The engine
/// rejects values outside `0..=1439`.
#[tauri::command]
#[specta::specta]
pub fn set_work_end(engine: State<'_, EngineHandle>, minutes: u32) -> Result<(), String> {
    send(&engine, Ok(set_work_end_msg(minutes)))
}

// --- Argument parsing: syntax only, no semantic checks ---

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

fn start_task_msg(item_id: &str, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Start {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
    })
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

fn set_work_end_msg(minutes: u32) -> EngineMsg {
    EngineMsg::SetWorkEnd { minutes }
}

fn insert_task_msg(
    title: String,
    planned_secs: u32,
    anchor: &InsertAnchorArg,
    expected_rev: u32,
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
    })
}

/// The tray's Extend… item was picked: the view focuses the running row's
/// extend-minutes field. Carries nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, specta::Type, Event)]
pub struct FocusExtendEvent;

/// The last engine snapshot, as the view sees it. Also the reply of
/// [`current_snapshot`], so a window that loads late still gets the latest state.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    pub rev: u32,
    /// Today. `null` only before startup finishes (rev 0).
    pub day: Option<DayView>,
    /// Today's non-removed items in schedule order (`Snapshot.schedule`). The view
    /// shows them as given and never re-sorts.
    pub items: Vec<ItemView>,
    pub running: Option<RunningView>,
    /// The work-end time of day, `HH:MM` local.
    pub work_end: String,
    /// UUIDs of the flexible items that end after work-end, in schedule order. The
    /// view renders them as given and never computes overflow itself.
    pub overflow: Vec<String>,
    /// One entry per status key, sorted by key.
    pub status: Vec<StatusView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DayView {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    /// The IANA zone fixed for this day.
    pub tz: String,
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
}

impl From<StatusKey> for StatusKeyView {
    fn from(k: StatusKey) -> Self {
        match k {
            StatusKey::Store => Self::Store,
            StatusKey::CommandRejected => Self::CommandRejected,
            StatusKey::Overflow => Self::Overflow,
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

impl From<&Snapshot> for SnapshotEvent {
    fn from(s: &Snapshot) -> Self {
        let items: Vec<ItemView> = s
            .day
            .as_ref()
            .map(|day| {
                s.schedule
                    .slots()
                    .iter()
                    .filter_map(|slot| day.item(slot.item_id).map(|i| item_view(day, i, slot)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            rev: saturating_u32(s.rev),
            day: s.day.as_ref().map(|d| DayView {
                date: d.date().to_string(),
                tz: d.tz().to_owned(),
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

fn item_view(day: &Day, i: &PlanItem, slot: &Slot) -> ItemView {
    ItemView {
        id: i.id().to_string(),
        title: i.title().to_owned(),
        planned_secs: saturating_u32(i.planned_secs()),
        fixed: i.fixed(),
        pinned_start: i.pinned_start().map(|t| t.to_string()),
        status: i.status().into(),
        actual_secs: saturating_u32(closed_secs(i.segments())),
        scheduled_start: slot.span.map(|sp| sp.start.to_string()),
        scheduled_end: slot.span.map(|sp| sp.end.to_string()),
        interrupted: day.interrupt_stack().contains(&i.id()),
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

pub fn specta_builder() -> Builder<Wry> {
    Builder::<Wry>::new()
        .commands(collect_commands![
            create_task,
            edit_task,
            reorder_task,
            toggle_fixed,
            remove_task,
            start_task,
            pause_task,
            switch_task,
            extend_task,
            insert_task,
            set_work_end,
            current_snapshot
        ])
        .events(collect_events![SnapshotEvent, FocusExtendEvent])
}

#[cfg(any(debug_assertions, test))]
pub fn export_bindings(builder: &Builder<Wry>, path: &str) -> Result<(), specta_typescript::Error> {
    builder.export(Typescript::default(), path)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use nguda_adapters::{SqliteStore, SystemClock, UuidV7Ids};
    use nguda_core::{OrderKey, PlanItemRecord, Running, Segment, TimeZone, WorkEnd};
    use nguda_engine::{EngineDeps, StatusEntry};
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
            "pauseTask",
            "switchTask",
            "extendTask",
            "insertTask",
            "setWorkEnd",
            "currentSnapshot",
        ] {
            assert!(committed.contains(&format!("\t{name}: ")), "{name}");
        }
        assert!(!committed.contains("startDemo"));
        assert!(committed.contains("focusExtendEvent"));
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
            start_task_msg(A, 4),
            Ok(EngineMsg::Start {
                item_id: item(A),
                expected_rev: 4
            })
        );
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
            set_work_end_msg(1080),
            EngineMsg::SetWorkEnd { minutes: 1080 }
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
                insert_task_msg("Call".into(), 900, &arg, 4),
                Ok(EngineMsg::InsertTask {
                    title: "Call".into(),
                    planned_secs: 900,
                    anchor: expected,
                    expected_rev: 4
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
            assert!(start_task_msg(bad, 1).is_err(), "{bad}");
            assert!(pause_task_msg(bad).is_err(), "{bad}");
            assert!(switch_task_msg(bad, 1).is_err(), "{bad}");
            assert!(extend_task_msg(bad, 300, 1).is_err(), "{bad}");
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
                assert!(insert_task_msg("t".into(), 60, &arg, 1).is_err(), "{bad}");
            }
        }
        for place in [InsertPlace::Before, InsertPlace::After] {
            let arg = InsertAnchorArg {
                place,
                item_id: None,
            };
            assert!(
                insert_task_msg("t".into(), 60, &arg, 1).is_err(),
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
        }
    }

    const F: &str = "0192f3a0-0000-7000-8000-00000000000f";
    const R: &str = "0192f3a0-0000-7000-8000-000000000009";

    /// Fixed F (pinned), flexible A (running, 60 s banked) and B, and removed R.
    fn sample_day() -> Day {
        let f = PlanItemRecord {
            fixed: true,
            pinned_start: Some(ts("2026-11-11T08:00:00Z")),
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
        };
        let ev = SnapshotEvent::from(&snap);
        assert_eq!(ev.rev, 5);
        assert_eq!(ev.work_end, "18:05");
        assert_eq!(ev.overflow, [B]);
        assert_eq!(
            ev.day,
            Some(DayView {
                date: "2026-11-11".into(),
                tz: "Europe/Berlin".into()
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
            }
        );
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
        let status = |s| serde_json::to_string(&ItemStatusView::from(s)).unwrap();
        assert_eq!(status(ItemStatus::CarriedOver), "\"carriedOver\"");
        assert_eq!(status(ItemStatus::Paused), "\"paused\"");
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
            }
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
            nguda_engine::spawn(EngineDeps {
                clock: Arc::new(SystemClock),
                store: Arc::new(store),
                ids: Arc::new(UuidV7Ids),
                tz: TimeZone::UTC,
            })
            .0
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
