//! Wire protocol for Remora sessions.
//!
//! Every Remora client talks to a session source through the messages defined
//! here, whether the source is in-process (direct mode) or reached over a
//! WebSocket (relay mode). Keeping this crate dependency-light is deliberate:
//! it is the contract third-party clients build against.

use serde::{Deserialize, Serialize};

/// Identifies one session on the sandbox: a workspace (git worktree, or the
/// project directory in shared mode) that is live while its named tmux
/// session exists and stopped when only the worktree survives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_through_json() {
        let id = SessionId("remora-feature-x".to_string());
        let json = serde_json::to_string(&id).expect("serialize");
        let back: SessionId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }
}
