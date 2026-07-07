//! ctl.sock line protocol (spec D1): newline-delimited JSON, one request
//! line then response line(s). INTERNAL and UNSTABLE by design — the same
//! image ships both ends; this is local IPC, not remora-protocol wire.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum CtlRequest {
    Status,
    Devices,
    Fingerprint,
    Revoke { device_id: String },
    PairOpen { ttl_secs: u64 },
    PairConfirm { device_id: String },
    PairReject { device_id: String },
    PairCancel,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RelayStateDto {
    Starting,
    Connected { since: u64 },
    Reconnecting { since: u64, attempts: u32 },
    Rejected { at: u64, detail: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceDto {
    pub device_id: String,
    pub name: String,
    pub fingerprint: String,
    pub enrolled_at: Option<u64>,
    pub last_connected_at: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CtlResponse {
    Status {
        relay: RelayStateDto,
        device_id: String,
        fingerprint: String,
    },
    Devices {
        devices: Vec<DeviceDto>,
    },
    Fingerprint {
        device_id: String,
        fingerprint: String,
    },
    Ok,
    Error {
        message: String,
    },
    WindowOpened {
        code: String,
        expires_at: u64,
    },
    DeviceArrived {
        device_id: String,
        name: String,
        fingerprint: String,
    },
    PairResult {
        outcome: String, // "paired" | "rejected" | "expired"
        device_id: Option<String>,
        name: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_round_trip() {
        for req in [
            CtlRequest::Status,
            CtlRequest::Devices,
            CtlRequest::Fingerprint,
            CtlRequest::Revoke {
                device_id: "ab12".into(),
            },
            CtlRequest::PairOpen { ttl_secs: 300 },
            CtlRequest::PairConfirm {
                device_id: "ab12".into(),
            },
            CtlRequest::PairReject {
                device_id: "ab12".into(),
            },
            CtlRequest::PairCancel,
        ] {
            let line = serde_json::to_string(&req).expect("ser");
            assert!(!line.contains('\n'));
            let _back: CtlRequest = serde_json::from_str(&line).expect("de");
        }
    }

    #[test]
    fn unknown_cmd_is_a_decode_error() {
        assert!(serde_json::from_str::<CtlRequest>("{\"cmd\":\"rm_rf\"}").is_err());
    }
}
