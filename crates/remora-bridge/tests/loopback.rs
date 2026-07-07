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
use rand::Rng as _;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use remora_bridge::{
    prologue, run_pairing, serve_bridge, wake_channel, BridgeConfig, BridgeEvent, BridgeHealth,
    BridgeIdentity, Handshake, HandshakeKind, PairingCommand, PairingError, PairingFile,
    PairingOutcome, PairingProgress, RemoteSource, Roster, RosterEntry, Transport, NOISE_PATTERN,
};
use remora_core::{
    ExclusiveSource, FakeSessionSource, SessionChannel, SessionLocks, SessionSource,
};
use remora_protocol::{
    BridgeMessage, ChannelInput, ChannelOutput, ClientMessage, DeviceId, Envelope, FrameType,
    HelloRole, PairingBridgeMsg, PairingClientMsg, PairingCode, PairingRejectReason, ProjectId,
    RelayHello, RemoteOp, RemoteResult, SessionId, SessionMeta, SessionState, SessionStatus,
    SpawnSpec, PROTOCOL_VERSION,
};
use remora_relay::{serve, AuditSink, BridgeEntry, PushConfig, RelayConfig};

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

/// Ceiling for awaiting a single bridge [`BridgeEvent`] driven by the real
/// pairing ceremony. Generous slack above the sub-second loopback ceremony (and
/// above the short pairing-window TTLs the tests use for expiry paths).
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling for awaiting a [`run_pairing`] task to *complete* the whole ceremony.
/// Wider than [`EVENT_TIMEOUT`] and above the driver's own 15 s relay-read budget,
/// so a slow-but-correct completion on a loaded box surfaces the driver's real
/// outcome rather than tripping the outer bound and flaking.
const PAIR_JOIN_TIMEOUT: Duration = Duration::from_secs(25);

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
    /// Held open so the bridge's command branch never observes a closed channel
    /// (Task 14 drives commands through it); dropped when the harness drops.
    _commands_tx: mpsc::Sender<PairingCommand>,
    /// Held so the bridge's event sender always has a live receiver.
    _events_rx: mpsc::Receiver<BridgeEvent>,
    /// Held open so the bridge's wake branch (#233) never observes a closed
    /// channel; the wake path is otherwise unexercised in the loopback tests.
    _wake_handle: remora_bridge::BridgeWakeHandle,
    /// The bridge's health watch (spec D8, #234), observed by the health tests.
    health: tokio::sync::watch::Receiver<BridgeHealth>,
}

impl Harness {
    async fn setup(source: Arc<dyn SessionSource>, opts: SetupOptions) -> Harness {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity =
            BridgeIdentity::load_or_create(&dir.path().join("identity.toml")).expect("identity");
        let mut roster = Roster::default();

        // Loopback-harness scaffolding: mints a device + pairing file inline
        // (replaces the slice-1 `provision_device` helper, deleted in #232);
        // Task 14 drives this harness through the real pairing ceremony
        // instead.
        let bridge_id = identity.device_id;
        let device_keypair = {
            let params: snow::params::NoiseParams =
                NOISE_PATTERN.parse().expect("valid noise pattern");
            snow::Builder::new(params)
                .generate_keypair()
                .expect("generate device keypair")
        };
        let device_id = DeviceId(rand::random());
        let mut psk = [0u8; 32];
        rand::rng().fill_bytes(&mut psk);
        roster.entries.push(RosterEntry {
            device_id,
            static_pubkey: device_keypair.public.clone(),
            psk,
            relay_token: RENDEZVOUS_TOKEN.to_string(),
            name: "loopback test device".to_string(),
            enrolled_at: None,
            last_connected_at: None,
            push: None,
        });
        // Provision against a placeholder URL; the real one is stamped in once
        // the relay's ephemeral port is known.
        let mut pairing = PairingFile {
            relay_url: "ws://placeholder".to_string(),
            device_token: RENDEZVOUS_TOKEN.to_string(),
            bridge_id,
            bridge_static_pubkey: B64.encode(&identity.static_keypair.public),
            psk: B64.encode(psk),
            device_id,
            device_private_key: B64.encode(&device_keypair.private),
            device_public_key: B64.encode(&device_keypair.public),
        };

        let bridges = vec![BridgeEntry {
            token: BRIDGE_TOKEN.to_string(),
            device_id: bridge_id,
        }];

        // Start the relay on an ephemeral port, then learn its address.
        let config0 = Arc::new(RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges: bridges.clone(),
            buffer_bytes: opts.buffer_bytes,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
            push: PushConfig::default(),
        });
        let relay = Relay::start(config0);
        let addr = relay.addr;
        let relay_url = format!("ws://{addr}");
        pairing.relay_url = relay_url.clone();

        // A concrete-port twin config so a restart rebinds the same port.
        let relay_config = Arc::new(RelayConfig {
            listen: addr.to_string(),
            bridges,
            buffer_bytes: opts.buffer_bytes,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
            push: PushConfig::default(),
        });

        if !opts.include_in_roster {
            roster = Roster::default();
        }

