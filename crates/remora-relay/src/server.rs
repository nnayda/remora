//! WebSocket server that drives the sans-IO [`crate::router`] (spec
//! D5/D9/D10/D13, ADR-0021).
//!
//! One accept loop, one task per connection. Each connection runs a hello
//! handshake, then a reader/writer split:
//!
//! - The **reader** task owns the connection lifecycle. It decodes inbound
//!   frames, calls [`Router::route`], and is the single owner of teardown: it
//!   decides the [`CloseReason`], instructs the writer to emit the close frame,
//!   deregisters from both the router and the server registry, and writes the
//!   one-and-only audit record. Exactly one audit record per connection close.
//! - The **writer** task owns the socket's write half. It drains the
//!   connection's [`OutboundReceiver`] to the socket and, on the reader's
//!   signal, sends the final close frame. It counts outbound frames/bytes into
//!   the shared [`ConnStats`].
//!
//! ## The kill switch (how one connection closes another)
//!
//! [`Router::route`] returns the slow destination's handle on
//! [`RouteOutcome::Overflow`] (dst-kill, 4008), and [`Router::hello`] hands back
//! a displaced holder (4009). The router is sans-IO, so it cannot close a
//! socket; the server layer must. To close *another* connection promptly, each
//! connection registers a **kill channel** in the [`Registrar`], keyed by its
//! routing id. To kill a victim, we send its [`CloseReason`] on that channel;
//! the victim's reader is selecting on it in its data loop, wakes immediately,
//! and runs its own single teardown (so the audit record is still written once,
//! by the victim itself). Router `hello`/registry insert happen under the one
//! registrar lock, so the router's displacement winner and the registry's
//! displacement winner never diverge — the 4009 victim is always exactly the
//! connection the router displaced.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use remora_protocol::{
    DeviceId, Envelope, FrameType, HelloRole, RelayControl, RelayControlAck, RelayControlError,
    RelayHello, ENVELOPE_HEADER_LEN, MAX_ENVELOPE_PAYLOAD,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async_with_config, WebSocketStream};

use crate::audit::{AuditRecord, AuditSink, CloseReason};
use crate::config::RelayConfig;
use crate::router::{
    outbound_channel, ConnPermit, ControlOutcome, HelloOutcome, OutboundReceiver, RouteOutcome,
    Router,
};

/// The largest inbound WebSocket message the relay accepts: a full envelope
/// header plus a max-length payload. Set as both `max_message_size` and
/// `max_frame_size` so the WebSocket layer rejects oversize frames *before* the
/// envelope codec would allocate for them (spec D13).
const MAX_WS_MESSAGE: usize = ENVELOPE_HEADER_LEN + MAX_ENVELOPE_PAYLOAD;

/// Backoff between `accept()` retries after an accept error, so a persistent
/// failure condition (fd exhaustion) cannot spin the accept loop hot (#231).
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// Kill signal sent to a victim connection's reader.
type KillTx = mpsc::UnboundedSender<CloseReason>;

/// Outbound-frame writer sink half of a split [`WebSocketStream`].
type WsSink = SplitSink<WebSocketStream<TcpStream>, Message>;

/// Server-side connection registry: maps a routing id to the live connection's
/// kill channel, with a serial so a displaced connection's late deregister
/// cannot evict its replacement (the same epoch guard [`Router::disconnect`]
/// uses). Kept in lockstep with the router registry: both are mutated under
/// this one lock during hello.
#[derive(Default)]
struct Registrar {
    next_serial: u64,
    conns: HashMap<DeviceId, ServerConn>,
}

struct ServerConn {
    serial: u64,
    kill: KillTx,
}

/// Outbound traffic counters shared between a connection's reader (which reads
/// them at teardown for the audit record) and its writer (which increments
/// them as it drains). Inbound counters live locally in the reader.
#[derive(Default)]
struct ConnStats {
    frames_out: std::sync::atomic::AtomicU64,
    bytes_out: std::sync::atomic::AtomicU64,
}

