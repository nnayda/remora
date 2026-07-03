//! The bridge server half of relay mode (ADR-0021): [`serve_bridge`].
//!
//! [`serve_bridge`] dials the relay *outbound* as a WebSocket client, announces
//! itself with a `role=bridge` [`RelayHello`], and then serves end-to-end Noise
//! sessions with paired devices until `shutdown` fires — reconnecting with
//! capped exponential backoff + jitter whenever the relay connection drops.
//!
//! # Concurrency shape
//!
//! One `serve_bridge` call owns a reconnect loop. Each successful connection
//! runs a [`run_connection`] scope with:
//!
//! - a **writer task** that owns the WebSocket sink and drains a single
//!   outbound [`Message`] queue (so every frame the bridge sends is serialized
//!   through one place, preserving order);
//! - a **read loop** that decodes inbound envelopes and routes each by its
//!   `src` routing id to a **per-peer task**;
//! - **one task per client peer**, keyed by the envelope `src`. That task *owns*
//!   its Noise [`Transport`] outright — every `open` (inbound) and `seal`
//!   (outbound) for that peer happens in that one task, so the Noise nonce
//!   sequence is single-threaded with no shared lock. A peer's attached PTY
//!   stream is drained by a small **pump task** that forwards *plaintext*
//!   [`ChannelOutput`] back into the peer task over an mpsc; the peer task does
//!   all the sealing, keeping seal-order == send-order.
//!
//! A peer's Noise/protocol failure drops only that peer's task and state; the
//! relay connection dropping cancels a per-connection [`CancellationToken`],
//! which tears down every peer (clients then observe channel death). Nothing
//! holds a lock across an `.await`; the only per-peer unboundedness is the
//! bounded mpsc queues.
//!
//! # Bounding the peer map (#231)
//!
//! Under ADR-0021 the relay is untrusted for routing: it can inject Data frames
//! with arbitrarily many distinct `src` ids, each spawning a peer task that may
//! exit immediately (roster miss). To keep the connection-level peer map from
//! growing without bound, every spawned peer task carries a [`DoneGuard`] that
//! signals its `src` + generation back to the connection loop on *every* exit
//! path (roster miss, handshake fail, hello mismatch, decrypt error, channel
//! death, cancellation). The loop reaps that slot from the [`PeerRegistry`] —
//! but only if the stored generation still matches, so a client reconnecting on
//! the same routing id (a newer task) is never evicted by an older task's stale
//! done-signal.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures_util::stream::Stream;
use futures_util::{SinkExt as _, StreamExt as _};
use rand::TryRng as _;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use remora_core::{sanitize, SessionSource};
use remora_protocol::{
    validate_push_endpoint, AssertedDevice, BridgeMessage, ChannelInput, ChannelOutput,
    ClientMessage, DeviceId, DeviceInfo, Envelope, FrameType, HelloRole, PairingBridgeMsg,
    PairingClientMsg, PairingCode, PairingRejectReason, ProjectId, PushRegistration, RelayControl,
    RelayControlAck, RelayControlError, RelayHello, RemoteOp, RemoteResult, SessionId,
    SessionStatus, WireError, PROTOCOL_VERSION,
};

use crate::identity::{fingerprint, BridgeIdentity, IdentityError, Roster, RosterEntry};
use crate::noise::{chunk_bytes, prologue, Handshake, HandshakeKind, Transport};
use crate::wake::{wake_targets, EpisodeKey, WakeEpisodes};
use crate::wire_error::map_source_error;

/// Standard-base64 engine for the per-pair session PSK the bridge grants a
/// device ([`PairingBridgeMsg::Grant`]) — matching the encoding the identity
/// layer uses on disk, so the device decodes it the same way.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Upper bound on a paired device's display name, in characters. The name rides
/// in untrusted [`PairingClientMsg::Hello`] and is rendered in the confirm
/// dialog, so it is control-stripped and capped ([`sanitize`]) before use.
const MAX_DEVICE_NAME_CHARS: usize = 64;

/// Pending relay control requests awaiting their `RelayControlAck`/`Error`, keyed
/// by correlation id. Shared (`Arc<Mutex>`) between the read loop (which completes
/// waiters) and a spawned pairing task (which registers one for its assert). Each
/// value is completed with `Ok(())` on ack or `Err(message)` on a relay error.
type ControlWaiters = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<(), String>>>>>;

/// Live authenticated peer sessions, keyed by the peer's roster-proven
/// `device_id`, so revoking a device can sever every live session that device
/// currently holds bridge-side — the authoritative kick (ADR-0021 D6: the roster
/// is the boundary; the relay re-assert is defense-in-depth). One device may
/// hold several sessions at once (D16: each attach is its own connection), so
/// each device maps to a set of `(registration id → cancellation token)`; the
/// registration id lets a peer task deregister exactly its own slot on exit
/// without disturbing a sibling session that reused the same `device_id`.
type LivePeers = Arc<Mutex<HashMap<DeviceId, HashMap<u64, CancellationToken>>>>;

/// Length of a [`DeviceId`], and of the plaintext identity preamble that
/// prefixes a client's first (handshake) frame (spec D16).
const DEVICE_ID_LEN: usize = 32;

/// Lower bound of the reconnect backoff.
const BACKOFF_MIN: Duration = Duration::from_millis(200);

/// Upper bound (cap) of the reconnect backoff.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Depth of a connection's shared outbound frame queue.
const OUTBOUND_QUEUE: usize = 256;

/// Depth of a peer's inbound ciphertext-frame queue.
const PEER_FRAME_QUEUE: usize = 256;

/// Depth of a peer's internal PTY-event queue (pump task → peer task).
const PEER_EVENT_QUEUE: usize = 256;

/// Depth of the wake-note queue (host session pump → bridge task, #233). Small
/// and bounded: the wake path is best-effort, so a burst that overflows this is
/// dropped rather than allowed to back-pressure the session path.
const WAKE_QUEUE: usize = 64;

/// Number of concurrently-open `Awaiting` episodes the bridge remembers for
/// wake de-duplication (#233). Comfortably exceeds any realistic count of
/// simultaneously-attended sessions; overflow simply forgets the oldest, whose
/// next `Awaiting` re-wakes.
const WAKE_EPISODE_CAP: usize = 256;

/// Oldest age a queued `Awaiting` [`WakeNote`] may have and still be allowed to
/// open an episode / fire a `PushTrigger` (#233 stale-wake fix).
///
/// The wake channel's [`WakeReceiver`] survives relay reconnects, and its
/// sender (`try_send`) never blocks — so while the relay is down, notes pile
/// up in the bounded queue (capacity [`WAKE_QUEUE`]) instead of being dropped.
/// On reconnect they replay in FIFO order. Without this guard, an `Awaiting`
/// note stamped minutes ago (before the outage) would still open a fresh
/// episode and push a device *after* the session had already been resolved
/// (attended, closed, or replaced) during the outage. Discarding a stale
/// `Awaiting` note is safe: it opens no episode, so a later repaint/re-attach
/// re-emission will note the session again if it is genuinely still awaiting.
/// Non-`Awaiting` notes are never discarded for staleness — closing an episode
/// is always safe and keeps de-dup state honest regardless of age.
const STALE_WAKE_MAX: Duration = Duration::from_secs(60);

/// A session status change routed from the host's channel-output pump (#233)
/// into the bridge task, where it is de-duplicated into wake episodes and, on
/// an opening `Awaiting` edge, fanned out as `PushTrigger` frames.
struct WakeNote {
    /// The session whose status changed.
    key: EpisodeKey,
    /// Its new status.
    status: SessionStatus,
    /// When this note was queued ([`BridgeWakeHandle::note_session_status`]
    /// send time), used to discard stale replayed `Awaiting` notes after a
    /// relay reconnect (see [`STALE_WAKE_MAX`]).
    at: Instant,
}

/// The receiver half of the wake channel, handed to [`serve_bridge`]. Opaque so
/// the internal [`WakeNote`] shape stays private to this crate's bridge task.
pub struct WakeReceiver(mpsc::Receiver<WakeNote>);

/// A cheap, cloneable handle the host uses to tell the bridge a session changed
/// status (#233), driving the wake path without ever blocking or erroring the
/// session it observes.
///
/// Obtain one with [`wake_channel`]; hand the paired [`WakeReceiver`] to
/// [`serve_bridge`]. [`note_session_status`](Self::note_session_status) is
/// non-blocking and callable from sync contexts, so the channel-output pump can
/// call it inline.
#[derive(Clone)]
pub struct BridgeWakeHandle {
    tx: mpsc::Sender<WakeNote>,
}

impl BridgeWakeHandle {
    /// Records that `(project_id, session_id)` moved to `status`.
    ///
    /// Non-blocking and infallible to the caller: it `try_send`s onto a bounded
    /// queue and drops the note if the queue is full or the bridge task has
    /// stopped. The wake path is best-effort by design (ADR-0023) — a dropped
    /// note at worst misses one push, and must never block or fail the session
    /// path that produced it.
    pub fn note_session_status(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        status: SessionStatus,
    ) {
        let note = WakeNote {
            key: (project_id.clone(), session_id.clone()),
            status,
            at: Instant::now(),
        };
        // Drop on a full queue (64 unhandled notes) or a stopped bridge. There
        // is no logging framework in this crate; the drop is intentional and
        // silent, matching the crate's fire-and-forget convention (a full queue
        // means the bridge is far behind on wakes, and the next status change
        // supersedes this one anyway).
        let _ = self.tx.try_send(note);
    }
}

/// Creates the wake channel: the [`BridgeWakeHandle`] the host keeps and calls,
/// and the [`WakeReceiver`] it hands to [`serve_bridge`].
pub fn wake_channel() -> (BridgeWakeHandle, WakeReceiver) {
    let (tx, rx) = mpsc::channel::<WakeNote>(WAKE_QUEUE);
    (BridgeWakeHandle { tx }, WakeReceiver(rx))
}

/// How long a peer may take to send its E2E [`ClientMessage::Hello`] after the
/// Noise handshake completes. A paired client that finishes the handshake then
/// stalls would otherwise pin its peer task and [`PeerRegistry`] slot forever;
/// on expiry the peer task returns and its [`DoneGuard`] reaps the slot (#231).
const PEER_HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the bridge waits for the relay's `RelayControlAck` to its
/// connect-time `AssertDevices` before giving up on the connection. The relay is
/// untrusted (ADR-0021): one that accepts the socket but never acks must not
/// wedge the bridge in the pre-serve phase forever — on expiry the attempt is
/// treated like a failed connect (reconnect with growing backoff).
const CONTROL_ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Static configuration for one [`serve_bridge`] run.
///
/// Not `Debug`/`Clone`: [`BridgeIdentity`] holds a private key.
pub struct BridgeConfig {
    /// The relay endpoint to dial (`ws://…` or `wss://…`).
    pub relay_url: String,
    /// The relay-issued bridge registration token (proves admission).
    pub registration_token: String,
    /// This bridge's durable identity (device id + static keypair).
    pub identity: BridgeIdentity,
    /// The paired-device roster (pinned static keys + per-pair PSKs), shared so
    /// pairing/revocation can mutate it live and every relay (re)connect asserts
    /// the current set (ADR-0021 D4).
    pub roster: Arc<RwLock<Roster>>,
    /// Where the roster persists; every roster mutation is written back here so
    /// a restart re-asserts the same devices.
    pub roster_path: PathBuf,
}

/// Fatal, non-retryable error from [`serve_bridge`].
///
/// Transient failures (relay down, connection dropped) are never surfaced here
/// — they are retried with backoff. Only an unusable configuration stops the
/// loop, so that an operator's typo fails fast instead of spinning forever.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BridgeServeError {
    /// The relay URL did not begin with `ws://` or `wss://`.
    #[error("relay url must begin with ws:// or wss://, got `{0}`")]
    InvalidRelayUrl(String),
}

/// A per-command failure surfaced back to the desktop over a [`PairingCommand`]
/// reply channel — distinct from the fatal, loop-stopping [`BridgeServeError`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BridgeError {
    /// Persisting or reading roster/identity state on disk failed.
    #[error("identity/roster storage error: {0}")]
    Storage(#[from] IdentityError),
    /// No relay connection was live to carry the command (e.g. opening a pairing
    /// window while the bridge is between reconnects).
    #[error("no relay connection is currently established")]
    Disconnected,
    /// The bridge could not build a pairing code from its current state (e.g. a
    /// non-`ws` relay URL that cannot be handed to a device).
    #[error("cannot mint a pairing code: {0}")]
    Pairing(String),
}

/// A control message the desktop sends into a running [`serve_bridge`] to drive
/// the pairing ceremony and roster changes (ADR-0021 D3). `OpenWindow` mints the
/// code and registers the relay window; `Confirm`/`Reject` route the user's
/// decision into the in-flight pairing responder; `Revoke`/`CancelWindow`
/// administer the roster and window.
#[derive(Debug)]
#[non_exhaustive]
pub enum PairingCommand {
    /// Open (or replace) this bridge's single pairing window for `ttl_secs`,
    /// replying with the freshly minted [`PairingCode`] the desktop renders as a
    /// QR / copyable string.
    OpenWindow {
        ttl_secs: u64,
        reply: oneshot::Sender<Result<PairingCode, BridgeError>>,
    },
    /// The user confirmed the arrived device's fingerprint: the responder mints
    /// durable credentials, asserts-before-grant, and enrols the device.
    Confirm { device_id: DeviceId },
    /// The user rejected the arrived device: the responder sends `Rejected` and
    /// grants nothing durable.
    Reject { device_id: DeviceId },
    /// Un-pair a device: drop it from the roster, persist, and re-assert the
    /// shrunken set so the relay kicks any live connection (ADR-0021 D6).
    Revoke {
        device_id: DeviceId,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    /// Close the current pairing window without pairing anyone.
    CancelWindow,
}

/// An event the bridge emits to the desktop over the [`serve_bridge`] event
/// channel, mirroring pairing progress and roster changes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BridgeEvent {
    /// A pairing window opened; the desktop shows `code` until `expires_at`
    /// (Unix seconds).
    PairingWindowOpened { code: PairingCode, expires_at: u64 },
    /// A device reached the open pairing window and awaits confirmation.
    PairingDeviceArrived {
        device_id: DeviceId,
        name: String,
        fingerprint: String,
    },
    /// A pairing attempt reached a terminal state.
    PairingResult(PairingOutcome),
    /// The roster changed (a device was enrolled or revoked); the desktop may
    /// refresh its paired-devices view.
    RosterChanged,
}

/// The terminal result of one pairing attempt (ADR-0021).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairingOutcome {
    /// The device was enrolled into the roster.
    Paired { device_id: DeviceId, name: String },
    /// The user rejected the device.
    Rejected { device_id: DeviceId },
    /// The pairing window expired or was cancelled before completion.
    Expired,
}

/// Builds the [`RelayControl::AssertDevices`] message for the current roster
/// (ADR-0021 D4): one [`AssertedDevice`] per entry, carrying the entry's stored
/// per-device relay credential.
fn assert_devices_msg(id: u32, roster: &Roster) -> RelayControl {
    RelayControl::AssertDevices {
        id,
        devices: roster
            .entries
            .iter()
            .map(|e| AssertedDevice {
                device_id: e.device_id,
                token: device_token_for(e),
                push: e.push.clone(),
            })
            .collect(),
    }
}

/// The per-device relay credential the bridge asserts — the `relay_token` minted
/// at pairing (Task 6) and mirrored into the device's `PairingFile.device_token`.
fn device_token_for(entry: &RosterEntry) -> String {
    entry.relay_token.clone()
}

/// Allocates the next control-request correlation id (wraps after `u32::MAX`,
/// which no bridge reaches in one process lifetime).
fn next_control_id(seq: &AtomicU32) -> u32 {
    seq.fetch_add(1, Ordering::Relaxed)
}

/// Shared, cheaply-clonable material every peer task needs.
struct PeerDeps {
    /// The bridge's routing id (also its `src` on every outbound frame).
    bridge_id: DeviceId,
    /// The bridge's static private key, presented as the Noise responder.
    bridge_static_priv: Vec<u8>,
    /// The bridge's static public key — handed to a device in a minted
    /// [`PairingCode`] so it can pin the bridge.
    bridge_static_pub: Vec<u8>,
    /// The relay endpoint devices dial, embedded into minted pairing codes.
    relay_url: String,
    /// The paired-device roster, shared so pairing/revocation mutate it live and
    /// every (re)connect asserts the current set. Read under the lock per-peer.
    roster: Arc<RwLock<Roster>>,
    /// Where the roster persists; written back on every mutation.
    roster_path: PathBuf,
    /// The local session source the bridge serves.
    source: Arc<dyn SessionSource>,
    /// Live authenticated peer sessions keyed by `device_id`, so a revocation
    /// (from a wire `RevokeDevice` or a `PairingCommand::Revoke`) can cancel
    /// every live session that device holds. Shared across every connection and
    /// peer task (ADR-0021 D6 bridge-side kick).
    live_peers: LivePeers,
    /// Monotonic source of per-session registration ids for [`LivePeers`], unique
    /// for this bridge process so two sessions (even on different connections)
    /// never collide on a slot key.
    next_peer_reg: AtomicU64,
    /// Wake-episode tracker (#233), shared so the connection loop can note
    /// status changes (opening `Awaiting` episodes → fan out `PushTrigger`s) and
    /// a peer task can [`forget`](WakeEpisodes::forget) a session's episode when
    /// its attached channel is torn down. Behind a plain [`Mutex`] because
    /// [`WakeEpisodes`] is pure/sync; locks are brief and never held across an
    /// `.await`.
    episodes: Arc<Mutex<WakeEpisodes>>,
}

/// Deregisters a peer's [`LivePeers`] slot when the peer task returns, for any
/// reason. Mirrors [`DoneGuard`]/[`CancelGuard`]: registration is RAII so a
/// revocation kick never targets a session that has already ended.
struct LivePeerGuard {
    live_peers: LivePeers,
    device_id: DeviceId,
    reg_id: u64,
}

impl Drop for LivePeerGuard {
    fn drop(&mut self) {
        // A poisoned lock (a holder panicked) leaves the map unmutated; a stale
        // token there can only over-cancel an already-returning task, never a
        // wrong device, so it is harmless.
        if let Ok(mut map) = self.live_peers.lock() {
            if let Some(slots) = map.get_mut(&self.device_id) {
                slots.remove(&self.reg_id);
                if slots.is_empty() {
                    map.remove(&self.device_id);
                }
            }
        }
    }
}

