//! nguda shell: the composition root (AD-1). The only crate that knows Tauri.
//!
//! It builds the engine with the real clock, exposes fire-and-forget commands
//! (AD-4), mirrors snapshots to the view as a typed event, and renders the tray
//! from a snapshot subscriber (AD-11). The tray menu (status line and Quit) and
//! its Quit handler live in `tray`.

mod ipc;
mod tray;

use std::sync::Arc;

use nguda_adapters::SystemClock;
use nguda_core::Clock;
use nguda_engine::EngineHandle;
use tauri::Manager;

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
            builder.mount_events(app);

            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            // `nguda_engine::spawn` uses `tokio::spawn`, so enter Tauri's runtime first.
            let engine = {
                let _guard = tauri::async_runtime::handle().inner().enter();
                nguda_engine::spawn(clock.clone())
            };
            let snapshots = engine.subscribe();
            app.manage::<EngineHandle>(engine);

            let tray = match tray::build(app) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("nguda: failed to create tray icon: {e}");
                    std::process::exit(1);
                }
            };
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