/// Binds `config.listen` and serves relay connections until the returned
/// [`JoinHandle`] is dropped or the task ends. Returns the actual bound address
/// (so a `127.0.0.1:0` listen resolves to a concrete ephemeral port for tests).
pub async fn serve(
    config: Arc<RelayConfig>,
    audit: Arc<AuditSink>,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(&config.listen).await?;
    let addr = listener.local_addr()?;

    let router = Router::new(config.clone());
    let registrar = Arc::new(Mutex::new(Registrar::default()));
    // Global concurrent-connection cap (pre-auth resource bound, #231). Per-IP
    // fairness is deliberately out of scope here — that belongs to the deferred
    // per-sender rate-limiting follow-up; this is a global cap only.
    let conn_limit = Arc::new(Semaphore::new(config.max_connections));
    let handshake_timeout = Duration::from_secs(config.handshake_timeout_secs);

    let handle = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _peer)) => stream,
                // A transient accept error must not kill the whole relay — but a
                // *persistent* one (fd exhaustion, EMFILE/ENFILE) would spin this
                // loop hot, burning CPU and starving teardown of the very FDs it
                // needs to recover. Back off briefly before retrying (#231).
                Err(_) => {
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            };
            // Gate on a global permit before spawning. At the cap we drop the
            // freshly accepted socket immediately (before the WebSocket
            // upgrade), so an over-limit connection costs no task and no FD
            // beyond the moment it takes to close.
            let permit = match conn_limit.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    drop(stream);
                    continue;
                }
            };
            let router = router.clone();
            let registrar = registrar.clone();
            let audit = audit.clone();
            let buffer_bytes = config.buffer_bytes;
            tokio::spawn(async move {
                // `permit` is held for the whole connection lifetime and freed
                // when this task ends, releasing its slot back to the cap.
                let _permit = permit;
                handle_connection(
                    stream,
                    router,
                    registrar,
                    audit,
                    buffer_bytes,
                    handshake_timeout,
                )
                .await;
            });
        }
    });

    Ok((addr, handle))
}

/// WebSocket config that caps message and frame size to a single max envelope.
fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE))
        .max_frame_size(Some(MAX_WS_MESSAGE))
}

/// Accepts the WebSocket handshake and runs the connection.
///
/// The whole pre-authentication handshake — the WebSocket upgrade here plus the
/// first (hello) frame read in [`run_connection`] — shares one `handshake_timeout`
/// deadline. A client that stalls the upgrade or opens the socket and never
/// sends a hello is dropped once the deadline elapses (slowloris defense, #231).
///
/// Audit-record boundary: a connection that never completed the *WebSocket
/// upgrade* — an upgrade error or an upgrade/hello-read *timeout* — never became
/// a relay connection and emits **no** record. Once the upgrade succeeds, the
/// connection existed, so its close is audited exactly once even before a hello:
/// a clean pre-hello close/EOF as [`CloseReason::Normal`] and a malformed hello
/// as [`CloseReason::Protocol`] (see [`run_connection`]).
async fn handle_connection(
    stream: TcpStream,
    router: Arc<Router>,
    registrar: Arc<Mutex<Registrar>>,
    audit: Arc<AuditSink>,
    buffer_bytes: usize,
    handshake_timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + handshake_timeout;
    let accept = accept_async_with_config(stream, Some(ws_config()));
    let ws = match tokio::time::timeout_at(deadline, accept).await {
        Ok(Ok(ws)) => ws,
        // Handshake failed, or the upgrade itself stalled past the deadline.
        Ok(Err(_)) | Err(_) => return,
    };
    run_connection(ws, router, registrar, audit, buffer_bytes, deadline).await;
}

/// Outcome of decoding the first message as a hello (before router auth).
enum HelloParse {
    /// A well-formed hello whose src matched its claimed routing id.
    Ok { hello: RelayHello, bytes_in: u64 },
    /// The peer closed or the socket ended before any hello.
    ClosedEarly,
    /// The first message was not a valid hello (malformed / wrong shape / a
    /// non-binary frame): protocol error, close 4002.
    Malformed { frames_in: u64, bytes_in: u64 },
}

