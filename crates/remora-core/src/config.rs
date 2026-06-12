//! Per-device declarative configuration: hosts, projects, agent adapters.
//!
//! One human-editable TOML file per device (ADR-0004). Hosts and projects
//! are *configured* here; sessions are never stored — they are discovered
//! live from each host and joined back to this config. The app never
//! rewrites this file: types here are deserialize-only by design.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use remora_protocol::{AgentId, InvalidIdError, ProjectId};

/// Identifies a host in local config.
///
/// Hosts never cross the wire — how to *reach* a sandbox is inherently
/// client-side (ADR-0004) — so unlike `ProjectId`/`AgentId` this type lives
/// in `remora-core`, not the protocol crate. Same slug grammar as every
/// other id: lower-case `[a-z0-9-]+`, bounded length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub struct HostId(String);

impl HostId {
    /// Validates and wraps `value` under the shared id slug grammar.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdError> {
        let value = value.into();
        // Delegate validation to a protocol id type so the slug rules
        // (grammar, length cap, error hygiene) cannot drift from ADR-0004.
        ProjectId::new(value.as_str())?;
        Ok(Self(value))
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HostId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for HostId {
    type Err = InvalidIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for HostId {
    type Error = InvalidIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<HostId> for String {
    fn from(id: HostId) -> Self {
        id.0
    }
}

/// The whole per-device configuration: hosts, projects, agent adapters.
///
/// Maps are `BTreeMap` so iteration order is deterministic (sidebar render
/// order, stable tests). An absent section is an empty map — a brand-new
/// device with an empty file is a valid configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub hosts: BTreeMap<HostId, Host>,
    pub projects: BTreeMap<ProjectId, Project>,
    pub agents: BTreeMap<AgentId, Agent>,
}

/// A configured host: a transport plus its connection details (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// Optional display name; renames touch only this, never the id.
    pub name: Option<String>,
    pub transport: Transport,
}

/// How to reach a host. Adding a transport (e.g. `docker`, planned) should
/// force every `match` to update, so this is deliberately exhaustive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Ssh(SshHost),
    Kubectl(KubectlHost),
}

/// Connection details for an ssh host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// Any destination the `ssh` binary accepts, including a
    /// `~/.ssh/config` alias.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
}

/// Connection details for a `kubectl exec` host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubectlHost {
    pub pod: String,
    pub namespace: Option<String>,
    /// kubeconfig context; `None` uses the current context.
    pub context: Option<String>,
    pub container: Option<String>,
}

/// A directory on a host with a workspace mode and a default agent
/// (overridable per session via `SpawnSpec::agent`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// Optional display name; renames touch only this, never the id.
    pub name: Option<String>,
    /// Host this project lives on; must reference a configured host.
    pub host: HostId,
    /// Directory on the host. Must start with `/` or `~`; expansion of `~`
    /// is owned by the transport, never by a shell.
    pub path: String,
    pub workspace: WorkspaceMode,
    /// Default agent adapter; must reference a configured agent.
    pub agent: AgentId,
}

/// Workspace mode a project declares (ADR-0004). Required, not defaulted:
/// `shared` sessions can clobber each other, so the user opts in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// Each session gets a fresh git worktree + branch.
    Worktree,
    /// Sessions share the project directory (effectively single-writer).
    Shared,
}

/// Per-agent adapter data (ADR-0003): data, never code paths.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Launch command as an argv array — never joined into a shell string
    /// (ADR-0004).
    pub command: Vec<String>,
}

/// Why a config failed to load.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Syntax or shape error; the message carries line/column from the
    /// TOML parser.
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    /// The file parsed but is semantically invalid. Every issue found is
    /// reported, not just the first.
    #[error("{}", display_issues(.0))]
    Invalid(Vec<ValidationIssue>),
}

