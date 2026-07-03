//! Opt-in aggregate audit log (ADR-0021 "observability", spec D10).
//!
//! When [`crate::RelayConfig::audit`] is set, the relay appends one JSON Lines
//! record per connection *close* — never per frame. Each record captures only
//! what the relay legitimately observed to route the connection (its role,
//! device/routing ids, and coarse traffic counters); it never contains payload
//! bytes, because the relay is blind to them by construction. Recording exactly
//! these fields is a regression guard against accidental leakage, not an audit
//! of a malicious operator.
//!
//! Audit disabled ⇒ every [`AuditSink::record`] call is a no-op. The log file
//! is created `0600` (owner read/write only) so a shared host cannot read the
//! relay's connection metadata.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use remora_protocol::{DeviceId, HelloRole};
use serde::Serialize;

use crate::config::RelayConfig;

/// Why a connection was torn down. Serialized into the audit record's
/// `close_reason` field as snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The peer closed cleanly, or the socket reached EOF.
    Normal,
    /// The hello failed authentication (WebSocket close 4001).
    AuthFailure,
    /// A protocol violation: a malformed frame, an illegal frame type, or an
    /// adjacency violation (WebSocket close 4002).
    Protocol,
    /// The addressed destination was not registered (WebSocket close 4004).
    PeerGone,
    /// This connection's outbound buffer exceeded its byte budget and it was
    /// shed as a slow consumer (WebSocket close 4008).
    BufferOverflow,
    /// A newer connection registered under the same routing id and displaced
    /// this one (WebSocket close 4009).
    Replaced,
}

impl CloseReason {
    /// Stable snake_case name used in the audit log and close-frame reasons.
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::Normal => "normal",
            CloseReason::AuthFailure => "auth_failure",
            CloseReason::Protocol => "protocol",
            CloseReason::PeerGone => "peer_gone",
            CloseReason::BufferOverflow => "buffer_overflow",
            CloseReason::Replaced => "replaced",
        }
    }
}

/// A single connection-close audit record (spec D10). Serialized as one JSON
/// object per line.
#[derive(Debug, Serialize)]
pub struct AuditRecord {
    /// Wall-clock close time, seconds since the Unix epoch.
    pub ts_unix: u64,
    /// `"bridge"` or `"device"`, or `null` if the connection closed before a
    /// well-formed hello identified it.
    pub role: Option<&'static str>,
    /// The peer's claimed device id (hex), or `null` if unknown.
    pub device_id: Option<String>,
    /// The connection's routing id (hex), or `null` if unknown.
    pub routing_id: Option<String>,
    /// Inbound frames the relay read from this connection.
    pub frames_in: u64,
    /// Outbound frames the relay wrote to this connection.
    pub frames_out: u64,
    /// Inbound bytes the relay read from this connection.
    pub bytes_in: u64,
    /// Outbound bytes the relay wrote to this connection.
    pub bytes_out: u64,
    /// Connection lifetime in whole seconds.
    pub connected_secs: u64,
    /// Why the connection closed.
    pub close_reason: &'static str,
}

impl AuditRecord {
    /// Builds a record, stamping `ts_unix` from the current wall clock.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: Option<HelloRole>,
        device_id: Option<DeviceId>,
        routing_id: Option<DeviceId>,
        frames_in: u64,
        frames_out: u64,
        bytes_in: u64,
        bytes_out: u64,
        connected_secs: u64,
        close_reason: CloseReason,
    ) -> AuditRecord {
        AuditRecord {
            ts_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            role: role.map(role_str),
            device_id: device_id.map(|id| id.to_string()),
            routing_id: routing_id.map(|id| id.to_string()),
            frames_in,
            frames_out,
            bytes_in,
            bytes_out,
            connected_secs,
            close_reason: close_reason.as_str(),
        }
    }
}

fn role_str(role: HelloRole) -> &'static str {
    match role {
        HelloRole::Bridge => "bridge",
        HelloRole::Device => "device",
    }
}

/// An assert-time warning that a bridge asserted a device push endpoint that
/// failed syntax validation (ADR-0023). Unlike [`AuditRecord`] this is *not* a
/// connection-close event — it is emitted while the bridge stays live, because
/// a policy-invalid endpoint is stored-but-flagged (the `AssertDevices` still
/// ACKs) and only dropped at delivery time. Logging it here lets an operator
/// see the misconfiguration at assert time, not at the first missed wake. The
/// endpoint URL itself is deliberately **not** recorded (device-supplied,
/// potentially sensitive) — only that some endpoint for this device failed and
/// why (the validator's category).
#[derive(Debug, Serialize)]
pub struct PushEndpointWarning {
    /// Wall-clock time, seconds since the Unix epoch.
    pub ts_unix: u64,
    /// Discriminator so a JSONL consumer can tell this apart from an
    /// [`AuditRecord`] line. Always `"push_endpoint_invalid"`.
    pub event: &'static str,
    /// The asserting bridge's routing id (hex).
    pub bridge_id: String,
    /// The device the invalid endpoint was asserted for (hex).
    pub device_id: String,
    /// The validation-failure category (from `PushEndpointError`'s display),
    /// never the endpoint URL.
    pub reason: String,
}

