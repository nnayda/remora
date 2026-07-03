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
//! Bridge identity persists under the app config dir like real bridge state; the
//! relay's bridge-registration token is minted fresh per run (spec D11). The
//! device roster starts **empty** and is populated by the real pairing ceremony
//! (ADR-0021 D3): [`start_loopback`] opens a pairing window, runs the device-side
//! [`run_pairing`] driver over the in-process relay, and auto-confirms the (self)
//! device — see [`drive_loopback_pairing`] for why the auto-confirm is dev-only.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use rand::Rng as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    run_pairing, serve_bridge, wake_channel, BridgeConfig, BridgeEvent, BridgeIdentity,
    BridgeWakeHandle, PairingCommand, PairingFile, PairingProgress, RemoteSource, Roster,
};
use remora_core::config::{Config, ConfigError};
use remora_core::{SessionChannel, SessionSource, SourceError};
use remora_protocol::{AgentId, ProjectId, SessionId, SessionMeta, SpawnSpec};
use remora_relay::{serve, AuditSink, BridgeEntry, PushConfig, RelayConfig};

use crate::bridge::resolve::SourceResolver;
use crate::bridge::Bridge;

/// Lifetime of the dev-loopback pairing window. The self-device auto-confirms
/// within milliseconds, so this only bounds a wedged ceremony; kept short.
const LOOPBACK_PAIRING_TTL_SECS: u64 = 30;

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
    /// Cheap, cloneable handle the desktop's output pump tees session status
    /// transitions into, so a session driven through the loopback can push-wake
    /// the (self) device over the in-process relay (#233). Held here so the
    /// wake channel stays open for the loopback's life; `lib.rs` clones it into
    /// the Bridge via `set_wake_handle`.
    pub wake: BridgeWakeHandle,
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
        let (wake, _wake_rx) = wake_channel();
        RemoteHost {
            remote,
            wake,
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
    // Bridge identity lives alongside config.toml (`…/remora/`), so the bridge id
    // is stable across runs like real bridge state; the roster is per-run (below).
    // The identity/roster paths come from `bridge_state` so the loopback and the
    // real relay bridge share one load-bearing layout (ADR-0021).
    let config_path = bridge.config_path();
    let identity =
        BridgeIdentity::load_or_create(&crate::bridge_state::identity_path(&config_path))?;
    // Start from an EMPTY roster: the device this run pairs is enrolled by the
    // real ceremony below, not seeded inline. The confirm path does persist the
    // enrolled entry to `bridge_roster.toml`, but it is overwritten each run and
    // never loaded back — a saved entry could not reconnect through a later run's
    // fresh relay + per-run credentials anyway, so the on-disk copy is harmless
    // and any stale roster from an older build is deliberately ignored.
    let roster = Roster::default();

    // Fresh bridge-registration token per run (spec D11): the relay authorizes
    // this run's bridge and nothing else. The per-device relay credential is
    // minted by the bridge during the confirm-gated pairing below.
    let registration_token = random_token();
    let bridge_id = identity.device_id;

    // If `[relay] push_wake_url` is configured, the client half registers it with
    // the bridge on connect (ADR-0023, #233) — dogfooding the whole client-side
    // registration path over the in-process relay. Absent → registration off.
    let push_endpoint = load_push_wake_url(&config_path);

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
        // Enable in-process relay delivery when a push endpoint is configured, so
        // `REMORA_REMOTE_LOOPBACK=1` + `[relay] push_wake_url` dogfoods a real
        // end-to-end POST (spec Goal 5, ADR-0023). ntfy.sh is public https, so the
        // default network policy (no http, no private targets) suffices.
        //
        // M3 dogfooder note (#233): a session ATTACHED through the loopback keeps
        // the self-device in the relay's `live_peers` (it is dialed in), so the
        // wake is *dropped as connected* for that session. Only a *direct*-spawned
        // session can demo a delivered wake — and the loopback's hybrid routing
        // keeps `spawn` on the direct path, so that is the default. Registration
        // happens on the client's dial, so attach through the loopback at least
        // once first to register the endpoint, then drive a direct-spawned session
        // to `Awaiting` to see the POST.
        push: loopback_push_config(push_endpoint.as_deref()),
    });
    let audit = AuditSink::new(&relay_cfg)?;
    let (addr, relay_accept) = serve(relay_cfg, audit).await?;
    let relay_url = format!("ws://{addr}");

    // The bridge serves through the *same* wrapping resolver the direct path
    // uses (resolver carries the shared SessionLocks), so both actors serialize
    // per session against one registry.
    let source: Arc<dyn SessionSource> =
        Arc::new(ResolvingSource::new(bridge.resolver(), config_path.clone()));

    let shutdown = CancellationToken::new();
    let bridge_cfg = BridgeConfig {
        relay_url,
        registration_token,
        identity,
        roster: Arc::new(tokio::sync::RwLock::new(roster)),
        roster_path: crate::bridge_state::roster_path(&config_path),
    };
    // Thread the pairing command/event channels through `serve_bridge` and drive
    // the real ceremony over them (below). Once pairing completes we drop both
    // ends: the bridge keeps serving (a closed command channel just disables the
    // command branch) — no further commands are issued in loopback mode.
    let (commands_tx, commands_rx) = tokio::sync::mpsc::channel::<PairingCommand>(8);
    let (events_tx, events_rx) = tokio::sync::mpsc::channel::<BridgeEvent>(8);
    // Keep the wake handle (#233): `lib.rs` clones it into the Bridge via
    // `set_wake_handle` so the output pump tees session status transitions into
    // this loopback's push path — the in-process relay then delivers the wake
    // when a push endpoint is configured (see `loopback_push_config` above).
    let (wake, wake_rx) = wake_channel();
    let shutdown_c = shutdown.clone();
    let bridge_task = tokio::spawn(async move {
        if let Err(e) = serve_bridge(
            bridge_cfg,
            source,
            commands_rx,
            events_tx,
            wake_rx,
            shutdown_c,
        )
        .await
        {
            eprintln!("loopback bridge stopped: {e}");
        }
    });

    // Run the real pairing ceremony against our own bridge (dev-only auto-confirm)
    // and take the resulting durable `PairingFile` for the client transport.
    let pairing = drive_loopback_pairing(commands_tx, events_rx).await?;

    Ok(RemoteHost {
        remote: Arc::new(RemoteSource::new(pairing).with_push_endpoint(push_endpoint)),
        wake,
        shutdown,
        bridge_task,
        relay_accept,
    })
}

