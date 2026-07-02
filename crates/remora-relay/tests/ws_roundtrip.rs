//! Integration tests for the relay WebSocket server (Task 8, spec D5/D9/D10/D13).
//!
//! These drive a real `serve` instance bound to `127.0.0.1:0` with a
//! tokio-tungstenite client, exercising the hello handshake, adjacency
//! routing, the documented close codes, and the aggregate audit log. Timing is
//! event-driven: every wait is a `tokio::time::timeout` around the actual
//! message/close we expect, never a bare sleep.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use remora_protocol::{DeviceId, Envelope, FrameType, HelloRole, RelayHello};
use remora_relay::{serve, AuditConfig, AuditSink, BridgeEntry, DeviceEntry, RelayConfig};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const BRIDGE: u8 = 0x11;
const DEVICE: u8 = 0x22;
const DEV_ROUTING: u8 = 0x55;

fn did(fill: u8) -> DeviceId {
    DeviceId([fill; 32])
}

fn base_config(audit: Option<AuditConfig>) -> Arc<RelayConfig> {
    Arc::new(RelayConfig {
        listen: "127.0.0.1:0".to_string(),
        bridges: vec![BridgeEntry {
            token: "bridge-tok".to_string(),
            device_id: did(BRIDGE),
        }],
        devices: vec![DeviceEntry {
            token: "device-tok".to_string(),
            device_id: did(DEVICE),
            bridge_id: did(BRIDGE),
        }],
        buffer_bytes: 1_048_576,
        handshake_timeout_secs: 10,
        max_connections: 1024,
        audit,
    })
}

/// `base_config` with the two pre-auth resource bounds overridden, for the
/// hardening tests (#231).
fn config_with_bounds(handshake_timeout_secs: u64, max_connections: usize) -> Arc<RelayConfig> {
    let mut config = (*base_config(None)).clone();
    config.handshake_timeout_secs = handshake_timeout_secs;
    config.max_connections = max_connections;
    Arc::new(config)
}

fn bridge_hello() -> RelayHello {
    RelayHello {
        role: HelloRole::Bridge,
        token: "bridge-tok".to_string(),
        device_id: did(BRIDGE),
        routing_id: did(BRIDGE),
        bridge_id: did(BRIDGE),
    }
}

fn device_hello() -> RelayHello {
    RelayHello {
        role: HelloRole::Device,
        token: "device-tok".to_string(),
        device_id: did(DEVICE),
        routing_id: did(DEV_ROUTING),
        bridge_id: did(BRIDGE),
    }
}

fn hello_frame(hello: &RelayHello) -> Vec<u8> {
    Envelope {
        frame_type: FrameType::Hello,
        src: hello.routing_id,
        dst: DeviceId::ZERO,
        payload: serde_json::to_vec(hello).expect("serialize hello"),
    }
    .encode()
}

fn data_frame(src: u8, dst: u8, payload: &[u8]) -> Vec<u8> {
    Envelope {
        frame_type: FrameType::Data,
        src: did(src),
        dst: did(dst),
        payload: payload.to_vec(),
    }
    .encode()
}

fn frame(frame_type: FrameType, src: u8, dst: u8) -> Vec<u8> {
    Envelope {
        frame_type,
        src: did(src),
        dst: did(dst),
        payload: b"x".to_vec(),
    }
    .encode()
}

async fn start(config: Arc<RelayConfig>) -> String {
    let audit = AuditSink::new(&config).expect("audit sink");
    let (addr, _handle) = serve(config, audit).await.expect("serve binds");
    format!("ws://{addr}")
}

async fn connect(url: &str) -> Ws {
    let (ws, _resp) = connect_async(url).await.expect("ws connect");
    ws
}

async fn send_bin(ws: &mut Ws, bytes: Vec<u8>) {
    ws.send(Message::Binary(bytes.into())).await.expect("send");
}

/// Awaits the next binary message on `ws` (bounded), returning its raw bytes.
async fn recv_bin(ws: &mut Ws) -> Vec<u8> {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("recv within timeout")
        .expect("stream open")
        .expect("no ws error");
    match msg {
        Message::Binary(b) => b.to_vec(),
        other => panic!("expected binary, got {other:?}"),
    }
}

