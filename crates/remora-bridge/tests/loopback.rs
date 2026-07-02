//! Loopback E2E integration proof for relay slice 1 (ADR-0021, #231).
//!
//! Wires the whole stack together over real localhost sockets and real Noise:
//!
//! ```text
//! (Fake|Scripted)SessionSource → ExclusiveSource → serve_bridge
//!     ⇄ [ ws://127.0.0.1:0 blind relay, real IKpsk2 Noise ] ⇄
//!         RemoteSource (client) → SessionChannel → attach
//! ```
//!
//! The bridge dials the relay *outbound* and registers asynchronously, so the
//! harness never sleeps-and-hopes: [`Harness::wait_ready`] polls `remote.list()`
//! with a short backoff until the bridge's route is live (or a generous timeout
//! elapses). Every message-arrival assertion is wrapped in
//! [`tokio::time::timeout`] so a wiring regression fails fast instead of hanging.
//!
//! # Relay lifecycle (why a dedicated runtime)
//!
//! `remora_relay::serve` returns only the *accept-loop* `JoinHandle`; the
//! per-connection tasks it spawns are detached, so aborting that handle would
//! stop new accepts but leave the bridge's existing connection alive — the
//! reconnect test could never observe a drop. Instead each relay instance owns
//! a private multi-thread runtime on its own OS thread ([`Relay`]); "killing"
//! the relay drops that runtime, which aborts *every* relay task and closes its
//! sockets. Restarting binds the same concrete port again. This keeps
//! `remora-relay` unchanged (no shutdown token bolted on just for tests).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    prologue, provision_device, serve_bridge, BridgeConfig, BridgeIdentity, Handshake, PairingFile,
    RemoteSource, Roster, Transport,
};
use remora_core::{
    ExclusiveSource, FakeSessionSource, SessionChannel, SessionLocks, SessionSource,
};
use remora_protocol::{
    BridgeMessage, ChannelInput, ChannelOutput, ClientMessage, DeviceId, Envelope, FrameType,
    HelloRole, ProjectId, RelayHello, RemoteOp, SessionId, SessionMeta, SessionState,
    SessionStatus, SpawnSpec, PROTOCOL_VERSION,
};
use remora_relay::{serve, AuditSink, BridgeEntry, DeviceEntry, RelayConfig};

