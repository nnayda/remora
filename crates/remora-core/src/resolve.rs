//! Config-driven `SessionSource` resolution, shared by every bridge host
//! (desktop in-process bridge, dev loopback, headless `remora-bridge`).
//!
//! Lifted from the desktop shell (ADR-0021 D7 / spec D3, #234): one
//! authoritative copy of the security-relevant wiring that turns a
//! `Config` + project into an `ExclusiveSource`-wrapped transport against
//! one shared `SessionLocks` registry.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{Config, ConfigError, HostId, Transport};
use crate::{
    ExclusiveSource, KubectlSource, RemoteWorkspace, SessionChannel, SessionLocks, SessionSource,
    SourceError, SshSource,
};
use remora_protocol::{AgentId, ProjectId, SessionId, SessionMeta, SpawnSpec};

/// Why a project could not be resolved to a transport.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The project id is not in config.
    #[error("unknown project `{project}`")]
    UnknownProject { project: String },
    /// The project references a host id that is not in config.
    #[error("project `{project}` references unknown host `{host}`")]
    UnknownHost { project: String, host: String },
}

/// Selects the transport for a project's host from freshly-loaded config.
/// The bridge loads config per call (per-call resolution, design D1) and
/// hands an `Arc<Config>` here; `SshSource` is cheap and stateless, so
/// building one per call is fine (no connection opens until attach/list).
pub trait SourceResolver: Send + Sync {
    /// Transport for `project_id`'s host. Errors if the project or its host
    /// is unknown, or the host's transport config is invalid.
    fn for_project(
        &self,
        config: &Arc<Config>,
        project_id: &ProjectId,
    ) -> Result<Arc<dyn SessionSource>, ResolveError>;

    /// One transport per host (ssh and kubectl), paired with its config id,
    /// for discovery aggregation.
    fn all(&self, config: &Arc<Config>) -> Vec<(HostId, Arc<dyn SessionSource>)>;
}

/// Production resolver: config is the source of truth.
///
/// Every source it hands out is wrapped in [`ExclusiveSource`] against one
/// shared [`SessionLocks`] registry, so a session driven concurrently by the
/// desktop UI and (once the relay lands) a phone still serializes its mutating
/// ops below the `SessionSource` seam (ADR-0021). The registry is created once
/// per process and cloned into every wrapper — the wrapper is per-call, the
/// registry is shared.
pub struct ConfigResolver {
    locks: Arc<SessionLocks>,
}

impl ConfigResolver {
    /// Build a resolver that wraps every resolved source against `locks`. The
    /// caller owns the one-per-process registry (so a sibling — e.g. the future
    /// remote-host loopback — can share it); see `Bridge`.
    pub fn new(locks: Arc<SessionLocks>) -> Self {
        Self { locks }
    }

    /// Wrap a raw source in the shared exclusion registry under `host_key`.
    fn wrap(&self, raw: Arc<dyn SessionSource>, host_key: &str) -> Arc<dyn SessionSource> {
        Arc::new(ExclusiveSource::new(raw, Arc::clone(&self.locks), host_key))
    }
}

impl SourceResolver for ConfigResolver {
    fn for_project(
        &self,
        config: &Arc<Config>,
        project_id: &ProjectId,
    ) -> Result<Arc<dyn SessionSource>, ResolveError> {
        let project =
            config
                .projects
                .get(project_id)
                .ok_or_else(|| ResolveError::UnknownProject {
                    project: project_id.as_str().to_owned(),
                })?;
        let host = config
            .hosts
            .get(&project.host)
            .ok_or_else(|| ResolveError::UnknownHost {
                project: project_id.as_str().to_owned(),
                host: project.host.as_str().to_owned(),
            })?;
        let raw: Arc<dyn SessionSource> = match &host.transport {
            Transport::Ssh(ssh) => Arc::new(SshSource::new(ssh.clone(), Arc::clone(config))),
            Transport::Kubectl(k) => Arc::new(KubectlSource::new(k.clone(), Arc::clone(config))),
        };
        Ok(self.wrap(raw, project.host.as_str()))
    }

    fn all(&self, config: &Arc<Config>) -> Vec<(HostId, Arc<dyn SessionSource>)> {
        config
            .hosts
            .iter()
            .map(|(id, host)| {
                let raw: Arc<dyn SessionSource> = match &host.transport {
                    Transport::Ssh(ssh) => {
                        Arc::new(SshSource::new(ssh.clone(), Arc::clone(config)))
                    }
                    Transport::Kubectl(k) => {
                        Arc::new(KubectlSource::new(k.clone(), Arc::clone(config)))
                    }
                };
                (id.clone(), self.wrap(raw, id.as_str()))
            })
            .collect()
    }
}

/// The [`SessionSource`] the loopback bridge serves: it resolves each request's
/// project through the desktop's own resolver against freshly-loaded config,
/// exactly like the Bridge's direct path — so the bridge and the direct path go
/// through the same per-session exclusion registry.
pub struct ResolvingSource {
    resolver: Arc<dyn SourceResolver>,
    config_path: PathBuf,
}

impl ResolvingSource {
    /// Build a source that resolves each request through `resolver` against the
    /// config at `config_path`. Shared by the loopback and the real relay bridge
    /// so both serve through the desktop's one per-session exclusion registry.
    pub fn new(resolver: Arc<dyn SourceResolver>, config_path: PathBuf) -> Self {
        Self {
            resolver,
            config_path,
        }
    }

