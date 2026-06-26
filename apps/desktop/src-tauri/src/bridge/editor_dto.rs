//! Editor-channel config DTOs — the **un-redacted** counterpart to `dto.rs`.
//!
//! `dto.rs` is the display projection that crosses to the sidebar with every
//! connection secret stripped. This module is the *editor* projection: it
//! carries the **full** host/project/agent values (ssh host/user/port, kube
//! pod/namespace/context/container, on-host path, agent argv) because the
//! settings forms must round-trip exactly what is on disk.
//!
//! Because it is un-redacted it is **local-only**: these DTOs and their
//! commands must never travel over the future relay. They deliberately do NOT
//! share a `From<Config>` path with `ConfigDto` — a future "unify the DTOs"
//! refactor must not be able to silently un-redact the display path. A test
//! enforces the split.

use remora_core::config::{
    Agent, Config, Host, HostId, KubectlField, KubectlHost, PresentIds, Project, SshHost,
    Transport, WorkspaceMode,
};
use remora_protocol::AgentId;

use crate::bridge::error::BridgeError;

/// The editable config plus its validation state (ADR-0006 degraded mode).
///
/// In the normal case `config` is `Some` and `issues` is empty. When the base
/// file is *semantically* invalid, `config` is `None` (no typed config can be
/// produced), `issues` lists what is broken (rendered + sanitized), and
/// `present` lists the entry ids still in the file so the UI can offer
/// per-entity delete recovery without the whole document having to validate
/// first. A file that isn't even parseable surfaces as a `Config` load error,
/// not a degraded document.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EditableConfigDto {
    pub config: Option<EditorConfigDto>,
    pub issues: Vec<String>,
    pub present: PresentEntitiesDto,
}

/// Entry ids present in each section of the document, regardless of validity —
/// the delete targets degraded-mode recovery offers. Mirrors core's
/// [`PresentIds`] field-for-field; the `From` impl below is the single place to
/// update if either gains a section, so keep the two in lockstep.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PresentEntitiesDto {
    pub hosts: Vec<String>,
    pub projects: Vec<String>,
    pub agents: Vec<String>,
}

impl From<PresentIds> for PresentEntitiesDto {
    fn from(ids: PresentIds) -> Self {
        PresentEntitiesDto {
            hosts: ids.hosts,
            projects: ids.projects,
            agents: ids.agents,
        }
    }
}

/// The whole per-device config, projected **un-redacted** for the editor forms.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EditorConfigDto {
    pub hosts: Vec<EditorHostDto>,
    pub projects: Vec<EditorProjectDto>,
    pub agents: Vec<EditorAgentDto>,
}

/// A host with its full connection details (the editable form state).
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EditorHostDto {
    pub id: String,
    pub name: Option<String>,
    pub transport: TransportDto,
}

/// A kubectl connection field for the editor: `command = false` is a literal
/// value, `command = true` means `value` is a shell command line resolved at
/// connect time. Flat + form-friendly for the TS toggle.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct KubectlFieldDto {
    pub command: bool,
    pub value: String,
}

impl From<KubectlField> for KubectlFieldDto {
    fn from(field: KubectlField) -> Self {
        match field {
            KubectlField::Literal(value) => Self {
                command: false,
                value,
            },
            KubectlField::Command(value) => Self {
                command: true,
                value,
            },
        }
    }
}

impl From<KubectlFieldDto> for KubectlField {
    fn from(dto: KubectlFieldDto) -> Self {
        if dto.command {
            KubectlField::Command(dto.value)
        } else {
            KubectlField::Literal(dto.value)
        }
    }
}

/// Transport + connection details, shared by the read projection and the write
/// inputs (it round-trips: what the form shows is what it submits). Internally
/// tagged on `kind` so the TS side discriminates on a single field.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransportDto {
    Ssh {
        host: String,
        user: Option<String>,
        port: Option<u16>,
    },
    Kubectl {
        pod: KubectlFieldDto,
        namespace: Option<KubectlFieldDto>,
        context: Option<KubectlFieldDto>,
        container: Option<KubectlFieldDto>,
    },
}

/// A project with every editable field, including the on-host `path` and
/// `workspace` mode that the display `ProjectDto` omits.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EditorProjectDto {
    pub id: String,
    pub name: Option<String>,
    pub host_id: String,
    pub path: String,
    pub workspace: WorkspaceModeDto,
    pub agent: String,
    pub base: Option<String>,
}

