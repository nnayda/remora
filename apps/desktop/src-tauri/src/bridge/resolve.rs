use std::sync::Arc;

use remora_core::config::{Config, HostId, Transport};
use remora_core::{ExclusiveSource, KubectlSource, SessionLocks, SessionSource, SshSource};
use remora_protocol::ProjectId;

use super::error::BridgeError;

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
    ) -> Result<Arc<dyn SessionSource>, BridgeError>;

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
    ) -> Result<Arc<dyn SessionSource>, BridgeError> {
        let project = config
            .projects
            .get(project_id)
            .ok_or_else(|| BridgeError::Config {
                message: format!("unknown project `{}`", project_id.as_str()),
            })?;
        let host = config
            .hosts
            .get(&project.host)
            .ok_or_else(|| BridgeError::Config {
                message: format!(
                    "project `{}` references unknown host `{}`",
                    project_id.as_str(),
                    project.host.as_str()
                ),
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
    fn for_project_unknown_project_is_config_error() {
        let r = resolver();
        assert!(matches!(
            r.for_project(&config(SSH_PROJECT), &pid("ghost")),
            Err(BridgeError::Config { .. })
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