        let shutdown = CancellationToken::new();
        let (health_tx, health_rx) = tokio::sync::watch::channel(BridgeHealth::Starting);
        let bridge_cfg = BridgeConfig {
            relay_url,
            registration_token: BRIDGE_TOKEN.to_string(),
            identity,
            roster: Arc::new(tokio::sync::RwLock::new(roster)),
            // A never-written path: these tests never mutate the roster (Task 14
            // drives the pairing/revocation ceremony that persists it).
            roster_path: dir.path().join("bridge_roster.toml"),
            health: health_tx,
        };
        // The bridge serves through the same per-session-locked seam the desktop
        // uses (ADR-0021 D7): wrap the source in an ExclusiveSource.
        let bridge_source: Arc<dyn SessionSource> = Arc::new(ExclusiveSource::new(
            source,
            SessionLocks::new(),
            "loopback",
        ));
        // Task 10 wires the pairing command/event channels; this task does not
        // drive them (Task 14 does), so hold the command sender open so the
        // bridge's command branch never spuriously closes, and drain events.
        let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(8);
        let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(8);
        // The wake path (#233) is unused here; hold the handle so its channel
        // stays open (a dropped handle just closes the bridge's wake branch).
        let (wake_handle, wake_rx) = wake_channel();
        let shutdown_c = shutdown.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = serve_bridge(
                bridge_cfg,
                bridge_source,
                commands_rx,
                events_tx,
                wake_rx,
                shutdown_c,
            )
            .await;
        });

        let remote = RemoteSource::new(pairing.clone());
        Harness {
            remote,
            pairing,
            relay,
            relay_config,
            shutdown,
            bridge_task,
            _commands_tx: commands_tx,
            _events_rx: events_rx,
            _wake_handle: wake_handle,
            health: health_rx,
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

    async fn external_attach_command(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
    ) -> Result<Vec<String>, remora_core::SourceError> {
        Err(scripted_unsupported())
    }

    async fn remote_workspace(
        &self,
        _project_id: &ProjectId,
        _session_id: &SessionId,
        _workspace_path: &str,
    ) -> Result<remora_core::RemoteWorkspace, remora_core::SourceError> {
        Err(scripted_unsupported())
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
            token: pairing.device_token.clone(),
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

        let bound = prologue(HandshakeKind::Session, &device_id, &routing_id, &bridge_id);
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

/// Health watch across a real reconnect cycle (spec D8, #234): a successful
/// bridge⇄relay registration publishes `Connected`; killing the relay drives
/// `Reconnecting` whose `attempts` grow across one outage while its `since`
/// anchor stays put; a successful reconnect then RESETS the outage state, so
/// the next relay death starts a fresh anchor with `attempts` back at 1
/// (never continuing the previous outage's count).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_publishes_connected_and_resets_outage_across_reconnect() {
    let fake = Arc::new(FakeSessionSource::new());
    let mut harness = Harness::with_source(fake).await;
    let mut health = harness.health.clone();

    // --- Phase 1: initial registration reaches Connected. ---
    let connected_since = {
        let value = tokio::time::timeout(
            READY_TIMEOUT,
            health.wait_for(|h| matches!(h, BridgeHealth::Connected { .. })),
        )
        .await
        .expect("health should reach Connected within the ready timeout")
        .expect("health channel open");
        match *value {
            BridgeHealth::Connected { since } => since,
            ref other => unreachable!("wait_for matched Connected, got {other:?}"),
        }
    };

    // --- Phase 2: kill the relay; the established connection drops. The first
    // publication of this outage must be Reconnecting with attempts == 1.
    // Watch coalescing cannot skip it: the next publication (a failed redial)
    // is at least ~100 ms of jittered backoff away, while this observer is
    // already awaiting.
    harness.relay.kill();
    tokio::time::timeout(RECV_TIMEOUT, health.changed())
        .await
        .expect("health should change promptly after the relay dies")
        .expect("health channel open");
    let (outage1_since, mut seen_attempts) = match *health.borrow_and_update() {
        BridgeHealth::Reconnecting { since, attempts } => (since, attempts),
        ref other => panic!("expected Reconnecting after relay death, got {other:?}"),
    };
    assert_eq!(
        seen_attempts, 1,
        "a fresh outage starts counting attempts at 1"
    );
    assert!(
        outage1_since >= connected_since,
        "outage anchor ({outage1_since}) must not predate the connection ({connected_since})"
    );

    // --- Phase 3: let the outage deepen (failed redials against the dead
    // port). Every subsequent Reconnecting shares the SAME since anchor while
    // attempts grow — driving attempts >= 2 makes the later reset observable.
    while seen_attempts < 2 {
        tokio::time::timeout(RECV_TIMEOUT, health.changed())
            .await
            .expect("next failed redial should publish within the backoff window")
            .expect("health channel open");
        match *health.borrow_and_update() {
            BridgeHealth::Reconnecting { since, attempts } => {
                assert_eq!(
                    since, outage1_since,
                    "consecutive failed attempts share one outage anchor"
                );
                assert!(
                    attempts > seen_attempts,
                    "attempts must grow monotonically ({attempts} vs {seen_attempts})"
                );
                seen_attempts = attempts;
            }
            ref other => panic!("expected Reconnecting during the outage, got {other:?}"),
        }
    }

    // --- Phase 4: restart the relay on the same port; the bridge's backoff
    // redial re-registers and publishes Connected again.
    harness.relay = Relay::start(harness.relay_config.clone());
    let reconnected_since = {
        let value = tokio::time::timeout(
            READY_TIMEOUT,
            health.wait_for(|h| matches!(h, BridgeHealth::Connected { .. })),
        )
        .await
        .expect("health should return to Connected after the relay restart")
        .expect("health channel open");
        match *value {
            BridgeHealth::Connected { since } => since,
            ref other => unreachable!("wait_for matched Connected, got {other:?}"),
        }
    };

    // --- Phase 5: a second relay death starts a FRESH outage: attempts is
    // back at 1 (it was >= 2 before the reconnect — the reset is real, not a
    // continuation), and the anchor is re-taken at/after the reconnection.
    harness.relay.kill();
    tokio::time::timeout(RECV_TIMEOUT, health.changed())
        .await
        .expect("health should change promptly after the second relay death")
        .expect("health channel open");
    match *health.borrow_and_update() {
        BridgeHealth::Reconnecting { since, attempts } => {
            assert_eq!(
                attempts, 1,
                "a successful reconnect resets the attempt counter"
            );
            assert!(
                since >= reconnected_since,
                "second outage anchor ({since}) must be fresh — at/after the \
                 reconnection ({reconnected_since}), not the first outage's \
                 ({outage1_since})"
            );
        }
        ref other => panic!("expected Reconnecting after the second relay death, got {other:?}"),
    }

    // Clean shutdown keeps the loop's exit path honest.
    harness.shutdown.cancel();
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

    // The unpinned device never gets a channel (the security property): the
    // bridge drops the peer at the roster check without replying. It no longer
    // *hangs*, though — the client's bounded relay read (RELAY_READ_TIMEOUT)
    // turns the silent drop into a prompt typed `Transport` error (#231). The
    // outer timeout is a generous ceiling above that read deadline so a genuine
    // hang would still fail the test rather than wedge CI.
    let (project, session) = ids("api", "one");
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        harness.remote.attach(&project, &session),
    )
    .await
    .expect("attach must return (typed error), not hang past the read deadline");
    match outcome {
        Err(remora_core::SourceError::Transport(_)) => {} // prompt typed unavailability
        Ok(_) => panic!("an unpaired device must not attach"),
        Err(other) => panic!("expected a Transport error, got {other:?}"),
    }
}

