//! Errors crossing the `SessionSource` seam.

use remora_protocol::{ProjectId, SessionId};

/// Error returned by [`SessionSource`](crate::SessionSource) operations and
/// [`SessionChannel`](crate::SessionChannel) sends.
///
/// Small by design; real transport implementations extend it.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// Spawn fails closed when the session already exists — tmux name
    /// uniqueness is the lock (ADR-0004).
    #[error("session `{project_id}_{session_id}` already exists")]
    SessionExists {
        project_id: ProjectId,
        session_id: SessionId,
    },
    /// The attach target does not exist or is not live.
    #[error("session `{project_id}_{session_id}` not found")]
    SessionNotFound {
        project_id: ProjectId,
        session_id: SessionId,
    },
    /// The channel's other end is gone. Channel death is only observable
    /// locally (spine spike); there is no remote "detached" state.
    #[error("channel closed")]
    ChannelClosed,
    /// Backend-specific failure, rendered for display. A string rather than
    /// a nested error type so the seam stays backend-agnostic.
    #[error("transport error: {0}")]
    Transport(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_name_the_session() {
        let err = SourceError::SessionExists {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("fix-login").expect("valid slug"),
        };
        assert_eq!(err.to_string(), "session `api_fix-login` already exists");

        let err = SourceError::SessionNotFound {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("gone").expect("valid slug"),
        };
        assert_eq!(err.to_string(), "session `api_gone` not found");

        assert_eq!(SourceError::ChannelClosed.to_string(), "channel closed");
        assert_eq!(
            SourceError::Transport("ssh exited".to_string()).to_string(),
            "transport error: ssh exited"
        );
        let _: &dyn std::error::Error = &SourceError::ChannelClosed;
    }
}
