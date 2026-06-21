//! Session model and the `SessionSource` transport seam.
//!
//! The desktop app (and later the relay) depend on this crate. UI code never
//! talks to ssh/kubectl directly — it goes through a `SessionSource`
//! implementation, which is what makes the relay an optional drop-in.

pub mod config;
pub mod discovery;
pub mod naming;
pub mod spawn_plan;
pub mod transport;

mod channel;
mod error;
pub mod fake;
mod source;

pub use channel::{SessionChannel, CHANNEL_CAPACITY};
pub use error::{DirtyReason, SourceError};
pub use fake::FakeSessionSource;
pub use source::SessionSource;
pub use spawn_plan::{plan_spawn, PlanError, SpawnPlan};
pub use transport::{KubectlSource, SshSource};

pub use remora_protocol::{
    AgentId, ChannelInput, ChannelOutput, InvalidIdError, InvalidTerminalSizeError, ProjectId,
    SessionId, SessionMeta, SessionState, SpawnSpec, TerminalSize, PROTOCOL_VERSION,
};
