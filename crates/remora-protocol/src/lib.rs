//! Wire protocol for Remora sessions.
//!
//! Every Remora client talks to a session source through the messages defined
//! here, whether the source is in-process (direct mode) or reached over a
//! WebSocket (relay mode). Keeping this crate dependency-light is deliberate:
//! it is the contract third-party clients build against.

mod channel;
mod id;
mod session;

pub use channel::{ChannelInput, ChannelOutput, TerminalSize};
pub use id::{AgentId, InvalidIdError, ProjectId, SessionId};
pub use session::{SessionMeta, SessionState, SpawnSpec};
