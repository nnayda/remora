//! Frontend-facing config DTOs (display-only projection of `remora_core::config`).
//!
//! These cross the bridge to render the sidebar's Host → Project tree. They are
//! deliberately a *narrow* projection: only labels and the host↔project edges.
//! The `From` impls are the **redaction boundary** — connection secrets
//! (ssh user/host/port, kube pod/namespace/context/container) live in
//! `remora_core::config` and MUST NOT be copied here. A test enforces it.
use crate::bridge::editor_dto::WorkspaceModeDto;
use remora_core::config::{Config, Host, Project, Transport};

/// The whole per-device config, projected for the sidebar. `Default` is the
/// empty config a fresh device (no file yet) renders.
#[derive(Clone, Debug, Default, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub hosts: Vec<HostDto>,
    pub projects: Vec<ProjectDto>,
    pub agents: Vec<AgentDto>,
}

/// A configured host, label-only. The `transport` discriminant is all the UI
/// needs (an icon/badge); the connection details never cross.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HostDto {
    pub id: String,
    pub name: Option<String>,
    pub transport: TransportKindDto,
}

/// Which transport a host uses — the discriminant only, no connection fields.
#[derive(Clone, Copy, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum TransportKindDto {
    Ssh,
    Kubectl,
}

/// A configured project: its label, the host it lives on, its workspace mode,
/// and its default agent. The on-host `path` is intentionally omitted — it is
/// not needed to render the tree and is closer to a connection detail than a
/// label. Workspace mode is structural (it affects session UI), not a secret.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDto {
    pub id: String,
    pub name: Option<String>,
    pub host_id: String,
    pub workspace: WorkspaceModeDto,
    pub agent: String,
}

/// A configured agent, id-only. The launch `command` argv is a launch detail
/// (not a label) and is intentionally omitted — the new-session dialog needs
/// only the id to pass as `SpawnSpec::agent`.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AgentDto {
    pub id: String,
}

impl From<Config> for ConfigDto {
    fn from(config: Config) -> Self {
        // BTreeMap iteration is sorted, so the sidebar render order is stable.
        ConfigDto {
            hosts: config
                .hosts
                .into_iter()
                .map(|(id, host)| host_dto(id.as_str(), host))
                .collect(),
            projects: config
                .projects
                .into_iter()
                .map(|(id, project)| project_dto(id.as_str(), project))
                .collect(),
            agents: config
                .agents
                .into_keys()
                .map(|id| AgentDto {
                    id: id.as_str().to_owned(),
                })
                .collect(),
        }
    }
}

/// Redaction boundary: reads ONLY the host's label + transport discriminant.
/// The `Transport` payload (ssh/kube connection details) is matched but never
/// copied — adding a connection field to `HostDto` would have to happen here,
/// which is exactly where the `dto_redacts_all_connection_secrets` test guards.
fn host_dto(id: &str, host: Host) -> HostDto {
    let transport = match host.transport {
        Transport::Ssh(_) => TransportKindDto::Ssh,
        Transport::Kubectl(_) => TransportKindDto::Kubectl,
    };
    HostDto {
        id: id.to_owned(),
        name: host.name,
        transport,
    }
}

