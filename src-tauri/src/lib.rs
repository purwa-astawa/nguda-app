//! nguda shell: the composition root (AD-1). The only crate that knows Tauri.
//!
//! It builds the engine with the real clock, exposes fire-and-forget commands
//! (AD-4) plus the async read-only queries (`query_day`, `query_week`, AD-16), mirrors
//! snapshots to the view as a typed event, and renders the tray from a snapshot
//! subscriber (AD-11). The tray menu (status line, Switch, Extend +5 min, Extend
//! +15 min, Extend…, Today's summary, and Quit) and its handlers live in `tray`;
//! engine effects are rendered by `effects` through the system notifier (AD-10),
//! whose planned-end buttons come back here as `(prompt_id, action)` and are
//! turned by `effects::resolve` into the same targeted commands as the tray's
//! (AD-15). CSV exports (AD-16) ask for a path through `tauri-plugin-dialog`
//! from the `export_csv` command, the engine writes through the adapters'
//! `FsFileSink`, and its `ExportDone` effect reaches the view as an
//! `ExportDoneEvent`. Google Calendar import (CAP-2) runs through the adapters'
//! `GcalSource`, which keeps its refresh token in the OS keyring
//! (`KeyringSecretStore`) and opens the system browser for sign-in through
//! `tauri-plugin-opener`, called from Rust, so the webview needs no new
//! capability. On macOS the app runs as an accessory (no Dock icon) and the tray
//! title gets monospaced digits once, from `macos`.
//!
//! A second launch is caught by `tauri-plugin-single-instance` (registered first
//! among plugins): it shows and focuses the running instance's window and the
//! new process exits. Launch at login is the OS login item, managed through
//! `tauri-plugin-autostart` from Rust and never enabled on its own (opt-in);
//! the shell reads it at startup and after each `set_launch_at_login` toggle and
//! reports what it read to the engine as `AutostartObserved` (AD-14), which
//! only mirrors it into the snapshot.

mod effects;
mod ipc;
#[cfg(target_os = "macos")]
mod macos;
mod tray;

use std::sync::Arc;

use nguda_adapters::{
    ActionCallback, FsFileSink, GcalConfig, GcalSource, KeyringSecretStore, OpenUrl, SqliteStore,
    StdRandom, SystemClock, SystemNotifier, UuidV7Ids,
};
#[cfg(test)]
use nguda_core::{
    CalendarChange, CalendarFuture, CalendarInfo, FetchedEvent, OAuthClient, SessionStore,
    SyncBatch, Timestamp,
};
use nguda_core::{CalendarSource, Clock, Notifier, SecretStore, Store, TimeZone};
use nguda_engine::EngineMsg;
use nguda_engine::{EngineDeps, EngineHandle};
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as _};
use tauri_plugin_opener::OpenerExt;
use tauri_specta::Event;

/// The database file, in the app data directory.
const DB_FILE: &str = "nguda.sqlite3";

/// The keyring service the app's secrets are stored under (AD-13).
const KEYRING_SERVICE: &str = "nguda";

