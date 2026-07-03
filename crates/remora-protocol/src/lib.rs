//! Wire protocol for Remora sessions.
//!
//! Every Remora client talks to a session source through the messages defined
//! here, whether the source is in-process (direct mode) or reached over a
//! WebSocket (relay mode). Keeping this crate dependency-light is deliberate:
//! it is the contract third-party clients build against.

mod channel;
mod envelope;
mod id;
mod pairing;
mod push;
mod remote;
mod session;

pub use channel::{ChannelInput, ChannelOutput, InvalidTerminalSizeError, TerminalSize};
pub use envelope::{
    AssertedDevice, DeviceId, Envelope, EnvelopeError, FrameType, HelloRole, InvalidDeviceIdError,
    RelayControl, RelayControlAck, RelayControlError, RelayHello, ENVELOPE_HEADER_LEN,
    ENVELOPE_VERSION, MAX_ENVELOPE_PAYLOAD,
};
pub use id::{AgentId, InvalidIdError, ProjectId, SessionId, MAX_ID_LEN};
pub use pairing::{
    PairingBridgeMsg, PairingClientMsg, PairingCode, PairingCodeError, PairingRejectReason,
    PAIRING_CODE_VERSION,
};
pub use push::{
    validate_push_endpoint, PushEndpointError, PushRegistration, MAX_PUSH_ENDPOINT_LEN,
};
pub use remote::{BridgeMessage, ClientMessage, DeviceInfo, RemoteOp, RemoteResult, WireError};
pub use session::{SessionMeta, SessionState, SessionStatus, SpawnSpec, WorkspaceMode};

/// Version of the wire format defined by this crate.
///
/// Externally tagged serde enums reject unknown variants, so growing any
/// message enum (or changing a representation) is a breaking change: bump
/// this constant and gate compatibility on it. The tmux naming and worktree
/// conventions of ADR-0004 version alongside it. Bumped 3 → 4 for
/// ADR-0023's push-wake slice: the new `RemoteOp::RegisterPushEndpoint`/
/// `RemoteResult::PushEndpointSet` ops, `AssertedDevice.push`, and the new
/// `PushRegistration` wire shape. `FrameType::PushTrigger` itself was already
/// reserved (byte `3`) before this bump; what changes here is that it now
/// carries real wake-request semantics instead of being an inert reservation.
pub const PROTOCOL_VERSION: u32 = 4;
