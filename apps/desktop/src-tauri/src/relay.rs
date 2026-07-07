//! Real relay bridge hosting + live `[relay]` reconfig (ADR-0021 D7, #277).
//!
//! When the per-device config carries a `[relay]` section, the desktop hosts
//! *its own* bridge: it loads (or mints) this device's durable identity and its
//! paired-device roster from the app config dir — the same load-bearing layout
//! the dev loopback uses ([`crate::bridge_state`]) — and spawns [`serve_bridge`]
//! against the configured relay endpoint. Paired phones then reach this
//! device's sessions through the blind relay.
//!
//! The bridge is no longer launch-only: [`RelaySupervisor`] re-reads the
//! `[relay]` section whenever the config watcher (ADR-0014) fires and applies
//! the difference — section added → start, materially changed → clean restart,
//! removed → clean stop, otherwise → nothing (unrelated config edits never
//! churn live relay connections). "Clean stop" actually reaps: the shutdown
//! token is cancelled, the serve task and event-forwarder task are joined
//! (releasing the [`remora_bridge::IdentityLock`] held inside the serve task),
//! the wake tee is cleared, and the shared handles are removed so
//! Settings→Devices commands fail with "relay not configured" instead of
//! operating on a dead bridge's channels.
//!
//! Unlike the loopback ([`crate::remote_host`]), there is **no** dev-only
//! auto-confirm here: the pairing command/event channels are created and handed
//! back in [`PairingHandles`] for the pairing UI (#232) to drive on a real
//! human decision. This module only makes the bridge run and exposes the
//! handles; it issues no pairing commands itself.
//!
//! The served source is the desktop's own [`ResolvingSource`], so a session a
//! phone drives through this bridge serializes its mutating ops against the
//! *same* per-session exclusion registry as the direct UI path (ADR-0021 D7).

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    fingerprint, is_ws_url, serve_bridge, wake_channel, BridgeConfig, BridgeEvent, BridgeIdentity,
    BridgeWakeHandle, PairingCommand, Roster,
};
use remora_core::config::{Config, ConfigError, RelayConfigSection};
use remora_core::resolve::{ResolvingSource, SourceResolver};
use remora_core::SessionSource;
use remora_protocol::{ProjectId, SessionId, SessionStatus};

use crate::bridge::SessionWaker;

/// Channel depth for pairing commands/events between the UI and the bridge.
/// Pairing is a low-rate, human-driven ceremony; a small buffer is ample.
const PAIRING_CHANNEL_DEPTH: usize = 8;

/// How long a clean stop waits for a bridge task to honor its shutdown signal
/// before aborting it. `serve_bridge` selects on its token at every await, so
/// this is a defensive bound — a wedged task must not freeze config reloads.
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// A [`SessionWaker`] whose inner sink can be swapped at runtime (#277).
///
/// The [`crate::bridge::Bridge`]'s wake slot is set once, before it moves into
/// Tauri managed state, and can never be re-set — but the hosted bridge (and
/// with it the only real wake sink) now starts/stops/restarts on live config
/// edits. This indirection bridges the two lifetimes: the Bridge (and every
/// already-open output-pump channel, which clones the slot per channel) holds
/// this stable `Arc`, while the [`RelaySupervisor`] points its inner handle at
/// the *current* bridge — set on start, cleared on stop — so status tees never
/// land in a dead bridge's closed wake channel.
#[derive(Default)]
pub struct SwappableWaker {
    inner: std::sync::RwLock<Option<Arc<dyn SessionWaker>>>,
}

impl SwappableWaker {
    /// Route subsequent wake notes to `waker` (the freshly started bridge).
    fn set(&self, waker: Arc<dyn SessionWaker>) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = Some(waker);
    }

    /// Drop the inner sink: wake notes become no-ops (no bridge is running).
    fn clear(&self) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl SessionWaker for SwappableWaker {
    fn note_session_status(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        status: SessionStatus,
    ) {
        // Non-blocking by contract: the read lock is only ever contended by the
        // supervisor's brief set/clear writes, and the inner handle's note is a
        // try_send. A poisoned lock degrades to "no wake" rather than panicking
        // the output pump.
        if let Ok(guard) = self.inner.read() {
            if let Some(waker) = guard.as_ref() {
                waker.note_session_status(project_id, session_id, status);
            }
        }
    }
}

