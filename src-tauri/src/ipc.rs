//! The typed IPC contract (AD-5). `src/bindings.ts` is generated from this module.
//!
//! specta-typescript refuses 64-bit integers, so engine `u64` counters cross IPC as
//! saturating `u32` (a TS `number`) and instants as RFC 3339 strings. `f64` was not
//! used because specta types it `number | null` (NaN serializes as null). Command
//! arguments follow the same convention: item ids are UUID strings, instants RFC
//! 3339 strings, and revs and seconds `u32`.
//!
//! Commands are fire-and-forget (AD-4). Each one only parses its arguments, failing
//! with `Err` on malformed input without sending anything; every semantic check
//! happens in the engine, which reports rejections through `Snapshot.status`.

use nguda_core::{ItemId, Timestamp, Uuid};
use nguda_engine::{EngineHandle, EngineMsg, ReorderAnchor, Snapshot};
use serde::{Deserialize, Serialize};
use specta_typescript::Typescript;
use tauri::{State, Wry};
use tauri_specta::{Builder, Event, collect_commands, collect_events};

/// Where debug builds write the bindings, relative to this crate.
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

pub fn create_task_msg(title: String, planned_secs: u32) -> EngineMsg {
    EngineMsg::CreateTask {
        title,
        planned_secs: planned_secs.into(),
    }
}

pub fn edit_task_msg(
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

pub fn reorder_task_msg(
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

pub fn toggle_fixed_msg(
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

pub fn remove_task_msg(item_id: &str) -> Result<EngineMsg, String> {
    Ok(EngineMsg::RemoveTask {
        item_id: parse_item_id(item_id)?,
    })
}

pub fn start_task_msg(item_id: &str, expected_rev: u32) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Start {
        item_id: parse_item_id(item_id)?,
        expected_rev: expected_rev.into(),
    })
}

pub fn pause_task_msg(item_id: &str) -> Result<EngineMsg, String> {
    Ok(EngineMsg::Pause {
        item_id: parse_item_id(item_id)?,
    })
}

/// The last engine snapshot, as the view sees it.
#[derive(Clone, Debug, PartialEq, Serialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    pub rev: u32,
    pub running: Option<RunningView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RunningView {
    pub title: String,
    /// RFC 3339 UTC instant, e.g. `2026-11-10T09:00:00Z`.
    pub segment_started_at: String,
    pub accumulated_secs: u32,
}

impl From<&Snapshot> for SnapshotEvent {
    fn from(s: &Snapshot) -> Self {
        Self {
            rev: saturating_u32(s.rev),
            running: s.running.as_ref().map(|r| RunningView {
                title: r.title.clone(),
                segment_started_at: r.segment_started_at.to_string(),
                accumulated_secs: saturating_u32(r.accumulated_secs),
            }),
        }
    }
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
            pause_task
        ])
        .events(collect_events![SnapshotEvent])
}

pub fn export_bindings(builder: &Builder<Wry>, path: &str) -> Result<(), specta_typescript::Error> {
    builder.export(Typescript::default(), path)
}

#[cfg(test)]
mod tests {
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
    fn committed_bindings_export_the_plan_commands_only() {
        let committed = std::fs::read_to_string(BINDINGS_PATH).unwrap();
        for name in [
            "createTask",
            "editTask",
            "reorderTask",
            "toggleFixed",
            "removeTask",
            "startTask",
            "pauseTask",
        ] {
            assert!(committed.contains(&format!("\t{name}: ")), "{name}");
        }
        assert!(!committed.contains("startDemo"));
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
    }

    #[test]
    fn malformed_ids_and_instants_are_errors() {
        for bad in ["", "not-a-uuid", "0192f3a0-0000-7000-8000"] {
            assert!(edit_task_msg(bad, None, None).is_err(), "{bad}");
            assert!(remove_task_msg(bad).is_err(), "{bad}");
            assert!(start_task_msg(bad, 1).is_err(), "{bad}");
            assert!(pause_task_msg(bad).is_err(), "{bad}");
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
        }
        for bad in ["", "tomorrow", "2026-11-11", "2026-11-11T09:30:00"] {
            assert!(toggle_fixed_msg(A, Some(bad), 1).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_event_carries_title_and_rev() {
        let snap = Snapshot {
            rev: 3,
            running: Some(nguda_core::Running {
                title: "Demo".into(),
                segment_started_at: nguda_core::Timestamp::from_millisecond(1_500).unwrap(),
                accumulated_secs: 0,
            }),
            ..Default::default()
        };
        let ev = SnapshotEvent::from(&snap);
        assert_eq!(ev.rev, 3);
        let r = ev.running.unwrap();
        assert_eq!(r.title, "Demo");
        assert_eq!(r.segment_started_at, "1970-01-01T00:00:01.5Z");
    }
}