/// Builds the in-process relay's [`PushConfig`] for the dev loopback: delivery is
/// enabled exactly when a `[relay] push_wake_url` is configured, so the loopback
/// dogfoods a real end-to-end POST without a separate config step (ADR-0023,
/// #233). Everything else keeps the default policy (ntfy.sh is public https).
fn loopback_push_config(push_endpoint: Option<&str>) -> PushConfig {
    PushConfig {
        enabled: push_endpoint.is_some(),
        ..Default::default()
    }
}

/// Reads `[relay] push_wake_url` from the config at `config_path`, tolerating a
/// missing/unreadable/invalid file (a fresh device is valid; a config problem is
/// already surfaced elsewhere) by yielding `None` — registration simply stays off.
fn load_push_wake_url(config_path: &std::path::Path) -> Option<String> {
    Config::load(config_path)
        .ok()
        .and_then(|c| c.relay)
        .and_then(|r| r.push_wake_url)
}

/// Drives the ADR-0021 pairing ceremony against this run's freshly-served bridge
/// and returns the resulting durable [`PairingFile`].
///
/// **Dev-only auto-confirm.** This is the `REMORA_REMOTE_LOOPBACK` dogfood path:
/// the desktop pairs *with its own in-process bridge*, so there is no second
/// operator to eyeball the fingerprint. It opens a pairing window, runs the real
/// device-side [`run_pairing`] driver over the in-process relay, and — the moment
/// the bridge surfaces the arriving (self) device — auto-confirms it. The real
/// pairing UI (a later PR) confirms only on an explicit human decision; this
/// shortcut exists solely because the loopback's two ends are the same machine.
async fn drive_loopback_pairing(
    commands_tx: tokio::sync::mpsc::Sender<PairingCommand>,
    mut events_rx: tokio::sync::mpsc::Receiver<BridgeEvent>,
) -> Result<PairingFile, Box<dyn std::error::Error + Send + Sync>> {
    // Open this run's single pairing window; the bridge mints the code (relay
    // endpoint, bridge identity, one-shot PSK) and replies with it.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    commands_tx
        .send(PairingCommand::OpenWindow {
            ttl_secs: LOOPBACK_PAIRING_TTL_SECS,
            reply: reply_tx,
        })
        .await
        .map_err(|_| "loopback bridge stopped before opening a pairing window")?;
    let code = reply_rx
        .await
        .map_err(|_| "loopback bridge dropped the pairing-window reply")?
        .map_err(|e| format!("open pairing window failed: {e:?}"))?;

    // The device-side driver runs the whole IKpsk2 ceremony over the relay. Its
    // progress is UI-only; drain it so the driver's best-effort sends never wedge.
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<PairingProgress>(8);
    tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });

    let pairing = run_pairing(code, "loopback device".to_string(), progress_tx);
    tokio::pin!(pairing);

    loop {
        tokio::select! {
            event = events_rx.recv() => match event {
                // Auto-confirm the (self) device the instant it arrives — dev-only,
                // see this function's doc comment.
                Some(BridgeEvent::PairingDeviceArrived { device_id, .. }) => {
                    commands_tx
                        .send(PairingCommand::Confirm { device_id })
                        .await
                        .map_err(|_| "loopback bridge stopped before confirm")?;
                }
                // Window-opened / result / roster-changed need no action here; the
                // driver's return value is the source of truth for success.
                Some(_) => {}
                None => {
                    return Err(
                        "loopback bridge closed its event channel before pairing completed".into(),
                    )
                }
            },
            result = &mut pairing => {
                return result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                    format!("loopback pairing failed: {e}").into()
                });
            }
        }
    }
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
pub(crate) struct ResolvingSource {
    resolver: Arc<dyn SourceResolver>,
    config_path: PathBuf,
}

impl ResolvingSource {
    /// Build a source that resolves each request through `resolver` against the
    /// config at `config_path`. Shared by the loopback and the real relay bridge
    /// so both serve through the desktop's one per-session exclusion registry.
    pub(crate) fn new(resolver: Arc<dyn SourceResolver>, config_path: PathBuf) -> Self {
        Self {
            resolver,
            config_path,
        }
    }

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

    async fn remote_workspace(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        workspace_path: &str,
    ) -> Result<remora_core::RemoteWorkspace, SourceError> {
        self.for_project(project_id)?
            .remote_workspace(project_id, session_id, workspace_path)
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
    fn loopback_push_enabled_reflects_endpoint_presence() {
        // A configured push endpoint enables in-process relay delivery so the
        // loopback can dogfood a real POST (#233); absent leaves it disabled.
        assert!(loopback_push_config(Some("https://ntfy.sh/remora-demo")).enabled);
        assert!(!loopback_push_config(None).enabled);
        // Everything else stays at the default policy.
        assert_eq!(
            PushConfig {
                enabled: false,
                ..loopback_push_config(Some("https://ntfy.sh/remora-demo"))
            },
            PushConfig::default(),
        );
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
