pub mod bridge;
pub mod bridge_state;
pub mod config_watch;
mod external_terminal;
mod launch;
pub mod relay;
pub mod remote_host;
mod vscode;

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
            let loopback_active = if remote_host::loopback_enabled() {
                match tauri::async_runtime::block_on(remote_host::start_loopback(&app_bridge)) {
                    Ok(host) => {
                        // Tee the output pump's status transitions into the
                        // loopback's wake path (#233) so a session going
                        // `Awaiting` push-wakes the (self) device over the
                        // in-process relay. Clone the handle before the host
                        // moves into managed state.
                        app_bridge.set_wake_handle(Arc::new(host.wake.clone()));
                        app_bridge.set_remote_host(host);
                        eprintln!(
                            "REMORA_REMOTE_LOOPBACK=1: attach routes through the loopback bridge"
                        );
                        true
                    }
                    Err(e) => {
                        eprintln!("loopback failed to start, using direct path: {e}");
                        false
                    }
                }
            } else {
                false
            };

            // Real relay bridge (ADR-0021 D7): when `[relay]` is configured, host
            // this device's bridge so paired devices can reach it — supervised
            // (#277), so a live config edit starts/restarts/stops it without an
            // app relaunch. Precedence: the dev loopback WINS — when it is active
            // this device is already its own in-process bridge, so no supervisor
            // is managed at all (they would contend for the one identity/roster)
            // and live `[relay]` edits are deliberately inert.
            let supervisor = if loopback_active {
                None
            } else {
                // The Bridge's wake slot is set once, before it moves into
                // managed state — so it gets the *swappable* tee (#233/#277):
                // the supervisor points it at the current bridge's wake handle
                // on every start and clears it on stop.
                let wake = Arc::new(relay::SwappableWaker::default());
                app_bridge.set_wake_handle(wake.clone());
                Some(relay::RelaySupervisor::new(
                    app_bridge.resolver(),
                    config_path.clone(),
                    wake,
                ))
            };

            app.manage(app_bridge);
            if let Some(supervisor) = supervisor {
                // Launch-time start goes through the same transition path as a
                // live edit: nothing running + `[relay]` present → Start (and
                // absent → nothing). Applied before managing so the pairing
                // commands never observe a half-started supervisor.
                let handle = app.handle().clone();
                tauri::async_runtime::block_on(supervisor.reconfigure(&handle));
                app.manage(supervisor);
            }

            // Live-reload the sidebar when the config file changes on disk,
            // and re-diff the `[relay]` section against the hosted bridge
            // (#277). Non-fatal: on failure the app still runs with manual
            // refresh (and launch-time relay state).
            let handle = app.handle().clone();
            if let Err(e) =
                config_watch::watch_config(&config_path, CONFIG_WATCH_DEBOUNCE, move || {
                    let _ = ConfigChanged.emit(&handle);
                    // Off the watcher thread; the supervisor serializes bursts
                    // (each queued run re-reads the config, so the last write
                    // wins). No supervisor managed (loopback mode) → no-op.
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Some(supervisor) = handle.try_state::<relay::RelaySupervisor>() {
                            supervisor.reconfigure(&handle).await;
                        }
                    });
                })
            {
                eprintln!("config watcher failed to start: {e}");
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