/// Registers `token` as `device_id`'s live session under `reg_id`, returning an
/// RAII guard that deregisters it on drop. `None` only if the shared lock is
/// poisoned — the caller then serves without kick-registration (the relay
/// re-assert still revokes; the roster is the boundary).
fn register_live_peer(
    live_peers: &LivePeers,
    device_id: DeviceId,
    reg_id: u64,
    token: CancellationToken,
) -> Option<LivePeerGuard> {
    let mut map = live_peers.lock().ok()?;
    map.entry(device_id).or_default().insert(reg_id, token);
    drop(map);
    Some(LivePeerGuard {
        live_peers: live_peers.clone(),
        device_id,
        reg_id,
    })
}

/// Cancels every live session held by `device_id` (the bridge-side revocation
/// kick). A no-op when the device has no live session.
fn cancel_live_peers(device_id: &DeviceId, live_peers: &LivePeers) {
    let Ok(map) = live_peers.lock() else {
        return;
    };
    if let Some(slots) = map.get(device_id) {
        for token in slots.values() {
            token.cancel();
        }
    }
}

/// Why one connection attempt ended — drives the backoff decision.
enum ConnOutcome {
    /// `shutdown` fired: stop the whole loop.
    Shutdown,
    /// The connection was established then lost: reset backoff.
    Disconnected,
    /// The dial or relay-hello never succeeded: keep growing backoff.
    ConnectFailed,
}

/// A plaintext event from a peer's PTY pump task into its peer task.
enum PeerEvent {
    /// A [`ChannelOutput`] from the attached session, to be sealed + forwarded.
    Output(ChannelOutput),
    /// The attached channel died (its transport ends dropped).
    Dead,
}

/// Cancels a [`CancellationToken`] when dropped — used to stop a peer's pump
/// task the moment the peer task returns for any reason.
struct CancelGuard(CancellationToken);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One live peer's slot in the [`PeerRegistry`]: its inbound frame sender plus
/// the generation stamped when it was inserted.
struct PeerSlot {
    /// Inbound ciphertext-frame sender into the peer task.
    tx: mpsc::Sender<Vec<u8>>,
    /// Monotonic generation identifying *this* task's ownership of the slot, so
    /// a stale done-signal cannot evict a newer task that reused the same `src`.
    generation: u64,
}

/// The connection loop's map of live peers, keyed by routing `src`.
///
/// Bounded by proactive reaping: [`run_peer`] signals completion on every exit
/// path and the loop calls [`PeerRegistry::remove_if_generation`], so a relay
/// injecting many distinct `src` ids cannot grow this map without bound (#231).
struct PeerRegistry {
    peers: HashMap<DeviceId, PeerSlot>,
    /// Source of the next slot generation. Monotonic for the connection's life.
    next_generation: u64,
}

impl PeerRegistry {
    fn new() -> Self {
        Self {
            peers: HashMap::new(),
            next_generation: 0,
        }
    }

    /// Inserts (or replaces) the peer at `src`, returning the generation stamped
    /// into the new slot. The caller hands that generation to the spawned peer
    /// task, which echoes it back on exit for a generation-guarded reap.
    fn insert(&mut self, src: DeviceId, tx: mpsc::Sender<Vec<u8>>) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        self.peers.insert(src, PeerSlot { tx, generation });
        generation
    }

    /// The inbound frame sender for `src`, if a peer is live there.
    fn get(&self, src: &DeviceId) -> Option<&mpsc::Sender<Vec<u8>>> {
        self.peers.get(src).map(|slot| &slot.tx)
    }

    /// Unconditionally drops the slot at `src` (used when its sender is already
    /// observed `Closed`, so no newer task can be evicted).
    fn remove(&mut self, src: &DeviceId) {
        self.peers.remove(src);
    }

    /// Reaps the slot at `src` only if its stored generation equals
    /// `generation`. A displaced (reconnected) peer holds a newer generation, so
    /// an older task's late done-signal is a no-op and cannot evict the live one.
    fn remove_if_generation(&mut self, src: &DeviceId, generation: u64) {
        if let std::collections::hash_map::Entry::Occupied(slot) = self.peers.entry(*src) {
            if slot.get().generation == generation {
                slot.remove();
            }
        }
    }

    /// Number of live peers — for tests and invariant checks.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.peers.len()
    }
}

/// Signals a peer task's completion (its `src` + generation) to the connection
/// loop when dropped — firing on *every* peer exit path, including early
/// returns and cancellation. The loop reaps the matching registry slot, keeping
/// the peer map bounded against relay-driven `src` churn (#231).
struct DoneGuard {
    done: mpsc::UnboundedSender<(DeviceId, u64)>,
    src: DeviceId,
    generation: u64,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        // Unbounded: a non-blocking sync send that only fails when the loop has
        // already dropped the receiver (connection tearing down), where the
        // whole map is discarded anyway — so a lost signal is harmless.
        let _ = self.done.send((self.src, self.generation));
    }
}

/// A confirm/reject decision the desktop routes into the running pairing task.
///
/// `CancelWindow` and window replacement are conveyed by *closing* the task's
/// control channel (dropping [`PairingWindow`]), not a variant, so the task's
/// single `recv` covers "user decided" and "window gone" uniformly.
#[derive(Debug)]
enum PairingCtl {
    /// The user approved the fingerprint of the device identified by `device_id`
    /// (the id the desktop was shown in `PairingDeviceArrived`). The responder
    /// task drops a decision whose `device_id` does not match the device that
    /// actually arrived, so a stale queued decision cannot cross-confirm a
    /// different device after a window replacement.
    Confirm { device_id: DeviceId },
    /// The user rejected the device identified by `device_id` (see `Confirm`).
    Reject { device_id: DeviceId },
}

/// The bridge's single in-flight pairing window (ADR-0021 D3).
///
/// Created by `OpenWindow` with the minted pairing secret (the handshake PSK)
/// and the window deadline; `task` fills in once a device's first Pairing frame
/// arrives and the responder task spawns. At most one window exists at a time.
struct PairingWindow {
    /// The pairing secret (PSK) for the `IKpsk2` responder handshake — the same
    /// 32 bytes carried in the minted [`PairingCode`]'s `psk`.
    secret: [u8; 32],
    /// Unix seconds the window (and any unconfirmed arrival) expires; checked
    /// lazily when a Pairing frame arrives, matching the relay's no-timers design.
    expires_at: u64,
    /// Generation stamped at open; echoed back on task exit for a
    /// generation-guarded clear, so a newer window survives a stale done-signal.
    generation: u64,
    /// The running responder task, once a device has arrived. `None` while the
    /// window is open but no device has connected yet.
    task: Option<PairingTaskHandle>,
}

/// A handle to the running pairing responder task, held by [`PairingWindow`].
struct PairingTaskHandle {
    /// The device's routing id (envelope `src`) this task is bound to; a Pairing
    /// frame from any other `src` while it runs is dropped (single in-flight).
    src: DeviceId,
    /// Subsequent inbound Pairing frames (the E2E hello, the final confirm) for
    /// this device, forwarded from the read loop into the task.
    frame_tx: mpsc::Sender<Vec<u8>>,
    /// The user's confirm/reject decision, routed from a [`PairingCommand`].
    ctl_tx: mpsc::Sender<PairingCtl>,
}

/// How a pairing task ended, from the connection loop's perspective (ADR-0021
/// D3). The window is only *consumed* by an attempt that became user-visible;
/// a garbage frame or failed handshake must not burn the rest of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingExit {
    /// The attempt failed before anything user-visible happened — corrupt first
    /// frame, handshake failure (e.g. a wrong-PSK probe that reached the
    /// rendezvous window), a non-`Hello` first message, or a version mismatch.
    /// The in-flight slot is released so a fresh handshake from the legitimate
    /// device can start while the window is unexpired.
    ReleasedSlot,
    /// The ceremony reached `Pending` (the user saw the arrival): whatever the
    /// outcome — paired, rejected, expired, post-assert failure — the window is
    /// consumed. D3's "single completed handshake per window".
    ConsumedWindow,
}

/// Signals a pairing task's completion (its window generation + how it ended)
/// to the connection loop when dropped — firing on *every* exit path. Mirrors
/// [`DoneGuard`] for the singular pairing task.
///
/// Defaults to [`PairingExit::ReleasedSlot`]; [`run_pairing`] flips it to
/// `ConsumedWindow` the moment `Pending` is on the wire, so every earlier
/// return frees the slot and every later one consumes the window.
struct PairingDoneGuard {
    done: mpsc::UnboundedSender<(u64, PairingExit)>,
    generation: u64,
    exit: PairingExit,
}

impl PairingDoneGuard {
    /// Marks the window consumed: the attempt became user-visible (`Pending`
    /// sent), so no further handshake may start on this window.
    fn consume(&mut self) {
        self.exit = PairingExit::ConsumedWindow;
    }
}

impl Drop for PairingDoneGuard {
    fn drop(&mut self) {
        let _ = self.done.send((self.generation, self.exit));
    }
}

/// Applies a pairing task's done-signal to the loop's window state. Generation-
/// guarded (a stale signal from a replaced window is a no-op): a consumed window
/// is dropped entirely; a released slot keeps the window (secret + deadline)
/// alive with `task = None`, so the next Pairing frame from an unknown `src`
/// starts a fresh handshake (see [`dispatch_pairing_frame`]).
fn handle_pairing_done(pairing: &mut Option<PairingWindow>, generation: u64, exit: PairingExit) {
    let Some(window) = pairing.as_mut() else {
        return;
    };
    if window.generation != generation {
        return;
    }
    match exit {
        PairingExit::ConsumedWindow => *pairing = None,
        PairingExit::ReleasedSlot => window.task = None,
    }
}

/// Serves relay-mode sessions for `source` until `shutdown` fires.
///
/// Dials `config.relay_url` outbound, announces `role=bridge`, and serves E2E
/// Noise sessions with paired devices. On any connection loss it reconnects
/// with capped exponential backoff + jitter (200 ms … 30 s, ×2) until
/// `shutdown` is cancelled, at which point it returns `Ok(())`. The only `Err`
/// is a non-retryable [`BridgeServeError`] (an unusable relay URL).
pub async fn serve_bridge(
    config: BridgeConfig,
    source: Arc<dyn SessionSource>,
    mut commands: mpsc::Receiver<PairingCommand>,
    events: mpsc::Sender<BridgeEvent>,
    wake: WakeReceiver,
    shutdown: CancellationToken,
) -> Result<(), BridgeServeError> {
    if !is_ws_url(&config.relay_url) {
        return Err(BridgeServeError::InvalidRelayUrl(config.relay_url));
    }

    let deps = Arc::new(PeerDeps {
        bridge_id: config.identity.device_id,
        bridge_static_priv: config.identity.static_keypair.private.clone(),
        bridge_static_pub: config.identity.static_keypair.public.clone(),
        relay_url: config.relay_url.clone(),
        roster: config.roster.clone(),
        roster_path: config.roster_path.clone(),
        source,
        live_peers: Arc::new(Mutex::new(HashMap::new())),
        next_peer_reg: AtomicU64::new(0),
        episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
    });

    // The wake-note receiver, owned here so episode state survives relay
    // reconnects: a session still `Awaiting` across a reconnect is not re-woken.
    let mut wake = wake.0;

    // Correlation ids for relay control requests, monotonic across reconnects so
    // a late reply from a dropped connection can never be mistaken for a fresh
    // request's ack. Shared (`Arc`) so a spawned pairing task can mint ids from
    // the same sequence as the connection loop (ADR-0021 D3 assert-before-grant).
    let control_seq = Arc::new(AtomicU32::new(0));

    let mut backoff = BACKOFF_MIN;
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match run_connection(
            &config,
            &deps,
            &control_seq,
            &mut commands,
            &events,
            &mut wake,
            &shutdown,
        )
        .await
        {
            ConnOutcome::Shutdown => return Ok(()),
            // A fresh loss starts backoff over; a never-connected attempt keeps
            // the current (growing) delay so a down relay is not hammered.
            ConnOutcome::Disconnected => backoff = BACKOFF_MIN,
            ConnOutcome::ConnectFailed => {}
        }
        if shutdown.is_cancelled() {
            return Ok(());
        }
        // A fresh `ThreadRng` per iteration, never held across an `.await`, so
        // `serve_bridge`'s future stays `Send` and can be spawned onto a
        // multi-thread runtime (it is `tokio::spawn`ed by every caller).
        let delay = jittered(backoff, &mut rand::rng());
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        backoff = next_backoff(backoff);
    }
}

/// Runs one relay connection to completion: dial, hello, assert the roster, then
/// read/route until the connection drops or `shutdown` fires.
async fn run_connection(
    config: &BridgeConfig,
    deps: &Arc<PeerDeps>,
    control_seq: &Arc<AtomicU32>,
    commands: &mut mpsc::Receiver<PairingCommand>,
    events: &mpsc::Sender<BridgeEvent>,
    wake: &mut mpsc::Receiver<WakeNote>,
    shutdown: &CancellationToken,
) -> ConnOutcome {
    let ws = match connect_async(&config.relay_url).await {
        Ok((ws, _resp)) => ws,
        Err(_) => return ConnOutcome::ConnectFailed,
    };
    let (mut sink, mut stream) = ws.split();

    // Announce ourselves to the relay: role=bridge, routing_id == device_id.
    let hello = RelayHello {
        role: HelloRole::Bridge,
        token: config.registration_token.clone(),
        device_id: deps.bridge_id,
        routing_id: deps.bridge_id,
        bridge_id: deps.bridge_id,
    };
    let hello_payload = match serde_json::to_vec(&hello) {
        Ok(p) => p,
        Err(_) => return ConnOutcome::ConnectFailed,
    };
    let hello_frame = Envelope {
        frame_type: FrameType::Hello,
        src: deps.bridge_id,
        dst: DeviceId::ZERO,
        payload: hello_payload,
    }
    .encode();
    if sink
        .send(Message::Binary(hello_frame.into()))
        .await
        .is_err()
    {
        return ConnOutcome::ConnectFailed;
    }

    // The connection is established. A per-connection token tears down every
    // peer when the connection ends; it is a child of `shutdown`, so a global
    // shutdown also propagates to all peers.
    let conn_token = shutdown.child_token();

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Assert-before-serve (ADR-0021 D4): the relay admits no device hello until
    // it holds this bridge's device credentials, so we send `AssertDevices` for
    // the current roster and await the matching ack before entering the serve
    // loop. The reply rides back as an inbound Control frame, so we pump `stream`
    // here until it arrives (only control frames can precede admission).
    if let Some(outcome) = match assert_roster_and_await_ack(
        &mut stream,
        deps,
        &outbound_tx,
        control_seq.as_ref(),
        shutdown,
    )
    .await
    {
        AssertPhase::Acked => None,
        AssertPhase::Shutdown => Some(ConnOutcome::Shutdown),
        // A relay that refuses our assertion is a config-level problem, and
        // one that never answers is silent/hostile (ADR-0021 untrusted);
        // both grow the backoff rather than hammer a retry that will just
        // re-fail — the same path as a failed connect.
        AssertPhase::Rejected | AssertPhase::TimedOut => Some(ConnOutcome::ConnectFailed),
        AssertPhase::Disconnected => Some(ConnOutcome::Disconnected),
    } {
        conn_token.cancel();
        drop(outbound_tx);
        writer.abort();
        return outcome;
    }

    let mut peers = PeerRegistry::new();
    // Pending relay control requests (`RegisterPairing`/`AssertDevices`/…) awaiting
    // their `RelayControlAck`/`RelayControlError`, keyed by correlation id. Shared
    // (`Arc<Mutex>`) because the read loop completes waiters (see [`route_control_reply`])
    // while a spawned pairing task registers one for its assert-before-grant and
    // awaits it (ADR-0021 D3). Locks are brief and never held across an `.await`.
    let control_waiters: ControlWaiters = Arc::new(Mutex::new(HashMap::new()));
    // The bridge's single in-flight pairing window (ADR-0021 D3): `None` until the
    // desktop opens one, then the minted secret + deadline, and — once a device's
    // first Pairing frame arrives — a handle to the running responder task. At most
    // one exists; a second concurrent handshake is dropped.
    let mut pairing: Option<PairingWindow> = None;
    // Monotonic generation stamped into each pairing window, so a completed task's
    // late done-signal can never clear a newer window that replaced it.
    let mut pairing_generation: u64 = 0;
    // The running pairing task echoes its window generation + exit mode here so
    // the loop drops a consumed window or frees the slot of a pre-`Pending`
    // failure (mirrors [`DoneGuard`] for peers; see [`handle_pairing_done`]).
    let (pairing_done_tx, mut pairing_done_rx) = mpsc::unbounded_channel::<(u64, PairingExit)>();
    // Once the command channel closes (the desktop dropped its sender) we stop
    // selecting on it so a closed channel does not spin the loop.
    let mut commands_open = true;
    // Same for the wake-note channel: once the host drops its `BridgeWakeHandle`
    // we stop selecting on it (#233).
    let mut wake_open = true;
    // Peer tasks echo their `(src, generation)` here on exit so the loop can
    // reap the dead slot promptly (see [`DoneGuard`]). Unbounded so the reap
    // never blocks a returning peer; drained synchronously by the select below.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(DeviceId, u64)>();
    let outcome = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break ConnOutcome::Shutdown,
            done = done_rx.recv() => {
                // Only reaped when the stored generation still matches, so a
                // reconnected peer on the same `src` is never evicted.
                if let Some((src, generation)) = done {
                    peers.remove_if_generation(&src, generation);
                }
            }
            done = pairing_done_rx.recv() => {
                // Generation-guarded (a newer `OpenWindow` survives a stale
                // signal): consumed → drop the window; a pre-`Pending` failure
                // → free the slot so a fresh handshake can start.
                if let Some((generation, exit)) = done {
                    handle_pairing_done(&mut pairing, generation, exit);
                }
            }
            cmd = commands.recv(), if commands_open => match cmd {
                Some(cmd) => {
                    handle_command(
                        cmd,
                        deps,
                        &outbound_tx,
                        events,
                        control_seq,
                        &control_waiters,
                        &mut pairing,
                        &mut pairing_generation,
                    )
                    .await;
                }
                // Desktop dropped the command sender: no more commands will come.
                None => commands_open = false,
            },
            note = wake.recv(), if wake_open => match note {
                // A session changed status: de-dup into an episode and, on an
                // opening `Awaiting` edge, fan `PushTrigger` frames out over this
                // (live, authenticated) relay connection (#233).
                Some(note) => handle_wake_note(note, deps, &outbound_tx).await,
                // Host dropped its wake handle: no more notes will come.
                None => wake_open = false,
            },
            inbound = stream.next() => match inbound {
                None => break ConnOutcome::Disconnected,
                Some(Err(_)) => break ConnOutcome::Disconnected,
                Some(Ok(Message::Binary(bytes))) => {
                    dispatch_inbound(
                        bytes.as_ref(),
                        &mut peers,
                        &control_waiters,
                        &mut pairing,
                        deps,
                        &outbound_tx,
                        control_seq,
                        events,
                        &done_tx,
                        &pairing_done_tx,
                        &conn_token,
                    );
                }
                Some(Ok(Message::Close(_))) => break ConnOutcome::Disconnected,
                // Ping/Pong/Text/Frame: tungstenite answers pings itself; the
                // bridge speaks only binary Data/Control frames, so ignore the rest.
                Some(Ok(_)) => {}
            },
        }
    };

    // Tear down: cancel all peers and any pairing task, drop their inbound
    // queues, stop the writer. Dropping `pairing` closes the running task's
    // channels; `conn_token` (already a child of `shutdown`) also cancels it.
    conn_token.cancel();
    drop(peers);
    drop(pairing);
    drop(done_tx);
    drop(pairing_done_tx);
    drop(outbound_tx);
    writer.abort();
    outcome
}

