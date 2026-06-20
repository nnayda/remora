pub mod commands;
pub mod dto;
pub mod editor_dto;
pub mod error;
pub mod output;
pub mod resolve;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use remora_core::config::{Config, ConfigDocument, ConfigError, HostId};
use remora_core::{SessionChannel, SessionSource, SourceError};

use remora_protocol::{
    AgentId, ChannelInput, ChannelOutput, ProjectId, SessionId, SpawnSpec, TerminalSize,
};
use resolve::SourceResolver;
use tokio::sync::{mpsc, oneshot};

use dto::ConfigDto;
use editor_dto::{
    AgentInputDto, EditableConfigDto, EditorConfigDto, HostInputDto, ProjectInputDto,
};
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
    /// Per-device config file. Resolved once at construction; read fresh on
    /// every `config()`/`config_editable()` so an external edit shows on
    /// refresh, and rewritten in place by the editor commands.
    config_path: PathBuf,
    /// Serializes the editor channel's load → mutate → save critical section so
    /// overlapping in-process mutations cannot lose updates (eng review #2).
    /// Read paths don't take it — `save` is atomic (temp + rename), so a reader
    /// always sees a whole file. Cross-process is out of scope until the relay.
    config_mutex: tokio::sync::Mutex<()>,
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
            config_mutex: tokio::sync::Mutex::new(()),
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
        agent: Option<String>,
        sink: Arc<dyn OutputSink>,
    ) -> Result<ChannelHandle, BridgeError> {
        let (p, s) = parse_ids(project_id, session_id)?;
        let agent = agent
            .map(AgentId::new)
            .transpose()
            .map_err(|e| BridgeError::InvalidId {
                message: e.to_string(),
            })?;
        let source = self.resolve_for(&p)?;
        let channel = source.respawn(&p, &s, agent).await?;
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

    // ---- Editor channel (local-only, un-redacted) ----

    /// The full, **un-redacted** config for the settings forms (counterpart to
    /// `config()`), plus its validation state for degraded-mode recovery
    /// (ADR-0006). Local-only: it carries connection secrets and must never
    /// cross the relay.
    ///
    /// A *missing* file is an empty (valid) config. A *valid* file yields
    /// `config: Some` with no issues. A file that deserializes but is
    /// *semantically* invalid yields `config: None` plus the issues and the
    /// present entry ids, so the UI can delete the offending entries. A file
    /// that is unreadable or unparseable is a `Config` load error (banner).
    pub fn config_editable(&self) -> Result<EditableConfigDto, BridgeError> {
        let raw = self.read_config_string()?;
        match ConfigDocument::parse_lenient(&raw) {
            Ok((doc, issues)) => Ok(EditableConfigDto {
                // A strict (valid) doc yields a typed config; a degraded one
                // can't, so `config` is None and `issues`/`present` drive the
                // recovery UI instead.
                config: doc.config().ok().map(EditorConfigDto::from),
                issues: issues.iter().map(ToString::to_string).collect(),
                present: doc.present_ids().into(),
            }),
            // A TOML grammar error or a structural/type error carries no
            // per-entry issue the editor could repair — surface it as a load
            // banner rather than a degraded doc that pretends to have parsed.
            Err(e) => Err(BridgeError::Config {
                message: e.to_string(),
            }),
        }
    }

    pub async fn config_insert_host(
        &self,
        id: String,
        input: HostInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(HostId::new(id))?;
        let host = input.into_host();
        self.mutate(|doc| doc.insert_host(&id, &host)).await
    }

    pub async fn config_update_host(
        &self,
        id: String,
        input: HostInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(HostId::new(id))?;
        let host = input.into_host();
        self.mutate(|doc| doc.update_host(&id, &host)).await
    }

    pub async fn config_remove_host(&self, id: String) -> Result<(), BridgeError> {
        let id = parse_id(HostId::new(id))?;
        self.mutate(|doc| doc.remove_host(&id)).await
    }

    pub async fn config_insert_project(
        &self,
        id: String,
        input: ProjectInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(ProjectId::new(id))?;
        let project = input.into_project()?;
        self.mutate(|doc| doc.insert_project(&id, &project)).await
    }

    pub async fn config_update_project(
        &self,
        id: String,
        input: ProjectInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(ProjectId::new(id))?;
        let project = input.into_project()?;
        self.mutate(|doc| doc.update_project(&id, &project)).await
    }

    pub async fn config_remove_project(&self, id: String) -> Result<(), BridgeError> {
        let id = parse_id(ProjectId::new(id))?;
        self.mutate(|doc| doc.remove_project(&id)).await
    }

    pub async fn config_insert_agent(
        &self,
        id: String,
        input: AgentInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(AgentId::new(id))?;
        let agent = input.into_agent();
        self.mutate(|doc| doc.insert_agent(&id, &agent)).await
    }

    pub async fn config_update_agent(
        &self,
        id: String,
        input: AgentInputDto,
    ) -> Result<(), BridgeError> {
        let id = parse_id(AgentId::new(id))?;
        let agent = input.into_agent();
        self.mutate(|doc| doc.update_agent(&id, &agent)).await
    }

    pub async fn config_remove_agent(&self, id: String) -> Result<(), BridgeError> {
        let id = parse_id(AgentId::new(id))?;
        self.mutate(|doc| doc.remove_agent(&id)).await
    }

    /// The editor critical section: serialize on the mutex, then load → mutate →
    /// save. Reading fresh inside the lock means each mutation sees the prior
    /// one's result (no stale cache). A rejected mutation or a save failure
    /// surfaces as `ConfigEdit`; only a pre-mutation *read* failure (the file is
    /// unreadable) is a `Config` load error.
    async fn mutate(
        &self,
        edit: impl FnOnce(&mut ConfigDocument) -> Result<(), ConfigError>,
    ) -> Result<(), BridgeError> {
        let _guard = self.config_mutex.lock().await;
        let mut doc = self.read_document()?;
        edit(&mut doc).map_err(config_edit)?;
        doc.save(&self.config_path).map_err(config_edit)?;
        Ok(())
    }

    /// Reads the config file into an editable document. A *missing* file is an
    /// empty document (a fresh device edits from scratch); an unreadable file is
    /// a `Config` load error.
    ///
    /// Reads **leniently** so degraded-mode recovery (deleting the entry that
    /// breaks the file) can operate on a semantically-invalid base. A valid base
    /// still yields a *strict* document, so normal edits re-validate exactly as
    /// before; a degraded base yields a document whose edits skip per-edit
    /// validation but whose `save` still refuses to persist an invalid result
    /// (so a delete that doesn't fully recover the file is rejected, never
    /// bricking it). A grammar/structural error carries nothing to repair
    /// entry-by-entry, so it blocks the edit (`ConfigEdit`).
    fn read_document(&self) -> Result<ConfigDocument, BridgeError> {
        let raw = self.read_config_string()?;
        ConfigDocument::parse_lenient(&raw)
            .map(|(doc, _issues)| doc)
            .map_err(config_edit)
    }

    /// Reads the config file to a string with the same guards as [`Config::load`]
    /// — a *missing* file is an empty string (fresh device); a non-regular file
    /// (FIFO/dir/device) or an implausibly large one is refused — so a hostile
    /// config path can't hang or OOM the read (which may hold `config_mutex`).
    /// Shared by [`Self::read_document`] (mutations) and [`Self::config_editable`]
    /// (the degraded-aware read).
    fn read_config_string(&self) -> Result<String, BridgeError> {
        let path = &self.config_path;
        let config_err = |msg: String| BridgeError::Config { message: msg };
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(config_err(format!(
                        "cannot read config file `{}`: not a regular file",
                        path.display()
                    )));
                }
                if meta.len() > MAX_EDIT_CONFIG_BYTES {
                    return Err(config_err(format!(
                        "config file `{}` is {} bytes; refusing to read more than {MAX_EDIT_CONFIG_BYTES}",
                        path.display(),
                        meta.len()
                    )));
                }
                std::fs::read_to_string(path).map_err(|e| {
                    config_err(format!("cannot read config file `{}`: {e}", path.display()))
                })
            }
            // A missing file is a fresh device: edit from an empty document.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(config_err(format!(
                "cannot read config file `{}`: {e}",
                path.display()
            ))),
        }
    }
}

