//! Session metadata, lifecycle state, and the spawn request.

use serde::{Deserialize, Serialize};

use crate::{AgentId, ProjectId, SessionId};

/// Lifecycle state of a session as discovered on a host (ADR-0004).
///
/// `Live` means the named tmux session exists; `Stopped` means only the
/// workspace (worktree) survives — e.g. after a pod restart — and the
/// session can be respawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Live,
    Stopped,
}

/// One discovered session, as rendered in the sidebar.
///
/// `project_id`/`session_id`/`state` come from the tmux session name and
/// liveness check. The remaining fields ride in tmux session environment
/// variables and are **untrusted, display-only** input (ADR-0004): anyone
/// with a shell on the sandbox can forge them, so they are plain optional
/// strings — a forged value must never make the message undeserializable,
/// and nothing may build commands from them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub state: SessionState,
    /// Agent adapter id the session was launched with, as advertised by the
    /// sandbox. Untrusted; display only.
    pub agent: Option<String>,
    /// Creation time in unix epoch seconds, as advertised by the sandbox.
    /// Untrusted; display only.
    pub created_at: Option<u64>,
    /// Workspace (worktree) path, as advertised by the sandbox. Untrusted;
    /// display only.
    pub workspace_path: Option<String>,
}

/// Request to create a new session.
///
/// Deliberately carries only *references into local configuration* — never
/// paths or commands. Spawn builds the remote command exclusively from the
/// local project and agent adapter config (ADR-0004); the `session_id` is
/// minted client-side and creation fails closed if the tmux name already
/// exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    /// Agent adapter to launch; `None` uses the project's default agent.
    pub agent: Option<AgentId>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentId, ProjectId, SessionId};

    fn meta() -> SessionMeta {
        SessionMeta {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("fix-login").expect("valid slug"),
            state: SessionState::Live,
            agent: Some("claude".to_string()),
            created_at: Some(1_765_500_000),
            workspace_path: Some("/home/dev/.remora/worktrees/api/fix-login".to_string()),
        }
    }

    #[test]
    fn session_meta_round_trips() {
        let m = meta();
        let json = serde_json::to_string(&m).expect("serialize");
        let back: SessionMeta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn session_meta_wire_format() {
        let json = serde_json::to_string(&meta()).expect("serialize");
        assert_eq!(
            json,
            r#"{"project_id":"api","session_id":"fix-login","state":"live","agent":"claude","created_at":1765500000,"workspace_path":"/home/dev/.remora/worktrees/api/fix-login"}"#
        );
    }

    #[test]
    fn discovered_metadata_is_optional() {
        let json = r#"{"project_id":"api","session_id":"fix-login","state":"stopped","agent":null,"created_at":null,"workspace_path":null}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("deserialize");
        assert_eq!(m.state, SessionState::Stopped);
        assert_eq!(m.agent, None);
    }

    #[test]
    fn session_state_wire_format_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionState::Live).expect("serialize"),
            r#""live""#
        );
        assert_eq!(
            serde_json::to_string(&SessionState::Stopped).expect("serialize"),
            r#""stopped""#
        );
    }

    #[test]
    fn spawn_spec_round_trips_and_wire_format() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("fix-login").expect("valid slug"),
            agent: Some(AgentId::new("claude").expect("valid slug")),
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        assert_eq!(
            json,
            r#"{"project_id":"api","session_id":"fix-login","agent":"claude"}"#
        );
        let back: SpawnSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn spawn_spec_agent_defaults_to_project_agent() {
        let json = r#"{"project_id":"api","session_id":"fix-login","agent":null}"#;
        let spec: SpawnSpec = serde_json::from_str(json).expect("deserialize");
        assert_eq!(spec.agent, None);
    }

    #[test]
    fn spawn_spec_rejects_forged_ids() {
        let json = r#"{"project_id":"api; rm -rf /","session_id":"fix-login","agent":null}"#;
        assert!(serde_json::from_str::<SpawnSpec>(json).is_err());
    }
}
