//! End-to-end messages carried inside the Noise session (ADR-0021).
//!
//! Relay mode's E2E channel (phone/desktop ⇄ bridge) carries the existing
//! [`ChannelInput`]/[`ChannelOutput`] session protocol unchanged, wrapped in
//! a thin request/response envelope so a client can also ask the bridge for
//! things that have no analogue in direct mode's in-process call — listing
//! sessions, attaching to one. This module is that wrapper: [`ClientMessage`]
//! and [`BridgeMessage`] are the plaintext payload that rides inside a Noise
//! transport message, which itself rides inside the outer routing
//! [`Envelope`](crate::Envelope) the relay parses. Like the rest of this
//! crate, encoding is plain serde JSON — the relay never sees it (it is
//! opaque ciphertext at that layer), so there is no hot-path reason to
//! hand-roll a binary format here the way [`Envelope`](crate::Envelope) does.
//!
//! Externally tagged, snake_case, `#[non_exhaustive]`: growing any variant
//! set is a breaking wire change gated on [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION),
//! same convention as [`ChannelInput`]/[`ChannelOutput`].

use serde::{Deserialize, Serialize};

use crate::{ChannelInput, ChannelOutput, ProjectId, PushRegistration, SessionId, SessionMeta};

/// Client → bridge, inside Noise.
///
/// `id` on [`Request`](Self::Request) is a client-chosen correlation token
/// the bridge echoes back on the matching
/// [`BridgeMessage::Response`] — request/response pairs may interleave with
/// each other and with the unsolicited [`Input`](Self::Input)/
/// [`BridgeMessage::Output`] stream on the same Noise session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientMessage {
    /// First message on a fresh Noise session; carries
    /// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) so either side can fail
    /// closed on a mismatch before exchanging anything else.
    Hello { protocol_version: u32 },
    /// A correlated call — see [`RemoteOp`] for what a bridge supports.
    Request { id: u32, op: RemoteOp },
    /// Forwards to the attached channel unchanged (see module docs);
    /// meaningless before a successful `Request { op: RemoteOp::Attach }`.
    Input(ChannelInput),
}

/// The operation requested by a [`ClientMessage::Request`].
///
/// Has no analogue in direct mode's in-process `SessionSource` call — those
/// are ordinary async method calls; a remote peer needs them addressable on
/// the wire. `Attach`'s ids are the crate's validated [`ProjectId`]/
/// [`SessionId`] types (not raw strings), so a forged id fails
/// deserialization here exactly as it does everywhere else in the crate —
/// there is no separate trust boundary to defend on this path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RemoteOp {
    /// Discover sessions across the bridge's configured hosts.
    List,
    /// Attach to one session; success opens the [`ChannelInput`]/
    /// [`ChannelOutput`] stream that rides the same connection afterward.
    Attach {
        project_id: ProjectId,
        session_id: SessionId,
    },
    /// List the bridge's paired devices (ADR-0021 D6 remote revocation).
    ListDevices,
    /// Revoke a device from the bridge's roster by id (self-revoke = unpair).
    RevokeDevice { device_id: crate::DeviceId },
    /// Register (or, with `None`, clear) this device's push-wake endpoint
    /// (ADR-0023). Bridge-asserted: the requesting device tells its own
    /// bridge, which persists the registration in the device's roster entry
    /// and forwards it to the relay in every subsequent
    /// [`crate::RelayControl::AssertDevices`] via
    /// [`crate::AssertedDevice::push`].
    RegisterPushEndpoint {
        registration: Option<PushRegistration>,
    },
}

/// Bridge → client, inside Noise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BridgeMessage {
    /// Answers a [`ClientMessage::Hello`]; same version-gate contract.
    Hello { protocol_version: u32 },
    /// Answers the [`ClientMessage::Request`] with matching `id`.
    Response { id: u32, result: RemoteResult },
    /// Forwards from the attached channel unchanged (see module docs).
    Output(ChannelOutput),
    /// The attached channel's other end is gone — the remote-mode analogue
    /// of `SourceError::ChannelClosed`, but delivered as an unsolicited
    /// stream event (like [`Output`](Self::Output)) rather than a
    /// `Response`, since nothing local requested the close.
    ChannelClosed,
}

/// The result of a [`RemoteOp`], carried in [`BridgeMessage::Response`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RemoteResult {
    /// Answers [`RemoteOp::List`].
    Sessions(Vec<SessionMeta>),
    /// Answers a successful [`RemoteOp::Attach`]; the channel stream follows.
    Attached,
    /// Answers [`RemoteOp::ListDevices`].
    Devices(Vec<DeviceInfo>),
    /// Answers a successful [`RemoteOp::RevokeDevice`].
    Revoked,
    /// Answers a successful [`RemoteOp::RegisterPushEndpoint`] (ADR-0023).
    PushEndpointSet,
    /// Answers a failed request of either kind.
    Error(WireError),
}

