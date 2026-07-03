//! Thin #[tauri::command] shims + the tauri-specta builder.
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri_specta::{collect_commands, collect_events, Builder, Event};

use crate::bridge::dto::ConfigDto;
use crate::bridge::editor_dto::{
    AgentInputDto, EditableConfigDto, HostInputDto, ProjectInputDto, WorkspaceModeDto,
};
use crate::bridge::error::{BridgeError, SessionListDto};
use crate::bridge::output::{BridgeOutput, ChannelHandle, ChannelSink};
use crate::bridge::Bridge;

/// Emitted when the per-device config file changes on disk (file watcher).
/// Carries no payload — the frontend re-reads via `config_get` (its existing
/// refresh path), so the event is a ping, not a config snapshot.
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, Event)]
pub struct ConfigChanged;

#[tauri::command]
#[specta::specta]
async fn session_list(bridge: tauri::State<'_, Bridge>) -> Result<SessionListDto, BridgeError> {
    bridge.list().await
}

#[tauri::command]
#[specta::specta]
async fn config_get(bridge: tauri::State<'_, Bridge>) -> Result<ConfigDto, BridgeError> {
    bridge.config()
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn session_spawn(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    agent: Option<String>,
    base: Option<String>,
    workspace: Option<WorkspaceModeDto>,
    branch: Option<String>,
    worktree_root: Option<String>,
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .spawn(
            project_id,
            session_id,
            agent,
            base,
            workspace.map(Into::into),
            branch,
            worktree_root,
            Arc::new(ChannelSink(on_output)),
        )
        .await
}

#[tauri::command]
#[specta::specta]
async fn session_attach(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .attach(project_id, session_id, Arc::new(ChannelSink(on_output)))
        .await
}

#[tauri::command]
#[specta::specta]
async fn session_respawn(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    agent: Option<String>,
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .respawn(
            project_id,
            session_id,
            agent,
            Arc::new(ChannelSink(on_output)),
        )
        .await
}

#[tauri::command]
#[specta::specta]
async fn session_write(
    bridge: tauri::State<'_, Bridge>,
    handle: ChannelHandle,
    bytes: Vec<u8>,
) -> Result<(), BridgeError> {
    bridge.write(handle, bytes).await
}

#[tauri::command]
#[specta::specta]
async fn session_resize(
    bridge: tauri::State<'_, Bridge>,
    handle: ChannelHandle,
    rows: u16,
    cols: u16,
) -> Result<(), BridgeError> {
    bridge.resize(handle, rows, cols).await
}

#[tauri::command]
#[specta::specta]
async fn session_close(
    bridge: tauri::State<'_, Bridge>,
    handle: ChannelHandle,
) -> Result<(), BridgeError> {
    bridge.close(handle);
    Ok(())
}

#[tauri::command]
#[specta::specta]
async fn session_stop(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
) -> Result<(), BridgeError> {
    bridge.stop(project_id, session_id).await
}

#[tauri::command]
#[specta::specta]
async fn session_remove(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    force: bool,
) -> Result<(), BridgeError> {
    bridge.remove(project_id, session_id, force).await
}

// ---- Editor channel (PR2): local-only, un-redacted config management ----

#[tauri::command]
#[specta::specta]
async fn config_get_editable(
    bridge: tauri::State<'_, Bridge>,
) -> Result<EditableConfigDto, BridgeError> {
    bridge.config_editable()
}

#[tauri::command]
#[specta::specta]
async fn config_insert_host(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: HostInputDto,
) -> Result<(), BridgeError> {
    bridge.config_insert_host(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_update_host(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: HostInputDto,
) -> Result<(), BridgeError> {
    bridge.config_update_host(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_remove_host(
    bridge: tauri::State<'_, Bridge>,
    id: String,
) -> Result<(), BridgeError> {
    bridge.config_remove_host(id).await
}

#[tauri::command]
#[specta::specta]
async fn config_insert_project(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: ProjectInputDto,
) -> Result<(), BridgeError> {
    bridge.config_insert_project(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_update_project(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: ProjectInputDto,
) -> Result<(), BridgeError> {
    bridge.config_update_project(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_remove_project(
    bridge: tauri::State<'_, Bridge>,
    id: String,
) -> Result<(), BridgeError> {
    bridge.config_remove_project(id).await
}

#[tauri::command]
#[specta::specta]
async fn config_insert_agent(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: AgentInputDto,
) -> Result<(), BridgeError> {
    bridge.config_insert_agent(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_update_agent(
    bridge: tauri::State<'_, Bridge>,
    id: String,
    input: AgentInputDto,
) -> Result<(), BridgeError> {
    bridge.config_update_agent(id, input).await
}

#[tauri::command]
#[specta::specta]
async fn config_remove_agent(
    bridge: tauri::State<'_, Bridge>,
    id: String,
) -> Result<(), BridgeError> {
    bridge.config_remove_agent(id).await
}

// ---- External terminal (spec 2026-07-02) ----

use crate::external_terminal::{
    assemble_launch, detect_terminals, resolve_terminal, shell_quote_command, ResolveError,
};
use crate::launch::{spawn_detached, RealProbe};
use crate::vscode;

/// A detected terminal, id + display name only.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DetectedTerminalDto {
    pub id: String,
    pub name: String,
}

impl From<ResolveError> for BridgeError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::NotConfigured(message) => BridgeError::TerminalNotConfigured { message },
            ResolveError::UnknownId(message) | ResolveError::NotDetected(message) => {
                BridgeError::TerminalNotConfigured { message }
            }
        }
    }
}

#[tauri::command]
#[specta::specta]
async fn external_terminals(
    _bridge: tauri::State<'_, Bridge>,
) -> Result<Vec<DetectedTerminalDto>, BridgeError> {
    // Fresh probe per call (stat-cheap): no cache, no staleness when a
    // terminal is installed/uninstalled mid-session.
    Ok(detect_terminals(&RealProbe)
        .into_iter()
        .map(|t| DetectedTerminalDto {
            id: t.id.to_string(),
            name: t.name.to_string(),
        })
        .collect())
}

#[tauri::command]
#[specta::specta]
async fn open_external_terminal(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    terminal_id: Option<String>,
) -> Result<(), BridgeError> {
    // Resolve the terminal preference first: it's a cheap in-memory lookup,
    // and if it fails (unconfigured/unknown/uninstalled) we should surface
    // that -> Settings deep-link before paying for `external_attach_argv`,
    // which for kubectl sessions can shell out to resolve `{ command }`
    // fields.
    let pref = bridge.terminal_preference()?;
    let detected = detect_terminals(&RealProbe);
    let plan = resolve_terminal(terminal_id.as_deref(), pref.as_ref(), &detected)?;
    let attach = bridge.external_attach_argv(project_id, session_id).await?;
    let argv = assemble_launch(&plan, &attach, &RealProbe).map_err(|e| match e {
        // A missing transport binary is a launch-environment error, not a
        // Settings-fixable preference: surface the message (notice), don't
        // deep-link (spec flow: binary resolution -> error, not Settings).
        ResolveError::NotDetected(message) => BridgeError::Transport { message },
        other => other.into(),
    })?;
    let terminal_name = argv.first().cloned().unwrap_or_default();
    let mut child = spawn_detached(&argv).map_err(|e| BridgeError::Transport {
        message: format!("could not launch `{terminal_name}`: {e}"),
    })?;
    // Early-exit check (review D9): a terminal that dies within ~1s (bad
    // flag, broken install) becomes a real error instead of a flash-closed
    // window. A CLEAN exit is tolerated — forking launchers (`wezterm
    // start`) may return 0 immediately by design. >1s failures show in the
    // terminal itself; Copy attach command is the diagnostic.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    if let Ok(Some(status)) = child.try_wait() {
        if !status.success() {
            return Err(BridgeError::Transport {
                message: format!("`{terminal_name}` exited immediately ({status})"),
            });
        }
    }
    // Reap the child when it eventually exits (an unreaped exited child is a
    // zombie until Remora dies); the blocking slot parks until the terminal
    // exits, one per launch. The early-exit error path above already reaped
    // via try_wait.
    tauri::async_runtime::spawn_blocking(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
async fn copy_attach_command(
    app: tauri::AppHandle,
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
) -> Result<(), BridgeError> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    // Written to the clipboard RUST-SIDE: the string carries the hostname,
    // and connection details never cross to the frontend (the ConfigDto
    // redaction boundary, dto.rs). The frontend only triggers the copy.
    let attach = bridge.external_attach_argv(project_id, session_id).await?;
    let command = shell_quote_command(&attach);
    app.clipboard()
        .write_text(command)
        .map_err(|e| BridgeError::Transport {
            message: format!("could not write clipboard: {e}"),
        })
}

#[tauri::command]
#[specta::specta]
async fn config_set_terminal(
    bridge: tauri::State<'_, Bridge>,
    terminal_id: Option<String>,
) -> Result<(), BridgeError> {
    bridge.config_set_terminal(terminal_id).await
}

// ---- Pairing + roster (ADR-0021, #232) ----

use crate::bridge::pairing::{
    DeviceInfoDto, PairingCodeDto, PairingDeviceArrived, PairingResult, PairingWindowOpened,
    RosterChanged,
};

/// Open (or replace) this device's pairing window; returns the QR code + TTL.
#[tauri::command]
#[specta::specta]
async fn pairing_open_window(
    app: tauri::AppHandle,
    ttl_secs: Option<u64>,
) -> Result<PairingCodeDto, BridgeError> {
    crate::bridge::pairing::open_window(&app, ttl_secs).await
}

/// Confirm the arrived device's fingerprint (enrol it).
#[tauri::command]
#[specta::specta]
async fn pairing_confirm(app: tauri::AppHandle, device_id: String) -> Result<(), BridgeError> {
    crate::bridge::pairing::confirm(&app, device_id).await
}

/// Reject the arrived device (grant nothing durable).
#[tauri::command]
#[specta::specta]
async fn pairing_reject(app: tauri::AppHandle, device_id: String) -> Result<(), BridgeError> {
    crate::bridge::pairing::reject(&app, device_id).await
}

/// Close the current pairing window without pairing anyone.
#[tauri::command]
#[specta::specta]
async fn pairing_cancel(app: tauri::AppHandle) -> Result<(), BridgeError> {
    crate::bridge::pairing::cancel(&app).await
}

/// List this bridge's paired devices (live roster).
#[tauri::command]
#[specta::specta]
async fn list_devices(app: tauri::AppHandle) -> Result<Vec<DeviceInfoDto>, BridgeError> {
    crate::bridge::pairing::list(&app).await
}

/// Un-pair a device (drop from roster, kick any live session).
#[tauri::command]
#[specta::specta]
async fn revoke_device(app: tauri::AppHandle, device_id: String) -> Result<(), BridgeError> {
    crate::bridge::pairing::revoke(&app, device_id).await
}

/// This bridge's own identity fingerprint (ADR-0021 D5), for the pairing UI.
#[tauri::command]
#[specta::specta]
async fn bridge_fingerprint(app: tauri::AppHandle) -> Result<String, BridgeError> {
    crate::bridge::pairing::own_fingerprint(&app)
}

#[tauri::command]
#[specta::specta]
async fn open_in_vscode(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
) -> Result<(), BridgeError> {
    // Core resolves the authoritative path + ssh authority; the shell owns the
    // `code` binary and the launch (mirrors open_external_terminal).
    let target = bridge.remote_workspace(project_id, session_id).await?;
    let argv = vscode::launch_argv(&target, &RealProbe)
        .map_err(|message| BridgeError::Transport { message })?;
    let mut child = spawn_detached(&argv).map_err(|e| BridgeError::Transport {
        message: format!("could not launch VS Code: {e}"),
    })?;
    // Early-exit check (same rationale as open_external_terminal): a `code`
    // that dies within ~1s (bad install) becomes a real error, not a silent
    // no-op. A missing Remote-SSH extension fails later, inside VS Code.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    if let Ok(Some(status)) = child.try_wait() {
        if !status.success() {
            return Err(BridgeError::Transport {
                message: format!("VS Code exited immediately ({status})"),
            });
        }
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Shared by `run()` and the bindings export test, so the command list lives once.
pub fn builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            session_list,
            config_get,
            session_spawn,
            session_attach,
            session_respawn,
            session_stop,
            session_remove,
            session_write,
            session_resize,
            session_close,
            config_get_editable,
            config_insert_host,
            config_update_host,
            config_remove_host,
            config_insert_project,
            config_update_project,
            config_remove_project,
            config_insert_agent,
            config_update_agent,
            config_remove_agent,
            external_terminals,
            open_external_terminal,
            copy_attach_command,
            config_set_terminal,
            pairing_open_window,
            pairing_confirm,
            pairing_reject,
            pairing_cancel,
            list_devices,
            revoke_device,
            bridge_fingerprint,
            open_in_vscode
        ])
        .events(collect_events![
            ConfigChanged,
            PairingWindowOpened,
            PairingDeviceArrived,
            PairingResult,
            RosterChanged
        ])
}

#[cfg(test)]
mod bindings_test {
    use super::builder;

    /// Generates the committed TS bindings and fails if they drift (no-drift guard).
    /// Post-processes the tauri-specta rc.21 output: drop the stray
    /// `export type TAURI_CHANNEL<TSend> = null` (collides with the Channel import
    /// -> TS2440) and prepend `// @ts-nocheck` (silences TS6133 on generated decls).
    #[test]
    fn bindings_are_up_to_date() {
        let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/bindings.ts");
        // Unique per process so concurrent `cargo test` runs can't collide on
        // the temp path (matches the `remora-config-test-{pid}` convention).
        let tmp =
            std::env::temp_dir().join(format!("remora-bindings-gen-{}.ts", std::process::id()));
        builder()
            .export(
                // u64 (ChannelHandle, SessionMetaDto.created_at) -> TS `number`.
                // number is exact only to 2^53; a monotonic handle counter won't
                // approach that in a process lifetime, so this is safe here.
                specta_typescript::Typescript::default()
                    .bigint(specta_typescript::BigIntExportBehavior::Number),
                &tmp,
            )
            .expect("specta export");
        let raw = std::fs::read_to_string(&tmp).expect("read generated bindings");
        std::fs::remove_file(&tmp).ok();
        let generated: String = std::iter::once("// @ts-nocheck".to_string())
            .chain(
                raw.lines()
                    .filter(|l| !l.trim_start().starts_with("export type TAURI_CHANNEL"))
                    .map(str::to_string),
            )
            .collect::<Vec<_>>()
            .join("\n");
        let current = std::fs::read_to_string(&committed).unwrap_or_default();
        let stale = current.trim() != generated.trim();
        // Default (incl. CI): compare only — NEVER mutate the source tree, or a
        // stale checkout would surface as phantom diffs / cache churn. To
        // regenerate locally after changing a command, opt in explicitly:
        //   REMORA_UPDATE_BINDINGS=1 cargo test -p remora-desktop bindings_are_up_to_date
        if stale && std::env::var_os("REMORA_UPDATE_BINDINGS").is_some() {
            std::fs::write(&committed, format!("{generated}\n")).expect("write bindings");
        } else {
            assert!(
                !stale,
                "src/bindings.ts is stale. Regenerate with \
                 `REMORA_UPDATE_BINDINGS=1 cargo test -p remora-desktop bindings_are_up_to_date`, \
                 then commit src/bindings.ts."
            );
        }
    }
}
