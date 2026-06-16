//! Thin #[tauri::command] shims + the tauri-specta builder.
use std::sync::Arc;
use tauri::ipc::Channel;
use tauri_specta::{collect_commands, Builder};

use crate::bridge::error::{BridgeError, SessionMetaDto};
use crate::bridge::output::{BridgeOutput, ChannelHandle, ChannelSink};
use crate::bridge::Bridge;

#[tauri::command]
#[specta::specta]
async fn session_list(
    bridge: tauri::State<'_, Bridge>,
) -> Result<Vec<SessionMetaDto>, BridgeError> {
    bridge.list().await
}

#[tauri::command]
#[specta::specta]
async fn session_spawn(
    bridge: tauri::State<'_, Bridge>,
    project_id: String,
    session_id: String,
    agent: Option<String>,
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .spawn(
            project_id,
            session_id,
            agent,
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
    on_output: Channel<BridgeOutput>,
) -> Result<ChannelHandle, BridgeError> {
    bridge
        .respawn(project_id, session_id, Arc::new(ChannelSink(on_output)))
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

/// Shared by `run()` and the bindings export test, so the command list lives once.
pub fn builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        session_list,
        session_spawn,
        session_attach,
        session_respawn,
        session_write,
        session_resize,
        session_close
    ])
}
