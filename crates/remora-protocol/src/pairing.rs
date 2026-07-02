//! The pairing code (ADR-0021 D1): the compact, single-string carrier a QR
//! encodes and the "Copy pairing code" fallback pastes. It splits the pairing
//! secret from the relay's routing token — the relay sees only the rendezvous
//! token, never the `psk`.
//!
//! Wire form: `remora-pair:1:<base64url-nopadding(JSON)>`. Exactly one of
//! `relay_url` (with `rendezvous_token`) or `mesh_addr` is present; parsing
//! rejects any other combination, unknown versions, and non-32-byte keys.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// The `remora-pair` string version this module reads and writes.
pub const PAIRING_CODE_VERSION: u32 = 1;

const STD_B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;
const URL_B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Everything a device needs to reach and authenticate to one bridge. Decoded
/// from (or encoded to) the `remora-pair:1:…` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    pub relay_url: Option<String>,
    pub rendezvous_token: Option<String>,
    pub mesh_addr: Option<String>,
    pub psk: [u8; 32],
    pub bridge_id: DeviceId,
    pub bridge_key: [u8; 32],
    pub bridge_name: Option<String>,
    pub min_protocol: u32,
}

/// On-the-wire JSON shape (keys as base64, secrets as standard base64).
#[derive(Serialize, Deserialize)]
struct PairingCodeJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relay_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendezvous_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mesh_addr: Option<String>,
    psk: String,
    bridge_id: String,
    bridge_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bridge_name: Option<String>,
    min_protocol: u32,
}

/// Why [`PairingCode::parse`] rejected a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingCodeError {
    /// The string did not start with `remora-pair:`.
    BadPrefix,
    /// The version segment was not [`PAIRING_CODE_VERSION`].
    UnknownVersion(u32),
    /// The payload was not valid base64url.
    BadBase64,
    /// The decoded bytes were not valid pairing JSON.
    BadJson,
    /// Not exactly one transport (relay+rendezvous XOR mesh) was present.
    TransportAmbiguous,
    /// A base64 key or secret did not decode to 32 bytes.
    BadKeyLength,
}

impl std::fmt::Display for PairingCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingCodeError::BadPrefix => write!(f, "not a remora-pair code"),
            PairingCodeError::UnknownVersion(v) => {
                write!(f, "unsupported pairing code version {v}")
            }
            PairingCodeError::BadBase64 => write!(f, "pairing code payload is not valid base64url"),
            PairingCodeError::BadJson => write!(f, "pairing code payload is not valid JSON"),
            PairingCodeError::TransportAmbiguous => {
                write!(
                    f,
                    "pairing code must carry exactly one of relay+rendezvous or mesh address"
                )
            }
            PairingCodeError::BadKeyLength => write!(f, "pairing code key/secret is not 32 bytes"),
        }
    }
}

impl std::error::Error for PairingCodeError {}

fn decode32(s: &str) -> Result<[u8; 32], PairingCodeError> {
    let bytes = STD_B64
        .decode(s)
        .map_err(|_| PairingCodeError::BadKeyLength)?;
    bytes.try_into().map_err(|_| PairingCodeError::BadKeyLength)
}

impl PairingCode {
    /// Encodes to `remora-pair:1:<base64url-nopadding(JSON)>`.
    pub fn encode(&self) -> String {
        let json = PairingCodeJson {
            relay_url: self.relay_url.clone(),
            rendezvous_token: self.rendezvous_token.clone(),
            mesh_addr: self.mesh_addr.clone(),
            psk: STD_B64.encode(self.psk),
            bridge_id: self.bridge_id.to_string(),
            bridge_key: STD_B64.encode(self.bridge_key),
            bridge_name: self.bridge_name.clone(),
            min_protocol: self.min_protocol,
        };
        // serde_json::to_string cannot fail for this owned struct.
        let body = serde_json::to_string(&json).unwrap_or_default();
        format!(
            "remora-pair:{PAIRING_CODE_VERSION}:{}",
            URL_B64.encode(body)
        )
    }