/// The outcome of the assert-before-serve phase.
#[derive(Debug, PartialEq, Eq)]
enum AssertPhase {
    /// The relay acked our `AssertDevices`; proceed to serve.
    Acked,
    /// The relay rejected the assertion (`RelayControlError`).
    Rejected,
    /// The connection dropped before the ack arrived.
    Disconnected,
    /// The relay never answered within [`CONTROL_ACK_TIMEOUT`]: a silent (or
    /// malicious — ADR-0021 untrusted) relay must not wedge the pre-serve phase.
    TimedOut,
    /// `shutdown` fired while awaiting the ack.
    Shutdown,
}

/// Sends `AssertDevices` for the current roster and reads inbound frames until
/// the matching `RelayControlAck` (or error) arrives, bounded by
/// [`CONTROL_ACK_TIMEOUT`]. Generic over the stream so the reply-pump (and the
/// timeout) are exercised without a live socket in tests.
async fn assert_roster_and_await_ack<S, E>(
    stream: &mut S,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &AtomicU32,
    shutdown: &CancellationToken,
) -> AssertPhase
where
    S: Stream<Item = Result<Message, E>> + Unpin,
{
    let id = next_control_id(control_seq);
    let msg = {
        let roster = deps.roster.read().await;
        assert_devices_msg(id, &roster)
    };
    if send_control(outbound_tx, deps.bridge_id, &msg)
        .await
        .is_err()
    {
        return AssertPhase::Disconnected;
    }
    // One fixed deadline for the whole wait (not per-frame): a relay trickling
    // unrelated frames cannot keep resetting the clock.
    let deadline = tokio::time::sleep(CONTROL_ACK_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return AssertPhase::Shutdown,
            _ = &mut deadline => return AssertPhase::TimedOut,
            inbound = stream.next() => match inbound {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                    return AssertPhase::Disconnected
                }
                Some(Ok(Message::Binary(bytes))) => {
                    match parse_control_reply(bytes.as_ref(), deps.bridge_id) {
                        Some(ControlReply::Ack(ack)) if ack == id => return AssertPhase::Acked,
                        Some(ControlReply::Error(eid)) if eid == id => {
                            return AssertPhase::Rejected
                        }
                        // A stray/unrelated frame before the ack: keep reading
                        // (the relay admits no device until the assert lands).
                        _ => {}
                    }
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Routes one inbound binary frame to its peer task, spawning a new peer task
/// for a `src` not seen before. Pure-sync: never awaits, so the read loop stays
/// responsive across peers.
#[allow(clippy::too_many_arguments)]
fn dispatch_inbound(
    bytes: &[u8],
    peers: &mut PeerRegistry,
    control_waiters: &ControlWaiters,
    pairing: &mut Option<PairingWindow>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &Arc<AtomicU32>,
    events: &mpsc::Sender<BridgeEvent>,
    done_tx: &mpsc::UnboundedSender<(DeviceId, u64)>,
    pairing_done_tx: &mpsc::UnboundedSender<(u64, PairingExit)>,
    conn_token: &CancellationToken,
) {
    let envelope = match Envelope::decode(bytes) {
        Ok(e) => e,
        Err(_) => return, // malformed frame from the relay: ignore
    };
    // A relay-terminated Control reply (an ack/error for one of our control
    // requests) completes the matching waiter, then we are done with the frame.
    if envelope.frame_type == FrameType::Control && envelope.dst == deps.bridge_id {
        route_control_reply(&envelope.payload, control_waiters);
        return;
    }
    // A Pairing frame addressed to us drives the single in-flight pairing window
    // (ADR-0021 D3): its own responder task, not the per-`src` peer registry.
    if envelope.frame_type == FrameType::Pairing && envelope.dst == deps.bridge_id {
        dispatch_pairing_frame(
            envelope.src,
            envelope.payload,
            pairing,
            deps,
            outbound_tx,
            control_seq,
            control_waiters,
            events,
            pairing_done_tx,
            conn_token,
        );
        return;
    }
    // The relay only forwards Data frames between adjacent peers, addressed to
    // us; anything else is not ours to serve.
    if envelope.frame_type != FrameType::Data || envelope.dst != deps.bridge_id {
        return;
    }
    let src = envelope.src;
    let mut payload = envelope.payload;

    if let Some(frame_tx) = peers.get(&src) {
        match frame_tx.try_send(payload) {
            Ok(()) => return,
            // A wedged peer (not draining) drops frames; the resulting Noise
            // nonce gap fails that peer closed. Keeping it avoids churn.
            Err(mpsc::error::TrySendError::Full(_)) => return,
            // The peer task has exited; recover the frame and treat `src` as
            // new — the client is (re)starting a handshake on that route.
            Err(mpsc::error::TrySendError::Closed(returned)) => {
                peers.remove(&src);
                payload = returned;
            }
        }
    }

    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(PEER_FRAME_QUEUE);
    // The queue is empty and has capacity, so this first send cannot fail.
    if frame_tx.try_send(payload).is_ok() {
        // Stamp the slot's generation and hand it to the task, which echoes it
        // back on exit for a generation-guarded reap.
        let generation = peers.insert(src, frame_tx);
        tokio::spawn(run_peer(
            src,
            frame_rx,
            deps.clone(),
            outbound_tx.clone(),
            done_tx.clone(),
            generation,
            control_seq.clone(),
            conn_token.clone(),
        ));
    }
}

/// Routes one inbound Pairing frame into the pairing window's responder task,
/// spawning that task on the first frame from a device (ADR-0021 D3). Pure-sync
/// like [`dispatch_inbound`]: it never awaits.
///
/// - No open window, or a window past its deadline → the frame is dropped (and
///   an expired window is cleared so a later scan sees "no window").
/// - Window open, no device yet — including a slot freed by a pre-`Pending`
///   failure (see [`handle_pairing_done`]) → spawn [`run_pairing`] bound to this
///   `src`, handing it this frame as the handshake's first message.
/// - Task already running for this `src` → forward the frame (the E2E hello,
///   then the final confirm).
/// - A frame from any *other* `src` while a task runs → dropped: at most one
///   handshake is in flight, and exactly one *completed* (user-visible)
///   handshake consumes the window (ADR-0021 D3 single-use).
#[allow(clippy::too_many_arguments)]
fn dispatch_pairing_frame(
    src: DeviceId,
    payload: Vec<u8>,
    pairing: &mut Option<PairingWindow>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &Arc<AtomicU32>,
    control_waiters: &ControlWaiters,
    events: &mpsc::Sender<BridgeEvent>,
    pairing_done_tx: &mpsc::UnboundedSender<(u64, PairingExit)>,
    conn_token: &CancellationToken,
) {
    let Some(window) = pairing.as_mut() else {
        return; // no pairing window open: nothing to pair
    };
    // Lazy expiry (ADR-0021 D4): a window past its deadline is closed here rather
    // than by a background timer. Clearing it drops any running task's channels.
    if now_secs() > window.expires_at {
        *pairing = None;
        return;
    }

    if let Some(task) = window.task.as_ref() {
        // A device already arrived. Forward only frames from that same route; a
        // second concurrent handshake (different `src`) is dropped.
        if task.src == src {
            let _ = task.frame_tx.try_send(payload);
        }
        return;
    }

    // First frame of a fresh pairing: spawn the responder task bound to `src`.
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(PEER_FRAME_QUEUE);
    let (ctl_tx, ctl_rx) = mpsc::channel::<PairingCtl>(4);
    window.task = Some(PairingTaskHandle {
        src,
        frame_tx,
        ctl_tx,
    });
    tokio::spawn(run_pairing(
        PairingParams {
            src,
            first_frame: payload,
            secret: window.secret,
            expires_at: window.expires_at,
            generation: window.generation,
        },
        frame_rx,
        ctl_rx,
        deps.clone(),
        outbound_tx.clone(),
        control_seq.clone(),
        control_waiters.clone(),
        events.clone(),
        pairing_done_tx.clone(),
        conn_token.clone(),
    ));
}

/// The immutable per-attempt inputs handed to [`run_pairing`], grouped so the
/// task's signature stays legible.
struct PairingParams {
    /// The device's routing id (envelope `src`); also the Noise initiator
    /// routing id in the prologue and the `dst` of every reply frame.
    src: DeviceId,
    /// The device's first Pairing frame: `32-byte device identity ‖ noise msg1`,
    /// framed exactly like the session handshake's first frame.
    first_frame: Vec<u8>,
    /// The window's pairing secret (the handshake PSK).
    secret: [u8; 32],
    /// Unix-seconds deadline shared with the window; an unconfirmed arrival
    /// expires with it (ADR-0021 D3).
    expires_at: u64,
    /// The window generation echoed back on exit for a generation-guarded clear.
    generation: u64,
}

/// The whole confirm-gated pairing ceremony for one arrived device (ADR-0021
/// D3), run as a dedicated task off the connection loop.
///
/// Sequence: responder `IKpsk2` handshake (PSK = the window secret, prologue
/// bound with [`HandshakeKind::Pairing`]) → read [`PairingClientMsg::Hello`],
/// version-gate, emit `PairingDeviceArrived`, send `Pending` → await the user's
/// `Confirm`/`Reject` (or window expiry) → on `Confirm`, refuse a duplicate id,
/// else assert-before-grant, send `Grant`, await the device's `Confirm`, persist
/// the roster entry, send `Confirmed`, `CancelPairing`, and emit `Paired`.
///
/// Every early return re-asserts roster-only if a pending credential was already
/// asserted, so an unconfirmed pending device is never left in the relay's set.
#[allow(clippy::too_many_arguments)]
async fn run_pairing(
    params: PairingParams,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    mut ctl_rx: mpsc::Receiver<PairingCtl>,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    control_seq: Arc<AtomicU32>,
    control_waiters: ControlWaiters,
    events: mpsc::Sender<BridgeEvent>,
    pairing_done_tx: mpsc::UnboundedSender<(u64, PairingExit)>,
    conn_token: CancellationToken,
) {
    // Signal the connection loop when this task returns for any reason. Until
    // `Pending` is on the wire the exit is `ReleasedSlot` — a garbage frame or
    // failed handshake (e.g. a wrong-PSK probe) must not burn the window for the
    // legitimate device; after `Pending` (user-visible arrival) every outcome
    // consumes the window (D3's single completed handshake per window).
    let mut done = PairingDoneGuard {
        done: pairing_done_tx,
        generation: params.generation,
        exit: PairingExit::ReleasedSlot,
    };

    // --- Responder handshake. The first frame carries the device's minted
    // identity preamble + noise msg1, exactly like the session path; the routing
    // id is the envelope `src`. Both feed the `Pairing` prologue. ---
    let Some((device_id, msg1)) = split_preamble(&params.first_frame) else {
        return;
    };
    let bound = prologue(
        HandshakeKind::Pairing,
        &device_id,
        &params.src,
        &deps.bridge_id,
    );
    let Ok(mut hs) = Handshake::responder(&deps.bridge_static_priv, &params.secret, &bound) else {
        return;
    };
    if hs.read_message(msg1).is_err() {
        return;
    }
    let Ok(msg2) = hs.write_message(&[]) else {
        return;
    };
    if send_pairing_frame(&outbound_tx, deps.bridge_id, params.src, msg2)
        .await
        .is_err()
    {
        return;
    }
    let Ok((mut transport, remote_static)) = hs.into_transport() else {
        return;
    };
    // The pairing handshake authenticates the device's static; an empty one means
    // snow never learned it — treat as a failed handshake.
    if remote_static.is_empty() {
        return;
    }

    // --- E2E hello: version-gate, then surface the device for confirmation. ---
    let Some(hello_frame) = recv_pairing_frame(&mut frame_rx, params.expires_at, &conn_token).await
    else {
        return;
    };
    let (client_version, raw_name) = match transport.open::<PairingClientMsg>(&hello_frame) {
        Ok(PairingClientMsg::Hello {
            protocol_version,
            device_name,
        }) => (protocol_version, device_name),
        // A decrypt failure or any non-Hello first message is a protocol
        // violation — drop the attempt silently (no pending credential asserted).
        _ => return,
    };
    if !client_version_ok(client_version) {
        let _ = send_pairing_msg(
            &mut transport,
            &outbound_tx,
            deps.bridge_id,
            params.src,
            &PairingBridgeMsg::Rejected {
                reason: PairingRejectReason::VersionMismatch {
                    bridge_min: PROTOCOL_VERSION,
                },
            },
        )
        .await;
        return;
    }
    let name = sanitize(&raw_name, MAX_DEVICE_NAME_CHARS).into_string();
    let device_fingerprint = fingerprint(&remote_static);
    let _ = events
        .send(BridgeEvent::PairingDeviceArrived {
            device_id,
            name: name.clone(),
            fingerprint: device_fingerprint,
        })
        .await;
    if send_pairing_msg(
        &mut transport,
        &outbound_tx,
        deps.bridge_id,
        params.src,
        &PairingBridgeMsg::Pending,
    )
    .await
    .is_err()
    {
        return;
    }
    // The arrival is now user-visible: from here on, every exit consumes the
    // window — no second handshake may follow a completed one (ADR-0021 D3).
    done.consume();

    // --- Await the user's decision (or window expiry). No pending credential is
    // asserted yet, so reject/expiry here just closes with no relay cleanup. ---
    let decision =
        await_pairing_decision(&mut ctl_rx, &device_id, params.expires_at, &conn_token).await;
    match decision {
        PairingDecision::Confirm => {}
        PairingDecision::Reject => {
            let _ = send_pairing_msg(
                &mut transport,
                &outbound_tx,
                deps.bridge_id,
                params.src,
                &PairingBridgeMsg::Rejected {
                    reason: PairingRejectReason::UserRejected,
                },
            )
            .await;
            let _ = events
                .send(BridgeEvent::PairingResult(PairingOutcome::Rejected {
                    device_id,
                }))
                .await;
            return;
        }
        PairingDecision::Closed => {
            // Expiry, cancel, or window replacement: nothing durable was granted.
            let _ = send_pairing_msg(
                &mut transport,
                &outbound_tx,
                deps.bridge_id,
                params.src,
                &PairingBridgeMsg::Rejected {
                    reason: PairingRejectReason::WindowClosed,
                },
            )
            .await;
            let _ = events
                .send(BridgeEvent::PairingResult(PairingOutcome::Expired))
                .await;
            return;
        }
    }

    // --- Confirm path. Refuse a duplicate id before minting anything. ---
    if deps.roster.read().await.contains_device(&device_id) {
        let _ = send_pairing_msg(
            &mut transport,
            &outbound_tx,
            deps.bridge_id,
            params.src,
            &PairingBridgeMsg::Rejected {
                reason: PairingRejectReason::DuplicateId,
            },
        )
        .await;
        let _ = events
            .send(BridgeEvent::PairingResult(PairingOutcome::Rejected {
                device_id,
            }))
            .await;
        return;
    }

    // Mint the durable per-pair credentials the device will persist.
    let (Ok(relay_token), Ok(session_psk)) = (next_device_token(), next_session_psk()) else {
        return; // OS CSPRNG failure: abort before any assert
    };
    let entry = RosterEntry {
        device_id,
        static_pubkey: remote_static.clone(),
        psk: session_psk,
        relay_token: relay_token.clone(),
        name: name.clone(),
        enrolled_at: Some(now_secs()),
        last_connected_at: None,
        push: None,
    };

    // Assert-before-grant (ADR-0021 D3): the relay must credential the pending
    // device before it reconnects durably, so assert `roster ∪ pending` and await
    // the ack. From here on, a pending credential is live in the relay's set, so
    // every failure path re-asserts roster-only to drop it.
    let pending_assert = {
        let roster = deps.roster.read().await;
        let mut devices: Vec<AssertedDevice> = roster
            .entries
            .iter()
            .map(|e| AssertedDevice {
                device_id: e.device_id,
                token: device_token_for(e),
                push: e.push.clone(),
            })
            .collect();
        devices.push(AssertedDevice {
            device_id,
            token: relay_token.clone(),
            push: entry.push.clone(),
        });
        RelayControl::AssertDevices {
            id: next_control_id(control_seq.as_ref()),
            devices,
        }
    };
    let asserted = send_control_await_ack(
        &outbound_tx,
        deps.bridge_id,
        &pending_assert,
        &control_waiters,
        &conn_token,
    )
    .await;
    if !asserted {
        // The relay never acked (silent, errored, or the link dropped): the
        // pending credential may or may not be live; re-assert roster-only to be
        // sure it is dropped, then abandon the attempt.
        reassert_roster_only(&deps, &outbound_tx, &control_seq).await;
        let _ = events
            .send(BridgeEvent::PairingResult(PairingOutcome::Expired))
            .await;
        return;
    }

    // Grant the durable credentials. If the send fails the link is gone; drop the
    // pending credential and abandon.
    if send_pairing_msg(
        &mut transport,
        &outbound_tx,
        deps.bridge_id,
        params.src,
        &PairingBridgeMsg::Grant {
            device_token: relay_token,
            psk: B64.encode(session_psk),
            bridge_name: None,
        },
    )
    .await
    .is_err()
    {
        reassert_roster_only(&deps, &outbound_tx, &control_seq).await;
        return;
    }

    // Await the device's Confirm — only then does the roster persist (ADR-0021
    // D3: the device shows "paired ✓" solely after our final Confirmed ack, so we
    // must persist before sending it). If it never arrives, persist nothing and
    // re-assert roster-only so no ghost credential lingers.
    let confirmed = match recv_pairing_frame(&mut frame_rx, params.expires_at, &conn_token).await {
        Some(frame) => matches!(
            transport.open::<PairingClientMsg>(&frame),
            Ok(PairingClientMsg::Confirm)
        ),
        None => false,
    };
    if !confirmed {
        reassert_roster_only(&deps, &outbound_tx, &control_seq).await;
        let _ = events
            .send(BridgeEvent::PairingResult(PairingOutcome::Expired))
            .await;
        return;
    }

    // Persist the roster entry, then send the final Confirmed ack. The asserted
    // set (roster ∪ pending) already equals the post-push roster, so no re-assert
    // is needed on this success path.
    {
        let mut roster = deps.roster.write().await;
        roster.entries.push(entry);
        if let Err(_e) = roster.save(&deps.roster_path) {
            // Persist failed after the device already stored its credentials: undo
            // the in-memory push and re-assert roster-only so the relay drops the
            // pending credential. The device (no final ack) re-pairs.
            roster.entries.pop();
            drop(roster);
            reassert_roster_only(&deps, &outbound_tx, &control_seq).await;
            let _ = events
                .send(BridgeEvent::PairingResult(PairingOutcome::Expired))
                .await;
            return;
        }
    }
    let _ = send_pairing_msg(
        &mut transport,
        &outbound_tx,
        deps.bridge_id,
        params.src,
        &PairingBridgeMsg::Confirmed,
    )
    .await;
    // Close the relay's window now that the single handshake completed.
    let _ = send_control(
        &outbound_tx,
        deps.bridge_id,
        &RelayControl::CancelPairing {
            id: next_control_id(control_seq.as_ref()),
        },
    )
    .await;
    let _ = events
        .send(BridgeEvent::PairingResult(PairingOutcome::Paired {
            device_id,
            name,
        }))
        .await;
    let _ = events.send(BridgeEvent::RosterChanged).await;
}

/// The resolved outcome of the confirm/reject await.
enum PairingDecision {
    /// The user confirmed.
    Confirm,
    /// The user rejected.
    Reject,
    /// The window closed first: expiry, cancel, or replacement.
    Closed,
}

/// Awaits the user's confirm/reject decision for the device that actually
/// arrived (`arrived`), bounded by the window deadline and the connection token.
/// A closed control channel (the desktop cancelled or a new window replaced this
/// one) or the deadline both resolve to `Closed`.
///
/// A decision naming a *different* device — e.g. a stale `Confirm` still queued
/// on the control channel (cap 4) from a window that was replaced before this
/// device arrived — is dropped, and the await keeps waiting. This binds the
/// decision to the arrived device so a queued approval can never enroll the
/// wrong one after a window replacement.
async fn await_pairing_decision(
    ctl_rx: &mut mpsc::Receiver<PairingCtl>,
    arrived: &DeviceId,
    expires_at: u64,
    conn_token: &CancellationToken,
) -> PairingDecision {
    let sleep = tokio::time::sleep(deadline_from_now(expires_at));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = conn_token.cancelled() => return PairingDecision::Closed,
            _ = &mut sleep => return PairingDecision::Closed,
            ctl = ctl_rx.recv() => match ctl {
                Some(PairingCtl::Confirm { device_id }) if &device_id == arrived => {
                    return PairingDecision::Confirm;
                }
                Some(PairingCtl::Reject { device_id }) if &device_id == arrived => {
                    return PairingDecision::Reject;
                }
                // A decision for a stale/different device: ignore it and keep
                // waiting for one that matches the arrived device (or the window
                // to close).
                Some(_) => continue,
                None => return PairingDecision::Closed,
            },
        }
    }
}

/// Receives the next inbound Pairing frame for this attempt, bounded by the
/// window deadline and the connection token. `None` on deadline, teardown, or a
/// closed frame channel (window replaced/cancelled).
async fn recv_pairing_frame(
    frame_rx: &mut mpsc::Receiver<Vec<u8>>,
    expires_at: u64,
    conn_token: &CancellationToken,
) -> Option<Vec<u8>> {
    let sleep = tokio::time::sleep(deadline_from_now(expires_at));
    tokio::pin!(sleep);
    tokio::select! {
        _ = conn_token.cancelled() => None,
        _ = &mut sleep => None,
        frame = frame_rx.recv() => frame,
    }
}

/// The remaining duration until `expires_at` (Unix seconds), or zero if already
/// past — so an expired window fires its deadline branch immediately.
fn deadline_from_now(expires_at: u64) -> Duration {
    Duration::from_secs(expires_at.saturating_sub(now_secs()))
}

/// Sends a [`RelayControl`] and awaits its `RelayControlAck`, registering a
/// waiter in the shared map that the read loop completes. Returns `true` only on
/// a matching ack; `false` on a relay error, teardown, or [`CONTROL_ACK_TIMEOUT`].
async fn send_control_await_ack(
    outbound_tx: &mpsc::Sender<Message>,
    bridge_id: DeviceId,
    control: &RelayControl,
    control_waiters: &ControlWaiters,
    conn_token: &CancellationToken,
) -> bool {
    let RelayControl::AssertDevices { id, .. } = control else {
        // This helper is only used for AssertDevices; other control messages do
        // not correlate an ack here.
        return false;
    };
    let id = *id;
    let (tx, rx) = oneshot::channel();
    if let Ok(mut waiters) = control_waiters.lock() {
        waiters.insert(id, tx);
    } else {
        return false;
    }
    if send_control(outbound_tx, bridge_id, control).await.is_err() {
        if let Ok(mut waiters) = control_waiters.lock() {
            waiters.remove(&id);
        }
        return false;
    }
    let acked = tokio::select! {
        _ = conn_token.cancelled() => false,
        _ = tokio::time::sleep(CONTROL_ACK_TIMEOUT) => false,
        reply = rx => matches!(reply, Ok(Ok(()))),
    };
    if !acked {
        if let Ok(mut waiters) = control_waiters.lock() {
            waiters.remove(&id);
        }
    }
    acked
}

/// Re-asserts the persisted roster (roster-only) so the relay drops any pending
/// credential from an abandoned pairing. Best-effort: a dropped link cannot leave
/// a ghost credential because the relay clears a bridge's whole set on disconnect
/// (ADR-0021 D4).
async fn reassert_roster_only(
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &Arc<AtomicU32>,
) {
    let assert = {
        let roster = deps.roster.read().await;
        assert_devices_msg(next_control_id(control_seq.as_ref()), &roster)
    };
    let _ = send_control(outbound_tx, deps.bridge_id, &assert).await;
}

/// Whether a pairing device's advertised `protocol_version` is acceptable: it
/// must meet the bridge's minimum ([`PROTOCOL_VERSION`]). A lower version pairs
/// against a bridge that speaks newer wire types it cannot, so it is refused with
/// [`PairingRejectReason::VersionMismatch`] rather than left to fail as an opaque
/// AEAD error later (ADR-0021 D8, version preflight).
fn client_version_ok(client_version: u32) -> bool {
    client_version >= PROTOCOL_VERSION
}

/// Mints a fresh per-device relay token: hex of 32 OS-CSPRNG bytes.
fn next_device_token() -> Result<String, BridgeError> {
    let mut raw = [0u8; 32];
    fill_random(&mut raw)?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// Mints a fresh per-pair session PSK: 32 OS-CSPRNG bytes.
fn next_session_psk() -> Result<[u8; 32], BridgeError> {
    let mut psk = [0u8; 32];
    fill_random(&mut psk)?;
    Ok(psk)
}

/// Wraps `payload` in a Pairing envelope (frame_type = [`FrameType::Pairing`])
/// and enqueues it. Pairing-channel frames — the handshake response and every
/// sealed [`PairingBridgeMsg`] — ride this, distinct from Data frames.
async fn send_pairing_frame(
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let frame = Envelope {
        frame_type: FrameType::Pairing,
        src,
        dst,
        payload,
    }
    .encode();
    outbound_tx
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ())
}

/// Seals a [`PairingBridgeMsg`] on the pairing transport and enqueues it as a
/// Pairing frame. `Err(())` means the peer should be abandoned (seal/send failed).
async fn send_pairing_msg(
    transport: &mut Transport,
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
    msg: &PairingBridgeMsg,
) -> Result<(), ()> {
    let ciphertext = transport.seal(msg).map_err(|_| ())?;
    send_pairing_frame(outbound_tx, src, dst, ciphertext).await
}

/// One client peer's whole lifecycle: Noise handshake, E2E hello, then serve.
#[allow(clippy::too_many_arguments)]
async fn run_peer(
    routing_id: DeviceId,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    done_tx: mpsc::UnboundedSender<(DeviceId, u64)>,
    generation: u64,
    control_seq: Arc<AtomicU32>,
    conn_token: CancellationToken,
) {
    // Reap this peer's registry slot when the task returns for *any* reason —
    // roster miss, handshake fail, hello mismatch, decrypt error, channel death,
    // or cancellation. Generation-guarded so a reconnected peer survives (#231).
    let _done_guard = DoneGuard {
        done: done_tx,
        src: routing_id,
        generation,
    };
    // The handshake yields the roster-proven `device_id` (the authenticated
    // identity), distinct from the relay routing id: revocation targets the
    // device, so the serve loop keys its live-session registration by it.
    let Some((transport, device_id)) =
        handshake(&routing_id, &mut frame_rx, &deps, &outbound_tx, &conn_token).await
    else {
        return; // any handshake / auth failure drops just this peer
    };
    serve_peer(
        routing_id,
        device_id,
        frame_rx,
        transport,
        deps,
        outbound_tx,
        control_seq,
        conn_token,
    )
    .await;
}

/// Drives the responder side of the Noise handshake from the peer's first
/// frame, verifying the authenticated static against the roster. Returns the
/// established [`Transport`] paired with the roster-proven `device_id` (the
/// initiator's claimed identity, now Noise-authenticated against its pinned
/// static), or `None` to drop the peer.
async fn handshake(
    routing_id: &DeviceId,
    frame_rx: &mut mpsc::Receiver<Vec<u8>>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    conn_token: &CancellationToken,
) -> Option<(Transport, DeviceId)> {
    let first = tokio::select! {
        _ = conn_token.cancelled() => return None,
        f = frame_rx.recv() => f?,
    };

    // spec D16: 32-byte plaintext identity preamble ‖ noise msg1.
    let (initiator_identity, msg1) = split_preamble(&first)?;

    // Roster lookup by claimed identity selects the PSK and pins the static. The
    // shared roster can be mutated by pairing/revocation, so copy out the two
    // credentials this handshake needs and drop the read guard before awaiting.
    let (psk, static_pubkey) = {
        let roster = deps.roster.read().await;
        let entry = roster.find_by_device(&initiator_identity)?;
        (entry.psk, entry.static_pubkey.clone())
    };

    // The prologue binds this exact route; a forged identity/routing yields a
    // different prologue than the honest client used, failing the handshake.
    let bound = prologue(
        HandshakeKind::Session,
        &initiator_identity,
        routing_id,
        &deps.bridge_id,
    );
    let mut hs = Handshake::responder(&deps.bridge_static_priv, &psk, &bound).ok()?;
    hs.read_message(msg1).ok()?;
    let msg2 = hs.write_message(&[]).ok()?;
    send_frame(outbound_tx, deps.bridge_id, *routing_id, msg2)
        .await
        .ok()?;
    let (transport, remote_static) = hs.into_transport().ok()?;

    // spec C7: the Noise-authenticated initiator static MUST equal the pinned
    // roster key. `into_transport` yields an empty vec when snow never learned
    // a remote static; treat empty OR mismatch as an auth failure.
    if remote_static.is_empty() || remote_static != static_pubkey {
        return None;
    }
    Some((transport, initiator_identity))
}

/// Reads the peer's first post-handshake frame (its E2E hello), bounded by
/// `timeout`. Returns `None` — so the caller drops the peer — on connection
/// teardown, a closed frame channel, or the timeout elapsing (a client that
/// handshook then never sent Hello). Factored out so the timeout seam is unit
/// testable without a live Noise transport.
async fn recv_hello_frame(
    frame_rx: &mut mpsc::Receiver<Vec<u8>>,
    conn_token: &CancellationToken,
    timeout: Duration,
) -> Option<Vec<u8>> {
    tokio::select! {
        _ = conn_token.cancelled() => None,
        r = tokio::time::timeout(timeout, frame_rx.recv()) => match r {
            Ok(Some(frame)) => Some(frame),
            // Frame channel closed (Ok(None)) or the hello deadline elapsed (Err).
            Ok(None) | Err(_) => None,
        },
    }
}

/// After a good handshake: the strict E2E hello exchange, then the serve loop
/// (request/response + the attached PTY stream). Owns `transport` outright.
#[allow(clippy::too_many_arguments)]
async fn serve_peer(
    routing_id: DeviceId,
    device_id: DeviceId,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    mut transport: Transport,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    control_seq: Arc<AtomicU32>,
    conn_token: CancellationToken,
) {
    let bridge_id = deps.bridge_id;

    // A peer-scoped token (child of the connection token) so returning from
    // this task — for any reason — stops the peer's pump task promptly, and so a
    // revocation of this `device_id` can cancel *this* session directly. As a
    // child of `conn_token` it still fires on a connection-wide teardown.
    let peer_token = conn_token.child_token();
    let _guard = CancelGuard(peer_token.clone());

    // Register this authenticated session so revoking `device_id` severs it
    // bridge-side (ADR-0021 D6). Deregistered by the RAII guard on any exit.
    let reg_id = deps.next_peer_reg.fetch_add(1, Ordering::Relaxed);
    let _live_guard = register_live_peer(&deps.live_peers, device_id, reg_id, peer_token.clone());

    // --- E2E hello: the client's first application message must be Hello. A
    // paired client that completes the handshake but never sends it is dropped
    // once PEER_HELLO_TIMEOUT elapses, so the peer task + registry slot are
    // reaped instead of pinned forever (#231). ---
    let Some(hello_frame) = recv_hello_frame(&mut frame_rx, &peer_token, PEER_HELLO_TIMEOUT).await
    else {
        return;
    };
    let client_version = match transport.open::<ClientMessage>(&hello_frame) {
        Ok(ClientMessage::Hello { protocol_version }) => protocol_version,
        // A decrypt failure or a non-Hello first message is a protocol
        // violation — drop the peer.
        _ => return,
    };
    // Always answer with our version so the client can fail closed too, then
    // fail closed on our side on any mismatch (strict equality).
    if send_msg(
        &mut transport,
        &outbound_tx,
        bridge_id,
        routing_id,
        &BridgeMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .is_err()
    {
        return;
    }
    if client_version != PROTOCOL_VERSION {
        return;
    }

    // The session is now fully established: stamp the successful connect on the
    // device's roster entry so ghost / re-paired entries are self-evident in
    // `ListDevices`. Best-effort metadata — a failed persist must not drop a
    // live session, and a since-revoked device (no entry) is simply skipped.
    {
        let mut roster = deps.roster.write().await;
        if let Some(entry) = roster.find_by_device_mut(&device_id) {
            entry.last_connected_at = Some(now_secs());
            let _ = roster.save(&deps.roster_path);
        }
    }

    // --- Serve. `transport` is sealed/opened only here, so nonce order is the
    // send order. The peer's attached PTY stream arrives as plaintext
    // `PeerEvent`s over `events_rx`, which is always present (no `Option` in
    // the select), sidestepping any borrow tangle with the attach state. ---
    let (events_tx, mut events_rx) = mpsc::channel::<PeerEvent>(PEER_EVENT_QUEUE);
    let mut attach_input: Option<mpsc::Sender<ChannelInput>> = None;
    let mut has_attached = false;
    // The session this peer is attached to, if any — its wake episode is
    // forgotten when the channel is torn down (#233).
    let mut attached_session: Option<EpisodeKey> = None;

    loop {
        tokio::select! {
            _ = peer_token.cancelled() => return,
            frame = frame_rx.recv() => {
                let Some(frame) = frame else { return };
                let msg = match transport.open::<ClientMessage>(&frame) {
                    Ok(m) => m,
                    Err(_) => return, // decrypt / nonce failure: drop THIS peer only
                };
                match msg {
                    // A second hello is out of protocol; ignore it.
                    ClientMessage::Hello { .. } => {}
                    ClientMessage::Request { id, op } => {
                        // A successful `RevokeDevice` mutates the roster inside
                        // `handle_request` but defers the *kick* to here (see
                        // below), so the `Revoked` response is enqueued before
                        // the relay re-assert that severs the transport.
                        let mut revoke_target: Option<DeviceId> = None;
                        let result = handle_request(
                            op,
                            &deps,
                            &events_tx,
                            &peer_token,
                            &mut attach_input,
                            &mut has_attached,
                            &mut attached_session,
                            device_id,
                            &mut revoke_target,
                            &outbound_tx,
                            &control_seq,
                        )
                        .await;
                        let send_ok = send_msg(
                            &mut transport,
                            &outbound_tx,
                            bridge_id,
                            routing_id,
                            &BridgeMessage::Response { id, result },
                        )
                        .await
                        .is_ok();
                        // Response-first, kick-after (ADR-0021 D6): the roster is
                        // already shrunken; now sever the target's live
                        // session(s) bridge-side and re-assert the shrunken set to
                        // the relay. On the success path the `Revoked` response is
                        // already on the outbound queue ahead of the re-assert, so
                        // the requester still gets its answer. The kick runs
                        // *regardless* of the response-send outcome: a per-peer
                        // seal failure must not leave a revoked device asserted at
                        // the relay with its live sessions uncancelled until the
                        // next roster change. For a self-revoke this cancels *this*
                        // peer — the loop returns on its next iteration.
                        if let Some(target) = revoke_target.take() {
                            assert_and_kick(&target, &deps, &outbound_tx, &control_seq).await;
                        }
                        if !send_ok {
                            return;
                        }
                    }
                    ClientMessage::Input(input) => {
                        if let Some(tx) = &attach_input {
                            if tx.send(input).await.is_err() {
                                // The channel died mid-send: report it and clear
                                // the attach state (and forget its wake episode).
                                attach_input = None;
                                forget_episode(&deps, &mut attached_session);
                                if send_msg(
                                    &mut transport,
                                    &outbound_tx,
                                    bridge_id,
                                    routing_id,
                                    &BridgeMessage::ChannelClosed,
                                )
                                .await
                                .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        // Input before a successful attach is meaningless — drop.
                    }
                    // `ClientMessage` is `#[non_exhaustive]`: ignore a variant
                    // this bridge predates rather than dropping the peer.
                    _ => {}
                }
            }
            event = events_rx.recv() => match event {
                Some(PeerEvent::Output(out)) => {
                    if send_output(&mut transport, &outbound_tx, bridge_id, routing_id, out)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Some(PeerEvent::Dead) => {
                    attach_input = None;
                    // The attached session channel was torn down: forget its wake
                    // episode so a later respawn's `Awaiting` wakes afresh (#233).
                    forget_episode(&deps, &mut attached_session);
                    if send_msg(
                        &mut transport,
                        &outbound_tx,
                        bridge_id,
                        routing_id,
                        &BridgeMessage::ChannelClosed,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                // `events_tx` is held for the whole loop, so `recv` never yields
                // `None` here; nothing to do.
                None => {}
            },
        }
    }
}

/// Handles one `Request` op, returning the [`RemoteResult`] to send back.
///
/// `List` runs the source list; `Attach` enforces at-most-one attach per peer,
/// and on success installs the input sender and spawns the PTY pump task.
/// `ListDevices` projects the roster (with `is_self` marked for `requester_id`);
/// `RevokeDevice` removes the target from the roster and persists it, then sets
/// `revoke_target` so the caller can enqueue the response *before* the kick
/// (the relay re-assert severs the transport — see [`serve_peer`]).
/// `RegisterPushEndpoint` (ADR-0023) validates a `Some` registration before
/// storing it (never after — the #232 relay_url gotcha), updates *only* the
/// requesting device's roster entry, persists, and re-asserts the roster to
/// the relay inline — unlike revoke, nothing here severs the transport, so
/// the re-assert needs no deferral.
#[allow(clippy::too_many_arguments)]
async fn handle_request(
    op: RemoteOp,
    deps: &Arc<PeerDeps>,
    events_tx: &mpsc::Sender<PeerEvent>,
    peer_token: &CancellationToken,
    attach_input: &mut Option<mpsc::Sender<ChannelInput>>,
    has_attached: &mut bool,
    attached_session: &mut Option<EpisodeKey>,
    requester_id: DeviceId,
    revoke_target: &mut Option<DeviceId>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &Arc<AtomicU32>,
) -> RemoteResult {
    match op {
        RemoteOp::List => match deps.source.list().await {
            Ok(sessions) => RemoteResult::Sessions(sessions),
            Err(e) => RemoteResult::Error(map_source_error(&e)),
        },
        RemoteOp::Attach {
            project_id,
            session_id,
        } => {
            if *has_attached {
                return RemoteResult::Error(WireError::Transport {
                    message: "attach already active".to_string(),
                });
            }
            match deps.source.attach(&project_id, &session_id).await {
                Ok(channel) => {
                    *has_attached = true;
                    *attach_input = Some(channel.input);
                    // Remember which session this channel drives, so its episode
                    // can be forgotten when the channel is torn down (#233).
                    *attached_session = Some((project_id, session_id));
                    tokio::spawn(pump_output(
                        channel.output,
                        events_tx.clone(),
                        peer_token.child_token(),
                    ));
                    RemoteResult::Attached
                }
                Err(e) => RemoteResult::Error(map_source_error(&e)),
            }
        }
        RemoteOp::ListDevices => {
            let roster = deps.roster.read().await;
            RemoteResult::Devices(device_infos(&roster, &requester_id))
        }
        RemoteOp::RevokeDevice { device_id } => {
            match revoke_from_roster(&device_id, deps).await {
                // Removed: defer the kick so the response goes out first.
                Ok(true) => {
                    *revoke_target = Some(device_id);
                    RemoteResult::Revoked
                }
                // Not paired: an idempotent unpair — no kick, still success.
                Ok(false) => RemoteResult::Revoked,
                // Persisting the shrunken roster failed: report it rather than
                // claim a revocation the bridge did not durably record.
                Err(_e) => RemoteResult::Error(WireError::Transport {
                    message: "could not persist roster".to_string(),
                }),
            }
        }
        RemoteOp::RegisterPushEndpoint { registration } => {
            // Validate before storing (the #232 relay_url gotcha): a
            // client-controlled endpoint must be rejected before it ever
            // lands in the roster or gets asserted to the relay, never
            // after. `PushRegistration` is `#[non_exhaustive]`: an
            // unrecognized future variant is refused rather than matched,
            // so an older bridge fails safe instead of panicking.
            if let Some(reg) = &registration {
                let PushRegistration::UnifiedPush { endpoint } = reg else {
                    return RemoteResult::Error(WireError::Transport {
                        message: "unsupported push registration variant".to_string(),
                    });
                };
                if let Err(e) = validate_push_endpoint(endpoint) {
                    return RemoteResult::Error(WireError::Transport {
                        message: format!("invalid push endpoint: {e}"),
                    });
                }
            }
            {
                let mut roster = deps.roster.write().await;
                let Some(entry) = roster.find_by_device_mut(&requester_id) else {
                    // Not in the roster (already revoked/never paired): no
                    // entry to update, and nothing to re-assert.
                    return RemoteResult::Error(WireError::Transport {
                        message: "device not paired".to_string(),
                    });
                };
                entry.push = registration;
                if let Err(_e) = roster.save(&deps.roster_path) {
                    return RemoteResult::Error(WireError::Transport {
                        message: "could not persist roster".to_string(),
                    });
                }
            }
            // Refresh the relay's view so it starts (or stops) waking this
            // device, mirroring the credential set the roster now holds.
            // Best-effort: `reassert_roster_only` swallows a send failure (the
            // relay connection may be down), and the roster write above already
            // durably persisted the registration, so this still answers
            // `PushEndpointSet` — the next successful re-assert (e.g. on
            // reconnect) picks up the stored change.
            reassert_roster_only(deps, outbound_tx, control_seq).await;
            RemoteResult::PushEndpointSet
        }
        // `RemoteOp` is `#[non_exhaustive]`: a client speaking a newer protocol
        // could ask for an op this bridge predates. Refuse it explicitly rather
        // than dropping the peer, so the client gets a typed answer.
        _ => RemoteResult::Error(WireError::Transport {
            message: "unsupported operation".to_string(),
        }),
    }
}

/// Projects the roster into wire [`DeviceInfo`]s (ADR-0021 D6 `ListDevices`),
/// marking the entry whose id equals `requester_id` as `is_self`. `name` is the
/// already-sanitized roster value (scrubbed at pairing time), and `fingerprint`
/// is derived from the pinned static key — both display-safe, no re-sanitizing.
fn device_infos(roster: &Roster, requester_id: &DeviceId) -> Vec<DeviceInfo> {
    roster
        .entries
        .iter()
        .map(|e| DeviceInfo {
            device_id: e.device_id,
            name: e.name.clone(),
            fingerprint: fingerprint(&e.static_pubkey),
            enrolled_at: e.enrolled_at,
            last_connected_at: e.last_connected_at,
            is_self: e.device_id == *requester_id,
        })
        .collect()
}

/// Removes `device_id` from the roster and persists the shrunken set, returning
/// whether an entry was removed (`false` = already absent, an idempotent
/// no-op). The caller performs the kick (relay re-assert + live-session cancel)
/// separately — see [`assert_and_kick`] — so it can order it after any response.
async fn revoke_from_roster(
    device_id: &DeviceId,
    deps: &Arc<PeerDeps>,
) -> Result<bool, BridgeError> {
    let mut roster = deps.roster.write().await;
    if !roster.remove_by_device(device_id) {
        return Ok(false);
    }
    roster.save(&deps.roster_path)?;
    Ok(true)
}

/// Applies the revocation kick for a device already removed from the roster
/// (ADR-0021 D6): first the authoritative bridge-side cut — cancel every live
/// session that device holds — then the defense-in-depth relay re-assert of the
/// shrunken roster, which severs the device's relay connection so it cannot
/// route or reconnect. The relay step is best-effort: a dropped link needs no
/// re-assert because the relay clears a bridge's whole set on disconnect (D4).
async fn assert_and_kick(
    device_id: &DeviceId,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &AtomicU32,
) {
    cancel_live_peers(device_id, &deps.live_peers);
    let assert = {
        let roster = deps.roster.read().await;
        assert_devices_msg(next_control_id(control_seq), &roster)
    };
    let _ = send_control(outbound_tx, deps.bridge_id, &assert).await;
}

/// Drains one attached session's output receiver, forwarding each
/// [`ChannelOutput`] as a plaintext [`PeerEvent`] to the peer task (which does
/// the sealing). Exits on channel death, connection/peer teardown, or when the
/// peer task is gone.
async fn pump_output(
    mut session_output: mpsc::Receiver<ChannelOutput>,
    events: mpsc::Sender<PeerEvent>,
    peer_token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = peer_token.cancelled() => return,
            out = session_output.recv() => match out {
                Some(o) => {
                    if events.send(PeerEvent::Output(o)).await.is_err() {
                        return;
                    }
                }
                None => {
                    let _ = events.send(PeerEvent::Dead).await;
                    return;
                }
            },
        }
    }
}

/// Seals one `ChannelOutput` and forwards it as `BridgeMessage::Output`,
/// chunking `Bytes` runs so a large PTY burst never trips the Noise plaintext
/// cap; other variants seal directly.
async fn send_output(
    transport: &mut Transport,
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
    out: ChannelOutput,
) -> Result<(), ()> {
    match out {
        ChannelOutput::Bytes(bytes) => {
            for chunk in chunk_bytes(bytes) {
                send_msg(
                    transport,
                    outbound_tx,
                    src,
                    dst,
                    &BridgeMessage::Output(ChannelOutput::Bytes(chunk)),
                )
                .await?;
            }
            Ok(())
        }
        other => {
            send_msg(
                transport,
                outbound_tx,
                src,
                dst,
                &BridgeMessage::Output(other),
            )
            .await
        }
    }
}

/// Seals a [`BridgeMessage`] and enqueues it as an outbound Data frame.
/// `Err(())` means the peer should be dropped (seal or send failed).
async fn send_msg(
    transport: &mut Transport,
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
    msg: &BridgeMessage,
) -> Result<(), ()> {
    let ciphertext = transport.seal(msg).map_err(|_| ())?;
    send_frame(outbound_tx, src, dst, ciphertext).await
}

/// Wraps `payload` in a Data envelope and enqueues it on the connection's
/// outbound queue. `Err(())` means the writer is gone.
async fn send_frame(
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
    payload: Vec<u8>,
) -> Result<(), ()> {
    let frame = Envelope {
        frame_type: FrameType::Data,
        src,
        dst,
        payload,
    }
    .encode();
    outbound_tx
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ())
}

/// Wraps a [`RelayControl`] in a relay-terminated Control envelope (`dst` =
/// [`DeviceId::ZERO`]) and enqueues it. `Err(())` means the writer is gone.
async fn send_control(
    outbound_tx: &mpsc::Sender<Message>,
    bridge_id: DeviceId,
    control: &RelayControl,
) -> Result<(), ()> {
    let payload = serde_json::to_vec(control).map_err(|_| ())?;
    let frame = Envelope {
        frame_type: FrameType::Control,
        src: bridge_id,
        dst: DeviceId::ZERO,
        payload,
    }
    .encode();
    outbound_tx
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ())
}

/// Handles one wake note (#233): record the status change, and if it opens a
/// fresh `Awaiting` episode, fan an empty-payload `PushTrigger` frame out to
/// every paired device that has a push registration and no live session.
///
/// Best-effort and non-fatal by contract: a poisoned episode lock skips the
/// wake, and a full/closed outbound queue (the relay connection tearing down)
/// drops the frame silently. The wake path never errors the session path.
async fn handle_wake_note(
    note: WakeNote,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
) {
    // Discard a stale replayed `Awaiting` note (queued before a relay outage,
    // delivered only once the connection recovers): opening an episode for it
    // now could push a device for a session already resolved during the
    // outage. Do this before any episode bookkeeping, so a stale note leaves
    // no trace — a later repaint/re-attach re-notes if still awaiting (see
    // `STALE_WAKE_MAX`). Non-`Awaiting` notes always proceed: closing an
    // episode is always safe, regardless of age.
    if note.status == SessionStatus::Awaiting && note.at.elapsed() > STALE_WAKE_MAX {
        return;
    }
    // Episode de-dup under a brief lock — never held across the `.await`s below.
    let opens_episode = match deps.episodes.lock() {
        Ok(mut episodes) => episodes.note(note.key, &note.status),
        // A poisoned lock (a holder panicked) skips this wake; the tracker is
        // advisory, so losing one note only risks a missed push, never a crash.
        Err(_) => return,
    };
    if !opens_episode {
        return;
    }
    // The devices currently holding a live Noise session, keyed by their
    // roster-proven `device_id` — these see the change directly and are not
    // woken. A poisoned lock skips the wake rather than waking everyone.
    let live: HashSet<DeviceId> = match deps.live_peers.lock() {
        Ok(map) => map.keys().copied().collect(),
        Err(_) => return,
    };
    let targets = {
        let roster = deps.roster.read().await;
        wake_targets(&roster, &live)
    };
    for target in targets {
        // Fire-and-forget: a gone writer just means the connection is tearing
        // down, and the wake is best-effort (skip silently).
        let _ = send_push_trigger(outbound_tx, deps.bridge_id, target).await;
    }
}

/// Forgets `attached_session`'s wake episode (if any) and clears it, so a later
/// respawn of the same session opens a fresh `Awaiting` episode (#233). Called
/// from a peer task when its attached channel is torn down; a poisoned episode
/// lock is a no-op (the tracker is advisory).
fn forget_episode(deps: &Arc<PeerDeps>, attached_session: &mut Option<EpisodeKey>) {
    if let Some(key) = attached_session.take() {
        if let Ok(mut episodes) = deps.episodes.lock() {
            episodes.forget(&key);
        }
    }
}

/// Enqueues an empty-payload `PushTrigger` envelope addressed to `dst` (#233).
/// The relay routes it to the device's registered push channel; the payload is
/// empty by design (the relay is blind — it carries no session detail).
/// `Err(())` means the writer is gone.
async fn send_push_trigger(
    outbound_tx: &mpsc::Sender<Message>,
    src: DeviceId,
    dst: DeviceId,
) -> Result<(), ()> {
    let frame = Envelope {
        frame_type: FrameType::PushTrigger,
        src,
        dst,
        payload: vec![],
    }
    .encode();
    outbound_tx
        .send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ())
}

/// A decoded relay-terminated control reply, carrying its correlation `id`.
enum ControlReply {
    /// A `RelayControlAck`.
    Ack(u32),
    /// A `RelayControlError`.
    Error(u32),
}

/// Decodes a frame as a relay-terminated control reply addressed to this bridge,
/// or `None` if it is not one. `RelayControlError` is tried first: its `message`
/// field is absent from an ack, so an ack never mis-parses as an error, and an
/// error (which also carries `id`) is not mistaken for an ack.
fn parse_control_reply(bytes: &[u8], bridge_id: DeviceId) -> Option<ControlReply> {
    let envelope = Envelope::decode(bytes).ok()?;
    if envelope.frame_type != FrameType::Control || envelope.dst != bridge_id {
        return None;
    }
    if let Ok(err) = serde_json::from_slice::<RelayControlError>(&envelope.payload) {
        return Some(ControlReply::Error(err.id));
    }
    let ack = serde_json::from_slice::<RelayControlAck>(&envelope.payload).ok()?;
    Some(ControlReply::Ack(ack.id))
}

/// Completes the pending control waiter for a relay-terminated reply payload.
/// `RelayControlError` is tried first (see [`parse_control_reply`]).
fn route_control_reply(payload: &[u8], control_waiters: &ControlWaiters) {
    // A poisoned lock (a waiter-holder panicked) leaves acks unroutable; the
    // affected awaits then time out, which the reconnect path already tolerates.
    let Ok(mut waiters) = control_waiters.lock() else {
        return;
    };
    if let Ok(err) = serde_json::from_slice::<RelayControlError>(payload) {
        if let Some(tx) = waiters.remove(&err.id) {
            let _ = tx.send(Err(err.message));
        }
        return;
    }
    if let Ok(ack) = serde_json::from_slice::<RelayControlAck>(payload) {
        if let Some(tx) = waiters.remove(&ack.id) {
            let _ = tx.send(Ok(()));
        }
    }
}

/// Handles one [`PairingCommand`] from the desktop: opens/replaces or cancels the
/// pairing window, routes the user's confirm/reject decision into the running
/// pairing task, and applies revocation.
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd: PairingCommand,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    events: &mpsc::Sender<BridgeEvent>,
    control_seq: &Arc<AtomicU32>,
    control_waiters: &ControlWaiters,
    pairing: &mut Option<PairingWindow>,
    pairing_generation: &mut u64,
) {
    match cmd {
        PairingCommand::OpenWindow { ttl_secs, reply } => {
            let (code, rendezvous_token) = match mint_pairing_code(deps) {
                Ok(minted) => minted,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            // Open the relay's single pairing window keyed by the rendezvous
            // token embedded in the code, so the arriving device is admitted.
            let id = next_control_id(control_seq.as_ref());
            let register = RelayControl::RegisterPairing {
                id,
                token: rendezvous_token,
                ttl_secs,
            };
            // Register a waiter so the relay's ack/error is routed rather than
            // dropped. The receiver is dropped here (fire-and-forget): the relay
            // routes the arriving device even before this ack, so we surface the
            // code immediately rather than gating on it.
            let (tx, _rx) = oneshot::channel();
            if let Ok(mut waiters) = control_waiters.lock() {
                waiters.insert(id, tx);
            }
            if send_control(outbound_tx, deps.bridge_id, &register)
                .await
                .is_err()
            {
                if let Ok(mut waiters) = control_waiters.lock() {
                    waiters.remove(&id);
                }
                let _ = reply.send(Err(BridgeError::Disconnected));
                return;
            }
            let expires_at = now_secs().saturating_add(ttl_secs);
            // Record the local window (secret + deadline). Replacing an existing
            // one drops its channels, so any in-flight task exits; a fresh
            // generation guards against its stale done-signal clearing this one.
            *pairing_generation = pairing_generation.wrapping_add(1);
            *pairing = Some(PairingWindow {
                secret: code.psk,
                expires_at,
                generation: *pairing_generation,
                task: None,
            });
            let _ = events
                .send(BridgeEvent::PairingWindowOpened {
                    code: code.clone(),
                    expires_at,
                })
                .await;
            let _ = reply.send(Ok(code));
        }
        PairingCommand::CancelWindow => {
            // Drop local window state (closing any running task's channels), then
            // tell the relay to drop the routing window.
            *pairing = None;
            let id = next_control_id(control_seq.as_ref());
            let _ = send_control(
                outbound_tx,
                deps.bridge_id,
                &RelayControl::CancelPairing { id },
            )
            .await;
        }
        PairingCommand::Confirm { device_id } => {
            route_pairing_decision(pairing, PairingCtl::Confirm { device_id }).await;
        }
        PairingCommand::Reject { device_id } => {
            route_pairing_decision(pairing, PairingCtl::Reject { device_id }).await;
        }
        PairingCommand::Revoke { device_id, reply } => {
            let result = revoke_device(&device_id, deps, outbound_tx, control_seq.as_ref()).await;
            if result.is_ok() {
                let _ = events.send(BridgeEvent::RosterChanged).await;
            }
            let _ = reply.send(result);
        }
    }
}

/// Forwards a confirm/reject decision to the running pairing task, if any.
///
/// The connection loop does not know which device a running task is bound to
/// (the arrived device id is the identity preamble on the task's first frame),
/// so it forwards to whatever task is running and lets the task be the single
/// arbiter: the decision carries its target `device_id` inside the [`PairingCtl`],
/// and [`await_pairing_decision`] drops any decision whose id does not match the
/// arrived device. A stale queued decision therefore cannot cross-confirm a
/// different device after a window replacement.
async fn route_pairing_decision(pairing: &mut Option<PairingWindow>, ctl: PairingCtl) {
    if let Some(task) = pairing.as_ref().and_then(|w| w.task.as_ref()) {
        let _ = task.ctl_tx.send(ctl).await;
    }
}

/// Mints a fresh [`PairingCode`] for a new pairing window: a random rendezvous
/// token (the relay routing credential) and a random `psk`, bound to this
/// bridge's id, static public key, and relay URL. Returns the code and the
/// rendezvous token (which the caller also hands to `RegisterPairing`).
fn mint_pairing_code(deps: &Arc<PeerDeps>) -> Result<(PairingCode, String), BridgeError> {
    let mut psk = [0u8; 32];
    fill_random(&mut psk)?;
    let mut token_raw = [0u8; 32];
    fill_random(&mut token_raw)?;
    let rendezvous_token: String = token_raw.iter().map(|b| format!("{b:02x}")).collect();

    let bridge_key: [u8; 32] = deps
        .bridge_static_pub
        .clone()
        .try_into()
        .map_err(|_| BridgeError::Pairing("bridge static key is not 32 bytes".to_string()))?;

    let code = PairingCode {
        relay_url: Some(deps.relay_url.clone()),
        rendezvous_token: Some(rendezvous_token.clone()),
        mesh_addr: None,
        psk,
        bridge_id: deps.bridge_id,
        bridge_key,
        bridge_name: None,
        min_protocol: PROTOCOL_VERSION,
    };
    Ok((code, rendezvous_token))
}

/// Removes `device_id` from the roster, persists the change, and kicks the
/// device — cancelling its live session(s) bridge-side and re-asserting the
/// shrunken set to the relay (ADR-0021 D6). A device that was not paired is a
/// no-op success (idempotent unpair). Shares the roster-mutation and kick path
/// with the wire `RemoteOp::RevokeDevice` handler; this desktop-driven path has
/// no relay-response to order, so it kicks inline.
async fn revoke_device(
    device_id: &DeviceId,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &AtomicU32,
) -> Result<(), BridgeError> {
    if !revoke_from_roster(device_id, deps).await? {
        return Ok(());
    }
    assert_and_kick(device_id, deps, outbound_tx, control_seq).await;
    Ok(())
}

/// Fills `buf` with cryptographically secure random bytes from the OS CSPRNG.
fn fill_random(buf: &mut [u8]) -> Result<(), BridgeError> {
    rand::rngs::SysRng
        .try_fill_bytes(buf)
        .map_err(|e| BridgeError::Pairing(format!("could not read random bytes: {e}")))
}

/// Current Unix time in whole seconds (0 before the epoch, which never occurs).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Splits a client's first frame into its 32-byte identity preamble and the
/// trailing Noise `msg1`. `None` if the frame is too short to carry a preamble.
fn split_preamble(frame: &[u8]) -> Option<(DeviceId, &[u8])> {
    if frame.len() < DEVICE_ID_LEN {
        return None;
    }
    let (preamble, msg1) = frame.split_at(DEVICE_ID_LEN);
    let mut id = [0u8; DEVICE_ID_LEN];
    id.copy_from_slice(preamble);
    Some((DeviceId(id), msg1))
}

/// True if `url` is a WebSocket endpoint this bridge can dial. Public so a host
/// (e.g. the desktop shell) can reject an unusable `relay_url` up front, before
/// it starts a bridge task that `serve_bridge` would only fail asynchronously.
pub fn is_ws_url(url: &str) -> bool {
    url.starts_with("ws://") || url.starts_with("wss://")
}

/// Doubles `current`, capped at [`BACKOFF_MAX`].
fn next_backoff(current: Duration) -> Duration {
    current
        .checked_mul(2)
        .map_or(BACKOFF_MAX, |doubled| doubled.min(BACKOFF_MAX))
}

/// Applies "equal jitter" to a backoff `base`: keep half fixed, randomize the
/// other half, so the delay stays in `[base/2, base]` and never collapses to
/// ~0 (which would hammer a flapping relay).
fn jittered(base: Duration, rng: &mut impl rand::RngExt) -> Duration {
    let half = base / 2;
    // `half` is at most BACKOFF_MAX/2 = 15 s, whose nanos fit comfortably in a
    // u64, so this cast never truncates.
    let extra = rng.random_range(0..=half.as_nanos() as u64);
    half + Duration::from_nanos(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_projection_marks_self() {
        let roster = Roster {
            entries: vec![
                RosterEntry {
                    device_id: DeviceId([0x11; 32]),
                    static_pubkey: vec![0xaa; 32],
                    psk: [0; 32],
                    name: "a".into(),
                    enrolled_at: Some(1),
                    last_connected_at: None,
                    relay_token: "t".into(),
                    push: None,
                },
                RosterEntry {
                    device_id: DeviceId([0x22; 32]),
                    static_pubkey: vec![0xbb; 32],
                    psk: [0; 32],
                    name: "b".into(),
                    enrolled_at: Some(2),
                    last_connected_at: Some(9),
                    relay_token: "u".into(),
                    push: None,
                },
            ],
        };
        let infos = device_infos(&roster, &DeviceId([0x22; 32]));
        assert_eq!(infos.len(), 2);
        assert!(!infos[0].is_self);
        assert!(infos[1].is_self);
        assert_eq!(infos[0].fingerprint, fingerprint(&[0xaa; 32]));
        assert_eq!(infos[1].last_connected_at, Some(9));
    }

    fn entry(id: u8, name: &str) -> RosterEntry {
        RosterEntry {
            device_id: DeviceId([id; 32]),
            static_pubkey: vec![id; 32],
            psk: [0; 32],
            name: name.to_string(),
            enrolled_at: None,
            last_connected_at: None,
            relay_token: "tok".to_string(),
            push: None,
        }
    }

    #[tokio::test]
    async fn revoke_from_roster_removes_persists_and_is_idempotent() {
        // Removing a paired device drops its entry, persists the shrunken roster
        // to disk, and reports `true`; a second removal of the same id is an
        // idempotent no-op reporting `false` (the wire path still answers
        // `Revoked`, but performs no kick).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");
        let deps = Arc::new(PeerDeps {
            bridge_id: DeviceId([9u8; 32]),
            bridge_static_priv: vec![0u8; 32],
            bridge_static_pub: vec![0u8; 32],
            relay_url: "ws://test".to_string(),
            roster: Arc::new(RwLock::new(Roster {
                entries: vec![entry(0x11, "a"), entry(0x22, "b")],
            })),
            roster_path: path.clone(),
            source: Arc::new(remora_core::FakeSessionSource::new()),
            live_peers: Arc::new(Mutex::new(HashMap::new())),
            next_peer_reg: AtomicU64::new(0),
            episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
        });

        assert!(
            revoke_from_roster(&DeviceId([0x11; 32]), &deps)
                .await
                .expect("remove ok"),
            "removing a paired device reports true"
        );
        // Persisted: reloading from disk shows only the surviving device.
        let reloaded = Roster::load(&path).expect("reload roster");
        assert_eq!(reloaded.entries.len(), 1);
        assert_eq!(reloaded.entries[0].device_id, DeviceId([0x22; 32]));

        assert!(
            !revoke_from_roster(&DeviceId([0x11; 32]), &deps)
                .await
                .expect("second remove ok"),
            "removing an absent device is an idempotent no-op (false)"
        );
    }

    /// Builds a tempdir-backed `PeerDeps` seeded with `entries`, for
    /// `RegisterPushEndpoint` tests that only care about the roster and the
    /// relay re-assert, not the surrounding connection/pairing plumbing.
    fn push_test_deps(entries: Vec<RosterEntry>) -> (Arc<PeerDeps>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");
        let deps = Arc::new(PeerDeps {
            bridge_id: DeviceId([9u8; 32]),
            bridge_static_priv: vec![0u8; 32],
            bridge_static_pub: vec![0u8; 32],
            relay_url: "ws://test".to_string(),
            roster: Arc::new(RwLock::new(Roster { entries })),
            roster_path: path,
            source: Arc::new(remora_core::FakeSessionSource::new()),
            live_peers: Arc::new(Mutex::new(HashMap::new())),
            next_peer_reg: AtomicU64::new(0),
            episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
        });
        (deps, dir)
    }

    /// Drives `handle_request` for one `RegisterPushEndpoint { registration }`
    /// as `requester_id`, wiring up the plumbing args the op doesn't use
    /// (attach/events/revoke) with inert placeholders. Returns the wire
    /// result plus the `RelayControl` sent on `outbound_tx`, if any.
    async fn call_register_push(
        deps: &Arc<PeerDeps>,
        requester_id: DeviceId,
        registration: Option<PushRegistration>,
    ) -> (RemoteResult, Option<RelayControl>) {
        let (events_tx, _events_rx) = mpsc::channel::<PeerEvent>(1);
        let peer_token = CancellationToken::new();
        let mut attach_input: Option<mpsc::Sender<ChannelInput>> = None;
        let mut has_attached = false;
        let mut attached_session: Option<EpisodeKey> = None;
        let mut revoke_target: Option<DeviceId> = None;
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(4);
        let control_seq = Arc::new(AtomicU32::new(0));

        let result = handle_request(
            RemoteOp::RegisterPushEndpoint { registration },
            deps,
            &events_tx,
            &peer_token,
            &mut attach_input,
            &mut has_attached,
            &mut attached_session,
            requester_id,
            &mut revoke_target,
            &outbound_tx,
            &control_seq,
        )
        .await;

        let sent = outbound_rx.try_recv().ok().map(|msg| {
            let Message::Binary(bytes) = msg else {
                panic!("expected a binary control frame");
            };
            let envelope = Envelope::decode(&bytes).expect("decode envelope");
            serde_json::from_slice::<RelayControl>(&envelope.payload).expect("decode control")
        });

        (result, sent)
    }

    #[tokio::test]
    async fn register_push_endpoint_sets_and_reasserts() {
        // A valid `Some` registration is stored on the requester's roster
        // entry and immediately re-asserted to the relay, carrying the new
        // push registration alongside the device's routing credential.
        let device = DeviceId([0x11; 32]);
        let (deps, _dir) = push_test_deps(vec![entry(0x11, "phone")]);
        let registration = Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/t".to_string(),
        });

        let (result, sent) = call_register_push(&deps, device, registration.clone()).await;
        assert_eq!(result, RemoteResult::PushEndpointSet);

        let roster = deps.roster.read().await;
        assert_eq!(
            roster.find_by_device(&device).expect("entry present").push,
            registration
        );
        drop(roster);

        match sent.expect("an AssertDevices control message was sent") {
            RelayControl::AssertDevices { devices, .. } => {
                let d = devices
                    .iter()
                    .find(|d| d.device_id == device)
                    .expect("device present in assert");
                assert_eq!(d.push, registration, "assert carries the new registration");
            }
            other => panic!("expected AssertDevices, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_push_endpoint_none_clears() {
        // `None` clears an existing registration, and still answers
        // `PushEndpointSet` (clearing is a successful, idempotent request).
        let device = DeviceId([0x11; 32]);
        let mut seeded = entry(0x11, "phone");
        seeded.push = Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/old".to_string(),
        });
        let (deps, _dir) = push_test_deps(vec![seeded]);

        let (result, _sent) = call_register_push(&deps, device, None).await;
        assert_eq!(result, RemoteResult::PushEndpointSet);

        let roster = deps.roster.read().await;
        assert_eq!(
            roster.find_by_device(&device).expect("entry present").push,
            None
        );
    }

    #[tokio::test]
    async fn register_push_endpoint_rejects_invalid() {
        // Validation runs *before* the roster is touched (the #232
        // relay_url gotcha): a bad endpoint is refused, never stored or
        // asserted.
        let device = DeviceId([0x11; 32]);
        let (deps, _dir) = push_test_deps(vec![entry(0x11, "phone")]);
        let bad = Some(PushRegistration::UnifiedPush {
            endpoint: "file:///x".to_string(),
        });

        let (result, sent) = call_register_push(&deps, device, bad).await;
        assert!(matches!(result, RemoteResult::Error(_)));
        assert!(sent.is_none(), "an invalid endpoint is never asserted");

        let roster = deps.roster.read().await;
        assert_eq!(
            roster.find_by_device(&device).expect("entry present").push,
            None,
            "roster is unchanged by a rejected endpoint"
        );
    }

    #[tokio::test]
    async fn register_push_endpoint_targets_requester_only() {
        // Two paired devices: only the requesting device's roster entry
        // changes, never the other's.
        let requester = DeviceId([0x11; 32]);
        let other = DeviceId([0x22; 32]);
        let (deps, _dir) = push_test_deps(vec![entry(0x11, "phone"), entry(0x22, "laptop")]);
        let registration = Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/t".to_string(),
        });

        let (result, _sent) = call_register_push(&deps, requester, registration.clone()).await;
        assert_eq!(result, RemoteResult::PushEndpointSet);

        let roster = deps.roster.read().await;
        assert_eq!(
            roster
                .find_by_device(&requester)
                .expect("requester entry present")
                .push,
            registration
        );
        assert_eq!(
            roster
                .find_by_device(&other)
                .expect("other entry present")
                .push,
            None,
            "the other device's entry is untouched"
        );
    }

    #[tokio::test]
    async fn register_push_endpoint_unpaired_requester_errors() {
        // A RegisterPushEndpoint from a device id NOT in the roster is refused
        // (#233 F5a): no roster entry is created or mutated, and nothing is
        // asserted to the relay — even for an otherwise-valid endpoint.
        let (deps, _dir) = push_test_deps(vec![entry(0x11, "phone")]);
        let stranger = DeviceId([0x99; 32]);
        let registration = Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/t".to_string(),
        });

        let (result, sent) = call_register_push(&deps, stranger, registration).await;
        assert!(
            matches!(result, RemoteResult::Error(_)),
            "an unpaired requester is refused"
        );
        assert!(
            sent.is_none(),
            "an unpaired requester triggers no AssertDevices"
        );

        let roster = deps.roster.read().await;
        assert_eq!(
            roster.entries.len(),
            1,
            "no entry is created for a stranger"
        );
        assert_eq!(
            roster
                .find_by_device(&DeviceId([0x11; 32]))
                .expect("paired entry present")
                .push,
            None,
            "the paired device's entry is untouched"
        );
    }

    // --- Wake fan-out (#233) ------------------------------------------------

    /// A roster entry with a given id and optional push registration, for the
    /// wake-fan-out tests.
    fn wake_entry(id: u8, push: Option<PushRegistration>) -> RosterEntry {
        RosterEntry {
            device_id: DeviceId([id; 32]),
            static_pubkey: vec![id; 32],
            psk: [0; 32],
            relay_token: "tok".to_string(),
            name: "d".to_string(),
            enrolled_at: None,
            last_connected_at: None,
            push,
        }
    }

    fn some_push() -> Option<PushRegistration> {
        Some(PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/t".to_string(),
        })
    }

    /// Builds a `PeerDeps` seeded with `entries` in the roster and `live` device
    /// ids registered as holding a live session — the two inputs the wake
    /// fan-out reads (plus the shared episode tracker inside).
    fn wake_test_deps(
        entries: Vec<RosterEntry>,
        live: &[DeviceId],
    ) -> (Arc<PeerDeps>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let live_peers: LivePeers = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = live_peers.lock().expect("lock");
            for (i, id) in live.iter().enumerate() {
                map.entry(*id)
                    .or_default()
                    .insert(i as u64, CancellationToken::new());
            }
        }
        let deps = Arc::new(PeerDeps {
            bridge_id: DeviceId([9u8; 32]),
            bridge_static_priv: vec![0u8; 32],
            bridge_static_pub: vec![0u8; 32],
            relay_url: "ws://test".to_string(),
            roster: Arc::new(RwLock::new(Roster { entries })),
            roster_path: dir.path().join("roster.toml"),
            source: Arc::new(remora_core::FakeSessionSource::new()),
            live_peers,
            next_peer_reg: AtomicU64::new(0),
            episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
        });
        (deps, dir)
    }

    fn wake_note(project: &str, session: &str, status: SessionStatus) -> WakeNote {
        wake_note_at(project, session, status, Instant::now())
    }

    /// Like [`wake_note`] but with an explicit `at`, so tests can construct a
    /// backdated note (`Instant::now() - Duration::from_secs(..)`) to exercise
    /// the stale-`Awaiting`-discard path (#233) without sleeping.
    fn wake_note_at(project: &str, session: &str, status: SessionStatus, at: Instant) -> WakeNote {
        WakeNote {
            key: (
                ProjectId::new(project).expect("project id"),
                SessionId::new(session).expect("session id"),
            ),
            status,
            at,
        }
    }

    /// Drains every queued outbound frame, returning the `dst` of each
    /// `PushTrigger` (asserting each carries an empty payload). Non-PushTrigger
    /// frames fail the test — the wake path must emit nothing else.
    fn drain_push_triggers(outbound_rx: &mut mpsc::Receiver<Message>) -> Vec<DeviceId> {
        let mut dsts = Vec::new();
        while let Ok(msg) = outbound_rx.try_recv() {
            let Message::Binary(bytes) = msg else {
                panic!("expected a binary frame, got {msg:?}");
            };
            let env = Envelope::decode(&bytes).expect("decode envelope");
            assert_eq!(
                env.frame_type,
                FrameType::PushTrigger,
                "the wake path emits only PushTrigger frames"
            );
            assert!(env.payload.is_empty(), "PushTrigger payload is empty");
            assert_eq!(env.src, DeviceId([9u8; 32]), "framed from this bridge");
            dsts.push(env.dst);
        }
        dsts
    }

    #[tokio::test]
    async fn awaiting_wakes_only_absent_push_devices_once() {
        // Roster: A (push + offline) → woken; B (push + live) → skipped (it sees
        // the change directly); C (no push) → skipped (nowhere to wake). A
        // repeated Awaiting for the same session sends nothing (episode dedup).
        let a = DeviceId([0xa0; 32]);
        let b = DeviceId([0xb0; 32]);
        let (deps, _dir) = wake_test_deps(
            vec![
                wake_entry(0xa0, some_push()),
                wake_entry(0xb0, some_push()),
                wake_entry(0xc0, None),
            ],
            &[b],
        );
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(16);

        // First Awaiting: opens the episode, fans out exactly one PushTrigger — to A.
        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(
            drain_push_triggers(&mut outbound_rx),
            vec![a],
            "only the push-registered, offline device is woken"
        );

        // Repeated Awaiting for the same session: no duplicate wake.
        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert!(
            drain_push_triggers(&mut outbound_rx).is_empty(),
            "a session already inside its Awaiting episode is not re-woken"
        );
    }

    #[tokio::test]
    async fn non_awaiting_status_wakes_nobody() {
        // A Working/Idle/Unknown transition never fans out a wake, even with a
        // roster full of absent, push-registered devices.
        let (deps, _dir) = wake_test_deps(
            vec![wake_entry(0xa0, some_push()), wake_entry(0xb0, some_push())],
            &[],
        );
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(16);

        for status in [
            SessionStatus::Working,
            SessionStatus::Idle,
            SessionStatus::Unknown,
        ] {
            handle_wake_note(wake_note("proj", "sess", status), &deps, &outbound_tx).await;
            assert!(
                drain_push_triggers(&mut outbound_rx).is_empty(),
                "{status:?} must not wake any device"
            );
        }
    }

    #[tokio::test]
    async fn closing_and_reopening_an_episode_wakes_again() {
        // Awaiting → (some non-Awaiting close) → Awaiting fans out a second wake:
        // the second Awaiting is a fresh episode, so absent devices are re-woken.
        let a = DeviceId([0xa0; 32]);
        let (deps, _dir) = wake_test_deps(vec![wake_entry(0xa0, some_push())], &[]);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(16);

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(drain_push_triggers(&mut outbound_rx), vec![a]);

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Working),
            &deps,
            &outbound_tx,
        )
        .await;
        assert!(drain_push_triggers(&mut outbound_rx).is_empty());

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(
            drain_push_triggers(&mut outbound_rx),
            vec![a],
            "a reopened episode wakes the absent device again"
        );
    }

    #[tokio::test]
    async fn stale_awaiting_note_is_discarded_but_a_fresh_one_still_wakes() {
        // Simulates a relay outage: an `Awaiting` note queued minutes ago (older
        // than STALE_WAKE_MAX) is replayed on reconnect. It must be discarded
        // outright — no episode opened, no PushTrigger — because the session may
        // already have been resolved during the outage. Proof that no episode
        // was opened: a subsequent *fresh* `Awaiting` note for the same key still
        // fires (if the stale note had opened the episode, the fresh one would
        // be deduped into silence).
        let a = DeviceId([0xa0; 32]);
        let (deps, _dir) = wake_test_deps(vec![wake_entry(0xa0, some_push())], &[]);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(16);

        let stale_at = Instant::now() - (STALE_WAKE_MAX + Duration::from_secs(1));
        handle_wake_note(
            wake_note_at("proj", "sess", SessionStatus::Awaiting, stale_at),
            &deps,
            &outbound_tx,
        )
        .await;
        assert!(
            drain_push_triggers(&mut outbound_rx).is_empty(),
            "a stale replayed Awaiting note must not wake anyone"
        );

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(
            drain_push_triggers(&mut outbound_rx),
            vec![a],
            "a fresh Awaiting still wakes — proving the stale note opened no episode"
        );
    }

    #[tokio::test]
    async fn stale_non_awaiting_note_still_closes_an_open_episode() {
        // A non-`Awaiting` note is always processed regardless of age: closing
        // an episode is always safe and keeps de-dup state honest. Open an
        // episode with a fresh Awaiting, close it with a *stale* Working note,
        // then a fresh Awaiting must re-wake (proving the close took effect).
        let a = DeviceId([0xa0; 32]);
        let (deps, _dir) = wake_test_deps(vec![wake_entry(0xa0, some_push())], &[]);
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<Message>(16);

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(drain_push_triggers(&mut outbound_rx), vec![a]);

        let stale_at = Instant::now() - (STALE_WAKE_MAX + Duration::from_secs(1));
        handle_wake_note(
            wake_note_at("proj", "sess", SessionStatus::Working, stale_at),
            &deps,
            &outbound_tx,
        )
        .await;
        assert!(drain_push_triggers(&mut outbound_rx).is_empty());

        handle_wake_note(
            wake_note("proj", "sess", SessionStatus::Awaiting),
            &deps,
            &outbound_tx,
        )
        .await;
        assert_eq!(
            drain_push_triggers(&mut outbound_rx),
            vec![a],
            "the stale non-Awaiting note still closed the episode, so this re-wakes"
        );
    }

    #[test]
    fn note_session_status_is_non_blocking_and_drops_when_full() {
        // The public handle never blocks: a full queue (or a stopped bridge)
        // silently drops the note. Fill the bounded channel, then one more note
        // must return without panicking and without growing the queue.
        let (handle, WakeReceiver(mut rx)) = wake_channel();
        let project = ProjectId::new("proj").expect("project id");
        let session = SessionId::new("sess").expect("session id");
        for _ in 0..WAKE_QUEUE {
            handle.note_session_status(&project, &session, SessionStatus::Awaiting);
        }
        // The queue is full; this extra note is dropped, not blocked.
        handle.note_session_status(&project, &session, SessionStatus::Awaiting);
        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, WAKE_QUEUE, "the queue is bounded at WAKE_QUEUE");
    }

    #[test]
    fn cancel_live_peers_cancels_only_the_target_devices_sessions() {
        // Two live sessions for one device plus one for another: revoking the
        // first device cancels *both* of its sessions and leaves the other's
        // session untouched (D16: a device may hold several sessions at once).
        let live: LivePeers = Arc::new(Mutex::new(HashMap::new()));
        let target = DeviceId([0x11; 32]);
        let other = DeviceId([0x22; 32]);

        let t1 = CancellationToken::new();
        let t2 = CancellationToken::new();
        let t3 = CancellationToken::new();
        let _g1 = register_live_peer(&live, target, 0, t1.clone()).expect("register 1");
        let _g2 = register_live_peer(&live, target, 1, t2.clone()).expect("register 2");
        let _g3 = register_live_peer(&live, other, 2, t3.clone()).expect("register 3");

        cancel_live_peers(&target, &live);
        assert!(t1.is_cancelled(), "target session 1 kicked");
        assert!(t2.is_cancelled(), "target session 2 kicked");
        assert!(!t3.is_cancelled(), "another device's session survives");
    }

    #[test]
    fn live_peer_guard_deregisters_its_slot_on_drop() {
        // The RAII guard removes exactly its own slot on drop, and the device
        // key is dropped entirely once its last session ends — so a later
        // revocation kick never targets a session that already returned.
        let live: LivePeers = Arc::new(Mutex::new(HashMap::new()));
        let device = DeviceId([0x33; 32]);
        let g1 = register_live_peer(&live, device, 0, CancellationToken::new()).expect("reg 1");
        let g2 = register_live_peer(&live, device, 1, CancellationToken::new()).expect("reg 2");
        assert_eq!(
            live.lock().expect("lock").get(&device).map(HashMap::len),
            Some(2)
        );
        drop(g1);
        assert_eq!(
            live.lock().expect("lock").get(&device).map(HashMap::len),
            Some(1),
            "dropping one guard leaves the sibling session registered"
        );
        drop(g2);
        assert!(
            live.lock().expect("lock").get(&device).is_none(),
            "the device key is gone once its last session ends"
        );
    }

    #[test]
    fn assert_devices_msg_maps_roster() {
        // The AssertDevices payload mirrors the roster one-for-one: each entry's
        // device id, its stored `relay_token`, and its `push` registration
        // become one `AssertedDevice`.
        let roster = Roster {
            entries: vec![
                RosterEntry {
                    device_id: DeviceId([0x11; 32]),
                    static_pubkey: vec![0xaa; 32],
                    psk: [0xbb; 32],
                    name: "iPhone".to_string(),
                    enrolled_at: None,
                    last_connected_at: None,
                    relay_token: "tok-abc".to_string(),
                    push: Some(PushRegistration::UnifiedPush {
                        endpoint: "https://ntfy.sh/topic".to_string(),
                    }),
                },
                RosterEntry {
                    device_id: DeviceId([0x22; 32]),
                    static_pubkey: vec![0xcc; 32],
                    psk: [0xdd; 32],
                    name: "laptop".to_string(),
                    enrolled_at: None,
                    last_connected_at: None,
                    relay_token: "tok-def".to_string(),
                    push: None,
                },
            ],
        };
        match assert_devices_msg(7, &roster) {
            RelayControl::AssertDevices { id, devices } => {
                assert_eq!(id, 7);
                assert_eq!(devices.len(), 2);
                assert_eq!(devices[0].device_id, DeviceId([0x11; 32]));
                assert_eq!(devices[0].token, "tok-abc");
                assert_eq!(
                    devices[0].push,
                    Some(PushRegistration::UnifiedPush {
                        endpoint: "https://ntfy.sh/topic".to_string(),
                    }),
                    "the entry's push registration maps through"
                );
                assert_eq!(devices[1].push, None, "no push registration stays None");
            }
            other => panic!("expected AssertDevices, got {other:?}"),
        }
    }

    /// Minimal [`PeerDeps`] for exercising the assert phase without a socket.
    fn assert_phase_deps() -> Arc<PeerDeps> {
        Arc::new(PeerDeps {
            bridge_id: DeviceId([9u8; 32]),
            bridge_static_priv: vec![0u8; 32],
            bridge_static_pub: vec![0u8; 32],
            relay_url: "ws://test".to_string(),
            roster: Arc::new(RwLock::new(Roster::default())),
            roster_path: PathBuf::from("unused-roster.toml"),
            source: Arc::new(remora_core::FakeSessionSource::new()),
            live_peers: Arc::new(Mutex::new(HashMap::new())),
            next_peer_reg: AtomicU64::new(0),
            episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn assert_ack_wait_times_out_on_a_silent_relay() {
        // ADR-0021: the relay is untrusted. One that accepts the connection and
        // our AssertDevices but never answers must not wedge the bridge in the
        // pre-serve phase forever — the bounded wait returns TimedOut, which the
        // caller maps to the failed-connect path (reconnect with backoff).
        // Paused time auto-advances past CONTROL_ACK_TIMEOUT, keeping this fast.
        let deps = assert_phase_deps();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<Message>(8);
        let seq = AtomicU32::new(0);
        let shutdown = CancellationToken::new();
        let mut silent = futures_util::stream::pending::<Result<Message, ()>>();
        let phase =
            assert_roster_and_await_ack(&mut silent, &deps, &outbound_tx, &seq, &shutdown).await;
        assert_eq!(phase, AssertPhase::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn assert_ack_wait_completes_on_matching_ack() {
        // The happy path through the same seam: a relay that acks the assert's
        // correlation id completes the wait as Acked — the deadline must not
        // fire when the reply is already queued.
        let deps = assert_phase_deps();
        let (outbound_tx, _outbound_rx) = mpsc::channel::<Message>(8);
        let seq = AtomicU32::new(0); // first next_control_id() yields 0
        let shutdown = CancellationToken::new();
        let ack_frame = Envelope {
            frame_type: FrameType::Control,
            src: DeviceId::ZERO,
            dst: deps.bridge_id,
            payload: serde_json::to_vec(&RelayControlAck { id: 0 }).expect("encode ack"),
        }
        .encode();
        let mut replies =
            futures_util::stream::iter(vec![Ok::<_, ()>(Message::Binary(ack_frame.into()))]);
        let phase =
            assert_roster_and_await_ack(&mut replies, &deps, &outbound_tx, &seq, &shutdown).await;
        assert_eq!(phase, AssertPhase::Acked);
    }

    #[test]
    fn is_ws_url_accepts_ws_and_wss_only() {
        assert!(is_ws_url("ws://relay.example/ws"));
        assert!(is_ws_url("wss://relay.example/ws"));
        assert!(!is_ws_url("http://relay.example"));
        assert!(!is_ws_url("relay.example"));
        assert!(!is_ws_url(""));
    }

    #[test]
    fn next_backoff_doubles_and_caps_at_max() {
        // Doubling sequence from the floor, capping at 30 s and staying there.
        let mut d = BACKOFF_MIN;
        let expected = [
            Duration::from_millis(400),
            Duration::from_millis(800),
            Duration::from_millis(1600),
            Duration::from_millis(3200),
            Duration::from_millis(6400),
            Duration::from_millis(12800),
            Duration::from_millis(25600),
            BACKOFF_MAX, // 51200ms would overshoot -> capped
            BACKOFF_MAX, // stays pinned at the cap
        ];
        for want in expected {
            d = next_backoff(d);
            assert_eq!(d, want);
        }
    }

    #[test]
    fn jittered_stays_within_half_to_full() {
        let mut rng = rand::rng();
        for base in [BACKOFF_MIN, Duration::from_secs(5), BACKOFF_MAX] {
            for _ in 0..1000 {
                let d = jittered(base, &mut rng);
                assert!(d >= base / 2, "jitter below floor: {d:?} for {base:?}");
                assert!(d <= base, "jitter above base: {d:?} for {base:?}");
            }
        }
    }

    #[test]
    fn peer_registry_reaps_slot_on_matching_done_signal() {
        // A peer's done-signal (its stamped generation) reaps its slot, so the
        // map does not grow with every short-lived, immediately-exiting peer.
        let mut reg = PeerRegistry::new();
        let src = DeviceId([1u8; DEVICE_ID_LEN]);
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        let generation = reg.insert(src, tx);
        assert_eq!(reg.len(), 1);
        assert!(reg.get(&src).is_some());

        reg.remove_if_generation(&src, generation);
        assert_eq!(reg.len(), 0, "matching done-signal must reap the slot");
        assert!(reg.get(&src).is_none());
    }

    #[test]
    fn peer_registry_generation_guard_keeps_reconnected_peer() {
        // A client reconnecting on the same routing id installs a NEWER task
        // (higher generation). The OLD task's late done-signal must be a no-op —
        // it must never evict the live replacement.
        let mut reg = PeerRegistry::new();
        let src = DeviceId([2u8; DEVICE_ID_LEN]);

        let (tx_old, _rx_old) = mpsc::channel::<Vec<u8>>(1);
        let gen_old = reg.insert(src, tx_old);

        // Same src reconnects: a newer task replaces the slot.
        let (tx_new, _rx_new) = mpsc::channel::<Vec<u8>>(1);
        let gen_new = reg.insert(src, tx_new);
        assert_ne!(gen_old, gen_new);

        // The stale done-signal for the old generation must NOT evict gen_new.
        reg.remove_if_generation(&src, gen_old);
        assert_eq!(
            reg.len(),
            1,
            "reconnected peer must survive the stale done-signal"
        );
        assert!(reg.get(&src).is_some());

        // The live generation still reaps normally when its own task exits.
        reg.remove_if_generation(&src, gen_new);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn peer_registry_generations_are_monotonic() {
        // Distinct peers get distinct, increasing generations so done-signals
        // are unambiguous across the connection's lifetime.
        let mut reg = PeerRegistry::new();
        let a = DeviceId([3u8; DEVICE_ID_LEN]);
        let b = DeviceId([4u8; DEVICE_ID_LEN]);
        let (tx_a, _rx_a) = mpsc::channel::<Vec<u8>>(1);
        let (tx_b, _rx_b) = mpsc::channel::<Vec<u8>>(1);
        assert_eq!(reg.insert(a, tx_a), 0);
        assert_eq!(reg.insert(b, tx_b), 1);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn peer_registry_remove_drops_slot_unconditionally() {
        // The `Closed`-sender fast path removes the dead slot regardless of
        // generation (no newer task can exist at that instant).
        let mut reg = PeerRegistry::new();
        let src = DeviceId([5u8; DEVICE_ID_LEN]);
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        reg.insert(src, tx);
        reg.remove(&src);
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn recv_hello_frame_times_out_when_no_hello_arrives() {
        // A peer that completed the handshake but never sends its E2E hello must
        // be dropped once the deadline elapses, so its task returns and the
        // DoneGuard reaps the registry slot (#231). The sender is held open so
        // `recv` would otherwise block forever — only the timeout ends it. A tiny
        // real deadline keeps the test fast; production uses PEER_HELLO_TIMEOUT.
        let (_tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let token = CancellationToken::new();
        let got = recv_hello_frame(&mut rx, &token, Duration::from_millis(30)).await;
        assert!(got.is_none(), "a stalled hello must time out to None");
    }

    #[tokio::test]
    async fn recv_hello_frame_returns_a_delivered_frame() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        tx.send(vec![1, 2, 3]).await.expect("send hello frame");
        let token = CancellationToken::new();
        let got = recv_hello_frame(&mut rx, &token, Duration::from_secs(10)).await;
        assert_eq!(
            got,
            Some(vec![1, 2, 3]),
            "a delivered frame must pass through"
        );
    }

    #[tokio::test]
    async fn recv_hello_frame_returns_none_on_cancellation() {
        let (_tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let token = CancellationToken::new();
        token.cancel();
        let got = recv_hello_frame(&mut rx, &token, Duration::from_secs(10)).await;
        assert!(got.is_none(), "a cancelled connection drops the peer");
    }

    #[test]
    fn split_preamble_extracts_id_and_msg1() {
        // Too short: no preamble.
        assert!(split_preamble(&[0u8; DEVICE_ID_LEN - 1]).is_none());

        // Exactly the preamble: id from the bytes, empty msg1.
        let exact = [0xabu8; DEVICE_ID_LEN];
        let (id, msg1) = split_preamble(&exact).expect("exact preamble");
        assert_eq!(id, DeviceId([0xab; DEVICE_ID_LEN]));
        assert!(msg1.is_empty());

        // Preamble + trailing msg1: id is the first 32 bytes, rest is msg1.
        let mut frame = vec![0x11u8; DEVICE_ID_LEN];
        frame.extend_from_slice(&[0x22, 0x33, 0x44]);
        let (id, msg1) = split_preamble(&frame).expect("with msg1");
        assert_eq!(id, DeviceId([0x11; DEVICE_ID_LEN]));
        assert_eq!(msg1, &[0x22, 0x33, 0x44]);
    }

    #[test]
    fn confirm_rejects_duplicate_device_id() {
        // The confirm path calls `roster.contains_device(pending_id)` before
        // minting anything and sends `Rejected { DuplicateId }` on a hit, so a
        // broken/malicious client cannot shadow or resurrect another device's id
        // (ADR-0021 D3). Substantive end-to-end coverage is Task 14's loopback;
        // this pins the guard the responder relies on.
        let roster = Roster {
            entries: vec![RosterEntry {
                device_id: DeviceId([0x11; 32]),
                static_pubkey: vec![0xaa; 32],
                psk: [0xbb; 32],
                name: "existing".to_string(),
                enrolled_at: None,
                last_connected_at: None,
                relay_token: "t".to_string(),
                push: None,
            }],
        };
        // A pending device claiming the same id must be refused.
        assert!(roster.contains_device(&DeviceId([0x11; 32])));
        // A fresh id is not a duplicate and would be granted.
        assert!(!roster.contains_device(&DeviceId([0x22; 32])));
    }

    #[test]
    fn client_version_gate_requires_minimum() {
        // A device below the bridge's minimum is refused (VersionMismatch); one at
        // or above it is accepted.
        assert!(!client_version_ok(PROTOCOL_VERSION - 1));
        assert!(client_version_ok(PROTOCOL_VERSION));
        assert!(client_version_ok(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn device_name_is_control_stripped_and_bounded() {
        // The untrusted device name is scrubbed of control/escape bytes (the ESC
        // and newline here) and length-capped before it reaches the confirm
        // dialog — the same call the responder makes. The bare CSI *text* left
        // behind after the ESC is stripped is inert (no terminal re-interprets it).
        let cleaned = sanitize("iP\x1bhone\n", MAX_DEVICE_NAME_CHARS).into_string();
        assert_eq!(
            cleaned, "iPhone",
            "the ESC and newline control bytes are dropped"
        );
        assert!(
            !cleaned.contains('\x1b') && !cleaned.contains('\n'),
            "no control bytes survive"
        );

        let long: String = "x".repeat(MAX_DEVICE_NAME_CHARS * 2);
        let capped = sanitize(&long, MAX_DEVICE_NAME_CHARS).into_string();
        assert!(
            capped.chars().count() <= MAX_DEVICE_NAME_CHARS,
            "name is capped to the bound"
        );
    }

    #[test]
    fn minted_credentials_are_fresh_and_sized() {
        // The device token is 64 hex chars (32 bytes); the session PSK is 32
        // bytes; both are freshly random, so two mints differ.
        let t1 = next_device_token().expect("token");
        let t2 = next_device_token().expect("token");
        assert_eq!(t1.len(), 64, "32 bytes -> 64 hex chars");
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t1, t2, "each token is freshly random");

        let p1 = next_session_psk().expect("psk");
        let p2 = next_session_psk().expect("psk");
        assert_eq!(p1.len(), 32);
        assert_ne!(p1, p2, "each session psk is freshly random");
    }

    /// Mints a fresh X25519 static keypair the same way the identity layer does.
    fn test_keypair() -> snow::Keypair {
        snow::Builder::new(
            crate::noise::NOISE_PATTERN
                .parse()
                .expect("noise params parse"),
        )
        .generate_keypair()
        .expect("generate keypair")
    }

    /// [`PeerDeps`] with a real bridge keypair, for driving [`run_pairing`].
    fn pairing_deps(bridge: &snow::Keypair) -> Arc<PeerDeps> {
        Arc::new(PeerDeps {
            bridge_id: DeviceId([9u8; 32]),
            bridge_static_priv: bridge.private.clone(),
            bridge_static_pub: bridge.public.clone(),
            relay_url: "ws://test".to_string(),
            roster: Arc::new(RwLock::new(Roster::default())),
            roster_path: PathBuf::from("unused-roster.toml"),
            source: Arc::new(remora_core::FakeSessionSource::new()),
            live_peers: Arc::new(Mutex::new(HashMap::new())),
            next_peer_reg: AtomicU64::new(0),
            episodes: Arc::new(Mutex::new(WakeEpisodes::new(WAKE_EPISODE_CAP))),
        })
    }

    /// The channel bundle a spawned [`run_pairing`] is driven through in tests.
    struct PairingHarness {
        outbound_rx: mpsc::Receiver<Message>,
        events_rx: mpsc::Receiver<BridgeEvent>,
        frame_tx: mpsc::Sender<Vec<u8>>,
        ctl_tx: mpsc::Sender<PairingCtl>,
        done_rx: mpsc::UnboundedReceiver<(u64, PairingExit)>,
    }

    /// Spawns [`run_pairing`] for `first_frame` under generation 7 and a 60 s
    /// deadline, returning the harness the test drives it through.
    fn spawn_pairing(
        deps: &Arc<PeerDeps>,
        src: DeviceId,
        first_frame: Vec<u8>,
        secret: [u8; 32],
    ) -> PairingHarness {
        let (outbound_tx, outbound_rx) = mpsc::channel::<Message>(16);
        let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(16);
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(16);
        let (ctl_tx, ctl_rx) = mpsc::channel::<PairingCtl>(4);
        let (done_tx, done_rx) = mpsc::unbounded_channel::<(u64, PairingExit)>();
        tokio::spawn(run_pairing(
            PairingParams {
                src,
                first_frame,
                secret,
                expires_at: now_secs() + 60,
                generation: 7,
            },
            frame_rx,
            ctl_rx,
            deps.clone(),
            outbound_tx,
            Arc::new(AtomicU32::new(0)),
            Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            done_tx,
            CancellationToken::new(),
        ));
        PairingHarness {
            outbound_rx,
            events_rx,
            frame_tx,
            ctl_tx,
            done_rx,
        }
    }

    /// Receives the next outbound Pairing envelope's payload, asserting routing.
    async fn recv_pairing_payload(
        outbound_rx: &mut mpsc::Receiver<Message>,
        expect_dst: DeviceId,
    ) -> Vec<u8> {
        let msg = outbound_rx.recv().await.expect("an outbound frame");
        let Message::Binary(bytes) = msg else {
            panic!("expected a binary frame, got {msg:?}");
        };
        let env = Envelope::decode(bytes.as_ref()).expect("decode envelope");
        assert_eq!(env.frame_type, FrameType::Pairing, "pairing frame type");
        assert_eq!(env.dst, expect_dst, "addressed to the device's src");
        env.payload
    }

    /// Drives the device half of the pairing handshake against a spawned
    /// [`run_pairing`], returning the device's established transport.
    async fn complete_device_handshake(
        init: &mut Option<Handshake>,
        harness: &mut PairingHarness,
        src: DeviceId,
    ) -> Transport {
        let msg2 = recv_pairing_payload(&mut harness.outbound_rx, src).await;
        let mut hs = init.take().expect("initiator present");
        hs.read_message(&msg2).expect("read msg2");
        let (transport, _) = hs.into_transport().expect("device transport");
        transport
    }

    #[test]
    fn handle_pairing_done_consumed_drops_window_released_frees_slot() {
        let window = |task: Option<PairingTaskHandle>| PairingWindow {
            secret: [1; 32],
            expires_at: now_secs() + 60,
            generation: 5,
            task,
        };
        let task_handle = || {
            let (frame_tx, _frame_rx) = mpsc::channel::<Vec<u8>>(1);
            let (ctl_tx, _ctl_rx) = mpsc::channel::<PairingCtl>(1);
            PairingTaskHandle {
                src: DeviceId([2; 32]),
                frame_tx,
                ctl_tx,
            }
        };

        // Consumed: the whole window is dropped.
        let mut pairing = Some(window(Some(task_handle())));
        handle_pairing_done(&mut pairing, 5, PairingExit::ConsumedWindow);
        assert!(pairing.is_none(), "a consumed window is gone");

        // Released: the window survives with the slot freed for a fresh attempt.
        let mut pairing = Some(window(Some(task_handle())));
        handle_pairing_done(&mut pairing, 5, PairingExit::ReleasedSlot);
        let w = pairing.as_ref().expect("window survives a released slot");
        assert!(w.task.is_none(), "the in-flight slot is freed");

        // Generation guard: a stale signal (older window) is a no-op either way.
        let mut pairing = Some(window(Some(task_handle())));
        handle_pairing_done(&mut pairing, 4, PairingExit::ConsumedWindow);
        assert!(pairing.is_some(), "stale consumed signal must not drop");
        handle_pairing_done(&mut pairing, 4, PairingExit::ReleasedSlot);
        assert!(
            pairing.as_ref().is_some_and(|w| w.task.is_some()),
            "stale released signal must not free the live slot"
        );
    }

    #[tokio::test]
    async fn garbage_first_frame_releases_slot_and_admits_a_fresh_handshake() {
        // Reviewer finding (fix round 1): a corrupted/garbage first frame — or a
        // wrong-PSK probe that reached the rendezvous window — must NOT burn the
        // window. The failed task signals ReleasedSlot; the loop frees the slot;
        // the next Pairing frame from a fresh src starts a new handshake.
        let bridge = test_keypair();
        let deps = pairing_deps(&bridge);
        let psk = [0x42u8; 32];
        let src_a = DeviceId([0xa0; 32]);

        // 32-byte preamble + garbage msg1: the responder handshake read fails.
        let mut harness = spawn_pairing(&deps, src_a, vec![0u8; 48], psk);
        assert_eq!(
            harness.done_rx.recv().await,
            Some((7, PairingExit::ReleasedSlot)),
            "a pre-Pending handshake failure must release the slot"
        );

        // Apply the signal the way the connection loop does: window survives.
        let mut pairing = Some(PairingWindow {
            secret: psk,
            expires_at: now_secs() + 60,
            generation: 7,
            task: None,
        });
        handle_pairing_done(&mut pairing, 7, PairingExit::ReleasedSlot);
        assert!(pairing.is_some(), "the window must survive the failure");

        // A fresh device's first frame is admitted: dispatch spawns a new task.
        let (outbound_tx, _outbound_rx) = mpsc::channel::<Message>(16);
        let (events_tx, _events_rx) = mpsc::channel::<BridgeEvent>(16);
        let (done_tx, _done_rx) = mpsc::unbounded_channel::<(u64, PairingExit)>();
        let src_b = DeviceId([0xb0; 32]);
        dispatch_pairing_frame(
            src_b,
            vec![0u8; 48],
            &mut pairing,
            &deps,
            &outbound_tx,
            &Arc::new(AtomicU32::new(0)),
            &Arc::new(Mutex::new(HashMap::new())),
            &events_tx,
            &done_tx,
            &CancellationToken::new(),
        );
        let w = pairing.as_ref().expect("window still open");
        let task = w.task.as_ref().expect("a fresh handshake was admitted");
        assert_eq!(task.src, src_b, "the new task binds to the fresh src");
    }

    #[tokio::test]
    async fn version_mismatch_rejects_and_releases_the_slot() {
        // A too-old device gets a wire Rejected{VersionMismatch}, but since the
        // arrival never became user-visible the slot is released — a mixed fleet
        // scanning the same QR must not dead-end the window.
        let bridge = test_keypair();
        let device = test_keypair();
        let deps = pairing_deps(&bridge);
        let psk = [0x42u8; 32];
        let src = DeviceId([0xd0; 32]);
        let device_id = DeviceId([0xd1; 32]);

        let pro = prologue(HandshakeKind::Pairing, &device_id, &src, &deps.bridge_id);
        let mut init = Some(
            Handshake::initiator(&device.private, &bridge.public, &psk, &pro).expect("initiator"),
        );
        let msg1 = init
            .as_mut()
            .expect("initiator present")
            .write_message(&[])
            .expect("msg1");
        let mut first = device_id.0.to_vec();
        first.extend_from_slice(&msg1);

        let mut harness = spawn_pairing(&deps, src, first, psk);
        let mut transport = complete_device_handshake(&mut init, &mut harness, src).await;

        let hello = transport
            .seal(&PairingClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION - 1,
                device_name: "old phone".to_string(),
            })
            .expect("seal hello");
        harness.frame_tx.send(hello).await.expect("send hello");

        let payload = recv_pairing_payload(&mut harness.outbound_rx, src).await;
        let reply: PairingBridgeMsg = transport.open(&payload).expect("open reply");
        assert!(
            matches!(
                reply,
                PairingBridgeMsg::Rejected {
                    reason: PairingRejectReason::VersionMismatch {
                        bridge_min: PROTOCOL_VERSION
                    }
                }
            ),
            "got {reply:?}"
        );
        assert_eq!(
            harness.done_rx.recv().await,
            Some((7, PairingExit::ReleasedSlot)),
            "a version-mismatch arrival never became user-visible: release"
        );
    }

    #[tokio::test]
    async fn reject_after_pending_consumes_the_window() {
        // Once Pending is on the wire the arrival is user-visible: whatever the
        // outcome (here: user Reject), the window is consumed — D3's single
        // completed handshake per window.
        let bridge = test_keypair();
        let device = test_keypair();
        let deps = pairing_deps(&bridge);
        let psk = [0x42u8; 32];
        let src = DeviceId([0xd0; 32]);
        let device_id = DeviceId([0xd1; 32]);

        let pro = prologue(HandshakeKind::Pairing, &device_id, &src, &deps.bridge_id);
        let mut init = Some(
            Handshake::initiator(&device.private, &bridge.public, &psk, &pro).expect("initiator"),
        );
        let msg1 = init
            .as_mut()
            .expect("initiator present")
            .write_message(&[])
            .expect("msg1");
        let mut first = device_id.0.to_vec();
        first.extend_from_slice(&msg1);

        let mut harness = spawn_pairing(&deps, src, first, psk);
        let mut transport = complete_device_handshake(&mut init, &mut harness, src).await;

        let hello = transport
            .seal(&PairingClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "phone".to_string(),
            })
            .expect("seal hello");
        harness.frame_tx.send(hello).await.expect("send hello");

        // The arrival surfaces to the desktop, then Pending reaches the device.
        match harness.events_rx.recv().await {
            Some(BridgeEvent::PairingDeviceArrived {
                device_id: id,
                name,
                ..
            }) => {
                assert_eq!(id, device_id);
                assert_eq!(name, "phone");
            }
            other => panic!("expected PairingDeviceArrived, got {other:?}"),
        }
        let payload = recv_pairing_payload(&mut harness.outbound_rx, src).await;
        let pending: PairingBridgeMsg = transport.open(&payload).expect("open pending");
        assert_eq!(pending, PairingBridgeMsg::Pending);

        // The user rejects: wire Rejected{UserRejected}, Rejected event, consumed.
        harness
            .ctl_tx
            .send(PairingCtl::Reject { device_id })
            .await
            .expect("send reject");
        let payload = recv_pairing_payload(&mut harness.outbound_rx, src).await;
        let rejected: PairingBridgeMsg = transport.open(&payload).expect("open rejected");
        assert!(
            matches!(
                rejected,
                PairingBridgeMsg::Rejected {
                    reason: PairingRejectReason::UserRejected
                }
            ),
            "got {rejected:?}"
        );
        match harness.events_rx.recv().await {
            Some(BridgeEvent::PairingResult(PairingOutcome::Rejected { device_id: id })) => {
                assert_eq!(id, device_id);
            }
            other => panic!("expected PairingResult(Rejected), got {other:?}"),
        }
        assert_eq!(
            harness.done_rx.recv().await,
            Some((7, PairingExit::ConsumedWindow)),
            "a post-Pending outcome must consume the window"
        );
    }

    #[tokio::test]
    async fn stale_confirm_for_a_different_device_is_ignored() {
        // Reviewer finding: a decision must bind to the device that actually
        // arrived. A `Confirm` naming a DIFFERENT device (a stale approval still
        // queued on the ctl channel from a replaced window) must be dropped — it
        // must NOT enroll the device that arrived. Only the matching `Confirm`
        // advances the ceremony.
        let bridge = test_keypair();
        let device = test_keypair();
        let deps = pairing_deps(&bridge);
        let psk = [0x42u8; 32];
        let src = DeviceId([0xd0; 32]);
        let device_id = DeviceId([0xd1; 32]);

        let pro = prologue(HandshakeKind::Pairing, &device_id, &src, &deps.bridge_id);
        let mut init = Some(
            Handshake::initiator(&device.private, &bridge.public, &psk, &pro).expect("initiator"),
        );
        let msg1 = init
            .as_mut()
            .expect("initiator present")
            .write_message(&[])
            .expect("msg1");
        let mut first = device_id.0.to_vec();
        first.extend_from_slice(&msg1);

        let mut harness = spawn_pairing(&deps, src, first, psk);
        let mut transport = complete_device_handshake(&mut init, &mut harness, src).await;

        let hello = transport
            .seal(&PairingClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "phone".to_string(),
            })
            .expect("seal hello");
        harness.frame_tx.send(hello).await.expect("send hello");

        // Drain the arrival event and the Pending frame: the task is now awaiting.
        assert!(matches!(
            harness.events_rx.recv().await,
            Some(BridgeEvent::PairingDeviceArrived { .. })
        ));
        let payload = recv_pairing_payload(&mut harness.outbound_rx, src).await;
        let pending: PairingBridgeMsg = transport.open(&payload).expect("open pending");
        assert_eq!(pending, PairingBridgeMsg::Pending);

        // A stale Confirm for a DIFFERENT device is dropped: the task keeps
        // awaiting, so nothing is sent and the task does not finish.
        let other = DeviceId([0xee; 32]);
        harness
            .ctl_tx
            .send(PairingCtl::Confirm { device_id: other })
            .await
            .expect("send stale confirm");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), harness.outbound_rx.recv())
                .await
                .is_err(),
            "a stale confirm for a different device must not advance the ceremony"
        );
        assert!(
            harness.done_rx.try_recv().is_err(),
            "the task must still be pending after a mismatched confirm"
        );

        // The matching Confirm advances the ceremony: the task asserts the pending
        // credential to the relay (the first outbound Control frame after Confirm).
        harness
            .ctl_tx
            .send(PairingCtl::Confirm { device_id })
            .await
            .expect("send matching confirm");
        let msg = harness.outbound_rx.recv().await.expect("an outbound frame");
        let Message::Binary(bytes) = msg else {
            panic!("expected a binary frame, got {msg:?}");
        };
        let env = Envelope::decode(bytes.as_ref()).expect("decode envelope");
        assert_eq!(
            env.frame_type,
            FrameType::Control,
            "the matching confirm drives the assert-before-grant control frame"
        );
        let control: RelayControl =
            serde_json::from_slice(&env.payload).expect("decode relay control");
        match control {
            RelayControl::AssertDevices { devices, .. } => {
                assert!(
                    devices.iter().any(|d| d.device_id == device_id),
                    "the arrived device is asserted as the pending credential"
                );
            }
            other => panic!("expected AssertDevices, got {other:?}"),
        }
    }

    #[test]
    fn deadline_from_now_saturates_at_zero_when_past() {
        // A window already past its deadline yields a zero duration, so the
        // pairing task's deadline branch fires immediately rather than underflow.
        assert_eq!(deadline_from_now(0), Duration::ZERO);
        // A future deadline yields a positive, bounded remaining duration.
        let future = now_secs().saturating_add(60);
        let remaining = deadline_from_now(future);
        assert!(remaining > Duration::ZERO);
        assert!(remaining <= Duration::from_secs(60));
    }
}
