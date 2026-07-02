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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::Stream;
use futures_util::{SinkExt as _, StreamExt as _};
use rand::TryRngCore as _;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use remora_core::SessionSource;
use remora_protocol::{
    AssertedDevice, BridgeMessage, ChannelInput, ChannelOutput, ClientMessage, DeviceId, Envelope,
    FrameType, HelloRole, PairingCode, RelayControl, RelayControlAck, RelayControlError,
    RelayHello, RemoteOp, RemoteResult, WireError, PROTOCOL_VERSION,
};

use crate::identity::{BridgeIdentity, IdentityError, Roster, RosterEntry};
use crate::noise::{chunk_bytes, prologue, Handshake, HandshakeKind, Transport};
use crate::wire_error::map_source_error;

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

/// How long a peer may take to send its E2E [`ClientMessage::Hello`] after the
/// Noise handshake completes. A paired client that finishes the handshake then
/// stalls would otherwise pin its peer task and [`PeerRegistry`] slot forever;
/// on expiry the peer task returns and its [`DoneGuard`] reaps the slot (#231).
const PEER_HELLO_TIMEOUT: Duration = Duration::from_secs(10);

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
/// the pairing ceremony and roster changes (ADR-0021). The pairing-window and
/// confirm/reject responder behaviour lands with #232's ceremony work (Task 11);
/// this task wires the channel, the roster assertion, and revocation.
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
    /// The user confirmed the arrived device's fingerprint (Task 11 responder).
    Confirm { device_id: DeviceId },
    /// The user rejected the arrived device (Task 11 responder).
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
    });

    // Correlation ids for relay control requests, monotonic across reconnects so
    // a late reply from a dropped connection can never be mistaken for a fresh
    // request's ack.
    let control_seq = AtomicU32::new(0);

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
    control_seq: &AtomicU32,
    commands: &mut mpsc::Receiver<PairingCommand>,
    events: &mpsc::Sender<BridgeEvent>,
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
    if let Some(outcome) =
        match assert_roster_and_await_ack(&mut stream, deps, &outbound_tx, control_seq, shutdown)
            .await
        {
            AssertPhase::Acked => None,
            AssertPhase::Shutdown => Some(ConnOutcome::Shutdown),
            // A relay that refuses our assertion is a config-level problem; grow the
            // backoff rather than hammer a re-assert that will just re-fail.
            AssertPhase::Rejected => Some(ConnOutcome::ConnectFailed),
            AssertPhase::Disconnected => Some(ConnOutcome::Disconnected),
        }
    {
        conn_token.cancel();
        drop(outbound_tx);
        writer.abort();
        return outcome;
    }

    let mut peers = PeerRegistry::new();
    // Pending relay control requests (`RegisterPairing`/`CancelPairing`) awaiting
    // their `RelayControlAck`/`RelayControlError`, keyed by correlation id. The
    // read loop completes each waiter when its reply lands (see [`dispatch_inbound`]).
    let mut control_waiters: HashMap<u32, oneshot::Sender<Result<(), String>>> = HashMap::new();
    // Once the command channel closes (the desktop dropped its sender) we stop
    // selecting on it so a closed channel does not spin the loop.
    let mut commands_open = true;
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
            cmd = commands.recv(), if commands_open => match cmd {
                Some(cmd) => {
                    handle_command(
                        cmd,
                        deps,
                        &outbound_tx,
                        events,
                        control_seq,
                        &mut control_waiters,
                    )
                    .await;
                }
                // Desktop dropped the command sender: no more commands will come.
                None => commands_open = false,
            },
            inbound = stream.next() => match inbound {
                None => break ConnOutcome::Disconnected,
                Some(Err(_)) => break ConnOutcome::Disconnected,
                Some(Ok(Message::Binary(bytes))) => {
                    dispatch_inbound(
                        bytes.as_ref(),
                        &mut peers,
                        &mut control_waiters,
                        deps,
                        &outbound_tx,
                        &done_tx,
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

    // Tear down: cancel all peers, drop their inbound queues, stop the writer.
    conn_token.cancel();
    drop(peers);
    drop(done_tx);
    drop(outbound_tx);
    writer.abort();
    outcome
}

/// The outcome of the assert-before-serve phase.
enum AssertPhase {
    /// The relay acked our `AssertDevices`; proceed to serve.
    Acked,
    /// The relay rejected the assertion (`RelayControlError`).
    Rejected,
    /// The connection dropped before the ack arrived.
    Disconnected,
    /// `shutdown` fired while awaiting the ack.
    Shutdown,
}

/// Sends `AssertDevices` for the current roster and reads inbound frames until
/// the matching `RelayControlAck` (or error) arrives. Generic over the stream so
/// the reply-pump is exercised without a live socket in tests.
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
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return AssertPhase::Shutdown,
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
fn dispatch_inbound(
    bytes: &[u8],
    peers: &mut PeerRegistry,
    control_waiters: &mut HashMap<u32, oneshot::Sender<Result<(), String>>>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    done_tx: &mpsc::UnboundedSender<(DeviceId, u64)>,
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
            conn_token.clone(),
        ));
    }
}

