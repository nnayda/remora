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
async fn session_spawn(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    agent: Option<String>,
    base: Option<String>,
    workspace: Option<WorkspaceModeDto>,
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .spawn(
            project_id,
            session_id,
            agent,
            base,
            workspace.map(Into::into),
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
            config_remove_agent
        ])
        .events(collect_events![ConfigChanged])
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
