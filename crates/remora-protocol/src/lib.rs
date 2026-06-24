//! Wire protocol for Remora sessions.
//!
//! Every Remora client talks to a session source through the messages defined
//! here, whether the source is in-process (direct mode) or reached over a
//! WebSocket (relay mode). Keeping this crate dependency-light is deliberate:
//! it is the contract third-party clients build against.

mod channel;
mod id;
mod session;

pub use channel::{ChannelInput, ChannelOutput, InvalidTerminalSizeError, TerminalSize};
pub use id::{AgentId, InvalidIdError, ProjectId, SessionId, MAX_ID_LEN};
pub use session::{SessionMeta, SessionState, SpawnSpec, WorkspaceMode};

/// Version of the wire format defined by this crate.
///
/// Externally tagged serde enums reject unknown variants, so growing any
/// message enum (or changing a representation) is a breaking change: bump
/// this constant and gate compatibility on it. The tmux naming and worktree
/// conventions of ADR-0004 version alongside it.
pub const PROTOCOL_VERSION: u32 = 0;
