//! Per-device declarative configuration: hosts, projects, agent adapters.
//!
//! One human-editable TOML file per device (ADR-0004). Hosts and projects
//! are *configured* here; sessions are never stored — they are discovered
//! live from each host and joined back to this config.
//!
//! The types here are the validated, read-only *view*. The app can also
//! *write* this file (add/edit/remove hosts, projects, agents) through
//! [`ConfigDocument`], which preserves comments and re-uses the validation
//! below as its single source of truth — see ADR-0006. (This supersedes the
//! original "deserialize-only, never rewritten" stance from ADR-0004.)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use remora_protocol::{AgentId, InvalidIdError, ProjectId, WorkspaceMode};

pub mod document;
pub use document::{ConfigDocument, PresentIds};

/// Location of the per-device config file *relative to the OS config dir*
/// (ADR-0004: one human-editable TOML per device). The subdir + filename are
/// owned here so every client resolves the same `remora/config.toml` — the
/// desktop shell supplies the platform base (e.g. `~/.config`), a future relay
/// reuses this constant. Deliberately a predictable name, not a bundle id, so a
/// human can find and edit it.
pub const CONFIG_FILE_RELPATH: &str = "remora/config.toml";

/// Joins `base` (an OS config dir) with [`CONFIG_FILE_RELPATH`].
///
/// The caller owns choosing `base` (the platform config dir is a
/// runtime/shell concern); core owns only the suffix, so the path can't drift
/// between clients.
pub fn config_file_path(base: impl AsRef<Path>) -> PathBuf {
    base.as_ref().join(CONFIG_FILE_RELPATH)
}

/// Identifies a host in local config.
///
/// Hosts never cross the wire — how to *reach* a sandbox is inherently
/// client-side (ADR-0004) — so unlike `ProjectId`/`AgentId` this type lives
/// in `remora-core`, not the protocol crate. Same slug grammar as every
/// other id: lower-case `[a-z0-9-]+`, bounded length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
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
    /// Host-wide default worktree-root (#124); a project or session value wins.
    pub worktree_root: Option<String>,
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

/// A kubectl connection field: either a literal value used verbatim as one
/// argv token, or a user-authored shell command line resolved LOCALLY at
/// connect time (its trimmed stdout becomes the token). The `Command` form is
/// the single, opt-in crossing of ADR-0004's "config is never shell-evaluated"
/// line; `Literal` keeps the existing guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubectlField {
    Literal(String),
    Command(String),
}

/// Connection details for a `kubectl exec` host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubectlHost {
    pub pod: KubectlField,
    pub namespace: Option<KubectlField>,
    /// kubeconfig context; `None` uses the current context.
    pub context: Option<KubectlField>,
    pub container: Option<KubectlField>,
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
    /// Optional default git start-point for new worktrees (#54), e.g.
    /// `origin/develop`. Omitted = detect the remote default branch.
    #[serde(default)]
    pub base: Option<String>,
    /// Optional default worktree-root for new sessions (#124); a per-session
    /// `SpawnSpec::worktree_root` overrides it. Omitted = host default, then
    /// the `~/.remora/worktrees/<project>` convention.
    #[serde(default)]
    pub worktree_root: Option<String>,
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
///
/// `#[non_exhaustive]`: later stages add validation rules and load-time
/// failure modes, so downstream `match`es must keep a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// A rejected in-app edit (insert/update/remove). Carries the rendered,
    /// already-sanitized reason. Distinct from `Invalid` (a whole-file load
    /// failure) so the editor channel can surface it on the offending form.
    #[error("{0}")]
    Edit(String),
    /// The format-preserving editor could not parse the document. After
    /// [`Config::from_toml_str`] accepts the input this should not occur; it
    /// exists so the editor never has to unwrap a `toml_edit` parse.
    #[error("config document parse error: {0}")]
    DocumentParse(String),
}

/// One semantic problem in an otherwise well-formed config file.
///
/// Free-form config strings carried here (`transport`, `path`) are stored
/// pre-sanitized via [`sanitized`]: these messages are logged, so they must
/// not relay terminal escapes or unbounded lengths from a pasted config.
///
/// `#[non_exhaustive]`: stages 4-6 add new validation rules, so downstream
/// `match`es must keep a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
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
    #[error("project `{project}`: `{field}` {reason}")]
    InvalidProjectField {
        project: ProjectId,
        field: &'static str,
        reason: &'static str,
    },
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
    #[error("agent `{agent}`: `command` {reason}")]
    InvalidAgentCommand {
        agent: AgentId,
        reason: &'static str,
    },
}

