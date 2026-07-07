//! Tauri commands + tauri-specta events for the pairing ceremony and paired-
//! device roster (ADR-0021, #232).
//!
//! These are the desktop-side surface of the relay bridge this device hosts
//! ([`crate::relay`]). Commands drive the bridge over its [`PairingCommand`]
//! channel (open a pairing window, confirm/reject an arrived device, cancel the
//! window, revoke a paired device) and read its live state (list devices, this
//! bridge's fingerprint). A spawned forwarder task turns each [`BridgeEvent`]
//! into a frontend event so the pairing panel updates without polling.
//!
//! When no relay bridge is hosted (no `[relay]` section — or it was removed by
//! a live config edit, #277 — so the managed [`RelaySupervisor`] holds no
//! [`PairingHandles`]) every command fails cleanly with
//! [`BridgeError::RelayNotConfigured`] and the UI shows a "relay not configured"
//! state rather than a pairing panel.
//!
//! **Secret handling:** the [`PairingCodeDto::code`] string embeds the pairing
//! PSK by design (ADR-0021 D1 — it is what the phone scans as a QR / pastes).
//! It crosses to the frontend for rendering, but is **never** logged here.

use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tauri_specta::Event;
use tokio::sync::oneshot;

use remora_bridge::{fingerprint, BridgeEvent, PairingCommand, PairingOutcome};
use remora_protocol::{DeviceId, PairingCode};

use crate::bridge::error::BridgeError;
use crate::relay::{PairingHandles, RelaySupervisor};

/// The default pairing-window lifetime if the caller does not specify one.
/// A pairing ceremony is a brief, attended flow; two minutes is ample.
const DEFAULT_PAIRING_TTL_SECS: u64 = 120;

/// Current Unix time in seconds (0 before the epoch, which never happens).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The error a command returns when this device hosts no relay bridge.
fn relay_not_configured() -> BridgeError {
    BridgeError::RelayNotConfigured {
        message: "relay not configured: this device is not hosting a bridge".to_string(),
    }
}

/// The error when the bridge's command channel is gone (the serve task exited).
fn bridge_gone() -> BridgeError {
    BridgeError::Relay {
        message: "the bridge is no longer running".to_string(),
    }
}

/// Looks up the running bridge's [`PairingHandles`] through the managed
/// [`RelaySupervisor`], or the "relay not configured" error when no bridge is
/// hosted — either because no supervisor exists (loopback mode) or because the
/// supervisor currently runs no bridge (`[relay]` absent or removed, #277).
/// Cloning the `Arc` out (rather than holding managed state) means a bridge
/// stopped mid-command fails on its closed channels, never on freed handles.
fn handles(app: &AppHandle) -> Result<Arc<PairingHandles>, BridgeError> {
    app.try_state::<RelaySupervisor>()
        .and_then(|supervisor| supervisor.handles())
        .ok_or_else(relay_not_configured)
}

/// Parses a hex `device_id` from the frontend into a [`DeviceId`], mapping a
/// malformed value to [`BridgeError::InvalidId`].
fn parse_device_id(device_id: &str) -> Result<DeviceId, BridgeError> {
    device_id
        .parse::<DeviceId>()
        .map_err(|e| BridgeError::InvalidId {
            message: e.to_string(),
        })
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// A freshly minted pairing code for the UI: the encoded string to render as a
/// QR (and offer as a copyable fallback), plus the window deadline for a
/// countdown. `code` embeds the PSK by design (ADR-0021 D1); never log it.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PairingCodeDto {
    /// The `remora-pair:1:…` string (QR payload + copyable fallback).
    pub code: String,
    /// Unix seconds the window (and this code) expire — drives the countdown.
    pub expires_at: u64,
    /// The window lifetime, in seconds, it was opened for.
    pub ttl_secs: u64,
}

