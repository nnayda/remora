//! The client half of relay mode (ADR-0021): [`RemoteSource`].
//!
//! [`RemoteSource`] implements [`SessionSource`] by speaking the end-to-end
//! Noise wire ([`bridge`](crate::bridge) is the peer it talks to) through a
//! blind relay. One `RemoteSource` is bound to one [`PairingFile`] — i.e. one
//! bridge — because a relay-mode session's identity is `(bridge, session)`,
//! never a bare session id.
//!
//! # Connection model (spec D3/D15)
//!
//! Each control call opens a **fresh** WS + Noise connection, uses it for its
//! one request, and closes it:
//!
//! - [`list`](RemoteSource::list) dials, runs the relay hello + `IKpsk2`
//!   handshake + strict E2E hello, sends one `Request { List }`, reads the
//!   `Response`, and drops the connection.
//! - [`attach`](RemoteSource::attach) does the same up to a `Request { Attach }`;
//!   on `Attached` it hands the caller a [`SessionChannel`] and spawns a pump
//!   task that *owns* the connection (sink, stream, transport) for that one
//!   attach's lifetime. "One attach per connection" (D15): the next attach dials
//!   a fresh connection.
//!
//! This per-call dial is the simplest correct shape for slice 1: there is no
//! shared, persistent control connection to keep alive or re-dial, so `list`
//! and `attach` never contend and a dropped connection only ever kills the one
//! call that owned it. A persistent multiplexed control connection is a later
//! optimization, not needed for the loopback proof (Task 14).
//!
//! # Structural death
//!
//! An attach's pump task holds *both* transport-facing ends of the
//! [`SessionChannel`] pair (the input receiver and the output sender). Any
//! terminal event — the bridge's `ChannelClosed`, a decrypt failure, a decode
//! error, or the relay connection dropping — returns from the pump, dropping
//! both ends together. The caller then observes `recv() -> None` and
//! `send_bytes() -> ChannelClosed`, exactly as [`SessionChannel`] specifies.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use remora_core::{SessionChannel, SessionSource, SourceError};
use remora_protocol::{
    AgentId, BridgeMessage, ChannelInput, ChannelOutput, ClientMessage, DeviceId, Envelope,
    FrameType, HelloRole, ProjectId, RelayHello, RemoteOp, RemoteResult, SessionId, SessionMeta,
    SpawnSpec, PROTOCOL_VERSION,
};

use crate::identity::PairingFile;
use crate::noise::{chunk_bytes, prologue, Handshake, HandshakeKind, NoiseError, Transport};
use crate::wire_error::map_wire_error;

/// Length of the plaintext identity preamble prefixing a client's first
/// (handshake) frame (spec D16): a 32-byte [`DeviceId`].
const DEVICE_ID_LEN: usize = 32;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Deadline for a single relay setup/request read. The bridge can drop a peer
/// (roster miss, protocol violation) *without* closing the client's relay
/// socket, so a naive `recv` would hang `dial`/`list`/`attach` forever. Bounding
/// each setup + request read turns that into a prompt typed error (#231).
const RELAY_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Decoded Noise key material for one dial: device static private key, the
/// bridge's pinned static public key, and the per-pair PSK.
type KeyMaterial = (Vec<u8>, Vec<u8>, [u8; 32]);

/// The client [`SessionSource`] for relay mode (ADR-0021).
///
/// Bound to one [`PairingFile`] (one bridge). `spawn`/`respawn`/`stop`/`remove`
/// are intentionally unsupported in slice 1 (see #231); `list`/`attach` speak
/// the E2E wire through the relay.
pub struct RemoteSource {
    pairing: PairingFile,
}

impl RemoteSource {
    /// Binds a source to `pairing`. One `RemoteSource` per pairing file — the
    /// session identity is `(bridge, session)`, never a bare session id.
    pub fn new(pairing: PairingFile) -> RemoteSource {
        RemoteSource { pairing }
    }

    /// This source's bridge id — the routing target for every connection.
    ///
    /// Used by higher layers (Task 15) to label sessions by their owning bridge.
    pub fn bridge_id(&self) -> DeviceId {
        self.pairing.bridge_id
    }

