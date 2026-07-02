//! Per-session mutual exclusion below the `SessionSource` seam (ADR-0021).
//!
//! Once a phone can drive a session through the bridge concurrently with the
//! desktop UI, the desktop's frontend-store guards no longer cover every
//! actor — a second actor reaches the same host through the same
//! `SessionSource`, underneath the UI. [`ExclusiveSource`] wraps any source
//! and serializes the mutating operations (`spawn`/`attach`/`respawn`/`stop`/
//! `remove`) per `(host, project, session)` against a shared [`SessionLocks`]
//! registry; `list` passes through untouched.
//!
//! The registry is the shared state, not the wrapper: the desktop resolves a
//! fresh `SessionSource` (hence a fresh `ExclusiveSource`) per call, so every
//! wrapper for a process must share one `Arc<SessionLocks>`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use remora_protocol::{AgentId, ProjectId, SessionId, SessionMeta, SpawnSpec};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{SessionChannel, SessionSource, SourceError};

/// A per-session lock key: the host key plus the client-minted session
/// identity. The host key is a `String` (the config `HostId` string at the
/// call site) so one registry can span every configured host.
type LockKey = (String, ProjectId, SessionId);

/// Shared keyed-lock registry (spec D7). One per app/bridge process.
///
/// Holds an [`AsyncMutex`] per live key. Entries are created on first
/// [`acquire`](Self::acquire) and pruned when the last holder/waiter drops
/// its guard, so an idle registry is empty.
#[derive(Default)]
pub struct SessionLocks {
    map: Mutex<HashMap<LockKey, Arc<AsyncMutex<()>>>>,
}

impl SessionLocks {
    /// A fresh, empty registry. Returned behind an `Arc` because every
    /// wrapper in the process shares one registry.
    pub fn new() -> Arc<SessionLocks> {
        Arc::new(SessionLocks::default())
    }

    /// Acquires the per-session lock for `key`, returning a guard that
    /// releases on drop and prunes the map entry when no other holder or
    /// waiter remains.
    ///
    /// The std map mutex is never held across the `.await`: we clone the
    /// entry `Arc` out from under the map mutex, then `lock_owned().await`
    /// the async mutex outside it.
    pub(crate) async fn acquire(self: &Arc<Self>, key: LockKey) -> SessionLockGuard {
        // Get-or-insert the entry and clone it out — all under the std mutex.
        let entry = {
            let mut map = self.lock_map();
            Arc::clone(map.entry(key.clone()).or_default())
        };
        // Block on the async mutex OUTSIDE the std mutex. `lock_owned` moves a
        // clone of the entry into the returned guard; `entry` below is a
        // separate clone the guard keeps for the strong-count prune check.
        let guard = Arc::clone(&entry).lock_owned().await;
        SessionLockGuard {
            guard: Some(guard),
            locks: Arc::clone(self),
            key,
            entry,
        }
    }

    /// Locks the map, recovering from poisoning: no invariant spans the lock
    /// (it only ever holds a `HashMap`), so a panicked accessor must not wedge
    /// every later acquire.
    fn lock_map(&self) -> std::sync::MutexGuard<'_, HashMap<LockKey, Arc<AsyncMutex<()>>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of live lock entries. Test-only observation surface (no
    /// `is_empty` — this only exists to assert pruning).
    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.lock_map().len()
    }
}

