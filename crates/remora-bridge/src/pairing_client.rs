//! The device (initiator) half of the pairing ceremony (ADR-0021 D3):
//! [`run_pairing`].
//!
//! A freshly provisioned device holds only a scanned [`PairingCode`] — the
//! relay endpoint + rendezvous token, the bridge's id and static public key, the
//! one-shot pairing PSK, and the bridge's minimum protocol version. [`run_pairing`]
//! mints this device's durable identity (an X25519 static keypair + a random
//! [`DeviceId`], both of which never leave the device), dials the relay with the
//! rendezvous token, drives the `IKpsk2` initiator handshake bound to
//! [`HandshakeKind::Pairing`], exchanges the confirm-gated pairing messages with
//! the bridge responder ([`bridge`](crate::bridge)), and on the bridge's final
//! `Confirmed` writes out a [`PairingFile`] — the durable trust bundle a
//! [`RemoteSource`](crate::RemoteSource) then uses for every session.
//!
//! # Interop with the bridge responder (Task 11)
//!
//! The wire framing mirrors [`bridge`](crate::bridge)'s pairing responder exactly:
//!
//! - All ceremony traffic rides [`FrameType::Pairing`] envelopes, device↔bridge,
//!   through the blind relay. The device's *first* frame payload is
//!   `32-byte device identity id ‖ noise msg1` (the same preamble shape as a
//!   session handshake). The bridge's msg2 reply payload is the raw Noise message,
//!   preamble-free. After [`Handshake::into_transport`] every subsequent Pairing
//!   frame payload is a sealed [`PairingClientMsg`] / [`PairingBridgeMsg`].
//! - The prologue is `prologue(HandshakeKind::Pairing, &device_id, &routing_id,
//!   &bridge_id)`, where `routing_id` is the envelope `src` the device binds as
//!   the relay routing id (matching how the responder learns `src` and binds it
//!   as the initiator routing id).
//! - The relay hello is `role = Device`, `token = rendezvous_token`, with a
//!   freshly minted device id + routing id; the relay's anti-spoof check requires
//!   the envelope `src` to equal the announced `routing_id`.

use std::time::Duration;

use base64::Engine as _;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use rand::TryRng as _;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use zeroize::{Zeroize as _, Zeroizing};

use remora_protocol::{
    DeviceId, Envelope, FrameType, HelloRole, PairingBridgeMsg, PairingClientMsg, PairingCode,
    PairingRejectReason, RelayHello, PROTOCOL_VERSION,
};

use crate::identity::{fingerprint, PairingFile};
use crate::noise::{prologue, Handshake, HandshakeKind, NoiseError, Transport, NOISE_PATTERN};

/// Standard-base64 engine — matches the encoding the identity layer and the
/// bridge grant use, so a [`PairingFile`] this driver writes decodes the same way.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Length of the plaintext identity preamble prefixing the device's first
/// (handshake) Pairing frame (spec D16): a 32-byte [`DeviceId`].
const DEVICE_ID_LEN: usize = 32;

/// Deadline for a transport-level (machine-paced) recv: the handshake msg2, the
/// bridge's `Pending`, and the final `Confirmed`. Mirrors the client relay read
/// budget in [`remote_source`](crate::RemoteSource) so a bridge that drops the
/// peer without closing the socket turns into a prompt typed error, not a hang.
const RELAY_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Deadline for the human-paced recv: the `Grant`/`Rejected` that follows
/// `Pending` waits on a person confirming the fingerprint, so it is bounded far
/// more generously — by the bridge's pairing-window TTL rather than a transport
/// round-trip. On expiry the ceremony reports [`PairingError::Expired`].
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(300);

/// Progress the ceremony reports to its caller (the desktop UI) as it advances.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PairingProgress {
    /// Dialing the relay and running the Noise handshake.
    Connecting,
    /// The handshake completed and the bridge is waiting for the user to confirm
    /// this device. `own_fingerprint` is this device's static-key fingerprint —
    /// the exact value the bridge shows its operator to compare (ADR-0021 D5).
    WaitingForConfirmation {
        /// This device's static public key fingerprint (`XXXX-XXXX-XXXX`).
        own_fingerprint: String,
    },
}

