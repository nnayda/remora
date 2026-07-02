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
    pub worktree_root: Option<String>,
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
    pub worktree_root: Option<String>,
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
    pub provision: Option<ProvisionFileDto>,
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
                    provision: agent.provision.map(Into::into),
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
    /// Preserved across the editor round-trip so that a TOML-set `worktree_root`
    /// is not silently cleared when the user edits an unrelated field. B2 will
    /// add the form input; here we just thread the value through so the save
    /// path does not destroy it.
    #[serde(default)]
    pub worktree_root: Option<String>,
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
            worktree_root: self.worktree_root,
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
    /// Preserved across the editor round-trip so that a TOML-set `worktree_root`
    /// is not silently cleared when the user edits an unrelated field. B2 will
    /// add the form input; here we just thread the value through so the save
    /// path does not destroy it.
    #[serde(default)]
    pub worktree_root: Option<String>,
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
            worktree_root: self.worktree_root,
        })
    }
}

/// A single provisioned file (ADR-0003 data, #196): the editor's counterpart
/// to core's `ProvisionFile`. Shared by the read projection and the write
/// inputs so the form round-trips exactly what is on disk.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ProvisionFileDto {
    pub path: String,
    pub content: String,
    pub mode: Option<u32>,
}

impl From<remora_core::config::ProvisionFile> for ProvisionFileDto {
    fn from(file: remora_core::config::ProvisionFile) -> Self {
        ProvisionFileDto {
            path: file.path,
            content: file.content,
            mode: file.mode,
        }
    }
}

impl From<ProvisionFileDto> for remora_core::config::ProvisionFile {
    fn from(dto: ProvisionFileDto) -> Self {
        remora_core::config::ProvisionFile {
            path: dto.path,
            content: dto.content,
            mode: dto.mode,
        }
    }
}

/// Form payload for create/edit of an agent.
#[derive(Clone, Debug, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct AgentInputDto {
    pub command: Vec<String>,
    /// Preserved across the editor round-trip so that a TOML-set `provision`
    /// file is not silently dropped when the user edits an unrelated field.
    #[serde(default)]
    pub provision: Option<ProvisionFileDto>,
}

impl AgentInputDto {
    /// Infallible: argv shape (no blank elements in a non-empty command; an
    /// empty command is a valid plain shell) is validated by the core's
    /// re-validation at save time.
    pub fn into_agent(self) -> Agent {
        Agent {
            command: self.command,
            provision: self.provision.map(Into::into),
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
        worktree_root: host.worktree_root,
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
        worktree_root: project.worktree_root,
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
                provision: None,
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
            worktree_root: out.worktree_root,
        }
        .into_project()
        .expect("into_project");
        assert_eq!(back.base.as_deref(), Some("origin/develop"));
    }

    #[test]
    fn project_worktree_root_survives_dto_round_trip() {
        // A TOML-set `worktree_root` must not be silently cleared when the user
        // edits and saves a project through the config editor. The editor reads
        // `worktree_root` from the Project into `EditorProjectDto`, and the form
        // submission carries it back through `ProjectInputDto::into_project`.
        let project = Project {
            name: Some("API".into()),
            host: HostId::new("ssh-box").expect("id"),
            path: "/srv/api".into(),
            workspace: WorkspaceMode::Worktree,
            agent: AgentId::new("claude").expect("id"),
            base: None,
            worktree_root: Some("~/work".into()),
        };
        // out to the form…
        let out = editor_project_dto("api", project);
        assert_eq!(out.worktree_root.as_deref(), Some("~/work"));
        // …and back from the form must NOT drop worktree_root.
        let back = ProjectInputDto {
            name: out.name,
            host_id: out.host_id,
            path: out.path,
            workspace: out.workspace,
            agent: out.agent,
            base: out.base,
            worktree_root: out.worktree_root,
        }
        .into_project()
        .expect("into_project");
        assert_eq!(back.worktree_root.as_deref(), Some("~/work"));
    }

    #[test]
    fn host_worktree_root_survives_dto_round_trip() {
        // A TOML-set host-level `worktree_root` must not be silently cleared when
        // the user edits and saves a host through the config editor.
        let host = Host {
            name: Some("Dev box".into()),
            transport: Transport::Ssh(SshHost {
                host: "devbox".into(),
                user: None,
                port: None,
            }),
            worktree_root: Some("~/host-work".into()),
        };
        // out to the form…
        let out = editor_host_dto("devbox", host);
        assert_eq!(out.worktree_root.as_deref(), Some("~/host-work"));
        // …and back from the form must NOT drop worktree_root.
        let back = HostInputDto {
            name: out.name,
            transport: out.transport,
            worktree_root: out.worktree_root,
        }
        .into_host();
        assert_eq!(back.worktree_root.as_deref(), Some("~/host-work"));
    }

    #[test]
    fn agent_input_converts_to_agent() {
        let input: AgentInputDto =
            serde_json::from_str(r#"{"command":["claude","--flag"]}"#).expect("deserialize");
        assert_eq!(input.into_agent().command, vec!["claude", "--flag"]);
    }

    #[test]
    fn agent_input_maps_provision() {
        let dto = AgentInputDto {
            command: vec!["claude".into()],
            provision: Some(ProvisionFileDto {
                path: "~/.remora/hooks/claude-notify.sh".into(),
                content: "x".into(),
                mode: Some(0o755),
            }),
        };
        let a = dto.into_agent();
        let provision = a.provision.expect("p");
        assert_eq!(provision.path, "~/.remora/hooks/claude-notify.sh");
        assert_eq!(provision.content, "x");
        assert_eq!(provision.mode, Some(0o755));
    }

    #[test]
    fn agent_input_without_provision_deserializes_to_none() {
        // `provision` must be optional on the wire (existing configs/forms omit
        // it) — `#[serde(default)]` keeps old callers working.
        let input: AgentInputDto =
            serde_json::from_str(r#"{"command":["claude"]}"#).expect("deserialize");
        assert!(input.provision.is_none());
    }

    #[test]
    fn editor_agent_dto_round_trips_provision() {
        // A TOML-set agent `provision` must survive out to the form and back
        // through `into_agent`, matching the worktree_root/base patterns above.
        let mut config = Config::default();
        config.agents.insert(
            AgentId::new("claude").expect("id"),
            Agent {
                command: vec!["claude".into()],
                provision: Some(remora_core::config::ProvisionFile {
                    path: "~/.remora/hooks/claude-notify.sh".into(),
                    content: "#!/bin/sh\necho hi".into(),
                    mode: Some(0o755),
                }),
            },
        );

        // out to the form…
        let dto = EditorConfigDto::from(config);
        let out_provision = dto.agents[0].provision.clone().expect("provision dto");
        assert_eq!(out_provision.path, "~/.remora/hooks/claude-notify.sh");
        assert_eq!(out_provision.content, "#!/bin/sh\necho hi");
        assert_eq!(out_provision.mode, Some(0o755));

        // …and back from the form must NOT drop provision.
        let back = AgentInputDto {
            command: dto.agents[0].command.clone(),
            provision: dto.agents[0].provision.clone(),
        }
        .into_agent();
        let back_provision = back.provision.expect("provision survives round trip");
        assert_eq!(back_provision.path, "~/.remora/hooks/claude-notify.sh");
        assert_eq!(back_provision.mode, Some(0o755));
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
            worktree_root: None,
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
