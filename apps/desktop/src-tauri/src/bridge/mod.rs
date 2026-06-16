pub mod commands;
pub mod error;
pub mod output;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use remora_core::{SessionChannel, SessionSource};
use remora_protocol::{
    AgentId, ChannelInput, ChannelOutput, ProjectId, SessionId, SpawnSpec, TerminalSize,
};
use tokio::sync::{mpsc, oneshot};

use error::{BridgeError, SessionMetaDto};
use output::{BridgeOutput, ChannelHandle, OutputSink};

type Registry = Arc<Mutex<HashMap<u64, OpenChannel>>>;
type Spawner = Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>;

struct OpenChannel {
    input: mpsc::Sender<ChannelInput>,
    cancel: oneshot::Sender<()>,
}

/// Owns the transport(s) and the open-channel registry. UI talks only to this.
pub struct Bridge {
    source: Arc<dyn SessionSource>,
    channels: Registry,
    next_handle: AtomicU64,
    spawn_task: Spawner,
}

impl Bridge {
    /// Production: forward tasks run on Tauri's async runtime.
    pub fn new(source: Arc<dyn SessionSource>) -> Self {
        Self::with_spawner(
            source,
            Arc::new(|fut| {
                tauri::async_runtime::spawn(fut);
            }),
        )
    }

