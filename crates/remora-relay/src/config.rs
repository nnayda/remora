//! Relay configuration (ADR-0021's "Bridge→relay registration" and
//! "Roster authority is per-bridge" sections): a self-hosted relay's TOML
//! file lists the bridge-registration tokens and per-(device, bridge)
//! device tokens it admits to routing. Absent `bridges`/`devices` means
//! closed registration — the documented default.

use std::path::PathBuf;

use remora_protocol::DeviceId;
use serde::Deserialize;
use subtle::ConstantTimeEq;

/// Top-level relay config, deserialized from TOML.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RelayConfig {
    /// Address the relay's WebSocket server binds to, e.g. `"127.0.0.1:9440"`.
    pub listen: String,
    /// Bridge registration tokens admitted to routing. Empty = closed
    /// registration: no bridge can register.
    #[serde(default)]
    pub bridges: Vec<BridgeEntry>,
    /// Per-(device, bridge) device tokens admitted to routing.
    #[serde(default)]
    pub devices: Vec<DeviceEntry>,
    /// Per-connection buffer cap in bytes, enforcing the bounded-buffer
    /// requirement from ADR-0021 (relay load-shedding is
    /// connection-granular, not frame-granular).
    #[serde(default = "default_buffer_bytes")]
    pub buffer_bytes: usize,
    /// Opt-in audit log config. `None` = audit mode disabled (the default).
    #[serde(default)]
    pub audit: Option<AuditConfig>,
}

fn default_buffer_bytes() -> usize {
    1_048_576
}

/// One registered bridge: the token it presents in its `RelayHello` and the
/// [`DeviceId`] that token is scoped to. A token admits exactly this
/// identity — nothing else (spec D5).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BridgeEntry {
    pub token: String,
    pub device_id: DeviceId,
}

/// One registered device: the token it presents in its `RelayHello`, the
/// device's own [`DeviceId`], and the bridge it is scoped to route through.
/// A device token is valid only for this exact (device, bridge) pair.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceEntry {
    pub token: String,
    pub device_id: DeviceId,
    pub bridge_id: DeviceId,
}

/// Opt-in audit log config (ADR-0021's "observability... catchable"
/// section). Recording exactly the fields the relay observed is a
/// regression guard against accidental leakage, not an audit of a
/// malicious operator.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AuditConfig {
    pub path: PathBuf,
}

/// Error returned when [`RelayConfig::from_toml_str`] rejects a config.
#[derive(Debug, thiserror::Error)]
pub enum RelayConfigError {
    /// The TOML was malformed, missing a required field, or contained a
    /// value that failed to deserialize (including a malformed
    /// [`DeviceId`] hex string).
    #[error("invalid relay config: {0}")]
    Parse(#[from] toml::de::Error),
}

impl RelayConfig {
    /// Parses a [`RelayConfig`] from a TOML document.
    pub fn from_toml_str(s: &str) -> Result<RelayConfig, RelayConfigError> {
        Ok(toml::from_str(s)?)
    }
}

/// Constant-time token comparison (`subtle::ConstantTimeEq` over bytes),
/// used to check a `RelayHello`'s token against the configured
/// [`BridgeEntry`]/[`DeviceEntry`] token it claims to be.
///
/// Equal-length inputs are compared in constant time. Differing lengths
/// short-circuit before any constant-time comparison runs, so **token
/// length is not secret** — only its content is. That is an accepted
/// leak: relay tokens are not fixed-width secrets the way key material is,
/// and hiding length would require padding every comparison to a
/// worst-case bound for no threat-model benefit here.
///
/// Not yet called outside tests — the hello-frame authentication that wires
/// it in lands with the router (later slice of #231).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn token_matches(candidate: &str, expected: &str) -> bool {
    if candidate.len() != expected.len() {
        return false;
    }
    candidate.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let bridge_id = "11".repeat(32);
        let device_id = "22".repeat(32);
        let toml = format!(
            r#"
            listen = "127.0.0.1:9440"

            [[bridges]]
            token = "bridge-token"
            device_id = "{bridge_id}"

            [[devices]]
            token = "device-token"
            device_id = "{device_id}"
            bridge_id = "{bridge_id}"

            [audit]
            path = "/var/log/remora-relay/audit.log"
            "#
        );

        let config = RelayConfig::from_toml_str(&toml).expect("valid config parses");

        assert_eq!(config.listen, "127.0.0.1:9440");
        assert_eq!(config.bridges.len(), 1);
        assert_eq!(config.bridges[0].token, "bridge-token");
        let bridge_device_id: DeviceId = bridge_id.parse().expect("valid hex device id");
        let device_device_id: DeviceId = device_id.parse().expect("valid hex device id");
        assert_eq!(config.bridges[0].device_id, bridge_device_id);
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.devices[0].token, "device-token");
        assert_eq!(config.devices[0].device_id, device_device_id);
        assert_eq!(config.devices[0].bridge_id, bridge_device_id);
        assert_eq!(config.buffer_bytes, 1_048_576);
        assert_eq!(
            config.audit,
            Some(AuditConfig {
                path: "/var/log/remora-relay/audit.log".into(),
            })
        );
    }

    #[test]
    fn empty_config_is_closed() {
        let config = RelayConfig::from_toml_str(r#"listen = "127.0.0.1:9440""#)
            .expect("minimal config parses");

        assert_eq!(config.listen, "127.0.0.1:9440");
        assert!(
            config.bridges.is_empty(),
            "no bridges = closed registration"
        );
        assert!(config.devices.is_empty());
        assert_eq!(config.buffer_bytes, 1_048_576);
        assert_eq!(config.audit, None);
    }

    #[test]
    fn rejects_malformed_device_id() {
        let toml = r#"
            listen = "127.0.0.1:9440"

            [[bridges]]
            token = "bridge-token"
            device_id = "not-hex"
            "#;

        let err = RelayConfig::from_toml_str(toml).expect_err("malformed device id rejected");
        assert!(
            err.to_string().to_lowercase().contains("device")
                || err.to_string().to_lowercase().contains("hex")
                || err.to_string().to_lowercase().contains("invalid"),
            "error should mention the parse failure, got: {err}"
        );
    }

    #[test]
    fn token_matches_is_exact() {
        assert!(token_matches("secret-token", "secret-token"));
        assert!(!token_matches("secret-token", "different-token"));
        assert!(!token_matches("", "non-empty"));
        assert!(!token_matches("non-empty", ""));
        assert!(token_matches("", ""));
    }
}
