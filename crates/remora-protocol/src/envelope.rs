//! Outer envelope wire format for relay mode (ADR-0021).
//!
//! Relay mode wraps the existing session protocol (defined by [`crate`]'s
//! other modules) in a thin routing envelope: a header the relay parses
//! (source/destination device IDs, frame type) around an opaque payload the
//! relay never inspects — in practice Noise ciphertext, but the codec here
//! does not know or care what the payload contains. This is a hand-rolled
//! binary format, not JSON: the relay is on the hot forwarding path for
//! every keystroke, and framing a fixed-offset header is cheaper than
//! parsing a self-describing one.
//!
//! Wire layout, byte-exact:
//!
//! ```text
//! offset 0    u8   ENVELOPE_VERSION (decode rejects != 1)
//! offset 1    u8   frame type (decode rejects > 3)
//! offset 2    [u8;32] src routing id
//! offset 34   [u8;32] dst routing id
//! offset 66   payload (len 0..=65535; decode rejects longer)
//! ```
//!
//! [`RelayHello`] is the one payload type this module defines: unlike the
//! rest of an envelope's payload, the hello frame is relay-visible (it is
//! how a device or bridge authenticates *to the relay*, before any Noise
//! session exists), so it is plain serde JSON rather than opaque bytes.

use serde::{Deserialize, Serialize};

/// Version of the envelope wire format defined by this module.
pub const ENVELOPE_VERSION: u8 = 1;

/// Length of the fixed envelope header in bytes: version (1) + frame type
/// (1) + src [`DeviceId`] (32) + dst `DeviceId` (32).
pub const ENVELOPE_HEADER_LEN: usize = 66;

/// Maximum payload length in bytes. [`Envelope::decode`] rejects anything
/// longer.
pub const MAX_ENVELOPE_PAYLOAD: usize = 65535;

/// Opaque 32-byte device routing identity (ADR-0021).
///
/// Serializes as 64 lowercase hex characters, both via [`std::fmt::Display`]
/// and via serde (JSON strings, e.g. inside [`RelayHello`]). Parsing accepts
/// upper- or lower-case hex; only length and hex-ness are validated — the
/// bytes themselves are opaque to this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(pub [u8; 32]);

impl DeviceId {
    /// Reserved all-zero id. Valid only as the `dst` of `Hello` frames,
    /// before the relay has assigned or the peer has learned a real routing
    /// id; any other use is a protocol violation the relay/bridge reject.
    pub const ZERO: DeviceId = DeviceId([0u8; 32]);

    /// True if this is the reserved [`DeviceId::ZERO`] value.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Error returned when a string is not a valid 64-hex-character [`DeviceId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDeviceIdError {
    value: String,
}

impl std::fmt::Display for InvalidDeviceIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid device id `{}`: must be 64 hex characters",
            self.value
        )
    }
}

impl std::error::Error for InvalidDeviceIdError {}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl std::str::FromStr for DeviceId {
    type Err = InvalidDeviceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(InvalidDeviceIdError {
                value: s.to_string(),
            });
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            let (Some(hi), Some(lo)) = (hex_val(bytes[i * 2]), hex_val(bytes[i * 2 + 1])) else {
                return Err(InvalidDeviceIdError {
                    value: s.to_string(),
                });
            };
            out[i] = (hi << 4) | lo;
        }
        Ok(DeviceId(out))
    }
}

impl TryFrom<String> for DeviceId {
    type Error = InvalidDeviceIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<DeviceId> for String {
    fn from(id: DeviceId) -> Self {
        id.to_string()
    }
}

/// Envelope frame kind (ADR-0021). The relay dispatches on this byte without
/// touching the payload; `Pairing` and `PushTrigger` frames are reserved for
/// the pairing and push follow-ups (#232, #233) — this codec accepts and
/// round-trips them, but nothing in this crate constructs or interprets one
/// yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Hello/auth frame: a device or bridge introducing itself to the relay.
    Hello = 0,
    /// Opaque session-protocol payload (Noise ciphertext in practice).
    Data = 1,
    /// Reserved for the pairing follow-up (#232).
    Pairing = 2,
    /// Reserved for the push-notification follow-up (#233).
    PushTrigger = 3,
}

/// One relay-routed frame: the fixed header plus an opaque payload.
///
/// The relay reads `frame_type`/`src`/`dst` to route; `payload` is opaque to
/// it (Noise ciphertext in practice, plain JSON only for [`RelayHello`]
/// during the pre-Noise hello handshake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub frame_type: FrameType,
    pub src: DeviceId,
    pub dst: DeviceId,
    pub payload: Vec<u8>,
}

