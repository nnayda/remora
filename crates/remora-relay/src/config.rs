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

pub use crate::push::PushConfig;

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
    /// Opt-in push-wake policy (ADR-0023). An absent `[push]` section = every
    /// field its default = push delivery disabled.
    #[serde(default)]
    pub push: PushConfig,
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
#[derive(Clone, PartialEq, Deserialize)]
pub struct BridgeEntry {
    /// The registration bearer token — redacted from the manual [`Debug`]
    /// impl so a `{:?}` of the (reload-logged) config never carries it (#278).
    pub token: String,
    pub device_id: DeviceId,
}

impl std::fmt::Debug for BridgeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeEntry")
            .field("token", &"[redacted]")
            .field("device_id", &self.device_id)
            .finish()
    }
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

    /// Parses `contents` and merges it into this (running) config for a
    /// SIGHUP reload (#276): [`RelayConfig::from_toml_str`] then
    /// [`RelayConfig::merge_reload`]. A parse failure returns the error and
    /// touches nothing — the caller keeps the running config unchanged.
    pub fn reload_from_str(&self, contents: &str) -> Result<ReloadOutcome, RelayConfigError> {
        Ok(self.merge_reload(RelayConfig::from_toml_str(contents)?))
    }

    /// Merges a freshly parsed config into this (running) one for a SIGHUP
    /// reload (#276). Only the `bridges` table is hot-appliable; every other
    /// field keeps its running value in [`ReloadOutcome::effective`] and, if
    /// it changed in the file, is named in
    /// [`ReloadOutcome::restart_required`] so the operator learns a restart
    /// is needed rather than believing the change took effect.
    ///
    /// Because `effective` keeps the *running* values for non-hot fields, a
    /// still-pending change (e.g. a new `listen` address) is re-flagged on
    /// every subsequent reload until the process restarts.
    pub fn merge_reload(&self, new: RelayConfig) -> ReloadOutcome {
        let mut restart_required = Vec::new();
        if new.listen != self.listen {
            restart_required.push("listen");
        }
        if new.buffer_bytes != self.buffer_bytes {
            restart_required.push("buffer_bytes");
        }
        if new.handshake_timeout_secs != self.handshake_timeout_secs {
            restart_required.push("handshake_timeout_secs");
        }
        if new.max_connections != self.max_connections {
            restart_required.push("max_connections");
        }
        if new.audit != self.audit {
            restart_required.push("audit");
        }
        if new.push != self.push {
            restart_required.push("push");
        }
        let bridges_changed = new.bridges != self.bridges;
        let effective = RelayConfig {
            bridges: new.bridges,
            ..self.clone()
        };
        ReloadOutcome {
            effective,
            bridges_changed,
            restart_required,
        }
    }
}