/// Fixed test tokens. Real deployments mint random ones; the loopback proof
/// only needs the relay config, bridge, and client to agree on a value.
const BRIDGE_TOKEN: &str = "loopback-bridge-token";
const RENDEZVOUS_TOKEN: &str = "loopback-rendezvous-token";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A generous ceiling for the async readiness gate (bridge registration /
/// reconnect). Far longer than the observed sub-second settle, so a slow CI box
/// never flakes; a genuine wiring break still fails within it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-message-arrival timeout. Once the route is live, a loopback round-trip is
/// sub-millisecond; two seconds is pure slack against scheduler noise.
const RECV_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Relay instance: a killable blind relay on its own runtime/thread.
// ---------------------------------------------------------------------------

/// One running relay, owning a private runtime so it can be hard-killed
/// (dropping the runtime aborts every relay task and closes its sockets).
struct Relay {
    addr: std::net::SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    /// Binds and serves `config` on a fresh background runtime, returning once
    /// the listener is bound (its concrete address is available in `addr`).
    fn start(config: Arc<RelayConfig>) -> Relay {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("relay runtime");
            rt.block_on(async move {
                let audit = AuditSink::new(&config).expect("audit sink");
                let (addr, _accept) = serve(config, audit).await.expect("relay serve");
                addr_tx.send(addr).expect("publish relay addr");
                // Park until killed; the detached accept + connection tasks run
                // on this runtime's workers meanwhile.
                let _ = shutdown_rx.await;
            });
            // `rt` drops here: all relay tasks are aborted, all sockets closed.
        });
        let addr = addr_rx.recv().expect("relay addr");
        Relay {
            addr,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    /// Hard-stops the relay: signals the runtime thread to return (dropping the
    /// runtime) and joins it, so on return every relay socket is closed.
    fn kill(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Harness.
// ---------------------------------------------------------------------------

/// Knobs a few tests need to bend from the defaults.
struct SetupOptions {
    /// Per-connection relay buffer budget (bytes).
    buffer_bytes: usize,
    /// When false, the relay still authorizes the device but the bridge's
    /// roster is emptied — the unpaired-device case (relay-admitted, unpinned).
    include_in_roster: bool,
}

impl Default for SetupOptions {
    fn default() -> SetupOptions {
        SetupOptions {
            buffer_bytes: 1 << 20,
            include_in_roster: true,
        }
    }
}

/// The fully wired stack: a client [`RemoteSource`], the running relay, and the
/// spawned bridge task. Dropping it tears the whole thing down.
struct Harness {
    remote: RemoteSource,
    /// The good pairing file (adversarial tests clone + corrupt it).
    pairing: PairingFile,
    relay: Relay,
    /// The relay config bound to the *concrete* port, for a same-port restart.
    relay_config: Arc<RelayConfig>,
    shutdown: CancellationToken,
    bridge_task: JoinHandle<()>,
}

impl Harness {
    async fn setup(source: Arc<dyn SessionSource>, opts: SetupOptions) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity =
            BridgeIdentity::load_or_create(&dir.path().join("identity.toml")).expect("identity");
        let mut roster = Roster::default();
        // Provision against a placeholder URL; the real one is stamped in once
        // the relay's ephemeral port is known.
        let mut pairing =
            provision_device(&identity, &mut roster, "ws://placeholder", RENDEZVOUS_TOKEN)
                .expect("provision device");

        let bridge_id = identity.device_id;
        let device_id = pairing.device_id;

        let bridges = vec![BridgeEntry {
            token: BRIDGE_TOKEN.to_string(),
            device_id: bridge_id,
        }];
        let devices = vec![DeviceEntry {
            token: RENDEZVOUS_TOKEN.to_string(),
            device_id,
            bridge_id,
        }];

        // Start the relay on an ephemeral port, then learn its address.
        let config0 = Arc::new(RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges: bridges.clone(),
            devices: devices.clone(),
            buffer_bytes: opts.buffer_bytes,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
        });
        let relay = Relay::start(config0);
        let addr = relay.addr;
        let relay_url = format!("ws://{addr}");
        pairing.relay_url = relay_url.clone();

        // A concrete-port twin config so a restart rebinds the same port.
        let relay_config = Arc::new(RelayConfig {
            listen: addr.to_string(),
            bridges,
            devices,
            buffer_bytes: opts.buffer_bytes,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
        });

        if !opts.include_in_roster {
            roster = Roster::default();
        }

        let shutdown = CancellationToken::new();
        let bridge_cfg = BridgeConfig {
            relay_url,
            registration_token: BRIDGE_TOKEN.to_string(),
            identity,
            roster,
        };
        // The bridge serves through the same per-session-locked seam the desktop
        // uses (ADR-0021 D7): wrap the source in an ExclusiveSource.
        let bridge_source: Arc<dyn SessionSource> = Arc::new(ExclusiveSource::new(
            source,
            SessionLocks::new(),
            "loopback",
        ));
        let shutdown_c = shutdown.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = serve_bridge(bridge_cfg, bridge_source, shutdown_c).await;
        });

        let remote = RemoteSource::new(pairing.clone());
        Harness {
            remote,
            pairing,
            relay,
            relay_config,
            shutdown,
            bridge_task,
        }
    }

    /// Default wiring: 1 MiB buffer, device pinned in the roster.
    async fn with_source(source: Arc<dyn SessionSource>) -> Harness {
        Harness::setup(source, SetupOptions::default()).await
    }

    /// Readiness gate. The bridge registers with the relay asynchronously (an
    /// outbound dial plus relay hello, retried with backoff), so a client
    /// `list()` races that registration and can transiently fail with a
    /// `PeerUnavailable`. Poll until it succeeds or the generous timeout
    /// elapses — never a bare sleep.
    async fn wait_ready(&self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last: Option<remora_core::SourceError> = None;
        while Instant::now() < deadline {
            match self.remote.list().await {
                Ok(_) => return,
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        panic!("bridge never became ready within {READY_TIMEOUT:?}: {last:?}");
    }

    /// Hard-kills the relay and restarts it on the same concrete port. The
    /// bridge's reconnect loop redials the unchanged `ws://addr` on its own.
    async fn restart_relay(&mut self) {
        self.relay.kill();
        self.relay = Relay::start(self.relay_config.clone());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.bridge_task.abort();
        self.relay.kill();
    }
}

// ---------------------------------------------------------------------------
// A scripted source: the test drives its output stream and observes its input.
// ---------------------------------------------------------------------------

/// The transport-facing ends handed out on each `attach`, published to the test
/// so it can inject arbitrary [`ChannelOutput`]s (the real fake only echoes
/// `Bytes`) and observe forwarded [`ChannelInput`].
struct AttachEnds {
    input_rx: mpsc::Receiver<ChannelInput>,
    output_tx: mpsc::Sender<ChannelOutput>,
}

/// A minimal `SessionSource` whose attach channel is driven by the test. `list`
/// returns a fixed roster so the readiness gate and attach targeting work; the
/// mutating ops are unsupported (this double exists only to script a stream).
struct ScriptedSource {
    sessions: Vec<SessionMeta>,
    attaches: mpsc::UnboundedSender<AttachEnds>,
}

impl ScriptedSource {
    fn new(
        sessions: Vec<SessionMeta>,
    ) -> (Arc<ScriptedSource>, mpsc::UnboundedReceiver<AttachEnds>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(ScriptedSource {
                sessions,
                attaches: tx,
            }),
            rx,
        )
    }
}