/// Reads and validates the first message as a [`RelayHello`] (spec D5): it must
/// be a single binary [`FrameType::Hello`] frame addressed to
/// [`DeviceId::ZERO`], whose envelope `src` equals the JSON `routing_id`.
async fn read_hello(ws: &mut WebSocketStream<TcpStream>) -> HelloParse {
    let msg = match ws.next().await {
        Some(Ok(msg)) => msg,
        // Socket error or clean EOF before a hello ever arrived.
        Some(Err(_)) | None => return HelloParse::ClosedEarly,
    };
    let bytes = match msg {
        Message::Binary(bytes) => bytes,
        Message::Close(_) => return HelloParse::ClosedEarly,
        // Any non-binary frame before a hello is a protocol violation.
        _ => {
            return HelloParse::Malformed {
                frames_in: 0,
                bytes_in: 0,
            }
        }
    };
    let bytes_in = bytes.len() as u64;
    let malformed = || HelloParse::Malformed {
        frames_in: 1,
        bytes_in,
    };
    let Ok(envelope) = Envelope::decode(&bytes) else {
        return malformed();
    };
    if envelope.frame_type != FrameType::Hello || !envelope.dst.is_zero() {
        return malformed();
    }
    let Ok(hello) = serde_json::from_slice::<RelayHello>(&envelope.payload) else {
        return malformed();
    };
    // Anti-spoof: the envelope src must be the routing id the hello claims.
    if hello.routing_id != envelope.src {
        return malformed();
    }
    HelloParse::Ok { hello, bytes_in }
}

async fn run_connection(
    mut ws: WebSocketStream<TcpStream>,
    router: Arc<Router>,
    registrar: Arc<Mutex<Registrar>>,
    audit: Arc<AuditSink>,
    buffer_bytes: usize,
    handshake_deadline: tokio::time::Instant,
) {
    let started = Instant::now();

    // Bound the pre-hello read on the shared handshake deadline: a client that
    // connected but never sends a hello is dropped when it elapses. It never
    // authenticated, so — like every other handshake failure — it emits no
    // audit record.
    let parsed = match tokio::time::timeout_at(handshake_deadline, read_hello(&mut ws)).await {
        Ok(parsed) => parsed,
        Err(_) => return,
    };

    let (hello, mut frames_in, mut bytes_in) = match parsed {
        HelloParse::Ok { hello, bytes_in } => (hello, 1u64, bytes_in),
        HelloParse::ClosedEarly => {
            audit.record(&AuditRecord::new(
                None,
                None,
                None,
                0,
                0,
                0,
                0,
                started.elapsed().as_secs(),
                CloseReason::Normal,
            ));
            return;
        }
        HelloParse::Malformed {
            frames_in,
            bytes_in,
        } => {
            close_now(&mut ws, CloseReason::Protocol).await;
            audit.record(&AuditRecord::new(
                None,
                None,
                None,
                frames_in,
                0,
                bytes_in,
                0,
                started.elapsed().as_secs(),
                CloseReason::Protocol,
            ));
            return;
        }
    };

    // Authenticate and register under the one registrar lock so the router's
    // registry and the kill-channel registry pick the same displacement winner.
    let (out_handle, out_rx) = outbound_channel(buffer_bytes);
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel::<CloseReason>();

    let accepted = {
        let mut reg = registrar.lock().unwrap_or_else(|p| p.into_inner());
        let (outcome, _displaced) = router.hello(&hello, out_handle);
        match outcome {
            HelloOutcome::Rejected => None,
            HelloOutcome::Accepted(permit) => {
                let serial = reg.next_serial;
                reg.next_serial += 1;
                let displaced = reg.conns.insert(
                    permit.routing_id(),
                    ServerConn {
                        serial,
                        kill: kill_tx.clone(),
                    },
                );
                // The displaced holder is exactly the router's 4009 loser.
                if let Some(old) = displaced {
                    let _ = old.kill.send(CloseReason::Replaced);
                }
                Some((permit, serial))
            }
        }
    };

    let (permit, serial) = match accepted {
        Some(pair) => pair,
        None => {
            close_now(&mut ws, CloseReason::AuthFailure).await;
            audit.record(&AuditRecord::new(
                Some(hello.role),
                Some(hello.device_id),
                Some(hello.routing_id),
                frames_in,
                0,
                bytes_in,
                0,
                started.elapsed().as_secs(),
                CloseReason::AuthFailure,
            ));
            return;
        }
    };

    // Split: the writer task owns the sink and drains the outbound queue; the
    // reader loop owns the stream. `out_rx` was created before `router.hello`,
    // so any frame enqueued in the window between registration and the writer
    // starting is buffered in the channel, not lost.
    let (sink, mut stream) = ws.split();
    let stats = Arc::new(ConnStats::default());
    let (final_tx, final_rx) = oneshot::channel::<CloseFrame>();
    let writer = tokio::spawn(writer_task(sink, out_rx, final_rx, stats.clone()));

    // Post-hello loop: `Data`/`Pairing` route blindly, a bridge's `Control`
    // frame is dispatched (D4), everything else closes the connection. The
    // reader also selects on its kill channel so another connection can shut it
    // down (4001/4008/4009) promptly.
    let close_reason = loop {
        tokio::select! {
            inbound = stream.next() => {
                match inbound {
                    None => break CloseReason::Normal,
                    Some(Err(_)) => break CloseReason::Normal,
                    Some(Ok(msg)) => {
                        match handle_data_message(msg, &router, &permit, &registrar, &mut frames_in, &mut bytes_in) {
                            DataStep::Continue => {}
                            DataStep::Close(reason) => break reason,
                        }
                    }
                }
            }
            Some(reason) = kill_rx.recv() => break reason,
        }
    };

    // Single teardown path. Signal the writer to emit the close frame, wait for
    // it to flush and finish counting, then deregister and audit exactly once.
    let _ = final_tx.send(close_frame(close_reason));
    let _ = writer.await;

    router.disconnect(&permit);
    {
        let mut reg = registrar.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(conn) = reg.conns.get(&permit.routing_id()) {
            if conn.serial == serial {
                reg.conns.remove(&permit.routing_id());
            }
        }
    }

    audit.record(&AuditRecord::new(
        Some(permit.role()),
        Some(hello.device_id),
        Some(permit.routing_id()),
        frames_in,
        stats.frames_out.load(std::sync::atomic::Ordering::Relaxed),
        bytes_in,
        stats.bytes_out.load(std::sync::atomic::Ordering::Relaxed),
        started.elapsed().as_secs(),
        close_reason,
    ));
}