/// One paired device, as returned by [`RemoteOp::ListDevices`]. Display-safe:
/// `name`/`fingerprint` are the sender's already-sanitized values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: crate::DeviceId,
    pub name: String,
    /// `XXXX-XXXX-XXXX` fingerprint of the device's static key (ADR-0021 D5).
    pub fingerprint: String,
    pub enrolled_at: Option<u64>,
    pub last_connected_at: Option<u64>,
    /// True when this entry is the requesting device itself.
    pub is_self: bool,
}

/// Stable protocol error type (spec review C15) — mirrors
/// `SourceError` (`remora-core`) variants TODAY but evolves by protocol
/// rules (append-only, gated on [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION))
/// rather than tracking `SourceError` 1:1 forever. `remora-protocol` cannot
/// depend on `remora-core`'s error type directly — the dependency runs the
/// other way — so this is a hand-mirrored wire projection: variants that
/// carry structured, typed context in `SourceError` (session identity) keep
/// that shape here; variants whose `SourceError` payload is backend-specific
/// or already-formatted (`DirtyReason`, `PlanError`, transport detail)
/// instead carry a plain `message: String`. All fields are display-safe: the
/// sender is responsible for producing a value already safe to render (the
/// same escaping discipline `SourceError::Transport` and `InvalidIdError`
/// apply on the sending side), since this type does no escaping of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WireError {
    /// Mirrors `SourceError::SessionExists`.
    SessionExists {
        project_id: ProjectId,
        session_id: SessionId,
    },
    /// Mirrors `SourceError::SessionNotFound`.
    SessionNotFound {
        project_id: ProjectId,
        session_id: SessionId,
    },
    /// Mirrors `SourceError::WorkspaceDirty`; `message` is the sender's
    /// already-rendered `Display` text (session identity + reason).
    WorkspaceDirty { message: String },
    /// Mirrors `SourceError::Plan`; `message` is the sender's
    /// already-rendered `Display` text.
    Plan { message: String },
    /// Mirrors `SourceError::ChannelClosed`.
    ChannelClosed,
    /// Mirrors `SourceError::Transport`; `message` is already escaped and
    /// bounded by the sender (`SourceError::Transport`'s `Display` impl does
    /// this today), since this type applies no further sanitization.
    Transport { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionState, SessionStatus};

    /// Mirrors `session::tests::meta` so `response_sessions_wire_format`'s
    /// expected JSON matches `session_meta_wire_format`'s fixture shape.
    fn meta() -> SessionMeta {
        SessionMeta {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("fix-login").expect("valid slug"),
            state: SessionState::Live,
            agent: Some("claude".to_string()),
            created_at: Some(1_765_500_000),
            workspace_path: Some("/home/dev/.remora/worktrees/api/fix-login".to_string()),
            workspace: None,
            branch: None,
        }
    }

    #[test]
    fn client_hello_wire_format() {
        let msg = ClientMessage::Hello {
            protocol_version: 2,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"hello":{"protocol_version":2}}"#);
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn request_attach_wire_format() {
        let msg = ClientMessage::Request {
            id: 7,
            op: RemoteOp::Attach {
                project_id: ProjectId::new("api").expect("valid slug"),
                session_id: SessionId::new("fix-login").expect("valid slug"),
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"request":{"id":7,"op":{"attach":{"project_id":"api","session_id":"fix-login"}}}}"#
        );
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn input_bytes_rides_unchanged() {
        let msg = ClientMessage::Input(ChannelInput::Bytes(b"hi".to_vec()));
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"input":{"bytes":[104,105]}}"#);
        let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn bridge_hello_wire_format() {
        let msg = BridgeMessage::Hello {
            protocol_version: 2,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"hello":{"protocol_version":2}}"#);
        let back: BridgeMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn response_sessions_wire_format() {
        let msg = BridgeMessage::Response {
            id: 3,
            result: RemoteResult::Sessions(vec![meta()]),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"response":{"id":3,"result":{"sessions":[{"project_id":"api","session_id":"fix-login","state":"live","agent":"claude","created_at":1765500000,"workspace_path":"/home/dev/.remora/worktrees/api/fix-login","workspace":null,"branch":null}]}}}"#
        );
        let back: BridgeMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn response_error_session_not_found_wire_format() {
        let msg = BridgeMessage::Response {
            id: 4,
            result: RemoteResult::Error(WireError::SessionNotFound {
                project_id: ProjectId::new("api").expect("valid slug"),
                session_id: SessionId::new("gone").expect("valid slug"),
            }),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"response":{"id":4,"result":{"error":{"session_not_found":{"project_id":"api","session_id":"gone"}}}}}"#
        );
        let back: BridgeMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn output_status_change_rides_unchanged() {
        let msg = BridgeMessage::Output(ChannelOutput::StatusChange(SessionStatus::Awaiting));
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"output":{"status_change":"awaiting"}}"#);
        let back: BridgeMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn channel_closed_wire_format() {
        let msg = BridgeMessage::ChannelClosed;
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#""channel_closed""#);
        let back: BridgeMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn wire_error_round_trips_all_variants() {
        let variants = [
            WireError::SessionExists {
                project_id: ProjectId::new("api").expect("valid slug"),
                session_id: SessionId::new("fix-login").expect("valid slug"),
            },
            WireError::SessionNotFound {
                project_id: ProjectId::new("api").expect("valid slug"),
                session_id: SessionId::new("gone").expect("valid slug"),
            },
            WireError::WorkspaceDirty {
                message: "session `api_fix-login` has uncommitted changes that would be lost"
                    .to_string(),
            },
            WireError::Plan {
                message: "spawn could not be planned: unknown project `ghost`".to_string(),
            },
            WireError::ChannelClosed,
            WireError::Transport {
                message: "transport error: ssh exited".to_string(),
            },
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).expect("serialize");
            let back: WireError = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn list_devices_op_wire_format() {
        let msg = ClientMessage::Request {
            id: 5,
            op: RemoteOp::ListDevices,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"request":{"id":5,"op":"list_devices"}}"#);
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).expect("deserialize"),
            msg
        );
    }

    #[test]
    fn revoke_device_op_wire_format() {
        use crate::DeviceId;
        let msg = ClientMessage::Request {
            id: 6,
            op: RemoteOp::RevokeDevice {
                device_id: DeviceId([0xaa; 32]),
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).expect("deserialize"),
            msg
        );
    }

    #[test]
    fn devices_result_wire_format() {
        use crate::DeviceId;
        let msg = BridgeMessage::Response {
            id: 5,
            result: RemoteResult::Devices(vec![DeviceInfo {
                device_id: DeviceId([0xaa; 32]),
                name: "iPhone".to_string(),
                fingerprint: "ABCD-1234-EF56".to_string(),
                enrolled_at: Some(1_765_500_000),
                last_connected_at: None,
                is_self: true,
            }]),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            serde_json::from_str::<BridgeMessage>(&json).expect("deserialize"),
            msg
        );
    }

    #[test]
    fn revoked_result_round_trips() {
        let msg = BridgeMessage::Response {
            id: 6,
            result: RemoteResult::Revoked,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            serde_json::from_str::<BridgeMessage>(&json).expect("deserialize"),
            msg
        );
    }

    #[test]
    fn register_push_endpoint_round_trips() {
        let msg = ClientMessage::Request {
            id: 8,
            op: RemoteOp::RegisterPushEndpoint {
                registration: Some(PushRegistration::UnifiedPush {
                    endpoint: "https://ntfy.sh/topic".to_string(),
                }),
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            json,
            r#"{"request":{"id":8,"op":{"register_push_endpoint":{"registration":{"unified_push":{"endpoint":"https://ntfy.sh/topic"}}}}}}"#
        );
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).expect("deserialize"),
            msg
        );

        // `None` clears an existing registration.
        let clear = ClientMessage::Request {
            id: 9,
            op: RemoteOp::RegisterPushEndpoint { registration: None },
        };
        let json = serde_json::to_string(&clear).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ClientMessage>(&json).expect("deserialize"),
            clear
        );
    }

    #[test]
    fn push_endpoint_set_round_trips() {
        let msg = BridgeMessage::Response {
            id: 8,
            result: RemoteResult::PushEndpointSet,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(
            serde_json::from_str::<BridgeMessage>(&json).expect("deserialize"),
            msg
        );
    }

    #[test]
    fn attach_rejects_forged_ids() {
        // Id validation (ADR-0004) already guards ProjectId/SessionId
        // construction and deserialization everywhere in the crate; this
        // proves it still holds when the ids are reached through
        // `ClientMessage` -> `RemoteOp::Attach`, not just directly.
        let json = r#"{"request":{"id":1,"op":{"attach":{"project_id":"api; rm -rf /","session_id":"fix-login"}}}}"#;
        assert!(serde_json::from_str::<ClientMessage>(json).is_err());
    }
}
