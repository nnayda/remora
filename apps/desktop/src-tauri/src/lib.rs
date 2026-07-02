pub mod bridge;
pub mod config_watch;
mod external_terminal;

use std::sync::Arc;
use std::time::Duration;

use bridge::commands::ConfigChanged;
use bridge::resolve::ConfigResolver;
use bridge::Bridge;
use remora_core::config::config_file_path;
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
            app.manage(Bridge::new(Arc::new(ConfigResolver), config_path.clone()));

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
