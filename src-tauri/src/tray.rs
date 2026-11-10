//! The tray surface (AD-11). Setters run only in the snapshot subscriber (AD-4).
//!
//! One text source, `title_for`, drives the title, the first (disabled) menu line,
//! and the tooltip. On Linux the title becomes the AppIndicator label, the tooltip
//! is a no-op in Tauri's backend, and the first menu line is the fallback. Tray
//! clicks don't fire on Linux, so every action is a menu item. The menu is built
//! once and only edited (`set_text`), because on Linux it can't be replaced.

use std::sync::Arc;

use nguda_core::{Clock, Timestamp, elapsed_secs, format_elapsed, until_next_second};
use nguda_engine::Snapshot;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{App, AppHandle, Wry};
use tauri_specta::Event;
use tokio::sync::watch;

use crate::ipc::SnapshotEvent;

const TRAY_ID: &str = "main";
const IDLE_TITLE: &str = "nguda";
const STATUS_ID: &str = "status";
const QUIT_ID: &str = "quit";

/// A black template image that macOS tints for the menu bar.
#[cfg(not(target_os = "linux"))]
const ICON: &[u8] = include_bytes!("../icons/tray.png");
#[cfg(not(target_os = "linux"))]
const ICON_AS_TEMPLATE: bool = true;
/// Same shape in white, so it stays visible on dark Linux panels (GNOME's top bar).
#[cfg(target_os = "linux")]
const ICON: &[u8] = include_bytes!("../icons/tray-linux.png");
#[cfg(target_os = "linux")]
const ICON_AS_TEMPLATE: bool = false;

/// The tray icon plus the menu's status line, which the subscriber keeps in step
/// with the title.
pub struct Tray {
    icon: TrayIcon,
    status: MenuItem<Wry>,
}

pub fn build(app: &App) -> tauri::Result<Tray> {
    let status = MenuItem::with_id(app, STATUS_ID, IDLE_TITLE, false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&status, &separator, &quit])?;

    let icon = TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(ICON_AS_TEMPLATE)
        .tooltip(IDLE_TITLE)
        .menu(&menu)
        .on_menu_event(|app, event| {
            if event.id() == QUIT_ID {
                app.exit(0);
            }
        })
        .build(app)?;
    icon.set_title(Some(IDLE_TITLE))?;
    Ok(Tray { icon, status })
}

/// The three places that show the tray text. A seam so the render step is testable.
trait Surface {
    fn set_title(&self, text: &str) -> tauri::Result<()>;
    fn set_status_line(&self, text: &str) -> tauri::Result<()>;
    fn set_tooltip(&self, text: &str) -> tauri::Result<()>;
}

impl Surface for Tray {
    fn set_title(&self, text: &str) -> tauri::Result<()> {
        self.icon.set_title(Some(text))
    }
    fn set_status_line(&self, text: &str) -> tauri::Result<()> {
        self.status.set_text(menu_text(text))
    }
    fn set_tooltip(&self, text: &str) -> tauri::Result<()> {
        self.icon.set_tooltip(Some(text))
    }
}

/// Menu labels treat `&` as a mnemonic marker (muda strips it on macOS and turns
/// it into `_` on GTK), so a literal `&` is written as `&&`.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// Writes one text to the title, the status line, and the tooltip. Each failure is
/// logged and does not stop the others.
fn render(surface: &impl Surface, text: &str) {
    if let Err(e) = surface.set_title(text) {
        eprintln!("nguda: failed to set tray title: {e}");
    }
    if let Err(e) = surface.set_status_line(text) {
        eprintln!("nguda: failed to set tray menu status line: {e}");
    }
    // A documented no-op on Linux; the status line is the fallback there.
    if let Err(e) = surface.set_tooltip(text) {
        eprintln!("nguda: failed to set tray tooltip: {e}");
    }
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
    tray: Tray,
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
        render(&tray, &title_for(&snapshot, clock.now()));
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
    use std::cell::RefCell;

    use nguda_core::Running;

    use super::*;

    /// Records every write; `set_status_line` fails when `fail_status` is set.
    #[derive(Default)]
    struct FakeSurface {
        fail_status: bool,
        calls: RefCell<Vec<(&'static str, String)>>,
    }

    impl Surface for FakeSurface {
        fn set_title(&self, text: &str) -> tauri::Result<()> {
            self.calls.borrow_mut().push(("title", text.to_owned()));
            Ok(())
        }
        fn set_status_line(&self, text: &str) -> tauri::Result<()> {
            if self.fail_status {
                return Err(tauri::Error::FailedToReceiveMessage);
            }
            self.calls.borrow_mut().push(("status", text.to_owned()));
            Ok(())
        }
        fn set_tooltip(&self, text: &str) -> tauri::Result<()> {
            self.calls.borrow_mut().push(("tooltip", text.to_owned()));
            Ok(())
        }
    }

    fn rendered(surface: &FakeSurface) -> Vec<(&'static str, String)> {
        surface.calls.borrow().clone()
    }

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

    #[test]
    fn idle_renders_app_name_on_title_status_line_and_tooltip() {
        let surface = FakeSurface::default();
        render(
            &surface,
            &title_for(&Snapshot::default(), Timestamp::UNIX_EPOCH),
        );
        assert_eq!(
            rendered(&surface),
            vec![
                ("title", "nguda".to_owned()),
                ("status", "nguda".to_owned()),
                ("tooltip", "nguda".to_owned()),
            ]
        );
    }

    #[test]
    fn running_renders_task_and_elapsed_on_all_three_each_tick() {
        let s = snapshot_started_at(1_000);
        let surface = FakeSurface::default();
        for now in [1_005, 1_006] {
            render(
                &surface,
                &title_for(&s, Timestamp::from_second(now).unwrap()),
            );
        }
        let expect = |t: &str| {
            vec![
                ("title", t.to_owned()),
                ("status", t.to_owned()),
                ("tooltip", t.to_owned()),
            ]
        };
        assert_eq!(
            rendered(&surface),
            [expect("Demo · 00:05"), expect("Demo · 00:06")].concat()
        );
    }

    #[test]
    fn status_line_failure_does_not_stop_title_or_tooltip() {
        let surface = FakeSurface {
            fail_status: true,
            ..FakeSurface::default()
        };
        render(&surface, "Demo · 00:05");
        assert_eq!(
            rendered(&surface),
            vec![
                ("title", "Demo · 00:05".to_owned()),
                ("tooltip", "Demo · 00:05".to_owned()),
            ]
        );
    }

    #[test]
    fn menu_text_escapes_ampersands() {
        assert_eq!(menu_text("R&D · 00:05"), "R&&D · 00:05");
        assert_eq!(menu_text("Demo · 00:05"), "Demo · 00:05");
    }
}