/// Workspace mode, shared by the read projection and the write inputs.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceModeDto {
    Worktree,
    Shared,
}

/// An agent with its full launch argv (the display `AgentDto` carries only id).
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EditorAgentDto {
    pub id: String,
    pub command: Vec<String>,
}

impl From<Config> for EditorConfigDto {
    fn from(config: Config) -> Self {
        // BTreeMap iteration is sorted, so the form list order is stable.
        EditorConfigDto {
            hosts: config
                .hosts
                .into_iter()
                .map(|(id, host)| editor_host_dto(id.as_str(), host))
                .collect(),
            projects: config
                .projects
                .into_iter()
                .map(|(id, project)| editor_project_dto(id.as_str(), project))
                .collect(),
            agents: config
                .agents
                .into_iter()
                .map(|(id, agent)| EditorAgentDto {
                    id: id.as_str().to_owned(),
                    command: agent.command,
                })
                .collect(),
        }
    }
}

impl From<WorkspaceMode> for WorkspaceModeDto {
    fn from(mode: WorkspaceMode) -> Self {
        match mode {
            WorkspaceMode::Worktree => WorkspaceModeDto::Worktree,
            WorkspaceMode::Shared => WorkspaceModeDto::Shared,
        }
    }
}

impl From<Transport> for TransportDto {
    fn from(transport: Transport) -> Self {
        match transport {
            Transport::Ssh(ssh) => TransportDto::Ssh {
                host: ssh.host,
                user: ssh.user,
                port: ssh.port,
            },
            Transport::Kubectl(k) => TransportDto::Kubectl {
                pod: k.pod.into(),
                namespace: k.namespace.map(Into::into),
                context: k.context.map(Into::into),
                container: k.container.map(Into::into),
            },
        }
    }
}

/// Form payload for create/edit of a host. The entry id is a separate command
/// argument, so it is not carried here.
#[derive(Clone, Debug, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HostInputDto {
    pub name: Option<String>,
    pub transport: TransportDto,
}

impl HostInputDto {
    /// Infallible: a host carries no cross-references, so every field is a plain
    /// value. Validation (e.g. an empty ssh host) happens at save time through
    /// the core's re-validation, not here.
    pub fn into_host(self) -> Host {
        let transport = match self.transport {
            TransportDto::Ssh { host, user, port } => Transport::Ssh(SshHost { host, user, port }),
            TransportDto::Kubectl {
                pod,
                namespace,
                context,
                container,
            } => Transport::Kubectl(KubectlHost {
                pod: pod.into(),
                namespace: namespace.map(Into::into),
                context: context.map(Into::into),
                container: container.map(Into::into),
            }),
        };
        Host {
            name: self.name,
            transport,
            worktree_root: None,
        }
    }
}

/// Form payload for create/edit of a project. `host_id` and `agent` are
/// references to existing entries; converting parses them into ids.
#[derive(Clone, Debug, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInputDto {
    pub name: Option<String>,
    pub host_id: String,
    pub path: String,
    pub workspace: WorkspaceModeDto,
    pub agent: String,
    #[serde(default)]
    pub base: Option<String>,
}

impl ProjectInputDto {
    /// Parses the `host_id`/`agent` reference slugs into ids. A malformed slug
    /// yields [`BridgeError::InvalidId`]; *referential* validity (the host/agent
    /// actually existing) is enforced later by the core's re-validation.
    pub fn into_project(self) -> Result<Project, BridgeError> {
        let invalid = |e: remora_protocol::InvalidIdError| BridgeError::InvalidId {
            message: e.to_string(),
        };
        Ok(Project {
            name: self.name,
            host: HostId::new(self.host_id).map_err(invalid)?,
            path: self.path,
            workspace: self.workspace.into(),
            agent: AgentId::new(self.agent).map_err(invalid)?,
            base: self.base,
            worktree_root: None,
        })
    }
}

/// Form payload for create/edit of an agent.
#[derive(Clone, Debug, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AgentInputDto {
    pub command: Vec<String>,
}

impl AgentInputDto {
    /// Infallible: argv shape (no blank elements in a non-empty command; an
    /// empty command is a valid plain shell) is validated by the core's
    /// re-validation at save time.
    pub fn into_agent(self) -> Agent {
        Agent {
            command: self.command,
        }
    }
}

