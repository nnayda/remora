//! Real relay bridge launch wiring (ADR-0021 D7).
//!
//! When the per-device config carries a `[relay]` section, the desktop hosts
//! *its own* bridge at launch: it loads (or mints) this device's durable
//! identity and its paired-device roster from the app config dir — the same
//! load-bearing layout the dev loopback uses ([`crate::bridge_state`]) — and
//! spawns [`serve_bridge`] against the configured relay endpoint. Paired phones
//! then reach this device's sessions through the blind relay.
//!
//! Unlike the loopback ([`crate::remote_host`]), there is **no** dev-only
//! auto-confirm here: the pairing command/event channels are created and handed
//! back in [`PairingHandles`] for the pairing UI (Task 16, #232) to drive on a
//! real human decision. This module only makes the bridge run and exposes the
//! handles; it issues no pairing commands itself.
//!
//! The served source is the desktop's own [`ResolvingSource`], so a session a
//! phone drives through this bridge serializes its mutating ops against the
//! *same* per-session exclusion registry as the direct UI path (ADR-0021 D7).

use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    fingerprint, serve_bridge, BridgeConfig, BridgeEvent, BridgeIdentity, PairingCommand, Roster,
};
use remora_core::config::{Config, ConfigError};
use remora_core::SessionSource;

use crate::bridge::Bridge;
use crate::remote_host::ResolvingSource;

/// Channel depth for pairing commands/events between the UI and the bridge.
/// Pairing is a low-rate, human-driven ceremony; a small buffer is ample.
const PAIRING_CHANNEL_DEPTH: usize = 8;

/// The desktop-side ends of a running relay bridge's pairing channels, stored in
/// Tauri managed state so the pairing UI's commands (Task 16, #232) can reach
/// them. Present only when `[relay]` was configured at launch.
///
/// `commands` is cloned per outgoing [`PairingCommand`]; `events` is a single
/// consumer taken out exactly once at setup by the event-forwarder task (Task
/// 16), so it sits behind an `Option` guarded by a plain [`std::sync::Mutex`]
/// (locked once, never contended). Holding `shutdown` + the task handle keeps
/// the bridge serving for the app's lifetime and lets a future teardown cancel
/// it cleanly.
///
/// `roster` is the *same* `Arc<RwLock<Roster>>` the running bridge mutates on
/// every pairing/revocation, so `list_devices` reads the live set with no
/// staleness. `bridge_fingerprint` is this bridge's stable identity fingerprint,
/// captured once at launch (the static key never changes for the process life).
pub struct PairingHandles {
    /// Sender for pairing/roster commands into the running bridge.
    pub commands: mpsc::Sender<PairingCommand>,
    /// Receiver for the bridge's pairing/roster events. Single consumer: taken
    /// out once by the event-forwarder at setup ([`PairingHandles::take_events`]).
    pub events: std::sync::Mutex<Option<mpsc::Receiver<BridgeEvent>>>,
    /// The live paired-device roster, shared with the running bridge — reading it
    /// reflects pairings/revocations immediately (no on-disk re-read).
    pub roster: Arc<RwLock<Roster>>,
    /// This bridge's own identity fingerprint (ADR-0021 D5), for the pairing UI
    /// to display. Stable for the process lifetime.
    pub bridge_fingerprint: String,
    /// Cancels the bridge's serve loop on app teardown.
    pub shutdown: CancellationToken,
    /// The spawned `serve_bridge` task; kept so it lives for the app lifetime.
    pub task: tauri::async_runtime::JoinHandle<()>,
}

impl PairingHandles {
    /// Takes the bridge-event receiver out for the forwarder task. Returns the
    /// receiver on the first call and `None` after (single consumer). A poisoned
    /// lock also yields `None` — the forwarder simply does not start, which is
    /// safe (the commands still work; only live event push is lost).
    pub fn take_events(&self) -> Option<mpsc::Receiver<BridgeEvent>> {
        self.events.lock().ok().and_then(|mut guard| guard.take())
    }
}