/// `ListDevices` over the wire returns the bridge's roster projected as
/// `DeviceInfo`s: the single paired device, marked `is_self` for the requester,
/// with `last_connected_at` stamped by the session it is asking over. The full
/// revoke-kick E2E (`revoke_kicks_live_device`) is Task 14's; this proves the
/// read side and the connect-time stamp end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_devices_returns_self_with_connect_stamp() {
    let fake = Arc::new(FakeSessionSource::new());
    let harness = Harness::with_source(fake).await;
    harness.wait_ready().await;

    // Drive a real handshake + E2E hello, then ask for the device list.
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

    raw.send(&ClientMessage::Request {
        id: 1,
        op: RemoteOp::ListDevices,
    })
    .await;
    match raw.recv(RECV_TIMEOUT).await {
        Some(BridgeMessage::Response {
            id: 1,
            result: RemoteResult::Devices(devices),
        }) => {
            assert_eq!(devices.len(), 1, "one paired device in the roster");
            let d = &devices[0];
            assert_eq!(d.device_id, harness.pairing.device_id);
            assert!(d.is_self, "the requesting device is marked is_self");
            assert!(
                d.last_connected_at.is_some(),
                "a successful session stamps last_connected_at"
            );
        }
        other => panic!("expected Response(Devices), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Real-pairing harness + E2E lifecycle tests (Task 14, ADR-0021 D3/D6).
//
// The `Harness` above pre-populates the roster and mints the `PairingFile`
// inline (Task 6 scaffolding) — the right shape for the PTY/echo/adversarial
// proofs, which need a *paired* device to exercise the session path and would
// only be slowed by re-pairing per test. This `PairingHarness` instead starts
// with an EMPTY roster and drives the *real* ceremony end to end (OpenWindow →
// run_pairing → Confirm → PairingFile → RemoteSource), so the confirm-gated
// grant, revoke kick, and abandoned-confirm paths are proven over real sockets
// and real Noise, not asserted from unit seams.
// ---------------------------------------------------------------------------

/// A relay + bridge stood up with an empty roster, plus the desktop-side command
/// sender, event receiver, and a handle on the shared roster — everything a test
/// needs to drive and observe the pairing ceremony. Dropping it tears the stack
/// down.
struct PairingHarness {
    relay: Relay,
    /// Shared with the bridge; the confirm path pushes the enrolled entry here.
    roster: Arc<RwLock<Roster>>,
    commands_tx: mpsc::Sender<PairingCommand>,
    events_rx: mpsc::Receiver<BridgeEvent>,
    shutdown: CancellationToken,
    bridge_task: JoinHandle<()>,
    /// Held open so the bridge's wake branch (#233) stays live; unexercised here.
    _wake_handle: remora_bridge::BridgeWakeHandle,
    _dir: tempfile::TempDir,
}

impl PairingHarness {
    async fn new(source: Arc<dyn SessionSource>) -> PairingHarness {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity =
            BridgeIdentity::load_or_create(&dir.path().join("identity.toml")).expect("identity");
        let bridge_id = identity.device_id;
        // Shared roster (empty): the bridge enrols the paired device into *this*
        // handle, so the test can assert the durable trust boundary directly.
        let roster = Arc::new(RwLock::new(Roster::default()));

        let bridges = vec![BridgeEntry {
            token: BRIDGE_TOKEN.to_string(),
            device_id: bridge_id,
        }];
        let config0 = Arc::new(RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges,
            buffer_bytes: 1 << 20,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
            push: PushConfig::default(),
        });
        let relay = Relay::start(config0);
        let relay_url = format!("ws://{}", relay.addr);

        let shutdown = CancellationToken::new();
        let bridge_cfg = BridgeConfig {
            relay_url: relay_url.clone(),
            registration_token: BRIDGE_TOKEN.to_string(),
            identity,
            roster: roster.clone(),
            roster_path: dir.path().join("bridge_roster.toml"),
            health: tokio::sync::watch::channel(BridgeHealth::Starting).0,
        };
        let bridge_source: Arc<dyn SessionSource> = Arc::new(ExclusiveSource::new(
            source,
            SessionLocks::new(),
            "loopback-pairing",
        ));
        let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(8);
        let (events_tx, events_rx) = mpsc::channel::<BridgeEvent>(32);
        let (wake_handle, wake_rx) = wake_channel();
        let shutdown_c = shutdown.clone();
        let bridge_task = tokio::spawn(async move {
            let _ = serve_bridge(
                bridge_cfg,
                bridge_source,
                commands_rx,
                events_tx,
                wake_rx,
                shutdown_c,
            )
            .await;
        });

        PairingHarness {
            relay,
            roster,
            commands_tx,
            events_rx,
            shutdown,
            bridge_task,
            _wake_handle: wake_handle,
            _dir: dir,
        }
    }

    /// Opens (or replaces) the pairing window, returning the minted code. The
    /// reply resolves once the bridge's connection loop processes the command, so
    /// this doubles as a "bridge is connected" gate.
    async fn open_window(&self, ttl_secs: u64) -> PairingCode {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(PairingCommand::OpenWindow {
                ttl_secs,
                reply: reply_tx,
            })
            .await
            .expect("send OpenWindow");
        match tokio::time::timeout(EVENT_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(code))) => code,
            Ok(Ok(Err(e))) => panic!("OpenWindow rejected: {e:?}"),
            Ok(Err(_)) => panic!("OpenWindow reply channel dropped"),
            Err(_) => panic!("timed out awaiting OpenWindow reply"),
        }
    }

    async fn confirm(&self, device_id: DeviceId) {
        self.commands_tx
            .send(PairingCommand::Confirm { device_id })
            .await
            .expect("send Confirm");
    }

    async fn reject(&self, device_id: DeviceId) {
        self.commands_tx
            .send(PairingCommand::Reject { device_id })
            .await
            .expect("send Reject");
    }

    async fn revoke(&self, device_id: DeviceId) {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands_tx
            .send(PairingCommand::Revoke {
                device_id,
                reply: reply_tx,
            })
            .await
            .expect("send Revoke");
        match tokio::time::timeout(EVENT_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => panic!("Revoke failed: {e:?}"),
            Ok(Err(_)) => panic!("Revoke reply channel dropped"),
            Err(_) => panic!("timed out awaiting Revoke reply"),
        }
    }

    /// Awaits the next bridge event, failing on close or timeout.
    async fn next_event(&mut self) -> BridgeEvent {
        match tokio::time::timeout(EVENT_TIMEOUT, self.events_rx.recv()).await {
            Ok(Some(ev)) => ev,
            Ok(None) => panic!("bridge event channel closed"),
            Err(_) => panic!("timed out waiting for a bridge event"),
        }
    }

    /// The next event within `dur`, or `None` on timeout — for bounded *negative*
    /// assertions ("no arrival while a handshake is already pending").
    async fn try_next_event(&mut self, dur: Duration) -> Option<BridgeEvent> {
        tokio::time::timeout(dur, self.events_rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Reads events until a `PairingDeviceArrived`, returning its `(device_id,
    /// name, fingerprint)`. Tolerates a leading `PairingWindowOpened`.
    async fn await_arrival(&mut self) -> (DeviceId, String, String) {
        loop {
            match self.next_event().await {
                BridgeEvent::PairingDeviceArrived {
                    device_id,
                    name,
                    fingerprint,
                } => return (device_id, name, fingerprint),
                BridgeEvent::PairingWindowOpened { .. } => continue,
                other => panic!("expected PairingDeviceArrived, got {other:?}"),
            }
        }
    }

    /// Reads events until a terminal `PairingResult`, returning the outcome.
    async fn await_result(&mut self) -> PairingOutcome {
        loop {
            match self.next_event().await {
                BridgeEvent::PairingResult(outcome) => return outcome,
                BridgeEvent::PairingWindowOpened { .. }
                | BridgeEvent::PairingDeviceArrived { .. }
                | BridgeEvent::RosterChanged => continue,
                other => panic!("expected PairingResult, got {other:?}"),
            }
        }
    }

    async fn roster_len(&self) -> usize {
        self.roster.read().await.entries.len()
    }
}

impl Drop for PairingHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.bridge_task.abort();
        self.relay.kill();
    }
}

