//! In-process `SessionSource` for tests and UI development.
//!
//! Behaves like a minimal sandbox: spawn registers a live session whose
//! "agent" echoes input bytes back, attach replays a deterministic banner
//! (mimicking tmux repaint-on-attach) then echoes, list reflects the
//! registry. Inherent methods simulate the failure modes later layers must
//! handle: [`stop_session`](FakeSessionSource::stop_session) (pod restart),
//! [`kill_channels`](FakeSessionSource::kill_channels) (dropped
//! connection), and [`resizes`](FakeSessionSource::resizes) exposes
//! observed geometry.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use remora_protocol::{
    AgentId, ChannelInput, ChannelOutput, ProjectId, SessionId, SessionMeta, SessionState,
    SpawnSpec, TerminalSize,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::{SessionChannel, SessionSource, SourceError};

/// In-memory [`SessionSource`] double. One instance = one fake host.
///
/// Must be driven from a tokio runtime: spawn/attach start echo tasks via
/// `tokio::spawn` and panic outside one.
#[derive(Default)]
pub struct FakeSessionSource {
    sessions: Mutex<Registry>,
}

type Registry = HashMap<(ProjectId, SessionId), FakeSession>;

struct FakeSession {
    state: SessionState,
    agent: Option<String>,
    resizes: Arc<Mutex<Vec<TerminalSize>>>,
    /// Echo tasks for currently open channels; aborting one drops its
    /// transport ends, which the caller observes as channel death.
    channels: Vec<JoinHandle<()>>,
}

impl FakeSession {
    /// Aborts every open channel task, dropping their transport ends.
    fn kill_channels(&mut self) {
        for task in self.channels.drain(..) {
            task.abort();
        }
    }
}