/// Holds a per-session lock for its lifetime and prunes the registry entry on
/// drop when it is the last reference.
pub struct SessionLockGuard {
    /// The owned async-mutex guard. `Option` so `Drop` can release it
    /// *before* touching the map, letting a waiter proceed and dropping the
    /// clone `lock_owned` stashed inside it.
    guard: Option<OwnedMutexGuard<()>>,
    locks: Arc<SessionLocks>,
    key: LockKey,
    /// A separate clone of the entry `Arc`, kept only so `Drop` can read
    /// `Arc::strong_count` to decide whether to prune.
    entry: Arc<AsyncMutex<()>>,
}

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        // Release the async mutex first: a waiter parked in `acquire` can now
        // wake, and the `lock_owned` clone this guard held is gone, so the
        // only clones attributable to *this* guard is `self.entry`.
        self.guard.take();
        let mut map = self.locks.lock_map();
        // strong_count == 2 means exactly: the map's clone + `self.entry`. Any
        // other waiter/holder cloned the entry under the same map mutex (or
        // holds it inside a pending `lock_owned`), so any such reference makes
        // the count > 2 and blocks the prune — we never drop an entry someone
        // else is using. Checked under the map mutex so the count cannot move
        // between the read and the remove.
        if Arc::strong_count(&self.entry) == 2 {
            map.remove(&self.key);
        }
    }
}

/// Wraps a [`SessionSource`], serializing mutating operations per
/// `(host, project, session)` against a shared [`SessionLocks`].
///
/// `list` passes through lock-free — discovery must never be blocked by an
/// in-flight mutation on any one session.
pub struct ExclusiveSource {
    inner: Arc<dyn SessionSource>,
    locks: Arc<SessionLocks>,
    host_key: String,
}

impl ExclusiveSource {
    /// Wraps `inner`, guarding its mutations with `locks` under `host_key`.
    /// Every wrapper for one process must share the same `locks`.
    pub fn new(
        inner: Arc<dyn SessionSource>,
        locks: Arc<SessionLocks>,
        host_key: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            locks,
            host_key: host_key.into(),
        }
    }

    fn key(&self, project_id: &ProjectId, session_id: &SessionId) -> LockKey {
        (
            self.host_key.clone(),
            project_id.clone(),
            session_id.clone(),
        )
    }
}