/// Drains a [`run_pairing`] progress stream until `WaitingForConfirmation`,
/// returning the device's own fingerprint (the value the operator compares).
async fn await_waiting_fingerprint(progress: &mut mpsc::Receiver<PairingProgress>) -> String {
    loop {
        match tokio::time::timeout(EVENT_TIMEOUT, progress.recv()).await {
            Ok(Some(PairingProgress::WaitingForConfirmation { own_fingerprint })) => {
                return own_fingerprint
            }
            Ok(Some(_)) => continue, // Connecting
            Ok(None) => panic!("progress channel closed before WaitingForConfirmation"),
            Err(_) => panic!("timed out waiting for WaitingForConfirmation"),
        }
    }
}

/// Runs one full happy-path pairing on a fresh window and returns the resulting
/// `PairingFile` (the device's durable trust bundle). Asserts the operator-shown
/// fingerprint equals the device's own. Used by tests that need a paired device
/// without re-asserting every intermediate step.
async fn pair_one_device(h: &mut PairingHarness, device_name: &str) -> PairingFile {
    let code = h.open_window(30).await;
    let (progress_tx, mut progress_rx) = mpsc::channel(8);
    let task = tokio::spawn(run_pairing(code, device_name.to_string(), progress_tx));

    let (device_id, name, fingerprint) = h.await_arrival().await;
    assert_eq!(
        name, device_name,
        "arrival carries the device's declared name"
    );
    let own = await_waiting_fingerprint(&mut progress_rx).await;
    assert_eq!(
        fingerprint, own,
        "the fingerprint the bridge shows must equal the device's own"
    );
    h.confirm(device_id).await;

    match tokio::time::timeout(PAIR_JOIN_TIMEOUT, task).await {
        Ok(Ok(Ok(file))) => file,
        Ok(Ok(Err(e))) => panic!("pairing failed: {e:?}"),
        Ok(Err(join)) => panic!("pairing task panicked: {join:?}"),
        Err(_) => panic!("timed out awaiting the pairing file"),
    }
}