/// Reads the project's label, its host edge, workspace mode, and default agent.
/// The on-host `path` is intentionally omitted — it is not needed to render the
/// tree and is closer to a connection detail than a label.
fn project_dto(id: &str, project: Project) -> ProjectDto {
    ProjectDto {
        id: id.to_owned(),
        name: project.name,
        host_id: project.host.as_str().to_owned(),
        workspace: project.workspace.into(),
        agent: project.agent.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_core::config::{
        Agent, Config, Host, HostId, KubectlField, KubectlHost, Project, SshHost, Transport,
        WorkspaceMode,
    };
    use remora_protocol::{AgentId, ProjectId};

    fn ssh_host() -> Host {
        Host {
            name: Some("Dev box".into()),
            transport: Transport::Ssh(SshHost {
                host: "secret-hostname".into(),
                user: Some("rootuser".into()),
                port: Some(2222),
            }),
            worktree_root: None,
        }
    }

    fn kubectl_host() -> Host {
        Host {
            name: None,
            transport: Transport::Kubectl(KubectlHost {
                pod: KubectlField::Command("get-secret-command".into()),
                namespace: Some(KubectlField::Literal("secret-namespace".into())),
                context: Some(KubectlField::Literal("secret-context".into())),
                container: Some(KubectlField::Literal("secret-container".into())),
            }),
            worktree_root: None,
        }
    }

    #[test]
    fn maps_config_in_btreemap_order() {
        let mut config = Config::default();
        config
            .hosts
            .insert(HostId::new("zeta").expect("id"), ssh_host());
        config
            .hosts
            .insert(HostId::new("alpha").expect("id"), kubectl_host());
        config.projects.insert(
            ProjectId::new("api").expect("id"),
            Project {
                name: Some("API".into()),
                host: HostId::new("alpha").expect("id"),
                path: "/srv/api".into(),
                workspace: WorkspaceMode::Worktree,
                agent: AgentId::new("claude").expect("id"),
                base: None,
                worktree_root: None,
            },
        );
        config.agents.insert(
            AgentId::new("claude").expect("id"),
            Agent {
                command: vec!["claude".into()],
                provision: None,
            },
        );

        let dto = ConfigDto::from(config);

        // BTreeMap order: alpha before zeta.
        assert_eq!(dto.hosts[0].id, "alpha");
        assert_eq!(dto.hosts[1].id, "zeta");
        assert!(matches!(dto.hosts[0].transport, TransportKindDto::Kubectl));
        assert!(matches!(dto.hosts[1].transport, TransportKindDto::Ssh));
        assert_eq!(dto.hosts[1].name.as_deref(), Some("Dev box"));
        assert_eq!(dto.projects.len(), 1);
        assert_eq!(dto.projects[0].id, "api");
        assert_eq!(dto.projects[0].host_id, "alpha");
        assert_eq!(dto.projects[0].agent, "claude");
    }

    #[test]
    fn maps_agent_ids_in_btreemap_order_without_argv() {
        let mut config = Config::default();
        config.agents.insert(
            AgentId::new("zeta").expect("id"),
            Agent {
                command: vec!["zeta-cli".into()],
                provision: None,
            },
        );
        config.agents.insert(
            AgentId::new("alpha").expect("id"),
            Agent {
                command: vec!["alpha-cli".into()],
                provision: None,
            },
        );

        let dto = ConfigDto::from(config);

        // BTreeMap order: alpha before zeta. The dialog needs the id to pass as
        // `SpawnSpec::agent`, nothing more.
        assert_eq!(
            dto.agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );

        // The launch argv is a launch detail, not a label — it must not cross.
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(!json.contains("zeta-cli"), "AgentDto leaked argv: {json}");
        assert!(!json.contains("alpha-cli"), "AgentDto leaked argv: {json}");
    }

    #[test]
    fn project_dto_carries_workspace_mode() {
        let mut config = Config::default();
        config.projects.insert(
            ProjectId::new("api").expect("id"),
            Project {
                name: None,
                host: HostId::new("h").expect("id"),
                path: "/p".into(),
                workspace: WorkspaceMode::Shared,
                agent: AgentId::new("claude").expect("id"),
                base: None,
                worktree_root: None,
            },
        );
        let dto = ConfigDto::from(config);
        assert!(matches!(
            dto.projects[0].workspace,
            WorkspaceModeDto::Shared
        ));
    }

    #[test]
    fn dto_redacts_all_connection_secrets() {
        let mut config = Config::default();
        config
            .hosts
            .insert(HostId::new("ssh-box").expect("id"), ssh_host());
        config
            .hosts
            .insert(HostId::new("kube-box").expect("id"), kubectl_host());

        let dto = ConfigDto::from(config);
        let json = serde_json::to_string(&dto).expect("serialize");

        // None of the connection details may cross the bridge.
        for secret in [
            "secret-hostname",
            "rootuser",
            "2222",
            "get-secret-command",
            "secret-namespace",
            "secret-context",
            "secret-container",
        ] {
            assert!(
                !json.contains(secret),
                "HostDto leaked connection secret `{secret}`: {json}"
            );
        }
    }
}
