//! `SshSource` — the first real transport. Builds the ssh argv from a
//! validated `SshHost` and delegates to the PTY-process bridge.

use std::sync::Arc;

use async_trait::async_trait;
use portable_pty::CommandBuilder;
use remora_protocol::{ProjectId, SessionId, SessionMeta, SpawnSpec};

use super::pty_process::spawn_pty_channel;
use crate::config::{Config, SshHost, WorkspaceMode};
use crate::discovery::{self, DiscoveredEnv};
use crate::naming::{parse_tmux_session_name, tmux_session_name};
use crate::spawn_plan::{plan_spawn, PlanError, SpawnPlan};
use crate::{SessionChannel, SessionSource, SourceError};

/// Single-token shell quoting for the remote login shell, via `shlex`.
/// Config validation bans control/nul characters (stage 3), so `try_quote`
/// cannot hit its nul-byte error path here.
fn shell_quote(token: &str) -> String {
    shlex::try_quote(token)
        .expect("config bans control/nul characters")
        .into_owned()
}

/// Renders a logical remote path (`/…`, `~/…`, or `~`) into one shell token
/// that the remote shell resolves to the intended directory. Quoting
/// disables tilde expansion, so a leading `~` is emitted as a double-quoted
/// `$HOME` with the remainder passed through `shell_quote` (bare for normal
/// slug/path chars, quoted only if it contains shell-special bytes):
/// `~/api` -> `"$HOME"/api`. Config rejects `~user` and control chars
/// (stage 3), so these three cases are exhaustive.
fn quote_remote_path(path: &str) -> String {
    if path == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("\"$HOME\"{}", shell_quote(&format!("/{rest}")))
    } else {
        shell_quote(path)
    }
}

/// Result of a blocking remote command: success, captured stdout, and stderr.
pub(crate) struct RemoteOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The executor seam every spawn/attach step crosses.
pub(crate) trait SshExec: Send + Sync {
    fn run(&self, argv: &[String]) -> Result<RemoteOutput, SourceError>;
    fn open_channel(&self, argv: &[String]) -> Result<SessionChannel, SourceError>;
}

struct RealSshExec;

impl SshExec for RealSshExec {
    fn run(&self, argv: &[String]) -> Result<RemoteOutput, SourceError> {
        debug_assert!(!argv.is_empty(), "argv must contain at least the program");
        let output = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| SourceError::Transport(format!("ssh exec: {e}")))?;
        Ok(RemoteOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn open_channel(&self, argv: &[String]) -> Result<SessionChannel, SourceError> {
        spawn_pty_channel(command_from_argv(argv))
    }
}

/// One instance = one configured ssh host (matches the `SessionSource`
/// trait doc).
pub struct SshSource {
    host: SshHost,
    config: Arc<Config>,
    exec: Arc<dyn SshExec>,
}

impl SshSource {
    /// Wraps a configured ssh host as a transport.
    pub fn new(host: SshHost, config: Arc<Config>) -> Self {
        Self {
            host,
            config,
            exec: Arc::new(RealSshExec),
        }
    }

    #[cfg(test)]
    fn with_exec(host: SshHost, config: Arc<Config>, exec: Arc<dyn SshExec>) -> Self {
        Self { host, config, exec }
    }
}

/// The ssh program + connection flags shared by every command this transport
/// runs. `interactive` adds `-tt` (force a remote PTY) for the attach; the
/// blocking setup commands (git, tmux create) don't need it. Keepalive lets
/// a half-open link (laptop sleep) surface as channel death in ~45s.
fn ssh_base_argv(host: &SshHost, interactive: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["ssh".into()];
    if interactive {
        argv.push("-tt".into());
    }
    argv.push("-o".into());
    argv.push("ServerAliveInterval=15".into());
    argv.push("-o".into());
    argv.push("ServerAliveCountMax=3".into());
    // Bound the connect phase so an unreachable/slow host fails fast instead
    // of parking a spawn_blocking thread. Execution-phase hangs (a wedged
    // remote git/tmux) are not covered — see TODOS.md (execution watchdog).
    argv.push("-o".into());
    argv.push("ConnectTimeout=10".into());
    if let Some(port) = host.port {
        argv.push("-p".into());
        argv.push(port.to_string());
    }
    if let Some(user) = &host.user {
        argv.push("-l".into());
        argv.push(user.clone());
    }
    argv.push(host.host.clone());
    argv
}

/// Builds the ssh argv (program + args) for attaching to `tmux_name`, as a
/// pure `Vec<String>` so it is unit-testable without spawning anything.
///
/// `host`/`user` are config-validated (no leading `-`, no whitespace, no
/// control chars — stage 3) and `port` is a `u16`, so every token is safe;
/// the remote command is still passed as discrete argv elements, never a
/// joined shell string (ADR-0004). No `--` separator: the remote command
/// begins with the literal `tmux` and nothing here needs an options
/// terminator (a trailing `--` breaks on ssh clients that don't re-parse
/// options after the destination, e.g. Dropbear).
fn attach_argv(host: &SshHost, tmux_name: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, true);
    // `-d` evicts every other client on attach (sequential-handoff model).
    argv.push("tmux".into());
    argv.push("attach-session".into());
    argv.push("-d".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv
}

/// `git -C <project> worktree add -b <branch> <worktree>` over ssh.
/// Precondition: `plan.branch` is `Some` (worktree mode); the only caller
/// checks. `git worktree add` creates leading directories.
fn worktree_add_argv(host: &SshHost, plan: &SpawnPlan) -> Vec<String> {
    let branch = plan.branch.as_deref().unwrap_or_default();
    let mut argv = ssh_base_argv(host, false);
    argv.push("git".into());
    argv.push("-C".into());
    argv.push(quote_remote_path(&plan.project_path));
    argv.push("worktree".into());
    argv.push("add".into());
    argv.push("-b".into());
    argv.push(shell_quote(branch));
    argv.push(quote_remote_path(&plan.dir));
    argv
}

/// `git -C <project> worktree remove --force <worktree>` — best-effort
/// cleanup of an orphaned worktree after a non-duplicate `new-session`
/// failure (no live session owns it), so the project/session slot stays
/// retryable. `--force` because the fresh worktree may have a checked-out
/// branch and no commits yet.
fn worktree_remove_argv(host: &SshHost, plan: &SpawnPlan) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("git".into());
    argv.push("-C".into());
    argv.push(quote_remote_path(&plan.project_path));
    argv.push("worktree".into());
    argv.push("remove".into());
    argv.push("--force".into());
    argv.push(quote_remote_path(&plan.dir));
    argv
}