/// Reads inbound frames until one is a *Pairing* frame addressed to `routing_id`,
/// returning its payload; `None` on close/EOF/error. The `recv_payload` sibling
/// filters `Data` frames — the ceremony rides `Pairing` frames.
async fn recv_pairing_payload(stream: &mut WsStream, routing_id: DeviceId) -> Option<Vec<u8>> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let envelope = Envelope::decode(&bytes).ok()?;
                if envelope.frame_type == FrameType::Pairing && envelope.dst == routing_id {
                    return Some(envelope.payload);
                }
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

/// A hand-driven device (initiator) half of the pairing ceremony, built from the
/// same primitives [`run_pairing`] uses (relay hello + `Handshake::initiator` +
/// `Pairing` prologue + envelope encode). It stops after `Pending` so a test can
/// choose to receive the `Grant` and then vanish — the abandoned-confirm case the
/// safe driver never produces. Mirrors bridge.rs's own initiator test harness.
struct PairingRawClient {
    stream: WsStream,
    transport: Transport,
    routing_id: DeviceId,
    device_private: Vec<u8>,
    device_public: Vec<u8>,
    // Held so the relay connection stays up until the client is dropped.
    _sink: WsSink,
}

impl PairingRawClient {
    /// Dials the window, completes the handshake, sends the sealed `Hello`, and
    /// reads `Pending` — leaving the client parked at the confirm gate.
    async fn connect_through_pending(code: &PairingCode, device_name: &str) -> PairingRawClient {
        let params: snow::params::NoiseParams = NOISE_PATTERN.parse().expect("valid noise pattern");
        let device_keypair = snow::Builder::new(params)
            .generate_keypair()
            .expect("generate device keypair");
        let device_id = DeviceId(rand::random());
        let relay_url = code.relay_url.clone().expect("relay url");
        let rendezvous = code.rendezvous_token.clone().expect("rendezvous token");
        let bridge_id = code.bridge_id;

        let (ws, _resp) = connect_async(&relay_url).await.expect("pairing dial");
        let (mut sink, mut stream) = ws.split();
        let routing_id = DeviceId(rand::random());

        let hello = RelayHello {
            role: HelloRole::Device,
            token: rendezvous,
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

        let bound = prologue(HandshakeKind::Pairing, &device_id, &routing_id, &bridge_id);
        let mut hs =
            Handshake::initiator(&device_keypair.private, &code.bridge_key, &code.psk, &bound)
                .expect("build initiator");
        let msg1 = hs.write_message(&[]).expect("write msg1");
        let mut first = Vec::with_capacity(32 + msg1.len());
        first.extend_from_slice(&device_id.0);
        first.extend_from_slice(&msg1);
        send_frame(&mut sink, FrameType::Pairing, routing_id, bridge_id, first).await;

        let msg2 = recv_pairing_payload(&mut stream, routing_id)
            .await
            .expect("bridge msg2");
        hs.read_message(&msg2).expect("read msg2");
        let (mut transport, _bridge_static) = hs.into_transport().expect("into transport");

        // Sealed E2E hello.
        let ciphertext = transport
            .seal(&PairingClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
                device_name: device_name.to_string(),
            })
            .expect("seal pairing hello");
        send_frame(
            &mut sink,
            FrameType::Pairing,
            routing_id,
            bridge_id,
            ciphertext,
        )
        .await;

        // Read Pending — the arrival is now user-visible on the bridge.
        let pending = recv_pairing_payload(&mut stream, routing_id)
            .await
            .expect("pairing pending");
        match transport
            .open::<PairingBridgeMsg>(&pending)
            .expect("open pending")
        {
            PairingBridgeMsg::Pending => {}
            other => panic!("expected Pending, got {other:?}"),
        }

        PairingRawClient {
            stream,
            transport,
            routing_id,
            device_private: device_keypair.private.clone(),
            device_public: device_keypair.public.clone(),
            _sink: sink,
        }
    }

