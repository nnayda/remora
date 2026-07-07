//! Noise `IKpsk2` session plumbing for the relay-mode E2E channel (ADR-0021).
//!
//! Both halves of a relay session — the bridge (Task 12) and the client
//! [`RemoteSource`] (Task 13) — build on the thin `snow` wrappers here:
//!
//! - [`Handshake`] drives the `IKpsk2` handshake to completion. The initiator
//!   pins the responder's static public key (from the pairing file); the
//!   responder learns the initiator's static during the handshake and exposes
//!   it via [`Handshake::into_transport`] so the caller can verify it against
//!   the roster (spec C7).
//! - The [`prologue`] binds each session to its routing context: envelope
//!   version, protocol version, and the three device ids (initiator identity,
//!   initiator routing id, responder/bridge id). A mismatch anywhere fails the
//!   handshake's first AEAD check, so a relay cannot splice a session onto a
//!   different route (spec D14/D16).
//! - [`Transport`] seals/opens application messages, enforcing the Noise
//!   plaintext cap on the *encoded* payload (an error, never truncation) and
//!   relying on `snow`'s in-order nonce discipline for replay/reorder safety.
//! - [`chunk_bytes`] splits a raw PTY byte run into transport-sized chunks so a
//!   large burst never trips the plaintext cap.

use serde::de::DeserializeOwned;
use serde::Serialize;
use zeroize::Zeroizing;

use remora_protocol::{DeviceId, ENVELOPE_VERSION, PROTOCOL_VERSION};

/// The Noise pattern every relay session speaks. `IK` pins the responder's
/// static and authenticates the initiator's; `psk2` mixes the per-pair PSK into
/// the second handshake message; `25519_ChaChaPoly_BLAKE2s` is the cipher
/// suite. Must match the pattern the identity layer mints keypairs for.
pub const NOISE_PATTERN: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

/// Maximum plaintext, in bytes, that fits in a single Noise transport message:
/// the Noise message ceiling (65535) minus the 16-byte ChaChaPoly AEAD tag.
/// [`Transport::seal`] rejects anything larger rather than truncating.
pub const MAX_NOISE_PLAINTEXT: usize = 65519;

/// Chunk size for raw PTY byte runs. Kept well under [`MAX_NOISE_PLAINTEXT`] so
/// that even the JSON-array encoding of a full chunk (each byte becomes up to
/// four characters) stays within one transport message.
pub const PTY_CHUNK_BYTES: usize = 8192;

/// Noise's absolute per-message ceiling (plaintext + tag), from the spec.
const MAX_NOISE_MESSAGE: usize = 65535;

/// Length of the ChaChaPoly AEAD tag appended to every ciphertext.
const NOISE_TAG_LEN: usize = 16;

/// The PSK position for `IKpsk2`: the PSK is mixed after the second message.
const PSK_POSITION: u8 = 2;

/// Errors from the Noise handshake or transport.
#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    /// A `snow` operation failed (handshake step, key setup, decrypt, …).
    /// Carries `snow`'s own message; the concrete `snow::Error` is not
    /// `std::error::Error`, so it is flattened to a string here.
    #[error("noise protocol error: {0}")]
    Snow(String),
    /// A message's encoded plaintext exceeded [`MAX_NOISE_PLAINTEXT`]. The
    /// sender must split the payload (see [`chunk_bytes`]) — the transport
    /// never truncates, since a silently clipped PTY stream corrupts the
    /// terminal.
    #[error("message plaintext of {size} bytes exceeds the {max}-byte Noise limit")]
    Oversized {
        /// The encoded plaintext length that was rejected.
        size: usize,
        /// The maximum allowed ([`MAX_NOISE_PLAINTEXT`]).
        max: usize,
    },
    /// The message could not be serialized to JSON before sealing.
    #[error("could not serialize message: {0}")]
    Serialize(serde_json::Error),
    /// The decrypted plaintext could not be deserialized from JSON.
    #[error("could not deserialize message: {0}")]
    Deserialize(serde_json::Error),
}

/// Maps a `snow::Error` into a [`NoiseError::Snow`].
fn snow_err(e: snow::Error) -> NoiseError {
    NoiseError::Snow(e.to_string())
}

