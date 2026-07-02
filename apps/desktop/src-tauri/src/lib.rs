pub mod bridge;
pub mod config_watch;
pub mod remote_host;

use std::sync::Arc;
use std::time::Duration;

use bridge::commands::ConfigChanged;
use bridge::resolve::ConfigResolver;
use bridge::Bridge;
use remora_core::config::config_file_path;
use remora_core::SessionLocks;
use tauri::Manager;
use tauri_specta::Event;

/// Debounce window for config-file writes. Coalesces multi-write editor saves
/// into a single sidebar refresh.
const CONFIG_WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = bridge::commands::builder();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            // Register the tauri-specta event machinery before anything emits.
            builder.mount_events(app);

            // Resolve the per-device config path once: Tauri owns the platform
            // config dir; remora-core owns the `remora/config.toml` suffix.
            let base = app.path().config_dir()?;
            let config_path = config_file_path(base);
            // One session-lock registry per process, shared between the
            // resolver's `ExclusiveSource` wrappers and the Bridge's handle
            // (ADR-0021).
            let session_locks = SessionLocks::new();
            let mut app_bridge = Bridge::new(
                Arc::new(ConfigResolver::new(Arc::clone(&session_locks))),
                config_path.clone(),
                session_locks,
            );

            // Dev-only relay loopback (ADR-0021 spec D11): when
            // `REMORA_REMOTE_LOOPBACK=1`, the desktop attaches through its own
            // in-process bridge + relay. Non-fatal: a startup failure logs and
            // falls back to the direct path rather than bricking the app.
            if remote_host::loopback_enabled() {
                match tauri::async_runtime::block_on(remote_host::start_loopback(&app_bridge)) {
                    Ok(host) => {
                        app_bridge.set_remote_host(host);
                        eprintln!(
                            "REMORA_REMOTE_LOOPBACK=1: attach routes through the loopback bridge"
                        );
                    }
                    Err(e) => {
                        eprintln!("loopback failed to start, using direct path: {e}");
                    }
                }
            }
            app.manage(app_bridge);

            // Live-reload the sidebar when the config file changes on disk.
            // Non-fatal: on failure the app still runs with manual refresh.
            let handle = app.handle().clone();
            if let Err(e) =
                config_watch::watch_config(&config_path, CONFIG_WATCH_DEBOUNCE, move || {
                    let _ = ConfigChanged.emit(&handle);
                })
            {
                eprintln!("config watcher failed to start: {e}");
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
