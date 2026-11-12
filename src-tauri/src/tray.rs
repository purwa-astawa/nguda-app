//! The tray surface (AD-11). Setters run only in the snapshot subscriber (AD-4).
//!
//! One text source, `title_for`, drives the title, the first (disabled) menu line,
//! and the tooltip: `<task> · <elapsed>` while a task runs, `Next: <title>` while
//! idle with something left, otherwise the app name. On Linux the title becomes
//! the AppIndicator label, the tooltip is a no-op in Tauri's backend, and the
//! first menu line is the fallback. Tray clicks don't fire on Linux, so every
//! action is a menu item. The menu is built once and only edited (`set_text`,
//! `set_enabled`), because on Linux it can't be replaced.
//!
//! The actions (Switch, Extend +5 min, Extend +15 min, Extend…) are enabled only
//! while a task runs. The subscriber records the `Switch` and `ExtendTask`
//! messages for the snapshot it rendered (item and rev, AD-15) in [`MenuTargets`];
//! the menu handlers send exactly those, so a click acts on what the user saw, and
//! a stale one is rejected by the engine. Extend… sends nothing: it brings the plan
//! window forward and emits [`FocusExtendEvent`] so the view focuses the running
//! row's extend-minutes field. The notification actions (in `effects`) reuse the
//! same message shapes and `focus_extend`, so each acts like its tray item.

use std::sync::{Arc, Mutex};

use nguda_core::{Clock, ItemId, Timestamp, elapsed_secs, format_elapsed, until_next_second};
use nguda_engine::{EngineHandle, EngineMsg, Snapshot};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{App, AppHandle, Manager, Wry};
use tauri_specta::Event;
use tokio::sync::watch;

use crate::ipc::{FocusExtendEvent, SnapshotEvent};

const TRAY_ID: &str = "main";
const IDLE_TITLE: &str = "nguda";
const STATUS_ID: &str = "status";
const SWITCH_ID: &str = "switch";
const EXTEND_5_ID: &str = "extend_5";
const EXTEND_15_ID: &str = "extend_15";
const EXTEND_CUSTOM_ID: &str = "extend_custom";
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

/// The tray icon plus the menu items the subscriber keeps in step with the
/// snapshot: the status line and the actions.
pub struct Tray {
    icon: TrayIcon,
    status: MenuItem<Wry>,
    actions: [MenuItem<Wry>; 4],
}

/// The engine messages the menu actions send, for the last rendered snapshot.
/// Each is `None` while nothing runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Targets {
    switch: Option<EngineMsg>,
    extend5: Option<EngineMsg>,
    extend15: Option<EngineMsg>,
}

/// Which recorded message a menu item sends.
#[derive(Clone, Copy, Debug)]
enum Action {
    Switch,
    Extend5,
    Extend15,
}

impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Switch => "Switch",
            Self::Extend5 | Self::Extend15 => "ExtendTask",
        }
    }
}

/// [`Targets`] as managed state; the lock is held only to copy in or out.
#[derive(Default)]
pub struct MenuTargets(Mutex<Targets>);

impl MenuTargets {
    fn set(&self, targets: Targets) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = targets;
    }

    fn get(&self, action: Action) -> Option<EngineMsg> {
        let t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match action {
            Action::Switch => t.switch.clone(),
            Action::Extend5 => t.extend5.clone(),
            Action::Extend15 => t.extend15.clone(),
        }
    }
}

/// Builds the tray and manages its [`MenuTargets`].
pub fn build(app: &App) -> tauri::Result<Tray> {
    app.manage(MenuTargets::default());
    let item = |id: &str, label: &str| MenuItem::with_id(app, id, label, false, None::<&str>);
    let status = item(STATUS_ID, IDLE_TITLE)?;
    let actions = [
        item(SWITCH_ID, "Switch")?,
        item(EXTEND_5_ID, "Extend +5 min")?,
        item(EXTEND_15_ID, "Extend +15 min")?,
        item(EXTEND_CUSTOM_ID, "Extend…")?,
    ];
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "Quit", true, None::<&str>)?;
    let [switch, extend5, extend15, extend_custom] = &actions;
    let menu = Menu::with_items(
        app,
        &[
            &status,
            switch,
            extend5,
            extend15,
            extend_custom,
            &separator,
            &quit,
        ],
    )?;

    let icon = TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON)?)
        .icon_as_template(ICON_AS_TEMPLATE)
        .tooltip(IDLE_TITLE)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            QUIT_ID => app.exit(0),
            SWITCH_ID => send_target(app, Action::Switch),
            EXTEND_5_ID => send_target(app, Action::Extend5),
            EXTEND_15_ID => send_target(app, Action::Extend15),
            EXTEND_CUSTOM_ID => focus_extend(app),
            _ => {}
        })
        .build(app)?;
    icon.set_title(Some(IDLE_TITLE))?;
    Ok(Tray {
        icon,
        status,
        actions,
    })
}

