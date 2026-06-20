//! Format-preserving write-back for the per-device config file (ADR-0006).
//!
//! [`Config`](super::Config) (in `config/mod.rs`) is the validated, read-only
//! view. `ConfigDocument` is the *editable* counterpart: it wraps a
//! `toml_edit` document so the app can add/edit/remove hosts, projects, and
//! agents while preserving the user's comments and formatting. Every mutation
//! re-validates by round-tripping through [`Config::from_toml_str`], so the
//! single existing validation path is the only source of truth — there are no
//! parallel rules here.

use std::io::Write;
use std::path::Path;

use tempfile::NamedTempFile;
use toml_edit::{value, Array, DocumentMut, Item, Table};

use super::{
    Agent, AgentId, Config, ConfigError, Host, HostId, Project, ProjectId, Transport,
    ValidationIssue,
};

/// The entry ids present in each section of the document, regardless of whether
/// the document is semantically valid. Powers degraded-mode recovery (ADR-0006):
/// when a degraded base can't produce a typed [`Config`], the UI still needs the
/// ids so the user can delete the offending entries one by one until the file
/// validates.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PresentIds {
    pub hosts: Vec<String>,
    pub projects: Vec<String>,
    pub agents: Vec<String>,
}

/// An editable, format-preserving view of the config file (ADR-0006).
pub struct ConfigDocument {
    doc: DocumentMut,
    /// `true` for a normally-loaded (valid) base, where every mutation
    /// re-validates the whole document. `false` for a degraded base opened via
    /// [`ConfigDocument::parse_lenient`], where mutations apply without
    /// whole-document validation so the user can still delete the broken entry
    /// that prevents the file from loading.
    strict: bool,
}