    /// Decodes the pairing file's base64 key material once per dial: the
    /// device's static private key, the bridge's pinned static public key, and
    /// the per-pair PSK.
    fn key_material(&self) -> Result<KeyMaterial, SourceError> {
        let device_priv = B64
            .decode(&self.pairing.device_private_key)
            .map_err(|e| SourceError::Transport(format!("bad device private key: {e}")))?;
        let bridge_pub = B64
            .decode(&self.pairing.bridge_static_pubkey)
            .map_err(|e| SourceError::Transport(format!("bad bridge public key: {e}")))?;
        let psk_bytes = B64
            .decode(&self.pairing.psk)
            .map_err(|e| SourceError::Transport(format!("bad psk: {e}")))?;
        let psk: [u8; 32] = psk_bytes
            .try_into()
            .map_err(|_| SourceError::Transport("psk is not 32 bytes".to_string()))?;
        Ok((device_priv, bridge_pub, psk))
    }

    /// Opens a fresh connection: relay hello, `IKpsk2` initiator handshake, then
    /// the strict E2E hello (fail closed on a [`PROTOCOL_VERSION`] mismatch).
    /// Returns a [`Conn`] ready to carry one request.
    async fn dial(&self) -> Result<Conn, SourceError> {
        let (device_priv, bridge_pub, psk) = self.key_material()?;
        let device_id = self.pairing.device_id;
        let bridge_id = self.pairing.bridge_id;

        let (ws, _resp) = connect_async(&self.pairing.relay_url)
            .await
            .map_err(|e| SourceError::Transport(format!("relay dial failed: {e}")))?;
        let (mut sink, mut stream) = ws.split();

        // A fresh random routing id per connection (spec D3). 32 random bytes is
        // never the reserved all-zero id the relay would reject.
        let routing_id = DeviceId(rand::random());

        // --- Relay hello: role=device, addressed to the relay (dst=ZERO). The
        // relay's anti-spoof check requires the envelope src == routing_id. ---
        let hello = RelayHello {
            role: HelloRole::Device,
            token: self.pairing.rendezvous_token.clone(),
            device_id,
            routing_id,
            bridge_id,
        };
        let hello_payload = serde_json::to_vec(&hello)
            .map_err(|e| SourceError::Transport(format!("hello serialize failed: {e}")))?;
        send_frame(
            &mut sink,
            FrameType::Hello,
            routing_id,
            DeviceId::ZERO,
            hello_payload,
        )
        .await?;

        // --- Noise IKpsk2 as initiator. The prologue binds this exact route
        // (identity, routing id, bridge id); the bridge builds the same one. ---
        let bound = prologue(HandshakeKind::Session, &device_id, &routing_id, &bridge_id);
        let mut hs =
            Handshake::initiator(&device_priv, &bridge_pub, &psk, &bound).map_err(noise_err)?;
        let msg1 = hs.write_message(&[]).map_err(noise_err)?;

        // The client's first Data frame is `32-byte identity preamble ‖ msg1`
        // (spec D16); the bridge splits it to pick the roster PSK and pin.
        let mut first = Vec::with_capacity(DEVICE_ID_LEN + msg1.len());
        first.extend_from_slice(&device_id.0);
        first.extend_from_slice(&msg1);
        send_frame(&mut sink, FrameType::Data, routing_id, bridge_id, first).await?;

        let msg2 = recv_frame(&mut stream, routing_id).await?;
        hs.read_message(&msg2).map_err(noise_err)?;
        // The initiator already pinned the responder's static via `bridge_pub`,
        // so its learned copy is redundant — discard it.
        let (mut transport, _remote_static) = hs.into_transport().map_err(noise_err)?;

        // --- Strict E2E hello: send ours, then fail closed on any mismatch. ---
        let ciphertext = transport
            .seal(&ClientMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            })
            .map_err(noise_err)?;
        send_frame(
            &mut sink,
            FrameType::Data,
            routing_id,
            bridge_id,
            ciphertext,
        )
        .await?;

        let reply = recv_frame(&mut stream, routing_id).await?;
        match transport.open::<BridgeMessage>(&reply).map_err(noise_err)? {
            BridgeMessage::Hello { protocol_version } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(SourceError::Transport(format!(
                        "protocol version mismatch: bridge {protocol_version}, client {PROTOCOL_VERSION}"
                    )));
                }
            }
            _ => {
                return Err(SourceError::Transport(
                    "bridge did not answer hello with hello".to_string(),
                ))
            }
        }

        Ok(Conn {
            sink,
            stream,
            transport,
            routing_id,
            bridge_id,
        })
    }
}