    /// Parses a `remora-pair:1:…` string, validating version, base64, JSON,
    /// transport exclusivity, and key lengths.
    pub fn parse(s: &str) -> Result<PairingCode, PairingCodeError> {
        let rest = s
            .strip_prefix("remora-pair:")
            .ok_or(PairingCodeError::BadPrefix)?;
        let (version, payload) = rest.split_once(':').ok_or(PairingCodeError::BadPrefix)?;
        let version: u32 = version.parse().map_err(|_| PairingCodeError::BadPrefix)?;
        if version != PAIRING_CODE_VERSION {
            return Err(PairingCodeError::UnknownVersion(version));
        }
        let bytes = URL_B64
            .decode(payload)
            .map_err(|_| PairingCodeError::BadBase64)?;
        let json: PairingCodeJson =
            serde_json::from_slice(&bytes).map_err(|_| PairingCodeError::BadJson)?;

        // Exactly one transport: relay_url (with rendezvous) XOR mesh_addr.
        let relay_ok = json.relay_url.is_some() && json.rendezvous_token.is_some();
        let relay_partial = json.relay_url.is_some() != json.rendezvous_token.is_some();
        let mesh_ok = json.mesh_addr.is_some();
        if relay_partial || (relay_ok == mesh_ok) {
            return Err(PairingCodeError::TransportAmbiguous);
        }

        let bridge_id = json
            .bridge_id
            .parse::<DeviceId>()
            .map_err(|_| PairingCodeError::BadKeyLength)?;
        Ok(PairingCode {
            relay_url: json.relay_url,
            rendezvous_token: json.rendezvous_token,
            mesh_addr: json.mesh_addr,
            psk: decode32(&json.psk)?,
            bridge_id,
            bridge_key: decode32(&json.bridge_key)?,
            bridge_name: json.bridge_name,
            min_protocol: json.min_protocol,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay_code() -> PairingCode {
        PairingCode {
            relay_url: Some("wss://relay.example/ws".to_string()),
            rendezvous_token: Some("rvz-123".to_string()),
            mesh_addr: None,
            psk: [0x5a; 32],
            bridge_id: DeviceId([0x11; 32]),
            bridge_key: [0x22; 32],
            bridge_name: Some("desktop".to_string()),
            min_protocol: 3,
        }
    }

    #[test]
    fn round_trips_relay_code() {
        let code = relay_code();
        let s = code.encode();
        assert!(s.starts_with("remora-pair:1:"), "got {s}");
        let back = PairingCode::parse(&s).expect("parse");
        assert_eq!(back, code);
    }

    #[test]
    fn rejects_unknown_version() {
        let err = PairingCode::parse("remora-pair:9:abc").expect_err("bad version");
        assert!(matches!(err, PairingCodeError::UnknownVersion(9)));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(matches!(
            PairingCode::parse("nope:1:abc").expect_err("bad prefix"),
            PairingCodeError::BadPrefix
        ));
    }

    #[test]
    fn rejects_both_transports_present() {
        // Hand-build JSON with both relay_url and mesh_addr — must be rejected.
        let json = r#"{"relay_url":"wss://r/ws","rendezvous_token":"t","mesh_addr":"host:1","psk":"WlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlpaWlo=","bridge_id":"1111111111111111111111111111111111111111111111111111111111111111","bridge_key":"IiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiI=","min_protocol":3}"#;
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        let err = PairingCode::parse(&format!("remora-pair:1:{enc}")).expect_err("ambiguous");
        assert!(matches!(err, PairingCodeError::TransportAmbiguous));
    }

    #[test]
    fn rejects_relay_without_rendezvous() {
        let mut code = relay_code();
        code.rendezvous_token = None;
        let s = code.encode();
        assert!(matches!(
            PairingCode::parse(&s).expect_err("relay needs rendezvous"),
            PairingCodeError::TransportAmbiguous
        ));
    }

    #[test]
    fn parses_mesh_code() {
        let code = PairingCode {
            relay_url: None,
            rendezvous_token: None,
            mesh_addr: Some("bridge.tailnet:9440".to_string()),
            psk: [1; 32],
            bridge_id: DeviceId([2; 32]),
            bridge_key: [3; 32],
            bridge_name: None,
            min_protocol: 3,
        };
        assert_eq!(
            PairingCode::parse(&code.encode()).expect("mesh parse"),
            code
        );
    }
}