impl Envelope {
    /// Encodes this envelope as `header ‖ payload` per the module's wire
    /// layout. Does not validate `payload.len()`; callers that decoded from
    /// [`Envelope::decode`] already have a bounded payload, and callers that
    /// construct one directly are trusted to respect [`MAX_ENVELOPE_PAYLOAD`].
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ENVELOPE_HEADER_LEN + self.payload.len());
        buf.push(ENVELOPE_VERSION);
        buf.push(self.frame_type as u8);
        buf.extend_from_slice(&self.src.0);
        buf.extend_from_slice(&self.dst.0);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decodes an envelope from `bytes`, validating version, frame type,
    /// minimum length, and payload length per the module's wire layout.
    pub fn decode(bytes: &[u8]) -> Result<Envelope, EnvelopeError> {
        if bytes.len() < ENVELOPE_HEADER_LEN {
            return Err(EnvelopeError::Truncated(bytes.len()));
        }
        let version = bytes[0];
        if version != ENVELOPE_VERSION {
            return Err(EnvelopeError::UnknownVersion(version));
        }
        let frame_type = match bytes[1] {
            0 => FrameType::Hello,
            1 => FrameType::Data,
            2 => FrameType::Pairing,
            3 => FrameType::PushTrigger,
            other => return Err(EnvelopeError::UnknownFrameType(other)),
        };
        let mut src = [0u8; 32];
        src.copy_from_slice(&bytes[2..34]);
        let mut dst = [0u8; 32];
        dst.copy_from_slice(&bytes[34..66]);
        let payload = &bytes[ENVELOPE_HEADER_LEN..];
        if payload.len() > MAX_ENVELOPE_PAYLOAD {
            return Err(EnvelopeError::Oversized(payload.len()));
        }
        Ok(Envelope {
            frame_type,
            src: DeviceId(src),
            dst: DeviceId(dst),
            payload: payload.to_vec(),
        })
    }
}

/// Error returned when [`Envelope::decode`] rejects a byte slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The version byte was not [`ENVELOPE_VERSION`].
    UnknownVersion(u8),
    /// The frame type byte was not one of the four defined [`FrameType`]
    /// values.
    UnknownFrameType(u8),
    /// Fewer than [`ENVELOPE_HEADER_LEN`] bytes were supplied.
    Truncated(usize),
    /// The payload exceeded [`MAX_ENVELOPE_PAYLOAD`].
    Oversized(usize),
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeError::UnknownVersion(v) => {
                write!(
                    f,
                    "unknown envelope version {v}: expected {ENVELOPE_VERSION}"
                )
            }
            EnvelopeError::UnknownFrameType(t) => {
                write!(f, "unknown envelope frame type {t}: expected 0-3")
            }
            EnvelopeError::Truncated(n) => {
                write!(
                    f,
                    "truncated envelope: {n} bytes, header requires {ENVELOPE_HEADER_LEN}"
                )
            }
            EnvelopeError::Oversized(n) => {
                write!(
                    f,
                    "oversized envelope payload: {n} bytes exceeds max {MAX_ENVELOPE_PAYLOAD}"
                )
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Relay-visible hello payload (serde JSON). See ADR-0021's D5/D16: this is
/// how a device or bridge introduces itself to the relay before any Noise
/// session exists, so — unlike the rest of an envelope's payload — its
/// fields are plaintext the relay legitimately reads to route and
/// authenticate the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RelayHello {
    /// Whether the connecting peer is a bridge or a device.
    pub role: HelloRole,
    /// The relay-issued credential (rendezvous token or bridge registration
    /// token) proving admission to routing.
    pub token: String,
    /// Pairing identity: the long-lived key this peer authenticates as.
    pub device_id: DeviceId,
    /// Envelope routing id for this connection. Equal to `device_id` for
    /// bridges; devices are routed by the bridge's id, not their own.
    pub routing_id: DeviceId,
    /// For a device: the bridge it wants routed to. For a bridge: its own
    /// id (mirrors `routing_id`).
    pub bridge_id: DeviceId,
}