impl From<WorkspaceModeDto> for WorkspaceMode {
    fn from(mode: WorkspaceModeDto) -> Self {
        match mode {
            WorkspaceModeDto::Worktree => WorkspaceMode::Worktree,
            WorkspaceModeDto::Shared => WorkspaceMode::Shared,
        }
    }
}

fn editor_host_dto(id: &str, host: Host) -> EditorHostDto {
    EditorHostDto {
        id: id.to_owned(),
        name: host.name,
        transport: host.transport.into(),
    }
}

fn editor_project_dto(id: &str, project: Project) -> EditorProjectDto {
    EditorProjectDto {
        id: id.to_owned(),
        name: project.name,
        host_id: project.host.as_str().to_owned(),
        path: project.path,
        workspace: project.workspace.into(),
        agent: project.agent.as_str().to_owned(),
        base: project.base,
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
                pod: KubectlField::Literal("secret-pod".into()),
                namespace: Some(KubectlField::Literal("secret-namespace".into())),
                context: Some(KubectlField::Literal("secret-context".into())),
                container: Some(KubectlField::Literal("secret-container".into())),
            }),
            worktree_root: None,
        }
    }

    fn linked_config() -> Config {
        let mut config = Config::default();
        config
            .hosts
            .insert(HostId::new("ssh-box").expect("id"), ssh_host());
        config
            .hosts
            .insert(HostId::new("kube-box").expect("id"), kubectl_host());
        config.projects.insert(
            ProjectId::new("api").expect("id"),
            Project {
                name: Some("API".into()),
                host: HostId::new("ssh-box").expect("id"),
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
                command: vec!["claude".into(), "--flag".into()],
            },
        );
        config
    }

    #[test]
    fn editor_dto_carries_every_connection_value() {
        let dto = EditorConfigDto::from(linked_config());
        let json = serde_json::to_string(&dto).expect("serialize");

        // The editor channel is the redaction *counterpart*: every secret the
        // display `ConfigDto` strips MUST be present here so the form can edit it.
        for value in [
            "secret-hostname",
            "rootuser",
            "2222",
            "secret-pod",
            "secret-namespace",
            "secret-context",
            "secret-container",
            "/srv/api", // on-host path (omitted from ConfigDto)
            "--flag",   // agent argv (omitted from ConfigDto)
        ] {
            assert!(
                json.contains(value),
                "EditorConfigDto dropped editable value `{value}`: {json}"
            );
        }
    }

    #[test]
    fn editor_dto_maps_btreemap_order_and_full_project_fields() {
        let dto = EditorConfigDto::from(linked_config());
        // BTreeMap order: kube-box before ssh-box.
        assert_eq!(dto.hosts[0].id, "kube-box");
        assert_eq!(dto.hosts[1].id, "ssh-box");
        let project = &dto.projects[0];
        assert_eq!(project.id, "api");
        assert_eq!(project.host_id, "ssh-box");
        assert_eq!(project.path, "/srv/api");
        assert_eq!(project.agent, "claude");
        assert!(matches!(project.workspace, WorkspaceModeDto::Worktree));
        assert_eq!(dto.agents[0].id, "claude");
        assert_eq!(dto.agents[0].command, vec!["claude", "--flag"]);
    }

    #[test]
    fn host_input_deserializes_and_converts_to_ssh_host() {
        let input: HostInputDto = serde_json::from_str(
            r#"{"name":"Dev","transport":{"kind":"ssh","host":"h","user":"u","port":22}}"#,
        )
        .expect("deserialize host input");
        let host = input.into_host();
        assert_eq!(host.name.as_deref(), Some("Dev"));
        match host.transport {
            Transport::Ssh(ssh) => {
                assert_eq!(ssh.host, "h");
                assert_eq!(ssh.user.as_deref(), Some("u"));
                assert_eq!(ssh.port, Some(22));
            }
            other => panic!("expected ssh, got {other:?}"),
        }
    }

    #[test]
    fn host_input_converts_to_kubectl_host() {
        let input: HostInputDto = serde_json::from_str(
            r#"{"name":null,"transport":{"kind":"kubectl","pod":{"command":false,"value":"p"},"namespace":{"command":false,"value":"ns"},"context":null,"container":null}}"#,
        )
        .expect("deserialize kubectl input");
        match input.into_host().transport {
            Transport::Kubectl(k) => {
                assert_eq!(k.pod, KubectlField::Literal("p".into()));
                assert_eq!(k.namespace, Some(KubectlField::Literal("ns".into())));
                assert_eq!(k.context, None);
            }
            other => panic!("expected kubectl, got {other:?}"),
        }
    }

    #[test]
    fn project_input_converts_with_valid_ids() {
        let input: ProjectInputDto = serde_json::from_str(
            r#"{"name":"API","hostId":"devbox","path":"/srv/api","workspace":"shared","agent":"claude"}"#,
        )
        .expect("deserialize project input");
        let project = input.into_project().expect("valid ids convert");
        assert_eq!(project.host.as_str(), "devbox");
        assert_eq!(project.agent.as_str(), "claude");
        assert_eq!(project.path, "/srv/api");
        assert!(matches!(project.workspace, WorkspaceMode::Shared));
    }

    #[test]
    fn project_input_rejects_a_bad_host_id() {
        let input: ProjectInputDto = serde_json::from_str(
            r#"{"name":null,"hostId":"BAD UPPER","path":"/x","workspace":"worktree","agent":"claude"}"#,
        )
        .expect("deserialize");
        let err = input
            .into_project()
            .expect_err("an invalid host id slug must be rejected");
        assert!(
            matches!(err, crate::bridge::error::BridgeError::InvalidId { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn project_base_survives_dto_round_trip() {
        let project = Project {
            name: Some("API".into()),
            host: HostId::new("ssh-box").expect("id"),
            path: "/srv/api".into(),
            workspace: WorkspaceMode::Worktree,
            agent: AgentId::new("claude").expect("id"),
            base: Some("origin/develop".into()),
            worktree_root: None,
        };
        // out to the form…
        let out = editor_project_dto("api", project);
        assert_eq!(out.base.as_deref(), Some("origin/develop"));
        // …and back from the form must NOT drop base.
        let back = ProjectInputDto {
            name: out.name,
            host_id: out.host_id,
            path: out.path,
            workspace: out.workspace,
            agent: out.agent,
            base: out.base,
        }
        .into_project()
        .expect("into_project");
        assert_eq!(back.base.as_deref(), Some("origin/develop"));
    }

    #[test]
    fn agent_input_converts_to_agent() {
        let input: AgentInputDto =
            serde_json::from_str(r#"{"command":["claude","--flag"]}"#).expect("deserialize");
        assert_eq!(input.into_agent().command, vec!["claude", "--flag"]);
    }

    #[test]
    fn editor_dto_round_trips_command_form_kubectl_fields() {
        let host = Host {
            name: None,
            transport: Transport::Kubectl(KubectlHost {
                pod: KubectlField::Command("kubectl get pods -o name | head -n1".into()),
                namespace: Some(KubectlField::Literal("sb".into())),
                context: None,
                container: None,
            }),
            worktree_root: None,
        };
        let dto = TransportDto::from(host.transport.clone());
        let TransportDto::Kubectl {
            ref pod,
            ref namespace,
            ..
        } = dto
        else {
            panic!("expected kubectl");
        };
        assert!(pod.command, "pod is a command");
        assert_eq!(pod.value, "kubectl get pods -o name | head -n1");
        assert_eq!(namespace.as_ref().map(|f| f.command), Some(false));

        // Round-trips back to the same Host.
        let back = HostInputDto {
            name: None,
            transport: dto,
        }
        .into_host();
        assert_eq!(back, host);
    }

    #[test]
    fn display_and_editor_dtos_are_distinct_projections() {
        // The drift guard (eng review #8): the two DTOs must remain *separate*
        // projections of Config. If a refactor ever unified them onto one
        // `From<Config>`, the display path would carry the editor's secrets.
        // Proven behaviourally: the same Config yields a redacted display JSON
        // and an un-redacted editor JSON.
        let config = linked_config();
        let display = serde_json::to_string(&crate::bridge::dto::ConfigDto::from(config.clone()))
            .expect("display serialize");
        let editor =
            serde_json::to_string(&EditorConfigDto::from(config)).expect("editor serialize");
        assert!(
            !display.contains("secret-hostname"),
            "display DTO must stay redacted: {display}"
        );
        assert!(
            editor.contains("secret-hostname"),
            "editor DTO must carry the secret: {editor}"
        );
    }
}