impl ConfigDocument {
    /// Parses and **validates** `input`. Editing requires a semantically valid
    /// base, so this returns the same `ConfigError` as a normal load when the
    /// file is broken. (For invalid-base recovery, see [`Self::parse_lenient`].)
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        // Validate semantics via the single existing path: syntax errors come
        // back as `Parse`, semantic problems as `Invalid`.
        Config::from_toml_str(input)?;
        // Past validation the input is well-formed TOML, so this parse cannot
        // fail in practice; map rather than unwrap to stay panic-free.
        let doc = input
            .parse::<DocumentMut>()
            .map_err(|e| ConfigError::DocumentParse(e.to_string()))?;
        Ok(Self { doc, strict: true })
    }

    /// Opens a file the schema can still deserialize but that may be
    /// **semantically invalid**, returning the document plus every validation
    /// issue found. This is the degraded-mode recovery path: the UI shows what's
    /// broken and lets the user delete/replace the offending entry without the
    /// whole file having to be valid first.
    ///
    /// Two kinds of failure are *not* recoverable this way and surface as
    /// `Err`: a TOML grammar error (`toml_edit` cannot parse it), and a
    /// structural/type error where the schema cannot even deserialize the
    /// document (a section that is not a table, a field of the wrong type, …).
    /// Those carry no per-entry [`ValidationIssue`] the editor could act on, so
    /// the caller is told the file is unopenable rather than handed a document
    /// that silently pretends to be clean. A fully valid file comes back as a
    /// strict document (its edits re-validate like any normal load).
    pub fn parse_lenient(input: &str) -> Result<(Self, Vec<ValidationIssue>), ConfigError> {
        let doc = input
            .parse::<DocumentMut>()
            .map_err(|e| ConfigError::DocumentParse(e.to_string()))?;
        let (strict, issues) = match Config::from_toml_str(input) {
            Ok(_) => (true, Vec::new()),
            Err(ConfigError::Invalid(issues)) => (false, issues),
            // A structural/type error (toml deserialize) leaves nothing the
            // editor can repair entry-by-entry; surface it, don't hide it.
            Err(e) => return Err(e),
        };
        Ok((Self { doc, strict }, issues))
    }

    /// The entry ids present in each section, regardless of validity. Listed in
    /// the same accessor the mutators use (a plain `[section.id]` table), so
    /// every id returned here is one [`Self::remove_host`] et al. can act on —
    /// the contract degraded-mode recovery relies on.
    pub fn present_ids(&self) -> PresentIds {
        PresentIds {
            hosts: section_ids(&self.doc, "hosts"),
            projects: section_ids(&self.doc, "projects"),
            agents: section_ids(&self.doc, "agents"),
        }
    }

    /// Serializes the document back to TOML, preserving comments and layout.
    pub fn to_toml(&self) -> String {
        self.doc.to_string()
    }

    /// The validated, typed view. A strictly-parsed document always yields
    /// `Ok` (every mutation re-validates), so callers on the normal path can
    /// expect success; a degraded ([`Self::parse_lenient`]) document can still
    /// be invalid, so this returns the error rather than panicking on it.
    pub fn config(&self) -> Result<Config, ConfigError> {
        Config::from_toml_str(&self.to_toml())
    }

    /// CREATE a host. Errors with [`ConfigError::Edit`] if the id already
    /// exists (so a typo on the add form can never overwrite a working host).
    pub fn insert_host(&mut self, id: &HostId, host: &Host) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_absent(doc, "hosts", id.as_str(), "host")?;
            set_entry(doc, "hosts", id.as_str(), host_item(host));
            Ok(())
        })
    }

    /// EDIT a host in place (also the rename path). Errors if the id is absent.
    pub fn update_host(&mut self, id: &HostId, host: &Host) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_present(doc, "hosts", id.as_str(), "host")?;
            set_entry(doc, "hosts", id.as_str(), host_item(host));
            Ok(())
        })
    }

    /// CREATE a project. Errors if the id already exists.
    pub fn insert_project(&mut self, id: &ProjectId, p: &Project) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_absent(doc, "projects", id.as_str(), "project")?;
            set_entry(doc, "projects", id.as_str(), project_item(p));
            Ok(())
        })
    }

    /// EDIT a project in place. Errors if the id is absent.
    pub fn update_project(&mut self, id: &ProjectId, p: &Project) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_present(doc, "projects", id.as_str(), "project")?;
            set_entry(doc, "projects", id.as_str(), project_item(p));
            Ok(())
        })
    }

    /// CREATE an agent. Errors if the id already exists.
    pub fn insert_agent(&mut self, id: &AgentId, a: &Agent) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_absent(doc, "agents", id.as_str(), "agent")?;
            set_entry(doc, "agents", id.as_str(), agent_item(a));
            Ok(())
        })
    }

    /// EDIT an agent in place. Errors if the id is absent.
    pub fn update_agent(&mut self, id: &AgentId, a: &Agent) -> Result<(), ConfigError> {
        self.commit(|doc| {
            ensure_present(doc, "agents", id.as_str(), "agent")?;
            set_entry(doc, "agents", id.as_str(), agent_item(a));
            Ok(())
        })
    }

    /// DELETE a host. Re-validation enforces referential integrity: removing a
    /// host a project still references comes back as [`ConfigError::Invalid`].
    pub fn remove_host(&mut self, id: &HostId) -> Result<(), ConfigError> {
        self.commit(|doc| remove_entry(doc, "hosts", id.as_str(), "host"))
    }

    /// DELETE a project. Nothing references projects, so this only fails if the
    /// id is absent.
    pub fn remove_project(&mut self, id: &ProjectId) -> Result<(), ConfigError> {
        self.commit(|doc| remove_entry(doc, "projects", id.as_str(), "project"))
    }

    /// DELETE an agent. Re-validation rejects removing an agent a project
    /// still references.
    pub fn remove_agent(&mut self, id: &AgentId) -> Result<(), ConfigError> {
        self.commit(|doc| remove_entry(doc, "agents", id.as_str(), "agent"))
    }

    /// Applies `edit` to a clone, re-validates the serialized result through
    /// the single existing validation path, and commits only if valid. A
    /// rejected edit leaves the live document untouched.
    fn commit<F>(&mut self, edit: F) -> Result<(), ConfigError>
    where
        F: FnOnce(&mut DocumentMut) -> Result<(), ConfigError>,
    {
        let mut next = self.doc.clone();
        edit(&mut next)?;
        // A degraded (lenient) base is already invalid; re-validating would
        // reject the very recovery delete the user is trying to make. Skip it
        // there and validate only strict documents.
        if self.strict {
            Config::from_toml_str(&next.to_string())?;
        }
        self.doc = next;
        Ok(())
    }

    /// Atomically writes the document to `path`: a sibling temp file is written
    /// and then renamed over the target, so a crash or error never truncates
    /// the live config. On unix the file is created `0600` (it holds connection
    /// details). On Windows it inherits default ACLs (see ADR-0006).
    ///
    /// Refuses to persist a semantically-invalid document (the degraded
    /// recovery path can reach one): writing a config the app cannot reload
    /// would brick it. Recover by deleting every offending entry first.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        let serialized = self.to_toml();
        // Never write a file `Config::load` would reject. For a strict document
        // this always passes; for a degraded one it is the guard that stops a
        // half-finished recovery from bricking the config.
        Config::from_toml_str(&serialized)?;

        let path = path.as_ref();
        let io = |source: std::io::Error| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        // If the config path is a symlink (common when dotfiles symlink it into
        // a repo), resolve it and write the real target, so `persist` replaces
        // the file the link points at instead of clobbering the link itself.
        let resolved;
        let target = if path.is_symlink() {
            resolved = std::fs::canonicalize(path).map_err(io)?;
            resolved.as_path()
        } else {
            path
        };
        // Temp file must share the target's directory so `persist` is a rename
        // within one filesystem (atomic), not a cross-device copy.
        let dir = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp = NamedTempFile::new_in(dir).map_err(io)?;
        // Tighten permissions *before* writing any bytes, so the connection
        // details never sit on disk world-readable at the default umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(io)?;
        }
        tmp.write_all(serialized.as_bytes()).map_err(io)?;
        tmp.flush().map_err(io)?;
        tmp.persist(target).map_err(|e| io(e.error))?;
        Ok(())
    }
}

