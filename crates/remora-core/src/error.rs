//! Errors crossing the `SessionSource` seam.

use crate::spawn_plan::PlanError;
use remora_protocol::{ProjectId, SessionId};

/// Cap on backend detail rendered into a [`SourceError::Transport`]
/// message, so one error cannot flood a log line.
const MAX_TRANSPORT_DETAIL_LEN: usize = 256;

/// Why a worktree is unsafe to remove without `force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyReason {
    /// Working tree has uncommitted changes.
    Uncommitted,
    /// HEAD has commits not reachable from any remote-tracking ref.
    NotOnRemote,
    /// Both of the above.
    Both,
}

impl std::fmt::Display for DirtyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DirtyReason::Uncommitted => "uncommitted changes",
            DirtyReason::NotOnRemote => "commits not on any remote",
            DirtyReason::Both => "uncommitted changes and commits not on any remote",
        };
        f.write_str(s)
    }
}

/// Escapes and bounds transport detail for display. Real transports fill
/// it from backend output (ssh/kubectl stderr), which carries
/// remote-influenced bytes — escape control characters inside `Display`
/// itself so no render path can leak terminal escapes (same rule as
/// `InvalidIdError` in `remora-protocol`).
fn escape_detail(detail: &str) -> String {
    let mut shown: String = detail
        .chars()
        .take(MAX_TRANSPORT_DETAIL_LEN)
        .flat_map(char::escape_default)
        .collect();
    if detail.chars().nth(MAX_TRANSPORT_DETAIL_LEN).is_some() {
        shown.push('…');
    }
    shown
}

/// Error returned by [`SessionSource`](crate::SessionSource) operations and
/// [`SessionChannel`](crate::SessionChannel) sends.
///
/// Small by design; grows variants in core as real transports need them
/// (backend-specific detail stays in [`Transport`](Self::Transport)) —
/// `#[non_exhaustive]` so downstream crates match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// `remove` refused: the worktree has uncommitted work or commits not on
    /// any remote, and `force` was not set. Nothing was changed.
    #[error("session `{project_id}_{session_id}` has {reason} that would be lost")]
    WorkspaceDirty {
        project_id: ProjectId,
        session_id: SessionId,
        reason: DirtyReason,
    },
    /// Spawn could not be planned from local config (unknown project or
    /// agent). Carries the typed [`PlanError`] so the offending id survives
    /// for display.
    #[error("spawn could not be planned: {0}")]
    Plan(#[from] PlanError),
    /// The channel's other end is gone. Channel death is only observable
    /// locally (spine spike); there is no remote "detached" state.
    #[error("channel closed")]
    ChannelClosed,
    /// Backend-specific failure, rendered for display. A string rather than
    /// a nested error type so the seam stays backend-agnostic. The detail
    /// may carry remote-influenced bytes; `Display` escapes and truncates
    /// it, so rendering this error anywhere is safe.
    #[error("transport error: {}", escape_detail(.0))]
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

    #[test]
    fn plan_error_converts_into_source_error() {
        let plan = PlanError::UnknownProject(ProjectId::new("ghost").expect("slug"));
        let err: SourceError = plan.into();
        assert!(matches!(err, SourceError::Plan(_)));
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn transport_detail_is_escaped_and_truncated() {
        // Remote-influenced control bytes must not pass through Display.
        let err = SourceError::Transport("\x1b]0;pwn\x07x".to_string());
        assert_eq!(err.to_string(), r"transport error: \u{1b}]0;pwn\u{7}x");

        let long = "a".repeat(MAX_TRANSPORT_DETAIL_LEN + 1);
        let shown = SourceError::Transport(long).to_string();
        assert!(shown.ends_with('…'));
        assert_eq!(
            shown.len(),
            "transport error: ".len() + MAX_TRANSPORT_DETAIL_LEN + '…'.len_utf8()
        );
    }

    #[test]
    fn workspace_dirty_names_session_and_reason() {
        let err = SourceError::WorkspaceDirty {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            reason: DirtyReason::Both,
        };
        assert_eq!(
            err.to_string(),
            "session `api_fix-login` has uncommitted changes and commits not on any remote that would be lost"
        );
        assert_eq!(DirtyReason::Uncommitted.to_string(), "uncommitted changes");
        assert_eq!(
            DirtyReason::NotOnRemote.to_string(),
            "commits not on any remote"
        );
    }
}