impl FakeSessionSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the registry, recovering from poisoning: the fake backs UI
    /// dev sessions, so one panicked accessor must not wedge every later
    /// call, and no invariant spans the lock.
    fn lock_sessions(&self) -> MutexGuard<'_, Registry> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Simulates a sandbox restart: tmux dies (open channels die with it),
    /// the workspace survives, the session surfaces as *stopped*.
    pub fn stop_session(&self, project_id: &ProjectId, session_id: &SessionId) {
        let mut sessions = self.lock_sessions();
        if let Some(session) = sessions.get_mut(&(project_id.clone(), session_id.clone())) {
            session.state = SessionState::Stopped;
            session.kill_channels();
        }
    }

    /// Simulates a dropped connection: open channels die, the session
    /// itself stays live and re-attachable. Output already buffered in a
    /// channel's queue is still delivered before `recv` reports death.
    pub fn kill_channels(&self, project_id: &ProjectId, session_id: &SessionId) {
        let mut sessions = self.lock_sessions();
        if let Some(session) = sessions.get_mut(&(project_id.clone(), session_id.clone())) {
            session.kill_channels();
        }
    }

    /// Resize messages the session has observed, across all its channels.
    /// Order between concurrent channels is not guaranteed. Grows for the
    /// session's lifetime — a test-scoped observation surface, not a model
    /// of real transport state.
    pub fn resizes(&self, project_id: &ProjectId, session_id: &SessionId) -> Vec<TerminalSize> {
        let sessions = self.lock_sessions();
        sessions
            .get(&(project_id.clone(), session_id.clone()))
            .map(|session| {
                session
                    .resizes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default()
    }
}

/// Forwards input bytes back as output and records resizes — the fake's
/// stand-in for an agent in a PTY. Exits when the caller drops its input
/// sender or stops reading output (send error).
async fn run_echo(
    mut input: mpsc::Receiver<ChannelInput>,
    output: mpsc::Sender<ChannelOutput>,
    resizes: Arc<Mutex<Vec<TerminalSize>>>,
    banner: Option<Vec<u8>>,
) {
    if let Some(banner) = banner {
        if output.send(ChannelOutput::Bytes(banner)).await.is_err() {
            return;
        }
    }
    while let Some(message) = input.recv().await {
        match message {
            ChannelInput::Bytes(bytes) => {
                if output.send(ChannelOutput::Bytes(bytes)).await.is_err() {
                    return;
                }
            }
            ChannelInput::Resize(size) => {
                resizes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(size);
            }
            // ChannelInput is #[non_exhaustive]; ignore unknown messages.
            _ => {}
        }
    }
}

#[async_trait]
impl SessionSource for FakeSessionSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let key = (spec.project_id, spec.session_id);
        let mut sessions = self.lock_sessions();
        if sessions.contains_key(&key) {
            return Err(SourceError::SessionExists {
                project_id: key.0,
                session_id: key.1,
            });
        }
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let (channel, input_rx, output_tx) = SessionChannel::pair();
        let task = tokio::spawn(run_echo(input_rx, output_tx, Arc::clone(&resizes), None));
        sessions.insert(
            key,
            FakeSession {
                state: SessionState::Live,
                agent: spec.agent.map(String::from),
                resizes,
                channels: vec![task],
            },
        );
        Ok(channel)
    }

    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let key = (project_id.clone(), session_id.clone());
        let mut sessions = self.lock_sessions();
        let not_found = || SourceError::SessionNotFound {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        };
        let session = sessions.get_mut(&key).ok_or_else(not_found)?;
        if session.state != SessionState::Live {
            return Err(not_found());
        }
        let banner = format!("[fake attach {project_id}_{session_id}]\r\n").into_bytes();
        let (channel, input_rx, output_tx) = SessionChannel::pair();
        let task = tokio::spawn(run_echo(
            input_rx,
            output_tx,
            Arc::clone(&session.resizes),
            Some(banner),
        ));
        // Reap handles of echo tasks that already exited (caller dropped its
        // channel) so repeated attaches don't grow the vec unboundedly.
        session.channels.retain(|handle| !handle.is_finished());
        session.channels.push(task);
        Ok(channel)
    }

    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        let key = (project_id.clone(), session_id.clone());
        let mut sessions = self.lock_sessions();
        let Some(session) = sessions.get_mut(&key) else {
            return Err(SourceError::SessionNotFound {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            });
        };
        // Record the requested agent so tests can assert D6 plumbing; the fake
        // has no real agent process, so this only affects the reported meta.
        if let Some(a) = agent {
            session.agent = Some(a.as_str().to_string());
        }
        // Already live -> a concurrent respawner won; attach (banner). Stopped
        // -> bring it back live with a fresh spawn-style channel (no banner).
        let banner = if session.state == SessionState::Live {
            Some(format!("[fake attach {project_id}_{session_id}]\r\n").into_bytes())
        } else {
            session.state = SessionState::Live;
            None
        };
        let (channel, input_rx, output_tx) = SessionChannel::pair();
        let task = tokio::spawn(run_echo(
            input_rx,
            output_tx,
            Arc::clone(&session.resizes),
            banner,
        ));
        session.channels.retain(|handle| !handle.is_finished());
        session.channels.push(task);
        Ok(channel)
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let sessions = self.lock_sessions();
        let mut metas: Vec<SessionMeta> = sessions
            .iter()
            .map(|((project_id, session_id), session)| SessionMeta {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                state: session.state,
                agent: session.agent.clone(),
                created_at: None,
                workspace_path: None,
            })
            .collect();
        // Deterministic order for callers and tests.
        metas.sort_by(|a, b| {
            (a.project_id.as_str(), a.session_id.as_str())
                .cmp(&(b.project_id.as_str(), b.session_id.as_str()))
        });
        Ok(metas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionSource;
    use remora_protocol::{
        AgentId, ChannelOutput, ProjectId, SessionId, SessionState, SpawnSpec, TerminalSize,
    };

    fn spec(project: &str, session: &str) -> SpawnSpec {
        SpawnSpec {
            project_id: ProjectId::new(project).expect("valid slug"),
            session_id: SessionId::new(session).expect("valid slug"),
            agent: Some(AgentId::new("claude").expect("valid slug")),
        }
    }

    fn ids(project: &str, session: &str) -> (ProjectId, SessionId) {
        (
            ProjectId::new(project).expect("valid slug"),
            SessionId::new(session).expect("valid slug"),
        )
    }

    async fn recv_bytes(channel: &mut crate::SessionChannel) -> Vec<u8> {
        let Some(ChannelOutput::Bytes(bytes)) = channel.recv().await else {
            panic!("expected bytes output");
        };
        bytes
    }

    #[tokio::test]
    async fn spawn_echoes_input_bytes() {
        let source = FakeSessionSource::new();
        let mut channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        channel.send_bytes(b"hello".to_vec()).await.expect("send");
        assert_eq!(recv_bytes(&mut channel).await, b"hello");
    }

    #[tokio::test]
    async fn spawn_fails_closed_on_existing_session() {
        let source = FakeSessionSource::new();
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let err = source
            .spawn(spec("api", "fix-login"))
            .await
            .expect_err("duplicate");
        assert!(matches!(err, SourceError::SessionExists { .. }));
    }

    #[tokio::test]
    async fn attach_emits_banner_then_echoes() {
        let source = FakeSessionSource::new();
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        let mut channel = source.attach(&project, &session).await.expect("attach");
        assert_eq!(
            recv_bytes(&mut channel).await,
            b"[fake attach api_fix-login]\r\n"
        );
        channel
            .send_bytes(b"still here".to_vec())
            .await
            .expect("send");
        assert_eq!(recv_bytes(&mut channel).await, b"still here");
    }

    #[tokio::test]
    async fn attach_to_unknown_or_stopped_session_fails() {
        let source = FakeSessionSource::new();
        let (project, session) = ids("api", "ghost");
        let err = source
            .attach(&project, &session)
            .await
            .expect_err("unknown");
        assert!(matches!(err, SourceError::SessionNotFound { .. }));

        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        source.stop_session(&project, &session);
        let err = source
            .attach(&project, &session)
            .await
            .expect_err("stopped");
        assert!(matches!(err, SourceError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn list_tracks_state_transitions() {
        let source = FakeSessionSource::new();
        assert!(source.list().await.expect("list").is_empty());

        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let listed = source.list().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, SessionState::Live);
        assert_eq!(listed[0].agent.as_deref(), Some("claude"));
        assert_eq!(listed[0].project_id.as_str(), "api");
        assert_eq!(listed[0].session_id.as_str(), "fix-login");

        let (project, session) = ids("api", "fix-login");
        source.stop_session(&project, &session);
        let listed = source.list().await.expect("list");
        assert_eq!(listed[0].state, SessionState::Stopped);
    }

    #[tokio::test]
    async fn resizes_are_recorded_across_channels() {
        let source = FakeSessionSource::new();
        let mut channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let first = TerminalSize::new(24, 80).expect("nonzero size");
        channel.resize(first).await.expect("resize");
        // Echo tasks consume input asynchronously; round-trip a byte on each
        // channel after its resize to know the resize was processed.
        channel.send_bytes(b"sync".to_vec()).await.expect("send");
        let _ = recv_bytes(&mut channel).await;

        let (project, session) = ids("api", "fix-login");
        let mut attached = source.attach(&project, &session).await.expect("attach");
        let _banner = recv_bytes(&mut attached).await;
        let second = TerminalSize::new(30, 100).expect("nonzero size");
        attached.resize(second).await.expect("resize");
        attached.send_bytes(b"sync".to_vec()).await.expect("send");
        let _ = recv_bytes(&mut attached).await;
        // Ordering between channels is not guaranteed; assert as a set.
        let mut got = source.resizes(&project, &session);
        got.sort_by_key(|s| (s.rows(), s.cols()));
        assert_eq!(got, vec![first, second]);
    }

    #[tokio::test]
    async fn kill_channels_kills_pipes_but_session_stays_live() {
        let source = FakeSessionSource::new();
        let mut channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");

        source.kill_channels(&project, &session);
        assert!(channel.recv().await.is_none());
        let err = channel
            .send_bytes(b"x".to_vec())
            .await
            .expect_err("dead channel");
        assert!(matches!(err, SourceError::ChannelClosed));

        // Session survives the disconnect: still listed Live, re-attachable.
        let listed = source.list().await.expect("list");
        assert_eq!(listed[0].state, SessionState::Live);
        let mut reattached = source.attach(&project, &session).await.expect("attach");
        assert_eq!(
            recv_bytes(&mut reattached).await,
            b"[fake attach api_fix-login]\r\n"
        );
    }

    #[tokio::test]
    async fn dropping_the_channel_does_not_poison_the_session() {
        let source = FakeSessionSource::new();
        let channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        drop(channel);

        let (project, session) = ids("api", "fix-login");
        let mut reattached = source.attach(&project, &session).await.expect("attach");
        reattached
            .send_bytes(b"alive".to_vec())
            .await
            .expect("send");
        let _banner = recv_bytes(&mut reattached).await;
        assert_eq!(recv_bytes(&mut reattached).await, b"alive");
    }

    #[tokio::test]
    async fn works_through_dyn_session_source() {
        let source: Box<dyn SessionSource> = Box::new(FakeSessionSource::new());
        let mut channel = source.spawn(spec("api", "dyn")).await.expect("spawn");
        channel.send_bytes(b"via dyn".to_vec()).await.expect("send");
        assert_eq!(recv_bytes(&mut channel).await, b"via dyn");
        assert_eq!(source.list().await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn spawn_fails_closed_on_stopped_session() {
        let source = FakeSessionSource::new();
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        source.stop_session(&project, &session);
        // A stopped session still occupies the name (ADR-0004 fail-closed).
        let err = source
            .spawn(spec("api", "fix-login"))
            .await
            .expect_err("stopped session still occupies the name");
        assert!(matches!(err, SourceError::SessionExists { .. }));
    }

    #[tokio::test]
    async fn list_orders_sessions_deterministically() {
        let source = FakeSessionSource::new();
        source.spawn(spec("web", "zeta")).await.expect("spawn");
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        source.spawn(spec("api", "add-tests")).await.expect("spawn");
        let listed = source.list().await.expect("list");
        let keys: Vec<(&str, &str)> = listed
            .iter()
            .map(|m| (m.project_id.as_str(), m.session_id.as_str()))
            .collect();
        assert_eq!(
            keys,
            vec![("api", "add-tests"), ("api", "fix-login"), ("web", "zeta")]
        );
    }

    #[tokio::test]
    async fn stop_session_kills_open_channels() {
        let source = FakeSessionSource::new();
        let mut channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");

        source.stop_session(&project, &session);
        assert!(channel.recv().await.is_none());
        let err = channel
            .send_bytes(b"x".to_vec())
            .await
            .expect_err("dead channel");
        assert!(matches!(err, SourceError::ChannelClosed));
    }

    #[tokio::test]
    async fn kill_channels_delivers_buffered_output_before_death() {
        let source = FakeSessionSource::new();
        let mut channel = source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");

        // Queue output before killing, so we exercise the buffered bytes
        // surviving the abort. On the current-thread runtime `#[tokio::test]`
        // uses (the crate enables no `rt-multi-thread` feature), a single
        // `yield_now` deterministically runs the woken echo task through one
        // recv→send cycle to its next `recv().await`, so the echo of
        // "buffered" is in the output queue before `kill_channels` aborts the
        // task. (Under a multi-thread runtime this would need a stronger
        // barrier, but the property — queued output is not lost when the
        // sender is dropped — can only be observed by leaving it unread.)
        channel
            .send_bytes(b"buffered".to_vec())
            .await
            .expect("send");
        tokio::task::yield_now().await;
        source.kill_channels(&project, &session);
        assert_eq!(recv_bytes(&mut channel).await, b"buffered");
        assert!(channel.recv().await.is_none());
    }

    #[tokio::test]
    async fn respawn_revives_a_stopped_session() {
        let source = FakeSessionSource::new();
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        source.stop_session(&project, &session);

        let mut channel = source
            .respawn(&project, &session, None)
            .await
            .expect("respawn");
        channel.send_bytes(b"alive".to_vec()).await.expect("send");
        assert_eq!(recv_bytes(&mut channel).await, b"alive");

        // Now Live again.
        let listed = source.list().await.expect("list");
        assert_eq!(listed[0].state, SessionState::Live);
    }

    #[tokio::test]
    async fn respawn_of_live_session_attaches() {
        let source = FakeSessionSource::new();
        source.spawn(spec("api", "fix-login")).await.expect("spawn");
        let (project, session) = ids("api", "fix-login");
        // Already live -> concurrent respawner attaches (banner).
        let mut channel = source
            .respawn(&project, &session, None)
            .await
            .expect("respawn");
        assert_eq!(
            recv_bytes(&mut channel).await,
            b"[fake attach api_fix-login]\r\n"
        );
    }

    #[tokio::test]
    async fn respawn_of_unknown_session_is_not_found() {
        let source = FakeSessionSource::new();
        let (project, session) = ids("api", "ghost");
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("unknown");
        assert!(matches!(err, SourceError::SessionNotFound { .. }));
    }

    #[tokio::test]
    async fn inherent_helpers_are_noops_for_unknown_sessions() {
        let source = FakeSessionSource::new();
        let (project, session) = ids("api", "ghost");
        source.stop_session(&project, &session);
        source.kill_channels(&project, &session);
        assert!(source.resizes(&project, &session).is_empty());
        assert!(source.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn full_input_queue_exerts_backpressure() {
        use remora_protocol::ChannelInput;

        // No echo task drains this pair, so the input queue fills and the
        // (CAPACITY + 1)th try_send is rejected — the bound counts messages.
        let (channel, _input_rx, _output_tx) = crate::SessionChannel::pair();
        for _ in 0..crate::CHANNEL_CAPACITY {
            channel
                .input
                .try_send(ChannelInput::Bytes(vec![0]))
                .expect("queue not yet full");
        }
        assert!(matches!(
            channel.input.try_send(ChannelInput::Bytes(vec![0])),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
    }
}