/// Joins the agent argv into a single shell command line (one sh-safe
/// string). `tmux new-session` re-runs its shell-command argument through
/// `sh -c` — a *second* shell parse — so per-token quoting alone would be
/// stripped before tmux sees it. Joining here (minimal per-token quoting)
/// produces a string that `sh -c` re-parses back into the original argv.
/// Config bans control/nul characters, so `try_join` cannot error.
fn join_agent_command(argv: &[String]) -> String {
    shlex::try_join(argv.iter().map(String::as_str)).expect("config bans control/nul characters")
}

/// Wraps the joined agent command so a clean / user-interrupted exit
/// (0 graceful, 130 SIGINT/Ctrl-C, 143 SIGTERM) execs an interactive login
/// shell in the same dir, keeping the pane alive with a real prompt (#30); any
/// other non-zero exit propagates so `remain-on-exit` retains the dead pane and
/// its error for inspection (#28). `${SHELL:-/bin/sh}` defends against an unset
/// SHELL in the pane environment.
///
/// `__remora_rc=$?` MUST be the first statement after the agent command —
/// nothing may run between the agent and the `$?` capture or it records the
/// wrong code.
///
/// ```text
///   <agent> exits
///        │  $? captured immediately
///        ├── 0 | 130 | 143 ──▶ exec $SHELL -l   (pane lives → usable shell, #30)
///        └── else ───────────▶ exit $rc          (pane dies → remain-on-exit
///                                                 keeps dead pane + status, #28)
/// ```
fn wrap_with_shell_fallback(agent_command: &str) -> String {
    format!(
        r#"{agent_command}; __remora_rc=$?; case "$__remora_rc" in 0|130|143) exec "${{SHELL:-/bin/sh}}" -l ;; *) exit "$__remora_rc" ;; esac"#
    )
}

/// `tmux new-session -d -s <name> -c <dir> <agent…>` — the atomic creation
/// lock. No metadata trailer (set-environment runs separately so a metadata
/// failure can't falsely fail a live session). The agent command is joined
/// and quoted as a single arg (see `join_agent_command`).
fn new_session_argv(host: &SshHost, plan: &SpawnPlan) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("new-session".into());
    argv.push("-d".into());
    argv.push("-s".into());
    argv.push(plan.tmux_name.clone());
    argv.push("-c".into());
    argv.push(quote_remote_path(&plan.dir));
    // The agent command is ONE shell-quoted arg, not one per token: the login
    // shell strips this outer layer, then tmux's `sh -c` re-parses the inner
    // string back into the intended argv (the double-shell hazard). It is
    // wrapped first so a clean / interrupted agent exit drops to a login shell
    // instead of a dead pane (#30); a crash still propagates to the retained
    // pane (#28).
    argv.push(shell_quote(&wrap_with_shell_fallback(&join_agent_command(
        &plan.agent_argv,
    ))));
    argv
}