/// Hosts this device's bridge when `[relay]` is configured, returning the
/// pairing-channel handles for the UI to drive (Task 16). Returns `None` when
/// no `[relay]` section is present.
///
/// Non-fatal by contract: a missing/unreadable config or an identity/roster
/// storage failure logs and yields `None` (the app keeps running on the direct
/// path) rather than bricking launch — mirroring the loopback's fallback.
pub(crate) fn start_relay_bridge(bridge: &Bridge) -> Option<PairingHandles> {
    let config_path = bridge.config_path();

    let relay = match load_relay_section(&config_path) {
        Ok(Some(relay)) => relay,
        // No `[relay]` section: the common case — run purely on the direct path.
        Ok(None) => return None,
        Err(e) => {
            eprintln!("relay bridge not started: could not read config: {e}");
            return None;
        }
    };

    // Durable identity (stable across runs) + the paired-device roster (persists
    // real pairings), both from the shared bridge-state layout.
    let identity =
        match BridgeIdentity::load_or_create(&crate::bridge_state::identity_path(&config_path)) {
            Ok(identity) => identity,
            Err(e) => {
                eprintln!("relay bridge not started: bridge identity unavailable: {e}");
                return None;
            }
        };
    let roster_path = crate::bridge_state::roster_path(&config_path);
    let roster = match Roster::load(&roster_path) {
        Ok(roster) => roster,
        Err(e) => {
            eprintln!("relay bridge not started: roster unavailable: {e}");
            return None;
        }
    };

    // Serve through the desktop's own resolver so a phone-driven session shares
    // the one per-session exclusion registry with the direct path (ADR-0021 D7).
    let source: Arc<dyn SessionSource> =
        Arc::new(ResolvingSource::new(bridge.resolver(), config_path));

    // This bridge's own fingerprint (ADR-0021 D5) for the pairing UI. Captured
    // now, before `identity` moves into the config; the static key is stable for
    // the process life, so no re-read is ever needed.
    let bridge_fingerprint = fingerprint(&identity.static_keypair.public);

    // Share the one roster Arc between the running bridge and the UI's
    // `list_devices`: the bridge mutates it on every pairing/revocation, so
    // reading this handle reflects the live set with no staleness.
    let roster = Arc::new(RwLock::new(roster));

    let shutdown = CancellationToken::new();
    let bridge_cfg = BridgeConfig {
        relay_url: relay.relay_url,
        registration_token: relay.registration_token,
        identity,
        roster: roster.clone(),
        roster_path,
    };

    let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(PAIRING_CHANNEL_DEPTH);
    let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(PAIRING_CHANNEL_DEPTH);
    let shutdown_task = shutdown.clone();
    let task = tauri::async_runtime::spawn(async move {
        // `serve_bridge` only returns `Err` on an unusable configuration (e.g. a
        // non-`ws` relay URL); transient relay/network failures are retried
        // internally. Log a fatal stop rather than panic.
        if let Err(e) =
            serve_bridge(bridge_cfg, source, commands_rx, events_tx, shutdown_task).await
        {
            eprintln!("relay bridge stopped: {e}");
        }
    });

    Some(PairingHandles {
        commands: commands_tx,
        events: std::sync::Mutex::new(Some(events_rx)),
        roster,
        bridge_fingerprint,
        shutdown,
        task,
    })
}

/// Loads the config's `[relay]` section at launch. A *missing* file is success
/// with no section (a fresh device is valid, ADR-0004); any other load failure
/// is surfaced so the caller can log it.
fn load_relay_section(
    config_path: &std::path::Path,
) -> Result<Option<remora_core::config::RelayConfigSection>, ConfigError> {
    match Config::load(config_path) {
        Ok(config) => Ok(config.relay),
        Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp path per process/tag so concurrent `cargo test` runs don't
    /// collide (matches the convention elsewhere in the crate).
    fn temp_config_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "remora-relay-cfg-{}-{}.toml",
            tag,
            std::process::id()
        ))
    }

    #[test]
    fn missing_config_yields_no_relay() {
        let path = temp_config_path("missing").join("definitely-absent.toml");
        assert!(load_relay_section(&path)
            .expect("missing file is ok")
            .is_none());
    }

    #[test]
    fn config_without_relay_yields_none() {
        let path = temp_config_path("no-relay");
        std::fs::write(&path, "[agents.claude]\ncommand = [\"claude\"]\n").expect("write");
        let got = load_relay_section(&path);
        std::fs::remove_file(&path).ok();
        assert!(got.expect("valid config").is_none());
    }

    #[test]
    fn config_with_relay_is_loaded() {
        let path = temp_config_path("with-relay");
        std::fs::write(
            &path,
            "[relay]\nrelay_url = \"wss://relay.example/ws\"\nregistration_token = \"reg-tok\"\n",
        )
        .expect("write");
        let got = load_relay_section(&path);
        std::fs::remove_file(&path).ok();
        let relay = got.expect("valid config").expect("relay section present");
        assert_eq!(relay.relay_url, "wss://relay.example/ws");
        assert_eq!(relay.registration_token, "reg-tok");
    }
}