/// One client peer's whole lifecycle: Noise handshake, E2E hello, then serve.
async fn run_peer(
    routing_id: DeviceId,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    done_tx: mpsc::UnboundedSender<(DeviceId, u64)>,
    generation: u64,
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
    let Some(transport) =
        handshake(&routing_id, &mut frame_rx, &deps, &outbound_tx, &conn_token).await
    else {
        return; // any handshake / auth failure drops just this peer
    };
    serve_peer(
        routing_id,
        frame_rx,
        transport,
        deps,
        outbound_tx,
        conn_token,
    )
    .await;
}

/// Drives the responder side of the Noise handshake from the peer's first
/// frame, verifying the authenticated static against the roster. Returns the
/// established [`Transport`], or `None` to drop the peer.
async fn handshake(
    routing_id: &DeviceId,
    frame_rx: &mut mpsc::Receiver<Vec<u8>>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    conn_token: &CancellationToken,
) -> Option<Transport> {
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
    Some(transport)
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
async fn serve_peer(
    routing_id: DeviceId,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    mut transport: Transport,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    conn_token: CancellationToken,
) {
    let bridge_id = deps.bridge_id;

    // A peer-scoped token (child of the connection token) so returning from
    // this task — for any reason — stops the peer's pump task promptly.
    let peer_token = conn_token.child_token();
    let _guard = CancelGuard(peer_token.clone());

    // --- E2E hello: the client's first application message must be Hello. A
    // paired client that completes the handshake but never sends it is dropped
    // once PEER_HELLO_TIMEOUT elapses, so the peer task + registry slot are
    // reaped instead of pinned forever (#231). ---
    let Some(hello_frame) = recv_hello_frame(&mut frame_rx, &conn_token, PEER_HELLO_TIMEOUT).await
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

    // --- Serve. `transport` is sealed/opened only here, so nonce order is the
    // send order. The peer's attached PTY stream arrives as plaintext
    // `PeerEvent`s over `events_rx`, which is always present (no `Option` in
    // the select), sidestepping any borrow tangle with the attach state. ---
    let (events_tx, mut events_rx) = mpsc::channel::<PeerEvent>(PEER_EVENT_QUEUE);
    let mut attach_input: Option<mpsc::Sender<ChannelInput>> = None;
    let mut has_attached = false;

    loop {
        tokio::select! {
            _ = conn_token.cancelled() => return,
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
                        let result = handle_request(
                            op,
                            &deps,
                            &events_tx,
                            &peer_token,
                            &mut attach_input,
                            &mut has_attached,
                        )
                        .await;
                        if send_msg(
                            &mut transport,
                            &outbound_tx,
                            bridge_id,
                            routing_id,
                            &BridgeMessage::Response { id, result },
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                    ClientMessage::Input(input) => {
                        if let Some(tx) = &attach_input {
                            if tx.send(input).await.is_err() {
                                // The channel died mid-send: report it and clear
                                // the attach state.
                                attach_input = None;
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
async fn handle_request(
    op: RemoteOp,
    deps: &Arc<PeerDeps>,
    events_tx: &mpsc::Sender<PeerEvent>,
    peer_token: &CancellationToken,
    attach_input: &mut Option<mpsc::Sender<ChannelInput>>,
    has_attached: &mut bool,
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
        // `RemoteOp` is `#[non_exhaustive]`: a client speaking a newer protocol
        // could ask for an op this bridge predates. Refuse it explicitly rather
        // than dropping the peer, so the client gets a typed answer.
        _ => RemoteResult::Error(WireError::Transport {
            message: "unsupported operation".to_string(),
        }),
    }
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
fn route_control_reply(
    payload: &[u8],
    control_waiters: &mut HashMap<u32, oneshot::Sender<Result<(), String>>>,
) {
    if let Ok(err) = serde_json::from_slice::<RelayControlError>(payload) {
        if let Some(tx) = control_waiters.remove(&err.id) {
            let _ = tx.send(Err(err.message));
        }
        return;
    }
    if let Ok(ack) = serde_json::from_slice::<RelayControlAck>(payload) {
        if let Some(tx) = control_waiters.remove(&ack.id) {
            let _ = tx.send(Ok(()));
        }
    }
}

/// Handles one [`PairingCommand`] from the desktop. This task wires the command
/// plumbing, the pairing-window control sends + code minting, and revocation;
/// the confirm/reject Pairing-frame responder lands with #232's ceremony work
/// (Task 11), which slots into the corresponding arms.
async fn handle_command(
    cmd: PairingCommand,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    events: &mpsc::Sender<BridgeEvent>,
    control_seq: &AtomicU32,
    control_waiters: &mut HashMap<u32, oneshot::Sender<Result<(), String>>>,
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
            let id = next_control_id(control_seq);
            let register = RelayControl::RegisterPairing {
                id,
                token: rendezvous_token,
                ttl_secs,
            };
            // Register a waiter so the relay's ack/error is routed rather than
            // dropped; the Task 11 responder awaits it before treating the window
            // as live. The receiver is dropped here (fire-and-forget for now).
            let (tx, _rx) = oneshot::channel();
            control_waiters.insert(id, tx);
            if send_control(outbound_tx, deps.bridge_id, &register)
                .await
                .is_err()
            {
                control_waiters.remove(&id);
                let _ = reply.send(Err(BridgeError::Disconnected));
                return;
            }
            let expires_at = now_secs().saturating_add(ttl_secs);
            let _ = events
                .send(BridgeEvent::PairingWindowOpened {
                    code: code.clone(),
                    expires_at,
                })
                .await;
            let _ = reply.send(Ok(code));
        }
        PairingCommand::CancelWindow => {
            let id = next_control_id(control_seq);
            let _ = send_control(
                outbound_tx,
                deps.bridge_id,
                &RelayControl::CancelPairing { id },
            )
            .await;
        }
        PairingCommand::Revoke { device_id, reply } => {
            let result = revoke_device(&device_id, deps, outbound_tx, control_seq).await;
            if result.is_ok() {
                let _ = events.send(BridgeEvent::RosterChanged).await;
            }
            let _ = reply.send(result);
        }
        // The confirm/reject responder (the Pairing-frame ceremony) lands with
        // #232's Task 11; the arms exist now so the desktop channel compiles.
        PairingCommand::Confirm { .. } | PairingCommand::Reject { .. } => {}
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

/// Removes `device_id` from the roster, persists the change, and re-asserts the
/// shrunken set so the relay kicks any live connection (ADR-0021 D6). A device
/// that was not paired is a no-op success (idempotent unpair).
async fn revoke_device(
    device_id: &DeviceId,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    control_seq: &AtomicU32,
) -> Result<(), BridgeError> {
    let assert = {
        let mut roster = deps.roster.write().await;
        if !roster.remove_by_device(device_id) {
            return Ok(());
        }
        roster.save(&deps.roster_path)?;
        assert_devices_msg(next_control_id(control_seq), &roster)
    };
    send_control(outbound_tx, deps.bridge_id, &assert)
        .await
        .map_err(|_| BridgeError::Disconnected)
}

/// Fills `buf` with cryptographically secure random bytes from the OS CSPRNG.
fn fill_random(buf: &mut [u8]) -> Result<(), BridgeError> {
    rand::rngs::OsRng
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

/// True if `url` is a WebSocket endpoint this bridge can dial.
fn is_ws_url(url: &str) -> bool {
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
fn jittered(base: Duration, rng: &mut impl rand::Rng) -> Duration {
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
    fn assert_devices_msg_maps_roster() {
        // The AssertDevices payload mirrors the roster one-for-one: each entry's
        // device id and its stored `relay_token` become one `AssertedDevice`.
        let roster = Roster {
            entries: vec![RosterEntry {
                device_id: DeviceId([0x11; 32]),
                static_pubkey: vec![0xaa; 32],
                psk: [0xbb; 32],
                name: "iPhone".to_string(),
                enrolled_at: None,
                last_connected_at: None,
                relay_token: "tok-abc".to_string(),
            }],
        };
        match assert_devices_msg(7, &roster) {
            RelayControl::AssertDevices { id, devices } => {
                assert_eq!(id, 7);
                assert_eq!(devices.len(), 1);
                assert_eq!(devices[0].device_id, DeviceId([0x11; 32]));
                assert_eq!(devices[0].token, "tok-abc");
            }
            other => panic!("expected AssertDevices, got {other:?}"),
        }
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
}