/// Result of merging a reloaded config into the running one
/// ([`RelayConfig::merge_reload`], SIGHUP reload #276).
#[derive(Debug, PartialEq)]
pub struct ReloadOutcome {
    /// The new effective config: the running config with the hot-appliable
    /// `bridges` table taken from the reloaded file. Every non-hot field
    /// keeps its running value, so this is what the caller should treat as
    /// "running" from now on.
    pub effective: RelayConfig,
    /// Whether the `bridges` table actually changed (the caller only needs to
    /// swap the router's live table when it did).
    pub bridges_changed: bool,
    /// Names of fields that changed in the file but are not hot-reloadable —
    /// most notably `listen`: a listen-address change requires a restart, the
    /// old listener stays bound. The caller logs one warning per entry.
    pub restart_required: Vec<&'static str>,
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
        // No [push] section: every push field is its default (disabled).
        assert_eq!(config.push, PushConfig::default());
        assert!(!config.push.enabled);
    }

    #[test]
    fn parses_full_push_section() {
        let toml = r#"
            listen = "127.0.0.1:9440"

            [push]
            enabled = true
            allow_http = true
            allow_private_endpoints = true
            device_cooldown_secs = 5
            per_bridge_per_minute = 100
            max_in_flight = 8
        "#;
        let config = RelayConfig::from_toml_str(toml).expect("valid config parses");
        assert_eq!(
            config.push,
            PushConfig {
                enabled: true,
                allow_http: true,
                allow_private_endpoints: true,
                device_cooldown_secs: 5,
                per_bridge_per_minute: 100,
                max_in_flight: 8,
            }
        );
    }

    #[test]
    fn partial_push_section_fills_defaults() {
        // Only `enabled` is set; every other field falls back to its default.
        let toml = r#"
            listen = "127.0.0.1:9440"

            [push]
            enabled = true
        "#;
        let config = RelayConfig::from_toml_str(toml).expect("valid config parses");
        assert!(config.push.enabled, "the one set field is honoured");
        assert!(!config.push.allow_http);
        assert!(!config.push.allow_private_endpoints);
        assert_eq!(config.push.device_cooldown_secs, 30);
        assert_eq!(config.push.per_bridge_per_minute, 10);
        assert_eq!(config.push.max_in_flight, 32);
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
        assert_eq!(config.push, PushConfig::default());
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

    /// A minimal running config with one bridge entry, for the reload tests.
    fn running_config() -> RelayConfig {
        let bridge_id = "11".repeat(32);
        RelayConfig::from_toml_str(&format!(
            r#"
            listen = "127.0.0.1:9440"

            [[bridges]]
            token = "old-token"
            device_id = "{bridge_id}"
            "#
        ))
        .expect("valid running config")
    }

    #[test]
    fn reload_swaps_bridges_table() {
        let running = running_config();
        let bridge_id = "11".repeat(32);
        let outcome = running
            .reload_from_str(&format!(
                r#"
                listen = "127.0.0.1:9440"

                [[bridges]]
                token = "rotated-token"
                device_id = "{bridge_id}"
                "#
            ))
            .expect("valid reload parses");

        assert!(outcome.bridges_changed);
        assert!(
            outcome.restart_required.is_empty(),
            "a pure token rotation needs no restart"
        );
        assert_eq!(outcome.effective.bridges.len(), 1);
        assert_eq!(outcome.effective.bridges[0].token, "rotated-token");
        // Everything else keeps its running value.
        assert_eq!(outcome.effective.listen, running.listen);
        assert_eq!(outcome.effective.buffer_bytes, running.buffer_bytes);
    }

    #[test]
    fn reload_with_unchanged_bridges_reports_no_change() {
        let running = running_config();
        let bridge_id = "11".repeat(32);
        let outcome = running
            .reload_from_str(&format!(
                r#"
                listen = "127.0.0.1:9440"

                [[bridges]]
                token = "old-token"
                device_id = "{bridge_id}"
                "#
            ))
            .expect("valid reload parses");
        assert!(!outcome.bridges_changed);
        assert!(outcome.restart_required.is_empty());
        assert_eq!(outcome.effective, running);
    }

    #[test]
    fn reload_rejects_invalid_toml_without_touching_running_config() {
        let running = running_config();
        let err = running
            .reload_from_str("listen = not-even-toml {{{")
            .expect_err("malformed reload rejected");
        assert!(err.to_string().contains("invalid relay config"));
        // The running config is untouched by construction (`reload_from_str`
        // borrows immutably); assert it still parses hellos as before.
        assert_eq!(running.bridges[0].token, "old-token");
    }

    #[test]
    fn reload_flags_listen_change_and_keeps_old_listener_address() {
        let running = running_config();
        let outcome = running
            .reload_from_str(r#"listen = "0.0.0.0:9999""#)
            .expect("valid reload parses");
        assert!(outcome.restart_required.contains(&"listen"));
        assert_eq!(
            outcome.effective.listen, "127.0.0.1:9440",
            "the effective config keeps the address actually bound"
        );
        // The bridges table in the file (empty) still applies.
        assert!(outcome.bridges_changed);
        assert!(outcome.effective.bridges.is_empty());
    }

    #[test]
    fn reload_flags_every_non_hot_field() {
        let running = running_config();
        let outcome = running
            .reload_from_str(
                r#"
                listen = "0.0.0.0:9999"
                buffer_bytes = 42
                handshake_timeout_secs = 99
                max_connections = 7

                [audit]
                path = "/tmp/audit.log"

                [push]
                enabled = true
                "#,
            )
            .expect("valid reload parses");
        assert_eq!(
            outcome.restart_required,
            vec![
                "listen",
                "buffer_bytes",
                "handshake_timeout_secs",
                "max_connections",
                "audit",
                "push",
            ]
        );
        // None of them are half-applied: the effective config keeps every
        // running value except the (hot) bridges table.
        assert_eq!(
            outcome.effective,
            RelayConfig {
                bridges: Vec::new(),
                ..running
            }
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

    #[test]
    fn bridge_entry_debug_redacts_the_token() {
        let entry = BridgeEntry {
            token: "bridge-SECRET-token".to_string(),
            device_id: DeviceId([0x11; 32]),
        };
        let dbg = format!("{entry:?}");
        assert!(!dbg.contains("bridge-SECRET-token"), "leaked: {dbg}");
        assert!(dbg.contains("[redacted]"), "no redaction marker: {dbg}");
        assert!(
            dbg.contains("device_id"),
            "device_id should stay visible: {dbg}"
        );
    }
}