/// The entry ids under `[section.*]`, in document order. Uses the same
/// table-like accessor as [`contains`]/[`remove_entry`]/[`set_entry`], so an id
/// listed here is always one those mutators can find — degraded recovery never
/// offers a delete that can't fire. `as_table_like` (not `as_table`) so the
/// inline-table form `section = { id = { … } }` is seen too, not just
/// `[section.id]` headers.
fn section_ids(doc: &DocumentMut, section: &str) -> Vec<String> {
    doc.get(section)
        .and_then(|s| s.as_table_like())
        .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default()
}

/// True when `[section.id]` is present in the document. `as_table_like` so an
/// inline-table section counts too (see [`section_ids`]).
fn contains(doc: &DocumentMut, section: &str, id: &str) -> bool {
    doc.get(section)
        .and_then(|s| s.as_table_like())
        .map(|t| t.contains_key(id))
        .unwrap_or(false)
}

fn ensure_absent(
    doc: &DocumentMut,
    section: &str,
    id: &str,
    kind: &str,
) -> Result<(), ConfigError> {
    if contains(doc, section, id) {
        return Err(ConfigError::Edit(format!("{kind} `{id}` already exists")));
    }
    Ok(())
}

fn ensure_present(
    doc: &DocumentMut,
    section: &str,
    id: &str,
    kind: &str,
) -> Result<(), ConfigError> {
    if !contains(doc, section, id) {
        return Err(ConfigError::Edit(format!("{kind} `{id}` not found")));
    }
    Ok(())
}

