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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use remora_core::SessionSource;
use remora_protocol::{
    BridgeMessage, ChannelInput, ChannelOutput, ClientMessage, DeviceId, Envelope, FrameType,
    HelloRole, RelayHello, RemoteOp, RemoteResult, WireError, PROTOCOL_VERSION,
};

use crate::identity::{BridgeIdentity, Roster};
use crate::noise::{chunk_bytes, prologue, Handshake, Transport};
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
    /// The paired-device roster (pinned static keys + per-pair PSKs).
    pub roster: Roster,
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

/// Shared, cheaply-clonable material every peer task needs.
struct PeerDeps {
    /// The bridge's routing id (also its `src` on every outbound frame).
    bridge_id: DeviceId,
    /// The bridge's static private key, presented as the Noise responder.
    bridge_static_priv: Vec<u8>,
    /// The paired-device roster (lookup by claimed identity).
    roster: Roster,
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
    shutdown: CancellationToken,
) -> Result<(), BridgeServeError> {
    if !is_ws_url(&config.relay_url) {
        return Err(BridgeServeError::InvalidRelayUrl(config.relay_url));
    }

    let deps = Arc::new(PeerDeps {
        bridge_id: config.identity.device_id,
        bridge_static_priv: config.identity.static_keypair.private.clone(),
        roster: config.roster.clone(),
        source,
    });

    let mut rng = rand::rng();
    let mut backoff = BACKOFF_MIN;
    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        match run_connection(&config, &deps, &shutdown).await {
            ConnOutcome::Shutdown => return Ok(()),
            // A fresh loss starts backoff over; a never-connected attempt keeps
            // the current (growing) delay so a down relay is not hammered.
            ConnOutcome::Disconnected => backoff = BACKOFF_MIN,
            ConnOutcome::ConnectFailed => {}
        }
        if shutdown.is_cancelled() {
            return Ok(());
        }
        let delay = jittered(backoff, &mut rng);
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
        backoff = next_backoff(backoff);
    }
}

/// Runs one relay connection to completion: dial, hello, then read/route until
/// the connection drops or `shutdown` fires.
async fn run_connection(
    config: &BridgeConfig,
    deps: &Arc<PeerDeps>,
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

    let mut peers: HashMap<DeviceId, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let outcome = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break ConnOutcome::Shutdown,
            inbound = stream.next() => match inbound {
                None => break ConnOutcome::Disconnected,
                Some(Err(_)) => break ConnOutcome::Disconnected,
                Some(Ok(Message::Binary(bytes))) => {
                    dispatch_inbound(bytes.as_ref(), &mut peers, deps, &outbound_tx, &conn_token);
                }
                Some(Ok(Message::Close(_))) => break ConnOutcome::Disconnected,
                // Ping/Pong/Text/Frame: tungstenite answers pings itself; the
                // bridge speaks only binary Data frames, so ignore the rest.
                Some(Ok(_)) => {}
            },
        }
    };

    // Tear down: cancel all peers, drop their inbound queues, stop the writer.
    conn_token.cancel();
    drop(peers);
    drop(outbound_tx);
    writer.abort();
    outcome
}

/// Routes one inbound binary frame to its peer task, spawning a new peer task
/// for a `src` not seen before. Pure-sync: never awaits, so the read loop stays
/// responsive across peers.
fn dispatch_inbound(
    bytes: &[u8],
    peers: &mut HashMap<DeviceId, mpsc::Sender<Vec<u8>>>,
    deps: &Arc<PeerDeps>,
    outbound_tx: &mpsc::Sender<Message>,
    conn_token: &CancellationToken,
) {
    let envelope = match Envelope::decode(bytes) {
        Ok(e) => e,
        Err(_) => return, // malformed frame from the relay: ignore
    };
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
    tokio::spawn(run_peer(
        src,
        frame_rx,
        deps.clone(),
        outbound_tx.clone(),
        conn_token.clone(),
    ));
    // The queue is empty and has capacity, so this first send cannot fail.
    if frame_tx.try_send(payload).is_ok() {
        peers.insert(src, frame_tx);
    }
}

/// One client peer's whole lifecycle: Noise handshake, E2E hello, then serve.
async fn run_peer(
    routing_id: DeviceId,
    mut frame_rx: mpsc::Receiver<Vec<u8>>,
    deps: Arc<PeerDeps>,
    outbound_tx: mpsc::Sender<Message>,
    conn_token: CancellationToken,
) {
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

    // Roster lookup by claimed identity selects the PSK and pins the static.
    let entry = deps.roster.find_by_device(&initiator_identity)?;

    // The prologue binds this exact route; a forged identity/routing yields a
    // different prologue than the honest client used, failing the handshake.
    let bound = prologue(&initiator_identity, routing_id, &deps.bridge_id);
    let mut hs = Handshake::responder(&deps.bridge_static_priv, &entry.psk, &bound).ok()?;
    hs.read_message(msg1).ok()?;
    let msg2 = hs.write_message(&[]).ok()?;
    send_frame(outbound_tx, deps.bridge_id, *routing_id, msg2)
        .await
        .ok()?;
    let (transport, remote_static) = hs.into_transport().ok()?;

    // spec C7: the Noise-authenticated initiator static MUST equal the pinned
    // roster key. `into_transport` yields an empty vec when snow never learned
    // a remote static; treat empty OR mismatch as an auth failure.
    if remote_static.is_empty() || remote_static != entry.static_pubkey {
        return None;
    }
    Some(transport)
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

    // --- E2E hello: the client's first application message must be Hello. ---
    let hello_frame = tokio::select! {
        _ = conn_token.cancelled() => return,
        f = frame_rx.recv() => match f {
            Some(f) => f,
            None => return,
        },
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
