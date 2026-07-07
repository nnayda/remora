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
    /// External-terminal launch could not resolve a terminal: nothing
    /// configured (zero or several detected), or the config names an unknown
    /// or uninstalled registry id. The frontend deep-links Settings on this
    /// kind (spec §4); launch/exec failures are `Transport` instead.
    TerminalNotConfigured {
        message: String,
    },
    /// A session removal was blocked because the workspace has unsaved state
    /// (uncommitted changes or commits not pushed to any remote). Carry the
    /// reason so the frontend can show a targeted warning and offer `force`.
    WorkspaceDirty {
        message: String,
        reason: DirtyReasonDto,
    },
    /// A relay/pairing command was invoked but this device hosts no relay bridge
    /// (no `[relay]` section — or it was removed by a live config edit, #277 —
    /// so the supervisor holds no `PairingHandles`). The UI shows a "relay not
    /// configured" state instead of a pairing panel.
    RelayNotConfigured {
        message: String,
    },
    /// A relay/pairing or roster operation failed inside the running bridge
    /// (e.g. the relay link was down when opening a window, or roster storage
    /// errored on revoke). Distinct from `RelayNotConfigured` (no bridge at all).
    Relay {
        message: String,
    },
}

impl From<remora_bridge::BridgeError> for BridgeError {
    fn from(e: remora_bridge::BridgeError) -> Self {
        // Display already bounds/escapes any untrusted content; carry it as the
        // frontend message. Every bridge-side pairing/roster failure maps here.
        BridgeError::Relay {
            message: e.to_string(),
        }
    }
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

/// One host's discovery outcome for a single `Bridge::list` poll. `available`
/// is false when the host's `source.list()` errored (then `sessions` is empty);
/// retention of last-good rows for a transiently-down host is the frontend's job.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionsDto {
    pub host_id: String,
    pub available: bool,
    pub sessions: Vec<SessionMetaDto>,
}

/// Result of a discovery poll: one bucket per host attempted this poll, in
/// config order. Sessions are sorted by (project_id, session_id) within each
/// bucket (the frontend re-sorts after flattening across hosts).
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SessionListDto {
    pub hosts: Vec<HostSessionsDto>,
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
    fn host_sessions_dto_serializes_camelcase() {
        let dto = HostSessionsDto {
            host_id: "hermes".into(),
            available: false,
            sessions: vec![],
        };
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(json.contains(r#""hostId":"hermes""#));
        assert!(json.contains(r#""available":false"#));
        assert!(json.contains(r#""sessions":[]"#));
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
    fn terminal_not_configured_serializes_with_its_kind_tag() {
        let e = BridgeError::TerminalNotConfigured {
            message: "no external terminal configured".into(),
        };
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(json.contains(r#""kind":"terminalNotConfigured""#), "{json}");
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
