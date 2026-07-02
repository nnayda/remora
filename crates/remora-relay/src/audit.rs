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
use std::os::unix::fs::OpenOptionsExt;
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
                Some(Mutex::new(file))
            }
        };
        Ok(std::sync::Arc::new(AuditSink { file }))
    }

    /// Appends one record as a JSON line. A no-op when audit is disabled. A
    /// serialization or write error is dropped rather than propagated: audit is
    /// observability, and a failed record must never take down a live routing
    /// path.
    pub fn record(&self, record: &AuditRecord) {
        let Some(file) = &self.file else {
            return;
        };
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        if let Ok(mut guard) = file.lock() {
            let _ = guard.write_all(line.as_bytes());
        }
    }
}
