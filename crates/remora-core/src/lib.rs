//! Session model and the `SessionSource` transport seam.
//!
//! The desktop app (and later the relay) depend on this crate. UI code never
//! talks to ssh/kubectl directly — it goes through a `SessionSource`
//! implementation, which is what makes the relay an optional drop-in.

mod error;

pub use error::SourceError;

pub use remora_protocol::{InvalidIdError, SessionId};