/// Why [`run_pairing`] did not produce a [`PairingFile`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PairingError {
    /// The bridge refused the pairing; `reason` distinguishes a user reject from
    /// a duplicate id or a closed window.
    #[error("pairing rejected: {0:?}")]
    Rejected(PairingRejectReason),
    /// The pairing window expired before the user confirmed (the confirmation
    /// wait elapsed with no `Grant`/`Rejected`).
    #[error("pairing window expired before confirmation")]
    Expired,
    /// A transport-level failure (dial, relay read/write, Noise, decode).
    #[error("pairing transport error: {0}")]
    Transport(String),
    /// This device's [`PROTOCOL_VERSION`] is below the bridge's advertised
    /// minimum — either caught by the pre-dial preflight (`bridge_min` =
    /// `code.min_protocol`) or reported by the bridge's version-mismatch reject.
    #[error("protocol too old: bridge requires at least version {bridge_min}")]
    VersionMismatch {
        /// The minimum protocol version the bridge accepts.
        bridge_min: u32,
    },
}

/// Whether this device's protocol version satisfies the code's advertised
/// minimum. Split out so the preflight gate is unit-testable without a dial.
fn min_protocol_ok(code_min: u32, ours: u32) -> bool {
    ours >= code_min
}

/// Projects a [`NoiseError`] onto a transport [`PairingError`]. Its `Display`
/// carries no plaintext, so no key or PSK material reaches a log.
fn noise_err(e: NoiseError) -> PairingError {
    PairingError::Transport(format!("noise error: {e}"))
}

/// Maps a bridge-sent [`PairingRejectReason`] onto a [`PairingError`]. A version
/// mismatch is surfaced as the dedicated [`PairingError::VersionMismatch`] so the
/// caller handles a bridge-reported skew identically to the pre-dial preflight;
/// every other reason is an opaque [`PairingError::Rejected`].
fn map_rejection(reason: PairingRejectReason) -> PairingError {
    match reason {
        PairingRejectReason::VersionMismatch { bridge_min } => {
            PairingError::VersionMismatch { bridge_min }
        }
        other => PairingError::Rejected(other),
    }
}

/// Fills a 32-byte [`DeviceId`] from the OS CSPRNG (per-crate convention for
/// minted ids/keys). Used for both the device's durable identity id and its
/// per-connection routing id; 32 random bytes is never the reserved all-zero id.
fn random_device_id() -> Result<DeviceId, PairingError> {
    let mut raw = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut raw)
        .map_err(|e| PairingError::Transport(format!("csprng error: {e}")))?;
    Ok(DeviceId(raw))
}

/// A freshly minted device keypair whose private half is zeroized on drop
/// (#278). `snow::Keypair` itself has no drop hygiene, and the ceremony has
/// many early-return paths — the wrapper covers them all. Derefs to the inner
/// keypair so callers read `.private`/`.public` unchanged.
struct MintedKeypair(snow::Keypair);

impl std::ops::Deref for MintedKeypair {
    type Target = snow::Keypair;

    fn deref(&self) -> &snow::Keypair {
        &self.0
    }
}

impl Drop for MintedKeypair {
    fn drop(&mut self) {
        self.0.private.zeroize();
    }
}

/// Mints this device's durable identity: a fresh X25519 static keypair (the same
/// way the identity layer does) plus a random [`DeviceId`]. Both are secret to
/// the device — the private key never leaves, and the id is bound into the Noise
/// prologue and preamble so it cannot be swapped by the relay.
fn mint_device_identity() -> Result<(MintedKeypair, DeviceId), PairingError> {
    let params: snow::params::NoiseParams = NOISE_PATTERN
        .parse()
        .map_err(|e: snow::Error| PairingError::Transport(format!("noise params: {e}")))?;
    let keypair = snow::Builder::new(params)
        .generate_keypair()
        .map_err(|e| PairingError::Transport(format!("keypair generation failed: {e}")))?;
    let device_id = random_device_id()?;
    Ok((MintedKeypair(keypair), device_id))
}