    /// Load config fresh (a missing file is an empty config — a fresh device is
    /// valid, ADR-0004). Config problems surface as `Transport` across the seam.
    fn load_config(&self) -> Result<Arc<Config>, SourceError> {
        match Config::load(&self.config_path) {
            Ok(config) => Ok(Arc::new(config)),
            Err(ConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Arc::new(Config::default()))
            }
            Err(e) => Err(SourceError::Transport(format!("config load failed: {e}"))),
        }
    }

    /// Resolve `project_id`'s (already exclusion-wrapped) source from fresh
    /// config.
    fn for_project(&self, project_id: &ProjectId) -> Result<Arc<dyn SessionSource>, SourceError> {
        let config = self.load_config()?;
        self.resolver
            .for_project(&config, project_id)
            .map_err(|e| SourceError::Transport(format!("resolve failed: {e}")))
    }
}

#[async_trait]
impl SessionSource for ResolvingSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let source = self.for_project(&spec.project_id)?;
        source.spawn(spec).await
    }

    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        self.for_project(project_id)?
            .attach(project_id, session_id)
            .await
    }

    async fn external_attach_command(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<Vec<String>, SourceError> {
        self.for_project(project_id)?
            .external_attach_command(project_id, session_id)
            .await
    }

    async fn remote_workspace(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        workspace_path: &str,
    ) -> Result<RemoteWorkspace, SourceError> {
        self.for_project(project_id)?
            .remote_workspace(project_id, session_id, workspace_path)
            .await
    }

    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        self.for_project(project_id)?
            .respawn(project_id, session_id, agent)
            .await
    }

    async fn stop(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<(), SourceError> {
        self.for_project(project_id)?
            .stop(project_id, session_id)
            .await
    }

    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError> {
        self.for_project(project_id)?
            .remove(project_id, session_id, force)
            .await
    }

    /// Every configured host's sessions, flattened. Not routed through by the
    /// desktop today (hybrid keeps `list` direct), but implemented so the served
    /// source is complete and any future both-route switch is a one-liner.
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let config = self.load_config()?;
        let sources = self.resolver.all(&config);
        let results = futures_util::future::join_all(
            sources
                .into_iter()
                .map(|(_id, src)| async move { src.list().await }),
        )
        .await;
        let mut all = Vec::new();
        for result in results {
            all.extend(result?);
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Arc<Config> {
        Arc::new(Config::from_toml_str(toml).expect("valid test config"))
    }
    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("slug")
    }
    /// A resolver with its own fresh registry. Construction wiring is asserted
    /// structurally here (it compiles and resolves); the load-bearing
    /// serialization behaviour lives in `exclusive.rs` and the Bridge-level
    /// ordering test in `mod.rs`.
    fn resolver() -> ConfigResolver {
        ConfigResolver::new(SessionLocks::new())
    }

    const SSH_PROJECT: &str = "[hosts.hermes]\ntransport = \"ssh\"\nhost = \"hermes\"\n\
        [projects.api]\nhost = \"hermes\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\
        [agents.claude]\ncommand = [\"claude\"]\n";

    #[test]
    fn for_project_builds_ssh_source() {
        let r = resolver();
        assert!(r.for_project(&config(SSH_PROJECT), &pid("api")).is_ok());
    }

    #[test]
    fn for_project_unknown_project_is_resolve_error() {
        let r = resolver();
        assert!(matches!(
            r.for_project(&config(SSH_PROJECT), &pid("ghost")),
            Err(ResolveError::UnknownProject { .. })
        ));
    }

    #[test]
    fn for_project_builds_kubectl_source() {
        let toml = "[hosts.k8s]\ntransport = \"kubectl\"\npod = \"p\"\n\
            [projects.api]\nhost = \"k8s\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\
            [agents.claude]\ncommand = [\"claude\"]\n";
        let r = resolver();
        assert!(r.for_project(&config(toml), &pid("api")).is_ok());
    }

    #[test]
    fn all_counts_ssh_and_kubectl_hosts() {
        let toml = "[hosts.a]\ntransport = \"ssh\"\nhost = \"a\"\n\
            [hosts.b]\ntransport = \"ssh\"\nhost = \"b\"\n\
            [hosts.k]\ntransport = \"kubectl\"\npod = \"p\"\n";
        let r = resolver();
        assert_eq!(r.all(&config(toml)).len(), 3);
    }

    #[test]
    fn all_empty_config_is_empty() {
        let r = resolver();
        assert!(r.all(&config("")).is_empty());
    }

    #[test]
    fn all_pairs_each_source_with_its_host_id() {
        let toml = "[hosts.a]\ntransport = \"ssh\"\nhost = \"a\"\n\
            [hosts.k]\ntransport = \"kubectl\"\npod = \"p\"\n";
        let r = resolver();
        let ids: Vec<String> = r
            .all(&config(toml))
            .iter()
            .map(|(id, _)| id.as_str().to_owned())
            .collect();
        // config.hosts is a BTreeMap, so ids come back sorted.
        assert_eq!(ids, vec!["a".to_string(), "k".to_string()]);
    }
}
