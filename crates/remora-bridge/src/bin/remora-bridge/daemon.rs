//! The `serve` daemon (#234): validates config, claims the identity and the
//! single-instance lock, binds the hardened ctl socket, runs `serve_bridge`,
//! and exits cleanly on SIGTERM/SIGINT. Startup failures are loud, named,
//! and happen before any background task spawns (spec: error handling).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, watch, RwLock};
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    fingerprint, is_ws_url, serve_bridge, wake_channel, BridgeConfig, BridgeEvent, BridgeHealth,
    BridgeIdentity, IdentityLock, PairingCommand, Roster,
};
use remora_core::config::Config;
use remora_core::resolve::{ConfigResolver, ResolvingSource};
use remora_core::{SessionLocks, SessionSource};

pub const CTL_SOCKET: &str = "ctl.sock";
pub const DAEMON_LOCK: &str = "daemon.lock";
const IDENTITY_FILE: &str = "bridge_identity.toml";
const ROSTER_FILE: &str = "bridge_roster.toml";
/// D1: bound on one ctl request line; a wedged/hostile client cannot grow
/// an unbounded buffer.
#[allow(dead_code, reason = "consumed by the ctl server, plan Task 9")]
pub(crate) const MAX_REQUEST_LINE: usize = 64 * 1024;
/// D1: a connection must present its first request promptly; interactive
/// pauses only happen later, inside a pair ceremony (bounded by the window).
#[allow(dead_code, reason = "consumed by the ctl server, plan Task 9")]
pub(crate) const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(10);
const PAIRING_CHANNEL_DEPTH: usize = 8;
const EVENT_FANOUT_DEPTH: usize = 64;

/// Shared handles the ctl server serves requests from (Task 9).
#[allow(
    dead_code,
    reason = "fields consumed by the ctl server, plan Task 9; the stub ignores them"
)]
#[derive(Clone)]
pub(crate) struct DaemonState {
    pub commands: mpsc::Sender<PairingCommand>,
    pub events: broadcast::Sender<BridgeEvent>,
    pub health: watch::Receiver<BridgeHealth>,
    pub roster: Arc<RwLock<Roster>>,
    pub device_id: String,
    pub fingerprint: String,
}

pub fn identity_path(state_dir: &Path) -> PathBuf {
    state_dir.join(IDENTITY_FILE)
}

/// `init` (spec D12): offline identity mint/load — no [relay], no network.
/// Prints the two values a relay operator and a pairing human need.
pub fn run_init(state_dir: &Path) -> Result<(), String> {
    ensure_state_dir(state_dir)?;
    let path = identity_path(state_dir);
    let _lock = IdentityLock::acquire(&path).map_err(|e| e.to_string())?;
    let identity = BridgeIdentity::load_or_create(&path).map_err(|e| e.to_string())?;
    println!("device_id   {}", identity.device_id);
    println!(
        "fingerprint {}",
        fingerprint(&identity.static_keypair.public)
    );
    println!();
    println!("Register this bridge on your relay (relay.toml):");
    println!("  [[bridges]]");
    println!("  device_id = \"{}\"", identity.device_id);
    println!("  token = \"<mint a long random token>\"");
    Ok(())
}