/// What the data loop should do after one inbound message.
#[derive(Debug, PartialEq, Eq)]
enum DataStep {
    Continue,
    Close(CloseReason),
}

/// Maps a `PeerUnavailable` route outcome to the *sender's* next step, keyed by
/// the sender's role — a routing/availability policy, not payload inspection.
///
/// - **Device sender**: its only reachable peer is its bridge, so a gone bridge
///   means the device has nothing to do — close it (4004, `PeerGone`).
/// - **Bridge sender**: addressing a departed device is *routine*, not a
///   protocol violation. Under spec D3 a device reconnects with a fresh routing
///   id, so its old id goes offline on every reconnect, and the blind relay
///   sends the bridge no departure signal — its orphaned per-peer task keeps
///   draining that session's PTY and emits one more Output frame to the now-gone
///   id. Tearing down the bridge's whole relay connection here would cancel
///   *every other* device's session on that bridge and force a full reconnect
///   with backoff. So the relay drops the undeliverable frame and the bridge
///   continues; the bridge's own per-peer task notices the client is gone
///   through its normal channel-death path.
fn peer_unavailable_step(sender_role: HelloRole) -> DataStep {
    match sender_role {
        HelloRole::Device => DataStep::Close(CloseReason::PeerGone),
        HelloRole::Bridge => DataStep::Continue,
    }
}

/// Handles one post-hello inbound message: routes a `Data`/`Pairing` frame,
/// dispatches a bridge's `Control` frame (ADR-0021 D4), or maps anything else to
/// a close reason.
fn handle_data_message(
    msg: Message,
    router: &Router,
    permit: &ConnPermit,
    registrar: &Mutex<Registrar>,
    frames_in: &mut u64,
    bytes_in: &mut u64,
) -> DataStep {
    let bytes = match msg {
        Message::Binary(bytes) => bytes,
        // Post-hello, tungstenite answers ping/pong itself; ignore them here.
        Message::Ping(_) | Message::Pong(_) => return DataStep::Continue,
        Message::Close(_) => return DataStep::Close(CloseReason::Normal),
        // Text or a raw frame is a protocol violation.
        _ => return DataStep::Close(CloseReason::Protocol),
    };
    *frames_in += 1;
    *bytes_in += bytes.len() as u64;

    let Ok(envelope) = Envelope::decode(&bytes) else {
        return DataStep::Close(CloseReason::Protocol);
    };
    match envelope.frame_type {
        // `Data` and `Pairing` both flow device↔bridge blindly: the relay never
        // inspects either payload and routes them by the same adjacency rules.
        FrameType::Data | FrameType::Pairing => route_frame(
            router,
            permit,
            registrar,
            envelope.src,
            envelope.dst,
            bytes.to_vec(),
        ),
        // `Control` is a bridge→relay message the relay terminates (ADR-0021 D4):
        // it is the only frame whose JSON the relay decodes.
        FrameType::Control => dispatch_control(router, permit, registrar, &envelope.payload),
        // Hello-again and PushTrigger post-hello are protocol violations.
        FrameType::Hello | FrameType::PushTrigger => DataStep::Close(CloseReason::Protocol),
    }
}

