pub mod bridge;

use std::sync::Arc;

use bridge::resolve::ConfigResolver;
use bridge::Bridge;
use remora_core::config::config_file_path;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = bridge::commands::builder();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            // Resolve the per-device config path once: Tauri owns the platform
            // config dir; remora-core owns the `remora/config.toml` suffix
            // (so the future relay resolves the same human-editable file).
            let base = app.path().config_dir()?;
            let config_path = config_file_path(base);
            app.manage(Bridge::new(Arc::new(ConfigResolver), config_path));
            Ok(())
        })
        .invoke_handler(builder.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