pub async fn run_serve(config_path: PathBuf, state_dir: PathBuf) -> Result<(), String> {
    // ---- Startup validation: loud, named, pre-daemonization (G2). ----
    let config = Config::load(&config_path)
        .map_err(|e| format!("config `{}`: {e}", config_path.display()))?;
    let relay = config.relay.clone().ok_or_else(|| {
        format!(
            "config `{}` has no [relay] section — a headless bridge without a relay \
             cannot serve devices (mesh mode is #275)",
            config_path.display()
        )
    })?;
    if !is_ws_url(&relay.relay_url) {
        return Err(format!(
            "[relay].relay_url must be ws:// or wss://, got `{}`",
            relay.relay_url
        ));
    }
    ensure_state_dir(&state_dir)?;

    // ---- Single-instance + identity claims (D1, D2; G4, G12). ----
    let daemon_lock = acquire_daemon_lock(&state_dir)?;
    let identity_file = identity_path(&state_dir);
    let identity_lock = IdentityLock::acquire(&identity_file).map_err(|e| e.to_string())?;
    let identity = BridgeIdentity::load_or_create(&identity_file).map_err(|e| e.to_string())?;
    let roster_path = state_dir.join(ROSTER_FILE);
    let roster = Roster::load(&roster_path).map_err(|e| e.to_string())?;

    let device_id = identity.device_id.to_string();
    let own_fingerprint = fingerprint(&identity.static_keypair.public);
    eprintln!("remora-bridge {device_id} (fingerprint {own_fingerprint})");
    eprintln!("state dir {}", state_dir.display());

    // ---- Hardened ctl socket (D1; G3, G5): we hold the daemon lock, so a
    // leftover socket is provably stale — unlink and rebind under a tight
    // umask so the socket is never world-connectable, even briefly. ----
    let socket_path = state_dir.join(CTL_SOCKET);
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .map_err(|e| format!("removing stale {}: {e}", socket_path.display()))?;
    }
    let listener = bind_private(&socket_path)?;

    // ---- Engine wiring: the same shape as the desktop hosts (relay.rs). ----
    let source: Arc<dyn SessionSource> = Arc::new(ResolvingSource::new(
        Arc::new(ConfigResolver::new(SessionLocks::new())),
        config_path.clone(),
    ));
    let roster = Arc::new(RwLock::new(roster));
    let (health_tx, health_rx) = watch::channel(BridgeHealth::Starting);
    let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(PAIRING_CHANNEL_DEPTH);
    let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(PAIRING_CHANNEL_DEPTH);
    // KNOWN GAP: the push-pipeline wake path (#233) is fed by the desktop's
    // session-output pump (bridge/mod.rs wires `wake.note_session_status`);
    // the headless daemon has no such pump, so nothing ever sends on
    // `_wake_handle` (it stays alive to the end of run_serve, unused) and
    // the bridge's wake arm simply never fires — disconnected phones get no
    // PushTrigger from a headless bridge yet. Tracked as a #234 follow-up
    // issue.
    let (_wake_handle, wake_rx) = wake_channel();
    let shutdown = CancellationToken::new();

    let bridge_cfg = BridgeConfig {
        relay_url: relay.relay_url,
        registration_token: relay.registration_token,
        identity,
        roster: roster.clone(),
        roster_path,
        health: health_tx,
    };
    let serve_shutdown = shutdown.clone();
    let bridge_task = tokio::spawn(async move {
        // Locks live exactly as long as the daemon (D2).
        let _identity_lock = identity_lock;
        let _daemon_lock = daemon_lock;
        if let Err(e) = serve_bridge(
            bridge_cfg,
            source,
            commands_rx,
            events_tx,
            wake_rx,
            serve_shutdown,
        )
        .await
        {
            eprintln!("remora-bridge: bridge stopped: {e}");
        }
    });

    // ---- Event fan-out (D13b): the daemon ALWAYS drains BridgeEvents so
    // the engine's bounded sends can never back-pressure; subscribers (a
    // live `pair` session) get a broadcast copy, everyone else drops. ----
    let (fanout_tx, _) = broadcast::channel::<BridgeEvent>(EVENT_FANOUT_DEPTH);
    let fanout = fanout_tx.clone();
    let drain_shutdown = shutdown.clone();
    tokio::spawn(drain_events(events_rx, fanout, drain_shutdown));

    let state = DaemonState {
        commands: commands_tx,
        events: fanout_tx,
        health: health_rx,
        roster,
        device_id,
        fingerprint: own_fingerprint,
    };
    let ctl_shutdown = shutdown.clone();
    let ctl_task = tokio::spawn(crate::ctl_server::serve_ctl(listener, state, ctl_shutdown));

    // ---- Signals → cancel → cleanup (G3). ----
    wait_for_shutdown_signal().await;
    eprintln!("remora-bridge: shutting down");
    shutdown.cancel();
    let _ = bridge_task.await;
    ctl_task.abort();
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn drain_events(
    mut events: mpsc::Receiver<BridgeEvent>,
    fanout: broadcast::Sender<BridgeEvent>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            event = events.recv() => match event {
                Some(event) => {
                    // No subscriber is fine — send() just returns Err.
                    let _ = fanout.send(event);
                }
                None => return,
            },
        }
    }
}

/// Creates the state dir 0700 when absent (a fresh container volume);
/// pre-existing dirs (e.g. the desktop's config dir) keep their mode.
fn ensure_state_dir(state_dir: &Path) -> Result<(), String> {
    if state_dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("creating state dir {}: {e}", state_dir.display()))?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod 0700 {}: {e}", state_dir.display()))?;
    Ok(())
}

/// D1: the single-instance guard. flock on `daemon.lock`; a second daemon
/// (or a racing pair of daemons) fails fast here, so the stale-socket
/// unlink above can never fight a live sibling (no TOCTOU).
fn acquire_daemon_lock(state_dir: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = state_dir.join(DAEMON_LOCK);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(format!(
            "another remora-bridge serve is already running (holds {})",
            path.display()
        )),
        Err(e) => Err(format!("locking {}: {e}", path.display())),
    }
}

/// D1/G5: bind under umask 0o177 so the socket file is born 0600 — no
/// bind-then-chmod window. umask is process-global; this runs before any
/// concurrent file creation (single startup path), and is restored
/// immediately.
fn bind_private(socket_path: &Path) -> Result<UnixListener, String> {
    let old = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o177));
    let bound = UnixListener::bind(socket_path);
    rustix::process::umask(old);
    bound.map_err(|e| format!("binding ctl socket {}: {e}", socket_path.display()))
}

async fn wait_for_shutdown_signal() {
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("remora-bridge: cannot install SIGTERM handler: {e}");
            // Fall back to Ctrl-C only: in this mode a SIGTERM takes the
            // process default (immediate kill), forgoing the clean-shutdown
            // guarantee (socket cleanup, exit 0) for SIGTERM.
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