/// The typed error every unsupported op returns (spec: slice-1 scope, #231).
fn unsupported() -> SourceError {
    SourceError::Transport("not supported over relay (slice 1, see #231)".to_string())
}

/// Projects a [`NoiseError`] onto a transport [`SourceError`]. Its `Display` is
/// re-escaped by `SourceError::Transport`, so no raw bytes reach a terminal.
fn noise_err(e: NoiseError) -> SourceError {
    SourceError::Transport(format!("noise error: {e}"))
}

#[async_trait]
impl SessionSource for RemoteSource {
    async fn spawn(&self, _spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        Err(unsupported())
    }

    async fn external_attach_command(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
    ) -> Result<Vec<String>, SourceError> {
        // An external-terminal attach runs a LOCAL transport argv
        // (ssh/kubectl); a relay client holds no such command, so it is
        // unsupported over the wire (spec D15, slice-1 scope).
        Err(unsupported())
    }

    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let mut conn = self.dial().await?;
        let id = rand::random::<u32>();
        conn.send(&ClientMessage::Request {
            id,
            op: RemoteOp::Attach {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        })
        .await?;

        match conn.recv().await? {
            // Per the per-call single-request dial model, the response id must
            // echo the request id; anything else is a protocol violation.
            BridgeMessage::Response { id: got, result } if got == id => match result {
                RemoteResult::Attached => {}
                RemoteResult::Error(w) => return Err(map_wire_error(w)),
                other => {
                    return Err(SourceError::Transport(format!(
                        "unexpected attach result: {other:?}"
                    )))
                }
            },
            BridgeMessage::Response { .. } => {
                return Err(SourceError::Transport("unexpected response id".to_string()))
            }
            other => {
                return Err(SourceError::Transport(format!(
                    "expected attach response, got {other:?}"
                )))
            }
        }

        // Success: hand the caller its channel and give the connection to a pump
        // task that owns both transport-facing ends for this attach's lifetime.
        let (channel, input_rx, output_tx) = SessionChannel::pair();
        let Conn {
            sink,
            stream,
            transport,
            routing_id,
            bridge_id,
        } = conn;
        tokio::spawn(run_pump(
            sink, stream, transport, input_rx, output_tx, routing_id, bridge_id,
        ));
        Ok(channel)
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let mut conn = self.dial().await?;
        let id = rand::random::<u32>();
        conn.send(&ClientMessage::Request {
            id,
            op: RemoteOp::List,
        })
        .await?;

        match conn.recv().await? {
            // The response id must echo the request id (single-request dial).
            BridgeMessage::Response { id: got, result } if got == id => match result {
                RemoteResult::Sessions(sessions) => Ok(sessions),
                RemoteResult::Error(w) => Err(map_wire_error(w)),
                other => Err(SourceError::Transport(format!(
                    "unexpected list result: {other:?}"
                ))),
            },
            BridgeMessage::Response { .. } => {
                Err(SourceError::Transport("unexpected response id".to_string()))
            }
            other => Err(SourceError::Transport(format!(
                "expected list response, got {other:?}"
            ))),
        }
        // `conn` drops here, closing the connection.
    }

    async fn respawn(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
        _agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        Err(unsupported())
    }

    async fn stop(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
    ) -> Result<(), SourceError> {
        Err(unsupported())
    }

    async fn remove(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
        _force: bool,
    ) -> Result<(), SourceError> {
        Err(unsupported())
    }
}

/// The split write half of a relay WebSocket.
type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// The split read half of a relay WebSocket.
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// One established relay connection: the split WebSocket, the Noise transport,
/// and the routing ids for each outbound frame. Owns `transport` outright, so
/// its seal/open nonce sequences stay single-threaded.
struct Conn {
    sink: WsSink,
    stream: WsStream,
    transport: Transport,
    /// This connection's routing id (its `src`, and every reply's `dst`).
    routing_id: DeviceId,
    /// The bridge this connection routes to (every outbound frame's `dst`).
    bridge_id: DeviceId,
}

impl Conn {
    /// Seals `msg` and enqueues it as an outbound Data frame to the bridge.
    async fn send(&mut self, msg: &ClientMessage) -> Result<(), SourceError> {
        let ciphertext = self.transport.seal(msg).map_err(noise_err)?;
        send_frame(
            &mut self.sink,
            FrameType::Data,
            self.routing_id,
            self.bridge_id,
            ciphertext,
        )
        .await
    }