fn scripted_unsupported() -> remora_core::SourceError {
    remora_core::SourceError::Transport("scripted source: unsupported op".to_string())
}

#[async_trait]
impl SessionSource for ScriptedSource {
    async fn spawn(&self, _spec: SpawnSpec) -> Result<SessionChannel, remora_core::SourceError> {
        Err(scripted_unsupported())
    }

    async fn attach(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
    ) -> Result<SessionChannel, remora_core::SourceError> {
        let (channel, input_rx, output_tx) = SessionChannel::pair();
        // Publish the transport ends so the test can drive this attach.
        let _ = self.attaches.send(AttachEnds {
            input_rx,
            output_tx,
        });
        Ok(channel)
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, remora_core::SourceError> {
        Ok(self.sessions.clone())
    }

    async fn respawn(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
        _agent: Option<remora_protocol::AgentId>,
    ) -> Result<SessionChannel, remora_core::SourceError> {
        Err(scripted_unsupported())
    }

    async fn stop(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
    ) -> Result<(), remora_core::SourceError> {
        Err(scripted_unsupported())
    }

    async fn remove(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
        _force: bool,
    ) -> Result<(), remora_core::SourceError> {
        Err(scripted_unsupported())
    }
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

fn ids(project: &str, session: &str) -> (ProjectId, SessionId) {
    (
        ProjectId::new(project).expect("valid project slug"),
        SessionId::new(session).expect("valid session slug"),
    )
}

fn spec(project: &str, session: &str) -> SpawnSpec {
    let (project_id, session_id) = ids(project, session);
    SpawnSpec {
        project_id,
        session_id,
        agent: None,
        base: None,
        workspace: None,
        branch: None,
        worktree_root: None,
    }
}

fn session_meta(project: &str, session: &str) -> SessionMeta {
    let (project_id, session_id) = ids(project, session);
    SessionMeta {
        project_id,
        session_id,
        state: SessionState::Live,
        agent: None,
        created_at: None,
        workspace_path: None,
        workspace: None,
        branch: None,
    }
}

/// Receives the next output, failing the test on death or timeout.
async fn recv_out(channel: &mut SessionChannel) -> ChannelOutput {
    match tokio::time::timeout(RECV_TIMEOUT, channel.recv()).await {
        Ok(Some(out)) => out,
        Ok(None) => panic!("channel died while awaiting output"),
        Err(_) => panic!("timed out waiting for channel output"),
    }
}

/// Receives the next output, asserting it is `Bytes`.
async fn recv_bytes(channel: &mut SessionChannel) -> Vec<u8> {
    match recv_out(channel).await {
        ChannelOutput::Bytes(bytes) => bytes,
        other => panic!("expected Bytes output, got {other:?}"),
    }
}

/// Decodes the pairing file's Noise key material (device priv, bridge pub, psk).
fn decode_keys(p: &PairingFile) -> (Vec<u8>, Vec<u8>, [u8; 32]) {
    let device_priv = B64
        .decode(&p.device_private_key)
        .expect("device private key b64");
    let bridge_pub = B64
        .decode(&p.bridge_static_pubkey)
        .expect("bridge public key b64");
    let psk_bytes = B64.decode(&p.psk).expect("psk b64");
    let psk: [u8; 32] = psk_bytes.try_into().expect("psk is 32 bytes");
    (device_priv, bridge_pub, psk)
}

// ---------------------------------------------------------------------------
// A hand-rolled raw client for the adversarial tests.
// ---------------------------------------------------------------------------

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// A minimal relay-mode client with the guardrails removed: it drives the relay
/// hello + IKpsk2 handshake exactly like [`RemoteSource`], then lets the test
/// send *arbitrary* application frames (a wrong-version hello, garbage
/// ciphertext) that the safe client would never emit. Reused across the
/// version-mismatch and tamper tests.
struct RawClient {
    sink: WsSink,
    stream: WsStream,
    transport: Transport,
    routing_id: DeviceId,
    bridge_id: DeviceId,
}

impl RawClient {
    /// Dials the relay and completes the Noise handshake, stopping *before* the
    /// E2E hello so the caller controls that first application frame.
    async fn connect(pairing: &PairingFile) -> RawClient {
        let (device_priv, bridge_pub, psk) = decode_keys(pairing);
        let device_id = pairing.device_id;
        let bridge_id = pairing.bridge_id;

        let (ws, _resp) = connect_async(&pairing.relay_url)
            .await
            .expect("raw client dial");
        let (mut sink, mut stream) = ws.split();

        let routing_id = DeviceId(rand::random());
        let hello = RelayHello {
            role: HelloRole::Device,
            token: pairing.rendezvous_token.clone(),
            device_id,
            routing_id,
            bridge_id,
        };
        let hello_payload = serde_json::to_vec(&hello).expect("serialize relay hello");
        send_frame(
            &mut sink,
            FrameType::Hello,
            routing_id,
            DeviceId::ZERO,
            hello_payload,
        )
        .await;

        let bound = prologue(&device_id, &routing_id, &bridge_id);
        let mut hs =
            Handshake::initiator(&device_priv, &bridge_pub, &psk, &bound).expect("build initiator");
        let msg1 = hs.write_message(&[]).expect("write msg1");
        let mut first = Vec::with_capacity(32 + msg1.len());
        first.extend_from_slice(&device_id.0);
        first.extend_from_slice(&msg1);
        send_frame(&mut sink, FrameType::Data, routing_id, bridge_id, first).await;

        let msg2 = recv_payload(&mut stream, routing_id)
            .await
            .expect("bridge msg2");
        hs.read_message(&msg2).expect("read msg2");
        let (transport, _remote_static) = hs.into_transport().expect("into transport");

        RawClient {
            sink,
            stream,
            transport,
            routing_id,
            bridge_id,
        }
    }

    /// Seals and sends one application message.
    async fn send(&mut self, msg: &ClientMessage) {
        let ciphertext = self.transport.seal(msg).expect("seal client message");
        send_frame(
            &mut self.sink,
            FrameType::Data,
            self.routing_id,
            self.bridge_id,
            ciphertext,
        )
        .await;
    }

    /// Sends a raw (unsealed) Data payload — used to inject garbage ciphertext.
    async fn send_raw(&mut self, payload: Vec<u8>) {
        send_frame(
            &mut self.sink,
            FrameType::Data,
            self.routing_id,
            self.bridge_id,
            payload,
        )
        .await;
    }

    /// Awaits the next bridge message, or `None` on timeout/connection death.
    async fn recv(&mut self, dur: Duration) -> Option<BridgeMessage> {
        let payload = tokio::time::timeout(dur, recv_payload(&mut self.stream, self.routing_id))
            .await
            .ok()??;
        self.transport.open::<BridgeMessage>(&payload).ok()
    }
}

/// Writes one enveloped frame to a raw client's sink.
async fn send_frame(
    sink: &mut WsSink,
    frame_type: FrameType,
    src: DeviceId,
    dst: DeviceId,
    payload: Vec<u8>,
) {
    let frame = Envelope {
        frame_type,
        src,
        dst,
        payload,
    }
    .encode();
    sink.send(Message::Binary(frame.into()))
        .await
        .expect("raw client write");
}

/// Reads inbound frames until one is a Data frame addressed to `routing_id`,
/// returning its payload; `None` on close/EOF/error.
async fn recv_payload(stream: &mut WsStream, routing_id: DeviceId) -> Option<Vec<u8>> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let envelope = Envelope::decode(&bytes).ok()?;
                if envelope.frame_type == FrameType::Data && envelope.dst == routing_id {
                    return Some(envelope.payload);
                }
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// The core proof: a byte survives the round trip in both directions through
/// the relay + real Noise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_attach_echoes_bytes_both_ways() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "fix-login")).await.expect("spawn");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    // `list` sees the seeded session over the wire.
    let listed = harness.remote.list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id.as_str(), "fix-login");