/// Escapes control bytes and bounds the length of a config value echoed in
/// an error message — the same hygiene as the protocol's `InvalidIdError`
/// Display, because these messages end up in logs.
fn sanitized(value: &str) -> String {
    const MAX_ECHOED_CHARS: usize = 64;
    let mut out: String = value
        .chars()
        .take(MAX_ECHOED_CHARS)
        .flat_map(char::escape_default)
        .collect();
    if value.chars().nth(MAX_ECHOED_CHARS).is_some() {
        out.push('…');
    }
    out
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
    // A generated config can carry thousands of issues; keep the rendered
    // error bounded for whatever logs or displays it.
    const MAX_DISPLAYED_ISSUES: usize = 20;
    let n = issues.len();
    let mut out = format!(
        "invalid config ({n} problem{})",
        if n == 1 { "" } else { "s" }
    );
    for issue in issues.iter().take(MAX_DISPLAYED_ISSUES) {
        out.push_str("\n  - ");
        out.push_str(&issue.to_string());
    }
    if n > MAX_DISPLAYED_ISSUES {
        out.push_str(&format!("\n  … and {} more", n - MAX_DISPLAYED_ISSUES));
    }
    out
}

/// Raw kubectl field as authored in TOML: a bare string (literal) or a
/// `{ command = "…" }` table. A hand-written `Deserialize` is required: serde
/// SILENTLY IGNORES `#[serde(deny_unknown_fields)]` inside an `#[serde(untagged)]`
/// variant (it buffers into `Content` first), so a typo'd inner key would parse
/// and be dropped — breaking the loud-typo invariant `RawHost` relies on. The
/// Visitor rejects unknown keys with TOML line/column, exactly like
/// `deny_unknown_fields` elsewhere.
#[derive(Debug)]
enum RawKubectlField {
    Literal(String),
    Command(String),
}

impl From<RawKubectlField> for KubectlField {
    fn from(raw: RawKubectlField) -> Self {
        match raw {
            RawKubectlField::Literal(v) => KubectlField::Literal(v),
            RawKubectlField::Command(c) => KubectlField::Command(c),
        }
    }
}

impl<'de> Deserialize<'de> for RawKubectlField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, MapAccess, Visitor};
        use std::fmt;

        struct FieldVisitor;

        impl<'de> Visitor<'de> for FieldVisitor {
            type Value = RawKubectlField;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or a { command = \"…\" } table")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(RawKubectlField::Literal(v.to_owned()))
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut command: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "command" => {
                            if command.is_some() {
                                return Err(de::Error::duplicate_field("command"));
                            }
                            command = Some(map.next_value()?);
                        }
                        other => return Err(de::Error::unknown_field(other, &["command"])),
                    }
                }
                command
                    .map(RawKubectlField::Command)
                    .ok_or_else(|| de::Error::missing_field("command"))
            }
        }

        deserializer.deserialize_any(FieldVisitor)
    }
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
    pod: Option<RawKubectlField>,
    namespace: Option<RawKubectlField>,
    context: Option<RawKubectlField>,
    container: Option<RawKubectlField>,
    #[serde(default)]
    worktree_root: Option<String>,
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

/// The guard for a value used verbatim as one argv token (literal fields AND
/// re-validated resolved command output): non-empty, no control chars, no edge
/// whitespace, no leading `-` (which ssh/kubectl would parse as a flag). Single
/// source of truth so config-time and resolve-time checks cannot drift.
pub(crate) fn literal_field_problem(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        Some("must not be empty")
    } else if value.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else if value != value.trim() {
        Some("must not have leading or trailing whitespace")
    } else if value.starts_with('-') {
        Some("must not start with `-`")
    } else {
        None
    }
}

/// The looser guard for a `{ command }` field at config time: it is a shell
/// command line, so dashes and internal/edge whitespace are allowed; only an
/// empty/whitespace-only command, control characters, or a value fully wrapped
/// in command substitution (`$(...)` or backticks) fail. The field holds the
/// command itself, so wrapping the whole thing in substitution is
/// double-evaluation: `sh -c` substitutes the inner pipeline and then runs its
/// output (e.g. a pod name) as a command, surfacing a misleading
/// `sh: pod/...: No such file or directory` at resolve time. An *interior*
/// `$(...)` (resolving part of the pipeline inline) is legitimate shell and is
/// intentionally allowed. See #127.
pub(crate) fn command_field_problem(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Some("must not be empty")
    } else if value.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else if is_fully_command_substituted(trimmed) {
        Some("must be the command itself, not wrapped in $(...) or backticks")
    } else {
        None
    }
}