/// The desktop-side ends of a running relay bridge's pairing channels, shared
/// (via [`RelaySupervisor`]) with the pairing UI's commands (#232). Present
/// only while a bridge is actually running.
///
/// `commands` is cloned per outgoing [`PairingCommand`]; `events` is a single
/// consumer taken out exactly once by the event-forwarder task, so it sits
/// behind an `Option` guarded by a plain [`std::sync::Mutex`] (locked once,
/// never contended). `shutdown` plus the two task slots let a clean stop
/// (#277) cancel the bridge and *join* its tasks instead of leaking them to
/// process exit.
///
/// `roster` is the *same* `Arc<RwLock<Roster>>` the running bridge mutates on
/// every pairing/revocation, so `list_devices` reads the live set with no
/// staleness. `bridge_fingerprint` is this bridge's stable identity
/// fingerprint, captured once at start (the static key never changes for the
/// bridge's life).
pub struct PairingHandles {
    /// Sender for pairing/roster commands into the running bridge.
    pub commands: mpsc::Sender<PairingCommand>,
    /// Receiver for the bridge's pairing/roster events. Single consumer: taken
    /// out once by the event-forwarder at start ([`PairingHandles::take_events`]).
    pub events: std::sync::Mutex<Option<mpsc::Receiver<BridgeEvent>>>,
    /// The live paired-device roster, shared with the running bridge — reading it
    /// reflects pairings/revocations immediately (no on-disk re-read).
    pub roster: Arc<RwLock<Roster>>,
    /// This bridge's own identity fingerprint (ADR-0021 D5), for the pairing UI
    /// to display. Stable for the bridge's lifetime.
    pub bridge_fingerprint: String,
    /// Cheap, cloneable handle the session-output pump uses to wake paired
    /// devices when a session goes `Awaiting` (#233). Held here so its channel
    /// stays open for the bridge's life; the supervisor installs it in the
    /// app-wide [`SwappableWaker`] on start (and clears it on stop), so the
    /// session-output pump tees every status transition into this wake path.
    pub wake: BridgeWakeHandle,
    /// Cancels the bridge's serve loop on stop (live `[relay]` removal/change,
    /// #277) or app teardown.
    pub shutdown: CancellationToken,
    /// The spawned `serve_bridge` task, taken out (once) and joined by a clean
    /// stop so the task — and the identity lock its future owns — is actually
    /// reaped rather than left to process exit.
    pub task: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// The spawned bridge-event forwarder task
    /// ([`crate::bridge::pairing::spawn_event_forwarder`]), joined by a clean
    /// stop; it ends on its own once the serve task drops the event sender.
    pub forwarder: std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl PairingHandles {
    /// Takes the bridge-event receiver out for the forwarder task. Returns the
    /// receiver on the first call and `None` after (single consumer). A poisoned
    /// lock also yields `None` — the forwarder simply does not start, which is
    /// safe (the commands still work; only live event push is lost).
    pub fn take_events(&self) -> Option<mpsc::Receiver<BridgeEvent>> {
        self.events.lock().ok().and_then(|mut guard| guard.take())
    }

    /// Stores the spawned event-forwarder task so a clean stop can join it.
    pub fn set_forwarder(&self, task: tauri::async_runtime::JoinHandle<()>) {
        *self
            .forwarder
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(task);
    }
}

/// What a `[relay]` config change means for the hosted bridge (#277). Pure
/// decision, executed by [`RelaySupervisor::reconfigure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayTransition {
    /// No bridge is running and `[relay]` is present: start one.
    Start,
    /// A material field changed: cleanly stop the running bridge, then start
    /// with the new config.
    Restart,
    /// `[relay]` was removed: cleanly stop the running bridge.
    Stop,
    /// No material change: leave the running bridge (and its live relay
    /// connections) alone. Unrelated config edits land here.
    Nothing,
}