    let (project, session) = ids("api", "fix-login");
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach");

    // Bridge → client: the fake's attach banner arrives first.
    assert_eq!(
        recv_bytes(&mut channel).await,
        b"[fake attach api_fix-login]\r\n"
    );

    // Client → bridge → fake → bridge → client: a full echo round trip.
    channel.send_bytes(b"hi".to_vec()).await.expect("send hi");
    assert_eq!(recv_bytes(&mut channel).await, b"hi");
}

/// Activity events (not just raw bytes) ride the same Noise stream, in order,
/// after the bytes that preceded them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_and_marker_events_ride_through() {
    let (scripted, mut attaches) = ScriptedSource::new(vec![session_meta("api", "one")]);
    let harness = Harness::with_source(scripted).await;
    harness.wait_ready().await;

    let (project, session) = ids("api", "one");
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach");

    // The scripted attach published its transport ends; drive the stream.
    let ends = attaches.recv().await.expect("attach ends published");
    // Keep the input receiver alive so the bridge's input side stays open.
    let _input_alive = ends.input_rx;
    let out = ends.output_tx;
    out.send(ChannelOutput::Bytes(b"boot".to_vec()))
        .await
        .expect("send bytes");
    out.send(ChannelOutput::StatusChange(SessionStatus::Awaiting))
        .await
        .expect("send status");
    out.send(ChannelOutput::PreviewUpdate("run tests? (y/n)".to_string()))
        .await
        .expect("send preview");
    out.send(ChannelOutput::MarkerSeen)
        .await
        .expect("send marker");

    assert_eq!(
        recv_out(&mut channel).await,
        ChannelOutput::Bytes(b"boot".to_vec())
    );
    assert_eq!(
        recv_out(&mut channel).await,
        ChannelOutput::StatusChange(SessionStatus::Awaiting)
    );
    assert_eq!(
        recv_out(&mut channel).await,
        ChannelOutput::PreviewUpdate("run tests? (y/n)".to_string())
    );
    assert_eq!(recv_out(&mut channel).await, ChannelOutput::MarkerSeen);
}