/// One paired device, for the roster view. `device_id` is the 64-hex string;
/// `fingerprint` is the short human-comparable form (ADR-0021 D5) of the
/// device's pinned static key.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfoDto {
    pub device_id: String,
    pub name: String,
    pub fingerprint: String,
    pub enrolled_at: Option<u64>,
    pub last_connected_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A pairing window opened; the UI shows `code` until `expires_at` (Unix
/// seconds). `code` embeds the PSK by design (ADR-0021 D1); never log it.
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct PairingWindowOpened {
    pub code: String,
    pub expires_at: u64,
}

/// A device reached the open window and awaits the user's confirm/reject; the
/// UI shows `fingerprint` for the human to compare against the device's screen.
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct PairingDeviceArrived {
    pub device_id: String,
    pub name: String,
    pub fingerprint: String,
}

/// A pairing attempt reached a terminal state.
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct PairingResult {
    pub outcome: PairingOutcomeDto,
}

/// The terminal outcome of one pairing attempt (ADR-0021).
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PairingOutcomeDto {
    // Field-level renames (not `rename_all_fields`, which specta does not honor)
    // keep the wire contract camelCase like every other DTO.
    Paired {
        #[serde(rename = "deviceId")]
        device_id: String,
        name: String,
    },
    Rejected {
        #[serde(rename = "deviceId")]
        device_id: String,
    },
    Expired,
}

/// The roster changed (a device enrolled or was revoked); the UI re-queries
/// `list_devices`. No payload — a ping, like [`super::commands::ConfigChanged`].
#[derive(Clone, serde::Serialize, serde::Deserialize, specta::Type, Event)]
pub struct RosterChanged;

// ---------------------------------------------------------------------------
// BridgeEvent → frontend event mapping
// ---------------------------------------------------------------------------

/// Which frontend event a [`BridgeEvent`] forwards to. One variant per emitted
/// event; the mapping is exercised for totality by [`tests`].
enum PairingEmit {
    WindowOpened(PairingWindowOpened),
    DeviceArrived(PairingDeviceArrived),
    Result(PairingResult),
    RosterChanged(RosterChanged),
}

/// Maps a [`PairingOutcome`] to its DTO. A future (`#[non_exhaustive]`) variant
/// has no UI meaning yet, so it degrades to `Expired` (a terminal "nothing was
/// granted" state) rather than being dropped — matching the repo's
/// non-exhaustive-guard convention (`SessionStateDto`).
fn map_outcome(outcome: PairingOutcome) -> PairingOutcomeDto {
    match outcome {
        PairingOutcome::Paired { device_id, name } => PairingOutcomeDto::Paired {
            device_id: device_id.to_string(),
            name,
        },
        PairingOutcome::Rejected { device_id } => PairingOutcomeDto::Rejected {
            device_id: device_id.to_string(),
        },
        PairingOutcome::Expired => PairingOutcomeDto::Expired,
        _ => PairingOutcomeDto::Expired,
    }
}

/// Maps one [`BridgeEvent`] to the frontend event to emit, or `None` for a
/// future (`#[non_exhaustive]`) variant with no frontend surface yet.
///
/// The window `generation` the bridge tags pairing events with (#299) is not
/// forwarded yet: surfacing it to the PairingDialog needs new DTO plumbing and
/// dialog-side filtering, which rides with the planned generation work in
/// #281. Until then the dialog keeps the pre-#299 behavior.
fn map_bridge_event(event: BridgeEvent) -> Option<PairingEmit> {
    match event {
        BridgeEvent::PairingWindowOpened {
            code, expires_at, ..
        } => Some(PairingEmit::WindowOpened(PairingWindowOpened {
            code: code.encode(),
            expires_at,
        })),
        BridgeEvent::PairingDeviceArrived {
            device_id,
            name,
            fingerprint,
            ..
        } => Some(PairingEmit::DeviceArrived(PairingDeviceArrived {
            device_id: device_id.to_string(),
            name,
            fingerprint,
        })),
        BridgeEvent::PairingResult { outcome, .. } => Some(PairingEmit::Result(PairingResult {
            outcome: map_outcome(outcome),
        })),
        BridgeEvent::RosterChanged => Some(PairingEmit::RosterChanged(RosterChanged)),
        _ => None,
    }
}

/// Emits a mapped bridge event to the frontend. Emit failure (no listener /
/// window gone) is ignored, matching the config-watcher ping.
fn emit_event(app: &AppHandle, emit: PairingEmit) {
    let _ = match emit {
        PairingEmit::WindowOpened(e) => e.emit(app),
        PairingEmit::DeviceArrived(e) => e.emit(app),
        PairingEmit::Result(e) => e.emit(app),
        PairingEmit::RosterChanged(e) => e.emit(app),
    };
}

/// Spawns the forwarder task that drains the bridge's [`BridgeEvent`] receiver
/// (taken out of `handles` once) and emits the matching frontend event for each.
/// A no-op if the receiver was already taken. Called once per bridge start
/// (launch or live reconfig, #277); the task's handle is stored back into
/// `handles` so a clean stop can join it (it ends on its own when the serve
/// task drops the event sender).
pub fn spawn_event_forwarder(app: AppHandle, handles: &PairingHandles) {
    let Some(mut rx) = handles.take_events() else {
        return;
    };
    let task = tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let Some(emit) = map_bridge_event(event) {
                emit_event(&app, emit);
            }
        }
    });
    handles.set_forwarder(task);
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------
//
// These are plain `async fn`s, not `#[tauri::command]`s: the repo keeps every
// command shim in `commands.rs` (so `collect_commands!` finds each generated
// helper macro in one module), and command bodies call into helper modules like
// this one. The thin `#[tauri::command]` wrappers live in `commands.rs`.

