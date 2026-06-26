//! Frontend-facing error + the session-metadata DTO (keeps remora-protocol serde-only).
use remora_core::SourceError;
use remora_protocol::{SessionMeta, SessionState};

use crate::bridge::editor_dto::WorkspaceModeDto;

#[derive(Clone, Copy, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub enum DirtyReasonDto {
    Uncommitted,
    NotOnRemote,
    Both,
}

impl From<remora_core::DirtyReason> for DirtyReasonDto {
    fn from(r: remora_core::DirtyReason) -> Self {
        match r {
            remora_core::DirtyReason::Uncommitted => DirtyReasonDto::Uncommitted,
            remora_core::DirtyReason::NotOnRemote => DirtyReasonDto::NotOnRemote,
            remora_core::DirtyReason::Both => DirtyReasonDto::Both,
        }
    }
}

#[derive(Debug, serde::Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BridgeError {
    SessionExists {
        message: String,
    },
    SessionNotFound {
        message: String,
    },
    ChannelClosed,
    Transport {
        message: String,
    },
    Plan {
        message: String,
    },
    InvalidId {
        message: String,
    },
    UnknownHandle,
    InvalidSize {
        message: String,
    },
    /// The config file exists but could not be read or parsed. A *missing*
    /// file is NOT this error — it maps to an empty config (a fresh device is
    /// valid). Permission/parse/validation failures surface here so the sidebar
    /// shows a banner instead of a silently-empty tree.
    Config {
        message: String,
    },
    /// An in-app config edit (insert/update/remove) was rejected: a duplicate
    /// id, a missing id, a dangling reference, a referenced entry being removed,
    /// or a save IO failure. Carries the rendered (already sanitized)
    /// `ConfigError`. Distinct from `Config` (whole-file load failure → sidebar
    /// banner) so the frontend can show this inline on the offending form.
    ConfigEdit {
        message: String,
    },
    /// A session removal was blocked because the workspace has unsaved state
    /// (uncommitted changes or commits not pushed to any remote). Carry the
    /// reason so the frontend can show a targeted warning and offer `force`.
    WorkspaceDirty {
        message: String,
        reason: DirtyReasonDto,
    },
}

impl From<SourceError> for BridgeError {
    fn from(e: SourceError) -> Self {
        // message comes ONLY from Display (already escapes/bounds untrusted bytes).
        let message = e.to_string();
        match e {
            SourceError::SessionExists { .. } => BridgeError::SessionExists { message },
            SourceError::SessionNotFound { .. } => BridgeError::SessionNotFound { message },
            SourceError::ChannelClosed => BridgeError::ChannelClosed,
            SourceError::Plan(_) => BridgeError::Plan { message },
            // WorkspaceDirty must be placed BEFORE the catch-all.
            SourceError::WorkspaceDirty { reason, .. } => BridgeError::WorkspaceDirty {
                message,
                reason: reason.into(),
            },
            // #[non_exhaustive]: unknown future variants degrade to Transport.
            _ => BridgeError::Transport { message },
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionStateDto {
    Live,
    Stopped,
}

impl From<SessionState> for SessionStateDto {
    fn from(s: SessionState) -> Self {
        match s {
            SessionState::Live => SessionStateDto::Live,
            SessionState::Stopped => SessionStateDto::Stopped,
            _ => SessionStateDto::Stopped, // #[non_exhaustive] guard
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaDto {
    pub project_id: String,
    pub session_id: String,
    pub state: SessionStateDto,
    /// Agent id the sandbox advertises for this session. Untrusted,
    /// display-only: the discovery layer sanitizes it; never build a command
    /// or path from it.
    pub agent: Option<String>,
    pub created_at: Option<u64>,
    /// Workspace path the sandbox advertises. Untrusted, display-only (same
    /// rule as `agent`).
    pub workspace_path: Option<String>,
    /// Effective workspace mode discovered for this session (real state), or
    /// null from an older sender. Drives sidebar/tab gating.
    pub workspace: Option<WorkspaceModeDto>,
    /// Git branch the sandbox advertises for this session. Untrusted,
    /// display-only (same rule as `agent`/`workspace_path`).
    pub branch: Option<String>,
}

impl From<SessionMeta> for SessionMetaDto {
    fn from(m: SessionMeta) -> Self {
        SessionMetaDto {
            project_id: m.project_id.to_string(),
            session_id: m.session_id.to_string(),
            state: m.state.into(),
            agent: m.agent,
            created_at: m.created_at,
            workspace_path: m.workspace_path,
            workspace: m.workspace.map(Into::into),
            branch: m.branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_core::SourceError;
    use remora_protocol::{ProjectId, SessionId, SessionMeta, SessionState};

    #[test]
    fn maps_source_errors_to_kinds() {
        let e = SourceError::SessionNotFound {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("x").expect("slug"),
        };
        assert!(matches!(
            BridgeError::from(e),
            BridgeError::SessionNotFound { .. }
        ));
        assert!(matches!(
            BridgeError::from(SourceError::ChannelClosed),
            BridgeError::ChannelClosed
        ));
        assert!(matches!(
            BridgeError::from(SourceError::Transport("x".into())),
            BridgeError::Transport { .. }
        ));
        assert!(matches!(
            BridgeError::from(SourceError::SessionExists {
                project_id: ProjectId::new("api").expect("slug"),
                session_id: SessionId::new("x").expect("slug"),
            }),
            BridgeError::SessionExists { .. }
        ));
        assert!(matches!(
            BridgeError::from(SourceError::Plan(remora_core::PlanError::UnknownProject(
                ProjectId::new("ghost").expect("slug")
            ))),
            BridgeError::Plan { .. }
        ));
        assert!(matches!(
            BridgeError::from(SourceError::WorkspaceDirty {
                project_id: ProjectId::new("api").expect("slug"),
                session_id: SessionId::new("x").expect("slug"),
                reason: remora_core::DirtyReason::NotOnRemote,
            }),
            BridgeError::WorkspaceDirty {
                reason: DirtyReasonDto::NotOnRemote,
                ..
            }
        ));
    }

    #[test]
    fn dto_round_trips_camelcase() {
        let meta = SessionMeta {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            state: SessionState::Stopped,
            agent: Some("claude".into()),
            created_at: Some(1_765_500_000),
            workspace_path: None,
            workspace: Some(remora_protocol::WorkspaceMode::Worktree),
            branch: None,
        };
        let dto = SessionMetaDto::from(meta);
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""projectId":"api""#));
        assert!(json.contains(r#""state":"stopped""#));
        assert!(json.contains(r#""workspace":"worktree""#));
    }

    #[test]
    fn session_meta_branch_is_copied_to_dto() {
        let meta = SessionMeta {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            state: SessionState::Live,
            agent: None,
            created_at: None,
            workspace_path: None,
            workspace: None,
            branch: Some("feat/login".into()),
        };
        let dto = SessionMetaDto::from(meta);
        assert_eq!(dto.branch, Some("feat/login".to_string()));
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""branch":"feat/login""#));
    }
}
