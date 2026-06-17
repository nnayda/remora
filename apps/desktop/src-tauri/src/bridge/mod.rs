pub mod commands;
pub mod dto;
pub mod error;
pub mod output;
pub mod resolve;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use remora_core::config::{Config, ConfigError};
use remora_core::{SessionChannel, SessionSource, SourceError};

use remora_protocol::{
    AgentId, ChannelInput, ChannelOutput, ProjectId, SessionId, SpawnSpec, TerminalSize,
};
use resolve::SourceResolver;
use tokio::sync::{mpsc, oneshot};

use dto::ConfigDto;
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
    resolver: Arc<dyn SourceResolver>,
    channels: Registry,
    next_handle: AtomicU64,
    spawn_task: Spawner,
    /// Per-device config file (read-only). Resolved once at construction; read
    /// fresh on every `config()` so an external edit shows on manual refresh.
    config_path: PathBuf,
}

impl Bridge {
    /// Production: forward tasks run on Tauri's async runtime.
    pub fn new(resolver: Arc<dyn SourceResolver>, config_path: PathBuf) -> Self {
        Self::with_spawner(
            resolver,
            config_path,
            Arc::new(|fut| {
                tauri::async_runtime::spawn(fut);
            }),
        )
    }

    fn with_spawner(
        resolver: Arc<dyn SourceResolver>,
        config_path: PathBuf,
        spawn_task: Spawner,
    ) -> Self {
        Self {
            resolver,
            channels: Arc::new(Mutex::new(HashMap::new())),
            next_handle: AtomicU64::new(0),
            spawn_task,
            config_path,
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
        let source = self.resolve_for(&spec.project_id)?;
        let channel = source.spawn(spec).await?;
        Ok(self.open_channel(channel, sink))
    }

    pub async fn attach(
        &self,
        project_id: String,
        session_id: String,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let (p, s) = parse_ids(project_id, session_id)?;
        let source = self.resolve_for(&p)?;
        let channel = source.attach(&p, &s).await?;
        Ok(self.open_channel(channel, sink))
    }

    pub async fn respawn(
        &self,
        project_id: String,
        session_id: String,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let (p, s) = parse_ids(project_id, session_id)?;
        let source = self.resolve_for(&p)?;
        let channel = source.respawn(&p, &s).await?;
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
        let config = Arc::new(self.load_config()?);
        let sources = self.resolver.all(&config);
        let total = sources.len();
        let mut metas: Vec<SessionMetaDto> = Vec::new();
        let mut failed = 0usize;
        let mut last_err: Option<SourceError> = None;
        for source in sources {
            match source.list().await {
                Ok(ms) => metas.extend(ms.into_iter().map(Into::into)),
                // One host down must not blank the whole sidebar — skip it and
                // carry the partial result. TODO(stage 11+): surface per-host
                // availability instead of silently dropping a down host.
                Err(e) => {
                    failed += 1;
                    last_err = Some(e);
                }
            }
        }
        if total > 0 && failed == total {
            return Err(BridgeError::Transport {
                message: match last_err {
                    Some(e) => format!("all configured hosts are unreachable: {e}"),
                    None => "all configured hosts are unreachable".into(),
                },
            });
        }
        metas.sort_by(|a, b| {
            (a.project_id.as_str(), a.session_id.as_str())
                .cmp(&(b.project_id.as_str(), b.session_id.as_str()))
        });
        Ok(metas)
    }

    /// Resolve the transport for a project: load config fresh, then pick the
    /// project's host's source (per-call resolution, D1). Shared by
    /// spawn/attach/respawn so the load-then-resolve step lives in one place.
    fn resolve_for(&self, project_id: &ProjectId) -> Result<Arc<dyn SessionSource>, BridgeError> {
        let config = Arc::new(self.load_config()?);
        self.resolver.for_project(&config, project_id)
    }

    /// Load the per-device config fresh. A *missing* file is success → an
    /// empty config (a fresh device is valid, ADR-0004). Every other failure
    /// is a real `BridgeError::Config`.
    fn load_config(&self) -> Result<Config, BridgeError> {
        match Config::load(&self.config_path) {
            Ok(config) => Ok(config),
            Err(ConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Config::default())
            }
            Err(e) => Err(BridgeError::Config {
                message: e.to_string(),
            }),
        }
    }

