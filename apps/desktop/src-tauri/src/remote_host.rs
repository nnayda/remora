//! Dev-only relay loopback (ADR-0021 spec D11): the desktop attaches to its own
//! sessions *through its own bridge*, over a real in-process blind relay on
//! `127.0.0.1`, exercising the whole Noise + envelope stack the phone will use.
//!
//! Gated entirely behind `REMORA_REMOTE_LOOPBACK=1` — off by default, no UI
//! surface. When on, [`start_loopback`] stands up:
//!
//! ```text
//! ResolvingSource (shares the Bridge's ONE SessionLocks via its resolver)
//!   → serve_bridge  ⇄  [ ws://127.0.0.1:0 blind relay ]  ⇄  RemoteSource
//! ```
//!
//! The bridge serves the *same* wrapping resolver the desktop's direct path
//! uses, so a session driven concurrently by the direct UI path and by the
//! loopback bridge serializes its mutating ops against the one lock registry —
//! the load-bearing cross-device-exclusion demo (ADR-0021 D7).
//!
//! **Hybrid routing (spec D11):** only `attach` routes through the loopback
//! `RemoteSource`; `list` and every mutating op stay on the direct path. Attach
//! is the load-bearing "desktop attaches via its own bridge" dogfood; `list`
//! keeps the host-grouped `SessionListDto` the frontend expects (the flat
//! `RemoteSource::list` would collapse host identity). See the Bridge's
//! `session_source_for_attach`.
//!
//! Bridge identity + roster persist under the app config dir like real bridge
//! state; the relay's admission tokens are minted fresh per run (spec D11).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use rand::RngCore as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    serve_bridge, BridgeConfig, BridgeIdentity, PairingFile, RemoteSource, Roster, RosterEntry,
    NOISE_PATTERN,
};
use remora_core::config::{Config, ConfigError};
use remora_core::{SessionChannel, SessionSource, SourceError};
use remora_protocol::{AgentId, DeviceId, ProjectId, SessionId, SessionMeta, SpawnSpec};
use remora_relay::{serve, AuditSink, BridgeEntry, RelayConfig};

use crate::bridge::resolve::SourceResolver;
use crate::bridge::Bridge;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// `true` only when `REMORA_REMOTE_LOOPBACK` is exactly `"1"`. Any other value
/// (unset, `"0"`, `"true"`, whitespace) leaves the loopback off.
pub fn loopback_enabled() -> bool {
    loopback_enabled_value(std::env::var("REMORA_REMOTE_LOOPBACK").ok().as_deref())
}

/// The pure decision, split out so it can be unit-tested without racing on the
/// process-global environment.
fn loopback_enabled_value(value: Option<&str>) -> bool {
    value == Some("1")
}

/// A live loopback: the client-side [`RemoteSource`] the Bridge routes `attach`
/// through, plus the relay + bridge tasks that keep it serving. Held by the
/// [`Bridge`] so the tasks live for the app's lifetime; dropping it tears the
/// whole loopback down.
pub struct RemoteHost {
    /// The client transport the Bridge's `attach` routes through.
    pub remote: Arc<dyn SessionSource>,
    shutdown: CancellationToken,
    bridge_task: JoinHandle<()>,
    relay_accept: JoinHandle<()>,
}

impl Drop for RemoteHost {
    fn drop(&mut self) {
        // Signal the bridge's reconnect loop to stop, then abort both tasks so a
        // shutting-down app leaves no relay/bridge task spinning.
        self.shutdown.cancel();
        self.bridge_task.abort();
        self.relay_accept.abort();
    }
}

impl RemoteHost {
    /// A cheap stand-in for unit tests: a fake `remote` and no-op tasks. Lets the
    /// Bridge routing test inject a `RemoteHost` without standing up a relay.
    #[cfg(test)]
    pub(crate) fn stub_for_test(remote: Arc<dyn SessionSource>) -> RemoteHost {
        RemoteHost {
            remote,
            shutdown: CancellationToken::new(),
            bridge_task: tokio::spawn(async {}),
            relay_accept: tokio::spawn(async {}),
        }
    }
}