/// Fire-and-forget (AD-4): sends the recorded message for `action`, if any.
fn send_target(app: &AppHandle, action: Action) {
    let Some(msg) = app.state::<MenuTargets>().get(action) else {
        return;
    };
    if let Err(e) = app.state::<EngineHandle>().send(msg) {
        eprintln!("nguda: failed to send {}: {e}", action.name());
    }
}

/// Extend…: brings the plan window forward and asks the view to focus the running
/// row's extend-minutes field. Sends nothing to the engine. Shared with the
/// notification's Extend… action.
pub(crate) fn focus_extend(app: &AppHandle) {
    show_main_window(app);
    if let Err(e) = FocusExtendEvent.emit(app) {
        eprintln!("nguda: failed to emit focus-extend: {e}");
    }
}

/// Restores, shows and focuses the plan window.
pub(crate) fn show_main_window(app: &AppHandle) {
    match app.get_webview_window("main") {
        Some(window) => {
            // macOS ignores focus on a minimized window, so restore it first.
            if let Err(e) = window.unminimize() {
                eprintln!("nguda: failed to unminimize the main window: {e}");
            }
            if let Err(e) = window.show() {
                eprintln!("nguda: failed to show the main window: {e}");
            }
            if let Err(e) = window.set_focus() {
                eprintln!("nguda: failed to focus the main window: {e}");
            }
        }
        None => eprintln!("nguda: no main window to focus for Extend"),
    }
}

/// Extend +5 min, in seconds. Shared with the notification actions.
pub(crate) const EXTEND_5_SECS: u64 = 5 * 60;
/// Extend +15 min, in seconds. Shared with the notification actions.
pub(crate) const EXTEND_15_SECS: u64 = 15 * 60;

/// The `Switch` for `item_id` as rendered at `rev` (AD-15). Shared with the
/// notification actions, which pass the target recorded when they posted.
pub(crate) fn switch_target(item_id: ItemId, rev: u64) -> EngineMsg {
    EngineMsg::Switch {
        item_id,
        expected_rev: rev,
    }
}

/// The `ExtendTask` by `secs` for `item_id` as rendered at `rev` (AD-15). Shared
/// with the notification actions.
pub(crate) fn extend_target(item_id: ItemId, secs: u64, rev: u64) -> EngineMsg {
    EngineMsg::ExtendTask {
        item_id,
        secs,
        expected_rev: rev,
    }
}

/// `Switch` for the running item at the snapshot's rev, or `None` while idle.
fn switch_msg(snapshot: &Snapshot) -> Option<EngineMsg> {
    snapshot
        .running
        .as_ref()
        .map(|r| switch_target(r.item_id, snapshot.rev))
}

/// `ExtendTask` by `secs` for the running item at the snapshot's rev, or `None`
/// while idle.
fn extend_msg(snapshot: &Snapshot, secs: u64) -> Option<EngineMsg> {
    snapshot
        .running
        .as_ref()
        .map(|r| extend_target(r.item_id, secs, snapshot.rev))
}

/// Every message the menu sends for `snapshot`.
fn targets_for(snapshot: &Snapshot) -> Targets {
    Targets {
        switch: switch_msg(snapshot),
        extend5: extend_msg(snapshot, EXTEND_5_SECS),
        extend15: extend_msg(snapshot, EXTEND_15_SECS),
    }
}

/// The three places that show the tray text, and the action items. A seam so the
/// render steps are testable.
trait Surface {
    fn set_title(&self, text: &str) -> tauri::Result<()>;
    fn set_status_line(&self, text: &str) -> tauri::Result<()>;
    fn set_tooltip(&self, text: &str) -> tauri::Result<()>;
    /// Enables or disables Switch, Extend +5, Extend +15 and Extend… together.
    fn set_actions_enabled(&self, enabled: bool) -> tauri::Result<()>;
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
    fn set_actions_enabled(&self, enabled: bool) -> tauri::Result<()> {
        self.actions
            .iter()
            .try_for_each(|item| item.set_enabled(enabled))
    }
}