/// Writes (or replaces) `[section.id]` with `item`. The parent section is
/// created as an *implicit* table so the output is `[hosts.devbox]`, not a
/// bare `[hosts]` header or an inline `hosts = { ... }`. Replacing a whole
/// table drops any comments *inside* that table (a rename re-serializes the
/// table); comments on other keys and unrelated tables are preserved.
fn set_entry(doc: &mut DocumentMut, section: &str, id: &str, item: Item) {
    let root = doc.as_table_mut();
    // Create the parent when it is missing *or* present but not table-like, so
    // the insert below can never silently no-op against a stray scalar of the
    // same name. (`parse_lenient` now rejects a non-table section up front, so
    // this is defence-in-depth; an inline `section = { … }` is already
    // table-like and is left untouched.)
    if !root.get(section).map(Item::is_table_like).unwrap_or(false) {
        let mut parent = Table::new();
        // Implicit parent => emit `[hosts.devbox]`, never a standalone `[hosts]`.
        parent.set_implicit(true);
        root.insert(section, Item::Table(parent));
    }
    // `as_table_like_mut` (not `as_table_mut`) so an inline-table section
    // (`hosts = { … }`) is mutated in place instead of silently no-op'd;
    // toml_edit coerces the `Item::Table` into the inline form and preserves it.
    if let Some(parent) = root[section].as_table_like_mut() {
        parent.insert(id, item);
    }
}

/// Removes `[section.id]`, erroring if it is absent. (Referential integrity is
/// enforced afterwards by the caller's re-validation, not here.)
fn remove_entry(
    doc: &mut DocumentMut,
    section: &str,
    id: &str,
    kind: &str,
) -> Result<(), ConfigError> {
    ensure_present(doc, section, id, kind)?;
    // `as_table_like_mut` so a delete also fires on an inline-table section.
    if let Some(parent) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) {
        parent.remove(id);
    }
    Ok(())
}

fn host_item(host: &Host) -> Item {
    let mut t = Table::new();
    if let Some(name) = &host.name {
        t["name"] = value(name);
    }
    match &host.transport {
        Transport::Ssh(ssh) => {
            t["transport"] = value("ssh");
            t["host"] = value(&ssh.host);
            if let Some(user) = &ssh.user {
                t["user"] = value(user);
            }
            if let Some(port) = ssh.port {
                t["port"] = value(i64::from(port));
            }
        }
        Transport::Kubectl(k) => {
            t["transport"] = value("kubectl");
            t["pod"] = value(&k.pod);
            if let Some(ns) = &k.namespace {
                t["namespace"] = value(ns);
            }
            if let Some(ctx) = &k.context {
                t["context"] = value(ctx);
            }
            if let Some(c) = &k.container {
                t["container"] = value(c);
            }
        }
    }
    Item::Table(t)
}

fn project_item(p: &Project) -> Item {
    use super::WorkspaceMode;
    let mut t = Table::new();
    if let Some(name) = &p.name {
        t["name"] = value(name);
    }
    t["host"] = value(p.host.as_str());
    t["path"] = value(&p.path);
    t["workspace"] = value(match p.workspace {
        WorkspaceMode::Worktree => "worktree",
        WorkspaceMode::Shared => "shared",
    });
    t["agent"] = value(p.agent.as_str());
    Item::Table(t)
}