    /// Reads the next inbound Data frame addressed to us and opens it.
    async fn recv(&mut self) -> Result<BridgeMessage, SourceError> {
        let payload = recv_frame(&mut self.stream, self.routing_id).await?;
        self.transport
            .open::<BridgeMessage>(&payload)
            .map_err(noise_err)
    }
}

/// Owns one attach's connection for its lifetime: seals caller input into the
/// bridge, opens bridge output back to the caller. Holds *both* transport-facing
/// [`SessionChannel`] ends, so returning (on any terminal event) drops both
/// together — the caller then sees structural channel death.
async fn run_pump(
    mut sink: WsSink,
    mut stream: WsStream,
    mut transport: Transport,
    mut input_rx: mpsc::Receiver<ChannelInput>,
    output_tx: mpsc::Sender<ChannelOutput>,
    routing_id: DeviceId,
    bridge_id: DeviceId,
) {
    loop {
        tokio::select! {
            // Caller input -> sealed ClientMessage::Input -> Data frame.
            maybe_input = input_rx.recv() => {
                // `None`: the caller dropped its input sender — channel is dead.
                let Some(input) = maybe_input else { return };
                if send_input(&mut sink, &mut transport, routing_id, bridge_id, input)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Bridge frame -> open -> forward Output; any error/close is death.
            inbound = stream.next() => {
                let payload = match inbound {
                    Some(Ok(Message::Binary(bytes))) => match Envelope::decode(&bytes) {
                        Ok(env)
                            if env.frame_type == FrameType::Data && env.dst == routing_id =>
                        {
                            env.payload
                        }
                        // A frame not addressed to us (relay never sends these):
                        // ignore rather than tear down.
                        Ok(_) => continue,
                        // Malformed post-handshake frame: structural death.
                        Err(_) => return,
                    },
                    // Close / clean EOF / any read error: the connection is gone.
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    // Ping/Pong/Text: tungstenite answers pings; ignore the rest.
                    Some(Ok(_)) => continue,
                };
                match transport.open::<BridgeMessage>(&payload) {
                    Ok(BridgeMessage::Output(out)) => {
                        // The caller dropped its output receiver: channel is dead.
                        if output_tx.send(out).await.is_err() {
                            return;
                        }
                    }
                    // The bridge's attached channel died: structural death.
                    Ok(BridgeMessage::ChannelClosed) => return,
                    // A stray Hello/Response mid-stream is out of protocol here;
                    // ignore it rather than corrupting the byte stream.
                    Ok(_) => {}
                    // Decrypt / nonce failure: structural death.
                    Err(_) => return,
                }
            }
        }
    }
}

/// Seals one [`ChannelInput`] to the bridge, chunking a large `Bytes` run so no
/// single message trips the Noise plaintext cap; `Resize` seals directly.
/// `Err(())` means the connection is gone (seal or write failed).
async fn send_input(
    sink: &mut WsSink,
    transport: &mut Transport,
    src: DeviceId,
    dst: DeviceId,
    input: ChannelInput,
) -> Result<(), ()> {
    match input {
        ChannelInput::Bytes(bytes) => {
            for chunk in chunk_bytes(bytes) {
                seal_and_send(
                    sink,
                    transport,
                    src,
                    dst,
                    &ClientMessage::Input(ChannelInput::Bytes(chunk)),
                )
                .await?;
            }
            Ok(())
        }
        other => seal_and_send(sink, transport, src, dst, &ClientMessage::Input(other)).await,
    }
}

/// Seals `msg` and writes it as an outbound Data frame; `Err(())` on failure.
async fn seal_and_send(
    sink: &mut WsSink,
    transport: &mut Transport,
    src: DeviceId,
    dst: DeviceId,
    msg: &ClientMessage,
) -> Result<(), ()> {
    let ciphertext = transport.seal(msg).map_err(|_| ())?;
    let frame = Envelope {
        frame_type: FrameType::Data,
        src,
        dst,
        payload: ciphertext,
    }
    .encode();
    sink.send(Message::Binary(frame.into()))
        .await
        .map_err(|_| ())
}

/// Wraps `payload` in an [`Envelope`] and writes it to the socket. Used for the
/// pre-Noise hello and handshake frames plus every sealed control frame.
async fn send_frame(
    sink: &mut WsSink,
    frame_type: FrameType,
    src: DeviceId,
    dst: DeviceId,
    payload: Vec<u8>,
) -> Result<(), SourceError> {
    let frame = Envelope {
        frame_type,
        src,
        dst,
        payload,
    }
    .encode();
    sink.send(Message::Binary(frame.into()))
        .await
        .map_err(|e| SourceError::Transport(format!("relay write failed: {e}")))
}

/// Reads the next Data frame addressed to `routing_id`, bounded by
/// [`RELAY_READ_TIMEOUT`]. Used for every setup + request read (`dial`,
/// `list`, `attach`); the attach *pump* reads the stream directly and is not
/// deadline-bound, since a live attach legitimately idles waiting for output.
async fn recv_frame(stream: &mut WsStream, routing_id: DeviceId) -> Result<Vec<u8>, SourceError> {
    match tokio::time::timeout(RELAY_READ_TIMEOUT, recv_frame_inner(stream, routing_id)).await {
        Ok(result) => result,
        Err(_) => Err(SourceError::Transport("relay read timed out".to_string())),
    }
}

/// Reads inbound frames until one is a Data frame addressed to `routing_id`,
/// returning its payload. Frames not addressed to us are skipped; a malformed
/// frame, a close, an EOF, or a read error is a transport error.
async fn recv_frame_inner(
    stream: &mut WsStream,
    routing_id: DeviceId,
) -> Result<Vec<u8>, SourceError> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let envelope = Envelope::decode(&bytes)
                    .map_err(|e| SourceError::Transport(format!("malformed frame: {e}")))?;
                if envelope.frame_type == FrameType::Data && envelope.dst == routing_id {
                    return Ok(envelope.payload);
                }
                // Not a Data frame for us: keep waiting.
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(SourceError::Transport(
                    "relay connection closed".to_string(),
                ))
            }
            Some(Err(e)) => return Err(SourceError::Transport(format!("relay read error: {e}"))),
            // Ping/Pong/Text: ignore and keep waiting for a Data frame.
            Some(Ok(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid pairing file. The key material is all-zero base64:
    /// good enough for the no-dial unit tests here (Task 14 drives a real
    /// handshake end-to-end).
    fn pairing() -> PairingFile {
        PairingFile {
            relay_url: "wss://relay.example/ws".to_string(),
            rendezvous_token: "rendezvous-tok".to_string(),
            bridge_id: DeviceId([0xab; 32]),
            bridge_static_pubkey: B64.encode([0u8; 32]),
            psk: B64.encode([0u8; 32]),
            device_id: DeviceId([0xcd; 32]),
            device_private_key: B64.encode([0u8; 32]),
            device_public_key: B64.encode([0u8; 32]),
        }
    }

    #[test]
    fn remote_source_exposes_bridge_id() {
        // Task 15 labels sessions by their owning bridge; the accessor must
        // return exactly the pairing file's bridge id.
        let source = RemoteSource::new(pairing());
        assert_eq!(source.bridge_id(), DeviceId([0xab; 32]));
    }

    #[tokio::test]
    async fn unsupported_ops_are_typed_transport_errors() {
        let source = RemoteSource::new(pairing());
        let project = ProjectId::new("api").expect("valid slug");
        let session = SessionId::new("fix-login").expect("valid slug");

        // Every write op is a typed `Transport` error naming slice 1 (#231),
        // never a panic and never a different variant.
        let spec = SpawnSpec {
            project_id: project.clone(),
            session_id: session.clone(),
            agent: None,
            base: None,
            workspace: None,
            branch: None,
            worktree_root: None,
        };
        assert_unsupported(source.spawn(spec).await.err());
        assert_unsupported(source.respawn(&project, &session, None).await.err());
        assert_unsupported(source.stop(&project, &session).await.err());
        assert_unsupported(source.remove(&project, &session, false).await.err());
    }

    fn assert_unsupported(err: Option<SourceError>) {
        match err {
            Some(SourceError::Transport(message)) => {
                assert!(
                    message.contains("slice 1"),
                    "unsupported op must mention slice 1, got: {message}"
                );
            }
            other => panic!("expected Transport error, got: {other:?}"),
        }
    }
}
