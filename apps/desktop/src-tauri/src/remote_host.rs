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

use std::sync::Arc;

use rand::Rng as _;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    run_pairing, serve_bridge, wake_channel, BridgeConfig, BridgeEvent, BridgeIdentity,
    BridgeWakeHandle, PairingCommand, PairingFile, PairingProgress, RemoteSource, Roster,
};
use remora_core::config::Config;
use remora_core::resolve::ResolvingSource;
use remora_core::SessionSource;
use remora_relay::{serve, AuditSink, BridgeEntry, PushConfig, RelayConfig};

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
    /// Keeps the relay + bridge tasks alive for the loopback's life; dropping
    /// the host tears them down (see [`LoopbackTasks`]).
    _tasks: LoopbackTasks,
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
            _tasks: LoopbackTasks {
                shutdown: CancellationToken::new(),
                bridge_task: tokio::spawn(async {}),
                relay_accept: tokio::spawn(async {}),
            },
        }
    }
}

/// The loopback's spawned tasks plus their shutdown token, grouped so a single
/// owner both keeps them alive and tears them down. On success the group moves
/// into [`RemoteHost`]; until then it is a drop-guard inside [`start_loopback`],
/// so *every* early return after the spawns aborts the tasks instead of leaking
/// them for the process life (#297). The leak was load-bearing: the bridge task
/// owns the [`remora_bridge::IdentityLock`], so a leaked task kept the identity
/// flock and made the relay-bridge fallback in `lib.rs` fail with a misleading
/// "in use by another bridge process".
struct LoopbackTasks {
    shutdown: CancellationToken,
    bridge_task: JoinHandle<()>,
    relay_accept: JoinHandle<()>,
}

impl Drop for LoopbackTasks {
    fn drop(&mut self) {
        // Signal the bridge's reconnect loop to stop, then abort both tasks so
        // dropping the last owner (the RemoteHost of a shutting-down app, or the
        // guard on an early return out of `start_loopback`) leaves no
        // relay/bridge task spinning.
        self.shutdown.cancel();
        self.bridge_task.abort();
        self.relay_accept.abort();
    }
}

impl LoopbackTasks {
    /// Aborts both tasks and waits for them to actually finish. `abort()` alone
    /// only *signals*: the aborted future is dropped later, on a runtime worker.
    /// The bridge task holds the identity flock, which is released only when its
    /// future is dropped — so the pairing-failure path awaits here, guaranteeing
    /// the identity is claimable again the moment `start_loopback` returns `Err`
    /// and `lib.rs` falls back to the real relay bridge (#297).
    async fn abort_and_wait(mut self) {
        self.shutdown.cancel();
        self.bridge_task.abort();
        self.relay_accept.abort();
        // `JoinHandle` is `Unpin`, so await through `&mut`, leaving `self` for
        // its Drop (a no-op re-abort of finished tasks). A cancelled task
        // resolves `Err(Cancelled)`; either way its future is gone on return.
        let _ = (&mut self.bridge_task).await;
        let _ = (&mut self.relay_accept).await;
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
    // Claim exclusive use of this identity for the process lifetime before
    // touching it (spec D2, #234): the desktop's in-process bridge and a
    // future headless `remora-bridge serve` pointed at the same state dir
    // would otherwise silently share one identity file. Loopback and the real
    // relay bridge are mutually exclusive at runtime (see `lib.rs`), so this
    // only ever contends with a *different* process, never the relay bridge
    // in the same one.
    let identity_path = crate::bridge_state::identity_path(&config_path);
    let identity_lock = remora_bridge::IdentityLock::acquire(&identity_path)?;
    let identity = BridgeIdentity::load_or_create(&identity_path)?;
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
    let (addr, _relay_router, relay_accept) = serve(relay_cfg, audit).await?;
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
        // The desktop drops the receiver for now; #282 will consume it.
        health: tokio::sync::watch::channel(remora_bridge::BridgeHealth::Starting).0,
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
        // Held for exactly the bridge's lifetime: dropped when this task ends,
        // releasing the identity for a future claimant.
        let _identity_lock = identity_lock;
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

    // From here on the spawned tasks live inside the guard: any return path that
    // does not hand them to a `RemoteHost` aborts them instead of leaking them
    // (and, with the bridge task, the identity flock) for the process life (#297).
    let tasks = LoopbackTasks {
        shutdown,
        bridge_task,
        relay_accept,
    };

    // Run the real pairing ceremony against our own bridge (dev-only auto-confirm)
    // and take the resulting durable `PairingFile` for the client transport.
    let pairing = match drive_loopback_pairing(commands_tx, events_rx).await {
        Ok(pairing) => pairing,
        Err(e) => {
            // Tear the just-started relay + bridge down *to completion* before
            // surfacing the failure: the caller falls back to the real relay
            // bridge, which must be able to re-acquire the identity flock the
            // bridge task is still holding (#297).
            tasks.abort_and_wait().await;
            return Err(e);
        }
    };

    Ok(RemoteHost {
        remote: Arc::new(RemoteSource::new(pairing).with_push_endpoint(push_endpoint)),
        wake,
        _tasks: tasks,
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

    /// The crux of #297: `abort_and_wait` must not just *signal* the aborts but
    /// wait for the tasks' futures to be dropped. The bridge task owns the
    /// [`remora_bridge::IdentityLock`], and flock is per open-file-description,
    /// so a leaked (or merely signalled) task blocks a same-process re-acquire —
    /// exactly what the relay-bridge fallback in `lib.rs` does after a pairing
    /// failure.
    #[tokio::test]
    async fn abort_and_wait_releases_the_identity_flock() {
        // Unique pid-tagged dir so concurrent `cargo test` runs don't collide
        // (matches the temp-path convention elsewhere in the crate).
        let dir = std::env::temp_dir().join(format!("remora-loopback-297-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let identity_path = dir.join("bridge_identity.toml");
        let lock = remora_bridge::IdentityLock::acquire(&identity_path).expect("first acquire");

        // Mirror `start_loopback`: the lock lives inside the spawned bridge task
        // and is released only when that task's future is dropped. The task parks
        // on a channel nobody writes, like a reconnect loop that never exits.
        let (_park_tx, park_rx) = tokio::sync::oneshot::channel::<()>();
        let bridge_task = tokio::spawn(async move {
            let _identity_lock = lock;
            let _ = park_rx.await;
        });

        // Pre-fix symptom: while the bridge task lives, the identity is not
        // claimable — this is the misleading "in use by another bridge process".
        assert!(
            remora_bridge::IdentityLock::acquire(&identity_path).is_err(),
            "identity flock should be held while the bridge task lives"
        );

        let shutdown = CancellationToken::new();
        let tasks = LoopbackTasks {
            shutdown: shutdown.clone(),
            bridge_task,
            relay_accept: tokio::spawn(async {}),
        };
        tasks.abort_and_wait().await;

        // The aborts were *awaited*, so the flock must already be free here — no
        // polling or sleeping — and the cooperative shutdown was signalled too.
        assert!(shutdown.is_cancelled(), "shutdown token must be cancelled");
        drop(
            remora_bridge::IdentityLock::acquire(&identity_path)
                .expect("identity re-acquirable after teardown (#297)"),
        );
        std::fs::remove_dir_all(&dir).ok();
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