/// Stands up the loopback described in the module docs and returns a live
/// [`RemoteHost`]. Reuses the Bridge's resolver and config path so the served
/// source shares the one [`SessionLocks`](remora_core::SessionLocks) registry.
///
/// A failure here is non-fatal to the app: the caller logs it and falls back to
/// the direct path (see `lib.rs`).
pub async fn start_loopback(
    bridge: &Bridge,
) -> Result<RemoteHost, Box<dyn std::error::Error + Send + Sync>> {
    // Bridge identity + roster live alongside config.toml (`…/remora/`), so the
    // bridge id is stable across runs like real bridge state.
    let config_path = bridge.config_path();
    let state_dir: PathBuf = config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let identity = BridgeIdentity::load_or_create(&state_dir.join("bridge_identity.toml"))?;
    // Load-or-empty; the per-run device is ephemeral (fresh keypair + per-run
    // tokens below), so we never persist the roster — a saved entry could never
    // reconnect through a later run's fresh relay anyway.
    let mut roster = Roster::load(&state_dir.join("bridge_roster.toml")).unwrap_or_default();

    // Fresh admission tokens per run (spec D11): the relay authorizes this run's
    // bridge + device and nothing else.
    let registration_token = random_token();
    let device_relay_token = random_token();

    // Loopback-only scaffolding: mints this run's ephemeral device + pairing
    // file inline. This replaces the slice-1 `provision_device` helper (deleted
    // in #232) — it is dev-only dogfood wiring, not the real pairing story; the
    // out-of-band pairing ceremony (QR display, confirm-gated enrollment)
    // lands in this branch's later work and replaces this block outright.
    let bridge_id = identity.device_id;
    let device_keypair = {
        let params: snow::params::NoiseParams =
            NOISE_PATTERN.parse().map_err(|e: snow::Error| {
                Box::<dyn std::error::Error + Send + Sync>::from(e.to_string())
            })?;
        snow::Builder::new(params)
            .generate_keypair()
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))?
    };
    let device_id = DeviceId(rand::random());
    let mut psk = [0u8; 32];
    rand::rng().fill_bytes(&mut psk);

    roster.entries.push(RosterEntry {
        device_id,
        static_pubkey: device_keypair.public.clone(),
        psk,
        relay_token: device_relay_token.clone(),
        name: "desktop loopback".to_string(),
        enrolled_at: None,
        last_connected_at: None,
    });

    // Provisioned against a placeholder relay URL; the real ws:// URL is
    // stamped in once the relay's ephemeral port is known.
    let pairing = PairingFile {
        relay_url: "ws://placeholder".to_string(),
        device_token: device_relay_token.clone(),
        bridge_id,
        bridge_static_pubkey: B64.encode(&identity.static_keypair.public),
        psk: B64.encode(psk),
        device_id,
        device_private_key: B64.encode(&device_keypair.private),
        device_public_key: B64.encode(&device_keypair.public),
    };

    let relay_cfg = Arc::new(RelayConfig {
        listen: "127.0.0.1:0".to_string(),
        bridges: vec![BridgeEntry {
            token: registration_token.clone(),
            device_id: bridge_id,
        }],
        buffer_bytes: 1 << 20,
        handshake_timeout_secs: 10,
        max_connections: 1024,
        audit: None,
    });
    let audit = AuditSink::new(&relay_cfg)?;
    let (addr, relay_accept) = serve(relay_cfg, audit).await?;

    let relay_url = format!("ws://{addr}");
    let mut pairing = pairing;
    pairing.relay_url = relay_url.clone();

    // The bridge serves through the *same* wrapping resolver the direct path
    // uses (resolver carries the shared SessionLocks), so both actors serialize
    // per session against one registry.
    let source: Arc<dyn SessionSource> = Arc::new(ResolvingSource {
        resolver: bridge.resolver(),
        config_path,
    });

    let shutdown = CancellationToken::new();
    let bridge_cfg = BridgeConfig {
        relay_url,
        registration_token,
        identity,
        roster: Arc::new(tokio::sync::RwLock::new(roster)),
        roster_path: state_dir.join("bridge_roster.toml"),
    };
    // Task 10 threads the pairing command/event channels through `serve_bridge`.
    // The loopback dogfood path does not drive pairing yet (Task 14 rewrites this
    // to run the real ceremony), so the desktop end of each channel is unused for
    // now: an unfed command receiver and an undrained event sender both degrade
    // gracefully (the bridge disables its command branch and ignores send errors).
    let (_commands_tx, commands_rx) = tokio::sync::mpsc::channel(8);
    let (events_tx, _events_rx) = tokio::sync::mpsc::channel(8);
    let shutdown_c = shutdown.clone();
    let bridge_task = tokio::spawn(async move {
        if let Err(e) = serve_bridge(bridge_cfg, source, commands_rx, events_tx, shutdown_c).await {
            eprintln!("loopback bridge stopped: {e}");
        }
    });

    Ok(RemoteHost {
        remote: Arc::new(RemoteSource::new(pairing)),
        shutdown,
        bridge_task,
        relay_accept,
    })
}