/// Diffs the `[relay]` section the running bridge was started with (`running`,
/// `None` when no bridge runs) against the section the config file now wants
/// (`desired`, `None` when absent).
fn plan_transition(
    running: Option<&RelayConfigSection>,
    desired: Option<&RelayConfigSection>,
) -> RelayTransition {
    match (running, desired) {
        (None, None) => RelayTransition::Nothing,
        (None, Some(_)) => RelayTransition::Start,
        (Some(_), None) => RelayTransition::Stop,
        (Some(running), Some(desired)) => {
            if material_change(running, desired) {
                RelayTransition::Restart
            } else {
                RelayTransition::Nothing
            }
        }
    }
}

/// True when a `[relay]` edit affects the *hosted bridge's* connection:
/// `relay_url` (where it dials) or `registration_token` (what admits it).
///
/// `push_wake_url` is deliberately **not** material: the hosted bridge never
/// reads it — it is the client half's wake registration (ADR-0023), consumed
/// by the loopback / a paired device — so restarting on it would churn live
/// relay connections for nothing.
fn material_change(running: &RelayConfigSection, desired: &RelayConfigSection) -> bool {
    running.relay_url != desired.relay_url
        || running.registration_token != desired.registration_token
}

/// Owns the hosted relay bridge's lifecycle (#277): starts it at launch when
/// `[relay]` is configured, and starts/restarts/stops it when the config
/// watcher reports a change. Lives in Tauri managed state (only when the dev
/// loopback is off — the loopback wins the one identity/roster, so live relay
/// reconfig is inert under `REMORA_REMOTE_LOOPBACK=1`); the pairing commands
/// read the current bridge's [`PairingHandles`] through it.
pub struct RelaySupervisor {
    /// Builds the served [`ResolvingSource`]; shared with the Bridge so a
    /// phone-driven session uses the same per-session exclusion registry.
    resolver: Arc<dyn SourceResolver>,
    /// The per-device config file the `[relay]` section is read from (and the
    /// anchor for the identity/roster state dir beside it).
    config_path: PathBuf,
    /// The Bridge's stable wake slot: pointed at the running bridge's
    /// [`BridgeWakeHandle`] on start, cleared on stop.
    wake: Arc<SwappableWaker>,
    /// Serializes transitions (the watcher can fire in bursts — each queued
    /// [`Self::reconfigure`] re-reads the config *inside* this lock, so the
    /// last write wins and intermediates collapse to `Nothing`) and owns the
    /// section the running bridge was started with: the diff baseline.
    running: tokio::sync::Mutex<Option<RelayConfigSection>>,
    /// The running bridge's handles, read by every pairing/devices command.
    /// Taken out *first* on stop so commands see "relay not configured"
    /// immediately rather than racing a dying bridge's channels.
    handles: std::sync::RwLock<Option<Arc<PairingHandles>>>,
}

impl RelaySupervisor {
    /// A supervisor with no bridge running. Call [`Self::reconfigure`] to apply
    /// the on-disk config (at launch and on every watcher ping).
    pub(crate) fn new(
        resolver: Arc<dyn SourceResolver>,
        config_path: PathBuf,
        wake: Arc<SwappableWaker>,
    ) -> Self {
        Self {
            resolver,
            config_path,
            wake,
            running: tokio::sync::Mutex::new(None),
            handles: std::sync::RwLock::new(None),
        }
    }