/// Awaits the next message and asserts it is a Close with the given code.
async fn expect_close(ws: &mut Ws, code: u16) {
    let deadline = Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(deadline, ws.next())
            .await
            .expect("recv within timeout");
        match msg {
            Some(Ok(Message::Close(Some(cf)))) => {
                assert_eq!(u16::from(cf.code), code, "close code mismatch");
                return;
            }
            Some(Ok(Message::Close(None))) => panic!("close with no frame; wanted {code}"),
            // Tungstenite may surface a trailing pong/ping/empty; keep reading.
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("ws error awaiting close {code}: {e}"),
            None => panic!("stream ended without close frame {code}"),
        }
    }
}

#[tokio::test]
async fn round_trip_device_bridge_data() {
    let url = start(base_config(None)).await;

    // Bridge connects and registers (long-lived receiver).
    let mut bridge = connect(&url).await;
    send_bin(&mut bridge, hello_frame(&bridge_hello())).await;

    // Device delivery: retry across the tiny bridge-registration race. Each
    // attempt is event-driven — we race "bridge receives" against "device gets
    // 4004 because the bridge was not registered yet".
    let payload = b"ping";
    let want = data_frame(DEV_ROUTING, BRIDGE, payload);
    let start_t = std::time::Instant::now();
    let (mut device, got) = loop {
        assert!(
            start_t.elapsed() < Duration::from_secs(10),
            "delivery timed out"
        );
        let mut device = connect(&url).await;
        send_bin(&mut device, hello_frame(&device_hello())).await;
        send_bin(&mut device, want.clone()).await;
        tokio::select! {
            r = tokio::time::timeout(Duration::from_millis(500), bridge.next()) => {
                if let Ok(Some(Ok(Message::Binary(b)))) = r {
                    break (device, b.to_vec());
                }
            }
            _ = device.next() => { /* device closed (4004): bridge not ready, retry */ }
        }
    };
    assert_eq!(got, want, "bridge received the identical raw frame");

    // Bridge replies to the device.
    let reply = data_frame(BRIDGE, DEV_ROUTING, b"pong");
    send_bin(&mut bridge, reply.clone()).await;
    let back = recv_bin(&mut device).await;
    assert_eq!(back, reply, "device received the identical reply frame");
}

#[tokio::test]
async fn bad_token_closed_4001() {
    let url = start(base_config(None)).await;
    let mut ws = connect(&url).await;
    let mut hello = device_hello();
    hello.token = "wrong".to_string();
    send_bin(&mut ws, hello_frame(&hello)).await;
    expect_close(&mut ws, 4001).await;
}

#[tokio::test]
async fn data_before_hello_closed_4002() {
    let url = start(base_config(None)).await;
    let mut ws = connect(&url).await;
    // First frame is Data, not Hello.
    send_bin(&mut ws, data_frame(DEV_ROUTING, BRIDGE, b"early")).await;
    expect_close(&mut ws, 4002).await;
}

#[tokio::test]
async fn pairing_frame_after_hello_closed_4002() {
    let url = start(base_config(None)).await;
    let mut ws = connect(&url).await;
    send_bin(&mut ws, hello_frame(&device_hello())).await;
    // Post-hello, only Data is legal; a Pairing frame is a protocol error.
    send_bin(&mut ws, frame(FrameType::Pairing, DEV_ROUTING, BRIDGE)).await;
    expect_close(&mut ws, 4002).await;
}

#[tokio::test]
async fn data_to_offline_dst_closes_sender_4004() {
    let url = start(base_config(None)).await;
    let mut ws = connect(&url).await;
    send_bin(&mut ws, hello_frame(&device_hello())).await;
    // Bridge never connected — its routing id is unavailable.
    send_bin(&mut ws, data_frame(DEV_ROUTING, BRIDGE, b"ping")).await;
    expect_close(&mut ws, 4004).await;
}

#[tokio::test]
async fn second_hello_same_routing_id_replaces_first_4009() {
    let url = start(base_config(None)).await;
    let mut first = connect(&url).await;
    send_bin(&mut first, hello_frame(&device_hello())).await;
    let mut second = connect(&url).await;
    send_bin(&mut second, hello_frame(&device_hello())).await;

    // Order-independent: exactly one of the two same-routing-id connections is
    // displaced with 4009; the other stays open.
    let deadline = Duration::from_secs(5);
    let mut got_4009 = 0;
    tokio::select! {
        m = tokio::time::timeout(deadline, first.next()) => {
            if let Ok(Some(Ok(Message::Close(Some(cf))))) = m {
                assert_eq!(u16::from(cf.code), 4009);
                got_4009 += 1;
            }
        }
        m = tokio::time::timeout(deadline, second.next()) => {
            if let Ok(Some(Ok(Message::Close(Some(cf))))) = m {
                assert_eq!(u16::from(cf.code), 4009);
                got_4009 += 1;
            }
        }
    }
    assert_eq!(got_4009, 1, "exactly one connection replaced with 4009");
}