    /// Reads the bridge's decision after the operator confirms — expects `Grant`,
    /// returning `(device_token, session_psk_b64)`.
    async fn recv_grant(&mut self) -> (String, String) {
        let frame = recv_pairing_payload(&mut self.stream, self.routing_id)
            .await
            .expect("grant frame");
        match self
            .transport
            .open::<PairingBridgeMsg>(&frame)
            .expect("open grant")
        {
            PairingBridgeMsg::Grant {
                device_token,
                psk,
                bridge_name: _,
            } => (device_token, psk),
            other => panic!("expected Grant, got {other:?}"),
        }
    }
}

/// Dials the pairing window and sends a first frame whose Noise `msg1` is garbage
/// (a valid 32-byte identity preamble followed by random bytes), so the bridge's
/// responder `read_message` fails immediately — the pre-confirmation "released
/// slot" path (Task 11 fix round 1). A corrupt msg1 is the honest probe here: in
/// IKpsk2 the PSK is mixed only in message 2, so a *valid* msg1 under a wrong PSK
/// would still be read (and answered) by the responder and would instead park the
/// slot until the window deadline. Returns after a bounded wait confirms no `msg2`
/// comes back (the handshake failed), then drops the connection — which also gives
/// the bridge time to release the slot before the legitimate handshake starts.
async fn send_garbage_pairing_probe(code: &PairingCode) {
    let device_id = DeviceId(rand::random());
    let relay_url = code.relay_url.clone().expect("relay url");
    let rendezvous = code.rendezvous_token.clone().expect("rendezvous token");
    let bridge_id = code.bridge_id;

    let (ws, _resp) = connect_async(&relay_url).await.expect("probe dial");
    let (mut sink, mut stream) = ws.split();
    let routing_id = DeviceId(rand::random());

    let hello = RelayHello {
        role: HelloRole::Device,
        token: rendezvous,
        device_id,
        routing_id,
        bridge_id,
    };
    send_frame(
        &mut sink,
        FrameType::Hello,
        routing_id,
        DeviceId::ZERO,
        serde_json::to_vec(&hello).expect("serialize relay hello"),
    )
    .await;

    // 32-byte preamble (so `split_preamble` succeeds) + 64 random bytes standing in
    // for msg1: the responder builds fine but `read_message` fails the AEAD, drops
    // the attempt, and (pre-`Pending`) frees the window for the next handshake.
    let mut first = Vec::with_capacity(32 + 64);
    first.extend_from_slice(&device_id.0);
    first.extend((0..64).map(|_| rand::random::<u8>()));
    send_frame(&mut sink, FrameType::Pairing, routing_id, bridge_id, first).await;

    // The failed responder never replies; wait (bounded) for a msg2 that will never
    // come, which also lets the bridge process + release the slot before we return.
    assert!(
        tokio::time::timeout(RECV_TIMEOUT, recv_pairing_payload(&mut stream, routing_id))
            .await
            .is_err(),
        "a garbage first frame must draw no handshake response"
    );
}

/// The capstone: a device pairs through the real confirm-gated ceremony over the
/// relay, and the resulting `PairingFile` attaches on its FIRST try — the
/// assert-before-grant guarantee means no first-connect race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_end_to_end_then_attach() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let mut h = PairingHarness::new(fake).await;

    let code = h.open_window(30).await;
    // The window-opened event carries the same code the reply did.
    match h.next_event().await {
        BridgeEvent::PairingWindowOpened {
            code: opened,
            expires_at,
        } => {
            assert_eq!(opened.bridge_id, code.bridge_id);
            assert!(expires_at >= 1, "expiry is a real unix timestamp");
        }
        other => panic!("expected PairingWindowOpened, got {other:?}"),
    }

    let (progress_tx, mut progress_rx) = mpsc::channel(8);
    let task = tokio::spawn(run_pairing(code, "phone".to_string(), progress_tx));

    // The bridge surfaces the arrival; its fingerprint equals the device's own.
    let (device_id, name, fingerprint) = h.await_arrival().await;
    assert_eq!(name, "phone");
    let own = await_waiting_fingerprint(&mut progress_rx).await;
    assert_eq!(
        fingerprint, own,
        "operator and device compare the same string"
    );

    // Empty until the confirm-gated grant persists the entry.
    assert_eq!(h.roster_len().await, 0, "nothing durable before Confirm");
    h.confirm(device_id).await;

    // Terminal event is Paired, and the roster now pins exactly this device.
    match h.await_result().await {
        PairingOutcome::Paired {
            device_id: id,
            name,
        } => {
            assert_eq!(id, device_id);
            assert_eq!(name, "phone");
        }
        other => panic!("expected Paired, got {other:?}"),
    }
    let pairing_file = match tokio::time::timeout(PAIR_JOIN_TIMEOUT, task).await {
        Ok(Ok(Ok(file))) => file,
        other => panic!("pairing did not produce a file: {other:?}"),
    };
    assert_eq!(h.roster_len().await, 1, "the paired device is enrolled");
    assert_eq!(pairing_file.device_id, device_id);

    // FIRST list + attach succeed — no readiness poll, proving assert-before-grant
    // credentialed the device at the relay before it ever received the file.
    let remote = RemoteSource::new(pairing_file);
    let listed = remote.list().await.expect("first list succeeds");
    assert_eq!(listed.len(), 1);
    let (project, session) = ids("api", "one");
    let mut channel = remote
        .attach(&project, &session)
        .await
        .expect("first attach");
    assert_eq!(recv_bytes(&mut channel).await, b"[fake attach api_one]\r\n");
    channel.send_bytes(b"hi".to_vec()).await.expect("send hi");
    assert_eq!(recv_bytes(&mut channel).await, b"hi");
}

