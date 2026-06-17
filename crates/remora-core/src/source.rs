//! The transport seam every client drives (ADR-0005).

use async_trait::async_trait;
use remora_protocol::{AgentId, ProjectId, SessionId, SessionMeta, SpawnSpec};

use crate::{SessionChannel, SourceError};

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

    /// Discovers sessions and their liveness without attaching.
    ///
    /// Never inferred from PTY bytes — listing is a separate control plane
    /// (spine spike). Order is unspecified; callers that render a stable
    /// list must sort. (The fake sorts by `(project_id, session_id)` for
    /// test determinism, but transports need not.)
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError>;

    /// Re-creates the tmux session for a *stopped* worktree and attaches.
    ///
    /// The worktree already survives, so this never runs `git worktree add`;
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
}