#[tokio::test]
async fn audit_file_written_0600_with_close_records() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.log");
    let config = base_config(Some(AuditConfig { path: path.clone() }));
    let url = start(config).await;

    // A successful device connection that then closes cleanly.
    let mut ws = connect(&url).await;
    send_bin(&mut ws, hello_frame(&device_hello())).await;
    ws.close(None).await.expect("client close");
    // Drain until the stream ends so the server observes the close.
    while let Some(Ok(_)) = ws.next().await {}
    drop(ws);

    // Poll for the record to be flushed (bounded, event-driven).
    let start_t = std::time::Instant::now();
    let record = loop {
        assert!(
            start_t.elapsed() < Duration::from_secs(5),
            "no audit record written"
        );
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(line) = contents.lines().next() {
                if !line.trim().is_empty() {
                    break serde_json::from_str::<serde_json::Value>(line)
                        .expect("record is valid json");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert_eq!(record["role"], "device");
    assert_eq!(record["device_id"], did(DEVICE).to_string());
    assert_eq!(record["routing_id"], did(DEV_ROUTING).to_string());
    assert!(record["ts_unix"].is_u64());
    assert!(record["frames_in"].as_u64().expect("frames_in") >= 1);
    assert!(record["frames_out"].is_u64());
    assert!(record["bytes_in"].as_u64().expect("bytes_in") >= 1);
    assert!(record["bytes_out"].is_u64());
    assert!(record["connected_secs"].is_u64());
    assert!(record["close_reason"].is_string());

    let mode = std::fs::metadata(&path)
        .expect("audit file metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "audit file is owner rw only");
}

/// Slowloris defense (#231): a client that completes the WebSocket upgrade but
/// never sends a hello is dropped once the handshake deadline elapses, rather
/// than pinning a task/FD/read-buffer forever.
#[tokio::test]
async fn slow_client_that_never_sends_hello_is_dropped() {
    // 1s handshake window; assert the server-side drop within 2×.
    let url = start(config_with_bounds(1, 1024)).await;
    let mut ws = connect(&url).await;

    // Never send a hello. The read must *resolve* (close/EOF/err) within the
    // deadline window — proving the server dropped us — not hang indefinitely.
    let outcome = tokio::time::timeout(Duration::from_secs(2), ws.next()).await;
    let msg =
        outcome.expect("server dropped the hello-less connection within the handshake window");
    match msg {
        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => {}
        Some(Ok(other)) => panic!("expected close/EOF after handshake timeout, got {other:?}"),
    }
}

/// Global connection cap (#231): with `max_connections = N`, the N+1th
/// connection is refused promptly. Ordering is deterministic — the accept loop
/// takes a permit before it spawns (and thus before the client's upgrade
/// completes), so once N clients have connected, N permits are held.
#[tokio::test]
async fn connections_beyond_max_are_rejected() {
    let url = start(config_with_bounds(10, 2)).await;

    // Two long-lived connections that complete hello and hold both permits.
    let mut bridge = connect(&url).await;
    send_bin(&mut bridge, hello_frame(&bridge_hello())).await;
    let mut device = connect(&url).await;
    send_bin(&mut device, hello_frame(&device_hello())).await;

    // The 3rd is over the cap: the freshly accepted socket is dropped before
    // the upgrade, so the client's handshake fails (or, if the TCP+upgrade
    // sneaks through, the next read is an immediate close/EOF).
    let third = tokio::time::timeout(Duration::from_secs(3), connect_async(&url))
        .await
        .expect("3rd connect resolves");
    match third {
        Err(_) => {} // upgrade refused at the cap — the common path.
        Ok((mut ws, _resp)) => {
            let r = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("over-cap read resolves");
            assert!(
                matches!(r, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
                "3rd connection past the cap was not dropped: {r:?}",
            );
        }
    }

    drop(bridge);
    drop(device);
}
