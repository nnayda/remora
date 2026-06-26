//! Session metadata, lifecycle state, and the spawn request.

use serde::{Deserialize, Serialize};

use crate::{AgentId, ProjectId, SessionId};

/// Workspace mode for a session (ADR-0004/ADR-0008). `shared` sessions can
/// clobber each other, so it is an explicit opt-in, never defaulted. Lives in
/// the protocol crate because both `SpawnSpec` (the per-session override) and
/// `SessionMeta` (the effective mode discovered from the sandbox) carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Each session gets a fresh git worktree + branch.
    Worktree,
    /// Sessions share the project directory (effectively single-writer).
    Shared,
}

/// Lifecycle state of a session as discovered on a host (ADR-0004).
///
/// `Live` means the named tmux session exists; `Stopped` means only the
/// workspace (worktree) survives — e.g. after a pod restart — and the
/// session can be respawned. The set is closed by construction (a tmux
/// session either exists or it doesn't); adding a state is a breaking
/// protocol change requiring a [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)
/// bump, since older clients reject unknown variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionState {
    Live,
    Stopped,
}

/// Per-session agent activity, distinct from lifecycle [`SessionState`].
///
/// Produced by the core-side detector (ADR-0013). `Awaiting` is **marker-only,
/// never inferred** from a quiescent screen. `Unknown` is the initial state of a
/// freshly-attached channel before any byte arrives; it is never produced by a
/// parse failure. Closed by construction; adding a variant is a breaking change
/// (bump [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionStatus {
    Working,
    Idle,
    Awaiting,
    Unknown,
}