/// Records the snapshot's targets, then enables the actions exactly while a task
/// runs. The lock is released before the setters run (AD-4).
fn render_actions(surface: &impl Surface, targets: &MenuTargets, snapshot: &Snapshot) {
    let next = targets_for(snapshot);
    let enabled = next.switch.is_some();
    targets.set(next);
    if let Err(e) = surface.set_actions_enabled(enabled) {
        eprintln!("nguda: failed to set tray action items: {e}");
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

/// `<task> · <elapsed>` while a task runs, `Next: <title>` while idle with an item
/// left, otherwise the app name.
fn title_for(snapshot: &Snapshot, now: Timestamp) -> String {
    match (&snapshot.running, &snapshot.next_item) {
        (Some(r), _) => format!("{} · {}", r.title, format_elapsed(elapsed_secs(r, now))),
        (None, Some(n)) => format!("Next: {}", n.title),
        (None, None) => IDLE_TITLE.to_owned(),
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
    let targets = app.state::<MenuTargets>();
    let mut snapshot = rx.borrow_and_update().clone();
    render_actions(&tray, &targets, &snapshot);
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
                render_actions(&tray, &targets, &snapshot);
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

    use nguda_core::{Running, Uuid};
    use nguda_engine::NextItem;

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
        fn set_actions_enabled(&self, enabled: bool) -> tauri::Result<()> {
            self.calls
                .borrow_mut()
                .push(("actions", enabled.to_string()));
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
                item_id: ItemId(Uuid::from_u128(7)),
                title: "Demo".into(),
                segment_started_at: Timestamp::from_second(secs).unwrap(),
                accumulated_secs: 0,
            }),
            ..Default::default()
        }
    }

    fn snapshot_next(title: &str) -> Snapshot {
        Snapshot {
            rev: 2,
            next_item: Some(NextItem {
                item_id: ItemId(Uuid::from_u128(1)),
                title: title.into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn idle_title_shows_the_next_item() {
        assert_eq!(
            title_for(&snapshot_next("B"), Timestamp::UNIX_EPOCH),
            "Next: B"
        );
    }

    #[test]
    fn idle_with_nothing_left_after_startup_is_the_app_name() {
        let s = Snapshot {
            rev: 3,
            ..Default::default()
        };
        assert_eq!(title_for(&s, Timestamp::UNIX_EPOCH), "nguda");
    }

    #[test]
    fn running_wins_over_next_item() {
        let s = Snapshot {
            next_item: snapshot_next("B").next_item,
            ..snapshot_started_at(1_000)
        };
        assert_eq!(
            title_for(&s, Timestamp::from_second(1_005).unwrap()),
            "Demo · 00:05"
        );
    }

    #[test]
    fn idle_next_renders_on_title_status_line_and_tooltip() {
        let surface = FakeSurface::default();
        render(
            &surface,
            &title_for(&snapshot_next("B"), Timestamp::UNIX_EPOCH),
        );
        assert_eq!(
            rendered(&surface),
            vec![
                ("title", "Next: B".to_owned()),
                ("status", "Next: B".to_owned()),
                ("tooltip", "Next: B".to_owned()),
            ]
        );
    }

    #[test]
    fn idle_next_with_an_ampersand_escapes_only_the_menu_line() {
        let text = title_for(&snapshot_next("R&D"), Timestamp::UNIX_EPOCH);
        assert_eq!(text, "Next: R&D");
        assert_eq!(menu_text(&text), "Next: R&&D");
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

    #[test]
    fn switch_targets_the_running_item_at_the_rendered_rev() {
        assert_eq!(
            switch_msg(&snapshot_started_at(1_000)),
            Some(EngineMsg::Switch {
                item_id: ItemId(Uuid::from_u128(7)),
                expected_rev: 1,
            })
        );
        assert_eq!(switch_msg(&snapshot_next("B")), None);
        assert_eq!(switch_msg(&Snapshot::default()), None);
    }

    #[test]
    fn extend_targets_the_running_item_at_the_rendered_rev() {
        assert_eq!(
            extend_msg(&snapshot_started_at(1_000), 300),
            Some(EngineMsg::ExtendTask {
                item_id: ItemId(Uuid::from_u128(7)),
                secs: 300,
                expected_rev: 1,
            })
        );
        assert_eq!(extend_msg(&snapshot_next("B"), 300), None);
        assert_eq!(extend_msg(&Snapshot::default(), 300), None);
    }

    #[test]
    fn targets_are_switch_and_extend_by_5_and_15_minutes() {
        let s = snapshot_started_at(1_000);
        assert_eq!(
            targets_for(&s),
            Targets {
                switch: switch_msg(&s),
                extend5: extend_msg(&s, 300),
                extend15: extend_msg(&s, 900),
            }
        );
        assert_eq!(targets_for(&snapshot_next("B")), Targets::default());
    }

    #[test]
    fn render_actions_records_the_targets_and_toggles_the_items() {
        let surface = FakeSurface::default();
        let targets = MenuTargets::default();
        let running = snapshot_started_at(1_000);
        render_actions(&surface, &targets, &running);
        assert_eq!(targets.get(Action::Switch), switch_msg(&running));
        assert_eq!(targets.get(Action::Extend5), extend_msg(&running, 300));
        assert_eq!(targets.get(Action::Extend15), extend_msg(&running, 900));
        render_actions(&surface, &targets, &snapshot_next("B"));
        for action in [Action::Switch, Action::Extend5, Action::Extend15] {
            assert_eq!(targets.get(action), None, "{action:?}");
        }
        assert_eq!(
            rendered(&surface),
            vec![
                ("actions", "true".to_owned()),
                ("actions", "false".to_owned())
            ]
        );
    }
}