    /// Reads + projects the per-device config for the sidebar.
    ///
    /// A *missing* file is success → an empty config (a fresh device is a
    /// valid configuration, ADR-0004). Every other failure — permission
    /// denied, not-a-regular-file, oversized, parse error, validation error —
    /// is a real `BridgeError::Config` so the UI shows a banner rather than a
    /// silently-empty sidebar.
    pub fn config(&self) -> Result<ConfigDto, BridgeError> {
        Ok(self.load_config()?.into())
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
    use remora_core::{FakeSessionSource, SessionSource, SourceError};
    use remora_protocol::{SessionMeta, SessionState};

    use super::resolve::SourceResolver;

    /// Test resolver: returns one fixed source for every project, and as the
    /// sole element of `all()`. Lets the existing single-source tests run
    /// unchanged through the per-call resolution path.
    struct FixedResolver(Arc<dyn SessionSource>);
    impl SourceResolver for FixedResolver {
        fn for_project(
            &self,
            _config: &Arc<Config>,
            _project_id: &ProjectId,
        ) -> Result<Arc<dyn SessionSource>, BridgeError> {
            Ok(Arc::clone(&self.0))
        }
        fn all(&self, _config: &Arc<Config>) -> Vec<Arc<dyn SessionSource>> {
            vec![Arc::clone(&self.0)]
        }
    }

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
        // Non-config tests don't touch config(); point at a path that does not
        // exist so an accidental read would be an obvious empty config.
        bridge_with_config(
            source,
            std::env::temp_dir().join("remora-no-such-config.toml"),
        )
    }
    fn bridge_with_config(source: Arc<dyn SessionSource>, config_path: PathBuf) -> Bridge {
        Bridge::with_spawner(
            Arc::new(FixedResolver(source)),
            config_path,
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        )
    }
    /// Unique temp path per process so concurrent `cargo test` runs don't
    /// collide (matches the `remora-config-test-{pid}` convention in core).
    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "remora-bridge-cfg-{}-{}.toml",
            tag,
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn config_missing_file_is_empty() {
        let b = bridge_with_config(
            Arc::new(FakeSessionSource::new()),
            temp_config_path("missing").join("definitely-absent.toml"),
        );
        let dto = b
            .config()
            .expect("missing file is an empty config, not an error");
        assert!(dto.hosts.is_empty() && dto.projects.is_empty());
    }

    #[tokio::test]
    async fn config_unreadable_is_config_error() {
        // A directory at the path: metadata succeeds but it is not a regular
        // file → ConfigError::Io with kind != NotFound → must surface as Config.
        let b = bridge_with_config(Arc::new(FakeSessionSource::new()), std::env::temp_dir());
        assert!(matches!(b.config(), Err(BridgeError::Config { .. })));
    }

