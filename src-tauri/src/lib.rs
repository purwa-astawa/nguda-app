//! nguda shell: the composition root (AD-1). The only crate that knows Tauri.
//!
//! It builds the engine with the real clock, exposes fire-and-forget commands
//! (AD-4), mirrors snapshots to the view as a typed event, and renders the tray
//! from a snapshot subscriber (AD-11). The tray menu (status line, Switch, Extend
//! +5 min, Extend +15 min, Extend…, and Quit) and its handlers live in `tray`;
//! engine effects are rendered by `effects` through the system notifier (AD-10),
//! whose planned-end buttons come back here as `(prompt_id, action)` and are
//! turned by `effects::resolve` into the same targeted commands as the tray's
//! (AD-15). On macOS the app runs as an accessory (no Dock icon) and the tray
//! title gets monospaced digits once, from `macos`.

mod effects;
mod ipc;
#[cfg(target_os = "macos")]
mod macos;
mod tray;

use std::sync::Arc;

use nguda_adapters::{ActionCallback, SqliteStore, SystemClock, SystemNotifier, UuidV7Ids};
use nguda_core::{Clock, Notifier, Store, TimeZone};
use nguda_engine::{EngineDeps, EngineHandle};
use tauri::{AppHandle, Manager};

/// The database file, in the app data directory.
const DB_FILE: &str = "nguda.sqlite3";

pub use ipc::specta_builder;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = specta_builder();

    #[cfg(debug_assertions)]
    if let Err(e) = ipc::export_bindings(&builder, ipc::BINDINGS_PATH) {
        eprintln!("nguda: failed to export TypeScript bindings: {e}");
    }

    let app = tauri::Builder::default()
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
            let deps = EngineDeps {
                clock: clock.clone(),
                store,
                ids: Arc::new(UuidV7Ids),
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
            ));
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