/// Upper bound on a plausible hand-edited config, mirroring the private cap in
/// [`Config::load`]. Keep in step with it.
const MAX_EDIT_CONFIG_BYTES: u64 = 1024 * 1024;

/// Rejected mutations carry the rendered `ConfigError` (already sanitized via
/// `config.rs`); the frontend shows it inline on the form.
fn config_edit(e: ConfigError) -> BridgeError {
    BridgeError::ConfigEdit {
        message: e.to_string(),
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
            .respawn("api".into(), "x".into(), None, s)
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
            _: Option<AgentId>,
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
            b.respawn("API".into(), "x".into(), None, s2).await,
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
            temp_config_path("routing"),
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
            _: Option<AgentId>,
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
            temp_config_path("twohost"),
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
            temp_config_path("alldown"),
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

    #[tokio::test]
    async fn repeated_reconnect_closes_each_prior_channel() {
        // Simulate the store's reconnect swap: open, close, open again — each open
        // must register a fresh handle and each close must deregister the prior so
        // no handle (and thus no underlying ssh child / forward task) leaks.
        let src = Arc::new(FakeSessionSource::new());
        src.spawn(SpawnSpec {
            project_id: pid("api"),
            session_id: sid("x"),
            agent: None,
        })
        .await
        .expect("spawn");
        let b = bridge(src);
        let (s1, _r1) = sink();
        let h1 = b
            .attach("api".into(), "x".into(), s1)
            .await
            .expect("attach 1");
        b.close(h1);
        wait_for_deregister(&b, h1, "first channel did not deregister after close").await;
        let (s2, _r2) = sink();
        let h2 = b
            .attach("api".into(), "x".into(), s2)
            .await
            .expect("attach 2");
        assert_ne!(h1.0, h2.0, "reconnect must use a fresh handle");
        b.write(h2, b"alive".to_vec())
            .await
            .expect("new channel writable");
    }

    // ---- Editor channel (PR2): config mutation commands ----
    use super::editor_dto::{
        AgentInputDto, HostInputDto, ProjectInputDto, TransportDto, WorkspaceModeDto,
    };

    fn ssh_input() -> HostInputDto {
        HostInputDto {
            name: Some("Dev box".into()),
            transport: TransportDto::Ssh {
                host: "secret-hostname".into(),
                user: Some("rootuser".into()),
                port: Some(2222),
            },
        }
    }
    fn project_input() -> ProjectInputDto {
        ProjectInputDto {
            name: Some("API".into()),
            host_id: "devbox".into(),
            path: "/srv/api".into(),
            workspace: WorkspaceModeDto::Worktree,
            agent: "claude".into(),
        }
    }
    fn agent_input() -> AgentInputDto {
        AgentInputDto {
            command: vec!["claude".into(), "--flag".into()],
        }
    }
    /// A bridge over a guaranteed-absent temp config file (fresh-device start).
    fn editor_bridge(tag: &str) -> (Bridge, PathBuf) {
        let path = temp_config_path(tag);
        std::fs::remove_file(&path).ok();
        let b = bridge_with_config(Arc::new(FakeSessionSource::new()), path.clone());
        (b, path)
    }

    #[tokio::test]
    async fn all_insert_commands_persist_and_are_editable() {
        let (b, path) = editor_bridge("insert-all");
        // Order matters: a project references its host + agent, which must exist
        // first (re-validation enforces it).
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("insert host");
        b.config_insert_agent("claude".into(), agent_input())
            .await
            .expect("insert agent");
        b.config_insert_project("api".into(), project_input())
            .await
            .expect("insert project");
        let dto = b
            .config_editable()
            .expect("editable read")
            .config
            .expect("valid base");
        std::fs::remove_file(&path).ok();
        assert!(dto.hosts.iter().any(|h| h.id == "devbox"), "{dto:?}");
        assert!(dto.agents.iter().any(|a| a.id == "claude"), "{dto:?}");
        assert!(dto.projects.iter().any(|p| p.id == "api"), "{dto:?}");
    }

    #[tokio::test]
    async fn update_commands_edit_in_place() {
        let (b, path) = editor_bridge("update-all");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("insert host");
        b.config_insert_agent("claude".into(), agent_input())
            .await
            .expect("insert agent");
        let mut renamed = ssh_input();
        renamed.name = Some("Renamed".into());
        b.config_update_host("devbox".into(), renamed)
            .await
            .expect("update host");
        b.config_update_agent(
            "claude".into(),
            AgentInputDto {
                command: vec!["claude".into(), "--resume".into()],
            },
        )
        .await
        .expect("update agent");
        let dto = b
            .config_editable()
            .expect("editable")
            .config
            .expect("valid base");
        std::fs::remove_file(&path).ok();
        let host = dto.hosts.iter().find(|h| h.id == "devbox").expect("host");
        assert_eq!(host.name.as_deref(), Some("Renamed"));
        let agent = dto.agents.iter().find(|a| a.id == "claude").expect("agent");
        assert_eq!(agent.command, vec!["claude", "--resume"]);
    }

    #[tokio::test]
    async fn remove_commands_delete_in_dependency_order() {
        let (b, path) = editor_bridge("remove-all");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("host");
        b.config_insert_agent("claude".into(), agent_input())
            .await
            .expect("agent");
        b.config_insert_project("api".into(), project_input())
            .await
            .expect("project");
        // Remove the project first (it references host+agent), then both.
        b.config_remove_project("api".into())
            .await
            .expect("remove project");
        b.config_remove_agent("claude".into())
            .await
            .expect("remove agent");
        b.config_remove_host("devbox".into())
            .await
            .expect("remove host");
        let dto = b
            .config_editable()
            .expect("editable")
            .config
            .expect("valid base");
        std::fs::remove_file(&path).ok();
        assert!(dto.hosts.is_empty() && dto.projects.is_empty() && dto.agents.is_empty());
    }

    #[tokio::test]
    async fn insert_host_rejects_a_bad_slug_id() {
        let (b, path) = editor_bridge("bad-slug");
        let err = b
            .config_insert_host("BAD UPPER".into(), ssh_input())
            .await
            .expect_err("a non-slug id must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::InvalidId { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn insert_duplicate_host_is_config_edit() {
        let (b, path) = editor_bridge("dup-host");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("first insert");
        let err = b
            .config_insert_host("devbox".into(), ssh_input())
            .await
            .expect_err("duplicate id must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::ConfigEdit { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn remove_referenced_host_is_config_edit() {
        let (b, path) = editor_bridge("remove-ref");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("host");
        b.config_insert_agent("claude".into(), agent_input())
            .await
            .expect("agent");
        b.config_insert_project("api".into(), project_input())
            .await
            .expect("project");
        // devbox is still referenced by project api — integrity must block it.
        let err = b
            .config_remove_host("devbox".into())
            .await
            .expect_err("removing a referenced host must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::ConfigEdit { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn degraded_single_fault_delete_recovers_the_file() {
        let (b, path) = editor_bridge("degraded-recover");
        // One broken host, everything else fine: deleting it restores validity,
        // so the mutation must read the degraded base, delete, and save.
        std::fs::write(
            &path,
            "[hosts.bad]\ntransport = \"telnet\"\n[agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("seed degraded config");
        b.config_remove_host("bad".into())
            .await
            .expect("deleting the offending host recovers the file");
        let dto = b.config_editable().expect("editable after recovery");
        std::fs::remove_file(&path).ok();
        let config = dto.config.expect("file is valid again");
        assert!(config.hosts.is_empty(), "{config:?}");
        assert!(config.agents.iter().any(|a| a.id == "claude"), "{config:?}");
    }

    #[tokio::test]
    async fn degraded_delete_that_leaves_it_invalid_is_config_edit() {
        let (b, path) = editor_bridge("degraded-still-bad");
        // Two independent faults: deleting one still leaves an invalid file, so
        // save must refuse rather than brick it — surfaced as ConfigEdit.
        std::fs::write(
            &path,
            "[hosts.a]\ntransport = \"telnet\"\n[hosts.b]\ntransport = \"nope\"\n",
        )
        .expect("seed");
        let err = b
            .config_remove_host("a".into())
            .await
            .expect_err("a still-invalid result must be rejected at save");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::ConfigEdit { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn config_editable_of_a_valid_base_has_config_and_no_issues() {
        let (b, path) = editor_bridge("editable-valid");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("host");
        let dto = b.config_editable().expect("editable");
        std::fs::remove_file(&path).ok();
        assert!(dto.issues.is_empty(), "a valid base has no issues: {dto:?}");
        let config = dto.config.expect("valid base carries a typed config");
        assert!(config.hosts.iter().any(|h| h.id == "devbox"), "{config:?}");
    }

    #[tokio::test]
    async fn config_editable_reports_a_degraded_base_with_present_ids() {
        let (b, path) = editor_bridge("editable-degraded");
        // A semantically invalid base: two hosts with bad transports. The file
        // can't produce a typed config, but degraded mode must still open it,
        // report what's wrong, and list the ids the user can delete to recover.
        std::fs::write(
            &path,
            "[hosts.a]\ntransport = \"telnet\"\n[hosts.b]\ntransport = \"nope\"\n",
        )
        .expect("seed invalid config");
        let dto = b.config_editable().expect("a degraded base still opens");
        std::fs::remove_file(&path).ok();
        assert!(
            dto.config.is_none(),
            "a degraded base yields no typed config: {dto:?}"
        );
        assert_eq!(dto.issues.len(), 2, "both bad transports reported: {dto:?}");
        assert_eq!(dto.present.hosts, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn config_editable_of_an_unparseable_base_is_a_load_error() {
        let (b, path) = editor_bridge("editable-unparseable");
        // A TOML grammar error isn't recoverable entry-by-entry — it must
        // surface as a load banner, not a degraded document pretending to parse.
        std::fs::write(&path, "this is not = = toml\n").expect("seed");
        let err = b
            .config_editable()
            .expect_err("an unparseable base is a load error");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::Config { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn config_editable_carries_connection_secrets() {
        let (b, path) = editor_bridge("editable-secrets");
        b.config_insert_host("devbox".into(), ssh_input())
            .await
            .expect("host");
        let dto = b.config_editable().expect("editable");
        let json = serde_json::to_string(&dto).expect("serialize");
        std::fs::remove_file(&path).ok();
        // The editor channel is un-redacted (counterpart to config_get).
        assert!(json.contains("secret-hostname"), "{json}");
        assert!(json.contains("rootuser"), "{json}");
        assert!(json.contains("2222"), "{json}");
    }

    #[tokio::test]
    async fn update_and_remove_missing_id_are_config_edit() {
        let (b, path) = editor_bridge("missing-id");
        let upd = b
            .config_update_host("ghost".into(), ssh_input())
            .await
            .expect_err("update of a missing id must be rejected");
        let rem = b
            .config_remove_agent("ghost".into())
            .await
            .expect_err("remove of a missing id must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(upd, BridgeError::ConfigEdit { .. }), "{upd:?}");
        assert!(matches!(rem, BridgeError::ConfigEdit { .. }), "{rem:?}");
    }

    #[tokio::test]
    async fn mutation_on_a_non_regular_config_path_is_config_error() {
        // A directory at the config path: metadata says not-a-regular-file, so
        // the editor read must refuse it (the size/is_file guard) and classify
        // it as a load error, not an inline edit rejection.
        let b = bridge_with_config(Arc::new(FakeSessionSource::new()), std::env::temp_dir());
        let err = b
            .config_insert_host("devbox".into(), ssh_input())
            .await
            .expect_err("a non-regular config path must be rejected");
        assert!(matches!(err, BridgeError::Config { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn mutation_on_a_malformed_base_is_config_edit() {
        let path = temp_config_path("malformed-edit");
        std::fs::write(&path, "this is = not = valid = toml =").expect("write");
        let b = bridge_with_config(Arc::new(FakeSessionSource::new()), path.clone());
        let err = b
            .config_insert_host("devbox".into(), ssh_input())
            .await
            .expect_err("editing a malformed base must be rejected");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, BridgeError::ConfigEdit { .. }), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_inserts_do_not_lose_updates() {
        let path = temp_config_path("concurrent");
        std::fs::remove_file(&path).ok();
        let b = Arc::new(bridge_with_config(
            Arc::new(FakeSessionSource::new()),
            path.clone(),
        ));
        // Two overlapping inserts of distinct hosts. The mutex serializes the
        // load → mutate → save critical section and each mutation re-reads
        // fresh, so both must land — without the lock a read-modify-write race
        // would drop one (last writer wins).
        let (b1, b2) = (Arc::clone(&b), Arc::clone(&b));
        let t1 =
            tokio::spawn(async move { b1.config_insert_host("hosta".into(), ssh_input()).await });
        let t2 =
            tokio::spawn(async move { b2.config_insert_host("hostb".into(), ssh_input()).await });
        t1.await.expect("join 1").expect("insert a");
        t2.await.expect("join 2").expect("insert b");
        let dto = b
            .config_editable()
            .expect("editable")
            .config
            .expect("valid base");
        std::fs::remove_file(&path).ok();
        assert!(dto.hosts.iter().any(|h| h.id == "hosta"), "{dto:?}");
        assert!(dto.hosts.iter().any(|h| h.id == "hostb"), "{dto:?}");
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
