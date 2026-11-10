//! The typed IPC contract (AD-5). `src/bindings.ts` is generated from this module.
//!
//! specta-typescript refuses 64-bit integers, so engine `u64` counters cross IPC as
//! saturating `u32` (a TS `number`) and instants as RFC 3339 strings. `f64` was not
//! used because specta types it `number | null` (NaN serializes as null).

use nguda_engine::{EngineHandle, EngineMsg, Snapshot};
use serde::Serialize;
use specta_typescript::Typescript;
use tauri::{State, Wry};
use tauri_specta::{Builder, Event, collect_commands, collect_events};

/// Where debug builds write the bindings, relative to this crate.
pub const BINDINGS_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../src/bindings.ts");

/// Start (or restart) the demo task. Fire-and-forget: it never waits for the engine.
#[tauri::command]
#[specta::specta]
pub fn start_demo(engine: State<'_, EngineHandle>) -> Result<(), String> {
    engine.send(EngineMsg::StartDemo).map_err(|e| e.to_string())
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
        .commands(collect_commands![start_demo])
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

    #[test]
    fn snapshot_event_carries_title_and_rev() {
        let snap = Snapshot {
            rev: 3,
            running: Some(nguda_core::Running {
                title: "Demo".into(),
                segment_started_at: nguda_core::Timestamp::from_millisecond(1_500).unwrap(),
                accumulated_secs: 0,
            }),
        };
        let ev = SnapshotEvent::from(&snap);
        assert_eq!(ev.rev, 3);
        let r = ev.running.unwrap();
        assert_eq!(r.title, "Demo");
        assert_eq!(r.segment_started_at, "1970-01-01T00:00:01.5Z");
    }
}