    #[tokio::test]
    async fn config_malformed_is_config_error() {
        let path = temp_config_path("malformed");
        std::fs::write(&path, "this is not = valid = toml =").expect("write");
        let result = b_config(&path);
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(BridgeError::Config { .. })));
    }

    #[tokio::test]
    async fn config_valid_returns_projected_dtos() {
        let path = temp_config_path("valid");
        std::fs::write(
            &path,
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"h\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("write");
        let result = b_config(&path);
        std::fs::remove_file(&path).ok();
        let dto = result.expect("valid config loads");
        assert_eq!(dto.hosts.len(), 1);
        assert_eq!(dto.hosts[0].id, "devbox");
        assert_eq!(dto.projects.len(), 1);
        assert_eq!(dto.projects[0].host_id, "devbox");
    }

    /// Loads config via a bridge pointed at `path` (keeps the temp-file cleanup
    /// in the caller so a panic still removes the file).
    fn b_config(path: &std::path::Path) -> Result<ConfigDto, BridgeError> {
        bridge_with_config(Arc::new(FakeSessionSource::new()), path.to_path_buf()).config()
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

    /// Records which project_id `for_project` was called with, and returns a
    /// per-project source so a test can prove the bridge routed correctly.
    struct RecordingResolver {
        seen: Arc<Mutex<Vec<String>>>,
        source: Arc<dyn SessionSource>,
    }
    impl super::resolve::SourceResolver for RecordingResolver {
        fn for_project(
            &self,
            _c: &Arc<Config>,
            project_id: &ProjectId,
        ) -> Result<Arc<dyn SessionSource>, BridgeError> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(project_id.as_str().to_string());
            Ok(Arc::clone(&self.source))
        }
        fn all(&self, _c: &Arc<Config>) -> Vec<Arc<dyn SessionSource>> {
            vec![Arc::clone(&self.source)]
        }
    }

    #[tokio::test]
    async fn spawn_routes_through_for_project_with_the_project_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let b = Bridge::with_spawner(
            Arc::new(RecordingResolver {
                seen: Arc::clone(&seen),
                source: Arc::new(FakeSessionSource::new()),
            }),
            std::env::temp_dir().join("remora-routing.toml"),
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        );
        let (s, _rx) = sink();
        b.spawn("api".into(), "x".into(), None, s)
            .await
            .expect("spawn");
        assert_eq!(
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_slice(),
            &["api".to_string()]
        );
    }

    /// A resolver whose `all()` returns two sources: one live with a session,
    /// one that always errors on list(). The bridge must return the live one's
    /// sessions (partial), not error.
    struct TwoHostResolver;
    struct ErroringSource;
    #[async_trait]
    impl SessionSource for ErroringSource {
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
            Err(SourceError::Transport("host down".into()))
        }
    }
    impl super::resolve::SourceResolver for TwoHostResolver {
        fn for_project(
            &self,
            _c: &Arc<Config>,
            _p: &ProjectId,
        ) -> Result<Arc<dyn SessionSource>, BridgeError> {
            unreachable!()
        }
        fn all(&self, _c: &Arc<Config>) -> Vec<Arc<dyn SessionSource>> {
            vec![Arc::new(ReverseSource), Arc::new(ErroringSource)]
        }
    }

    #[tokio::test]
    async fn list_tolerates_one_host_down_and_returns_partial() {
        let b = Bridge::with_spawner(
            Arc::new(TwoHostResolver),
            std::env::temp_dir().join("remora-twohost.toml"),
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        );
        let listed = b.list().await.expect("partial result, not an error");
        // ReverseSource contributes 2 sessions; ErroringSource contributes none.
        assert_eq!(listed.len(), 2);
    }

    /// All hosts down → the sidebar should see discovery-unavailable, so the
    /// bridge errors rather than reporting an empty (no-sessions) world.
    struct AllDownResolver;
    impl super::resolve::SourceResolver for AllDownResolver {
        fn for_project(
            &self,
            _c: &Arc<Config>,
            _p: &ProjectId,
        ) -> Result<Arc<dyn SessionSource>, BridgeError> {
            unreachable!()
        }
        fn all(&self, _c: &Arc<Config>) -> Vec<Arc<dyn SessionSource>> {
            vec![Arc::new(ErroringSource)]
        }
    }

    #[tokio::test]
    async fn list_errors_when_every_host_is_down() {
        let b = Bridge::with_spawner(
            Arc::new(AllDownResolver),
            std::env::temp_dir().join("remora-alldown.toml"),
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        );
        let err = b.list().await.expect_err("all hosts down should error");
        match err {
            BridgeError::Transport { message } => assert!(
                message.contains("host down"),
                "message should carry the host's cause, got: {message}"
            ),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    // End-to-end through the REAL ConfigResolver (the routing tests above use
    // fakes): a spawn for a project absent from config surfaces as Config, not
    // Transport — proving load_config → for_project is wired and classified.
    #[tokio::test]
    async fn spawn_unknown_project_is_config_error() {
        let path = temp_config_path("unknown-project");
        std::fs::write(
            &path,
            "[hosts.hermes]\ntransport = \"ssh\"\nhost = \"hermes\"\n\
             [projects.api]\nhost = \"hermes\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("write");
        let b = Bridge::with_spawner(
            Arc::new(super::resolve::ConfigResolver),
            path.clone(),
            Arc::new(|fut| {
                tokio::spawn(fut);
            }),
        );
        let (s, _rx) = sink();
        let result = b.spawn("ghost".into(), "x".into(), None, s).await;
        std::fs::remove_file(&path).ok();
        assert!(matches!(result, Err(BridgeError::Config { .. })));
    }
}