/// D16: two concurrent attaches are two independent connections (per-call dial),
/// each with its own routing id and bridge peer task — both work at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_attach_two_connections_both_work() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn one");
    fake.spawn(spec("api", "two")).await.expect("spawn two");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    let (p1, s1) = ids("api", "one");
    let (p2, s2) = ids("api", "two");
    let (a, b) = tokio::join!(
        harness.remote.attach(&p1, &s1),
        harness.remote.attach(&p2, &s2)
    );
    let mut chan_a = a.expect("attach one");
    let mut chan_b = b.expect("attach two");

    assert_eq!(recv_bytes(&mut chan_a).await, b"[fake attach api_one]\r\n");
    assert_eq!(recv_bytes(&mut chan_b).await, b"[fake attach api_two]\r\n");

    chan_a.send_bytes(b"aaa".to_vec()).await.expect("send a");
    chan_b.send_bytes(b"bbb".to_vec()).await.expect("send b");
    assert_eq!(recv_bytes(&mut chan_a).await, b"aaa");
    assert_eq!(recv_bytes(&mut chan_b).await, b"bbb");
}

/// A client claiming a future protocol version is rejected: the bridge still
/// answers Hello (so the client can fail closed too) but then drops the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_mismatch_is_rejected() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    let mut raw = RawClient::connect(&harness.pairing).await;
    raw.send(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION + 1,
    })
    .await;

    // The bridge answers its own version (fail-closed handshake), which the
    // client sees differs from what it claimed.
    match raw.recv(RECV_TIMEOUT).await {
        Some(BridgeMessage::Hello { protocol_version }) => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_ne!(protocol_version, PROTOCOL_VERSION + 1);
        }
        other => panic!("expected bridge Hello, got {other:?}"),
    }

    // Then the peer is dead: a follow-up request draws no response.
    let id = 7;
    raw.send(&ClientMessage::Request {
        id,
        op: RemoteOp::List,
    })
    .await;
    assert!(
        raw.recv(Duration::from_millis(500)).await.is_none(),
        "bridge must drop a version-mismatched peer (no further responses)"
    );

    // The bridge itself is unharmed: a well-behaved client still works.
    let good = harness.remote.list().await.expect("bridge still serves");
    assert_eq!(good.len(), 1);
}