/// Routes one blind `Data`/`Pairing` envelope through [`Router::route`] and maps
/// the outcome to the sender's next step. On [`RouteOutcome::Overflow`] the
/// *sender* keeps running and the slow *destination* is killed (spec D9).
fn route_frame(
    router: &Router,
    permit: &ConnPermit,
    registrar: &Mutex<Registrar>,
    src: DeviceId,
    dst: DeviceId,
    bytes: Vec<u8>,
) -> DataStep {
    let (outcome, _victim) = router.route(permit, src, dst, bytes);
    match outcome {
        RouteOutcome::Delivered => DataStep::Continue,
        // A gone peer closes only a *device* sender; a *bridge* addressing a
        // departed device is routine — drop the frame and continue (see
        // `peer_unavailable_step`).
        RouteOutcome::PeerUnavailable => peer_unavailable_step(permit.role()),
        RouteOutcome::NotAllowed => DataStep::Close(CloseReason::Protocol),
        RouteOutcome::Overflow => {
            // dst-kill: wake the slow destination's reader; the sender lives on.
            let reg = registrar.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(conn) = reg.conns.get(&dst) {
                let _ = conn.kill.send(CloseReason::BufferOverflow);
            }
            DataStep::Continue
        }
    }
}

/// Dispatches a bridge's relay-terminated [`RelayControl`] (ADR-0021 D4): decode
/// the JSON, apply it via [`Router::handle_control`], reply an
/// `RelayControlAck`/`RelayControlError` on the bridge's *own* outbound, and kick
/// any de-asserted devices via their kill channels (4001).
///
/// A `Control` frame from a device (not a bridge) is a protocol violation → 4002.
/// The relay decodes only this frame's JSON; `Data`/`Pairing` payloads stay
/// opaque.
fn dispatch_control(
    router: &Router,
    permit: &ConnPermit,
    registrar: &Mutex<Registrar>,
    payload: &[u8],
) -> DataStep {
    // Control is bridge→relay only; a device sending one is a protocol error.
    if permit.role() != HelloRole::Bridge {
        return DataStep::Close(CloseReason::Protocol);
    }
    let Ok(control) = serde_json::from_slice::<RelayControl>(payload) else {
        return DataStep::Close(CloseReason::Protocol);
    };
    // The request `id` (echoed in the reply) is read before `handle_control`
    // consumes the message. A successfully decoded control is always a known
    // variant, so the fallback is unreachable.
    let id = control_request_id(&control);
    match router.handle_control(permit, control) {
        ControlOutcome::Ack => {
            reply_control(router, permit, control_ack_bytes(id));
            DataStep::Continue
        }
        ControlOutcome::Error(message) => {
            reply_control(router, permit, control_error_bytes(id, message));
            DataStep::Continue
        }
        // Unreachable — the bridge role was checked above — but stay total.
        ControlOutcome::NotBridge => DataStep::Close(CloseReason::Protocol),
        ControlOutcome::KickDevices(routing_ids) => {
            // De-asserted devices with live connections are kicked by routing id
            // via the registrar kill channel (4001), then the assert is still
            // acked so the bridge learns its roster change was applied.
            {
                let reg = registrar.lock().unwrap_or_else(|p| p.into_inner());
                for routing_id in &routing_ids {
                    if let Some(conn) = reg.conns.get(routing_id) {
                        let _ = conn.kill.send(CloseReason::AuthFailure);
                    }
                }
            }
            reply_control(router, permit, control_ack_bytes(id));
            DataStep::Continue
        }
    }
}

