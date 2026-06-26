//! Transport-agnostic spawn resolution: `Config` + `SpawnSpec` -> `SpawnPlan`.
//!
//! One source of truth for the spawn conventions (paths, branch, tmux name,
//! env metadata, agent argv) so every transport (ssh now, kubectl later)
//! builds identical sessions. Carries only data; renders no commands.

use remora_protocol::{AgentId, ProjectId, SessionId, SpawnSpec};

use crate::config::{Config, WorkspaceMode};
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
    #[error("project `{0}` is not a worktree project; cannot respawn")]
    NotWorktreeProject(ProjectId),
    #[error("invalid base ref: {0}")]
    InvalidBase(&'static str),
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
    /// `remora/<session>` in worktree mode; `None` in shared mode.
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
            (
                worktree_path(&spec.project_id, &spec.session_id),
                Some(branch_name(&spec.session_id)),
                base,
            )
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

/// Normalizes a base override: trims, maps empty/whitespace to `None` (the
/// cascade fall-through), and rejects a non-empty value that contains control
/// characters or starts with `-` (a leading dash is read by `git worktree add`
/// as a flag — quoting does not stop git's own arg parsing). #54.
pub(crate) fn normalize_base(raw: Option<String>) -> Result<Option<String>, PlanError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(PlanError::InvalidBase(
            "must not contain control characters",
        ));
    }
    if trimmed.starts_with('-') {
        return Err(PlanError::InvalidBase("must not start with `-`"));
    }
    Ok(Some(trimmed.to_string()))
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
            Err(PlanError::InvalidBase(_))
        ));
    }

    #[test]
    fn normalize_base_rejects_control_chars() {
        assert!(matches!(
            normalize_base(Some("ma\nin".into())),
            Err(PlanError::InvalidBase(_))
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
}