    /// The running bridge's handles, or `None` when no bridge is hosted.
    pub(crate) fn handles(&self) -> Option<Arc<PairingHandles>> {
        self.handles
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Re-reads the `[relay]` section and applies the difference against the
    /// running bridge: start / clean-restart / clean-stop / nothing. Serialized
    /// on [`Self::running`], so overlapping watcher pings queue rather than
    /// racing a half-finished transition. Non-fatal by contract: every failure
    /// logs and leaves the app on the direct path.
    pub(crate) async fn reconfigure(&self, app: &tauri::AppHandle) {
        let mut running = self.running.lock().await;
        let desired = match load_relay_section(&self.config_path) {
            Ok(desired) => desired,
            Err(e) => {
                // A transiently unreadable/invalid config (an editor's partial
                // save) must not take down a healthy bridge; keep current state
                // and wait for the next watcher ping.
                eprintln!("relay reconfig skipped: could not read config: {e}");
                return;
            }
        };
        match plan_transition(running.as_ref(), desired.as_ref()) {
            RelayTransition::Nothing => {}
            RelayTransition::Stop => {
                self.stop_running().await;
                *running = None;
                eprintln!("relay bridge stopped: [relay] was removed from the config");
            }
            RelayTransition::Start | RelayTransition::Restart => {
                // For Start the stop is a no-op (nothing is running); for
                // Restart it guarantees the old serve task has released the
                // identity lock before the new bridge claims it.
                self.stop_running().await;
                *running = match desired {
                    Some(section) => self.start(app, section),
                    // Unreachable by plan_transition's contract (Start/Restart
                    // imply a desired section); treated as "not running".
                    None => None,
                };
            }
        }
    }

    /// Starts the bridge for `section`, wiring the event forwarder and wake
    /// tee, and publishes its handles for the pairing commands. Returns the
    /// section on success (the new diff baseline) or `None` when the start
    /// failed (already logged by [`start_bridge`]).
    fn start(
        &self,
        app: &tauri::AppHandle,
        section: RelayConfigSection,
    ) -> Option<RelayConfigSection> {
        let handles = start_bridge(
            Arc::clone(&self.resolver),
            self.config_path.clone(),
            section.clone(),
        )?;
        let handles = Arc::new(handles);
        // Forward the bridge's pairing/roster events to the frontend for this
        // bridge's lifetime (the task ends when the serve task drops the
        // event sender; a clean stop joins it).
        crate::bridge::pairing::spawn_event_forwarder(app.clone(), &handles);
        // Point the output pump's stable wake slot at the new bridge (#233).
        self.wake.set(Arc::new(handles.wake.clone()));
        *self.handles.write().unwrap_or_else(PoisonError::into_inner) = Some(handles);
        Some(section)
    }

    /// Cleanly stops the running bridge, if any: unpublish the handles (so
    /// commands fail with "relay not configured" instead of talking to a dead
    /// bridge), clear the wake tee, cancel the serve loop, and **join** the
    /// serve + forwarder tasks — reaping them (and releasing the identity
    /// lock the serve task's future owns) instead of leaking them to process
    /// exit. A no-op when no bridge is running.
    async fn stop_running(&self) {
        let handles = self
            .handles
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(handles) = handles else {
            return;
        };
        self.wake.clear();
        handles.shutdown.cancel();
        let task = handles
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        join_stopped_task(task, "serve").await;
        let forwarder = handles
            .forwarder
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        join_stopped_task(forwarder, "event-forwarder").await;
    }
}

/// Joins a stopping bridge task, bounded by [`STOP_JOIN_TIMEOUT`] so a wedged
/// task cannot freeze config reloads forever. On timeout the task is aborted —
/// dropping its future, which also releases anything it owns (notably the
/// serve task's identity flock) — and the abort is awaited (bounded again) so
/// the drop has actually happened before a successor starts.
async fn join_stopped_task(task: Option<tauri::async_runtime::JoinHandle<()>>, what: &str) {
    let Some(mut task) = task else {
        return;
    };
    if tokio::time::timeout(STOP_JOIN_TIMEOUT, &mut task)
        .await
        .is_ok()
    {
        return;
    }
    eprintln!(
        "relay bridge {what} task ignored shutdown for {}s; aborting it",
        STOP_JOIN_TIMEOUT.as_secs()
    );
    task.abort();
    let _ = tokio::time::timeout(STOP_JOIN_TIMEOUT, &mut task).await;
}

/// Hosts this device's bridge for `relay`, returning the pairing-channel
/// handles for the UI to drive (#232). Shared by the launch-time start and
/// every live restart (#277) — both go through [`RelaySupervisor`].
///
/// Non-fatal by contract: an identity/roster storage failure or an unusable
/// relay URL logs and yields `None` (the app keeps running on the direct path)
/// rather than bricking launch — mirroring the loopback's fallback.
fn start_bridge(
    resolver: Arc<dyn SourceResolver>,
    config_path: PathBuf,
    relay: RelayConfigSection,
) -> Option<PairingHandles> {
    // `serve_bridge` rejects a non-`ws`/`wss` relay URL, but only inside the
    // spawned task — after we would have returned `Some(PairingHandles)` and the
    // UI already believes the bridge is running. Reject the unusable URL up front
    // instead, so a misconfigured relay lands in the same "not started" bucket as
    // an identity/roster failure rather than a doomed task the UI can't cancel.
    if !is_ws_url(&relay.relay_url) {
        eprintln!(
            "relay bridge not started: relay_url must be a ws:// or wss:// endpoint, got {:?}",
            relay.relay_url
        );
        return None;
    }

    // Claim exclusive use of this identity for the bridge's lifetime before
    // touching it (spec D2, #234): the desktop's in-process bridge and a
    // headless `remora-bridge serve` pointed at the same state dir would
    // otherwise silently share one identity file. Non-fatal by the same
    // contract as the checks above: another bridge already holding it just
    // means this device doesn't also start one.
    let identity_path = crate::bridge_state::identity_path(&config_path);
    let identity_lock = match remora_bridge::IdentityLock::acquire(&identity_path) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("relay bridge not started: {e}");
            return None;
        }
    };

    // Durable identity (stable across runs) + the paired-device roster (persists
    // real pairings), both from the shared bridge-state layout.
    let identity = match BridgeIdentity::load_or_create(&identity_path) {
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
    let source: Arc<dyn SessionSource> = Arc::new(ResolvingSource::new(resolver, config_path));

    // This bridge's own fingerprint (ADR-0021 D5) for the pairing UI. Captured
    // now, before `identity` moves into the config; the static key is stable for
    // the bridge's life, so no re-read is ever needed.
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
        // The desktop drops the receiver for now; #282 will consume it.
        health: tokio::sync::watch::channel(remora_bridge::BridgeHealth::Starting).0,
    };

    let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(PAIRING_CHANNEL_DEPTH);
    let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(PAIRING_CHANNEL_DEPTH);
    let (wake, wake_rx) = wake_channel();
    let shutdown_task = shutdown.clone();
    let task = tauri::async_runtime::spawn(async move {
        // Held for exactly the bridge's lifetime: dropped when this task ends
        // (a clean stop joins it, #277), releasing the identity for the next
        // claimant — including this supervisor's own restart.
        let _identity_lock = identity_lock;
        // `serve_bridge` only returns `Err` on an unusable configuration (e.g. a
        // non-`ws` relay URL); transient relay/network failures are retried
        // internally. Log a fatal stop rather than panic.
        if let Err(e) = serve_bridge(
            bridge_cfg,
            source,
            commands_rx,
            events_tx,
            wake_rx,
            shutdown_task,
        )
        .await
        {
            eprintln!("relay bridge stopped: {e}");
        }
    });

    Some(PairingHandles {
        commands: commands_tx,
        events: std::sync::Mutex::new(Some(events_rx)),
        roster,
        bridge_fingerprint,
        wake,
        shutdown,
        task: std::sync::Mutex::new(Some(task)),
        forwarder: std::sync::Mutex::new(None),
    })
}