/// Which side of a relay connection a [`RelayHello`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloRole {
    Bridge,
    Device,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonzero_device_id(fill: u8) -> DeviceId {
        DeviceId([fill; 32])
    }

    #[test]
    fn encode_decode_round_trips_all_frame_types() {
        for frame_type in [
            FrameType::Hello,
            FrameType::Data,
            FrameType::Pairing,
            FrameType::PushTrigger,
        ] {
            let envelope = Envelope {
                frame_type,
                src: nonzero_device_id(0x11),
                dst: nonzero_device_id(0x22),
                payload: b"x".to_vec(),
            };
            let encoded = envelope.encode();
            let decoded = Envelope::decode(&encoded).expect("decode");
            assert_eq!(decoded, envelope, "round trip for {frame_type:?}");
        }
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let mut bytes = vec![2u8, 1u8];
        bytes.extend_from_slice(&[0u8; 64]);
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN);
        let err = Envelope::decode(&bytes).expect_err("unknown version");
        assert_eq!(err, EnvelopeError::UnknownVersion(2));
    }

    #[test]
    fn decode_rejects_unknown_frame_type() {
        let mut bytes = vec![ENVELOPE_VERSION, 4u8];
        bytes.extend_from_slice(&[0u8; 64]);
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN);
        let err = Envelope::decode(&bytes).expect_err("unknown frame type");
        assert_eq!(err, EnvelopeError::UnknownFrameType(4));
    }

    #[test]
    fn decode_rejects_truncated() {
        let bytes = vec![0u8; 65];
        let err = Envelope::decode(&bytes).expect_err("truncated");
        assert_eq!(err, EnvelopeError::Truncated(65));
    }

    #[test]
    fn decode_rejects_oversized() {
        let envelope = Envelope {
            frame_type: FrameType::Data,
            src: nonzero_device_id(0x33),
            dst: nonzero_device_id(0x44),
            payload: vec![0u8; MAX_ENVELOPE_PAYLOAD + 1],
        };
        let encoded = envelope.encode();
        let err = Envelope::decode(&encoded).expect_err("oversized");
        assert_eq!(err, EnvelopeError::Oversized(MAX_ENVELOPE_PAYLOAD + 1));
    }

    #[test]
    fn empty_payload_round_trips() {
        let envelope = Envelope {
            frame_type: FrameType::Data,
            src: nonzero_device_id(0x55),
            dst: nonzero_device_id(0x66),
            payload: Vec::new(),
        };
        let encoded = envelope.encode();
        assert_eq!(encoded.len(), ENVELOPE_HEADER_LEN);
        let decoded = Envelope::decode(&encoded).expect("decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn device_id_hex_serde_round_trips() {
        let id = DeviceId([0xab; 32]);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        let back: DeviceId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);

        for bad in [
            r#""""#,
            &format!("\"{}\"", "ab".repeat(31)),   // too short
            &format!("\"{}\"", "ab".repeat(33)),   // too long
            &format!("\"{}zz\"", "ab".repeat(31)), // non-hex chars
        ] {
            assert!(
                serde_json::from_str::<DeviceId>(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn zero_device_id_is_flagged() {
        assert!(DeviceId::ZERO.is_zero());
        assert!(DeviceId([0u8; 32]).is_zero());
        assert!(!nonzero_device_id(0x01).is_zero());
    }

    #[test]
    fn relay_hello_wire_format() {
        let hello = RelayHello {
            role: HelloRole::Device,
            token: "tok".to_string(),
            device_id: DeviceId([0x11; 32]),
            routing_id: DeviceId([0x22; 32]),
            bridge_id: DeviceId([0x33; 32]),
        };
        let json = serde_json::to_string(&hello).expect("serialize");
        let expected = format!(
            "{{\"role\":\"device\",\"token\":\"tok\",\"device_id\":\"{}\",\"routing_id\":\"{}\",\"bridge_id\":\"{}\"}}",
            "11".repeat(32),
            "22".repeat(32),
            "33".repeat(32),
        );
        assert_eq!(json, expected);
        let back: RelayHello = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, hello);
    }

    #[test]
    fn frame_type_reserved_values_decode() {
        for (byte, expected) in [(2u8, FrameType::Pairing), (3u8, FrameType::PushTrigger)] {
            let envelope = Envelope {
                frame_type: expected,
                src: nonzero_device_id(0x77),
                dst: nonzero_device_id(0x88),
                payload: b"reserved".to_vec(),
            };
            let encoded = envelope.encode();
            assert_eq!(encoded[1], byte);
            let decoded = Envelope::decode(&encoded).expect("decode");
            assert_eq!(decoded.frame_type, expected);
        }
    }
}