/// A valid envelope carrying garbage ciphertext to the bridge's routing id is
/// dropped cleanly (that one peer dies); the bridge does not crash and a fresh
/// legit attach still works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_tamper_kills_cleanly() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    // Complete a real handshake + E2E hello, then inject garbage.
    let mut raw = RawClient::connect(&harness.pairing).await;
    raw.send(&ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
    })
    .await;
    match raw.recv(RECV_TIMEOUT).await {
        Some(BridgeMessage::Hello { protocol_version }) => {
            assert_eq!(protocol_version, PROTOCOL_VERSION)
        }
        other => panic!("expected bridge Hello, got {other:?}"),
    }
    // 48 random bytes: decodes as a Data envelope, fails Noise `open`.
    raw.send_raw((0..48).map(|_| rand::random::<u8>()).collect())
        .await;
    // The tampered peer draws no valid response.
    assert!(
        raw.recv(Duration::from_millis(500)).await.is_none(),
        "tampered peer must be dropped"
    );
    drop(raw);

    // A fresh legit attach still works — the bridge survived the tamper.
    let (project, session) = ids("api", "one");
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach after tamper");
    assert_eq!(recv_bytes(&mut channel).await, b"[fake attach api_one]\r\n");
    channel.send_bytes(b"alive".to_vec()).await.expect("send");
    assert_eq!(recv_bytes(&mut channel).await, b"alive");
}

/// A corrupted PSK fails the handshake with a Transport error; the relay and
/// bridge stay up (a correctly-paired client still works).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_psk_fails() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    // Clone the pairing but swap in a bogus (valid-length) PSK.
    let mut bad = harness.pairing.clone();
    bad.psk = B64.encode([0xff; 32]);
    let bad_remote = RemoteSource::new(bad);

    let (project, session) = ids("api", "one");
    let err = bad_remote
        .attach(&project, &session)
        .await
        .expect_err("wrong psk must fail attach");
    assert!(
        matches!(err, remora_core::SourceError::Transport(_)),
        "expected Transport error, got {err:?}"
    );

    // Relay + bridge survive: the good client still lists and attaches.
    assert_eq!(harness.remote.list().await.expect("list").len(), 1);
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("good attach still works");
    assert_eq!(recv_bytes(&mut channel).await, b"[fake attach api_one]\r\n");
}