/// `tmux set-environment -t <name> <key> <value>`. The value is the logical
/// metadata string, single-quoted as a literal (no tilde expansion — the
/// stored value must round-trip via stage-6 `show-environment`).
fn set_environment_argv(host: &SshHost, tmux_name: &str, key: &str, value: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("set-environment".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv.push(key.into());
    argv.push(shell_quote(value));
    argv
}

/// `tmux set-option -t <name> remain-on-exit on` — keeps an exited agent's
/// pane inspectable instead of destroying the session.
fn set_option_remain_on_exit_argv(host: &SshHost, tmux_name: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("set-option".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv.push("remain-on-exit".into());
    argv.push("on".into());
    argv
}

/// `tmux list-sessions -F '#{session_name}'`. The format string is
/// shell-quoted: a bare `#` would start a comment in the remote login shell.
fn list_sessions_argv(host: &SshHost) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("list-sessions".into());
    argv.push("-F".into());
    argv.push(shell_quote("#{session_name}"));
    argv
}

/// `tmux show-environment -t <name>` — reads one session's env metadata.
fn show_environment_query_argv(host: &SshHost, tmux_name: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("show-environment".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv
}

/// `git -C <project-path> worktree list --porcelain`.
fn worktree_list_argv(host: &SshHost, project_path: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("git".into());
    argv.push("-C".into());
    argv.push(quote_remote_path(project_path));
    argv.push("worktree".into());
    argv.push("list".into());
    argv.push("--porcelain".into());
    argv
}

/// `test -d <dir>` — the respawn preflight that the worktree directory still
/// exists. `git worktree list` is the wrong probe here: it reports git's
/// admin entry, which survives a bare `rm -rf` of the worktree, so it would
/// pass for exactly the vanished-directory case we must reject. `test` writes
/// nothing to stderr, so a non-zero exit with empty stderr means "dir gone"
/// while a non-zero exit with stderr means the probe itself failed (the
/// remote shell or ssh transport), which we keep as a `Transport` error.
fn dir_exists_argv(host: &SshHost, dir: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("test".into());
    argv.push("-d".into());
    argv.push(quote_remote_path(dir));
    argv
}

/// Classifies `tmux list-sessions`: success → session-name lines; a
/// no-server / no-sessions stderr → empty (the normal cold state, decision 9,
/// matched case-insensitively); any other failure → `Transport`.
fn classify_list_sessions(out: &RemoteOutput) -> Result<Vec<String>, SourceError> {
    if out.success {
        return Ok(out.stdout.lines().map(str::to_string).collect());
    }
    let lower = out.stderr.to_ascii_lowercase();
    if lower.contains("no server running") || lower.contains("no sessions") {
        Ok(Vec::new())
    } else {
        Err(SourceError::Transport(out.stderr.clone()))
    }
}

/// Maps a failed `tmux new-session` to a `SourceError`. tmux prints
/// `duplicate session: NAME` and exits non-zero when the name is taken; the
/// match is case-insensitive on `duplicate` so a non-English `LC_MESSAGES`
/// still trips the fail-closed lock. Called only on non-success.
fn classify_new_session_failure(
    stderr: &str,
    project_id: &ProjectId,
    session_id: &SessionId,
) -> SourceError {
    if stderr.to_ascii_lowercase().contains("duplicate session") {
        SourceError::SessionExists {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        }
    } else {
        SourceError::Transport(stderr.to_string())
    }
}

/// Maps a failed `git worktree add` to a `SourceError`. A leftover worktree
/// dir or branch (from a prior stopped session) surfaces as `SessionExists`
/// — an actionable "already exists" rather than raw git stderr — keeping ssh
/// consistent with the fake. Reclaim/respawn is stage 6. Called only on
/// non-success.
fn classify_worktree_add_failure(
    stderr: &str,
    project_id: &ProjectId,
    session_id: &SessionId,
) -> SourceError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("already exists") || lower.contains("already checked out") {
        SourceError::SessionExists {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        }
    } else {
        SourceError::Transport(stderr.to_string())
    }
}

/// Turns a pure argv into a `CommandBuilder` (program = argv[0]).
///
/// Precondition: `argv` is non-empty. Callers feed it [`attach_argv`]
/// (via [`RealSshExec::open_channel`]), which always yields the `ssh`
/// program plus its args.
fn command_from_argv(argv: &[String]) -> CommandBuilder {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    // The terminal is xterm.js (xterm-256color). Pin it so the remote tmux
    // resolves terminfo regardless of the launching shell's $TERM (#26);
    // ssh forwards this to the remote PTY. .env() overrides only TERM, the
    // rest of the environment is still inherited.
    cmd.env("TERM", "xterm-256color");
    cmd
}

/// `tmux new-session -d` — the atomic creation lock. `Ok` on success;
/// `Err(SessionExists)` on a duplicate name (case-insensitive); otherwise
/// `Err(Transport)`. Opens no channel.
fn create_session(exec: &dyn SshExec, host: &SshHost, plan: &SpawnPlan) -> Result<(), SourceError> {
    let out = exec.run(&new_session_argv(host, plan))?;
    if out.success {
        Ok(())
    } else {
        Err(classify_new_session_failure(
            &out.stderr,
            &plan.project_id,
            &plan.session_id,
        ))
    }
}

/// Writes `remain-on-exit` then the `REMORA_*` env metadata. Every call is
/// tolerated: a metadata failure must never fail an already-live session
/// (env is untrusted/display-only — ADR-0004). `remain-on-exit` goes first so
/// a pane whose process exits at startup (a bad start dir, a missing agent
/// binary) keeps its now-empty session alive long enough to attach, instead
/// of tmux destroying it during the env-var round-trips and turning the
/// follow-up attach into a spurious `SessionNotFound`.
fn write_metadata(exec: &dyn SshExec, host: &SshHost, plan: &SpawnPlan) {
    let _ = exec.run(&set_option_remain_on_exit_argv(host, &plan.tmux_name));
    for (key, value) in &plan.env {
        let _ = exec.run(&set_environment_argv(host, &plan.tmux_name, key, value));
    }
}

/// `tmux has-session -t <name>` — the liveness preflight for `attach`.
fn has_session_argv(host: &SshHost, tmux_name: &str) -> Vec<String> {
    let mut argv = ssh_base_argv(host, false);
    argv.push("tmux".into());
    argv.push("has-session".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv
}

/// Opens the PTY attach channel to an existing session (no liveness
/// preflight — callers that need one do it first; see `SshSource::attach`).
fn attach_channel(
    exec: &dyn SshExec,
    host: &SshHost,
    tmux_name: &str,
) -> Result<SessionChannel, SourceError> {
    exec.open_channel(&attach_argv(host, tmux_name))
}

/// Orchestrates the full spawn sequence: optional worktree creation, tmux
/// new-session (the atomic lock), metadata (remain-on-exit then env vars),
/// then attach. Each step crosses the `SshExec` seam so tests can inject a
/// `FakeExec` without touching the network.
fn run_spawn(
    exec: &dyn SshExec,
    host: &SshHost,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    if plan.branch.is_some() {
        let out = exec.run(&worktree_add_argv(host, plan))?;
        if !out.success {
            return Err(classify_worktree_add_failure(
                &out.stderr,
                &plan.project_id,
                &plan.session_id,
            ));
        }
    }

    if let Err(err) = create_session(exec, host, plan) {
        // A non-duplicate failure means no session was created, so the
        // worktree we just made is orphaned — best-effort remove it so the
        // slot stays retryable. A duplicate means a live session owns it.
        if plan.branch.is_some() && !matches!(err, SourceError::SessionExists { .. }) {
            let _ = exec.run(&worktree_remove_argv(host, plan));
        }
        return Err(err);
    }

    write_metadata(exec, host, plan);
    attach_channel(exec, host, &plan.tmux_name)
}

/// Re-creates a stopped session's tmux session and attaches. Unlike spawn:
/// no `worktree add` (the worktree survives), and a duplicate name means a
/// concurrent respawner already won — attach to the live session instead of
/// erroring (ADR-0004). Requires worktree mode (R5): a shared-mode plan can't
/// claim a worktree, so it errors before any remote command.
///
/// A `test -d` preflight confirms the worktree directory still exists before
/// any tmux command: `tmux new-session -c <dir>` does not fail-closed on a
/// vanished start directory across tmux versions (the pane can chdir-fail and
/// exit, self-destructing the session), so a gone worktree surfaces here as
/// `SessionNotFound` rather than a confusing post-attach channel death.
fn run_respawn(
    exec: &dyn SshExec,
    host: &SshHost,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    if plan.branch.is_none() {
        return Err(PlanError::NotWorktreeProject(plan.project_id.clone()).into());
    }
    let probe = exec.run(&dir_exists_argv(host, &plan.dir))?;
    if !probe.success {
        // `test -d` is silent; empty stderr means the dir is gone (nothing to
        // respawn), non-empty means the probe itself couldn't run.
        return if probe.stderr.trim().is_empty() {
            Err(SourceError::SessionNotFound {
                project_id: plan.project_id.clone(),
                session_id: plan.session_id.clone(),
            })
        } else {
            Err(SourceError::Transport(probe.stderr))
        };
    }
    match create_session(exec, host, plan) {
        Ok(()) => {
            write_metadata(exec, host, plan);
            attach_channel(exec, host, &plan.tmux_name)
        }
        // Concurrent respawner already created it: attach to the live session.
        Err(SourceError::SessionExists { .. }) => attach_channel(exec, host, &plan.tmux_name),
        Err(err) => Err(err),
    }
}

/// Reads a live session's metadata; a failed `show-environment` (race: the
/// session died after `list-sessions`) yields empty metadata — the session is
/// still listed `Live` (don't downgrade a known-live session on a metadata
/// read flake). The target name is rebuilt from validated ids.
fn read_environment(
    exec: &dyn SshExec,
    host: &SshHost,
    project: &ProjectId,
    session: &SessionId,
) -> DiscoveredEnv {
    let tmux_name = tmux_session_name(project, session);
    match exec.run(&show_environment_query_argv(host, &tmux_name)) {
        Ok(out) if out.success => discovery::parse_session_environment(&out.stdout),
        _ => DiscoveredEnv::default(),
    }
}

/// Discovers sessions on the host and joins them to local config. Config-
/// scoped throughout (R1): the live set keeps only configured projects, and
/// the stopped scan runs only for configured worktree-mode projects.
fn run_list(
    exec: &dyn SshExec,
    host: &SshHost,
    config: &Config,
) -> Result<Vec<SessionMeta>, SourceError> {
    let names = classify_list_sessions(&exec.run(&list_sessions_argv(host))?)?;

    let mut live = Vec::new();
    for name in &names {
        let Some((project, session)) = parse_tmux_session_name(name) else {
            continue; // forged / non-remora name dropped
        };
        if !config.projects.contains_key(&project) {
            continue; // R1: configured projects only
        }
        let env = read_environment(exec, host, &project, &session);
        live.push((project, session, env));
    }

    let mut stopped = Vec::new();
    for (project_id, project) in &config.projects {
        if project.workspace != WorkspaceMode::Worktree {
            continue; // shared projects have no surviving worktree
        }
        // A failure for one project (bad path, not a repo) yields empty for
        // that project, never a failed discovery (decision 8).
        if let Ok(out) = exec.run(&worktree_list_argv(host, &project.path)) {
            if out.success {
                for (session, path) in discovery::parse_worktree_list(&out.stdout, project_id) {
                    stopped.push((project_id.clone(), session, path));
                }
            }
        }
    }

    Ok(discovery::join(live, stopped))
}

#[async_trait]
impl SessionSource for SshSource {
    /// Resolves the spawn plan from config, then runs the full spawn
    /// orchestration (worktree add → tmux new-session → env metadata →
    /// attach) via the injectable `SshExec` seam.
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        let host = self.host.clone();
        tokio::task::spawn_blocking(move || run_spawn(exec.as_ref(), &host, &plan))
            .await
            .map_err(|e| SourceError::Transport(format!("spawn task: {e}")))?
    }

    /// Opens a channel to an existing *live* session over ssh.
    ///
    /// A `tmux has-session` preflight returns `SessionNotFound` for a missing
    /// or stopped session (honoring the trait contract stage 4 deferred); a
    /// dead-pane session still exists and is attachable. The TOCTOU window
    /// (session dies between preflight and attach) degrades to channel death.
    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let tmux_name = tmux_session_name(project_id, session_id);
        let exec = Arc::clone(&self.exec);
        let host = self.host.clone();
        let project_id = project_id.clone();
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let out = exec.run(&has_session_argv(&host, &tmux_name))?;
            if !out.success {
                return Err(SourceError::SessionNotFound {
                    project_id,
                    session_id,
                });
            }
            attach_channel(exec.as_ref(), &host, &tmux_name)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("attach task: {e}")))?
    }

    /// Discovers sessions on the host and joins them to local config.
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let exec = Arc::clone(&self.exec);
        let host = self.host.clone();
        let config = Arc::clone(&self.config);
        tokio::task::spawn_blocking(move || run_list(exec.as_ref(), &host, &config))
            .await
            .map_err(|e| SourceError::Transport(format!("list task: {e}")))?
    }

    async fn respawn(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        agent: Option<remora_protocol::AgentId>,
    ) -> Result<SessionChannel, SourceError> {
        // REMORA_CREATED_AT is re-stamped by spawn's metadata write; the agent
        // is carried by the client from pre-stop discovery (D6), else the
        // project default resolves inside plan_spawn.
        let spec = SpawnSpec {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            agent,
        };
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        let host = self.host.clone();
        tokio::task::spawn_blocking(move || run_respawn(exec.as_ref(), &host, &plan))
            .await
            .map_err(|e| SourceError::Transport(format!("respawn task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceMode;
    use crate::spawn_plan::SpawnPlan;
    use crate::SessionSource;
    use remora_protocol::SessionState;

    fn host(host: &str, user: Option<&str>, port: Option<u16>) -> SshHost {
        SshHost {
            host: host.to_string(),
            user: user.map(String::from),
            port,
        }
    }

    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("slug")
    }

    fn sid(s: &str) -> SessionId {
        SessionId::new(s).expect("slug")
    }

    fn spec() -> SpawnSpec {
        use remora_protocol::AgentId;
        SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            agent: Some(AgentId::new("claude").expect("slug")),
        }
    }

    fn test_config() -> Arc<Config> {
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"
            [agents.claude]
            command = ["claude"]
        "#;
        Arc::new(Config::from_toml_str(toml).expect("config"))
    }

    /// Config with two agents: default "claude" for the api project, and a
    /// second "codex" agent — used to prove respawn can override the default.
    fn two_agent_config() -> Arc<Config> {
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"
            [agents.claude]
            command = ["claude"]
            [agents.codex]
            command = ["codex"]
        "#;
        Arc::new(Config::from_toml_str(toml).expect("config"))
    }

    /// Scripted executor: returns queued `run` results in order, records every
    /// argv, and hands back a dead `SessionChannel` for `open_channel`.
    struct FakeExec {
        results: std::sync::Mutex<std::collections::VecDeque<Result<RemoteOutput, SourceError>>>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        opened: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl FakeExec {
        fn new(results: Vec<Result<RemoteOutput, SourceError>>) -> Self {
            Self {
                results: std::sync::Mutex::new(results.into_iter().collect()),
                calls: std::sync::Mutex::new(Vec::new()),
                opened: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn ok() -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
        fn out(stdout: &str) -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: stdout.into(),
                stderr: String::new(),
            }
        }
        fn fail(stderr: &str) -> RemoteOutput {
            RemoteOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.into(),
            }
        }

        /// Returns the first recorded argv that contains the given substring.
        /// Panics if no call contains the substring.
        fn recorded_argv_containing(&self, needle: &str) -> Vec<String> {
            self.calls
                .lock()
                .expect("lock")
                .iter()
                .find(|argv| argv.iter().any(|a| a.contains(needle)))
                .cloned()
                .unwrap_or_else(|| panic!("no recorded argv contains {needle:?}"))
        }
    }

    impl SshExec for FakeExec {
        fn run(&self, argv: &[String]) -> Result<RemoteOutput, SourceError> {
            self.calls.lock().expect("lock").push(argv.to_vec());
            self.results
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| Ok(FakeExec::ok()))
        }
        fn open_channel(&self, argv: &[String]) -> Result<SessionChannel, SourceError> {
            self.opened.lock().expect("lock").push(argv.to_vec());
            let (channel, _rx, _tx) = SessionChannel::pair();
            Ok(channel)
        }
    }

    #[test]
    fn argv_minimal_host_has_keepalive_no_dashdash() {
        let argv = attach_argv(&host("devbox", None, None), "remora_api_fix-login");
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-tt",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ConnectTimeout=10",
                "devbox",
                "tmux",
                "attach-session",
                "-d",
                "-t",
                "remora_api_fix-login",
            ]
        );
        assert!(!argv.iter().any(|a| a == "--"), "no options terminator");
    }

    #[test]
    fn argv_inserts_port_then_user_before_host() {
        let argv = attach_argv(&host("devbox", Some("dev"), Some(2222)), "remora_api_s");
        // Order: ...keepalive, -p <port>, -l <user>, <host>, remote cmd.
        let host_idx = argv
            .iter()
            .position(|a| a == "devbox")
            .expect("host present");
        let p = argv.iter().position(|a| a == "-p").expect("-p present");
        let l = argv.iter().position(|a| a == "-l").expect("-l present");
        assert_eq!(argv[p + 1], "2222");
        assert_eq!(argv[l + 1], "dev");
        assert!(p < host_idx && l < host_idx, "flags precede the host");
        assert!(p < l, "port before user");
    }

    #[test]
    fn argv_omits_absent_optional_flags() {
        let argv = attach_argv(&host("devbox", None, None), "remora_api_s");
        assert!(!argv.iter().any(|a| a == "-p"), "no port flag");
        assert!(!argv.iter().any(|a| a == "-l"), "no user flag");
    }

    #[test]
    fn argv_carries_tmux_name_and_eviction_flags() {
        let argv = attach_argv(&host("devbox", None, None), "remora_web_zeta");
        assert_eq!(argv.last().map(String::as_str), Some("remora_web_zeta"));
        assert!(argv.iter().any(|a| a == "-tt"), "forces remote PTY");
        // `-d` is the tmux eviction flag, positioned after attach-session.
        let attach = argv
            .iter()
            .position(|a| a == "attach-session")
            .expect("attach");
        assert_eq!(argv[attach + 1], "-d");
    }

    #[tokio::test]
    async fn spawn_unknown_project_is_a_plan_error() {
        let source = SshSource::new(host("devbox", None, None), Arc::new(Config::default()));
        let err = source.spawn(spec()).await.expect_err("no such project");
        assert!(matches!(err, SourceError::Plan(_)), "{err}");
    }

    #[tokio::test]
    async fn usable_through_dyn_session_source() {
        let source: Box<dyn SessionSource> = Box::new(SshSource::new(
            host("devbox", None, None),
            Arc::new(Config::default()),
        ));
        assert!(source.spawn(spec()).await.is_err());
    }

    #[test]
    fn shell_quote_leaves_simple_tokens_and_quotes_spaces() {
        assert_eq!(shell_quote("claude"), "claude");
        assert_eq!(shell_quote("--continue"), "--continue");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn quote_remote_path_expands_tilde_via_home() {
        // `~/x` -> $HOME stays expandable; the remainder has no shell-special
        // chars so shlex returns it bare (slug/path chars are safe).
        assert_eq!(quote_remote_path("~/api"), "\"$HOME\"/api");
        assert_eq!(quote_remote_path("~"), "\"$HOME\"");
        // absolute path: all safe chars, returned bare (no quoting needed).
        assert_eq!(quote_remote_path("/home/dev/api"), "/home/dev/api");
        // a space in a path WOULD force quoting (defensive, not expected for slugs).
        assert_eq!(quote_remote_path("/a b"), "'/a b'");
        // `~/` + a space: $HOME stays unquoted, the remainder is quoted, and
        // the remote shell concatenates the two adjacent segments into one word.
        assert_eq!(quote_remote_path("~/a b"), "\"$HOME\"'/a b'");
    }

    fn worktree_plan() -> SpawnPlan {
        SpawnPlan {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            tmux_name: "remora_api_fix-login".into(),
            workspace: WorkspaceMode::Worktree,
            project_path: "/home/dev/api".into(),
            dir: "~/.remora/worktrees/api/fix-login".into(),
            branch: Some("remora/fix-login".into()),
            env: vec![
                ("REMORA_AGENT".into(), "claude".into()),
                (
                    "REMORA_WORKSPACE".into(),
                    "~/.remora/worktrees/api/fix-login".into(),
                ),
                ("REMORA_CREATED_AT".into(), "1700000000".into()),
            ],
            agent_argv: vec!["claude".into(), "--continue".into()],
        }
    }

    #[test]
    fn worktree_add_argv_builds_git_command() {
        let plan = worktree_plan();
        let argv = worktree_add_argv(&host("devbox", None, None), &plan);
        let g = argv.iter().position(|a| a == "git").expect("git");
        assert_eq!(argv[g + 1], "-C");
        assert_eq!(argv[g + 2], "/home/dev/api");
        assert_eq!(argv[g + 3], "worktree");
        assert_eq!(argv[g + 4], "add");
        assert_eq!(argv[g + 5], "-b");
        assert_eq!(argv[g + 6], "remora/fix-login");
        assert_eq!(argv[g + 7], "\"$HOME\"/.remora/worktrees/api/fix-login");
        assert!(!argv.iter().any(|a| a == "-tt"), "setup is non-interactive");
    }

    #[test]
    fn new_session_argv_is_the_lock_with_no_metadata_trailer() {
        let plan = worktree_plan();
        let argv = new_session_argv(&host("devbox", None, None), &plan);
        let n = argv
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        assert_eq!(argv[n + 1], "-d");
        assert_eq!(argv[n + 2], "-s");
        assert_eq!(argv[n + 3], "remora_api_fix-login");
        assert_eq!(argv[n + 4], "-c");
        assert_eq!(argv[n + 5], "\"$HOME\"/.remora/worktrees/api/fix-login");
        // Agent command wrapped in the shell-fallback compound, then joined into
        // ONE shell-quoted arg (double-shell safe).
        assert_eq!(
            argv[n + 6],
            shell_quote(&wrap_with_shell_fallback("claude --continue"))
        );
        assert!(argv.get(n + 7).is_none(), "agent command is a single arg");
        // The `;` inside the wrapped command is part of the single quoted arg,
        // never its own argv element — so there is still no metadata trailer.
        assert!(!argv.iter().any(|a| a == "set-environment" || a == ";"));
    }

    #[test]
    fn wrap_with_shell_fallback_gates_on_clean_exit_codes() {
        let wrapped = wrap_with_shell_fallback("claude --continue");
        // The agent runs first; `$?` is captured IMMEDIATELY after (nothing may
        // run between the agent and the capture or it records the wrong code).
        assert!(
            wrapped.starts_with("claude --continue; __remora_rc=$?;"),
            "got: {wrapped}"
        );
        // Graceful (0), SIGINT/Ctrl-C (130), SIGTERM (143) drop to a login shell.
        assert!(
            wrapped.contains(r#"0|130|143) exec "${SHELL:-/bin/sh}" -l ;;"#),
            "got: {wrapped}"
        );
        // Any other code propagates so `remain-on-exit` keeps the dead pane (#28).
        assert!(
            wrapped.contains(r#"*) exit "$__remora_rc" ;;"#),
            "got: {wrapped}"
        );
    }

    #[test]
    fn agent_command_survives_the_double_shell() {
        // An agent arg containing a space must survive BOTH the ssh login shell
        // and tmux's `sh -c` re-parse — now wrapped in the shell-fallback compound.
        let plan = SpawnPlan {
            agent_argv: vec![
                "claude".into(),
                "--append-system-prompt".into(),
                "Be concise".into(),
            ],
            ..worktree_plan()
        };
        let argv = new_session_argv(&host("devbox", None, None), &plan);
        let n = argv
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        // Still exactly one agent-command arg after `-c <dir>` (wrapped, not per-token).
        assert_eq!(argv.len(), n + 7);
        // The inner string the ssh login shell yields (tmux's `sh -c` re-parses
        // it) is the wrapped compound built from the joined agent fragment.
        let fragment = join_agent_command(&plan.agent_argv);
        let inner = wrap_with_shell_fallback(&fragment);
        assert_eq!(argv[n + 6], shell_quote(&inner));
        // The agent fragment keeps its per-token quoting inside the compound, so
        // the spaced arg stays a single shell word ahead of the gate.
        assert!(
            inner.starts_with("claude --append-system-prompt 'Be concise';"),
            "got: {inner}"
        );
    }

    #[test]
    fn set_environment_argv_quotes_logical_value() {
        let argv = set_environment_argv(
            &host("devbox", None, None),
            "remora_api_fix-login",
            "REMORA_WORKSPACE",
            "~/.remora/worktrees/api/fix-login",
        );
        let s = argv
            .iter()
            .position(|a| a == "set-environment")
            .expect("set-env");
        assert_eq!(argv[s + 1], "-t");
        assert_eq!(argv[s + 2], "remora_api_fix-login");
        assert_eq!(argv[s + 3], "REMORA_WORKSPACE");
        assert_eq!(argv[s + 4], "'~/.remora/worktrees/api/fix-login'");
    }

    #[test]
    fn set_option_argv_sets_remain_on_exit() {
        let argv = set_option_remain_on_exit_argv(&host("devbox", None, None), "remora_api_x");
        let o = argv
            .iter()
            .position(|a| a == "set-option")
            .expect("set-option");
        assert_eq!(argv[o + 1], "-t");
        assert_eq!(argv[o + 2], "remora_api_x");
        assert_eq!(argv[o + 3], "remain-on-exit");
        assert_eq!(argv[o + 4], "on");
    }

    fn ids() -> (ProjectId, SessionId) {
        (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        )
    }

    #[test]
    fn dup_new_session_maps_to_session_exists_case_insensitive() {
        let (p, s) = ids();
        let err = classify_new_session_failure("duplicate session: remora_api_fix-login\n", &p, &s);
        assert!(matches!(err, SourceError::SessionExists { .. }));
        let err = classify_new_session_failure("DUPLICATE SESSION", &p, &s);
        assert!(matches!(err, SourceError::SessionExists { .. }));
    }

    #[test]
    fn other_new_session_failure_is_transport() {
        let (p, s) = ids();
        let err = classify_new_session_failure("no server running on /tmp/tmux", &p, &s);
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn existing_worktree_maps_to_session_exists() {
        let (p, s) = ids();
        let err = classify_worktree_add_failure("fatal: '<path>' already exists", &p, &s);
        assert!(matches!(err, SourceError::SessionExists { .. }));
        let err = classify_worktree_add_failure(
            "fatal: 'remora/fix-login' is already checked out at '<path>'",
            &p,
            &s,
        );
        assert!(matches!(err, SourceError::SessionExists { .. }));
    }

    #[test]
    fn other_worktree_failure_is_transport() {
        let (p, s) = ids();
        let err = classify_worktree_add_failure("fatal: not a git repository", &p, &s);
        assert!(matches!(err, SourceError::Transport(_)));
    }

    // --- run_spawn orchestration tests ---

    #[test]
    fn shared_spawn_skips_worktree_add() {
        let plan = SpawnPlan {
            branch: None,
            workspace: WorkspaceMode::Shared,
            dir: "/home/dev/api".into(),
            ..worktree_plan()
        };
        let fake = FakeExec::new(vec![
            // Only new-session should fire; all other run() calls succeed by default.
            Ok(FakeExec::ok()),
        ]);
        let result = run_spawn(&fake, &host("devbox", None, None), &plan);
        assert!(result.is_ok(), "{result:?}");
        // First call must be new-session (no worktree-add).
        let calls = fake.calls.lock().expect("lock");
        assert!(
            calls[0].iter().any(|a| a == "new-session"),
            "first call is new-session"
        );
        assert!(!calls[0].iter().any(|a| a == "worktree"), "no worktree-add");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn existing_worktree_aborts_before_create() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("fatal: '<path>' already exists"))]);
        let err = run_spawn(&fake, &host("devbox", None, None), &plan)
            .expect_err("worktree already exists");
        assert!(matches!(err, SourceError::SessionExists { .. }), "{err}");
        // Exactly 1 call (worktree-add), no channel opened.
        assert_eq!(fake.calls.lock().expect("lock").len(), 1);
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[test]
    fn duplicate_session_does_not_open_a_channel() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            // worktree add succeeds
            Ok(FakeExec::ok()),
            // new-session fails with duplicate
            Ok(FakeExec::fail("duplicate session: remora_api_fix-login")),
        ]);
        let err =
            run_spawn(&fake, &host("devbox", None, None), &plan).expect_err("duplicate session");
        assert!(matches!(err, SourceError::SessionExists { .. }), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[test]
    fn run_spawn_opens_exactly_one_channel_on_success() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![]);
        let result = run_spawn(&fake, &host("devbox", None, None), &plan);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn run_spawn_attach_argv_ends_with_tmux_name() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![]);
        let _ = run_spawn(&fake, &host("devbox", None, None), &plan);
        let opened = fake.opened.lock().expect("lock");
        let attach_argv = &opened[0];
        assert_eq!(
            attach_argv.last().map(String::as_str),
            Some("remora_api_fix-login")
        );
    }

    #[test]
    fn worktree_spawn_runs_add_create_metadata_then_attaches_in_order() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),
            Ok(FakeExec::ok()),
            Ok(FakeExec::ok()),
            Ok(FakeExec::ok()),
            Ok(FakeExec::ok()),
            Ok(FakeExec::ok()),
        ]);
        let result = run_spawn(&fake, &host("devbox", None, None), &plan);
        assert!(result.is_ok());
        let calls = fake.calls.lock().expect("lock");
        // worktree add, new-session, set-option (remain-on-exit first), then
        // 3x set-environment = 6 blocking cmds.
        assert_eq!(calls.len(), 6);
        assert!(calls[0].iter().any(|a| a == "worktree"));
        assert!(calls[1].iter().any(|a| a == "new-session"));
        assert!(calls[2].iter().any(|a| a == "set-option"));
        assert!(calls[3].iter().any(|a| a == "set-environment"));
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn metadata_failure_is_tolerated_and_still_attaches() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                        // worktree add
            Ok(FakeExec::ok()),                        // new-session (live!)
            Ok(FakeExec::fail("opt boom")),            // remain-on-exit — tolerated
            Ok(FakeExec::fail("set-env boom")),        // REMORA_AGENT — tolerated
            Err(SourceError::Transport("net".into())), // REMORA_WORKSPACE — tolerated
            Ok(FakeExec::ok()),                        // REMORA_CREATED_AT
        ]);
        let result = run_spawn(&fake, &host("devbox", None, None), &plan);
        assert!(
            result.is_ok(),
            "metadata failures must not fail a live session"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn spawn_through_fake_exec_attaches() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let result = source.spawn(spec()).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
        // The plan's tmux name and env reached the recorded argv (not just
        // that the wiring dispatched).
        let calls = fake.calls.lock().expect("lock");
        assert!(
            calls.iter().any(|c| c.iter().any(|a| a == "new-session"))
                && calls
                    .iter()
                    .any(|c| c.iter().any(|a| a == "remora_api_fix-login")),
            "new-session argv carries the planned tmux name"
        );
        assert!(
            calls.iter().any(|c| c.iter().any(|a| a == "REMORA_AGENT")),
            "metadata was written via set-environment"
        );
    }

    #[test]
    fn new_session_generic_failure_is_transport_and_opens_no_channel() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                      // worktree add
            Ok(FakeExec::fail("no server running")), // new-session: generic failure
        ]);
        let err = run_spawn(&fake, &host("devbox", None, None), &plan).expect_err("should fail");
        assert!(matches!(err, SourceError::Transport(_)));
        assert!(fake.opened.lock().expect("lock").is_empty());
        // The orphaned worktree is cleaned up so the slot stays retryable.
        let calls = fake.calls.lock().expect("lock");
        assert!(
            calls.last().expect("a call").iter().any(|a| a == "remove"),
            "non-duplicate failure removes the orphaned worktree"
        );
    }

    #[test]
    fn duplicate_session_does_not_remove_the_worktree() {
        // A duplicate means a LIVE session owns the worktree — never remove it.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()), // worktree add
            Ok(FakeExec::fail("duplicate session: remora_api_fix-login")),
        ]);
        let err = run_spawn(&fake, &host("devbox", None, None), &plan).expect_err("dup");
        assert!(matches!(err, SourceError::SessionExists { .. }));
        let calls = fake.calls.lock().expect("lock");
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "remove")),
            "must not remove a live session's worktree"
        );
    }

    #[tokio::test]
    async fn list_joins_live_metadata_stopped_and_filters_unconfigured() {
        // Config has project `api` (worktree). `ghost` is NOT configured.
        let config = test_config();
        // Scripted exec, in call order:
        //  1) list-sessions -> api (configured) + ghost (unconfigured) +
        //     `main` & `remora__bad` (unparseable) — only api survives.
        //  2) show-environment for api/fix-login -> metadata
        //  3) git worktree list for api -> fix-login (live) + add-tests (stopped)
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\nremora_ghost_x\nmain\nremora__bad\n")),
            Ok(FakeExec::out("REMORA_AGENT=claude\nREMORA_CREATED_AT=1765500000\n")),
            Ok(FakeExec::out(
                "worktree /home/dev/.remora/worktrees/api/fix-login\nbranch refs/heads/remora/fix-login\n\n\
                 worktree /home/dev/.remora/worktrees/api/add-tests\nbranch refs/heads/remora/add-tests\n",
            )),
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let metas = source.list().await.expect("list");

        // ghost filtered out (R1). api/add-tests is Stopped; api/fix-login is Live.
        let keys: Vec<(&str, &str, SessionState)> = metas
            .iter()
            .map(|m| (m.project_id.as_str(), m.session_id.as_str(), m.state))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("api", "add-tests", SessionState::Stopped),
                ("api", "fix-login", SessionState::Live),
            ]
        );
        let live = &metas[1];
        assert_eq!(live.agent.as_deref(), Some("claude"));
        // Stopped carries the real discovered worktree path (R6).
        assert_eq!(
            metas[0].workspace_path.as_deref(),
            Some("/home/dev/.remora/worktrees/api/add-tests")
        );
    }

    #[tokio::test]
    async fn list_treats_no_server_as_empty() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "no server running on /tmp/tmux-1000/default",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        assert!(source.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn list_keeps_session_live_when_show_environment_fails() {
        // The session is listed live, but its show-environment read flakes
        // (race: it could die between list-sessions and the metadata read).
        // The session must stay Live with empty metadata, not be downgraded.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // list-sessions
            Ok(FakeExec::fail("connection reset")),      // show-environment flakes
            Ok(FakeExec::out("")),                       // worktree list: empty
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let metas = source.list().await.expect("list");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].agent, None);
        assert_eq!(metas[0].created_at, None);
    }

    #[tokio::test]
    async fn list_survives_worktree_list_failure_per_decision_8() {
        // A failed `git worktree list` for one project yields empty for that
        // project, never a failed discovery (decision 8): the live session
        // still lists, just with no Stopped twin.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // list-sessions
            Ok(FakeExec::out("REMORA_AGENT=claude\n")),  // show-environment
            Ok(FakeExec::fail("fatal: not a git repository")), // worktree list fails
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let metas = source.list().await.expect("list must not fail");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id.as_str(), "fix-login");
        assert_eq!(metas[0].state, SessionState::Live);
        assert!(metas.iter().all(|m| m.state == SessionState::Live));
    }

    #[test]
    fn classify_list_sessions_maps_no_sessions_to_empty() {
        // The second cold-state phrase tmux emits (server up, zero sessions).
        let out = RemoteOutput {
            success: false,
            stdout: String::new(),
            stderr: "no sessions".into(),
        };
        assert_eq!(
            classify_list_sessions(&out).expect("ok"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn classify_list_sessions_maps_no_server_to_empty() {
        let empty = RemoteOutput {
            success: false,
            stdout: String::new(),
            stderr: "no server running on /tmp/tmux".into(),
        };
        assert_eq!(
            classify_list_sessions(&empty).expect("ok"),
            Vec::<String>::new()
        );

        let ok = RemoteOutput {
            success: true,
            stdout: "remora_api_x\nremora_api_y\n".into(),
            stderr: String::new(),
        };
        assert_eq!(
            classify_list_sessions(&ok).expect("ok"),
            vec!["remora_api_x", "remora_api_y"]
        );

        let boom = RemoteOutput {
            success: false,
            stdout: String::new(),
            stderr: "permission denied".into(),
        };
        assert!(matches!(
            classify_list_sessions(&boom),
            Err(SourceError::Transport(_))
        ));
    }

    #[tokio::test]
    async fn attach_returns_not_found_when_session_is_absent() {
        let config = test_config();
        // has-session preflight fails (no such session) -> SessionNotFound,
        // and NO channel is opened.
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "can't find session",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        let err = source.attach(&project, &session).await.expect_err("absent");
        assert!(matches!(err, SourceError::SessionNotFound { .. }), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn attach_opens_channel_when_session_is_live() {
        let config = test_config();
        // has-session succeeds -> channel opened.
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::ok())]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        source.attach(&project, &session).await.expect("attach");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn has_session_argv_targets_the_name() {
        let argv = has_session_argv(&host("devbox", None, None), "remora_api_fix-login");
        let h = argv
            .iter()
            .position(|a| a == "has-session")
            .expect("has-session");
        assert_eq!(argv[h + 1], "-t");
        assert_eq!(argv[h + 2], "remora_api_fix-login");
    }

    #[test]
    fn interactive_command_pins_term_to_xterm_256color() {
        // xterm.js (the app's emulator) speaks xterm-256color; the remote tmux
        // must see that, not whatever TERM the app process inherited.
        let cmd = command_from_argv(&["ssh".to_string(), "host".to_string()]);
        assert_eq!(cmd.get_env("TERM"), Some("xterm-256color".as_ref()));
    }

    #[test]
    fn list_sessions_argv_quotes_the_format_string() {
        let argv = list_sessions_argv(&host("devbox", None, None));
        // `#{session_name}` MUST be shell-quoted: a bare `#` starts a comment
        // in the remote login shell and would swallow the format argument.
        assert_eq!(argv.last().map(String::as_str), Some("'#{session_name}'"));
        let l = argv
            .iter()
            .position(|a| a == "list-sessions")
            .expect("list-sessions");
        assert_eq!(argv[l + 1], "-F");
    }

    #[tokio::test]
    async fn respawn_creates_without_worktree_add_and_attaches() {
        let config = test_config(); // api is worktree-mode
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d preflight: dir exists
            Ok(FakeExec::ok()), // new-session ok
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        source
            .respawn(&project, &session, None)
            .await
            .expect("respawn");
        let calls = fake.calls.lock().expect("lock");
        // First call is the `test -d` preflight; then new-session; never a
        // `git worktree add` (the worktree survives).
        assert!(calls[0].iter().any(|a| a == "test") && calls[0].iter().any(|a| a == "-d"));
        assert!(calls[1].iter().any(|a| a == "new-session"));
        assert!(!calls.iter().any(|c| c.iter().any(|a| a == "add")));
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn respawn_duplicate_attaches_to_live_session() {
        let config = test_config();
        // Preflight passes, then new-session reports duplicate -> respawn
        // attaches to the live session instead of erroring.
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d preflight: dir exists
            Ok(FakeExec::fail("duplicate session: remora_api_fix-login")),
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        source
            .respawn(&project, &session, None)
            .await
            .expect("respawn attaches");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn respawn_of_vanished_worktree_is_not_found() {
        // The worktree directory was removed out from under the stopped
        // session: `test -d` fails with empty stderr -> SessionNotFound, and
        // NO tmux command runs (no new-session, no channel).
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(""))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("vanished");
        assert!(matches!(err, SourceError::SessionNotFound { .. }), "{err}");
        let calls = fake.calls.lock().expect("lock");
        assert_eq!(calls.len(), 1, "only the preflight runs");
        assert!(!calls.iter().any(|c| c.iter().any(|a| a == "new-session")));
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn respawn_preflight_probe_failure_is_transport() {
        // The probe itself could not run (ssh/transport): non-empty stderr ->
        // Transport, distinct from a vanished worktree.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "ssh: connect to host devbox port 22: Connection refused",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("probe failed");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn respawn_generic_new_session_failure_is_transport_and_opens_no_channel() {
        // Preflight passes; new-session fails non-duplicate -> Transport, no
        // channel (the `Err(err) => Err(err)` arm of run_respawn).
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()),                      // test -d preflight: dir exists
            Ok(FakeExec::fail("no server running")), // new-session: generic failure
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("api").expect("slug"),
            SessionId::new("fix-login").expect("slug"),
        );
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("generic");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn respawn_of_shared_project_errors_before_any_remote_call() {
        // A shared-mode project resolves to branch=None -> NotWorktreeProject,
        // and no remote command runs.
        let toml = r#"
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
        "#;
        let config = Arc::new(Config::from_toml_str(toml).expect("config"));
        let fake = Arc::new(FakeExec::new(vec![]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (
            ProjectId::new("scratch").expect("slug"),
            SessionId::new("s1").expect("slug"),
        );
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("shared");
        assert!(
            matches!(err, SourceError::Plan(PlanError::NotWorktreeProject(_))),
            "{err}"
        );
        assert!(
            fake.calls.lock().expect("lock").is_empty(),
            "no remote call"
        );
    }

    #[tokio::test]
    async fn respawn_uses_the_supplied_agent_not_the_project_default() {
        use remora_protocol::AgentId;
        // Project default is "claude"; respawn with "codex" must launch codex.
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d preflight: dir exists
            Ok(FakeExec::ok()), // new-session ok
        ]));
        let src =
            SshSource::with_exec(host("devbox", None, None), two_agent_config(), fake.clone());
        let _ = src
            .respawn(
                &pid("api"),
                &sid("fix"),
                Some(AgentId::new("codex").expect("slug")),
            )
            .await;
        // The new-session argv carries the codex launch command.
        let new_session = fake.recorded_argv_containing("new-session");
        assert!(
            new_session.iter().any(|a| a.contains("codex")),
            "respawn should launch the supplied agent, got: {new_session:?}"
        );
    }
}