/// One semantic problem in an otherwise well-formed config file.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationIssue {
    #[error("host `{host}`: unknown transport `{transport}` (expected `ssh` or `kubectl`)")]
    UnknownTransport { host: HostId, transport: String },
    #[error("host `{host}`: transport `{transport}` requires `{field}`")]
    MissingHostField {
        host: HostId,
        transport: &'static str,
        field: &'static str,
    },
    #[error("host `{host}`: `{field}` does not apply to transport `{transport}`")]
    ForeignHostField {
        host: HostId,
        transport: &'static str,
        field: &'static str,
    },
    #[error("host `{host}`: `{field}` {reason}")]
    InvalidHostField {
        host: HostId,
        field: &'static str,
        reason: &'static str,
    },
    #[error("project `{project}`: `path` must start with `/` or `~` (got `{path}`)")]
    RelativeProjectPath { project: ProjectId, path: String },
    #[error("project `{project}`: unknown host `{host}` (configured hosts: {known})")]
    UnknownHost {
        project: ProjectId,
        host: HostId,
        known: String,
    },
    #[error("project `{project}`: unknown agent `{agent}` (configured agents: {known})")]
    UnknownAgent {
        project: ProjectId,
        agent: AgentId,
        known: String,
    },
    #[error("agent `{agent}`: `command` must be a non-empty argv array")]
    EmptyAgentCommand { agent: AgentId },
}

/// Renders configured ids for "unknown reference" messages, e.g.
/// `` `devbox`, `staging` `` — or `none` so a typo in an empty config still
/// gets a useful hint.
fn known_list<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let list: Vec<String> = ids.map(|id| format!("`{id}`")).collect();
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

fn display_issues(issues: &[ValidationIssue]) -> String {
    let n = issues.len();
    let mut out = format!(
        "invalid config ({n} problem{})",
        if n == 1 { "" } else { "s" }
    );
    for issue in issues {
        out.push_str("\n  - ");
        out.push_str(&issue.to_string());
    }
    out
}

/// Deserialization shape for one `[hosts.<id>]` table.
///
/// Carries the *union* of every transport's fields because serde cannot
/// combine tagged enums with `deny_unknown_fields`: this way a typo'd field
/// name is still rejected with line/column info, and fields that don't
/// belong to the declared transport are rejected during conversion with the
/// host named.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHost {
    name: Option<String>,
    transport: String,
    // ssh
    host: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    // kubectl
    pod: Option<String>,
    namespace: Option<String>,
    context: Option<String>,
    container: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    hosts: BTreeMap<HostId, RawHost>,
    #[serde(default)]
    projects: BTreeMap<ProjectId, Project>,
    #[serde(default)]
    agents: BTreeMap<AgentId, Agent>,
}