    fn with_spawner(source: Arc<dyn SessionSource>, spawn_task: Spawner) -> Self {
        Self {
            source,
            channels: Arc::new(Mutex::new(HashMap::new())),
            next_handle: AtomicU64::new(0),
            spawn_task,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<u64, OpenChannel>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register-before-spawn (7A): insert the entry, THEN launch the forward
    /// task. Self-deregister and close() are both keyed by the unique handle.
    fn open_channel(&self, channel: SessionChannel, sink: Arc<dyn OutputSink>) -> ChannelHandle {
        let SessionChannel { input, output } = channel;
        let handle = ChannelHandle(self.next_handle.fetch_add(1, Ordering::SeqCst));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.lock().insert(
            handle.0,
            OpenChannel {
                input,
                cancel: cancel_tx,
            },
        );
        let registry = Arc::clone(&self.channels);
        (self.spawn_task)(Box::pin(forward(
            output, sink, registry, handle.0, cancel_rx,
        )));
        handle
    }

    pub async fn spawn(
        &self,
        project_id: String,
        session_id: String,
        agent: Option<String>,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let spec = SpawnSpec {
            project_id: parse_id(ProjectId::new(project_id))?,
            session_id: parse_id(SessionId::new(session_id))?,
            agent: agent
                .map(AgentId::new)
                .transpose()
                .map_err(|e| BridgeError::InvalidId {
                    message: e.to_string(),
                })?,
        };
        let channel = self.source.spawn(spec).await?;
        Ok(self.open_channel(channel, sink))
    }

    pub async fn attach(
        &self,
        project_id: String,
        session_id: String,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let (p, s) = parse_ids(project_id, session_id)?;
        let channel = self.source.attach(&p, &s).await?;
        Ok(self.open_channel(channel, sink))
    }

    pub async fn respawn(
        &self,
        project_id: String,
        session_id: String,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let (p, s) = parse_ids(project_id, session_id)?;
        let channel = self.source.respawn(&p, &s).await?;
        Ok(self.open_channel(channel, sink))
    }

    pub async fn write(&self, handle: ChannelHandle, bytes: Vec<u8>) -> Result<(), BridgeError> {
        let sender = self.lock().get(&handle.0).map(|c| c.input.clone());
        match sender {
            Some(input) => input
                .send(ChannelInput::Bytes(bytes))
                .await
                .map_err(|_| BridgeError::ChannelClosed),
            None => Err(BridgeError::UnknownHandle),
        }
    }

    pub async fn resize(
        &self,
        handle: ChannelHandle,
        rows: u16,
        cols: u16,
    ) -> Result<(), BridgeError> {
        let size = TerminalSize::new(rows, cols).map_err(|e| BridgeError::InvalidSize {
            message: e.to_string(),
        })?;
        let sender = self.lock().get(&handle.0).map(|c| c.input.clone());
        match sender {
            Some(input) => input
                .send(ChannelInput::Resize(size))
                .await
                .map_err(|_| BridgeError::ChannelClosed),
            None => Err(BridgeError::UnknownHandle),
        }
    }

    /// Local teardown (no protocol counterpart): drop this client's ends.
    /// Returning is the authoritative local-teardown signal — the frontend must
    /// NOT wait for a `closed` event after `session_close`. Idempotent.
    pub fn close(&self, handle: ChannelHandle) {
        if let Some(ch) = self.lock().remove(&handle.0) {
            let _ = ch.cancel.send(());
        }
    }

    /// Sorted by (project_id, session_id) for a transport-stable UI contract.
    pub async fn list(&self) -> Result<Vec<SessionMetaDto>, BridgeError> {
        let mut metas: Vec<SessionMetaDto> = self
            .source
            .list()
            .await?
            .into_iter()
            .map(Into::into)
            .collect();
        metas.sort_by(|a, b| {
            (a.project_id.as_str(), a.session_id.as_str())
                .cmp(&(b.project_id.as_str(), b.session_id.as_str()))
        });
        Ok(metas)
    }
}

fn parse_id<T>(r: Result<T, remora_protocol::InvalidIdError>) -> Result<T, BridgeError> {
    r.map_err(|e| BridgeError::InvalidId {
        message: e.to_string(),
    })
}

fn parse_ids(p: String, s: String) -> Result<(ProjectId, SessionId), BridgeError> {
    Ok((parse_id(ProjectId::new(p))?, parse_id(SessionId::new(s))?))
}

/// Pumps PTY output to the sink. On transport death OR frontend-gone (sink
/// error) it attempts to emit `Closed` (unless `close()` raced in after the
/// loop exited — see cancel guard below). On cancel (`close()`) it is always
/// silent. Always self-deregisters by handle.
async fn forward(
    mut output: mpsc::Receiver<ChannelOutput>,
    sink: Arc<dyn OutputSink>,
    registry: Registry,
    handle: u64,
    mut cancel: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            // Biased: cancel wins when both are ready. close() drops the input
            // sender, which ends the transport's output too, so cancel and
            // `recv() -> None` can race on one tick; without bias we could emit
            // a spurious Closed after a close() that is contracted to be silent.
            biased;
            // close() already removed the registry entry before signalling
            // cancel, so there is nothing to deregister here — just stop silently.
            _ = &mut cancel => return,
            msg = output.recv() => match msg {
                Some(ChannelOutput::Bytes(bytes)) => {
                    if sink.send(BridgeOutput::Bytes { bytes }).is_err() {
                        break;
                    }
                }
                Some(_) => {}        // ChannelOutput is #[non_exhaustive]
                None => break,        // transport death
            }
        }
    }
    // The loop exited via the output arm (transport death or frontend-gone).
    // If close() fired the cancel meanwhile (death raced close() OUTSIDE the
    // select), stay silent — close() is contracted to emit nothing. Otherwise
    // this is a genuine death: tell the frontend.
    match cancel.try_recv() {
        Ok(()) => {}
        Err(_) => {
            let _ = sink.send(BridgeOutput::Closed);
        }
    }
    deregister(&registry, handle);
}