/// A wrong-PSK probe on an open window frees the slot (pre-`Pending`), so a
/// legitimate device pairs on the SAME window right after. Proves the responder's
/// `ReleasedSlot` exit does not burn the window (Task 11 fix round 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_first_frame_then_real_pairing_succeeds() {
    let fake = Arc::new(FakeSessionSource::new());
    let mut h = PairingHarness::new(fake).await;

    let code = h.open_window(30).await;
    match h.next_event().await {
        BridgeEvent::PairingWindowOpened { .. } => {}
        other => panic!("expected PairingWindowOpened, got {other:?}"),
    }

    // A garbage first frame: the responder drops it and keeps the window open.
    send_garbage_pairing_probe(&code).await;

    // The same window now completes a real pairing.
    let (progress_tx, mut progress_rx) = mpsc::channel(8);
    let task = tokio::spawn(run_pairing(code, "phone".to_string(), progress_tx));
    let (device_id, _name, fingerprint) = h.await_arrival().await;
    let own = await_waiting_fingerprint(&mut progress_rx).await;
    assert_eq!(fingerprint, own);
    h.confirm(device_id).await;
    match tokio::time::timeout(PAIR_JOIN_TIMEOUT, task).await {
        Ok(Ok(Ok(_file))) => {}
        other => panic!("legit pairing after a probe must succeed: {other:?}"),
    }
    assert_eq!(h.roster_len().await, 1, "only the legit device is enrolled");
}

/// Once a handshake reaches `Pending`, a second handshake on the same window is
/// refused (never surfaces an arrival); the first completes and consumes the
/// window, after which a fresh attempt on the spent code cannot pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raced_second_pairing_is_refused() {
    let fake = Arc::new(FakeSessionSource::new());
    let mut h = PairingHarness::new(fake).await;

    let code = h.open_window(30).await;
    match h.next_event().await {
        BridgeEvent::PairingWindowOpened { .. } => {}
        other => panic!("expected PairingWindowOpened, got {other:?}"),
    }

    // First handshake reaches Pending (arrival surfaced) and parks on Confirm.
    let (p1_tx, mut p1_rx) = mpsc::channel(8);
    let first = tokio::spawn(run_pairing(code.clone(), "first".to_string(), p1_tx));
    let (first_id, _n, first_fp) = h.await_arrival().await;
    let first_own = await_waiting_fingerprint(&mut p1_rx).await;
    assert_eq!(first_fp, first_own);

    // Second handshake while the first is pending: its frames are dropped by the
    // single-in-flight window, so it NEVER surfaces an arrival and NEVER reaches
    // WaitingForConfirmation. Assert both bounded negatives.
    let (p2_tx, mut p2_rx) = mpsc::channel(8);
    let second = tokio::spawn(run_pairing(code.clone(), "second".to_string(), p2_tx));
    assert!(
        h.try_next_event(RECV_TIMEOUT).await.is_none(),
        "no second arrival may surface while the first is pending"
    );
    // The second's progress only ever reached Connecting (never WaitingForConfirmation).
    while let Ok(progress) = p2_rx.try_recv() {
        assert!(
            !matches!(progress, PairingProgress::WaitingForConfirmation { .. }),
            "the refused second handshake must not reach confirmation"
        );
    }
    second.abort();

    // The first completes and consumes the window: exactly one device is enrolled,
    // and the responder's single-completed-handshake rule (Task 11) means the
    // window is spent — a subsequent code carries no live routing (its relay
    // window is cancelled on completion). We assert the durable outcome (roster of
    // exactly one) rather than dialing a spent window, whose refusal only surfaces
    // via the client's fixed relay-read timeout.
    h.confirm(first_id).await;
    match tokio::time::timeout(PAIR_JOIN_TIMEOUT, first).await {
        Ok(Ok(Ok(_file))) => {}
        other => panic!("first pairing must complete: {other:?}"),
    }
    assert_eq!(h.roster_len().await, 1, "exactly one device enrolled");
}