#[async_trait]
impl SessionSource for ExclusiveSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        // The session_id is client-minted, so the key exists pre-spawn and
        // spawn contends for it like every other op (fail-closed on races).
        let key = (
            self.host_key.clone(),
            spec.project_id.clone(),
            spec.session_id.clone(),
        );
        let _guard = self.locks.acquire(key).await;
        self.inner.spawn(spec).await
    }

    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let _guard = self.locks.acquire(self.key(project_id, session_id)).await;
        self.inner.attach(project_id, session_id).await
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        // Lock-free passthrough: listing is a read across all sessions and
        // must not stall behind a mutation on any single one.
        self.inner.list().await
    }

    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        let _guard = self.locks.acquire(self.key(project_id, session_id)).await;
        self.inner.respawn(project_id, session_id, agent).await
    }

    async fn stop(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<(), SourceError> {
        let _guard = self.locks.acquire(self.key(project_id, session_id)).await;
        self.inner.stop(project_id, session_id).await
    }

    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError> {
        let _guard = self.locks.acquire(self.key(project_id, session_id)).await;
        self.inner.remove(project_id, session_id, force).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FakeSessionSource;
    use remora_protocol::{AgentId, ProjectId, SessionId, SpawnSpec};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    type OpLog = Arc<StdMutex<Vec<String>>>;

    fn log_push(log: &OpLog, entry: &str) {
        log.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry.to_string());
    }

    fn snapshot(log: &OpLog) -> Vec<String> {
        log.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn ids(project: &str, session: &str) -> (ProjectId, SessionId) {
        (
            ProjectId::new(project).expect("valid slug"),
            SessionId::new(session).expect("valid slug"),
        )
    }

    fn spec(project: &str, session: &str) -> SpawnSpec {
        SpawnSpec {
            project_id: ProjectId::new(project).expect("valid slug"),
            session_id: SessionId::new(session).expect("valid slug"),
            agent: Some(AgentId::new("claude").expect("valid slug")),
            base: None,
            workspace: None,
            branch: None,
            worktree_root: None,
        }
    }

    /// Test double that records op-order and parks in `stop`/`remove` until
    /// `release` is notified, announcing entry via `started`. Non-parking ops
    /// record a single token and return a benign result.
    struct Blocking {
        log: OpLog,
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    fn dead_channel() -> SessionChannel {
        // The op-order tests only inspect the log; the channel ends are
        // dropped immediately.
        let (channel, _input_rx, _output_tx) = SessionChannel::pair();
        channel
    }

    #[async_trait]
    impl SessionSource for Blocking {
        async fn spawn(&self, _spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
            log_push(&self.log, "spawn");
            Ok(dead_channel())
        }

        async fn attach(
            &self,
            _project_id: &ProjectId,
            _session_id: &SessionId,
        ) -> Result<SessionChannel, SourceError> {
            log_push(&self.log, "attach");
            Ok(dead_channel())
        }

        async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
            log_push(&self.log, "list");
            Ok(Vec::new())
        }

        async fn respawn(
            &self,
            _project_id: &ProjectId,
            _session_id: &SessionId,
            _agent: Option<AgentId>,
        ) -> Result<SessionChannel, SourceError> {
            log_push(&self.log, "respawn");
            Ok(dead_channel())
        }

        async fn stop(
            &self,
            _project_id: &ProjectId,
            _session_id: &SessionId,
        ) -> Result<(), SourceError> {
            log_push(&self.log, "stop:start");
            self.started.notify_one();
            self.release.notified().await;
            log_push(&self.log, "stop:end");
            Ok(())
        }

        async fn remove(
            &self,
            _project_id: &ProjectId,
            _session_id: &SessionId,
            _force: bool,
        ) -> Result<(), SourceError> {
            log_push(&self.log, "remove:start");
            self.started.notify_one();
            self.release.notified().await;
            log_push(&self.log, "remove:end");
            Ok(())
        }
    }

    fn blocking() -> (Arc<Blocking>, OpLog, Arc<Notify>, Arc<Notify>) {
        let log: OpLog = Arc::new(StdMutex::new(Vec::new()));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let double = Arc::new(Blocking {
            log: Arc::clone(&log),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        (double, log, started, release)
    }

    #[tokio::test]
    async fn attach_waits_for_in_flight_stop_on_same_session() {
        let (double, log, started, release) = blocking();
        let locks = SessionLocks::new();
        let src = Arc::new(ExclusiveSource::new(double, Arc::clone(&locks), "h1"));

        let (project, session) = ids("api", "fix-login");
        let stop_src = Arc::clone(&src);
        let stop_task = tokio::spawn(async move {
            stop_src.stop(&project, &session).await.expect("stop");
        });
        // Wait until the parked stop is holding the lock.
        started.notified().await;

        let (project, session) = ids("api", "fix-login");
        let attach_src = Arc::clone(&src);
        let attach_task = tokio::spawn(async move {
            attach_src.attach(&project, &session).await.expect("attach");
        });
        // Let the attach task reach (and block on) the lock.
        tokio::task::yield_now().await;

        release.notify_one();
        stop_task.await.expect("join stop");
        attach_task.await.expect("join attach");

        // attach did not interleave: it ran strictly after stop finished.
        assert_eq!(snapshot(&log), vec!["stop:start", "stop:end", "attach"]);
    }

    #[tokio::test]
    async fn respawn_waits_for_in_flight_remove() {
        let (double, log, started, release) = blocking();
        let locks = SessionLocks::new();
        let src = Arc::new(ExclusiveSource::new(double, Arc::clone(&locks), "h1"));

        let (project, session) = ids("api", "fix-login");
        let remove_src = Arc::clone(&src);
        let remove_task = tokio::spawn(async move {
            remove_src
                .remove(&project, &session, false)
                .await
                .expect("remove");
        });
        started.notified().await;

        let (project, session) = ids("api", "fix-login");
        let respawn_src = Arc::clone(&src);
        let respawn_task = tokio::spawn(async move {
            respawn_src
                .respawn(&project, &session, None)
                .await
                .expect("respawn");
        });
        tokio::task::yield_now().await;

        release.notify_one();
        remove_task.await.expect("join remove");
        respawn_task.await.expect("join respawn");

        assert_eq!(
            snapshot(&log),
            vec!["remove:start", "remove:end", "respawn"]
        );
    }

    #[tokio::test]
    async fn different_sessions_do_not_serialize() {
        let (double, log, started, release) = blocking();
        let locks = SessionLocks::new();
        let src = Arc::new(ExclusiveSource::new(double, Arc::clone(&locks), "h1"));

        // stop(a) parks holding a's lock.
        let (pa, sa) = ids("api", "aaa");
        let stop_src = Arc::clone(&src);
        let stop_task = tokio::spawn(async move {
            stop_src.stop(&pa, &sa).await.expect("stop");
        });
        started.notified().await;

        // attach(b) completes while a is parked — a different key never blocks.
        let (pb, sb) = ids("api", "bbb");
        src.attach(&pb, &sb).await.expect("attach b");

        release.notify_one();
        stop_task.await.expect("join stop");

        // attach ran while stop was still parked (before stop:end).
        assert_eq!(snapshot(&log), vec!["stop:start", "attach", "stop:end"]);
    }

    #[tokio::test]
    async fn different_hosts_do_not_serialize() {
        let (double, log, started, release) = blocking();
        let locks = SessionLocks::new();
        // Two wrappers, same (project, session), different host keys, one
        // shared registry.
        let src1 = Arc::new(ExclusiveSource::new(
            Arc::clone(&double) as Arc<dyn SessionSource>,
            Arc::clone(&locks),
            "h1",
        ));
        let src2 = ExclusiveSource::new(double, Arc::clone(&locks), "h2");

        let (project, session) = ids("api", "fix-login");
        let stop_src = Arc::clone(&src1);
        let stop_task = tokio::spawn(async move {
            stop_src.stop(&project, &session).await.expect("stop");
        });
        started.notified().await;

        let (project, session) = ids("api", "fix-login");
        src2.attach(&project, &session).await.expect("attach on h2");

        release.notify_one();
        stop_task.await.expect("join stop");

        assert_eq!(snapshot(&log), vec!["stop:start", "attach", "stop:end"]);
    }

    #[tokio::test]
    async fn list_is_never_blocked() {
        let (double, log, started, release) = blocking();
        let locks = SessionLocks::new();
        let src = Arc::new(ExclusiveSource::new(double, Arc::clone(&locks), "h1"));

        let (project, session) = ids("api", "fix-login");
        let stop_src = Arc::clone(&src);
        let stop_task = tokio::spawn(async move {
            stop_src.stop(&project, &session).await.expect("stop");
        });
        started.notified().await;

        // list completes even though a session mutation is parked.
        src.list().await.expect("list");

        release.notify_one();
        stop_task.await.expect("join stop");

        assert_eq!(snapshot(&log), vec!["stop:start", "list", "stop:end"]);
    }

    #[tokio::test]
    async fn lock_map_prunes_after_release() {
        let inner: Arc<dyn SessionSource> = Arc::new(FakeSessionSource::new());
        let locks = SessionLocks::new();
        let src = ExclusiveSource::new(inner, Arc::clone(&locks), "h1");

        src.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        src.stop(&project, &session).await.expect("stop");
        // attach after stop fails (session stopped); the guard still prunes.
        let _ = src.attach(&project, &session).await;

        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn errors_propagate_unchanged() {
        let inner: Arc<dyn SessionSource> = Arc::new(FakeSessionSource::new());
        let locks = SessionLocks::new();
        let src = ExclusiveSource::new(inner, Arc::clone(&locks), "h1");

        // No session exists → inner attach returns SessionNotFound verbatim.
        let (project, session) = ids("api", "ghost");
        let err = src.attach(&project, &session).await.expect_err("not found");
        assert!(matches!(err, SourceError::SessionNotFound { .. }));
        // And the map is pruned even on the error path.
        assert_eq!(locks.len(), 0);
    }
}
