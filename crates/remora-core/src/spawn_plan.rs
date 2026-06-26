//! Transport-agnostic spawn resolution: `Config` + `SpawnSpec` -> `SpawnPlan`.
//!
//! One source of truth for the spawn conventions (paths, branch, tmux name,
//! env metadata, agent argv) so every transport (ssh now, kubectl later)
//! builds identical sessions. Carries only data; renders no commands.

use remora_protocol::{AgentId, ProjectId, SessionId, SpawnSpec};

use crate::config::{Config, HostId, WorkspaceMode};
use crate::naming::{
    branch_name, tmux_session_name, worktree_path, ENV_AGENT, ENV_CREATED_AT, ENV_WORKSPACE,
};

/// Why a spawn could not be planned from local config. Typed (not a string)
/// so the UI keeps the offending id; stays off the transport error enum
/// (these are local-config precondition failures, not transport failures).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    #[error("unknown project `{0}`")]
    UnknownProject(ProjectId),
    #[error("unknown agent `{0}`")]
    UnknownAgent(AgentId),
    #[error("unknown host `{0}`")]
    UnknownHost(HostId),
    #[error("project `{0}` is not a worktree project; cannot respawn")]
    NotWorktreeProject(ProjectId),
    #[error("invalid {0}: {1}")]
    Invalid(&'static str, &'static str),
}

/// A resolved spawn, transport-agnostic. All paths are *logical* (raw `/…`
/// or `~/…`); transports apply their own quoting/expansion at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    /// `remora_<project>_<session>` — the tmux name and anti-race lock.
    pub tmux_name: String,
    pub workspace: WorkspaceMode,
    /// The project's configured directory (for `git -C` in worktree mode).
    pub project_path: String,
    /// Working directory for the agent: worktree path, or `project_path`.
    pub dir: String,
    /// `remora/<session>` in worktree mode (back-compat); raw branch name
    /// when an explicit branch is given; `None` in shared mode.
    pub branch: Option<String>,
    /// Resolved git start-point override (per-session, else per-project),
    /// normalized; `None` in shared mode or when both are empty (#54).
    pub base: Option<String>,
    /// `REMORA_*` session metadata, logical values.
    pub env: Vec<(String, String)>,
    /// Resolved agent launch command.
    pub agent_argv: Vec<String>,
}

/// Resolves a [`SpawnSpec`] against local [`Config`] into a [`SpawnPlan`].
/// The agent is `spec.agent` if present, else the project default. The
/// override is minted client-side and is not config-validated, so a missing
/// agent here is a real runtime path.
pub fn plan_spawn(config: &Config, spec: &SpawnSpec) -> Result<SpawnPlan, PlanError> {
    let project = config
        .projects
        .get(&spec.project_id)
        .ok_or_else(|| PlanError::UnknownProject(spec.project_id.clone()))?;

    let host = config
        .hosts
        .get(&project.host)
        .ok_or_else(|| PlanError::UnknownHost(project.host.clone()))?;

    let agent_id = spec.agent.clone().unwrap_or_else(|| project.agent.clone());
    let agent = config
        .agents
        .get(&agent_id)
        .ok_or_else(|| PlanError::UnknownAgent(agent_id.clone()))?;

    let tmux_name = tmux_session_name(&spec.project_id, &spec.session_id);
    // Session override wins for both workspace mode (#workspace) and the git
    // start-point (#54); each falls through to the project default when unset.
    let workspace = spec.workspace.unwrap_or(project.workspace);
    let (dir, branch, base) = match workspace {
        WorkspaceMode::Worktree => {
            // Session base override wins; empty falls through to the project default.
            let base = match normalize_base(spec.base.clone())? {
                Some(b) => Some(b),
                None => normalize_base(project.base.clone())?,
            };
            let branch = normalize_branch(spec.branch.clone())?;
            // worktree_root cascade: session → project → host → convention.
            let root = match normalize_worktree_root(spec.worktree_root.clone())? {
                Some(r) => Some(r),
                None => match normalize_worktree_root(project.worktree_root.clone())? {
                    Some(r) => Some(r),
                    None => normalize_worktree_root(host.worktree_root.clone())?,
                },
            };
            match branch {
                Some(branch) => {
                    // New: <root or convention-root>/<branch>, raw branch (no remora/ prefix).
                    let root = root.unwrap_or_else(|| {
                        format!("~/.remora/worktrees/{}", spec.project_id.as_str())
                    });
                    let dir = format!("{root}/{branch}");
                    (dir, Some(branch), base)
                }
                None => {
                    // Back-compat (pre-B2 client): exact convention path + remora/ branch.
                    (
                        worktree_path(&spec.project_id, &spec.session_id),
                        Some(branch_name(&spec.session_id)),
                        base,
                    )
                }
            }
        }
        WorkspaceMode::Shared => (project.path.clone(), None, None),
    };

    let env = vec![
        (ENV_AGENT.to_string(), agent_id.as_str().to_string()),
        (ENV_WORKSPACE.to_string(), dir.clone()),
        (ENV_CREATED_AT.to_string(), now_unix_secs().to_string()),
    ];

    Ok(SpawnPlan {
        project_id: spec.project_id.clone(),
        session_id: spec.session_id.clone(),
        tmux_name,
        workspace,
        project_path: project.path.clone(),
        dir,
        branch,
        base,
        env,
        agent_argv: agent.command.clone(),
    })
}

