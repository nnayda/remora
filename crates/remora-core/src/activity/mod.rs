//! Core-side agent-activity detection (ADR-0013): a pure, clock-free state
//! machine that turns the PTY byte stream into `SessionStatus` transitions and
//! sanitized previews. The settle clock lives in the bridge thread that drives
//! it (`transport::pty_process`), not here.

mod marker;
mod sanitize;

pub use marker::{MarkerHit, MarkerScanner};
pub use sanitize::{sanitize, SanitizedText};