/// Whether `trimmed` (already-trimmed, control-free) is one whole command
/// substitution — `$(...)` or `` `...` `` — rather than a bare command line.
/// This is the #127 double-evaluation mistake. Matched by exact outer shape:
/// an *interior* substitution (`kubectl -n $(cat ns) get …`) does not start
/// with the opener, so it is correctly left alone. The trailing-`)`/backtick
/// forms with extra suffix (`$(...)x`) are deliberately not caught here — they
/// are rare and still surface the legible resolve-time error.
fn is_fully_command_substituted(trimmed: &str) -> bool {
    (trimmed.starts_with("$(") && trimmed.ends_with(')'))
        || (trimmed.len() >= 2 && trimmed.starts_with('`') && trimmed.ends_with('`'))
}

/// Validates an optional display name (hosts and projects), returning the
/// problem if there is one. Names render verbatim in the sidebar, so blank
/// or control-character names fail at config time like every other field.
fn check_display_name(name: Option<&str>) -> Option<&'static str> {
    let name = name?;
    if name.trim().is_empty() {
        Some("must not be empty")
    } else if name.chars().any(char::is_control) {
        Some("must not contain control characters")
    } else {
        None
    }
}

/// Whether an argv element begins with a Unicode dash that *isn't* ASCII
/// hyphen-minus — the autocorrect/paste hazard that turns `--flag` into
/// `—flag`. Covers the Unicode `Dash_Punctuation` (Pd) code points an editor
/// or keyboard is plausibly substituting for `-`; ASCII `-` (U+002D) is
/// deliberately excluded so real flags pass. std exposes no Unicode-category
/// query, so the set is enumerated explicitly (Pd is small and stable).
fn starts_with_unicode_dash(arg: &str) -> bool {
    matches!(
        arg.trim_start().chars().next(),
        Some(
            '\u{058A}' // ARMENIAN HYPHEN
                | '\u{05BE}' // HEBREW PUNCTUATION MAQAF
                | '\u{1400}' // CANADIAN SYLLABICS HYPHEN
                | '\u{1806}' // MONGOLIAN TODO SOFT HYPHEN
                | '\u{2010}' // HYPHEN
                | '\u{2011}' // NON-BREAKING HYPHEN
                | '\u{2012}' // FIGURE DASH
                | '\u{2013}' // EN DASH
                | '\u{2014}' // EM DASH
                | '\u{2015}' // HORIZONTAL BAR
                | '\u{2E17}' // DOUBLE OBLIQUE HYPHEN
                | '\u{2E1A}' // HYPHEN WITH DIAERESIS
                | '\u{2E3A}' // TWO-EM DASH
                | '\u{2E3B}' // THREE-EM DASH
                | '\u{2E40}' // DOUBLE HYPHEN
                | '\u{301C}' // WAVE DASH
                | '\u{3030}' // WAVY DASH
                | '\u{30A0}' // KATAKANA-HIRAGANA DOUBLE HYPHEN
                | '\u{FE31}' // PRESENTATION FORM FOR VERTICAL EM DASH
                | '\u{FE32}' // PRESENTATION FORM FOR VERTICAL EN DASH
                | '\u{FE58}' // SMALL EM DASH
                | '\u{FE63}' // SMALL HYPHEN-MINUS
                | '\u{FF0D}' // FULLWIDTH HYPHEN-MINUS
        )
    )
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
fn require_field<T>(
    host: &HostId,
    transport: &'static str,
    field: &'static str,
    value: Option<T>,
    issues: &mut Vec<ValidationIssue>,
) -> Option<T> {
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
    /// blank strings, control characters, and edge whitespace fail loudly
    /// at config time instead of cryptically at connect time, and a leading
    /// `-` would be parsed as a flag by ssh/kubectl when the transport
    /// builds its argv (commands are built from config as argument arrays,
    /// ADR-0004). Whitespace is checked before the dash so a value like
    /// `" -oproxycommand=…"` cannot dodge the flag guard.
    fn check_field_values(&self, id: &HostId, issues: &mut Vec<ValidationIssue>) {
        let string_fields = [
            ("host", self.host.as_deref()),
            ("user", self.user.as_deref()),
        ];
        for (field, value) in string_fields {
            let Some(value) = value else { continue };
            if let Some(reason) = literal_field_problem(value) {
                issues.push(ValidationIssue::InvalidHostField {
                    host: id.clone(),
                    field,
                    reason,
                });
            }
        }

        let kube_fields = [
            ("pod", self.pod.as_ref()),
            ("namespace", self.namespace.as_ref()),
            ("context", self.context.as_ref()),
            ("container", self.container.as_ref()),
        ];
        for (field, value) in kube_fields {
            let Some(raw) = value else { continue };
            let reason = match raw {
                RawKubectlField::Literal(v) => literal_field_problem(v),
                RawKubectlField::Command(c) => command_field_problem(c),
            };
            if let Some(reason) = reason {
                issues.push(ValidationIssue::InvalidHostField {
                    host: id.clone(),
                    field,
                    reason,
                });
            }
        }

        if let Some(reason) = check_display_name(self.name.as_deref()) {
            issues.push(ValidationIssue::InvalidHostField {
                host: id.clone(),
                field: "name",
                reason,
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
                // Range-checked only here, where `port` belongs: on other
                // transports it is already reported as foreign, and a
                // second "pick a valid port" message would contradict it.
                if self.port == Some(0) {
                    issues.push(ValidationIssue::InvalidHostField {
                        host: id.clone(),
                        field: "port",
                        reason: "must be between 1 and 65535",
                    });
                }
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
                        pod: pod.into(),
                        namespace: self.namespace.map(Into::into),
                        context: self.context.map(Into::into),
                        container: self.container.map(Into::into),
                    })
                })
            }
            other => {
                issues.push(ValidationIssue::UnknownTransport {
                    host: id.clone(),
                    transport: sanitized(other),
                });
                None
            }
        };
        transport.map(|transport| Host {
            name: self.name,
            transport,
            worktree_root: self.worktree_root,
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
            let path = project.path.as_str();
            if !(path.starts_with('/') || path.starts_with('~')) {
                issues.push(ValidationIssue::RelativeProjectPath {
                    project: id.clone(),
                    path: sanitized(path),
                });
            } else if path.chars().any(char::is_control) {
                issues.push(ValidationIssue::InvalidProjectField {
                    project: id.clone(),
                    field: "path",
                    reason: "must not contain control characters",
                });
            } else if path != "~" && path.starts_with('~') && !path.starts_with("~/") {
                // Transports only promise `~/` expansion (ADR-0004); a
                // `~user` form would be silently mangled, so fail closed
                // until a transport actually supports it.
                issues.push(ValidationIssue::InvalidProjectField {
                    project: id.clone(),
                    field: "path",
                    reason: "must start with `/`, `~`, or `~/` (`~user` paths are not supported)",
                });
            }
            if let Some(reason) = check_display_name(project.name.as_deref()) {
                issues.push(ValidationIssue::InvalidProjectField {
                    project: id.clone(),
                    field: "name",
                    reason,
                });
            }
            if let Some(base) = project.base.as_deref() {
                let reason = if base.trim().is_empty() {
                    Some("must not be empty (omit the key instead)")
                } else if base.chars().any(char::is_control) {
                    Some("must not contain control characters")
                } else if base.trim_start().starts_with('-') {
                    Some("must not start with `-`")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    issues.push(ValidationIssue::InvalidProjectField {
                        project: id.clone(),
                        field: "base",
                        reason,
                    });
                }
            }
        }

        for (id, agent) in &raw.agents {
            // An empty argv (`command = []`) is the explicit "no agent / plain
            // shell" case (#35) and is allowed. A *non-empty* command with a
            // blank/whitespace element is still a typo (a hole in a real
            // command), and control characters are always rejected.
            let reason = if !agent.command.is_empty()
                && agent.command.iter().any(|arg| arg.trim().is_empty())
            {
                Some("must not contain blank elements (use an empty array `[]` for a plain shell)")
            } else if agent
                .command
                .iter()
                .any(|arg| arg.chars().any(char::is_control))
            {
                Some("must not contain control characters")
            } else if agent
                .command
                .iter()
                .any(|arg| starts_with_unicode_dash(arg))
            {
                // Autocorrect/paste turns `--flag` into `—flag` (em-dash). The
                // agent CLI only recognizes ASCII hyphen-minus, so a leading
                // Unicode dash is silently swallowed as a prompt instead of a
                // flag. Reject it here rather than let it surface as the
                // baffling "the flag became my prompt" runtime symptom.
                Some("must use ASCII `-`/`--` for flags, not a Unicode dash (e.g. — or –)")
            } else {
                None
            };
            if let Some(reason) = reason {
                issues.push(ValidationIssue::InvalidAgentCommand {
                    agent: id.clone(),
                    reason,
                });
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
    ///
    /// Refuses non-regular files (a FIFO at the config path would block a
    /// desktop app forever) and files over [`MAX_CONFIG_BYTES`] — a config
    /// is hand-written; anything that size is the wrong file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        /// Upper bound on a plausible hand-edited config file.
        const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

        let path = path.as_ref();
        let io_err = |source: std::io::Error| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        let meta = std::fs::metadata(path).map_err(io_err)?;
        if !meta.is_file() {
            return Err(io_err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            )));
        }
        if meta.len() > MAX_CONFIG_BYTES {
            return Err(io_err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "file is {} bytes; refusing to read more than {MAX_CONFIG_BYTES}",
                    meta.len()
                ),
            )));
        }
        let input = std::fs::read_to_string(path).map_err(io_err)?;
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
        assert_eq!(k8s.pod, KubectlField::Literal("sandbox-0".into()));
        assert_eq!(k8s.namespace, Some(KubectlField::Literal("agents".into())));
        assert_eq!(
            k8s.context,
            Some(KubectlField::Literal("staging-cluster".into()))
        );
        assert_eq!(k8s.container, Some(KubectlField::Literal("main".into())));

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
    fn allows_empty_command_as_plain_shell() {
        // An empty argv is the explicit "no agent / plain shell" case (#35).
        let cfg = Config::from_toml_str("[agents.shell]\ncommand = []\n")
            .expect("empty command is a valid plain shell");
        let shell_id = AgentId::new("shell").expect("valid agent id");
        assert!(cfg.agents.contains_key(&shell_id));
        assert!(cfg.agents[&shell_id].command.is_empty());
    }

    #[test]
    fn rejects_blank_elements_in_a_nonempty_command() {
        // A hole in an otherwise-real command is a typo, not a plain shell.
        let issues = issues_of("[agents.claude]\ncommand = [\"\"]\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("claude"), "{issues:?}");

        let issues = issues_of("[agents.claude]\ncommand = [\"  \"]\n");
        assert_eq!(issues.len(), 1, "{issues:?}");

        let issues = issues_of("[agents.claude]\ncommand = [\"claude\", \"\", \"--continue\"]\n");
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
            command = [""]
            "#,
        )
        .expect_err("config should be invalid");
        let ConfigError::Invalid(issues) = &err else {
            panic!("expected Invalid, got: {err}");
        };
        // telnet transport, relative path, and a blank command element
        // (`command = [""]` — a hole in a non-empty argv, distinct from the now
        // valid empty `[]` plain shell) — and the project's host reference stays
        // valid because `devbox` *is* configured, just broken: a broken host
        // must not cascade into phantom unknown-host errors.
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
    fn config_file_path_joins_base_with_relpath() {
        let base = Path::new("/home/u/.config");
        assert_eq!(
            config_file_path(base),
            PathBuf::from("/home/u/.config/remora/config.toml")
        );
        // The relpath is the documented, human-findable location.
        assert_eq!(CONFIG_FILE_RELPATH, "remora/config.toml");
    }

    #[test]
    fn load_reads_a_config_file() {
        let path =
            std::env::temp_dir().join(format!("remora-config-test-{}.toml", std::process::id()));
        std::fs::write(&path, FULL).expect("write temp config");
        // Hold the result so the temp file is removed even when the
        // assertions below panic.
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        let config = result.expect("load temp config");
        assert_eq!(config.hosts.len(), 2);
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn load_caps_config_file_size() {
        let path = std::env::temp_dir().join(format!(
            "remora-config-test-huge-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "#".repeat(2 * 1024 * 1024)).expect("write huge file");
        let result = Config::load(&path);
        std::fs::remove_file(&path).ok();
        let err = result.expect_err("oversized config");
        assert!(matches!(err, ConfigError::Io { .. }), "{err}");
        assert!(err.to_string().contains("remora-config-test-huge"), "{err}");
    }

    #[test]
    fn rejects_blank_agent_command_elements() {
        // Whitespace-only argv elements are invalid in command arrays.
        let issues = issues_of("[agents.claude]\ncommand = [\"   \"]\n");
        assert_eq!(issues.len(), 1, "{issues:?}");

        // Control characters are never legitimate in a launch command.
        let issues = issues_of("[agents.claude]\ncommand = [\"claude\\n--evil\"]\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("control"), "{issues:?}");
    }

    #[test]
    fn rejects_argv_element_starting_with_a_unicode_dash() {
        // Autocorrect/paste turns `--flag` into `—flag` (em-dash, U+2014). The
        // agent CLI's parser only recognizes ASCII hyphen-minus, so a leading
        // Unicode dash is silently taken as the prompt, not the flag. Catch it
        // at config time instead of as a baffling runtime symptom.
        for dash in ["\u{2014}", "\u{2013}", "\u{2012}", "\u{2010}", "\u{2015}"] {
            let toml =
                format!("[agents.claude]\ncommand = [\"claude\", \"{dash}dangerously-skip\"]\n");
            let issues = issues_of(&toml);
            assert_eq!(issues.len(), 1, "{dash:?}: {issues:?}");
            assert!(
                issues[0].to_string().contains("Unicode dash"),
                "{dash:?}: {issues:?}"
            );
        }

        // ASCII flags stay valid — the guard must not flag legitimate `--`/`-`.
        Config::from_toml_str(
            "[agents.claude]\ncommand = [\"claude\", \"--dangerously-skip\", \"-r\"]\n",
        )
        .expect("ASCII flags are valid");

        // A Unicode dash mid-token (not the flag prefix) is left alone — the bug
        // is specifically a confusable *leading* dash misread as a flag start.
        Config::from_toml_str("[agents.claude]\ncommand = [\"claude\", \"a\u{2014}b\"]\n")
            .expect("a non-leading dash is not a flag confusable");
    }

    #[test]
    fn kubectl_optional_fields_default_to_none() {
        let config = Config::from_toml_str(
            "[hosts.staging]\ntransport = \"kubectl\"\npod = \"sandbox-0\"\n",
        )
        .expect("minimal kubectl host parses");
        let Transport::Kubectl(k8s) = &config.hosts[&host_id("staging")].transport else {
            panic!("staging should be kubectl");
        };
        assert_eq!(k8s.pod, KubectlField::Literal("sandbox-0".into()));
        assert_eq!(k8s.namespace, None);
        assert_eq!(k8s.context, None);
        assert_eq!(k8s.container, None);
    }

    #[test]
    fn rejects_port_above_65535() {
        // Out-of-range ports fail u16 deserialization — a shape error with
        // location info, unlike the semantic port-zero check.
        let err = Config::from_toml_str(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\nport = 65536\n",
        )
        .expect_err("out-of-range port");
        assert!(matches!(err, ConfigError::Parse(_)), "{err}");
        assert!(err.to_string().contains("line"), "{err}");
    }

    #[test]
    fn host_id_enforces_length_bound() {
        assert!(HostId::new("a".repeat(64)).is_ok());
        assert!(HostId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn port_zero_on_kubectl_is_reported_once_as_foreign() {
        // The port-range check must not also fire for a transport that
        // doesn't accept `port` — one contradiction-free diagnostic.
        let issues =
            issues_of("[hosts.staging]\ntransport = \"kubectl\"\npod = \"sandbox-0\"\nport = 0\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(
            issues[0].to_string().contains("does not apply"),
            "{issues:?}"
        );
    }

    #[test]
    fn validation_issue_display_is_escaped_and_bounded() {
        // Config strings echoed in error messages get logged; a pasted
        // config must not inject terminal escapes or megabytes into the
        // log line (same hygiene as the protocol's InvalidIdError).
        let issues = issues_of("[hosts.devbox]\ntransport = \"tel\\u001Bnet\"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = issues[0].to_string();
        assert!(!msg.contains('\u{1b}'), "raw ESC leaked: {msg:?}");
        assert!(msg.contains("\\u{1b}"), "{msg}");

        let huge_path = "a".repeat(100_000);
        let issues = issues_of(&format!(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"{huge_path}\"\n\
             workspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n"
        ));
        assert_eq!(issues.len(), 1, "{issues:?}");
        let msg = issues[0].to_string();
        assert!(
            msg.len() < 1_000,
            "message not bounded: {} bytes",
            msg.len()
        );
        assert!(msg.contains('…'), "{msg}");
    }

    #[test]
    fn rejects_control_characters_in_string_fields() {
        // A newline in an ssh destination splits log lines and metadata
        // records; never legitimate.
        let issues = issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"dev\\nbox\"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("control"), "{issues:?}");

        let issues = issues_of(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"/tmp/\\u001Bevil\"\n\
             workspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("control"), "{issues:?}");
    }

    #[test]
    fn rejects_leading_or_trailing_whitespace_in_host_fields() {
        // " -oproxycommand=..." would dodge the leading-dash guard and can
        // become a flag the moment any layer trims or word-splits.
        let issues =
            issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \" -oproxycommand=evil\"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("whitespace"), "{issues:?}");

        let issues = issues_of("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox \"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
    }

    #[test]
    fn rejects_blank_or_control_display_names() {
        let issues =
            issues_of("[hosts.devbox]\nname = \"\"\ntransport = \"ssh\"\nhost = \"devbox\"\n");
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("name"), "{issues:?}");

        let issues = issues_of(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nname = \"api\\u0007\"\nhost = \"devbox\"\npath = \"/api\"\n\
             workspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].to_string().contains("control"), "{issues:?}");
    }

    #[test]
    fn rejects_tilde_user_paths() {
        // Transports only promise `~/` expansion (ADR-0004 worktrees live
        // under `~/.remora`); `~user` forms would be silently mangled.
        let issues = issues_of(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"~bob/code\"\n\
             workspace = \"worktree\"\nagent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        );
        assert_eq!(issues.len(), 1, "{issues:?}");

        for ok_path in ["~", "~/code", "/code"] {
            let config = Config::from_toml_str(&format!(
                "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
                 [projects.api]\nhost = \"devbox\"\npath = \"{ok_path}\"\n\
                 workspace = \"worktree\"\nagent = \"claude\"\n\
                 [agents.claude]\ncommand = [\"claude\"]\n"
            ))
            .unwrap_or_else(|e| panic!("path {ok_path:?} should be valid: {e}"));
            assert_eq!(config.projects.len(), 1);
        }
    }

    #[test]
    fn display_caps_the_issue_list() {
        let mut toml = String::new();
        for i in 0..25 {
            toml.push_str(&format!("[hosts.h{i}]\ntransport = \"nope\"\n"));
        }
        let err = Config::from_toml_str(&toml).expect_err("25 broken hosts");
        let msg = err.to_string();
        assert!(msg.contains("25 problems"), "{msg}");
        assert!(msg.contains("and 5 more"), "should cap listing: {msg}");
    }

    #[test]
    fn host_id_serializes_as_plain_string() {
        let id = HostId::new("devbox").expect("valid slug");
        assert_eq!(
            serde_json::to_string(&id).expect("serialize"),
            r#""devbox""#
        );
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

    #[test]
    fn project_base_parses_and_is_optional() {
        let cfg = Config::from_toml_str(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"/api\"\nworkspace = \"worktree\"\n\
             agent = \"claude\"\nbase = \"origin/develop\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("valid");
        let api = &cfg.projects[&ProjectId::new("api").expect("slug")];
        assert_eq!(api.base.as_deref(), Some("origin/develop"));
    }

    #[test]
    fn rejects_invalid_project_base() {
        for bad in ["\"\"", "\"  \"", "\"-x\"", "\" -x\"", "\"a\\nb\""] {
            let issues = issues_of(&format!(
                "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
                 [projects.api]\nhost = \"devbox\"\npath = \"/api\"\nworkspace = \"worktree\"\n\
                 agent = \"claude\"\nbase = {bad}\n\
                 [agents.claude]\ncommand = [\"claude\"]\n"
            ));
            assert_eq!(issues.len(), 1, "base {bad}: {issues:?}");
            assert!(issues[0].to_string().contains("base"), "{issues:?}");
        }
    }

    #[test]
    fn kubectl_pod_literal_parses() {
        let cfg =
            Config::from_toml_str("[hosts.k]\ntransport = \"kubectl\"\npod = \"sandbox-0\"\n")
                .expect("valid literal pod");
        let Transport::Kubectl(k) = &cfg.hosts.values().next().expect("one host").transport else {
            panic!("expected kubectl");
        };
        assert_eq!(k.pod, KubectlField::Literal("sandbox-0".into()));
    }

    #[test]
    fn kubectl_pod_command_parses() {
        let cfg = Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"kubectl get pods -o name | head -n1\" }\n",
        )
        .expect("valid command pod");
        let Transport::Kubectl(k) = &cfg.hosts.values().next().expect("one host").transport else {
            panic!("expected kubectl");
        };
        assert_eq!(
            k.pod,
            KubectlField::Command("kubectl get pods -o name | head -n1".into())
        );
    }

    #[test]
    fn kubectl_command_allows_dashes_and_whitespace_but_rejects_empty() {
        // A command line legitimately starts with a flag-y token / has spaces.
        Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"  kubectl -n sb get pods -o name | head -n1  \" }\n",
        )
        .expect("dashes + edge whitespace allowed in a command");
        // Empty command is rejected.
        let err =
            Config::from_toml_str("[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"\" }\n")
                .expect_err("empty command rejected");
        assert!(format!("{err}").contains("pod"), "{err}");
    }

    #[test]
    fn kubectl_command_wrapped_in_substitution_is_rejected() {
        // The field holds the command itself; wrapping the whole thing in
        // `$(...)` is double-evaluation — the shell substitutes the inner
        // pipeline, then tries to run its output (a pod name) as a command,
        // yielding a misleading `sh: pod/...: No such file or directory` at
        // resolve time. Catch it loudly at config time instead (#127).
        let err = Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"$(kubectl get pods -o name | head -n1)\" }\n",
        )
        .expect_err("fully wrapped $(...) rejected");
        let msg = format!("{err}");
        assert!(msg.contains("pod") && msg.contains("$(...)"), "{msg}");

        // The backtick form is the identical double-evaluation mistake and must
        // be caught too, not just `$(...)`.
        Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"`kubectl get pods -o name | head -n1`\" }\n",
        )
        .expect_err("fully wrapped backticks rejected");

        // Leading/trailing whitespace around the wrap is still the mistake.
        Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"  $(echo sandbox)  \" }\n",
        )
        .expect_err("wrap with edge whitespace rejected");

        // A command substitution *within* a larger pipeline is legitimate shell
        // (resolve a namespace inline) and must NOT be rejected.
        Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"kubectl -n $(cat ns) get pods -o name | head -n1\" }\n",
        )
        .expect("interior $(...) is valid shell");
    }

    #[test]
    fn kubectl_command_table_unknown_key_is_loud() {
        // The whole reason for the custom Visitor: a typo'd inner key must fail,
        // not be silently dropped (untagged + deny_unknown_fields would drop it).
        let err = Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"x\", typo = 1 }\n",
        )
        .expect_err("unknown inner key rejected");
        assert!(format!("{err}").contains("typo"), "{err}");
    }

    #[test]
    fn kubectl_command_table_duplicate_command_key_is_rejected() {
        // TOML likely rejects the duplicate key before serde sees it; either way
        // the diagnostic must name `command`. (The Visitor's `duplicate_field`
        // branch stays as defensive coverage for non-TOML deserializers.)
        let err = Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"x\", command = \"y\" }\n",
        )
        .expect_err("duplicate inner key rejected");
        assert!(format!("{err}").contains("command"), "{err}");
    }

    #[test]
    fn kubectl_command_with_control_char_is_rejected() {
        let err = Config::from_toml_str(
            "[hosts.k]\ntransport = \"kubectl\"\npod = { command = \"kubectl\\u0007get\" }\n",
        )
        .expect_err("control char in command rejected");
        assert!(format!("{err}").contains("control"), "{err}");
    }

    #[test]
    fn kubectl_command_table_missing_command_is_rejected() {
        let err = Config::from_toml_str("[hosts.k]\ntransport = \"kubectl\"\npod = {}\n")
            .expect_err("empty table rejected");
        assert!(format!("{err}").contains("command"), "{err}");
    }

    #[test]
    fn kubectl_pod_wrong_shape_has_clean_expected_type_message() {
        // pod = 42 is neither a string nor a table.
        let err = Config::from_toml_str("[hosts.k]\ntransport = \"kubectl\"\npod = 42\n")
            .expect_err("integer pod rejected");
        let msg = format!("{err}");
        assert!(msg.contains("string") && msg.contains("command"), "{msg}");
    }

    #[test]
    fn literal_pod_guard_unchanged() {
        // Leading dash on a LITERAL is still rejected (unchanged behavior).
        let err = Config::from_toml_str("[hosts.k]\ntransport = \"kubectl\"\npod = \"-bad\"\n")
            .expect_err("leading dash literal rejected");
        assert!(format!("{err}").contains("pod"), "{err}");
    }

    #[test]
    fn project_worktree_root_parses_and_is_optional() {
        let cfg = Config::from_toml_str(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"/api\"\nworkspace = \"worktree\"\n\
             agent = \"claude\"\nworktree_root = \"~/work\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("valid");
        let api = &cfg.projects[&ProjectId::new("api").expect("slug")];
        assert_eq!(api.worktree_root.as_deref(), Some("~/work"));
    }

    #[test]
    fn project_worktree_root_defaults_to_none() {
        let cfg = Config::from_toml_str(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\
             [projects.api]\nhost = \"devbox\"\npath = \"/api\"\nworkspace = \"worktree\"\n\
             agent = \"claude\"\n\
             [agents.claude]\ncommand = [\"claude\"]\n",
        )
        .expect("valid");
        let api = &cfg.projects[&ProjectId::new("api").expect("slug")];
        assert_eq!(api.worktree_root, None);
    }

    #[test]
    fn host_worktree_root_parses_and_is_optional() {
        let cfg = Config::from_toml_str(
            "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\nworktree_root = \"~/work\"\n",
        )
        .expect("valid");
        let devbox = &cfg.hosts[&host_id("devbox")];
        assert_eq!(devbox.worktree_root.as_deref(), Some("~/work"));
    }

    #[test]
    fn host_worktree_root_defaults_to_none() {
        let cfg = Config::from_toml_str("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n")
            .expect("valid");
        let devbox = &cfg.hosts[&host_id("devbox")];
        assert_eq!(devbox.worktree_root, None);
    }
}