pub use ipc::specta_builder;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = specta_builder();

    #[cfg(debug_assertions)]
    if let Err(e) = ipc::export_bindings(&builder, ipc::BINDINGS_PATH) {
        eprintln!("nguda: failed to export TypeScript bindings: {e}");
    }

    let app = tauri::Builder::default()
        // First among plugins, as its docs require: a second launch only brings
        // this instance's window forward, then exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_main_window(app);
        }))
        // Opt-in: never enabled here, only from `set_launch_at_login`.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            // A tray app: no Dock icon or app menu.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            // An accessory app is not activated at launch, so bring the plan window forward.
            #[cfg(target_os = "macos")]
            if let Some(window) = app.get_webview_window("main")
                && let Err(e) = window.set_focus()
            {
                eprintln!("nguda: failed to focus the main window: {e}");
            }

            builder.mount_events(app);

            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let store: Arc<dyn Store> = Arc::new(open_store(app));
            let secrets: Arc<dyn SecretStore> = Arc::new(KeyringSecretStore::new(KEYRING_SERVICE));
            let calendar: Arc<dyn CalendarSource> = Arc::new(GcalSource::new(
                GcalConfig::google(),
                secrets,
                open_url(app.handle().clone()),
            ));
            let deps = EngineDeps {
                clock: clock.clone(),
                store,
                ids: Arc::new(UuidV7Ids),
                files: Arc::new(FsFileSink),
                calendar,
                random: Arc::new(StdRandom::default()),
                tz: system_time_zone(),
            };
            // `nguda_engine::spawn` uses `tokio::spawn`, so enter Tauri's runtime first.
            let (engine, effects) = {
                let _guard = tauri::async_runtime::handle().inner().enter();
                nguda_engine::spawn(deps)
            };
            let snapshots = engine.subscribe();
            let targets = effects::PromptTargets::default();
            let notifier: Arc<dyn Notifier> = Arc::new(SystemNotifier::new(on_action(
                targets.clone(),
                engine.clone(),
                app.handle().clone(),
            )));
            tauri::async_runtime::spawn(effects::consume(
                effects,
                engine.subscribe(),
                notifier,
                targets,
                on_export_done(app.handle().clone()),
            ));
            report_autostart(app.handle(), &engine);
            app.manage::<EngineHandle>(engine);

            let tray = match tray::build(app) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("nguda: failed to create tray icon: {e}");
                    std::process::exit(1);
                }
            };
            // Once, on the main thread, with no lock held (AD-4): the button keeps its
            // font across title updates.
            #[cfg(target_os = "macos")]
            if !macos::apply_monospaced_digits() {
                eprintln!("nguda: status-bar button not found; tray title keeps the default font");
            }
            tauri::async_runtime::spawn(tray::subscribe(
                app.handle().clone(),
                tray,
                snapshots,
                clock,
            ));
            Ok(())
        })
        .build(tauri::generate_context!());

    match app {
        Ok(app) => app.run(|_, _| {}),
        Err(e) => {
            eprintln!("nguda: failed to start: {e}");
            std::process::exit(1);
        }
    }
}

/// Reads the OS login item and reports it to the engine (AD-14, fire-and-forget);
/// a failed read is logged and reports nothing, so the snapshot keeps its last
/// value (`None` at startup).
pub(crate) fn report_autostart(app: &AppHandle, engine: &EngineHandle) {
    let read = app.autolaunch().is_enabled().map_err(|e| e.to_string());
    if let Err(e) = &read {
        eprintln!("nguda: failed to read the launch-at-login item: {e}");
    }
    if let Some(msg) = autostart_report(read)
        && let Err(e) = engine.send(msg)
    {
        eprintln!("nguda: failed to report the launch-at-login item: {e}");
    }
}

/// What a read of the login item reports: the value read, or nothing when the
/// read failed.
fn autostart_report(read: Result<bool, String>) -> Option<EngineMsg> {
    read.ok().map(ipc::autostart_observed_msg)
}

/// Turns the login item on or off, then reports what it reads back, so the
/// snapshot stays truthful even after a failed or partial write. The write's
/// error text is the result.
pub(crate) fn set_autostart(
    app: &AppHandle,
    engine: &EngineHandle,
    enabled: bool,
) -> Result<(), String> {
    let manager = app.autolaunch();
    let write = if enabled {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|e| e.to_string());
    if let Err(e) = &write {
        eprintln!("nguda: failed to change the launch-at-login item: {e}");
    }
    report_autostart(app, engine);
    toggle_result(write)
}

/// The toggle command's result: the write's outcome, its error text unchanged.
fn toggle_result(write: Result<(), String>) -> Result<(), String> {
    write
}

/// Opens a URL in the system browser, for the Google sign-in (AD-1: the gcal
/// adapter only sees this callback). The error never echoes the URL.
fn open_url(app: AppHandle) -> OpenUrl {
    Arc::new(move |url: &str| {
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|_| "could not open the browser".to_owned())
    })
}

/// A calendar source that never answers, for shell tests that start a real
/// engine: no import result ever bumps the rev under a test's feet.
#[cfg(test)]
struct IdleCalendar;

#[cfg(test)]
impl CalendarSource for IdleCalendar {
    fn sign_in(&self) -> CalendarFuture<SessionStore> {
        Box::pin(std::future::pending())
    }

    fn sign_out(&self) -> CalendarFuture<()> {
        Box::pin(std::future::pending())
    }

    fn set_client(&self, _client: Option<OAuthClient>) {}

    fn has_client(&self) -> bool {
        false
    }