/// The correlation `id` a [`RelayControl`] carries, echoed in its reply. Every
/// decodable variant has one; the fallback covers a future `#[non_exhaustive]`
/// variant this build cannot decode (so it never actually runs).
fn control_request_id(control: &RelayControl) -> u32 {
    match control {
        RelayControl::RegisterPairing { id, .. } => *id,
        RelayControl::CancelPairing { id } => *id,
        RelayControl::AssertDevices { id, .. } => *id,
        _ => 0,
    }
}

/// Encodes a relay-terminated reply envelope: `src` = [`DeviceId::ZERO`] (the
/// relay), `dst` = the bridge's routing id.
fn control_reply_frame(dst: DeviceId, payload: Vec<u8>) -> Vec<u8> {
    Envelope {
        frame_type: FrameType::Control,
        src: DeviceId::ZERO,
        dst,
        payload,
    }
    .encode()
}

/// JSON payload of a [`RelayControlAck`] for request `id`.
fn control_ack_bytes(id: u32) -> Vec<u8> {
    serde_json::to_vec(&RelayControlAck { id }).unwrap_or_default()
}

/// JSON payload of a [`RelayControlError`] for request `id`.
fn control_error_bytes(id: u32, message: String) -> Vec<u8> {
    serde_json::to_vec(&RelayControlError { id, message }).unwrap_or_default()
}

/// Enqueues a relay-terminated control reply on the bridge's own outbound queue.
fn reply_control(router: &Router, permit: &ConnPermit, payload: Vec<u8>) {
    let routing_id = permit.routing_id();
    router.enqueue_to(&routing_id, control_reply_frame(routing_id, payload));
}

/// Drains the outbound queue to the socket and, on the reader's signal, sends
/// the final close frame. Biased toward the close signal so a kill (4008/4009)
/// closes the victim's socket promptly, dropping any still-queued frames.
async fn writer_task(
    mut sink: WsSink,
    mut out_rx: OutboundReceiver,
    mut final_rx: oneshot::Receiver<CloseFrame>,
    stats: Arc<ConnStats>,
) {
    loop {
        tokio::select! {
            biased;
            close = &mut final_rx => {
                if let Ok(frame) = close {
                    let _ = sink.send(Message::Close(Some(frame))).await;
                }
                break;
            }
            frame = out_rx.recv() => {
                match frame {
                    Some(frame) => {
                        let len = frame.len();
                        if sink
                            .send(Message::Binary(frame.bytes().to_vec().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        stats
                            .frames_out
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        stats
                            .bytes_out
                            .fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
                        // `frame` drops here, releasing its byte reservation
                        // only after the write completed.
                    }
                    // Every outbound handle dropped and the queue drained.
                    None => break,
                }
            }
        }
    }
    let _ = sink.close().await;
}

/// Maps a [`CloseReason`] to its WebSocket [`CloseFrame`] (spec close codes).
fn close_frame(reason: CloseReason) -> CloseFrame {
    let code = match reason {
        CloseReason::Normal => 1000,
        CloseReason::AuthFailure => 4001,
        CloseReason::Protocol => 4002,
        CloseReason::PeerGone => 4004,
        CloseReason::BufferOverflow => 4008,
        CloseReason::Replaced => 4009,
    };
    CloseFrame {
        code: CloseCode::from(code),
        reason: reason.as_str().into(),
    }
}

/// Sends a close frame directly on an unsplit stream (pre-hello close paths).
async fn close_now(ws: &mut WebSocketStream<TcpStream>, reason: CloseReason) {
    let _ = ws.send(Message::Close(Some(close_frame(reason)))).await;
    let _ = ws.close(None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_unavailable_closes_a_device_sender() {
        // A device whose only bridge is gone has nothing to do — close it 4004.
        assert_eq!(
            peer_unavailable_step(HelloRole::Device),
            DataStep::Close(CloseReason::PeerGone),
        );
    }

    #[test]
    fn peer_unavailable_drops_and_continues_for_a_bridge_sender() {
        // A bridge addressing a departed device is routine (D3 fresh routing
        // ids): drop the undeliverable frame, never tear down the bridge.
        assert_eq!(peer_unavailable_step(HelloRole::Bridge), DataStep::Continue);
    }
}
