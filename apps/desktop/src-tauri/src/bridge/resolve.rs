use std::sync::Arc;

use remora_core::config::{Config, HostId, Transport};
use remora_core::{KubectlSource, SessionSource, SshSource};
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
pub struct ConfigResolver;

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
        match &host.transport {
            Transport::Ssh(ssh) => Ok(Arc::new(SshSource::new(ssh.clone(), Arc::clone(config)))),
            Transport::Kubectl(k) => {
                Ok(Arc::new(KubectlSource::new(k.clone(), Arc::clone(config))))
            }
        }
    }

    fn all(&self, config: &Arc<Config>) -> Vec<(HostId, Arc<dyn SessionSource>)> {
        config
            .hosts
            .iter()
            .map(|(id, host)| {
                let source: Arc<dyn SessionSource> = match &host.transport {
                    Transport::Ssh(ssh) => {
                        Arc::new(SshSource::new(ssh.clone(), Arc::clone(config)))
                    }
                    Transport::Kubectl(k) => {
                        Arc::new(KubectlSource::new(k.clone(), Arc::clone(config)))
                    }
                };
                (id.clone(), source)
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

    const SSH_PROJECT: &str = "[hosts.hermes]\ntransport = \"ssh\"\nhost = \"hermes\"\n\
        [projects.api]\nhost = \"hermes\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\
        [agents.claude]\ncommand = [\"claude\"]\n";

    #[test]
    fn for_project_builds_ssh_source() {
        let r = ConfigResolver;
        assert!(r.for_project(&config(SSH_PROJECT), &pid("api")).is_ok());
    }

    #[test]
    fn for_project_unknown_project_is_config_error() {
        let r = ConfigResolver;
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
        let r = ConfigResolver;
        assert!(r.for_project(&config(toml), &pid("api")).is_ok());
    }

    #[test]
    fn all_counts_ssh_and_kubectl_hosts() {
        let toml = "[hosts.a]\ntransport = \"ssh\"\nhost = \"a\"\n\
            [hosts.b]\ntransport = \"ssh\"\nhost = \"b\"\n\
            [hosts.k]\ntransport = \"kubectl\"\npod = \"p\"\n";
        let r = ConfigResolver;
        assert_eq!(r.all(&config(toml)).len(), 3);
    }

    #[test]
    fn all_empty_config_is_empty() {
        let r = ConfigResolver;
        assert!(r.all(&config("")).is_empty());
    }

    #[test]
    fn all_pairs_each_source_with_its_host_id() {
        let toml = "[hosts.a]\ntransport = \"ssh\"\nhost = \"a\"\n\
            [hosts.k]\ntransport = \"kubectl\"\npod = \"p\"\n";
        let r = ConfigResolver;
        let ids: Vec<String> = r
            .all(&config(toml))
            .iter()
            .map(|(id, _)| id.as_str().to_owned())
            .collect();
        // config.hosts is a BTreeMap, so ids come back sorted.
        assert_eq!(ids, vec!["a".to_string(), "k".to_string()]);
    }
}