/// Runs the whole device-side pairing ceremony against the bridge responder
/// through the relay, returning the durable [`PairingFile`] on success.
///
/// Sequence: version preflight → mint device identity → dial relay + `role=Device`
/// hello → `IKpsk2` initiator handshake (Pairing prologue, PSK = the code's
/// pairing secret) → sealed [`PairingClientMsg::Hello`] → recv `Pending` (emit
/// [`PairingProgress::WaitingForConfirmation`]) → recv `Grant`/`Rejected` → on
/// `Grant` send [`PairingClientMsg::Confirm`], recv `Confirmed` → build the
/// [`PairingFile`] (relay from the code, `device_token`/`psk` from the grant,
/// bridge id + static key from the code, device keypair/id minted here).
pub async fn run_pairing(
    code: PairingCode,
    device_name: String,
    progress: mpsc::Sender<PairingProgress>,
) -> Result<PairingFile, PairingError> {
    // Version preflight before any network work (ADR-0021 D8): if this device is
    // older than the bridge's floor, fail fast with the minimum it needs rather
    // than dial and later hit an opaque reject.
    if !min_protocol_ok(code.min_protocol, PROTOCOL_VERSION) {
        return Err(PairingError::VersionMismatch {
            bridge_min: code.min_protocol,
        });
    }

    // This driver speaks the relay transport; a mesh-only code carries no relay
    // endpoint to dial. (`PairingCode::parse` guarantees the pair is present when
    // the transport is relay, but a hand-built code might not.)
    let relay_url = code.relay_url.clone().ok_or_else(|| {
        PairingError::Transport("pairing code has no relay transport".to_string())
    })?;
    let rendezvous_token = code.rendezvous_token.clone().ok_or_else(|| {
        PairingError::Transport("pairing code has no rendezvous token".to_string())
    })?;

    // Mint this device's durable identity — keypair + id never leave here.
    let (device_keypair, device_id) = mint_device_identity()?;

    let _ = progress.send(PairingProgress::Connecting).await;

    let (ws, _resp) = connect_async(&relay_url)
        .await
        .map_err(|e| PairingError::Transport(format!("relay dial failed: {e}")))?;
    let (mut sink, mut stream) = ws.split();

    // A fresh routing id per connection; the relay pins envelope `src` to it.
    let routing_id = random_device_id()?;
    let bridge_id = code.bridge_id;

    // --- Relay hello: role=device, rendezvous token, addressed to the relay. ---
    let hello = RelayHello {
        role: HelloRole::Device,
        token: rendezvous_token,
        device_id,
        routing_id,
        bridge_id,
    };
    let hello_payload = serde_json::to_vec(&hello)
        .map_err(|e| PairingError::Transport(format!("hello serialize failed: {e}")))?;
    send_frame(
        &mut sink,
        FrameType::Hello,
        routing_id,
        DeviceId::ZERO,
        hello_payload,
    )
    .await?;

    // --- Noise IKpsk2 initiator, Pairing prologue. The prologue binds this exact
    // route (identity, routing id, bridge id) + the pairing kind byte; the bridge
    // responder builds the same one from the msg1 preamble and envelope src. ---
    let bound = prologue(HandshakeKind::Pairing, &device_id, &routing_id, &bridge_id);
    let mut hs = Handshake::initiator(&device_keypair.private, &code.bridge_key, &code.psk, &bound)
        .map_err(noise_err)?;
    let msg1 = hs.write_message(&[]).map_err(noise_err)?;

    // The first Pairing frame is `32-byte identity preamble ‖ msg1` (spec D16);
    // the bridge splits it to bind the prologue and later pin the static.
    let mut first = Vec::with_capacity(DEVICE_ID_LEN + msg1.len());
    first.extend_from_slice(&device_id.0);
    first.extend_from_slice(&msg1);
    send_frame(&mut sink, FrameType::Pairing, routing_id, bridge_id, first).await?;

    // msg2 rides back preamble-free; complete the handshake into a transport.
    let msg2 = recv_step(&mut stream, routing_id).await?;
    hs.read_message(&msg2).map_err(noise_err)?;
    // The initiator already pinned the responder's static via `code.bridge_key`,
    // so its learned copy is redundant — discard it.
    let (mut transport, _bridge_static) = hs.into_transport().map_err(noise_err)?;

    // --- Sealed E2E hello: our version + display name for the confirm dialog. ---
    send_client_msg(
        &mut sink,
        &mut transport,
        routing_id,
        bridge_id,
        &PairingClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_name,
        },
    )
    .await?;

    // The bridge answers `Pending` once it has version-gated us and surfaced the
    // arrival to its operator; a `Rejected` here is a version skew or a race.
    let pending = recv_step(&mut stream, routing_id).await?;
    match transport
        .open::<PairingBridgeMsg>(&pending)
        .map_err(noise_err)?
    {
        PairingBridgeMsg::Pending => {}
        PairingBridgeMsg::Rejected { reason } => return Err(map_rejection(reason)),
        // Any other message before `Pending` is out of protocol. Do not format
        // the message itself — a mis-sequenced `Grant` would carry the session
        // PSK, which must never reach a log.
        _ => {
            return Err(PairingError::Transport(
                "unexpected pairing message (expected Pending)".to_string(),
            ))
        }
    }
    let _ = progress
        .send(PairingProgress::WaitingForConfirmation {
            own_fingerprint: fingerprint(&device_keypair.public),
        })
        .await;

    // --- The confirm gate: this waits on a human, so its deadline is generous. ---
    let decision = recv_confirmation(&mut stream, routing_id).await?;
    let (device_token, session_psk) = match transport
        .open::<PairingBridgeMsg>(&decision)
        .map_err(noise_err)?
    {
        PairingBridgeMsg::Grant {
            device_token,
            psk,
            bridge_name: _,
        } => (device_token, Zeroizing::new(psk)),
        PairingBridgeMsg::Rejected { reason } => return Err(map_rejection(reason)),
        _ => {
            return Err(PairingError::Transport(
                "unexpected pairing message (expected Grant)".to_string(),
            ))
        }
    };

    // --- Acknowledge we stored the credentials; only then does the bridge
    // persist its roster entry and send the final `Confirmed`. ---
    send_client_msg(
        &mut sink,
        &mut transport,
        routing_id,
        bridge_id,
        &PairingClientMsg::Confirm,
    )
    .await?;

    let confirmed = recv_step(&mut stream, routing_id).await?;
    match transport
        .open::<PairingBridgeMsg>(&confirmed)
        .map_err(noise_err)?
    {
        PairingBridgeMsg::Confirmed => {}
        PairingBridgeMsg::Rejected { reason } => return Err(map_rejection(reason)),
        _ => {
            return Err(PairingError::Transport(
                "unexpected pairing message (expected Confirmed)".to_string(),
            ))
        }
    }

    build_pairing_file(
        &code,
        &relay_url,
        device_token,
        &session_psk,
        &device_keypair,
        device_id,
    )
}