/// A rejected device gets nothing durable: `run_pairing` returns `Rejected`, the
/// roster stays empty, and the terminal event is `Rejected`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reject_grants_nothing_durable() {
    let fake = Arc::new(FakeSessionSource::new());
    let mut h = PairingHarness::new(fake).await;

    let code = h.open_window(30).await;
    match h.next_event().await {
        BridgeEvent::PairingWindowOpened { .. } => {}
        other => panic!("expected PairingWindowOpened, got {other:?}"),
    }

    let (progress_tx, mut progress_rx) = mpsc::channel(8);
    let task = tokio::spawn(run_pairing(code, "phone".to_string(), progress_tx));
    let (device_id, _n, _fp) = h.await_arrival().await;
    let _ = await_waiting_fingerprint(&mut progress_rx).await;

    h.reject(device_id).await;

    // The device driver reports a user rejection...
    match tokio::time::timeout(PAIR_JOIN_TIMEOUT, task).await {
        Ok(Ok(Err(PairingError::Rejected(PairingRejectReason::UserRejected)))) => {}
        other => panic!("expected Rejected(UserRejected), got {other:?}"),
    }
    // ...the terminal event is Rejected, and nothing durable was granted.
    match h.await_result().await {
        PairingOutcome::Rejected { device_id: id } => assert_eq!(id, device_id),
        other => panic!("expected Rejected outcome, got {other:?}"),
    }
    assert_eq!(h.roster_len().await, 0, "a reject enrols nothing");
}

/// Revoking a paired device kills its live attach and refuses its next dial: the
/// bridge cancels the session bridge-side and re-asserts the shrunken set, which
/// the relay applies by dropping the device credential (ADR-0021 D6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_kicks_live_device() {
    let fake = Arc::new(FakeSessionSource::new());
    fake.spawn(spec("api", "one")).await.expect("spawn");
    let mut h = PairingHarness::new(fake).await;

    let pairing_file = pair_one_device(&mut h, "phone").await;
    let device_id = pairing_file.device_id;
    assert_eq!(h.roster_len().await, 1);

    // Attach: a live peer session for the paired device.
    let remote = RemoteSource::new(pairing_file.clone());
    let (project, session) = ids("api", "one");
    let mut channel = remote.attach(&project, &session).await.expect("attach");
    assert_eq!(recv_bytes(&mut channel).await, b"[fake attach api_one]\r\n");

    // Revoke it: the roster shrinks and the live session is kicked.
    h.revoke(device_id).await;
    assert_eq!(h.roster_len().await, 0, "revoke drops the roster entry");

    // The live attach channel dies structurally.
    assert!(
        tokio::time::timeout(EVENT_TIMEOUT, channel.recv())
            .await
            .expect("revoked channel should report death promptly")
            .is_none(),
        "a revoked device's live channel must die"
    );

    // A subsequent dial is refused by the relay (credential de-asserted).
    let outcome = tokio::time::timeout(Duration::from_secs(20), remote.attach(&project, &session))
        .await
        .expect("post-revoke attach must return, not hang");
    match outcome {
        Err(remora_core::SourceError::Transport(_)) => {}
        other => panic!("a revoked device must not re-attach, got {other:?}"),
    }
}

/// A device that vanishes after `Grant` but before `Confirm` leaves no ghost: the
/// bridge persists no roster entry and re-asserts roster-only, so the abandoned
/// window expires to `Expired` and the bridge is healthy enough to pair a fresh
/// device (which then is the ONLY roster entry).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirm_lost_leaves_no_ghost() {
    let fake = Arc::new(FakeSessionSource::new());
    let mut h = PairingHarness::new(fake).await;

    // Short TTL so the bridge's post-Grant confirm-wait (bounded by the window
    // deadline) expires promptly once the hand-driven client vanishes.
    let code = h.open_window(3).await;
    match h.next_event().await {
        BridgeEvent::PairingWindowOpened { .. } => {}
        other => panic!("expected PairingWindowOpened, got {other:?}"),
    }

    // Hand-drive the device up to Grant, then drop it without sending Confirm.
    let mut client = PairingRawClient::connect_through_pending(&code, "ghost").await;
    let (device_id, _n, _fp) = h.await_arrival().await;
    h.confirm(device_id).await;
    let (_device_token, _psk_b64) = client.recv_grant().await;
    // Sanity: the client really did mint a static keypair (the identity the bridge
    // pinned during the handshake) — proves this was a real ceremony, not a stub.
    assert_eq!(client.device_private.len(), 32);
    assert_eq!(client.device_public.len(), 32);
    drop(client);

    // The window expires: the bridge reports Expired and (per the responder) has
    // re-asserted roster-only. No durable entry was persisted.
    match h.await_result().await {
        PairingOutcome::Expired => {}
        other => panic!("expected Expired after an abandoned confirm, got {other:?}"),
    }
    assert_eq!(
        h.roster_len().await,
        0,
        "an abandoned confirm enrols nothing"
    );

    // The bridge is healthy: a fresh device pairs cleanly and is the ONLY entry —
    // the abandoned attempt left no trace in the durable trust boundary.
    let fresh = pair_one_device(&mut h, "real-phone").await;
    assert_eq!(h.roster_len().await, 1, "only the fresh device is enrolled");
    assert_ne!(
        fresh.device_id, device_id,
        "the fresh device is not the vanished ghost"
    );
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