fn deregister(registry: &Registry, handle: u64) {
    registry
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use remora_core::{FakeSessionSource, SourceError};
    use remora_protocol::{SessionMeta, SessionState};

    // mpsc sink: collect output AND simulate frontend-gone (drop the receiver).
    struct ChanSink(mpsc::UnboundedSender<BridgeOutput>);
    impl OutputSink for ChanSink {
        fn send(&self, msg: BridgeOutput) -> Result<(), output::SinkClosed> {
            self.0.send(msg).map_err(|_| output::SinkClosed)
        }
    }
    fn sink() -> (Arc<dyn OutputSink>, mpsc::UnboundedReceiver<BridgeOutput>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ChanSink(tx)), rx)
    }
    fn bridge(source: Arc<dyn SessionSource>) -> Bridge {
        Bridge::with_spawner(
            source,
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        )
    }
    /// Polls up to ~500ms for the forward task to self-deregister `h`
    /// (write then returns UnknownHandle). Panics with `ctx` on timeout.
    async fn wait_for_deregister(b: &Bridge, h: ChannelHandle, ctx: &str) {
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if matches!(
                b.write(h, b"probe".to_vec()).await,
                Err(BridgeError::UnknownHandle)
            ) {
                return;
            }
        }
        panic!("{ctx}");
    }

    async fn next_bytes(rx: &mut mpsc::UnboundedReceiver<BridgeOutput>) -> Vec<u8> {
        match rx.recv().await {
            Some(BridgeOutput::Bytes { bytes }) => bytes,
            other => panic!("expected bytes, got {other:?}"),
        }
    }
    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("slug")
    }
    fn sid(s: &str) -> SessionId {
        SessionId::new(s).expect("slug")
    }

    #[tokio::test]
    async fn spawn_then_write_echoes() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, mut rx) = sink();
        let h = b
            .spawn("api".into(), "fix".into(), Some("claude".into()), s)
            .await
            .expect("spawn");
        b.write(h, b"hello".to_vec()).await.expect("write");
        assert_eq!(next_bytes(&mut rx).await, b"hello");
    }

    #[tokio::test]
    async fn attach_emits_banner_then_echoes() {
        let src = Arc::new(FakeSessionSource::new());
        let (s0, _r0) = sink();
        bridge(src.clone())
            .spawn("api".into(), "x".into(), None, s0)
            .await
            .expect("spawn");
        let b = bridge(src);
        let (s, mut rx) = sink();
        let h = b.attach("api".into(), "x".into(), s).await.expect("attach");
        assert_eq!(next_bytes(&mut rx).await, b"[fake attach api_x]\r\n");
        b.write(h, b"hi".to_vec()).await.expect("write");
        assert_eq!(next_bytes(&mut rx).await, b"hi");
    }

    #[tokio::test]
    async fn close_stops_forwarding_and_is_idempotent() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, mut rx) = sink();
        let h = b
            .spawn("api".into(), "x".into(), None, s)
            .await
            .expect("spawn");
        b.close(h);
        b.close(h); // no panic
        assert!(matches!(
            b.write(h, b"x".to_vec()).await,
            Err(BridgeError::UnknownHandle)
        ));
        // close() is contracted to be silent: no Closed event reaches the frontend
        // (the biased select gives cancel priority over the racing transport death).
        // After the forward task ends it drops the sink, so the receiver is
        // Disconnected rather than Empty — both mean "nothing was delivered";
        // only an Ok(_) would prove a (forbidden) message was sent.
        // One yield_now is deterministic on the current-thread runtime
        // `#[tokio::test]` uses: it runs the woken forward task through the
        // cancel arm to completion before control returns here.
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "close() must not emit any output");
    }

    #[tokio::test]
    async fn sink_error_stops_forwarding() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, rx) = sink();
        let h = b
            .spawn("api".into(), "x".into(), None, s)
            .await
            .expect("spawn");
        drop(rx); // frontend gone
                  // Trigger output so the forward task attempts a send that fails.
        let _ = b.write(h, b"x".to_vec()).await;
        // Let the forward task observe the failed send and self-deregister.
        wait_for_deregister(&b, h, "forward task did not deregister after sink failure").await;
    }

    #[tokio::test]
    async fn resize_zero_is_invalid_size() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, _rx) = sink();
        let h = b
            .spawn("api".into(), "x".into(), None, s)
            .await
            .expect("spawn");
        assert!(matches!(
            b.resize(h, 0, 80).await,
            Err(BridgeError::InvalidSize { .. })
        ));
        assert!(matches!(
            b.resize(h, 24, 0).await,
            Err(BridgeError::InvalidSize { .. })
        ));
    }

    #[tokio::test]
    async fn write_unknown_handle() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        assert!(matches!(
            b.write(ChannelHandle(999), b"x".to_vec()).await,
            Err(BridgeError::UnknownHandle)
        ));
    }

    #[tokio::test]
    async fn invalid_id_is_rejected() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, _rx) = sink();
        assert!(matches!(
            b.spawn("API".into(), "x".into(), None, s).await,
            Err(BridgeError::InvalidId { .. })
        ));
    }

    #[tokio::test]
    async fn duplicate_spawn_is_session_exists() {
        let src = Arc::new(FakeSessionSource::new());
        let (s1, _r1) = sink();
        bridge(src.clone())
            .spawn("api".into(), "x".into(), None, s1)
            .await
            .expect("spawn");
        let (s2, _r2) = sink();
        assert!(matches!(
            bridge(src).spawn("api".into(), "x".into(), None, s2).await,
            Err(BridgeError::SessionExists { .. })
        ));
    }

    #[tokio::test]
    async fn respawn_revives_stopped() {
        let src = Arc::new(FakeSessionSource::new());
        src.spawn(SpawnSpec {
            project_id: pid("api"),
            session_id: sid("x"),
            agent: None,
        })
        .await
        .expect("spawn");
        src.stop_session(&pid("api"), &sid("x"));
        let b = bridge(src);
        let (s, mut rx) = sink();
        let h = b
            .respawn("api".into(), "x".into(), s)
            .await
            .expect("respawn");
        b.write(h, b"alive".to_vec()).await.expect("write");
        assert_eq!(next_bytes(&mut rx).await, b"alive");
    }

    // Reverse-order source proves the bridge sorts (the fake already sorts, hiding it).
    struct ReverseSource;
    #[async_trait]
    impl SessionSource for ReverseSource {
        async fn spawn(&self, _: SpawnSpec) -> Result<SessionChannel, SourceError> {
            unreachable!()
        }
        async fn attach(
            &self,
            _: &ProjectId,
            _: &SessionId,
        ) -> Result<SessionChannel, SourceError> {
            unreachable!()
        }
        async fn respawn(
            &self,
            _: &ProjectId,
            _: &SessionId,
        ) -> Result<SessionChannel, SourceError> {
            unreachable!()
        }
        async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
            Ok(vec![
                SessionMeta {
                    project_id: pid("zeta"),
                    session_id: sid("b"),
                    state: SessionState::Live,
                    agent: None,
                    created_at: None,
                    workspace_path: None,
                },
                SessionMeta {
                    project_id: pid("api"),
                    session_id: sid("a"),
                    state: SessionState::Live,
                    agent: None,
                    created_at: None,
                    workspace_path: None,
                },
            ])
        }
    }
    #[tokio::test]
    async fn list_is_sorted_by_bridge() {
        let b = bridge(Arc::new(ReverseSource));
        let listed = b.list().await.expect("list");
        assert_eq!(
            (listed[0].project_id.as_str(), listed[1].project_id.as_str()),
            ("api", "zeta")
        );
    }

    #[tokio::test]
    async fn resize_unknown_handle() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        assert!(matches!(
            b.resize(ChannelHandle(999), 24, 80).await,
            Err(BridgeError::UnknownHandle)
        ));
    }

    #[tokio::test]
    async fn attach_and_respawn_reject_invalid_ids() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s1, _r1) = sink();
        assert!(matches!(
            b.attach("API".into(), "x".into(), s1).await,
            Err(BridgeError::InvalidId { .. })
        ));
        let (s2, _r2) = sink();
        assert!(matches!(
            b.respawn("API".into(), "x".into(), s2).await,
            Err(BridgeError::InvalidId { .. })
        ));
    }

    #[tokio::test]
    async fn attach_unknown_session_is_not_found() {
        let b = bridge(Arc::new(FakeSessionSource::new()));
        let (s, _rx) = sink();
        assert!(matches!(
            b.attach("api".into(), "ghost".into(), s).await,
            Err(BridgeError::SessionNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn natural_death_emits_closed_and_deregisters() {
        let src = Arc::new(FakeSessionSource::new());
        let (s, mut rx) = sink();
        let b = bridge(src.clone());
        let h = b
            .spawn("api".into(), "x".into(), None, s)
            .await
            .expect("spawn");
        src.stop_session(&pid("api"), &sid("x")); // kills the channel
        loop {
            match rx.recv().await {
                Some(BridgeOutput::Closed) => break,
                Some(_) => continue,
                None => panic!("no Closed emitted"),
            }
        }
        wait_for_deregister(&b, h, "forward task did not deregister after natural death").await;
    }
}