/// Assembles the durable [`PairingFile`] from the code (relay endpoint plus the
/// bridge id and static key), the bridge grant (`device_token`, session `psk`),
/// and this device's minted identity. The granted PSK is validated to be 32
/// bytes and re-encoded canonically so a malformed grant is a typed error, not a
/// pairing file that fails at first dial. The decoded PSK bytes are zeroized
/// once re-encoded (#278).
fn build_pairing_file(
    code: &PairingCode,
    relay_url: &str,
    device_token: String,
    session_psk_b64: &str,
    device_keypair: &snow::Keypair,
    device_id: DeviceId,
) -> Result<PairingFile, PairingError> {
    let psk_bytes = Zeroizing::new(
        B64.decode(session_psk_b64)
            .map_err(|_| PairingError::Transport("granted psk is not valid base64".to_string()))?,
    );
    if psk_bytes.len() != 32 {
        return Err(PairingError::Transport(
            "granted psk is not 32 bytes".to_string(),
        ));
    }
    Ok(PairingFile {
        relay_url: relay_url.to_string(),
        device_token,
        bridge_id: code.bridge_id,
        bridge_static_pubkey: B64.encode(code.bridge_key),
        psk: B64.encode(psk_bytes.as_slice()),
        device_id,
        device_private_key: B64.encode(&device_keypair.private),
        device_public_key: B64.encode(&device_keypair.public),
    })
}

/// The split write half of a relay WebSocket.
type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// The split read half of a relay WebSocket.
type WsStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

/// Wraps `payload` in an [`Envelope`] of `frame_type` and writes it to the relay.
async fn send_frame(
    sink: &mut WsSink,
    frame_type: FrameType,
    src: DeviceId,
    dst: DeviceId,
    payload: Vec<u8>,
) -> Result<(), PairingError> {
    let frame = Envelope {
        frame_type,
        src,
        dst,
        payload,
    }
    .encode();
    sink.send(Message::Binary(frame.into()))
        .await
        .map_err(|e| PairingError::Transport(format!("relay write failed: {e}")))
}

/// Seals `msg` on the pairing transport and enqueues it as a Pairing frame.
async fn send_client_msg(
    sink: &mut WsSink,
    transport: &mut Transport,
    src: DeviceId,
    dst: DeviceId,
    msg: &PairingClientMsg,
) -> Result<(), PairingError> {
    let ciphertext = transport.seal(msg).map_err(noise_err)?;
    send_frame(sink, FrameType::Pairing, src, dst, ciphertext).await
}

