//! The tray surface (AD-11). Setters run only in the snapshot subscriber (AD-4).

use std::sync::Arc;

use nguda_core::{Clock, Timestamp, elapsed_secs, format_elapsed, until_next_second};
use nguda_engine::Snapshot;
use tauri::image::Image;
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{App, AppHandle};
use tauri_specta::Event;
use tokio::sync::watch;

use crate::ipc::SnapshotEvent;

const TRAY_ID: &str = "main";
const IDLE_TITLE: &str = "nguda";

pub fn build(app: &App) -> tauri::Result<TrayIcon> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .icon_as_template(true)
        .tooltip(IDLE_TITLE)
        .build(app)?;
    tray.set_title(Some(IDLE_TITLE))?;
    Ok(tray)
}

/// `<task> · <elapsed>` while a task runs, otherwise the app name.
pub fn title_for(snapshot: &Snapshot, now: Timestamp) -> String {
    match &snapshot.running {
        Some(r) => format!("{} · {}", r.title, format_elapsed(elapsed_secs(r, now))),
        None => IDLE_TITLE.to_owned(),
    }
}

/// Re-renders the tray on every snapshot and on every elapsed second, and mirrors
/// each new snapshot to the view. Ends when the engine stops.
pub async fn subscribe(
    app: AppHandle,
    tray: TrayIcon,
    mut rx: watch::Receiver<Snapshot>,
    clock: Arc<dyn Clock>,
) {
    let mut snapshot = rx.borrow_and_update().clone();
    loop {
        let wait = snapshot
            .running
            .as_ref()
            .map(|r| until_next_second(r, clock.now()));
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                snapshot = rx.borrow_and_update().clone();
                if let Err(e) = SnapshotEvent::from(&snapshot).emit(&app) {
                    eprintln!("nguda: failed to emit snapshot: {e}");
                }
            }
            _ = sleep_or_forever(wait) => {}
        }
        let title = title_for(&snapshot, clock.now());
        if let Err(e) = tray.set_title(Some(title.as_str())) {
            eprintln!("nguda: failed to set tray title: {e}");
        }
        let _ = tray.set_tooltip(Some(title.as_str()));
    }
}

async fn sleep_or_forever(wait: Option<std::time::Duration>) {
    match wait {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use nguda_core::Running;

    use super::*;

    fn snapshot_started_at(secs: i64) -> Snapshot {
        Snapshot {
            rev: 1,
            running: Some(Running {
                title: "Demo".into(),
                segment_started_at: Timestamp::from_second(secs).unwrap(),
                accumulated_secs: 0,
            }),
        }
    }

    #[test]
    fn idle_title_is_the_app_name() {
        assert_eq!(
            title_for(&Snapshot::default(), Timestamp::UNIX_EPOCH),
            "nguda"
        );
    }

    #[test]
    fn running_title_shows_task_and_elapsed() {
        let s = snapshot_started_at(1_000);
        assert_eq!(
            title_for(&s, Timestamp::from_second(1_000).unwrap()),
            "Demo · 00:00"
        );
        assert_eq!(
            title_for(&s, Timestamp::from_second(1_005).unwrap()),
            "Demo · 00:05"
        );
        assert_eq!(
            title_for(&s, Timestamp::from_second(4_600).unwrap()),
            "Demo · 1:00:00"
        );
    }
}