/// 32 random bytes, hex-encoded — a per-run relay admission token.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The [`SessionSource`] the loopback bridge serves: it resolves each request's
/// project through the desktop's own resolver against freshly-loaded config,
/// exactly like the Bridge's direct path — so the bridge and the direct path go
/// through the same per-session exclusion registry.
struct ResolvingSource {
    resolver: Arc<dyn SourceResolver>,
    config_path: PathBuf,
}

impl ResolvingSource {
    /// Load config fresh (a missing file is an empty config — a fresh device is
    /// valid, ADR-0004). Config problems surface as `Transport` across the seam.
    fn load_config(&self) -> Result<Arc<Config>, SourceError> {
        match Config::load(&self.config_path) {
            Ok(config) => Ok(Arc::new(config)),
            Err(ConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Arc::new(Config::default()))
            }
            Err(e) => Err(SourceError::Transport(format!("config load failed: {e}"))),
        }
    }

    /// Resolve `project_id`'s (already exclusion-wrapped) source from fresh
    /// config.
    fn for_project(&self, project_id: &ProjectId) -> Result<Arc<dyn SessionSource>, SourceError> {
        let config = self.load_config()?;
        // `BridgeError` is a frontend serde DTO (no `Display`); render its Debug
        // for the transport detail. `Display` on `SourceError::Transport` still
        // escapes/bounds it before it ever reaches a log or the wire.
        self.resolver
            .for_project(&config, project_id)
            .map_err(|e| SourceError::Transport(format!("resolve failed: {e:?}")))
    }
}

#[async_trait]
impl SessionSource for ResolvingSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let source = self.for_project(&spec.project_id)?;
        source.spawn(spec).await
    }

    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        self.for_project(project_id)?
            .attach(project_id, session_id)
            .await
    }

    async fn external_attach_command(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<Vec<String>, SourceError> {
        self.for_project(project_id)?
            .external_attach_command(project_id, session_id)
            .await
    }

    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        self.for_project(project_id)?
            .respawn(project_id, session_id, agent)
            .await
    }

    async fn stop(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<(), SourceError> {
        self.for_project(project_id)?
            .stop(project_id, session_id)
            .await
    }

    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError> {
        self.for_project(project_id)?
            .remove(project_id, session_id, force)
            .await
    }

    /// Every configured host's sessions, flattened. Not routed through by the
    /// desktop today (hybrid keeps `list` direct), but implemented so the served
    /// source is complete and any future both-route switch is a one-liner.
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let config = self.load_config()?;
        let sources = self.resolver.all(&config);
        let results = futures_util::future::join_all(
            sources
                .into_iter()
                .map(|(_id, src)| async move { src.list().await }),
        )
        .await;
        let mut all = Vec::new();
        for result in results {
            all.extend(result?);
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_enabled_reads_env() {
        assert!(loopback_enabled_value(Some("1")));
        assert!(!loopback_enabled_value(Some("0")));
        assert!(!loopback_enabled_value(Some("true")));
        assert!(!loopback_enabled_value(Some(" 1")));
        assert!(!loopback_enabled_value(None));
    }

    #[test]
    fn random_token_is_64_hex_chars_and_unique() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "per-run tokens must not repeat");
    }
}