/// Opens (or replaces) this bridge's single pairing window for `ttl_secs`
/// seconds (defaulting when `None`), returning the code to render as a QR.
pub(crate) async fn open_window(
    app: &AppHandle,
    ttl_secs: Option<u64>,
) -> Result<PairingCodeDto, BridgeError> {
    let handles = handles(app)?;
    let ttl_secs = ttl_secs.unwrap_or(DEFAULT_PAIRING_TTL_SECS);
    let (reply_tx, reply_rx) = oneshot::channel();
    handles
        .commands
        .send(PairingCommand::OpenWindow {
            ttl_secs,
            reply: reply_tx,
        })
        .await
        .map_err(|_| bridge_gone())?;
    // Outer error: the bridge dropped the reply sender (serve task exited).
    // Inner error: the bridge could not mint/register the code.
    let code: PairingCode = reply_rx.await.map_err(|_| bridge_gone())??;
    // The bridge stamps `expires_at = now + ttl` at open; recompute the same
    // way for the countdown (the authoritative value also arrives on the
    // `PairingWindowOpened` event within milliseconds).
    let expires_at = now_secs().saturating_add(ttl_secs);
    Ok(PairingCodeDto {
        code: code.encode(),
        expires_at,
        ttl_secs,
    })
}

/// Confirms the arrived device's fingerprint: the bridge enrols it.
pub(crate) async fn confirm(app: &AppHandle, device_id: String) -> Result<(), BridgeError> {
    let handles = handles(app)?;
    let device_id = parse_device_id(&device_id)?;
    handles
        .commands
        .send(PairingCommand::Confirm { device_id })
        .await
        .map_err(|_| bridge_gone())
}

/// Rejects the arrived device: the bridge grants nothing durable.
pub(crate) async fn reject(app: &AppHandle, device_id: String) -> Result<(), BridgeError> {
    let handles = handles(app)?;
    let device_id = parse_device_id(&device_id)?;
    handles
        .commands
        .send(PairingCommand::Reject { device_id })
        .await
        .map_err(|_| bridge_gone())
}

/// Closes the current pairing window without pairing anyone.
pub(crate) async fn cancel(app: &AppHandle) -> Result<(), BridgeError> {
    let handles = handles(app)?;
    handles
        .commands
        .send(PairingCommand::CancelWindow)
        .await
        .map_err(|_| bridge_gone())
}

/// Lists this bridge's paired devices, read from the live shared roster.
pub(crate) async fn list(app: &AppHandle) -> Result<Vec<DeviceInfoDto>, BridgeError> {
    let handles = handles(app)?;
    let roster = handles.roster.read().await;
    Ok(roster
        .entries
        .iter()
        .map(|e| DeviceInfoDto {
            device_id: e.device_id.to_string(),
            name: e.name.clone(),
            fingerprint: fingerprint(&e.static_pubkey),
            enrolled_at: e.enrolled_at,
            last_connected_at: e.last_connected_at,
        })
        .collect())
}

/// Un-pairs a device: the bridge drops it from the roster, persists, and
/// re-asserts the shrunken set so the relay kicks any live session (D6).
pub(crate) async fn revoke(app: &AppHandle, device_id: String) -> Result<(), BridgeError> {
    let handles = handles(app)?;
    let device_id = parse_device_id(&device_id)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    handles
        .commands
        .send(PairingCommand::Revoke {
            device_id,
            reply: reply_tx,
        })
        .await
        .map_err(|_| bridge_gone())?;
    reply_rx.await.map_err(|_| bridge_gone())??;
    Ok(())
}

