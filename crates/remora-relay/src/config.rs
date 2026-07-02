//! Relay configuration (ADR-0021's "Bridge→relay registration" and
//! "Roster authority is per-bridge" sections): a self-hosted relay's TOML
//! file lists the bridge-registration tokens it admits to routing. Absent
//! `bridges` means closed registration for bridges — the documented
//! default. Device admission is no longer a static config table (ADR-0021
//! D4): it moves to bridge-asserted soft state, driven at runtime rather
//! than read from this file.

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
    /// Per-connection buffer cap in bytes, enforcing the bounded-buffer
    /// requirement from ADR-0021 (relay load-shedding is
    /// connection-granular, not frame-granular).
    #[serde(default = "default_buffer_bytes")]
    pub buffer_bytes: usize,
    /// Deadline, in seconds, for the pre-authentication handshake — the
    /// WebSocket upgrade plus the first (hello) frame. A connection that has
    /// not authenticated within this window is dropped, defeating slowloris
    /// clients that open a socket and never send a hello (holding a task, an
    /// FD, and a read buffer indefinitely). Applies only before a successful
    /// `Router::hello`; authenticated connections are never subject to it.
    #[serde(default = "default_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,
    /// Global cap on concurrent connections. The accept loop holds a semaphore
    /// with this many permits; a newly accepted socket that cannot take a
    /// permit is dropped immediately (before the WebSocket upgrade), bounding
    /// the relay's total FDs and tasks. This is a **global** cap only —
    /// per-IP/per-sender fairness is deferred to the rate-limiting follow-up
    /// and is deliberately out of scope here.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Opt-in audit log config. `None` = audit mode disabled (the default).
    #[serde(default)]
    pub audit: Option<AuditConfig>,
}

fn default_buffer_bytes() -> usize {
    1_048_576
}

fn default_handshake_timeout_secs() -> u64 {
    10
}

fn default_max_connections() -> usize {
    1024
}

/// One registered bridge: the token it presents in its `RelayHello` and the
/// [`DeviceId`] that token is scoped to. A token admits exactly this
/// identity — nothing else (spec D5).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BridgeEntry {
    pub token: String,
    pub device_id: DeviceId,
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
/// [`BridgeEntry`] token it claims to be.
///
/// Equal-length inputs are compared in constant time. Differing lengths
/// short-circuit before any constant-time comparison runs, so **token
/// length is not secret** — only its content is. That is an accepted
/// leak: relay tokens are not fixed-width secrets the way key material is,
/// and hiding length would require padding every comparison to a
/// worst-case bound for no threat-model benefit here.
///
/// Called by [`crate::Router`]'s hello authentication (`router.rs`) to check a
/// connecting peer's token against the configured entry it claims to be.
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
        let toml = format!(
            r#"
            listen = "127.0.0.1:9440"

            [[bridges]]
            token = "bridge-token"
            device_id = "{bridge_id}"

            [audit]
            path = "/var/log/remora-relay/audit.log"
            "#
        );

        let config = RelayConfig::from_toml_str(&toml).expect("valid config parses");

        assert_eq!(config.listen, "127.0.0.1:9440");
        assert_eq!(config.bridges.len(), 1);
        assert_eq!(config.bridges[0].token, "bridge-token");
        let bridge_device_id: DeviceId = bridge_id.parse().expect("valid hex device id");
        assert_eq!(config.bridges[0].device_id, bridge_device_id);
        assert_eq!(config.buffer_bytes, 1_048_576);
        assert_eq!(config.handshake_timeout_secs, 10);
        assert_eq!(config.max_connections, 1024);
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
        assert_eq!(config.buffer_bytes, 1_048_576);
        assert_eq!(config.handshake_timeout_secs, 10);
        assert_eq!(config.max_connections, 1024);
        assert_eq!(config.audit, None);
    }

    #[test]
    fn devices_table_is_ignored_not_an_error() {
        // A stale [[devices]] block from an old config must not break parsing
        // (serde ignores unknown fields by default), but the field is gone.
        let toml = r#"
            listen = "127.0.0.1:9440"
            [[devices]]
            token = "x"
            device_id = "2222222222222222222222222222222222222222222222222222222222222222"
            bridge_id = "1111111111111111111111111111111111111111111111111111111111111111"
        "#;
        let config = RelayConfig::from_toml_str(toml).expect("parses, devices ignored");
        assert_eq!(config.listen, "127.0.0.1:9440");
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