    fn list_calendars(&self) -> CalendarFuture<Vec<CalendarInfo>> {
        Box::pin(std::future::pending())
    }

    fn sync_calendar(
        &self,
        _calendar_id: String,
        _token: Option<String>,
        _window_start: Timestamp,
        _window_end: Timestamp,
    ) -> CalendarFuture<SyncBatch> {
        Box::pin(std::future::pending())
    }

    fn list_events(
        &self,
        _calendar_id: String,
        _window_start: Timestamp,
        _window_end: Timestamp,
    ) -> CalendarFuture<Vec<FetchedEvent>> {
        Box::pin(std::future::pending())
    }

    fn get_event(
        &self,
        _calendar_id: String,
        _event_id: String,
        _window_start: Timestamp,
        _window_end: Timestamp,
    ) -> CalendarFuture<Option<CalendarChange>> {
        Box::pin(std::future::pending())
    }
}

/// The deps of a real engine on `store` for shell tests: the system clock, UTC,
/// and a calendar source that never answers.
#[cfg(test)]
pub(crate) fn test_deps(store: SqliteStore) -> EngineDeps {
    EngineDeps {
        clock: Arc::new(SystemClock),
        store: Arc::new(store),
        ids: Arc::new(UuidV7Ids),
        files: Arc::new(FsFileSink),
        calendar: Arc::new(IdleCalendar),
        random: Arc::new(StdRandom::default()),
        tz: TimeZone::UTC,
    }
}

/// The notifier's action callback: resolves a click against the recorded prompt
/// targets and sends the message (fire-and-forget, AD-4), or shows the window and
/// focuses the extend field while the prompt's item still runs.
fn on_action(
    targets: effects::PromptTargets,
    engine: EngineHandle,
    app: AppHandle,
) -> ActionCallback {
    Arc::new(
        move |prompt_id, action| match effects::resolve(&targets, prompt_id, action) {
            Some(effects::ActionOutcome::Send(msg)) => {
                if let Err(e) = engine.send(msg) {
                    eprintln!("nguda: failed to send the action for prompt {prompt_id}: {e}");
                }
            }
            Some(effects::ActionOutcome::FocusExtend { item_id }) => {
                if effects::focuses_extend(item_id, &engine.subscribe().borrow()) {
                    tray::focus_extend(&app);
                } else {
                    tray::show_main_window(&app);
                }
            }
            None => {}
        },
    )
}

/// Emits each finished export's row count to the view as an `ExportDoneEvent`.
fn on_export_done(app: AppHandle) -> effects::ExportDoneCallback {
    Arc::new(move |rows| {
        if let Err(e) = (ipc::ExportDoneEvent { rows }).emit(&app) {
            eprintln!("nguda: failed to emit export done ({rows} rows): {e}");
        }
    })
}

/// Opens (creating and migrating) `<app_data_dir>/nguda.sqlite3`, or exits: the app
/// cannot run without its store (AD-12).
fn open_store(app: &tauri::App) -> SqliteStore {
    let dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("nguda: no app data directory: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("nguda: cannot create {}: {e}", dir.display());
        std::process::exit(1);
    }
    let path = dir.join(DB_FILE);
    match SqliteStore::open(&path) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("nguda: cannot open {}: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// The system zone, or UTC when it cannot be determined.
fn system_time_zone() -> TimeZone {
    match jiff::tz::TimeZone::try_system() {
        Ok(tz) => tz,
        Err(e) => {
            eprintln!("nguda: cannot determine the system time zone, using UTC: {e}");
            TimeZone::UTC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_login_item_is_reported_as_read() {
        assert_eq!(
            autostart_report(Ok(false)),
            Some(EngineMsg::AutostartObserved { enabled: false })
        );
        assert_eq!(
            autostart_report(Ok(true)),
            Some(EngineMsg::AutostartObserved { enabled: true })
        );
    }

    #[test]
    fn a_failed_read_reports_nothing() {
        assert_eq!(autostart_report(Err("no launch agents dir".into())), None);
    }

    #[test]
    fn the_toggle_returns_the_write_error_text() {
        assert_eq!(toggle_result(Ok(())), Ok(()));
        assert_eq!(
            toggle_result(Err("permission denied".into())),
            Err("permission denied".to_owned())
        );
    }
}