/// Shared trim + reject(control chars, leading `-`) core used by all
/// normalizers. Returns `None` for empty/whitespace-only input.
fn normalize_override(
    raw: Option<String>,
    what: &'static str,
) -> Result<Option<String>, PlanError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars().any(char::is_control) {
        return Err(PlanError::Invalid(
            what,
            "must not contain control characters",
        ));
    }
    if t.starts_with('-') {
        return Err(PlanError::Invalid(what, "must not start with `-`"));
    }
    Ok(Some(t.to_string()))
}

/// Normalizes a base override: trims, maps empty/whitespace to `None` (the
/// cascade fall-through), and rejects a non-empty value that contains control
/// characters or starts with `-` (a leading dash is read by `git worktree add`
/// as a flag — quoting does not stop git's own arg parsing). #54.
pub(crate) fn normalize_base(raw: Option<String>) -> Result<Option<String>, PlanError> {
    normalize_override(raw, "base")
}

/// Normalizes a branch override: trims, maps empty/whitespace to `None`,
/// and enforces git ref-name rules (no `..`, no trailing `/`, no `.lock`
/// suffix, no interior whitespace) in addition to the shared control-char
/// and leading-dash guards. Returns the raw branch name (no `remora/` prefix).
pub(crate) fn normalize_branch(raw: Option<String>) -> Result<Option<String>, PlanError> {
    let Some(b) = normalize_override(raw, "branch")? else {
        return Ok(None);
    };
    if b.contains("..")
        || b.ends_with('/')
        || b.ends_with(".lock")
        || b.chars().any(char::is_whitespace)
    {
        return Err(PlanError::Invalid("branch", "is not a valid git ref name"));
    }
    Ok(Some(b))
}

/// Normalizes a worktree-root override: trims trailing `/`, maps
/// empty/whitespace to `None`, and requires an absolute path (`/`) or a
/// `~/`-relative path. Rejects control chars and leading `-` via the
/// shared core.
pub(crate) fn normalize_worktree_root(raw: Option<String>) -> Result<Option<String>, PlanError> {
    let Some(r) = normalize_override(raw, "worktree_root")? else {
        return Ok(None);
    };
    if !(r.starts_with('/') || r.starts_with("~/")) {
        return Err(PlanError::Invalid(
            "worktree_root",
            "must be absolute or `~/`-relative",
        ));
    }
    let trimmed = r.trim_end_matches('/').to_string();
    // Guard degenerate roots: "/" trims to "" and "~/" trims to "~" — both
    // would produce broken paths (filesystem root or bare-tilde expansion).
    if trimmed.is_empty() || trimmed == "~" {
        return Err(PlanError::Invalid("worktree_root", "must not be empty"));
    }
    Ok(Some(trimmed))
}