/// Builds the Noise `NoiseParams` for [`NOISE_PATTERN`].
fn noise_params() -> Result<snow::params::NoiseParams, NoiseError> {
    NOISE_PATTERN.parse().map_err(snow_err)
}

/// Which handshake a prologue is for (ADR-0021 D2 domain separation). The
/// leading prologue byte differs, so a pairing handshake can never complete
/// against a session responder or vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeKind {
    /// A normal session attach between a paired device and the bridge.
    Session = 0,
    /// The first-contact pairing handshake (PSK = the QR's pairing secret).
    Pairing = 1,
}

/// Context prologue mixed into the handshake hash (ADR-0021 D2/D14/D16).
///
/// Layout: `kind` (1 byte) ‖ `ENVELOPE_VERSION` (1) ‖ `PROTOCOL_VERSION`
/// (big-endian u32) ‖ initiator identity id (32) ‖ initiator routing id (32) ‖
/// responder/bridge id (32). Both halves must construct byte-identical
/// prologues or the handshake's first AEAD check fails, so the relay cannot
/// re-point a session at a different route or peer, and a pairing handshake
/// can never complete against a session responder.
pub fn prologue(
    kind: HandshakeKind,
    initiator_identity: &DeviceId,
    initiator_routing: &DeviceId,
    responder: &DeviceId,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 1 + 4 + 32 * 3);
    out.push(kind as u8);
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    out.extend_from_slice(&initiator_identity.0);
    out.extend_from_slice(&initiator_routing.0);
    out.extend_from_slice(&responder.0);
    out
}

/// A Noise handshake in progress. Thin wrapper over `snow::HandshakeState` used
/// identically by the initiator (client) and responder (bridge) halves.
pub struct Handshake {
    state: snow::HandshakeState,
}

impl Handshake {
    /// Starts the initiator (client) side. Pins `peer_pub` as the responder's
    /// static (from the pairing file), presents `local_priv` as the initiator's
    /// static, and binds `psk` + `prologue` into the session.
    pub fn initiator(
        local_priv: &[u8],
        peer_pub: &[u8],
        psk: &[u8; 32],
        prologue: &[u8],
    ) -> Result<Handshake, NoiseError> {
        let state = snow::Builder::new(noise_params()?)
            .local_private_key(local_priv)
            .map_err(snow_err)?
            .remote_public_key(peer_pub)
            .map_err(snow_err)?
            .psk(PSK_POSITION, psk)
            .map_err(snow_err)?
            .prologue(prologue)
            .map_err(snow_err)?
            .build_initiator()
            .map_err(snow_err)?;
        Ok(Handshake { state })
    }

    /// Starts the responder (bridge) side. Presents `local_priv` as the
    /// responder's static and binds `psk` + `prologue`. The initiator's static
    /// is learned during the handshake and returned by [`into_transport`].
    ///
    /// [`into_transport`]: Handshake::into_transport
    pub fn responder(
        local_priv: &[u8],
        psk: &[u8; 32],
        prologue: &[u8],
    ) -> Result<Handshake, NoiseError> {
        let state = snow::Builder::new(noise_params()?)
            .local_private_key(local_priv)
            .map_err(snow_err)?
            .psk(PSK_POSITION, psk)
            .map_err(snow_err)?
            .prologue(prologue)
            .map_err(snow_err)?
            .build_responder()
            .map_err(snow_err)?;
        Ok(Handshake { state })
    }

