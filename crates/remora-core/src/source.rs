//! The transport seam every client drives (ADR-0005).

use async_trait::async_trait;
use remora_protocol::{AgentId, ProjectId, SessionId, SessionMeta, SpawnSpec};

use crate::{SessionChannel, SourceError};

/// How a client reaches a session's workspace over the transport, so a local
/// editor can open it. Core describes the *remote*; the desktop shell maps each
/// variant to a concrete editor invocation (VS Code:
/// `code --remote ssh-remote+{authority} {path}`). Editor-agnostic by design
/// (AGENTS.md's one rule) — core never names `code` or `ssh-remote+`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteWorkspace {
    /// Reachable over SSH at `authority` (`[user@]host[:port]`), workspace dir
    /// `path`. `authority` may be a `~/.ssh/config` alias.
    Ssh { authority: String, path: String },
}

/// The error every transport that cannot express a local-editor target returns
/// from [`SessionSource::remote_workspace`]. SSH-only in v1; kubectl is a
/// follow-up (VS Code Remote-Tunnels over `kubectl exec`).
pub fn unsupported_remote_workspace() -> SourceError {
    SourceError::Transport("Open in VS Code is only supported for SSH sessions".into())
}

/// One instance = one configured host (ssh, kubectl exec, or the
/// in-process [`fake`](crate::fake)). UI code never talks to a transport
/// directly — everything goes through this trait, which is what makes the
/// relay an optional drop-in (ARCHITECTURE.md).
#[async_trait]
pub trait SessionSource: Send + Sync {
    /// Creates a new session per the spec and opens a channel to it.
    ///
    /// Fails closed with [`SourceError::SessionExists`] if a *live* session
    /// already holds the name — tmux-name uniqueness is the anti-race lock
    /// (ADR-0004). Whether a *stopped* session (surviving worktree, no tmux
    /// session) also blocks spawn is implementation-defined: the in-process
    /// fake reports `SessionExists` for it, but a real transport can only
    /// detect it with a separate, non-atomic worktree check. Respawning a
    /// stopped session is discovery-layer logic (roadmap stage 6), not a
    /// spawn variant.
    ///
    /// Not cancellation-safe: dropping the returned future mid-flight may
    /// leave the session created without the caller learning of it —
    /// recover via [`list`](Self::list).
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError>;

    /// Opens a channel to an existing *live* session.
    ///
    /// Evicting stale clients (tmux `attach -d`) is the implementation's
    /// job; an evicted channel observes death, nothing more. Stopped or
    /// unknown sessions are [`SourceError::SessionNotFound`].
    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError>;

    /// The full local argv (`ssh …` / `kubectl exec …` + `tmux attach-session
    /// -t <name>`, no `-d`) that an EXTERNAL terminal can run to attach to
    /// this session alongside the app's own client. Pure composition — no
    /// liveness preflight (the UI gates on live state; a dead session shows
    /// tmux's own error in the external terminal). The first token is the
    /// transport binary by bare name; the desktop shell resolves it to an
    /// absolute path before spawning (GUI-launched apps inherit a bare PATH).
    /// kubectl hosts resolve `{ command }` fields locally first (ADR-0008),
    /// so this can fail with the same resolution errors as `attach`.
    async fn external_attach_command(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<Vec<String>, SourceError>;

    /// The editor-open target for this session's workspace, or
    /// [`unsupported_remote_workspace`] for a transport that has none
    /// (kubectl, the relay proxy). Pure composition: `workspace_path` is
    /// supplied by the caller (the desktop bridge resolves it from discovery),
    /// so this performs no round-trip and no per-session locking. The ids are
    /// passed for interface uniformity; SSH ignores them (the authority is
    /// per-host). Never evicts, never mutates.
    async fn remote_workspace(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        workspace_path: &str,
    ) -> Result<RemoteWorkspace, SourceError>;

    /// Discovers sessions and their liveness without attaching.
    ///
    /// Never inferred from PTY bytes — listing is a separate control plane
    /// (spine spike). Order is unspecified; callers that render a stable
    /// list must sort. (The fake sorts by `(project_id, session_id)` for
    /// test determinism, but transports need not.)
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError>;

    /// Re-creates the tmux session for a *stopped* worktree and attaches.
    ///
    /// The worktree already survives, so this never runs `git worktree add`.
    /// The agent is the caller-supplied `agent` (the client carries the pre-stop `REMORA_AGENT` from discovery, D6), else the project default.
    /// Requires the resolved project to be worktree-mode — a shared-mode project returns
    /// [`PlanError::NotWorktreeProject`](crate::PlanError) rather than
    /// spawning into the project root. A concurrent respawner that already won
    /// the race leaves a *live* session of this name, in which case this
    /// attaches to it instead of double-spawning (ADR-0004). A vanished
    /// worktree (its directory removed out from under a surviving session
    /// record) surfaces as [`SourceError::SessionNotFound`] — there is nothing
    /// left to respawn; a transport that cannot even probe surfaces as
    /// [`SourceError::Transport`].
    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError>;

    /// Kills the session's tmux session, leaving the worktree intact so the
    /// session surfaces as *stopped* and can be respawned. Idempotent: an
    /// already-absent or already-stopped session is `Ok(())`.
    async fn stop(&self, project_id: &ProjectId, session_id: &SessionId)
        -> Result<(), SourceError>;

    /// Permanently destroys the session: kills the tmux session, then for
    /// worktree-mode projects removes the worktree directory and branch.
    ///
    /// Fails with [`SourceError::WorkspaceDirty`] if `force` is `false` and
    /// the worktree has uncommitted changes or commits not on any remote.
    /// Idempotent: an already-absent session is `Ok(())`.
    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError>;
}