/// This bridge's own identity fingerprint (ADR-0021 D5) for the pairing UI.
pub(crate) fn own_fingerprint(app: &AppHandle) -> Result<String, BridgeError> {
    let handles = handles(app)?;
    Ok(handles.bridge_fingerprint.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_protocol::DeviceId;

    fn sample_code() -> PairingCode {
        PairingCode {
            relay_url: Some("wss://relay.example/ws".to_string()),
            rendezvous_token: Some("rzv".to_string()),
            mesh_addr: None,
            psk: [7u8; 32],
            bridge_id: DeviceId([1u8; 32]),
            bridge_key: [2u8; 32],
            bridge_name: None,
            min_protocol: 1,
        }
    }

    /// The `BridgeEvent → frontend event` mapping is total: every current
    /// variant produces its matching emit. (`BridgeEvent` is `#[non_exhaustive]`,
    /// so the wildcard cannot be dropped, but each *known* variant is covered.)
    #[test]
    fn maps_every_bridge_event_variant() {
        let opened = map_bridge_event(BridgeEvent::PairingWindowOpened {
            code: sample_code(),
            expires_at: 42,
            generation: 1,
        });
        match opened {
            Some(PairingEmit::WindowOpened(e)) => {
                assert!(e.code.starts_with("remora-pair:1:"));
                assert_eq!(e.expires_at, 42);
            }
            _ => panic!("PairingWindowOpened must map to WindowOpened"),
        }

        let arrived = map_bridge_event(BridgeEvent::PairingDeviceArrived {
            generation: 1,
            device_id: DeviceId([0xab; 32]),
            name: "phone".to_string(),
            fingerprint: "ABCD-1234-5678".to_string(),
        });
        match arrived {
            Some(PairingEmit::DeviceArrived(e)) => {
                assert_eq!(e.device_id, DeviceId([0xab; 32]).to_string());
                assert_eq!(e.name, "phone");
                assert_eq!(e.fingerprint, "ABCD-1234-5678");
            }
            _ => panic!("PairingDeviceArrived must map to DeviceArrived"),
        }

        let paired = map_bridge_event(BridgeEvent::PairingResult {
            generation: 1,
            outcome: PairingOutcome::Paired {
                device_id: DeviceId([0x11; 32]),
                name: "laptop".to_string(),
            },
        });
        match paired {
            Some(PairingEmit::Result(PairingResult {
                outcome: PairingOutcomeDto::Paired { device_id, name },
            })) => {
                assert_eq!(device_id, DeviceId([0x11; 32]).to_string());
                assert_eq!(name, "laptop");
            }
            _ => panic!("PairingResult(Paired) must map to Result(Paired)"),
        }

        let rejected = map_bridge_event(BridgeEvent::PairingResult {
            generation: 1,
            outcome: PairingOutcome::Rejected {
                device_id: DeviceId([0x22; 32]),
            },
        });
        assert!(matches!(
            rejected,
            Some(PairingEmit::Result(PairingResult {
                outcome: PairingOutcomeDto::Rejected { .. }
            }))
        ));

        let expired = map_bridge_event(BridgeEvent::PairingResult {
            generation: 1,
            outcome: PairingOutcome::Expired,
        });
        assert!(matches!(
            expired,
            Some(PairingEmit::Result(PairingResult {
                outcome: PairingOutcomeDto::Expired
            }))
        ));

        let roster = map_bridge_event(BridgeEvent::RosterChanged);
        assert!(matches!(roster, Some(PairingEmit::RosterChanged(_))));
    }

    #[test]
    fn device_id_parse_rejects_non_hex() {
        assert!(matches!(
            parse_device_id("not-hex"),
            Err(BridgeError::InvalidId { .. })
        ));
        // A valid 64-hex id round-trips.
        let id = DeviceId([0x3c; 32]).to_string();
        assert_eq!(parse_device_id(&id).expect("valid"), DeviceId([0x3c; 32]));
    }

    #[test]
    fn pairing_code_dto_serializes_camelcase() {
        let dto = PairingCodeDto {
            code: "remora-pair:1:abc".to_string(),
            expires_at: 100,
            ttl_secs: 120,
        };
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""expiresAt":100"#), "{json}");
        assert!(json.contains(r#""ttlSecs":120"#), "{json}");
    }

    #[test]
    fn device_info_dto_serializes_camelcase() {
        let dto = DeviceInfoDto {
            device_id: "ab".to_string(),
            name: "phone".to_string(),
            fingerprint: "ABCD-1234-5678".to_string(),
            enrolled_at: Some(1_765_000_000),
            last_connected_at: None,
        };
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""deviceId":"ab""#), "{json}");
        assert!(json.contains(r#""lastConnectedAt":null"#), "{json}");
    }
}