    /// Writes the next handshake message, embedding `payload`, and returns the
    /// bytes to send to the peer.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self
            .state
            .write_message(payload, &mut buf)
            .map_err(snow_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Reads a handshake message received from the peer and returns any
    /// embedded payload. A wrong PSK, pinned static, or prologue surfaces here
    /// as a decrypt failure.
    pub fn read_message(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
        let n = self.state.read_message(msg, &mut buf).map_err(snow_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// True once the handshake has completed and [`into_transport`] can run.
    ///
    /// [`into_transport`]: Handshake::into_transport
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Consumes a finished handshake, returning the [`Transport`] and the
    /// peer's authenticated static public key. The responder verifies these
    /// bytes against the roster entry's pinned key (spec C7); the initiator
    /// already pinned the responder's static, so its copy is redundant.
    pub fn into_transport(self) -> Result<(Transport, Vec<u8>), NoiseError> {
        let remote_static = self
            .state
            .get_remote_static()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let state = self.state.into_transport_mode().map_err(snow_err)?;
        Ok((Transport { state }, remote_static))
    }
}

/// An established Noise transport. Seals/opens one application message per call;
/// `snow` tracks the AEAD nonce, so messages must be opened in the exact order
/// they were sealed (the property the resume design leans on).
pub struct Transport {
    state: snow::TransportState,
}

impl Transport {
    /// Serializes `msg` (JSON), enforces [`MAX_NOISE_PLAINTEXT`] on the
    /// *encoded* bytes (a [`NoiseError::Oversized`] error, never truncation),
    /// then encrypts it into one envelope payload.
    ///
    /// The serialized plaintext buffer is zeroized after sealing (#278): some
    /// sealed messages carry secrets (a pairing `Grant` carries the session
    /// PSK and device token), and wiping every message costs one memset.
    pub fn seal<T: Serialize>(&mut self, msg: &T) -> Result<Vec<u8>, NoiseError> {
        let plaintext = Zeroizing::new(serde_json::to_vec(msg).map_err(NoiseError::Serialize)?);
        if plaintext.len() > MAX_NOISE_PLAINTEXT {
            return Err(NoiseError::Oversized {
                size: plaintext.len(),
                max: MAX_NOISE_PLAINTEXT,
            });
        }
        let mut buf = vec![0u8; plaintext.len() + NOISE_TAG_LEN];
        let n = self
            .state
            .write_message(&plaintext, &mut buf)
            .map_err(snow_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypts one envelope payload and deserializes it as `T`. Fails on a
    /// tampered ciphertext or an out-of-order message (nonce mismatch).
    ///
    /// The decrypted plaintext buffer is zeroized after deserializing (#278),
    /// mirroring [`seal`](Transport::seal).
    pub fn open<T: DeserializeOwned>(&mut self, ciphertext: &[u8]) -> Result<T, NoiseError> {
        // The plaintext is always shorter than the ciphertext (which carries
        // the AEAD tag), so `ciphertext.len()` is a safe output-buffer size.
        let mut buf = Zeroizing::new(vec![0u8; ciphertext.len()]);
        let n = self
            .state
            .read_message(ciphertext, &mut buf)
            .map_err(snow_err)?;
        serde_json::from_slice(&buf[..n]).map_err(NoiseError::Deserialize)
    }
}

/// Splits a raw PTY byte run into chunks of at most [`PTY_CHUNK_BYTES`], each of
/// which is small enough to seal after JSON encoding. Empty input yields an
/// empty vec (nothing to send); concatenating the chunks reproduces the input
/// exactly.
pub fn chunk_bytes(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.chunks(PTY_CHUNK_BYTES).map(<[u8]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_protocol::{BridgeMessage, ChannelOutput, ClientMessage};

    const ALICE_ID: DeviceId = DeviceId([0x11; 32]);
    const ALICE_ROUTING: DeviceId = DeviceId([0x22; 32]);
    const BRIDGE_ID: DeviceId = DeviceId([0x33; 32]);

    /// Mints a fresh X25519 static keypair the same way the identity layer does.
    fn keypair() -> snow::Keypair {
        snow::Builder::new(noise_params().expect("params"))
            .generate_keypair()
            .expect("keypair")
    }

    /// The material both halves share for a successful pairing.
    struct Pairing {
        client: snow::Keypair,
        bridge: snow::Keypair,
        psk: [u8; 32],
        prologue: Vec<u8>,
    }

    impl Pairing {
        fn new() -> Pairing {
            Pairing {
                client: keypair(),
                bridge: keypair(),
                psk: [0x5a; 32],
                prologue: prologue(
                    HandshakeKind::Session,
                    &ALICE_ID,
                    &ALICE_ROUTING,
                    &BRIDGE_ID,
                ),
            }
        }
    }

    /// An established transport plus the peer static it authenticated.
    type Established = (Transport, Vec<u8>);

    /// Drives an initiator/responder pair to completion, returning both
    /// transports (initiator first) plus the static each side authenticated for
    /// its peer. Any handshake step failing is surfaced as an `Err`.
    fn drive(
        mut initiator: Handshake,
        mut responder: Handshake,
    ) -> Result<(Established, Established), NoiseError> {
        // IK is a two-message handshake: -> e, es, s, ss  then  <- e, ee, se
        // (psk2 mixes the PSK into the second message).
        let msg1 = initiator.write_message(&[])?;
        responder.read_message(&msg1)?;
        let msg2 = responder.write_message(&[])?;
        initiator.read_message(&msg2)?;

        assert!(initiator.is_finished(), "initiator handshake unfinished");
        assert!(responder.is_finished(), "responder handshake unfinished");

        let init_transport = initiator.into_transport()?;
        let resp_transport = responder.into_transport()?;
        Ok((init_transport, resp_transport))
    }

    fn initiator(p: &Pairing) -> Handshake {
        Handshake::initiator(&p.client.private, &p.bridge.public, &p.psk, &p.prologue)
            .expect("build initiator")
    }

    fn responder(p: &Pairing) -> Handshake {
        Handshake::responder(&p.bridge.private, &p.psk, &p.prologue).expect("build responder")
    }

    #[test]
    fn ikpsk2_handshake_completes_and_round_trips_a_message() {
        let p = Pairing::new();
        let ((mut client, bridge_static), (mut bridge, client_static)) =
            drive(initiator(&p), responder(&p)).expect("handshake");

        // The responder learns the initiator's real static; the initiator's
        // pinned view of the responder matches the responder's own key.
        assert_eq!(
            client_static, p.client.public,
            "responder must authenticate the initiator's static"
        );
        assert_eq!(
            bridge_static, p.bridge.public,
            "initiator's peer static is the pinned bridge key"
        );

        // Client -> bridge.
        let hello = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let sealed = client.seal(&hello).expect("seal client");
        let opened: ClientMessage = bridge.open(&sealed).expect("open on bridge");
        assert_eq!(opened, hello);

        // Bridge -> client.
        let out = BridgeMessage::Output(ChannelOutput::Bytes(b"hi from bridge".to_vec()));
        let sealed = bridge.seal(&out).expect("seal bridge");
        let opened: BridgeMessage = client.open(&sealed).expect("open on client");
        assert_eq!(opened, out);
    }

    #[test]
    fn wrong_psk_fails_handshake() {
        let p = Pairing::new();
        let client = initiator(&p);
        let bad = Handshake::responder(&p.bridge.private, &[0xff; 32], &p.prologue)
            .expect("build responder");
        assert!(
            drive(client, bad).is_err(),
            "a mismatched PSK must fail the handshake, not silently pass"
        );
    }

    #[test]
    fn wrong_bridge_static_fails_handshake() {
        let p = Pairing::new();
        // Initiator pins a *different* responder static than the bridge holds.
        let imposter = keypair();
        let client = Handshake::initiator(&p.client.private, &imposter.public, &p.psk, &p.prologue)
            .expect("build initiator");
        assert!(
            drive(client, responder(&p)).is_err(),
            "pinning the wrong bridge static must fail the handshake"
        );
    }

    #[test]
    fn mismatched_prologue_fails_handshake() {
        let p = Pairing::new();
        // Responder binds a prologue that differs only in the routing id.
        let other_routing = DeviceId([0x99; 32]);
        let bridge_prologue = prologue(
            HandshakeKind::Session,
            &ALICE_ID,
            &other_routing,
            &BRIDGE_ID,
        );
        let bridge = Handshake::responder(&p.bridge.private, &p.psk, &bridge_prologue)
            .expect("build responder");
        assert!(
            drive(initiator(&p), bridge).is_err(),
            "a prologue mismatch must fail the handshake"
        );
    }

    #[test]
    fn session_and_pairing_prologues_differ() {
        let s = prologue(
            HandshakeKind::Session,
            &ALICE_ID,
            &ALICE_ROUTING,
            &BRIDGE_ID,
        );
        let p = prologue(
            HandshakeKind::Pairing,
            &ALICE_ID,
            &ALICE_ROUTING,
            &BRIDGE_ID,
        );
        assert_ne!(
            s, p,
            "domain separation: the kind byte must change the prologue"
        );
        assert_eq!(s[0], 0);
        assert_eq!(p[0], 1);
    }

    #[test]
    fn cross_kind_handshake_fails() {
        // Initiator binds a Pairing prologue; responder binds Session — must fail.
        let client_keys = keypair();
        let bridge_keys = keypair();
        let psk = [0x5a; 32];
        let init_pro = prologue(
            HandshakeKind::Pairing,
            &ALICE_ID,
            &ALICE_ROUTING,
            &BRIDGE_ID,
        );
        let resp_pro = prologue(
            HandshakeKind::Session,
            &ALICE_ID,
            &ALICE_ROUTING,
            &BRIDGE_ID,
        );
        let initiator =
            Handshake::initiator(&client_keys.private, &bridge_keys.public, &psk, &init_pro)
                .expect("init");
        let responder = Handshake::responder(&bridge_keys.private, &psk, &resp_pro).expect("resp");
        assert!(
            drive(initiator, responder).is_err(),
            "cross-kind prologue must fail the handshake"
        );
    }

    #[test]
    fn tampered_ciphertext_fails_open() {
        let p = Pairing::new();
        let ((mut client, _), (mut bridge, _)) =
            drive(initiator(&p), responder(&p)).expect("handshake");

        let msg = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let mut sealed = client.seal(&msg).expect("seal");
        sealed[0] ^= 0x01; // flip one bit
        let opened: Result<ClientMessage, _> = bridge.open(&sealed);
        assert!(opened.is_err(), "a tampered ciphertext must fail to open");
    }

    #[test]
    fn seal_rejects_oversized_plaintext() {
        let p = Pairing::new();
        let ((mut client, _), (mut bridge, _)) =
            drive(initiator(&p), responder(&p)).expect("handshake");

        // 70_000 raw bytes: JSON-encoded as an array this is far over the cap.
        let big = BridgeMessage::Output(ChannelOutput::Bytes(vec![0u8; 70_000]));
        match client.seal(&big) {
            Err(NoiseError::Oversized { size, max }) => {
                assert_eq!(max, MAX_NOISE_PLAINTEXT);
                assert!(size > MAX_NOISE_PLAINTEXT, "reported size must exceed cap");
            }
            other => panic!("expected Oversized, got {other:?}"),
        }

        // Chunking the same run makes every piece seal + open cleanly.
        for chunk in chunk_bytes(vec![0u8; 70_000]) {
            let msg = BridgeMessage::Output(ChannelOutput::Bytes(chunk));
            let sealed = client.seal(&msg).expect("chunk seals within cap");
            let opened: BridgeMessage = bridge.open(&sealed).expect("chunk opens");
            assert_eq!(opened, msg);
        }
    }

    #[test]
    fn chunk_bytes_boundaries() {
        assert!(chunk_bytes(Vec::new()).is_empty(), "empty -> empty vec");

        let one = chunk_bytes(vec![0u8; PTY_CHUNK_BYTES]);
        assert_eq!(one.len(), 1, "exactly one chunk at the boundary");

        let two = chunk_bytes(vec![0u8; PTY_CHUNK_BYTES + 1]);
        assert_eq!(two.len(), 2, "one byte over the boundary spills a chunk");
        assert_eq!(two[0].len(), PTY_CHUNK_BYTES);
        assert_eq!(two[1].len(), 1);

        // Content re-concatenates exactly, in order.
        let original: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        let recombined: Vec<u8> = chunk_bytes(original.clone()).concat();
        assert_eq!(recombined, original);
    }

    #[test]
    fn messages_must_arrive_in_order() {
        let p = Pairing::new();
        let ((mut client, _), (mut bridge, _)) =
            drive(initiator(&p), responder(&p)).expect("handshake");

        let first = client
            .seal(&ClientMessage::Hello {
                protocol_version: 1,
            })
            .expect("seal first");
        let second = client
            .seal(&ClientMessage::Hello {
                protocol_version: 2,
            })
            .expect("seal second");

        // Opening the second before the first breaks snow's nonce sequence.
        let out_of_order: Result<ClientMessage, _> = bridge.open(&second);
        assert!(
            out_of_order.is_err(),
            "opening messages out of order must fail (nonce property)"
        );

        // The stream is poisoned once a message is skipped: even the in-order
        // message no longer matches the expected nonce.
        let _ = first;
    }
}