impl PushEndpointWarning {
    /// Builds a warning, stamping `ts_unix` from the current wall clock.
    pub fn new(bridge_id: DeviceId, device_id: DeviceId, reason: String) -> PushEndpointWarning {
        PushEndpointWarning {
            ts_unix: now_unix(),
            event: "push_endpoint_invalid",
            bridge_id: bridge_id.to_string(),
            device_id: device_id.to_string(),
            reason,
        }
    }
}

/// Current wall-clock time in whole seconds since the Unix epoch (folds a
/// pre-epoch clock to `0` rather than panicking).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSONL audit sink. Cheap to clone-share via the [`std::sync::Arc`]
/// [`AuditSink::new`] returns; disabled sinks record nothing.
pub struct AuditSink {
    /// `None` when audit mode is off. The file handle is guarded so concurrent
    /// connection teardowns append whole lines without interleaving.
    file: Option<Mutex<std::fs::File>>,
}

impl AuditSink {
    /// Opens the audit log named by `config.audit` (creating it `0600`,
    /// append-mode), or returns a disabled no-op sink when audit is off.
    pub fn new(config: &RelayConfig) -> std::io::Result<std::sync::Arc<AuditSink>> {
        let file = match &config.audit {
            None => None,
            Some(audit) => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(&audit.path)?;
                // `.mode()` only applies when O_CREAT actually creates the file;
                // a pre-existing looser audit log would keep its mode. Re-assert
                // 0600 so the "created 0600" guarantee holds for a file that
                // already existed (e.g. left world-readable by an older build).
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                Some(Mutex::new(file))
            }
        };
        Ok(std::sync::Arc::new(AuditSink { file }))
    }

    /// Appends one connection-close record as a JSON line. A no-op when audit
    /// is disabled. A serialization or write error is dropped rather than
    /// propagated: audit is observability, and a failed record must never take
    /// down a live routing path.
    pub fn record(&self, record: &AuditRecord) {
        self.append_line(record);
    }

    /// Appends one assert-time push-endpoint warning (ADR-0023) as a JSON line.
    /// Same no-op-when-disabled, never-propagate-errors contract as [`record`].
    pub fn record_push_warning(&self, warning: &PushEndpointWarning) {
        self.append_line(warning);
    }

    /// Serializes `value` to one JSON line and appends it under the file lock.
    /// Shared by every audit surface so JSONL framing (one object per line) and
    /// the swallow-errors policy live in exactly one place.
    fn append_line<T: Serialize>(&self, value: &T) {
        let Some(file) = &self.file else {
            return;
        };
        let Ok(mut line) = serde_json::to_string(value) else {
            return;
        };
        line.push('\n');
        if let Ok(mut guard) = file.lock() {
            let _ = guard.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("stat audit file")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn opening_tightens_a_preexisting_permissive_audit_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");

        // A pre-existing audit log left world/group-readable (e.g. by an older
        // build whose `.mode()` was a no-op on the already-created file).
        std::fs::write(&path, b"{}\n").expect("seed audit file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen mode");
        assert_eq!(file_mode(&path), 0o644);

        let config = RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges: Vec::new(),
            buffer_bytes: 1 << 20,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: Some(crate::config::AuditConfig { path: path.clone() }),
            push: crate::config::PushConfig::default(),
        };
        let sink = AuditSink::new(&config).expect("audit sink");

        // Opening the sink must re-assert 0600 on the pre-existing file.
        assert_eq!(
            file_mode(&path),
            0o600,
            "a pre-existing audit file must be tightened to 0600 on open"
        );

        // And it is still a working append sink (the seed line is preserved).
        sink.record(&AuditRecord::new(
            Some(HelloRole::Device),
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            CloseReason::Normal,
        ));
        let contents = std::fs::read_to_string(&path).expect("read audit file");
        assert!(contents.starts_with("{}\n"), "existing content preserved");
        assert!(
            contents.lines().count() >= 2,
            "record must append a new line"
        );
    }

    #[test]
    fn push_endpoint_warning_is_appended_as_a_json_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");
        let config = RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges: Vec::new(),
            buffer_bytes: 1 << 20,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: Some(crate::config::AuditConfig { path: path.clone() }),
            push: crate::config::PushConfig::default(),
        };
        let sink = AuditSink::new(&config).expect("audit sink");

        sink.record_push_warning(&PushEndpointWarning::new(
            DeviceId([0x11; 32]),
            DeviceId([0x22; 32]),
            "unsupported URL scheme: expected http or https".to_string(),
        ));

        let contents = std::fs::read_to_string(&path).expect("read audit file");
        let line = contents.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
        assert_eq!(value["event"], "push_endpoint_invalid");
        assert_eq!(value["bridge_id"], DeviceId([0x11; 32]).to_string());
        assert_eq!(value["device_id"], DeviceId([0x22; 32]).to_string());
        assert!(value["reason"].is_string());
        assert!(value["ts_unix"].is_u64());
        // The endpoint URL itself must never be recorded.
        assert!(value.get("endpoint").is_none(), "no endpoint URL in audit");
    }
}