/// One discovered session, as rendered in the sidebar.
///
/// `project_id`/`session_id`/`state` come from the tmux session name and
/// liveness check. The remaining fields ride in tmux session environment
/// variables and are **untrusted, display-only** input (ADR-0004): anyone
/// with a shell on the sandbox can forge them, so they are plain optional
/// strings — a forged value must never make the message undeserializable,
/// and nothing may build commands from them.
///
/// Constructors (discovery, the future relay) own that invariant for the
/// typed fields too: ids and `created_at` validate during deserialization,
/// so senders must *drop* sessions whose discovered names don't parse and
/// map unparseable metadata to `None` — one forged element forwarded as-is
/// would make the entire enclosing message undeserializable for every
/// client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub state: SessionState,
    /// Agent adapter id the session was launched with, as advertised by the
    /// sandbox. Untrusted; display only.
    pub agent: Option<String>,
    /// Creation time in unix epoch seconds, as advertised by the sandbox.
    /// Untrusted; display only. Senders must map unparseable or
    /// out-of-range discovered values to `None` rather than forwarding
    /// them — a non-numeric forged value would fail the whole message.
    pub created_at: Option<u64>,
    /// Workspace (worktree) path, as advertised by the sandbox. Untrusted;
    /// display only.
    pub workspace_path: Option<String>,
    /// Effective workspace mode, discovered from real sandbox state (a surviving
    /// worktree ⇒ `Worktree`). Drives display gating. `None` from an older
    /// sender; the client then falls back to the project's configured mode.
    pub workspace: Option<WorkspaceMode>,
    /// Branch checked out in the worktree, as advertised by the sandbox
    /// (`git worktree list`). The session's display identity. Untrusted;
    /// display only. `None` for a shared session or a detached-HEAD worktree.
    #[serde(default)]
    pub branch: Option<String>,
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
    /// Per-session git start-point override for a new worktree (#54). `None`
    /// or empty falls through to the project default / detection. Raw here;
    /// `spawn_plan::normalize_base` trims and validates it.
    pub base: Option<String>,
    /// Per-session workspace-mode override; `None` uses the project's default.
    /// Always serialized (mirrors `agent`); an absent key deserializes to
    /// `None`, so older peers stay compatible without a `PROTOCOL_VERSION` bump.
    pub workspace: Option<WorkspaceMode>,
    /// Per-session branch name (#124). Raw; `spawn_plan::normalize_branch`
    /// validates it. `None` falls back to the convention (`remora/<session_id>`)
    /// so an older client keeps working. The session's display identity.
    #[serde(default)]
    pub branch: Option<String>,
    /// Per-session worktree-root override (#124); the worktree lands at
    /// `<worktree_root>/<branch>`. `None` falls through the project→host
    /// default cascade, then the `~/.remora/worktrees/<project>` convention.
    #[serde(default)]
    pub worktree_root: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> SessionMeta {
        SessionMeta {
            project_id: ProjectId::new("api").expect("valid slug"),
            session_id: SessionId::new("fix-login").expect("valid slug"),
            state: SessionState::Live,
            agent: Some("claude".to_string()),
            created_at: Some(1_765_500_000),
            workspace_path: Some("/home/dev/.remora/worktrees/api/fix-login".to_string()),
            workspace: None,
            branch: None,
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
            r#"{"project_id":"api","session_id":"fix-login","state":"live","agent":"claude","created_at":1765500000,"workspace_path":"/home/dev/.remora/worktrees/api/fix-login","workspace":null,"branch":null}"#
        );
    }

    #[test]
    fn session_meta_carries_optional_branch() {
        let mut m = meta();
        m.branch = Some("feat/login".to_string());
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(json.contains(r#""branch":"feat/login""#));
        let back: SessionMeta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.branch.as_deref(), Some("feat/login"));
    }

    #[test]
    fn session_meta_branch_defaults_to_none_when_absent() {
        // Older senders omit the key entirely.
        let json = r#"{"project_id":"api","session_id":"fix-login","state":"stopped"}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("deserialize");
        assert_eq!(m.branch, None);
    }

    #[test]
    fn discovered_metadata_is_optional() {
        let json = r#"{"project_id":"api","session_id":"fix-login","state":"stopped","agent":null,"created_at":null,"workspace_path":null}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("deserialize");
        assert_eq!(m.state, SessionState::Stopped);
        assert_eq!(m.agent, None);
    }

    #[test]
    fn discovered_metadata_tolerates_absent_keys() {
        // Senders may omit absent fields entirely rather than sending null.
        let json = r#"{"project_id":"api","session_id":"fix-login","state":"live"}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("deserialize");
        assert_eq!(m.agent, None);
        assert_eq!(m.created_at, None);
        assert_eq!(m.workspace_path, None);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        // A newer peer may add fields; older clients must keep parsing.
        let json = r#"{"project_id":"api","session_id":"fix-login","state":"live","agent":null,"created_at":null,"workspace_path":null,"future_field":true}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("deserialize");
        assert_eq!(m.state, SessionState::Live);
    }

    #[test]
    fn spawn_spec_tolerates_absent_agent_key() {
        let json = r#"{"project_id":"api","session_id":"fix-login"}"#;
        let spec: SpawnSpec = serde_json::from_str(json).expect("deserialize");
        assert_eq!(spec.agent, None);
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
            base: None,
            workspace: None,
            branch: None,
            worktree_root: None,
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        assert_eq!(
            json,
            r#"{"project_id":"api","session_id":"fix-login","agent":"claude","base":null,"workspace":null,"branch":null,"worktree_root":null}"#
        );
        let back: SpawnSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn spawn_spec_carries_optional_base() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: Some("origin/main".to_string()),
            workspace: None,
            branch: None,
            worktree_root: None,
        };
        assert_eq!(spec.base.as_deref(), Some("origin/main"));
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains(r#""base":"origin/main""#));
        let back: SpawnSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(spec, back);
    }

    #[test]
    fn spawn_spec_carries_branch_and_worktree_root() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: Some("feat/login".to_string()),
            worktree_root: Some("~/work".to_string()),
        };
        assert_eq!(spec.branch.as_deref(), Some("feat/login"));
        assert_eq!(spec.worktree_root.as_deref(), Some("~/work"));
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains(r#""branch":"feat/login""#));
        assert!(json.contains(r#""worktree_root":"~/work""#));
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

    #[test]
    fn spawn_spec_workspace_round_trips_and_defaults_to_none() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: Some(WorkspaceMode::Shared),
            branch: None,
            worktree_root: None,
        };
        let json = serde_json::to_string(&spec).expect("serialize");
        assert!(json.contains(r#""workspace":"shared""#));
        assert_eq!(serde_json::from_str::<SpawnSpec>(&json).expect("de"), spec);
        // Absent key tolerated → None.
        let bare = r#"{"project_id":"api","session_id":"s1"}"#;
        assert_eq!(
            serde_json::from_str::<SpawnSpec>(bare)
                .expect("de")
                .workspace,
            None
        );
    }

    #[test]
    fn session_meta_carries_effective_workspace() {
        let json =
            r#"{"project_id":"api","session_id":"s1","state":"live","workspace":"worktree"}"#;
        let m: SessionMeta = serde_json::from_str(json).expect("de");
        assert_eq!(m.workspace, Some(WorkspaceMode::Worktree));
        // Absent → None (older sender).
        let bare = r#"{"project_id":"api","session_id":"s1","state":"live"}"#;
        assert_eq!(
            serde_json::from_str::<SessionMeta>(bare)
                .expect("de")
                .workspace,
            None
        );
    }

    #[test]
    fn session_status_wire_format_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionStatus::Working).expect("ser"),
            r#""working""#
        );
        assert_eq!(
            serde_json::to_string(&SessionStatus::Idle).expect("ser"),
            r#""idle""#
        );
        assert_eq!(
            serde_json::to_string(&SessionStatus::Awaiting).expect("ser"),
            r#""awaiting""#
        );
        assert_eq!(
            serde_json::to_string(&SessionStatus::Unknown).expect("ser"),
            r#""unknown""#
        );
        let back: SessionStatus = serde_json::from_str(r#""awaiting""#).expect("de");
        assert_eq!(back, SessionStatus::Awaiting);
    }
}
