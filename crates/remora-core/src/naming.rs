//! The versioned tmux session name shared by every transport and (later)
//! discovery. The name *is* the session's identity on the sandbox.

use remora_protocol::{ProjectId, SessionId};

/// Builds the tmux session name for a session (ADR-0004):
/// `remora_<project-id>_<session-id>`. Ids are validated `[a-z0-9-]+`
/// slugs, so the `_`-separated name parses unambiguously. The inverse
/// (parsing) lands with discovery in stage 6.
pub fn tmux_session_name(project: &ProjectId, session: &SessionId) -> String {
    format!("remora_{}_{}", project.as_str(), session.as_str())
}

/// Worktree path convention (ADR-0004, versioned wire format):
/// `~/.remora/worktrees/<project-id>/<session-id>`. The `~` is expanded by
/// the transport, never stored expanded (stage-6 discovery round-trips it).
pub fn worktree_path(project: &ProjectId, session: &SessionId) -> String {
    format!(
        "~/.remora/worktrees/{}/{}",
        project.as_str(),
        session.as_str()
    )
}

/// Branch convention for a worktree session (ADR-0004): `remora/<session-id>`.
pub fn branch_name(session: &SessionId) -> String {
    format!("remora/{}", session.as_str())
}

/// Session-environment metadata keys (ADR-0004, versioned wire format).
/// Stage-5 spawn writes them; stage-6 discovery reads them via
/// `tmux show-environment`.
pub const ENV_AGENT: &str = "REMORA_AGENT";
pub const ENV_WORKSPACE: &str = "REMORA_WORKSPACE";
pub const ENV_CREATED_AT: &str = "REMORA_CREATED_AT";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_follows_the_versioned_convention() {
        let project = ProjectId::new("api").expect("valid slug");
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(
            worktree_path(&project, &session),
            "~/.remora/worktrees/api/fix-login"
        );
    }

    #[test]
    fn branch_name_is_remora_slash_session() {
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(branch_name(&session), "remora/fix-login");
    }

    #[test]
    fn env_var_names_are_stable_wire_format() {
        assert_eq!(ENV_AGENT, "REMORA_AGENT");
        assert_eq!(ENV_WORKSPACE, "REMORA_WORKSPACE");
        assert_eq!(ENV_CREATED_AT, "REMORA_CREATED_AT");
    }

    #[test]
    fn formats_as_remora_project_session() {
        let project = ProjectId::new("api").expect("valid slug");
        let session = SessionId::new("fix-login").expect("valid slug");
        assert_eq!(
            tmux_session_name(&project, &session),
            "remora_api_fix-login"
        );

        // Hyphenated slugs are unambiguous because `_` is the only separator.
        let project = ProjectId::new("web-app").expect("valid slug");
        let session = SessionId::new("add-tests").expect("valid slug");
        assert_eq!(
            tmux_session_name(&project, &session),
            "remora_web-app_add-tests"
        );
    }
}