/// Reads the next Pairing frame addressed to `routing_id`, bounded by
/// [`RELAY_READ_TIMEOUT`] (machine-paced step). A timeout is a transport error.
async fn recv_step(stream: &mut WsStream, routing_id: DeviceId) -> Result<Vec<u8>, PairingError> {
    match tokio::time::timeout(RELAY_READ_TIMEOUT, recv_pairing_inner(stream, routing_id)).await {
        Ok(result) => result,
        Err(_) => Err(PairingError::Transport("relay read timed out".to_string())),
    }
}

/// Reads the next Pairing frame addressed to `routing_id`, bounded by
/// [`CONFIRMATION_TIMEOUT`] (the human confirm gate). A timeout means the pairing
/// window elapsed with no decision, surfaced as [`PairingError::Expired`].
async fn recv_confirmation(
    stream: &mut WsStream,
    routing_id: DeviceId,
) -> Result<Vec<u8>, PairingError> {
    match tokio::time::timeout(CONFIRMATION_TIMEOUT, recv_pairing_inner(stream, routing_id)).await {
        Ok(result) => result,
        Err(_) => Err(PairingError::Expired),
    }
}

/// Reads inbound frames until one is a Pairing frame addressed to `routing_id`,
/// returning its payload. Frames not addressed to us are skipped; a malformed
/// frame, a close, an EOF, or a read error is a transport error.
async fn recv_pairing_inner(
    stream: &mut WsStream,
    routing_id: DeviceId,
) -> Result<Vec<u8>, PairingError> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                let envelope = Envelope::decode(&bytes)
                    .map_err(|e| PairingError::Transport(format!("malformed frame: {e}")))?;
                if envelope.frame_type == FrameType::Pairing && envelope.dst == routing_id {
                    return Ok(envelope.payload);
                }
                // Not a Pairing frame for us: keep waiting.
            }
            Some(Ok(Message::Close(_))) | None => {
                return Err(PairingError::Transport(
                    "relay connection closed".to_string(),
                ))
            }
            Some(Err(e)) => return Err(PairingError::Transport(format!("relay read error: {e}"))),
            // Ping/Pong/Text: ignore and keep waiting for a Pairing frame.
            Some(Ok(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A relay-transport pairing code with `min_protocol = ours` and all-zero
    /// key material — enough for the pure-logic tests here (Task 14 drives a real
    /// ceremony end to end).
    fn relay_code(min_protocol: u32) -> PairingCode {
        PairingCode {
            relay_url: Some("ws://relay.example/ws".to_string()),
            rendezvous_token: Some("rendezvous-tok".to_string()),
            mesh_addr: None,
            psk: [0u8; 32],
            bridge_id: DeviceId([0xab; 32]),
            bridge_key: [0x11; 32],
            bridge_name: Some("bridge".to_string()),
            min_protocol,
        }
    }

    #[test]
    fn version_gate_blocks_below_min() {
        // A code demanding min_protocol above ours fails before dialing.
        assert!(min_protocol_ok(3, 3));
        assert!(!min_protocol_ok(4, 3));
    }

    #[tokio::test]
    async fn run_pairing_version_skew_fails_before_dial() {
        // A code demanding a version above ours returns VersionMismatch without
        // any network work — the URL is unroutable, so a dial would error
        // differently. `bridge_min` echoes the code's minimum.
        let code = relay_code(PROTOCOL_VERSION + 1);
        let (tx, mut rx) = mpsc::channel(4);
        let err = run_pairing(code, "phone".to_string(), tx)
            .await
            .expect_err("version skew must fail");
        assert!(
            matches!(err, PairingError::VersionMismatch { bridge_min } if bridge_min == PROTOCOL_VERSION + 1),
            "got: {err:?}"
        );
        // The preflight fires before `Connecting` is ever emitted.
        assert!(
            rx.try_recv().is_err(),
            "no progress should be emitted before the dial"
        );
    }

    #[test]
    fn version_mismatch_reject_maps_to_typed_variant() {
        // A bridge-reported version skew becomes the dedicated variant (so the
        // caller treats it like the pre-dial preflight), carrying the floor.
        let err = map_rejection(PairingRejectReason::VersionMismatch { bridge_min: 9 });
        assert!(matches!(
            err,
            PairingError::VersionMismatch { bridge_min: 9 }
        ));
    }

    #[test]
    fn other_reject_reasons_map_to_rejected() {
        for reason in [
            PairingRejectReason::UserRejected,
            PairingRejectReason::DuplicateId,
            PairingRejectReason::WindowClosed,
        ] {
            match map_rejection(reason.clone()) {
                PairingError::Rejected(got) => assert_eq!(got, reason),
                other => panic!("expected Rejected({reason:?}), got {other:?}"),
            }
        }
    }

    #[test]
    fn mint_device_identity_yields_32_byte_keys_and_unique_ids() {
        let (kp1, id1) = mint_device_identity().expect("mint 1");
        let (kp2, id2) = mint_device_identity().expect("mint 2");
        assert_eq!(kp1.private.len(), 32, "X25519 private key is 32 bytes");
        assert_eq!(kp1.public.len(), 32, "X25519 public key is 32 bytes");
        assert_ne!(id1, id2, "each device mints a distinct random id");
        assert_ne!(kp1.public, kp2.public, "each device mints a distinct key");
        assert_ne!(
            id1.0, [0u8; 32],
            "a minted id is never the reserved zero id"
        );
    }

    #[test]
    fn build_pairing_file_carries_code_and_grant_fields() {
        let code = relay_code(PROTOCOL_VERSION);
        let (device_keypair, device_id) = mint_device_identity().expect("mint");
        let session_psk = [0x5a; 32];

        let pf = build_pairing_file(
            &code,
            "ws://relay.example/ws",
            "granted-device-token".to_string(),
            &B64.encode(session_psk),
            &device_keypair,
            device_id,
        )
        .expect("build pairing file");

        // Relay + bridge identity come from the code; token + psk from the grant;
        // the device identity is exactly what we minted.
        assert_eq!(pf.relay_url, "ws://relay.example/ws");
        assert_eq!(pf.device_token, "granted-device-token");
        assert_eq!(pf.bridge_id, code.bridge_id);
        assert_eq!(pf.bridge_static_pubkey, B64.encode(code.bridge_key));
        assert_eq!(pf.psk, B64.encode(session_psk));
        assert_eq!(pf.device_id, device_id);
        assert_eq!(pf.device_private_key, B64.encode(&device_keypair.private));
        assert_eq!(pf.device_public_key, B64.encode(&device_keypair.public));

        // The stored key material round-trips through the same decode
        // `RemoteSource::key_material` uses, so this file is dial-ready.
        assert_eq!(B64.decode(&pf.psk).expect("psk b64").len(), 32);
        assert_eq!(
            B64.decode(&pf.device_private_key).expect("priv b64").len(),
            32
        );
    }

    #[test]
    fn build_pairing_file_rejects_malformed_grant_psk() {
        let code = relay_code(PROTOCOL_VERSION);
        let (device_keypair, device_id) = mint_device_identity().expect("mint");

        // A grant PSK that is not 32 bytes is a corrupt grant, not a usable file.
        let err = build_pairing_file(
            &code,
            "ws://relay.example/ws",
            "tok".to_string(),
            &B64.encode([0u8; 16]),
            &device_keypair,
            device_id,
        )
        .expect_err("short psk must fail");
        assert!(matches!(err, PairingError::Transport(_)), "got: {err:?}");

        // Non-base64 is likewise a typed transport error, never a panic.
        let err = build_pairing_file(
            &code,
            "ws://relay.example/ws",
            "tok".to_string(),
            "not base64!!!",
            &device_keypair,
            device_id,
        )
        .expect_err("bad base64 must fail");
        assert!(matches!(err, PairingError::Transport(_)), "got: {err:?}");
    }

    #[test]
    fn own_fingerprint_matches_static_public_key() {
        // The progress fingerprint is over the device's static public key — the
        // exact bytes the bridge authenticates and fingerprints on its side, so
        // both operators compare the same string (ADR-0021 D5).
        let (device_keypair, _) = mint_device_identity().expect("mint");
        let progress = PairingProgress::WaitingForConfirmation {
            own_fingerprint: fingerprint(&device_keypair.public),
        };
        match progress {
            PairingProgress::WaitingForConfirmation { own_fingerprint } => {
                assert_eq!(own_fingerprint, fingerprint(&device_keypair.public));
            }
            other => panic!("expected WaitingForConfirmation, got {other:?}"),
        }
    }
}
