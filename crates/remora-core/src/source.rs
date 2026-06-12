//! The transport seam every client drives (ADR-0005).

use async_trait::async_trait;
use remora_protocol::{ProjectId, SessionId, SessionMeta, SpawnSpec};

use crate::{SessionChannel, SourceError};

/// One instance = one configured host (ssh, kubectl exec, or the
/// in-process [`fake`](crate::fake)). UI code never talks to a transport
/// directly — everything goes through this trait, which is what makes the
/// relay an optional drop-in (ARCHITECTURE.md).
#[async_trait]
pub trait SessionSource: Send + Sync {
    /// Creates a new session per the spec and opens a channel to it.
    ///
    /// Fails closed with [`SourceError::SessionExists`] if the session
    /// already exists in any state; tmux-name uniqueness is the anti-race
    /// lock (ADR-0004). Respawning a *stopped* session is discovery-layer
    /// logic, not a spawn variant.
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
    /// (spine spike).
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError>;
}
