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

#[cfg(test)]
mod tests {
    use super::*;

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