/// Loads the config's `[relay]` section. A *missing* file is success with no
/// section (a fresh device is valid, ADR-0004; a deleted config means the
/// section is gone); any other load failure is surfaced so the caller can keep
/// the current bridge rather than reacting to a half-written file.
fn load_relay_section(
    config_path: &std::path::Path,
) -> Result<Option<RelayConfigSection>, ConfigError> {
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

    // ---- plan_transition: the Start/Restart/Stop/Nothing decision (#277) ----

    fn section(url: &str, token: &str, push: Option<&str>) -> RelayConfigSection {
        RelayConfigSection {
            relay_url: url.to_string(),
            registration_token: token.to_string(),
            push_wake_url: push.map(str::to_string),
        }
    }

    #[test]
    fn no_relay_before_or_after_is_nothing() {
        assert_eq!(plan_transition(None, None), RelayTransition::Nothing);
    }

    #[test]
    fn relay_added_starts() {
        let desired = section("wss://relay.example/ws", "tok", None);
        assert_eq!(
            plan_transition(None, Some(&desired)),
            RelayTransition::Start
        );
    }

    #[test]
    fn relay_removed_stops() {
        let running = section("wss://relay.example/ws", "tok", None);
        assert_eq!(plan_transition(Some(&running), None), RelayTransition::Stop);
    }

    #[test]
    fn unchanged_relay_is_nothing() {
        let running = section(
            "wss://relay.example/ws",
            "tok",
            Some("https://push.example/t"),
        );
        let desired = running.clone();
        assert_eq!(
            plan_transition(Some(&running), Some(&desired)),
            RelayTransition::Nothing
        );
    }

    #[test]
    fn relay_url_change_restarts() {
        let running = section("wss://relay.example/ws", "tok", None);
        let desired = section("wss://other.example/ws", "tok", None);
        assert_eq!(
            plan_transition(Some(&running), Some(&desired)),
            RelayTransition::Restart
        );
    }

    #[test]
    fn registration_token_change_restarts() {
        let running = section("wss://relay.example/ws", "tok", None);
        let desired = section("wss://relay.example/ws", "tok2", None);
        assert_eq!(
            plan_transition(Some(&running), Some(&desired)),
            RelayTransition::Restart
        );
    }

    #[test]
    fn push_wake_url_only_change_is_not_material() {
        // The hosted bridge never reads push_wake_url (it is the client half's
        // wake registration) — restarting on it would churn live connections.
        let running = section("wss://relay.example/ws", "tok", None);
        let desired = section(
            "wss://relay.example/ws",
            "tok",
            Some("https://push.example/t"),
        );
        assert_eq!(
            plan_transition(Some(&running), Some(&desired)),
            RelayTransition::Nothing
        );
    }

    // ---- SwappableWaker: the pump's stable slot routes to the current bridge ----

    #[derive(Default)]
    struct CountingWaker {
        notes: std::sync::Mutex<Vec<SessionStatus>>,
    }
    impl SessionWaker for CountingWaker {
        fn note_session_status(
            &self,
            _project_id: &ProjectId,
            _session_id: &SessionId,
            status: SessionStatus,
        ) {
            self.notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(status);
        }
    }

    #[test]
    fn swappable_waker_routes_set_and_silences_clear() {
        let slot = SwappableWaker::default();
        let project = ProjectId::new("api").expect("id");
        let session = SessionId::new("s").expect("id");

        // Empty slot (no bridge running): a note is a silent no-op.
        slot.note_session_status(&project, &session, SessionStatus::Awaiting);

        let spy = Arc::new(CountingWaker::default());
        slot.set(spy.clone());
        slot.note_session_status(&project, &session, SessionStatus::Awaiting);
        slot.note_session_status(&project, &session, SessionStatus::Working);

        slot.clear();
        slot.note_session_status(&project, &session, SessionStatus::Awaiting);

        let notes = spy.notes.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            *notes,
            vec![SessionStatus::Awaiting, SessionStatus::Working],
            "only notes made while set() was active reach the inner waker"
        );
    }

    #[test]
    fn swappable_waker_replaces_the_inner_sink_on_restart() {
        let slot = SwappableWaker::default();
        let project = ProjectId::new("api").expect("id");
        let session = SessionId::new("s").expect("id");

        let first = Arc::new(CountingWaker::default());
        let second = Arc::new(CountingWaker::default());
        slot.set(first.clone());
        slot.note_session_status(&project, &session, SessionStatus::Awaiting);
        slot.set(second.clone());
        slot.note_session_status(&project, &session, SessionStatus::Working);

        assert_eq!(
            first
                .notes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            1
        );
        assert_eq!(
            *second.notes.lock().unwrap_or_else(PoisonError::into_inner),
            vec![SessionStatus::Working]
        );
    }
}