/// The bridge reconnects after the relay is hard-killed and restarted on the
/// same port; a new attach then succeeds, while the pre-restart channel died.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_reconnects_after_relay_restart() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let mut harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    let (project, session) = ids("api", "one");
    let mut old = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach before restart");
    assert_eq!(recv_bytes(&mut old).await, b"[fake attach api_one]\r\n");

    // Hard-kill + restart the relay on the same port.
    harness.restart_relay().await;

    // The old channel died structurally when its connection dropped.
    assert!(
        tokio::time::timeout(RECV_TIMEOUT, old.recv())
            .await
            .expect("old channel should report death promptly")
            .is_none(),
        "old channel must be dead after the relay restart"
    );

    // The bridge's backoff reconnect re-registers; wait for the route, then a
    // fresh attach works end to end again.
    harness.wait_ready().await;
    let mut fresh = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach after restart");
    assert_eq!(recv_bytes(&mut fresh).await, b"[fake attach api_one]\r\n");
    fresh.send_bytes(b"back".to_vec()).await.expect("send");
    assert_eq!(recv_bytes(&mut fresh).await, b"back");
}

/// A single large PTY burst is chunked by the bridge and reassembled in order by
/// the client across multiple `Bytes` messages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_pty_burst_survives_chunking() {
    let (scripted, mut attaches) = ScriptedSource::new(vec![session_meta("api", "one")]);
    // A generous relay buffer so the burst is never load-shed as a slow peer.
    let harness = Harness::setup(
        scripted,
        SetupOptions {
            buffer_bytes: 16 << 20,
            ..SetupOptions::default()
        },
    )
    .await;
    harness.wait_ready().await;

    let (project, session) = ids("api", "one");
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach");
    let ends = attaches.recv().await.expect("attach ends");
    let _input_alive = ends.input_rx;

    // One 256 KiB output message, deterministic pattern.
    let total = 256 * 1024;
    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    ends.output_tx
        .send(ChannelOutput::Bytes(data.clone()))
        .await
        .expect("send burst");

    // Reassemble across however many Bytes messages the chunking produced.
    let mut got = Vec::with_capacity(total);
    let mut messages = 0usize;
    while got.len() < total {
        got.extend_from_slice(&recv_bytes(&mut channel).await);
        messages += 1;
    }
    assert_eq!(
        got, data,
        "reassembled burst must match byte-for-byte, in order"
    );
    assert!(
        messages >= 2,
        "a 256 KiB burst must arrive across multiple chunks, got {messages}"
    );
}

/// A device the relay admits but the bridge's roster does not pin cannot attach:
/// the bridge silently drops the unpinned peer, so no channel is ever opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpaired_device_fails() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let harness = Harness::setup(
        fake,
        SetupOptions {
            include_in_roster: false,
            ..SetupOptions::default()
        },
    )
    .await;

    // Availability-only: the unpinned device never completes an attach. (The
    // bridge drops the peer at the roster check without replying, and the slice-1
    // client has no handshake deadline, so the observable outcome is "no attach"
    // rather than a prompt typed error — see the report's concerns.)
    let (project, session) = ids("api", "one");
    let outcome = tokio::time::timeout(
        Duration::from_millis(750),
        harness.remote.attach(&project, &session),
    )
    .await;
    match outcome {
        Err(_) => {}     // pending forever: attach never succeeded
        Ok(Err(_)) => {} // or a prompt transport error — both prove unavailability
        Ok(Ok(_)) => panic!("an unpaired device must not attach"),
    }
}

/// Not a correctness test: measures loopback round-trip latency (byte → echo)
/// and prints p50/p95/max. Ignored by default; run with:
/// `cargo test -p remora-bridge --test loopback -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "latency probe, run manually with --ignored --nocapture"]
async fn loopback_latency_measurement() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    let (project, session) = ids("api", "one");
    let mut channel = harness
        .remote
        .attach(&project, &session)
        .await
        .expect("attach");
    let _banner = recv_bytes(&mut channel).await;

    let rounds = 200usize;
    let mut samples: Vec<u128> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        channel.send_bytes(b"x".to_vec()).await.expect("send");
        let echoed = recv_bytes(&mut channel).await;
        assert_eq!(echoed, b"x");
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    let pct = |p: f64| -> u128 {
        let idx = ((p / 100.0) * (samples.len() - 1) as f64).round() as usize;
        samples[idx]
    };
    println!(
        "loopback round-trip latency over {rounds} samples: p50={}us p95={}us max={}us",
        pct(50.0),
        pct(95.0),
        samples[samples.len() - 1]
    );
}
