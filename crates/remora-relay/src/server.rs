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
use std::time::Instant;

use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use remora_protocol::{
    DeviceId, Envelope, FrameType, RelayHello, ENVELOPE_HEADER_LEN, MAX_ENVELOPE_PAYLOAD,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async_with_config, WebSocketStream};

use crate::audit::{AuditRecord, AuditSink, CloseReason};
use crate::config::RelayConfig;
use crate::router::{
    outbound_channel, ConnPermit, HelloOutcome, OutboundReceiver, RouteOutcome, Router,
};

/// The largest inbound WebSocket message the relay accepts: a full envelope
/// header plus a max-length payload. Set as both `max_message_size` and
/// `max_frame_size` so the WebSocket layer rejects oversize frames *before* the
/// envelope codec would allocate for them (spec D13).
const MAX_WS_MESSAGE: usize = ENVELOPE_HEADER_LEN + MAX_ENVELOPE_PAYLOAD;

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

    let handle = tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _peer)) => stream,
                // A transient accept error must not kill the whole relay.
                Err(_) => continue,
            };
            let router = router.clone();
            let registrar = registrar.clone();
            let audit = audit.clone();
            let buffer_bytes = config.buffer_bytes;
            tokio::spawn(async move {
                handle_connection(stream, router, registrar, audit, buffer_bytes).await;
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

/// Accepts the WebSocket handshake and runs the connection. A failed handshake
/// never became a relay connection, so it emits no audit record.
async fn handle_connection(
    stream: TcpStream,
    router: Arc<Router>,
    registrar: Arc<Mutex<Registrar>>,
    audit: Arc<AuditSink>,
    buffer_bytes: usize,
) {
    let ws = match accept_async_with_config(stream, Some(ws_config())).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    run_connection(ws, router, registrar, audit, buffer_bytes).await;
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
) {
    let started = Instant::now();

    let (hello, mut frames_in, mut bytes_in) = match read_hello(&mut ws).await {
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

    // Data loop: only Data frames are legal; the reader also selects on its
    // kill channel so another connection can shut it down (4008/4009) promptly.
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
enum DataStep {
    Continue,
    Close(CloseReason),
}

/// Handles one post-hello inbound message: routes a `Data` frame, or maps
/// anything else to a close reason. On [`RouteOutcome::Overflow`] the *sender*
/// keeps running and the slow *destination* is killed (spec D9).
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
    if envelope.frame_type != FrameType::Data {
        // Hello-again, Pairing, PushTrigger post-hello are all illegal.
        return DataStep::Close(CloseReason::Protocol);
    }

    let (outcome, _victim) = router.route(permit, envelope.src, envelope.dst, bytes.to_vec());
    match outcome {
        RouteOutcome::Delivered => DataStep::Continue,
        RouteOutcome::PeerUnavailable => DataStep::Close(CloseReason::PeerGone),
        RouteOutcome::NotAllowed => DataStep::Close(CloseReason::Protocol),
        RouteOutcome::Overflow => {
            // dst-kill: wake the slow destination's reader; the sender lives on.
            let reg = registrar.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(conn) = reg.conns.get(&envelope.dst) {
                let _ = conn.kill.send(CloseReason::BufferOverflow);
            }
            DataStep::Continue
        }
    }
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
