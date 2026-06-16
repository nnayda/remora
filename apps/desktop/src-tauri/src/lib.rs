pub mod bridge;

use std::sync::Arc;

use bridge::Bridge;
use remora_core::FakeSessionSource;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = bridge::commands::builder();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Bridge::new(Arc::new(FakeSessionSource::new())))
        .invoke_handler(builder.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