/// Records an issue for every field that is present but doesn't belong to
/// the declared transport.
fn reject_foreign(
    host: &HostId,
    transport: &'static str,
    foreign: &[(&'static str, bool)],
    issues: &mut Vec<ValidationIssue>,
) {
    for &(field, present) in foreign {
        if present {
            issues.push(ValidationIssue::ForeignHostField {
                host: host.clone(),
                transport,
                field,
            });
        }
    }
}

/// Unwraps a transport's required field, recording an issue if it is
/// missing. Value problems (blank, leading dash) are caught for every
/// present field by [`RawHost::check_field_values`].
fn require_field(
    host: &HostId,
    transport: &'static str,
    field: &'static str,
    value: Option<String>,
    issues: &mut Vec<ValidationIssue>,
) -> Option<String> {
    if value.is_none() {
        issues.push(ValidationIssue::MissingHostField {
            host: host.clone(),
            transport,
            field,
        });
    }
    value
}

impl RawHost {
    /// Validates every *present* field's value, independent of transport:
    /// blank strings fail loudly at config time instead of cryptically at
    /// connect time, and a leading `-` would be parsed as a flag by
    /// ssh/kubectl when the transport builds its argv (commands are built
    /// from config as argument arrays, ADR-0004).
    fn check_field_values(&self, id: &HostId, issues: &mut Vec<ValidationIssue>) {
        let string_fields = [
            ("host", self.host.as_deref()),
            ("user", self.user.as_deref()),
            ("pod", self.pod.as_deref()),
            ("namespace", self.namespace.as_deref()),
            ("context", self.context.as_deref()),
            ("container", self.container.as_deref()),
        ];
        for (field, value) in string_fields {
            let Some(value) = value else { continue };
            let reason = if value.trim().is_empty() {
                "must not be empty"
            } else if value.starts_with('-') {
                "must not start with `-`"
            } else {
                continue;
            };
            issues.push(ValidationIssue::InvalidHostField {
                host: id.clone(),
                field,
                reason,
            });
        }
        if self.port == Some(0) {
            issues.push(ValidationIssue::InvalidHostField {
                host: id.clone(),
                field: "port",
                reason: "must be between 1 and 65535",
            });
        }
    }

    /// Splits the raw table into the declared transport's typed config,
    /// recording every problem rather than stopping at the first. `None`
    /// always comes with at least one issue recorded.
    fn into_host(self, id: &HostId, issues: &mut Vec<ValidationIssue>) -> Option<Host> {
        self.check_field_values(id, issues);
        let transport = match self.transport.as_str() {
            "ssh" => {
                reject_foreign(
                    id,
                    "ssh",
                    &[
                        ("pod", self.pod.is_some()),
                        ("namespace", self.namespace.is_some()),
                        ("context", self.context.is_some()),
                        ("container", self.container.is_some()),
                    ],
                    issues,
                );
                require_field(id, "ssh", "host", self.host, issues).map(|host| {
                    Transport::Ssh(SshHost {
                        host,
                        user: self.user,
                        port: self.port,
                    })
                })
            }
            "kubectl" => {
                reject_foreign(
                    id,
                    "kubectl",
                    &[
                        ("host", self.host.is_some()),
                        ("user", self.user.is_some()),
                        ("port", self.port.is_some()),
                    ],
                    issues,
                );
                require_field(id, "kubectl", "pod", self.pod, issues).map(|pod| {
                    Transport::Kubectl(KubectlHost {
                        pod,
                        namespace: self.namespace,
                        context: self.context,
                        container: self.container,
                    })
                })
            }
            other => {
                issues.push(ValidationIssue::UnknownTransport {
                    host: id.clone(),
                    transport: other.to_string(),
                });
                None
            }
        };
        transport.map(|transport| Host {
            name: self.name,
            transport,
        })
    }
}

impl Config {
    /// Parses and validates a config file's contents.
    ///
    /// Syntax and shape errors surface with line/column information;
    /// semantic problems are all collected into a single
    /// [`ConfigError::Invalid`].
    pub fn from_toml_str(input: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(input)?;
        let mut issues = Vec::new();

        // References are checked against *declared* ids, captured before
        // conversion: a host that is configured but broken must produce its
        // own issue, not cascade into phantom unknown-host errors on every
        // project that references it.
        let declared_hosts: std::collections::BTreeSet<HostId> =
            raw.hosts.keys().cloned().collect();
        let known_hosts = known_list(declared_hosts.iter().map(HostId::as_str));
        let known_agents = known_list(raw.agents.keys().map(AgentId::as_str));

        let hosts: BTreeMap<HostId, Host> = raw
            .hosts
            .into_iter()
            .filter_map(|(id, raw_host)| {
                raw_host.into_host(&id, &mut issues).map(|host| (id, host))
            })
            .collect();

        for (id, project) in &raw.projects {
            if !declared_hosts.contains(&project.host) {
                issues.push(ValidationIssue::UnknownHost {
                    project: id.clone(),
                    host: project.host.clone(),
                    known: known_hosts.clone(),
                });
            }
            if !raw.agents.contains_key(&project.agent) {
                issues.push(ValidationIssue::UnknownAgent {
                    project: id.clone(),
                    agent: project.agent.clone(),
                    known: known_agents.clone(),
                });
            }
            if !(project.path.starts_with('/') || project.path.starts_with('~')) {
                issues.push(ValidationIssue::RelativeProjectPath {
                    project: id.clone(),
                    path: project.path.clone(),
                });
            }
        }

        for (id, agent) in &raw.agents {
            if agent.command.first().is_none_or(String::is_empty) {
                issues.push(ValidationIssue::EmptyAgentCommand { agent: id.clone() });
            }
        }

        let config = Config {
            hosts,
            projects: raw.projects,
            agents: raw.agents,
        };
        if issues.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Invalid(issues))
        }
    }

    /// Reads and parses the config file at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let input = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_accepts_lower_case_slugs() {
        for ok in ["devbox", "k8s-staging", "a", "host-2"] {
            let id = HostId::new(ok).expect("valid slug");
            assert_eq!(id.as_str(), ok);
        }
    }

    #[test]
    fn host_id_rejects_invalid_slugs() {
        for bad in ["", "Devbox", "dev_box", "dev box", "dev.box", "héte"] {
            assert!(HostId::new(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn host_id_display_and_string_conversion() {
        let id = HostId::new("devbox").expect("valid slug");
        assert_eq!(id.to_string(), "devbox");
        assert_eq!(String::from(id), "devbox");
    }

    #[test]
    fn host_id_error_names_the_offender() {
        let err = HostId::new("Dev_Box").expect_err("invalid slug");
        assert!(err.to_string().contains("Dev_Box"));
        let _: &dyn std::error::Error = &err;
    }

    const FULL: &str = r#"
        [hosts.devbox]
        name = "Dev box"
        transport = "ssh"
        host = "devbox.example.com"
        user = "dev"
        port = 2222

        [hosts.staging]
        transport = "kubectl"
        pod = "sandbox-0"
        namespace = "agents"
        context = "staging-cluster"
        container = "main"

        [projects.api]
        name = "API server"
        host = "devbox"
        path = "/home/dev/api"
        workspace = "worktree"
        agent = "claude"

        [agents.claude]
        command = ["claude", "--continue"]
    "#;

    fn host_id(s: &str) -> HostId {
        HostId::new(s).expect("valid slug")
    }

    #[test]
    fn empty_config_is_valid_and_empty() {
        let config = Config::from_toml_str("").expect("empty config parses");
        assert!(config.hosts.is_empty());
        assert!(config.projects.is_empty());
        assert!(config.agents.is_empty());
    }

    #[test]
    fn parses_a_full_config() {
        let config = Config::from_toml_str(FULL).expect("full config parses");

        let devbox = &config.hosts[&host_id("devbox")];
        assert_eq!(devbox.name.as_deref(), Some("Dev box"));
        let Transport::Ssh(ssh) = &devbox.transport else {
            panic!("devbox should be ssh");
        };
        assert_eq!(ssh.host, "devbox.example.com");
        assert_eq!(ssh.user.as_deref(), Some("dev"));
        assert_eq!(ssh.port, Some(2222));

        let staging = &config.hosts[&host_id("staging")];
        assert_eq!(staging.name, None);
        let Transport::Kubectl(k8s) = &staging.transport else {
            panic!("staging should be kubectl");
        };
        assert_eq!(k8s.pod, "sandbox-0");
        assert_eq!(k8s.namespace.as_deref(), Some("agents"));
        assert_eq!(k8s.context.as_deref(), Some("staging-cluster"));
        assert_eq!(k8s.container.as_deref(), Some("main"));

        let api = &config.projects[&ProjectId::new("api").expect("valid slug")];
        assert_eq!(api.name.as_deref(), Some("API server"));
        assert_eq!(api.host, host_id("devbox"));
        assert_eq!(api.path, "/home/dev/api");
        assert_eq!(api.workspace, WorkspaceMode::Worktree);
        assert_eq!(api.agent, AgentId::new("claude").expect("valid slug"));

        let claude = &config.agents[&AgentId::new("claude").expect("valid slug")];
        assert_eq!(claude.command, vec!["claude", "--continue"]);
    }

    #[test]
    fn optional_fields_default_to_none() {
        let config = Config::from_toml_str(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            "#,
        )
        .expect("minimal ssh host parses");
        let devbox = &config.hosts[&host_id("devbox")];
        assert_eq!(devbox.name, None);
        let Transport::Ssh(ssh) = &devbox.transport else {
            panic!("devbox should be ssh");
        };
        assert_eq!(ssh.user, None);
        assert_eq!(ssh.port, None);
    }

    #[test]
    fn shared_workspace_mode_parses() {
        let config = Config::from_toml_str(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.scratch]
            host = "devbox"
            path = "~/scratch"
            workspace = "shared"
            agent = "claude"

            [agents.claude]
            command = ["claude"]
            "#,
        )
        .expect("shared project parses");
        let scratch = &config.projects[&ProjectId::new("scratch").expect("valid slug")];
        assert_eq!(scratch.workspace, WorkspaceMode::Shared);
    }

    #[test]
    fn rejects_invalid_id_keys() {
        for (section, key) in [
            ("hosts", "Devbox"),
            ("hosts", "dev_box"),
            ("projects", "My_Project"),
            ("agents", "claude code"),
        ] {
            let toml = format!("[{section}.\"{key}\"]\n");
            let err = Config::from_toml_str(&toml).expect_err("invalid id key");
            let msg = err.to_string();
            assert!(
                msg.contains("lower-case slug"),
                "{section}.{key}: message should explain the slug rule: {msg}"
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_with_location() {
        // Typos in field names must fail loudly, not be ignored.
        let err = Config::from_toml_str(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            prot = 22
            "#,
        )
        .expect_err("typo'd field");
        let msg = err.to_string();
        assert!(msg.contains("prot"), "names the unknown field: {msg}");
        assert!(msg.contains("line"), "carries a location: {msg}");
    }

    #[test]
    fn rejects_unknown_top_level_sections() {
        let err = Config::from_toml_str("[hots.devbox]\ntransport = \"ssh\"\n")
            .expect_err("typo'd section");
        assert!(err.to_string().contains("hots"), "{err}");
    }

    #[test]
    fn rejects_unknown_workspace_mode_naming_alternatives() {
        let err = Config::from_toml_str(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.api]
            host = "devbox"
            path = "/api"
            workspace = "worktre"
            agent = "claude"

            [agents.claude]
            command = ["claude"]
            "#,
        )
        .expect_err("bad workspace mode");
        let msg = err.to_string();
        assert!(msg.contains("worktree") && msg.contains("shared"), "{msg}");
    }

    #[test]
    fn parse_errors_carry_line_information() {
        let err = Config::from_toml_str("[hosts.devbox\n").expect_err("syntax error");
        assert!(err.to_string().contains("line"), "{err}");
    }

    /// Parses a config expected to be semantically invalid and returns its
    /// issues.
    fn issues_of(toml: &str) -> Vec<ValidationIssue> {
        match Config::from_toml_str(toml).expect_err("config should be invalid") {
            ConfigError::Invalid(issues) => issues,
            other => panic!("expected Invalid, got: {other}"),
        }
    }

    #[test]
    fn rejects_unknown_transport_naming_alternatives() {
        let issues = issues_of("[hosts.devbox]\ntransport = \"telnet\"\n");
        assert_eq!(issues.len(), 1);
        let msg = issues[0].to_string();
        assert!(msg.contains("devbox"), "{msg}");
        assert!(msg.contains("telnet"), "{msg}");
        assert!(msg.contains("ssh") && msg.contains("kubectl"), "{msg}");
    }

    #[test]
    fn rejects_ssh_host_missing_destination() {
        let issues = issues_of("[hosts.devbox]\ntransport = \"ssh\"\n");
        assert_eq!(issues.len(), 1);
        let msg = issues[0].to_string();
        assert!(msg.contains("devbox") && msg.contains("host"), "{msg}");
    }

    #[test]
    fn rejects_kubectl_host_missing_pod() {
        let issues = issues_of("[hosts.staging]\ntransport = \"kubectl\"\n");
        assert_eq!(issues.len(), 1);
        let msg = issues[0].to_string();
        assert!(msg.contains("staging") && msg.contains("pod"), "{msg}");
    }

    #[test]
    fn rejects_fields_foreign_to_the_transport() {
        let issues =
            issues_of("[hosts.staging]\ntransport = \"kubectl\"\npod = \"sandbox-0\"\nport = 22\n");
        assert_eq!(issues.len(), 1);
        let msg = issues[0].to_string();
        assert!(
            msg.contains("port") && msg.contains("kubectl"),
            "should say `port` doesn't apply to kubectl: {msg}"
        );
    }

    #[test]
    fn rejects_empty_required_host_fields() {
        let issues = issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"\"\n");
        assert_eq!(issues.len(), 1);
        let msg = issues[0].to_string();
        assert!(msg.contains("devbox") && msg.contains("empty"), "{msg}");
    }

    #[test]
    fn rejects_project_referencing_unknown_host_and_agent() {
        let issues = issues_of(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.api]
            host = "devbo"
            path = "/api"
            workspace = "worktree"
            agent = "calude"

            [agents.claude]
            command = ["claude"]
            "#,
        );
        // Both dangling references reported at once.
        assert_eq!(issues.len(), 2, "{issues:?}");
        let host_msg = issues[0].to_string();
        assert!(
            host_msg.contains("api") && host_msg.contains("devbo"),
            "{host_msg}"
        );
        assert!(
            host_msg.contains("devbox"),
            "should list configured hosts: {host_msg}"
        );
        let agent_msg = issues[1].to_string();
        assert!(
            agent_msg.contains("api") && agent_msg.contains("calude"),
            "{agent_msg}"
        );
        assert!(
            agent_msg.contains("claude"),
            "should list configured agents: {agent_msg}"
        );
    }

    #[test]
    fn rejects_relative_project_paths() {
        let issues = issues_of(
            r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.api]
            host = "devbox"
            path = "code/api"
            workspace = "worktree"
            agent = "claude"

            [agents.claude]
            command = ["claude"]
            "#,
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = issues[0].to_string();
        assert!(msg.contains("api") && msg.contains("code/api"), "{msg}");
    }

    #[test]
    fn rejects_empty_agent_command() {
        let issues = issues_of("[agents.claude]\ncommand = []\n");
        assert_eq!(issues.len(), 1);
        assert!(issues[0].to_string().contains("claude"), "{issues:?}");

        let issues = issues_of("[agents.claude]\ncommand = [\"\"]\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
    }

    #[test]
    fn collects_every_issue_in_one_error() {
        let err = Config::from_toml_str(
            r#"
            [hosts.devbox]
            transport = "telnet"

            [projects.api]
            host = "devbox"
            path = "code/api"
            workspace = "worktree"
            agent = "claude"

            [agents.claude]
            command = []
            "#,
        )
        .expect_err("config should be invalid");
        let ConfigError::Invalid(issues) = &err else {
            panic!("expected Invalid, got: {err}");
        };
        // telnet transport, relative path, empty command — and the project's
        // host reference stays valid because `devbox` *is* configured, just
        // broken: a broken host must not cascade into phantom unknown-host
        // errors.
        assert_eq!(issues.len(), 3, "{issues:?}");
        let msg = err.to_string();
        assert!(msg.contains("3 problems"), "{msg}");
        assert!(msg.lines().count() >= 4, "one issue per line: {msg}");
    }

    #[test]
    fn rejects_blank_host_string_fields() {
        // Whitespace-only is as broken as empty — ssh'ing to "   " fails
        // cryptically at connect time instead of loudly at config time.
        let issues = issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"   \"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("empty"), "{issues:?}");

        // Optional fields are validated too when present.
        let issues =
            issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\nuser = \"\"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = issues[0].to_string();
        assert!(msg.contains("user") && msg.contains("empty"), "{msg}");
    }

    #[test]
    fn rejects_host_string_fields_with_a_leading_dash() {
        // A value like `-oProxyCommand=...` would be parsed as a flag by
        // ssh/kubectl when the transport builds its argv (ADR-0004 builds
        // commands from config as argument arrays); fail at config time.
        for toml in [
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"-oproxycommand=evil\"\n",
            "[hosts.staging]\ntransport = \"kubectl\"\npod = \"sandbox\"\nnamespace = \"--token=x\"\n",
        ] {
            let issues = issues_of(toml);
            assert_eq!(issues.len(), 1, "{issues:?}");
            let msg = issues[0].to_string();
            assert!(msg.contains('-'), "should explain the dash rule: {msg}");
        }
    }

    #[test]
    fn rejects_port_zero() {
        let issues =
            issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\nport = 0\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = issues[0].to_string();
        assert!(msg.contains("port"), "{msg}");
    }

    #[test]
    fn load_reads_a_config_file() {
        let path =
            std::env::temp_dir().join(format!("remora-config-test-{}.toml", std::process::id()));
        std::fs::write(&path, FULL).expect("write temp config");
        let config = Config::load(&path).expect("load temp config");
        std::fs::remove_file(&path).ok();
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn load_names_the_path_on_io_error() {
        let path = std::env::temp_dir().join("remora-config-test-does-not-exist.toml");
        let err = Config::load(&path).expect_err("missing file");
        let msg = err.to_string();
        assert!(
            msg.contains("remora-config-test-does-not-exist.toml"),
            "{msg}"
        );
        assert!(matches!(err, ConfigError::Io { .. }));
    }
}