/// Wall-clock unix seconds, client-stamped (display-only / untrusted per
/// `SessionMeta`). Saturates to 0 if the clock is before the epoch.
fn now_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"

            [projects.withbase]
            host = "devbox"
            path = "/home/dev/withbase"
            workspace = "worktree"
            agent = "claude"
            base = "origin/main"

            [projects.scratch]
            host = "devbox"
            path = "~/scratch"
            workspace = "shared"
            agent = "claude"

            [agents.claude]
            command = ["claude", "--continue"]

            [agents.codex]
            command = ["codex"]

            [agents.shell]
            command = []
        "#;
        Config::from_toml_str(toml).expect("valid config")
    }

    fn spec(project: &str, session: &str, agent: Option<&str>) -> SpawnSpec {
        SpawnSpec {
            project_id: ProjectId::new(project).expect("slug"),
            session_id: SessionId::new(session).expect("slug"),
            agent: agent.map(|a| AgentId::new(a).expect("slug")),
            base: None,
            workspace: None,
            branch: None,
            worktree_root: None,
        }
    }

    #[test]
    fn worktree_project_gets_worktree_dir_and_branch() {
        let plan = plan_spawn(&config(), &spec("api", "fix-login", None)).expect("plan");
        assert_eq!(plan.tmux_name, "remora_api_fix-login");
        assert_eq!(plan.dir, "~/.remora/worktrees/api/fix-login");
        assert_eq!(plan.branch.as_deref(), Some("remora/fix-login"));
        assert_eq!(plan.project_path, "/home/dev/api");
        assert_eq!(plan.agent_argv, vec!["claude", "--continue"]);
    }

    #[test]
    fn shared_project_uses_project_dir_and_no_branch() {
        let plan = plan_spawn(&config(), &spec("scratch", "s1", None)).expect("plan");
        assert_eq!(plan.dir, "~/scratch");
        assert_eq!(plan.branch, None);
    }

    #[test]
    fn agent_override_wins_over_project_default() {
        let plan = plan_spawn(&config(), &spec("api", "s1", Some("codex"))).expect("plan");
        assert_eq!(plan.agent_argv, vec!["codex"]);
    }

    #[test]
    fn env_carries_agent_workspace_and_parseable_created_at() {
        let plan = plan_spawn(&config(), &spec("api", "s1", None)).expect("plan");
        let env: std::collections::HashMap<_, _> = plan
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(env[ENV_AGENT], "claude");
        assert_eq!(env[ENV_WORKSPACE], "~/.remora/worktrees/api/s1");
        assert!(env[ENV_CREATED_AT].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn unknown_project_is_a_typed_error() {
        let err = plan_spawn(&config(), &spec("ghost", "s1", None)).expect_err("err");
        assert!(matches!(err, PlanError::UnknownProject(_)));
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn unknown_agent_override_is_a_typed_error() {
        let err = plan_spawn(&config(), &spec("api", "s1", Some("nope"))).expect_err("err");
        assert!(matches!(err, PlanError::UnknownAgent(_)));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn empty_command_agent_yields_a_plain_shell_plan() {
        let plan = plan_spawn(&config(), &spec("api", "s1", Some("shell"))).expect("plan");
        // No agent to launch: empty argv is the in-band "plain shell" signal.
        assert!(plan.agent_argv.is_empty());
        // The plain-shell agent is still a configured agent, so its id rides in
        // REMORA_AGENT and round-trips through discovery for a live session.
        let env: std::collections::HashMap<_, _> = plan
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(env[ENV_AGENT], "shell");
    }

    #[test]
    fn normalize_base_trims_and_empties_to_none() {
        assert_eq!(normalize_base(None).expect("ok"), None);
        assert_eq!(normalize_base(Some("   ".into())).expect("ok"), None);
        assert_eq!(
            normalize_base(Some("  origin/main ".into())).expect("ok"),
            Some("origin/main".to_string())
        );
    }

    #[test]
    fn normalize_base_rejects_leading_dash_after_trim() {
        assert!(matches!(
            normalize_base(Some(" -x".into())),
            Err(PlanError::Invalid(_, _))
        ));
    }

    #[test]
    fn normalize_base_rejects_control_chars() {
        assert!(matches!(
            normalize_base(Some("ma\nin".into())),
            Err(PlanError::Invalid(_, _))
        ));
    }

    #[test]
    fn session_base_wins_then_falls_through_to_project() {
        let cfg = config(); // existing helper; api project, worktree mode
        let mut spec = spec("api", "s1", None);
        spec.base = Some("origin/dev".into());
        assert_eq!(
            plan_spawn(&cfg, &spec).expect("plan").base.as_deref(),
            Some("origin/dev")
        );
        // empty session base falls through (project has no base here) -> None
        spec.base = Some("  ".into());
        assert_eq!(plan_spawn(&cfg, &spec).expect("plan").base, None);
    }

    #[test]
    fn whitespace_session_base_falls_through_to_project_base() {
        let cfg = config(); // withbase project has base = "origin/main"

        // Whitespace-only session base → falls through to project default.
        let mut s = spec("withbase", "s1", None);
        s.base = Some("   ".into());
        assert_eq!(
            plan_spawn(&cfg, &s).expect("plan").base.as_deref(),
            Some("origin/main")
        );

        // No session base at all → project default applies too.
        s.base = None;
        assert_eq!(
            plan_spawn(&cfg, &s).expect("plan").base.as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn shared_project_has_no_base() {
        let plan = plan_spawn(&config(), &spec("scratch", "s1", None)).expect("plan");
        assert_eq!(plan.base, None);
    }

    #[test]
    fn workspace_override_forces_worktree_on_a_shared_project() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("scratch").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: Some(WorkspaceMode::Worktree),
            branch: None,
            worktree_root: None,
        };
        let plan = plan_spawn(&config(), &spec).expect("plan");
        assert_eq!(plan.workspace, WorkspaceMode::Worktree);
        assert_eq!(plan.dir, "~/.remora/worktrees/scratch/s1");
        assert_eq!(plan.branch.as_deref(), Some("remora/s1"));
    }

    #[test]
    fn workspace_override_forces_shared_on_a_worktree_project() {
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: Some(WorkspaceMode::Shared),
            branch: None,
            worktree_root: None,
        };
        let plan = plan_spawn(&config(), &spec).expect("plan");
        assert_eq!(plan.workspace, WorkspaceMode::Shared);
        assert_eq!(plan.dir, "/home/dev/api");
        assert_eq!(plan.branch, None);
    }

    // ── Task 4: new validator + cascade + back-compat tests ─────────────────

    #[test]
    fn normalize_branch_rejects_bad_refs() {
        assert_eq!(
            normalize_branch(Some("  feat/login  ".into())).expect("ok"),
            Some("feat/login".into())
        );
        assert_eq!(normalize_branch(Some("   ".into())).expect("ok"), None);
        assert!(normalize_branch(Some("-x".into())).is_err()); // leading dash
        assert!(normalize_branch(Some("a\u{7}b".into())).is_err()); // control char
        assert!(normalize_branch(Some("feat/".into())).is_err()); // trailing slash
        assert!(normalize_branch(Some("a..b".into())).is_err()); // double dot
        assert!(normalize_branch(Some("a b".into())).is_err()); // whitespace
        assert!(normalize_branch(Some("x.lock".into())).is_err()); // .lock suffix
    }

    #[test]
    fn normalize_worktree_root_requires_absolute_or_tilde() {
        assert_eq!(
            normalize_worktree_root(Some("~/work".into())).expect("ok"),
            Some("~/work".into())
        );
        assert_eq!(
            normalize_worktree_root(Some("/mnt/x".into())).expect("ok"),
            Some("/mnt/x".into())
        );
        assert!(normalize_worktree_root(Some("relative/x".into())).is_err());
        assert!(normalize_worktree_root(Some("-x".into())).is_err());
        // Fix 2: trailing slash is trimmed.
        assert_eq!(
            normalize_worktree_root(Some("~/work/".into())).expect("ok"),
            Some("~/work".into())
        );
    }

    #[test]
    fn normalize_worktree_root_rejects_degenerate_roots() {
        // Fix 3: "/" trims to "" and "~/" trims to "~" — both must be rejected.
        assert!(normalize_worktree_root(Some("/".into())).is_err());
        assert!(normalize_worktree_root(Some("~/".into())).is_err());
    }

    #[test]
    fn plan_spawn_uses_root_and_branch_no_prefix() {
        // session-override branch + worktree_root → dir = <root>/<branch>, raw branch.
        let cfg = config();
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: Some("feat/login".into()),
            worktree_root: Some("~/work".into()),
        };
        let plan = plan_spawn(&cfg, &spec).expect("plan");
        assert_eq!(plan.branch.as_deref(), Some("feat/login")); // no remora/ prefix
        assert_eq!(plan.dir, "~/work/feat/login");
    }

    #[test]
    fn plan_spawn_worktree_root_cascades_then_falls_back_to_convention() {
        // (a) no session root, project has one → uses project root; branch given.
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"

            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"
            worktree_root = "~/proj"

            [agents.claude]
            command = ["claude", "--continue"]
        "#;
        let cfg = Config::from_toml_str(toml).expect("valid config");
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: Some("feat/login".into()),
            worktree_root: None, // session gives no root → falls to project
        };
        let plan = plan_spawn(&cfg, &spec).expect("plan");
        assert_eq!(plan.dir, "~/proj/feat/login");

        // (b) no root anywhere → convention root is used.
        let cfg2 = config(); // api project has no worktree_root; host has none either
        let spec2 = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: Some("feat/login".into()),
            worktree_root: None,
        };
        let plan2 = plan_spawn(&cfg2, &spec2).expect("plan");
        assert_eq!(plan2.dir, "~/.remora/worktrees/api/feat/login");
    }

    #[test]
    fn plan_spawn_worktree_root_cascades_host_level() {
        // Fix 1: host-level worktree_root is consulted when both session and
        // project leave it unset.  A bug that short-circuits the cascade before
        // the host check would produce the convention path instead.
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            worktree_root = "~/host-root"

            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"

            [agents.claude]
            command = ["claude", "--continue"]
        "#;
        let cfg = Config::from_toml_str(toml).expect("valid config");
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: Some("feat/login".into()),
            worktree_root: None, // session unset → falls to project (also unset) → host
        };
        let plan = plan_spawn(&cfg, &spec).expect("plan");
        assert_eq!(plan.dir, "~/host-root/feat/login");
    }

    #[test]
    fn plan_spawn_branch_none_reproduces_todays_behaviour() {
        // back-compat: branch None → remora/<session_id> + exact convention path.
        let cfg = config();
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("s1").expect("slug"),
            agent: None,
            base: None,
            workspace: None,
            branch: None,
            worktree_root: None,
        };
        let plan = plan_spawn(&cfg, &spec).expect("plan");
        assert_eq!(plan.branch.as_deref(), Some("remora/s1"));
        assert_eq!(plan.dir, "~/.remora/worktrees/api/s1");
    }
}