fn agent_item(a: &Agent) -> Item {
    let mut t = Table::new();
    let mut arr = Array::new();
    for arg in &a.command {
        arr.push(arg.as_str());
    }
    t["command"] = value(arr);
    Item::Table(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Agent, Host, KubectlHost, Project, SshHost, Transport, WorkspaceMode};
    use remora_protocol::{AgentId, ProjectId};

    fn hid(s: &str) -> super::super::HostId {
        super::super::HostId::new(s).expect("valid host id")
    }
    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("valid project id")
    }
    fn aid(s: &str) -> AgentId {
        AgentId::new(s).expect("valid agent id")
    }
    fn ssh_host() -> Host {
        Host {
            name: None,
            transport: Transport::Ssh(SshHost {
                host: "devbox".into(),
                user: None,
                port: None,
            }),
        }
    }
    fn claude_agent() -> Agent {
        Agent {
            command: vec!["claude".into()],
        }
    }

    #[test]
    fn to_toml_round_trips_preserving_comments() {
        let input = "# my hosts\n[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n";
        let doc = ConfigDocument::parse(input).expect("valid config parses");
        // The whole point: comments and formatting survive a load -> serialize.
        assert_eq!(doc.to_toml(), input);
    }

    #[test]
    fn insert_host_adds_and_validates() {
        let mut doc = ConfigDocument::parse("").expect("empty config is valid");
        doc.insert_host(&hid("devbox"), &ssh_host())
            .expect("insert");
        let cfg = doc.config().expect("valid config");
        assert!(cfg.hosts.contains_key(&hid("devbox")));
        // And it serializes back as a real table.
        assert!(doc.to_toml().contains("[hosts.devbox]"));
    }

    #[test]
    fn insert_host_rejects_duplicate_id() {
        let mut doc =
            ConfigDocument::parse("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n")
                .expect("valid");
        let err = doc
            .insert_host(&hid("devbox"), &ssh_host())
            .expect_err("duplicate id must be rejected");
        assert!(matches!(err, ConfigError::Edit(_)), "{err:?}");
        assert!(err.to_string().contains("devbox"), "{err}");
    }

    #[test]
    fn update_host_rejects_missing_id() {
        let mut doc = ConfigDocument::parse("").expect("empty");
        let err = doc
            .update_host(&hid("ghost"), &ssh_host())
            .expect_err("update of a missing id must be rejected");
        assert!(matches!(err, ConfigError::Edit(_)), "{err:?}");
    }

    #[test]
    fn update_host_edits_in_place_and_keeps_unrelated_tables() {
        let input = "# top\n[hosts.devbox]\ntransport = \"ssh\"\nhost = \"old\"\n\n# keep me\n[agents.claude]\ncommand = [\"claude\"]\n";
        let mut doc = ConfigDocument::parse(input).expect("valid");
        let renamed = Host {
            name: Some("Dev box".into()),
            transport: Transport::Ssh(SshHost {
                host: "new".into(),
                user: None,
                port: None,
            }),
        };
        doc.update_host(&hid("devbox"), &renamed).expect("update");
        let out = doc.to_toml();
        assert!(out.contains("host = \"new\""), "{out}");
        // Unrelated table + its comment survive the edit.
        assert!(out.contains("# keep me"), "{out}");
        assert!(out.contains("[agents.claude]"), "{out}");
    }

    #[test]
    fn rejected_insert_leaves_document_unchanged() {
        let input = "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n";
        let mut doc = ConfigDocument::parse(input).expect("valid");
        // A project referencing a missing agent is semantically invalid.
        let bad = Project {
            name: None,
            host: hid("devbox"),
            path: "/x".into(),
            workspace: WorkspaceMode::Worktree,
            agent: aid("nope"),
        };
        let err = doc
            .insert_project(&pid("api"), &bad)
            .expect_err("dangling agent ref is invalid");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err:?}");
        // Nothing was written.
        assert_eq!(doc.to_toml(), input);
    }

    /// A valid config with one host, one project referencing it, one agent.
    const LINKED: &str = "[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n\n[projects.api]\nhost = \"devbox\"\npath = \"/srv/api\"\nworkspace = \"worktree\"\nagent = \"claude\"\n\n[agents.claude]\ncommand = [\"claude\"]\n";

    #[test]
    fn remove_host_deletes_unreferenced() {
        let mut doc =
            ConfigDocument::parse("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n")
                .expect("valid");
        doc.remove_host(&hid("devbox")).expect("remove");
        assert!(!doc
            .config()
            .expect("valid config")
            .hosts
            .contains_key(&hid("devbox")));
    }

    #[test]
    fn remove_missing_host_is_rejected() {
        let mut doc = ConfigDocument::parse("").expect("empty");
        let err = doc.remove_host(&hid("ghost")).expect_err("missing");
        assert!(matches!(err, ConfigError::Edit(_)), "{err:?}");
    }

    #[test]
    fn remove_host_referenced_by_project_is_rejected() {
        let mut doc = ConfigDocument::parse(LINKED).expect("valid");
        let err = doc
            .remove_host(&hid("devbox"))
            .expect_err("a referenced host must not be removable");
        // Referential integrity falls out of re-validation for free.
        assert!(matches!(err, ConfigError::Invalid(_)), "{err:?}");
        // The rejected delete leaves the document byte-for-byte unchanged.
        assert_eq!(doc.to_toml(), LINKED);
    }

    #[test]
    fn remove_agent_referenced_by_project_is_rejected() {
        let mut doc = ConfigDocument::parse(LINKED).expect("valid");
        let err = doc
            .remove_agent(&aid("claude"))
            .expect_err("a referenced agent must not be removable");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err:?}");
        assert_eq!(doc.to_toml(), LINKED);
    }

    #[test]
    fn remove_project_then_host_succeeds() {
        let mut doc = ConfigDocument::parse(LINKED).expect("valid");
        doc.remove_project(&pid("api")).expect("remove project");
        // With the project gone, the host is no longer referenced.
        doc.remove_host(&hid("devbox")).expect("remove host");
        assert!(doc.config().expect("valid config").hosts.is_empty());
    }

    #[test]
    fn update_host_transport_switch_clears_stale_keys() {
        let mut doc = ConfigDocument::parse(
            "[hosts.box]\ntransport = \"ssh\"\nhost = \"h\"\nuser = \"u\"\nport = 22\n",
        )
        .expect("valid");
        doc.update_host(
            &hid("box"),
            &Host {
                name: None,
                transport: Transport::Kubectl(KubectlHost {
                    pod: "p".into(),
                    namespace: None,
                    context: None,
                    container: None,
                }),
            },
        )
        .expect("transport switch");
        let out = doc.to_toml();
        assert!(out.contains("transport = \"kubectl\""), "{out}");
        assert!(out.contains("pod = \"p\""), "{out}");
        // Stale ssh keys are gone. Anchor to line starts — "transport = "
        // contains the substring "port = ".
        let has_key = |k: &str| out.lines().any(|l| l.trim_start().starts_with(k));
        assert!(!has_key("host"), "stale host key: {out}");
        assert!(!has_key("user"), "stale user key: {out}");
        assert!(!has_key("port"), "stale port key: {out}");
        assert!(
            matches!(
                doc.config().expect("valid config").hosts[&hid("box")].transport,
                Transport::Kubectl(_)
            ),
            "config should reflect kubectl"
        );
    }

    #[test]
    fn parse_lenient_reports_issues_and_allows_recovery_delete() {
        // Two broken hosts: syntactically fine, semantically invalid.
        let input = "[hosts.a]\ntransport = \"telnet\"\n[hosts.b]\ntransport = \"nope\"\n";
        // Strict parse refuses it.
        assert!(ConfigDocument::parse(input).is_err());
        // Lenient parse opens it and reports what's wrong.
        let (mut doc, issues) =
            ConfigDocument::parse_lenient(input).expect("lenient parse opens an invalid base");
        assert_eq!(issues.len(), 2, "{issues:?}");
        // Recovery delete works even though the doc is still invalid afterwards.
        doc.remove_host(&hid("a")).expect("degraded-mode delete");
        assert!(doc.to_toml().contains("[hosts.b]"));
        assert!(!doc.to_toml().contains("[hosts.a]"));
    }

    #[test]
    fn present_ids_lists_every_entry_even_in_a_degraded_doc() {
        // A semantically invalid base (two hosts with bad transports) plus a
        // project and an agent. Degraded mode needs every id so the user can
        // delete entries one by one until the file validates.
        let input = "[hosts.a]\ntransport = \"telnet\"\n[hosts.b]\ntransport = \"nope\"\n[projects.api]\nhost = \"a\"\npath = \"/x\"\nworkspace = \"worktree\"\nagent = \"claude\"\n[agents.claude]\ncommand = [\"claude\"]\n";
        let (doc, _issues) = ConfigDocument::parse_lenient(input).expect("lenient");
        let present = doc.present_ids();
        assert_eq!(present.hosts, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(present.projects, vec!["api".to_string()]);
        assert_eq!(present.agents, vec!["claude".to_string()]);
    }

    #[test]
    fn inline_table_section_supports_edit_and_present_ids() {
        // A hand-written config may use the inline-table form of a section
        // (valid TOML, deserializes the same as `[hosts.devbox]`). The editor
        // must still see the entry and mutate it in place — not silently no-op
        // while reporting success.
        let input = "hosts = { devbox = { transport = \"ssh\", host = \"old\" } }\n";
        let mut doc = ConfigDocument::parse(input).expect("inline-table config is valid");
        assert_eq!(
            doc.present_ids().hosts,
            vec!["devbox".to_string()],
            "present_ids must list an inline-table entry"
        );
        doc.update_host(
            &hid("devbox"),
            &Host {
                name: None,
                transport: Transport::Ssh(SshHost {
                    host: "new".into(),
                    user: None,
                    port: None,
                }),
            },
        )
        .expect("update");
        assert_eq!(
            doc.config().expect("valid").hosts[&hid("devbox")].transport,
            Transport::Ssh(SshHost {
                host: "new".into(),
                user: None,
                port: None
            }),
            "the edit must actually change the stored host, not no-op: {}",
            doc.to_toml()
        );
        // And a delete must fire on the inline form too (degraded recovery).
        doc.remove_host(&hid("devbox")).expect("remove");
        assert!(doc.present_ids().hosts.is_empty(), "{}", doc.to_toml());
    }

    #[test]
    fn present_ids_of_an_empty_doc_is_empty() {
        let doc = ConfigDocument::parse("").expect("empty is valid");
        let present = doc.present_ids();
        assert!(present.hosts.is_empty());
        assert!(present.projects.is_empty());
        assert!(present.agents.is_empty());
    }

    #[test]
    fn config_on_a_degraded_doc_returns_err_not_panic() {
        let (doc, _issues) =
            ConfigDocument::parse_lenient("[hosts.a]\ntransport = \"telnet\"\n").expect("lenient");
        // A degraded document is invalid; config() must surface that, not panic.
        assert!(matches!(doc.config(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn parse_lenient_rejects_a_structural_error_instead_of_faking_clean() {
        // Grammatically valid TOML, but `hosts` is a string, not a table: the
        // schema can't deserialize it, so there are no per-entry issues to
        // recover. Lenient parse must surface the error, not hand back a doc
        // that reports zero issues while silently mutating nothing.
        let (doc, issues) = match ConfigDocument::parse_lenient("hosts = \"oops\"\n") {
            Ok(ok) => ok,
            Err(e) => {
                assert!(matches!(e, ConfigError::Parse(_)), "{e:?}");
                return;
            }
        };
        panic!(
            "expected a structural error, got a degraded doc: {issues:?}\n{}",
            doc.to_toml()
        );
    }

    #[test]
    fn parse_lenient_of_a_valid_file_yields_a_strict_doc() {
        // A clean file has nothing to recover, so lenient parse returns a strict
        // document: a later edit that breaks referential integrity is rejected,
        // exactly as on the normal load path.
        let (mut doc, issues) = ConfigDocument::parse_lenient(LINKED).expect("valid file opens");
        assert!(issues.is_empty(), "a valid file has no issues: {issues:?}");
        let dangling = Project {
            name: None,
            host: hid("devbox"),
            path: "/x".into(),
            workspace: WorkspaceMode::Worktree,
            agent: aid("nope"),
        };
        let err = doc
            .insert_project(&pid("api2"), &dangling)
            .expect_err("strict doc must re-validate and reject a dangling agent ref");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn save_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("remora-doc-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.toml");
        let doc = ConfigDocument::parse(LINKED).expect("valid");
        let result = doc.save(&path);
        let back = std::fs::read_to_string(&path).ok();
        std::fs::remove_dir_all(&dir).ok();
        result.expect("save");
        assert_eq!(back.as_deref(), Some(LINKED));
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("remora-doc-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.toml");
        let doc = ConfigDocument::parse(LINKED).expect("valid");
        let result = doc.save(&path);
        let mode = std::fs::metadata(&path).map(|m| m.permissions().mode());
        std::fs::remove_dir_all(&dir).ok();
        result.expect("save");
        assert_eq!(mode.expect("metadata") & 0o777, 0o600);
    }

    #[test]
    fn save_refuses_to_persist_an_invalid_degraded_document() {
        // Open an invalid base, delete one of two broken hosts: the doc is
        // still invalid. save() must refuse rather than brick the config file.
        let input = "[hosts.a]\ntransport = \"telnet\"\n[hosts.b]\ntransport = \"nope\"\n";
        let (mut doc, _issues) = ConfigDocument::parse_lenient(input).expect("lenient");
        doc.remove_host(&hid("a")).expect("degraded delete");
        let dir = std::env::temp_dir().join(format!("remora-doc-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("config.toml");
        let err = doc.save(&path);
        let existed = path.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(err, Err(ConfigError::Invalid(_))),
            "save must reject an invalid document: {err:?}"
        );
        assert!(
            !existed,
            "no file should be written for an invalid document"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_through_a_symlink_preserves_the_link() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("remora-doc-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let real = dir.join("real-config.toml");
        std::fs::write(&real, "# placeholder\n").expect("seed real file");
        let link = dir.join("config.toml");
        symlink(&real, &link).expect("symlink");

        let doc = ConfigDocument::parse(LINKED).expect("valid");
        let result = doc.save(&link);

        let still_symlink = std::fs::symlink_metadata(&link).map(|m| m.file_type().is_symlink());
        let real_content = std::fs::read_to_string(&real).ok();
        std::fs::remove_dir_all(&dir).ok();

        result.expect("save through symlink");
        // The link must survive (dotfiles stay attached), and the real target
        // gets the new content.
        assert_eq!(still_symlink.ok(), Some(true), "symlink was replaced");
        assert_eq!(real_content.as_deref(), Some(LINKED));
    }

    #[test]
    fn save_to_missing_dir_errors_and_writes_nothing() {
        let path = std::env::temp_dir()
            .join(format!("remora-absent-{}", std::process::id()))
            .join("config.toml");
        let doc = ConfigDocument::parse(LINKED).expect("valid");
        let err = doc.save(&path).expect_err("missing parent dir");
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
        // No partial file left at the target.
        assert!(!path.exists());
    }

    #[test]
    fn insert_project_and_agent_round_trip() {
        let mut doc =
            ConfigDocument::parse("[hosts.devbox]\ntransport = \"ssh\"\nhost = \"devbox\"\n")
                .expect("valid");
        doc.insert_agent(&aid("claude"), &claude_agent())
            .expect("agent");
        doc.insert_project(
            &pid("api"),
            &Project {
                name: Some("API".into()),
                host: hid("devbox"),
                path: "/srv/api".into(),
                workspace: WorkspaceMode::Worktree,
                agent: aid("claude"),
            },
        )
        .expect("project");
        let cfg = doc.config().expect("valid config");
        assert!(cfg.agents.contains_key(&aid("claude")));
        assert_eq!(cfg.projects[&pid("api")].path, "/srv/api");
    }
}
