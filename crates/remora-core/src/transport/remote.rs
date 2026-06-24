//! Transport-neutral core shared by every PTY-backed transport (ssh, kubectl).
//!
//! Holds the exec seam (`RemoteExec`), the two shared exec tails (`capture`
//! for blocking commands, `open_pty` for interactive channels), the logical
//! remote-command token builders, error classification, and the
//! spawn/respawn/list orchestration. A transport is just a connection adapter:
//! it composes its argv (connection prefix + shell wrap + interactive flag)
//! and delegates the tail here. This is the seam that proves the design isn't
//! ssh-shaped (roadmap stage 12).

use portable_pty::CommandBuilder;
use remora_protocol::{ProjectId, SessionId, SessionMeta};

use super::pty_process::spawn_pty_channel;
use crate::config::{Config, WorkspaceMode};
use crate::discovery::{self, DiscoveredEnv};
use crate::naming::{branch_name, parse_tmux_session_name, tmux_session_name, worktree_path};
use crate::spawn_plan::{PlanError, SpawnPlan};
use crate::{DirtyReason, SessionChannel, SourceError};

/// Result of a blocking remote command: success, captured stdout, and stderr.
pub(crate) struct RemoteOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The executor seam every spawn/attach/list step crosses. A transport's impl
/// composes the full argv (connection prefix + tokens) and delegates to
/// [`capture`] / [`open_pty`]. Interactive-ness is implied by which method is
/// called, so the impl owns the interactive flag and any shell wrapping.
pub(crate) trait RemoteExec: Send + Sync {
    /// Run a blocking, non-interactive remote command; capture status+output.
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError>;
    /// Open an interactive PTY channel running the remote command.
    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError>;
}

/// Runs a fully-composed argv to completion and captures its output. The
/// shared tail of every transport's `RemoteExec::run`.
///
/// Precondition: `argv` is non-empty (program + args).
pub(crate) fn capture(argv: &[String]) -> Result<RemoteOutput, SourceError> {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| SourceError::Transport(format!("exec: {e}")))?;
    Ok(RemoteOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Opens a PTY channel running a fully-composed argv. The shared tail of every
/// transport's `RemoteExec::open_channel`.
pub(crate) fn open_pty(argv: &[String]) -> Result<SessionChannel, SourceError> {
    spawn_pty_channel(command_from_argv(argv))
}

/// Turns a pure argv into a `CommandBuilder` (program = argv[0]) with `TERM`
/// pinned so a remote tmux resolves terminfo for xterm.js regardless of the
/// launching shell's `$TERM` (#26). ssh forwards this to the remote PTY; the
/// kubectl transport instead pins `TERM` (and a UTF-8 locale) in-container via
/// its own shell preamble because kubectl forwards neither the client TERM nor
/// a locale into the pod.
///
/// Precondition: `argv` is non-empty.
pub(crate) fn command_from_argv(argv: &[String]) -> CommandBuilder {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.env("TERM", "xterm-256color");
    cmd
}

// ---------------------------------------------------------------------------
// Quoting helpers (shared by all transports that speak to a remote shell)
// ---------------------------------------------------------------------------

/// Single-token shell quoting for the remote login shell, via `shlex`.
/// Config validation bans control/nul characters (stage 3), so `try_quote`
/// cannot hit its nul-byte error path here.
pub(crate) fn shell_quote(token: &str) -> String {
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
pub(crate) fn quote_remote_path(path: &str) -> String {
    if path == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("\"$HOME\"{}", shell_quote(&format!("/{rest}")))
    } else {
        shell_quote(path)
    }
}

/// Joins the agent argv into a single shell command line (one sh-safe
/// string). `tmux new-session` re-runs its shell-command argument through
/// `sh -c` — a *second* shell parse — so per-token quoting alone would be
/// stripped before tmux sees it. Joining here (minimal per-token quoting)
/// produces a string that `sh -c` re-parses back into the original argv.
/// Config bans control/nul characters, so `try_join` cannot error.
pub(crate) fn join_agent_command(argv: &[String]) -> String {
    shlex::try_join(argv.iter().map(String::as_str)).expect("config bans control/nul characters")
}

/// The pane command for a no-agent (plain shell) session (#35): an explicit
/// login shell. Double-quoted `$SHELL` so an unset SHELL falls back to
/// `/bin/sh`; `-l` for a login shell. Used verbatim instead of the agent-exit
/// `wrap_with_shell_fallback` so the pane is the shell directly, and instead of
/// omitting the command (which would delegate to the host tmux's
/// `default-command`). `wrap_with_shell_fallback` execs this same constant on a
/// clean agent exit (#30/#44), so a plain-shell session and a finished-agent
/// session land in an identical shell by construction, not by coincidence.
pub(crate) const PLAIN_SHELL_COMMAND: &str = r#""${SHELL:-/bin/sh}" -l"#;

/// Wraps the joined agent command so a clean / user-interrupted exit
/// (0 graceful, 130 SIGINT/Ctrl-C, 143 SIGTERM) execs [`PLAIN_SHELL_COMMAND`]
/// (an interactive login shell) in the same dir, keeping the pane alive with a
/// real prompt (#30); any other non-zero exit propagates so `remain-on-exit`
/// retains the dead pane and its error for inspection (#28). Reusing the
/// constant guarantees this fallback shell is identical to a no-agent pane;
/// its `${SHELL:-/bin/sh}` defends against an unset SHELL in the pane
/// environment.
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
pub(crate) fn wrap_with_shell_fallback(agent_command: &str) -> String {
    format!(
        r#"{agent_command}; __remora_rc=$?; case "$__remora_rc" in 0|130|143) exec {PLAIN_SHELL_COMMAND} ;; *) exit "$__remora_rc" ;; esac"#
    )
}

// ---------------------------------------------------------------------------
// Remote-command token builders (transport-neutral; no connection prefix)
// ---------------------------------------------------------------------------

/// Tokens for attaching to `tmux_name`. `-d` evicts every other client on
/// attach (sequential-handoff model).
pub(crate) fn attach_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "attach-session".into(),
        "-d".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

/// Tokens for `git -C <project> worktree add -b <branch> <worktree>`.
/// Precondition: `plan.branch` is `Some` (worktree mode); the only caller
/// checks. `git worktree add` creates leading directories.
pub(crate) fn worktree_add_tokens(plan: &SpawnPlan) -> Vec<String> {
    let branch = plan.branch.as_deref().unwrap_or_default();
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(&plan.project_path),
        "worktree".into(),
        "add".into(),
        "-b".into(),
        shell_quote(branch),
        quote_remote_path(&plan.dir),
    ]
}

/// Tokens for `git -C <project> worktree remove --force <worktree>` — best-effort
/// cleanup of an orphaned worktree after a non-duplicate `new-session`
/// failure (no live session owns it), so the project/session slot stays
/// retryable. `--force` because the fresh worktree may have a checked-out
/// branch and no commits yet. Also used by teardown (`run_remove`) to
/// idempotently remove a completed session's worktree.
pub(crate) fn worktree_remove_tokens(project_path: &str, dir: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        quote_remote_path(dir),
    ]
}

/// Tokens for `tmux kill-session -t <name>`.
pub(crate) fn kill_session_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "kill-session".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

/// Tokens for `git -C <worktree> status --porcelain` — uncommitted-changes probe.
pub(crate) fn status_porcelain_tokens(worktree_dir: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(worktree_dir),
        "status".into(),
        "--porcelain".into(),
    ]
}

/// Tokens for `git -C <worktree> rev-list --count HEAD --not --remotes` — counts
/// commits not reachable from any remote-tracking ref.
pub(crate) fn not_on_remote_tokens(worktree_dir: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(worktree_dir),
        "rev-list".into(),
        "--count".into(),
        "HEAD".into(),
        "--not".into(),
        "--remotes".into(),
    ]
}

/// Tokens for `git -C <project> branch -D <branch>` — force-deletes the
/// session branch after the worktree is removed.
pub(crate) fn branch_delete_tokens(project_path: &str, branch: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "branch".into(),
        "-D".into(),
        branch.into(),
    ]
}

/// Tokens for the atomic session-creation command — a single `tmux` invocation
/// chaining, via tmux's own argv `;` separator:
///
/// ```text
/// set-option -g history-limit 50000              (deep scrollback, #53)
///   ';' new-session -d -s <name> -c <dir> <agent…>   (creation + the lock)
///   ';' set-option -t <name> remain-on-exit on   (retain a dead pane, #28)
///   ';' set-option -t <name> mouse on            (wheel → scrollback, #53)
/// ```
///
/// `history-limit` leads and is **global** (`-g`): tmux applies it only to
/// windows created *after* it is set, and the agent's window is created by the
/// chained `new-session`, so the limit must already be in effect — and the
/// session it would `-t` doesn't exist yet. The bump hits the tmux server
/// Remora owns, so a global default is benign. `remain-on-exit` and `mouse` are
/// live session options applied after creation, targeted by `-t <name>`.
/// `remain-on-exit` (the #28 self-destruct guard) comes BEFORE `mouse` on
/// purpose: a failing mid-chain `set-option` aborts the rest of the invocation,
/// and `mouse on` can fail on tmux < 2.1 (the option didn't exist yet), so
/// ordering the load-bearing guard ahead of the cosmetic option keeps a mouse
/// failure from stripping it. Chaining keeps every option in the SAME
/// invocation as the lock so none can land in a separate, failure-prone
/// round-trip.
///
/// The `;` is **shell-quoted** (`';'`) so the remote login shell passes it to
/// tmux as a literal separator token rather than eating it as a shell statement
/// separator — this is tmux's argv `;`, distinct from ADR-0004's un-batching of
/// shell-`;`-joined remote commands.
pub(crate) fn new_session_tokens(plan: &SpawnPlan) -> Vec<String> {
    // A no-agent plan (#35) renders an explicit login shell; a normal agent is
    // wrapped so a clean/interrupted exit drops to a shell (#30) while a crash
    // is retained (#28).
    let pane_command = if plan.agent_argv.is_empty() {
        shell_quote(PLAIN_SHELL_COMMAND)
    } else {
        shell_quote(&wrap_with_shell_fallback(&join_agent_command(
            &plan.agent_argv,
        )))
    };
    let sep = shell_quote(";");
    vec![
        "tmux".into(),
        // Deep scrollback for the agent's long output — must precede the window
        // `new-session` creates (#53).
        "set-option".into(),
        "-g".into(),
        "history-limit".into(),
        "50000".into(),
        sep.clone(),
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        plan.tmux_name.clone(),
        "-c".into(),
        quote_remote_path(&plan.dir),
        pane_command,
        sep.clone(),
        // remain-on-exit MUST precede mouse: a failing mid-chain set-option
        // aborts the rest of the tmux invocation, and remain-on-exit is the
        // load-bearing #28 guard (retains a pane whose process exits at startup
        // before it can self-destruct the session). `mouse on` is cosmetic and
        // can fail on tmux < 2.1 (the option didn't exist), so it rides LAST —
        // a mouse failure then can't strip the guard.
        "set-option".into(),
        "-t".into(),
        plan.tmux_name.clone(),
        "remain-on-exit".into(),
        "on".into(),
        sep,
        // Mouse mode: the scroll wheel drives tmux copy-mode/scrollback instead
        // of being translated into arrow keys (#53).
        "set-option".into(),
        "-t".into(),
        plan.tmux_name.clone(),
        "mouse".into(),
        "on".into(),
    ]
}

/// Tokens for `tmux set-environment -t <name> <key> <value>`. The value is
/// the logical metadata string, single-quoted as a literal (no tilde
/// expansion — the stored value must round-trip via stage-6 `show-environment`).
pub(crate) fn set_environment_tokens(tmux_name: &str, key: &str, value: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "set-environment".into(),
        "-t".into(),
        tmux_name.into(),
        key.into(),
        shell_quote(value),
    ]
}

/// Tokens for `tmux list-sessions -F '#{session_name}'`. The format string is
/// shell-quoted: a bare `#` would start a comment in the remote login shell.
pub(crate) fn list_sessions_tokens() -> Vec<String> {
    vec![
        "tmux".into(),
        "list-sessions".into(),
        "-F".into(),
        shell_quote("#{session_name}"),
    ]
}

/// Tokens for `tmux show-environment -t <name>` — reads one session's env
/// metadata.
pub(crate) fn show_environment_query_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "show-environment".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

/// Tokens for `git -C <project-path> worktree list --porcelain`.
pub(crate) fn worktree_list_tokens(project_path: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "worktree".into(),
        "list".into(),
        "--porcelain".into(),
    ]
}

/// Tokens for `test -d <dir>` — the respawn preflight that the worktree
/// directory still exists. `git worktree list` is the wrong probe here: it
/// reports git's admin entry, which survives a bare `rm -rf` of the worktree,
/// so it would pass for exactly the vanished-directory case we must reject.
/// `test` writes nothing to stderr, so a non-zero exit with empty stderr means
/// "dir gone" while a non-zero exit with stderr means the probe itself failed.
pub(crate) fn dir_exists_tokens(dir: &str) -> Vec<String> {
    vec!["test".into(), "-d".into(), quote_remote_path(dir)]
}

/// Tokens for `tmux has-session -t <name>` — the liveness preflight for
/// `attach`.
pub(crate) fn has_session_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "has-session".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

// ---------------------------------------------------------------------------
// Error classifiers
// ---------------------------------------------------------------------------

/// Classifies `tmux list-sessions`: success → session-name lines; a
/// no-server / no-sessions stderr → empty (the normal cold state, decision 9,
/// matched case-insensitively); any other failure → `Transport`.
pub(crate) fn classify_list_sessions(out: &RemoteOutput) -> Result<Vec<String>, SourceError> {
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
pub(crate) fn classify_new_session_failure(
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
pub(crate) fn classify_worktree_add_failure(
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

/// Whether a failed `has-session`/`new-session` stderr positively means the
/// session does not exist (server up but name unknown, or no server at all),
/// as opposed to an ambiguous transport failure (ssh/auth/network) that also
/// exits non-zero. Matched case-insensitively to survive a non-English
/// `LC_MESSAGES`. Mirrors the cold-state phrases in `classify_list_sessions`,
/// plus has-session's own `can't find session`.
pub(crate) fn stderr_signals_session_absent(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("can't find session")
        || lower.contains("no server running")
        || lower.contains("no sessions")
}

// ---------------------------------------------------------------------------
// Orchestration (transport-neutral; called by every transport's SessionSource)
// ---------------------------------------------------------------------------

/// `tmux new-session -d` — the atomic creation lock. `Ok` on success;
/// `Err(SessionExists)` on a duplicate name (case-insensitive); otherwise
/// `Err(Transport)`. Opens no channel.
pub(crate) fn create_session(exec: &dyn RemoteExec, plan: &SpawnPlan) -> Result<(), SourceError> {
    let out = exec.run(&new_session_tokens(plan))?;
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

/// Writes the `REMORA_*` env metadata. Every call is tolerated: a metadata
/// failure must never fail an already-live session (env is untrusted/display-
/// only — ADR-0004). `remain-on-exit` is no longer written here — it is applied
/// atomically inside `new_session_tokens` so a pane whose process exits at startup
/// (a bad start dir, a missing agent binary) is retained before it can
/// self-destruct the session, instead of racing these env-var round-trips and
/// turning the follow-up attach into a spurious `SessionNotFound` (#28).
pub(crate) fn write_metadata(exec: &dyn RemoteExec, plan: &SpawnPlan) {
    for (key, value) in &plan.env {
        let _ = exec.run(&set_environment_tokens(&plan.tmux_name, key, value));
    }
}

/// Opens the PTY attach channel to an existing session (no liveness
/// preflight — callers that need one do it first; see `SshSource::attach`).
pub(crate) fn attach_channel(
    exec: &dyn RemoteExec,
    tmux_name: &str,
) -> Result<SessionChannel, SourceError> {
    exec.open_channel(&attach_tokens(tmux_name))
}

/// Orchestrates the full spawn sequence: optional worktree creation, tmux
/// new-session (the atomic lock, which also applies `remain-on-exit`), env
/// metadata, then attach. Each step crosses the `RemoteExec` seam so tests can
/// inject a `FakeExec` without touching the network.
pub(crate) fn run_spawn(
    exec: &dyn RemoteExec,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    if plan.branch.is_some() {
        let out = exec.run(&worktree_add_tokens(plan))?;
        if !out.success {
            return Err(classify_worktree_add_failure(
                &out.stderr,
                &plan.project_id,
                &plan.session_id,
            ));
        }
    }

    if let Err(err) = create_session(exec, plan) {
        // A non-duplicate failure usually means no session was created, so the
        // worktree we just made is orphaned — best-effort remove it so the slot
        // stays retryable. A duplicate means a live session already owns it.
        //
        // But the lock command chains `new-session ';' set-option …` under one
        // exit code (#28, #53): a non-zero exit can also mean the session WAS
        // created and only a trailing set-option failed. Force-removing the worktree then
        // would yank a LIVE session's cwd out from under it. So gate the cleanup
        // on a has-session probe and remove only once it confirms no session
        // exists; if the probe can't run, leave the worktree (better an orphan
        // than a nuked live session).
        if plan.branch.is_some() && !matches!(err, SourceError::SessionExists { .. }) {
            // A non-zero `has-session` does NOT by itself mean "absent": an
            // ssh/auth/network failure also exits non-zero (as `Ok(success=
            // false)`, not `Err`). Only a known tmux "no such session" stderr is
            // safe to clean up on; treat anything ambiguous as "leave it".
            let session_absent = exec
                .run(&has_session_tokens(&plan.tmux_name))
                .map(|out| !out.success && stderr_signals_session_absent(&out.stderr))
                .unwrap_or(false);
            if session_absent {
                let _ = exec.run(&worktree_remove_tokens(&plan.project_path, &plan.dir));
            }
        }
        return Err(err);
    }

    write_metadata(exec, plan);
    attach_channel(exec, &plan.tmux_name)
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
pub(crate) fn run_respawn(
    exec: &dyn RemoteExec,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    if plan.branch.is_none() {
        return Err(PlanError::NotWorktreeProject(plan.project_id.clone()).into());
    }
    let probe = exec.run(&dir_exists_tokens(&plan.dir))?;
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
    match create_session(exec, plan) {
        Ok(()) => {
            write_metadata(exec, plan);
            attach_channel(exec, &plan.tmux_name)
        }
        // Concurrent respawner already created it: attach to the live session.
        Err(SourceError::SessionExists { .. }) => attach_channel(exec, &plan.tmux_name),
        Err(err) => Err(err),
    }
}

/// Reads a live session's metadata; a failed `show-environment` (race: the
/// session died after `list-sessions`) yields empty metadata — the session is
/// still listed `Live` (don't downgrade a known-live session on a metadata
/// read flake). The target name is rebuilt from validated ids.
pub(crate) fn read_environment(
    exec: &dyn RemoteExec,
    project: &ProjectId,
    session: &SessionId,
) -> DiscoveredEnv {
    let tmux_name = tmux_session_name(project, session);
    match exec.run(&show_environment_query_tokens(&tmux_name)) {
        Ok(out) if out.success => discovery::parse_session_environment(&out.stdout),
        _ => DiscoveredEnv::default(),
    }
}

/// Discovers sessions on the host and joins them to local config. Config-
/// scoped throughout (R1): the live set keeps only configured projects, and
/// the stopped scan runs only for configured worktree-mode projects.
pub(crate) fn run_list(
    exec: &dyn RemoteExec,
    config: &Config,
) -> Result<Vec<SessionMeta>, SourceError> {
    let names = classify_list_sessions(&exec.run(&list_sessions_tokens())?)?;

    let mut live = Vec::new();
    for name in &names {
        let Some((project, session)) = parse_tmux_session_name(name) else {
            continue; // forged / non-remora name dropped
        };
        if !config.projects.contains_key(&project) {
            continue; // R1: configured projects only
        }
        let env = read_environment(exec, &project, &session);
        live.push((project, session, env));
    }

    let mut stopped = Vec::new();
    for (project_id, project) in &config.projects {
        if project.workspace != WorkspaceMode::Worktree {
            continue; // shared projects have no surviving worktree
        }
        // A failure for one project (bad path, not a repo) yields empty for
        // that project, never a failed discovery (decision 8).
        if let Ok(out) = exec.run(&worktree_list_tokens(&project.path)) {
            if out.success {
                for (session, path) in discovery::parse_worktree_list(&out.stdout, project_id) {
                    stopped.push((project_id.clone(), session, path));
                }
            }
        }
    }

    Ok(discovery::join(live, stopped))
}

/// Whether a failed `tmux kill-session` stderr positively signals the session
/// was already absent (server gone, or session not found) — tolerating this as
/// success makes `stop` and `remove` idempotent.
pub(crate) fn stderr_signals_session_already_gone(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("can't find session")
        || lower.contains("no server running")
        || lower.contains("no sessions")
}

/// `tmux kill-session`, treating an already-absent session as success.
pub(crate) fn kill_session(exec: &dyn RemoteExec, tmux_name: &str) -> Result<(), SourceError> {
    let out = exec.run(&kill_session_tokens(tmux_name))?;
    if out.success || stderr_signals_session_already_gone(&out.stderr) {
        Ok(())
    } else {
        Err(SourceError::Transport(out.stderr))
    }
}

/// Paths derived from config needed for teardown. All fields are pure strings
/// (no network round-trip): tmux name, project dir on the host, session
/// worktree dir, optional branch (worktree-mode only).
#[derive(Debug)]
struct TeardownPaths {
    tmux_name: String,
    project_path: String,
    dir: String,
    branch: Option<String>,
    workspace: WorkspaceMode,
}

/// Resolves teardown paths from config for `(project_id, session_id)`.
/// Returns `Transport` if the project is unknown (consistent with spawn
/// behavior — unknown project is a config error, not a protocol error).
/// Does NOT consult agents (D3): teardown needs only the project path and mode.
fn teardown_paths(
    config: &Config,
    project_id: &ProjectId,
    session_id: &SessionId,
) -> Result<TeardownPaths, SourceError> {
    let project = config
        .projects
        .get(project_id)
        .ok_or_else(|| SourceError::Transport(format!("unknown project `{project_id}`")))?;
    let tmux_name = tmux_session_name(project_id, session_id);
    let (dir, branch) = if project.workspace == WorkspaceMode::Worktree {
        (
            worktree_path(project_id, session_id),
            Some(branch_name(session_id)),
        )
    } else {
        (project.path.clone(), None)
    };
    Ok(TeardownPaths {
        tmux_name,
        project_path: project.path.clone(),
        dir,
        branch,
        workspace: project.workspace,
    })
}

/// Whether a worktree has uncommitted changes and/or commits not on any remote.
/// A probe that itself fails → `Transport` (fail-safe: never delete on an
/// unreadable probe).
fn worktree_has_work(
    exec: &dyn RemoteExec,
    worktree_dir: &str,
) -> Result<Option<DirtyReason>, SourceError> {
    let status = exec.run(&status_porcelain_tokens(worktree_dir))?;
    if !status.success {
        return Err(SourceError::Transport(status.stderr));
    }
    let uncommitted = !status.stdout.trim().is_empty();

    let rev = exec.run(&not_on_remote_tokens(worktree_dir))?;
    if !rev.success {
        return Err(SourceError::Transport(rev.stderr));
    }
    // `rev-list --count` always prints a number on success. Unparseable stdout
    // means the probe result is unreadable — fail safe toward Transport, NEVER
    // toward "clean", which would let `remove` delete a possibly-dirty worktree.
    let not_on_remote = match rev.stdout.trim().parse::<u64>() {
        Ok(count) => count > 0,
        Err(_) => {
            return Err(SourceError::Transport(format!(
                "unparseable rev-list count: {:?}",
                rev.stdout.trim()
            )))
        }
    };

    Ok(match (uncommitted, not_on_remote) {
        (false, false) => None,
        (true, false) => Some(DirtyReason::Uncommitted),
        (false, true) => Some(DirtyReason::NotOnRemote),
        (true, true) => Some(DirtyReason::Both),
    })
}

/// `git worktree remove` stderr meaning the worktree is already gone — so a
/// retry after a partial removal converges instead of erroring (D4).
fn stderr_signals_worktree_absent(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("is not a working tree")
}

/// `git branch -D` stderr meaning the branch is already gone (D4).
/// Requires BOTH "branch" and "not found" so unrelated errors like
/// `remote: Repository not found` or an SSH error cannot be mistaken for
/// an already-absent branch and silently swallowed by `run_remove`.
fn stderr_signals_branch_absent(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("branch") && lower.contains("not found")
}

/// Kills the session's tmux (idempotent). Worktree survives → Stopped.
pub(crate) fn run_stop(
    exec: &dyn RemoteExec,
    config: &Config,
    project_id: &ProjectId,
    session_id: &SessionId,
) -> Result<(), SourceError> {
    let paths = teardown_paths(config, project_id, session_id)?;
    kill_session(exec, &paths.tmux_name)
}

/// Ends a session for good. Worktree mode: optional dirty gate (unless force) →
/// kill tmux → idempotent worktree remove → idempotent branch delete. Shared
/// mode: kill tmux only (never touches the project dir).
///
/// Accepted limitation: the dirty probe and the kill are separate round-trips,
/// so a still-running agent could write new uncommitted work in the window
/// between the probe reading "clean" and tmux dying — that work is then lost to
/// `worktree remove`. Killing first would close the window but would break the
/// `WorkspaceDirty` "refuses and changes nothing" contract (a refusal would
/// leave tmux dead). The window is one round-trip on an explicit, confirmed
/// teardown of a session the user has decided is done; treated as accepted,
/// same race class as the cross-client teardown-vs-respawn race (ADR-0004).
pub(crate) fn run_remove(
    exec: &dyn RemoteExec,
    config: &Config,
    project_id: &ProjectId,
    session_id: &SessionId,
    force: bool,
) -> Result<(), SourceError> {
    let paths = teardown_paths(config, project_id, session_id)?;
    let worktree = matches!(paths.workspace, WorkspaceMode::Worktree);

    if worktree && !force {
        if let Some(reason) = worktree_has_work(exec, &paths.dir)? {
            return Err(SourceError::WorkspaceDirty {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                reason,
            });
        }
    }

    kill_session(exec, &paths.tmux_name)?;

    if let Some(branch) = paths.branch.as_deref() {
        let rm = exec.run(&worktree_remove_tokens(&paths.project_path, &paths.dir))?;
        if !rm.success && !stderr_signals_worktree_absent(&rm.stderr) {
            return Err(SourceError::Transport(rm.stderr));
        }
        let del = exec.run(&branch_delete_tokens(&paths.project_path, branch))?;
        if !del.success && !stderr_signals_branch_absent(&del.stderr) {
            return Err(SourceError::Transport(del.stderr));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, WorkspaceMode};
    use crate::spawn_plan::SpawnPlan;
    use remora_protocol::{ProjectId, SessionId, SessionState};

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

    pub(crate) fn test_config() -> Arc<Config> {
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

    /// Scripted executor: returns queued `run` results in order, records every
    /// argv, and hands back a dead `SessionChannel` for `open_channel`.
    pub(crate) struct FakeExec {
        pub results:
            std::sync::Mutex<std::collections::VecDeque<Result<RemoteOutput, SourceError>>>,
        pub calls: std::sync::Mutex<Vec<Vec<String>>>,
        pub opened: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl FakeExec {
        pub fn new(results: Vec<Result<RemoteOutput, SourceError>>) -> Self {
            Self {
                results: std::sync::Mutex::new(results.into_iter().collect()),
                calls: std::sync::Mutex::new(Vec::new()),
                opened: std::sync::Mutex::new(Vec::new()),
            }
        }
        pub fn ok() -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
        pub fn out(stdout: &str) -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: stdout.into(),
                stderr: String::new(),
            }
        }
        pub fn fail(stderr: &str) -> RemoteOutput {
            RemoteOutput {
                success: false,
                stdout: String::new(),
                stderr: stderr.into(),
            }
        }

        /// Returns the first recorded argv that contains the given substring.
        /// Panics if no call contains the substring.
        pub fn recorded_argv_containing(&self, needle: &str) -> Vec<String> {
            self.calls
                .lock()
                .expect("lock")
                .iter()
                .find(|argv| argv.iter().any(|a| a.contains(needle)))
                .cloned()
                .unwrap_or_else(|| panic!("no recorded argv contains {needle:?}"))
        }

        /// Counts recorded calls whose argv contains the given substring.
        pub fn count_calls_with(&self, needle: &str) -> usize {
            self.calls
                .lock()
                .expect("lock")
                .iter()
                .filter(|argv| argv.iter().any(|a| a.contains(needle)))
                .count()
        }
    }

    impl RemoteExec for FakeExec {
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

    // -----------------------------------------------------------------------
    // exec-tail tests
    // -----------------------------------------------------------------------

    #[test]
    fn capture_reports_success_and_stdout() {
        let argv = vec!["sh".into(), "-c".into(), "printf remora-cap".into()];
        let out = capture(&argv).expect("capture runs");
        assert!(out.success);
        assert_eq!(out.stdout, "remora-cap");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn capture_reports_nonzero_exit_and_stderr() {
        let argv = vec!["sh".into(), "-c".into(), "printf boom 1>&2; exit 3".into()];
        let out = capture(&argv).expect("capture runs");
        assert!(!out.success, "non-zero exit is success=false");
        assert_eq!(out.stderr, "boom");
    }

    #[test]
    fn command_from_argv_pins_term() {
        let cmd = command_from_argv(&["ssh".to_string(), "host".to_string()]);
        assert_eq!(cmd.get_env("TERM"), Some("xterm-256color".as_ref()));
    }

    // -----------------------------------------------------------------------
    // quoting helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn shell_quote_leaves_simple_tokens_and_quotes_spaces() {
        assert_eq!(shell_quote("claude"), "claude");
        assert_eq!(shell_quote("--continue"), "--continue");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn quote_remote_path_expands_tilde_via_home() {
        assert_eq!(quote_remote_path("~/api"), "\"$HOME\"/api");
        assert_eq!(quote_remote_path("~"), "\"$HOME\"");
        assert_eq!(quote_remote_path("/home/dev/api"), "/home/dev/api");
        assert_eq!(quote_remote_path("/a b"), "'/a b'");
        assert_eq!(quote_remote_path("~/a b"), "\"$HOME\"'/a b'");
    }

    #[test]
    fn wrap_with_shell_fallback_gates_on_clean_exit_codes() {
        let wrapped = wrap_with_shell_fallback("claude --continue");
        assert!(
            wrapped.starts_with("claude --continue; __remora_rc=$?;"),
            "got: {wrapped}"
        );
        assert!(
            wrapped.contains(r#"0|130|143) exec "${SHELL:-/bin/sh}" -l ;;"#),
            "got: {wrapped}"
        );
        assert!(
            wrapped.contains(r#"*) exit "$__remora_rc" ;;"#),
            "got: {wrapped}"
        );
    }

    // -----------------------------------------------------------------------
    // token builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn attach_tokens_builds_correct_tmux_command() {
        let tokens = attach_tokens("remora_api_fix-login");
        assert_eq!(
            tokens,
            vec!["tmux", "attach-session", "-d", "-t", "remora_api_fix-login"]
        );
    }

    #[test]
    fn worktree_add_tokens_builds_git_command() {
        let plan = worktree_plan();
        let tokens = worktree_add_tokens(&plan);
        let g = tokens.iter().position(|a| a == "git").expect("git");
        assert_eq!(tokens[g + 1], "-C");
        assert_eq!(tokens[g + 2], "/home/dev/api");
        assert_eq!(tokens[g + 3], "worktree");
        assert_eq!(tokens[g + 4], "add");
        assert_eq!(tokens[g + 5], "-b");
        assert_eq!(tokens[g + 6], "remora/fix-login");
        assert_eq!(tokens[g + 7], "\"$HOME\"/.remora/worktrees/api/fix-login");
        // No ssh flags — these are pure remote tokens.
        assert!(!tokens.iter().any(|a| a == "ssh"), "no ssh prefix");
        assert!(!tokens.iter().any(|a| a == "-tt"), "non-interactive");
    }

    #[test]
    fn new_session_tokens_is_the_lock_with_no_env_trailer() {
        let plan = worktree_plan();
        let tokens = new_session_tokens(&plan);
        let n = tokens
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        assert_eq!(tokens[n + 1], "-d");
        assert_eq!(tokens[n + 2], "-s");
        assert_eq!(tokens[n + 3], "remora_api_fix-login");
        assert_eq!(tokens[n + 4], "-c");
        assert_eq!(tokens[n + 5], "\"$HOME\"/.remora/worktrees/api/fix-login");
        assert_eq!(
            tokens[n + 6],
            shell_quote(&wrap_with_shell_fallback("claude --continue"))
        );
        assert!(!tokens.iter().any(|a| a == "set-environment"));
        assert!(!tokens.iter().any(|a| a == ";"), "no bare shell separator");
    }

    #[test]
    fn new_session_tokens_applies_remain_on_exit_atomically() {
        let plan = worktree_plan();
        let tokens = new_session_tokens(&plan);
        // remain-on-exit rides the SAME invocation as new-session, after a
        // shell-quoted separator, as `set-option -t <name> remain-on-exit on`.
        let r = tokens
            .iter()
            .position(|a| a == "remain-on-exit")
            .expect("remain-on-exit");
        assert_eq!(tokens[r - 4], "';'");
        assert_eq!(tokens[r - 3], "set-option");
        assert_eq!(tokens[r - 2], "-t");
        assert_eq!(tokens[r - 1], "remora_api_fix-login");
        assert_eq!(tokens[r + 1], "on");
        let new_session = tokens
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        assert!(r > new_session, "remain-on-exit follows new-session");
    }

    #[test]
    fn new_session_tokens_enables_mouse_and_deep_scrollback() {
        // #53: the scroll wheel must drive tmux scrollback (mouse on), with a
        // history-limit deep enough for long agent output.
        let plan = worktree_plan();
        let tokens = new_session_tokens(&plan);
        let ns = tokens
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");

        // history-limit is set globally and BEFORE new-session: tmux applies it
        // only to windows created afterward, and the target session/window does
        // not exist yet.
        let h = tokens
            .iter()
            .position(|a| a == "history-limit")
            .expect("history-limit");
        assert!(h < ns, "history-limit precedes new-session");
        assert_eq!(tokens[h - 1], "-g", "global: no session to -t yet");
        assert_eq!(tokens[h + 1], "50000");

        // mouse on is a live option, applied after creation, targeted by name.
        let m = tokens.iter().position(|a| a == "mouse").expect("mouse");
        assert!(m > ns, "mouse follows new-session");
        assert_eq!(tokens[m - 4], "';'");
        assert_eq!(tokens[m - 3], "set-option");
        assert_eq!(tokens[m - 2], "-t");
        assert_eq!(tokens[m - 1], "remora_api_fix-login");
        assert_eq!(tokens[m + 1], "on");
    }

    #[test]
    fn set_environment_tokens_quotes_logical_value() {
        let tokens = set_environment_tokens(
            "remora_api_fix-login",
            "REMORA_WORKSPACE",
            "~/.remora/worktrees/api/fix-login",
        );
        let s = tokens
            .iter()
            .position(|a| a == "set-environment")
            .expect("set-env");
        assert_eq!(tokens[s + 1], "-t");
        assert_eq!(tokens[s + 2], "remora_api_fix-login");
        assert_eq!(tokens[s + 3], "REMORA_WORKSPACE");
        assert_eq!(tokens[s + 4], "'~/.remora/worktrees/api/fix-login'");
    }

    #[test]
    fn list_sessions_tokens_quotes_the_format_string() {
        let tokens = list_sessions_tokens();
        assert_eq!(tokens.last().map(String::as_str), Some("'#{session_name}'"));
        let l = tokens
            .iter()
            .position(|a| a == "list-sessions")
            .expect("list-sessions");
        assert_eq!(tokens[l + 1], "-F");
    }

    #[test]
    fn has_session_tokens_targets_the_name() {
        let tokens = has_session_tokens("remora_api_fix-login");
        let h = tokens
            .iter()
            .position(|a| a == "has-session")
            .expect("has-session");
        assert_eq!(tokens[h + 1], "-t");
        assert_eq!(tokens[h + 2], "remora_api_fix-login");
    }

    #[test]
    fn agent_command_survives_the_double_shell() {
        let plan = SpawnPlan {
            agent_argv: vec![
                "claude".into(),
                "--append-system-prompt".into(),
                "Be concise".into(),
            ],
            ..worktree_plan()
        };
        let tokens = new_session_tokens(&plan);
        let n = tokens
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        // Still exactly one agent-command token after `-c <dir>` (wrapped, not
        // per-token), followed by two 6-token trailers (remain-on-exit, then
        // mouse). new-session + 6 (down to agent-cmd) + 6 + 6.
        assert_eq!(tokens.len(), n + 1 + 6 + 6 + 6);
        let fragment = join_agent_command(&plan.agent_argv);
        let inner = wrap_with_shell_fallback(&fragment);
        assert_eq!(tokens[n + 6], shell_quote(&inner));
        assert!(
            inner.starts_with("claude --append-system-prompt 'Be concise';"),
            "got: {inner}"
        );
    }

    #[test]
    fn new_session_tokens_no_agent_runs_a_login_shell() {
        // An empty agent_argv (#35: plain shell) renders an explicit login
        // shell, NOT the agent-exit fallback wrapper, and NOT an omitted
        // command (which would delegate to the host's tmux default-command).
        let plan = SpawnPlan {
            agent_argv: vec![],
            ..worktree_plan()
        };
        let tokens = new_session_tokens(&plan);
        let n = tokens
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        // -d -s <name> -c <dir> <cmd> ; set-option -t <name> remain-on-exit on
        assert_eq!(tokens[n + 4], "-c");
        assert_eq!(tokens[n + 6], shell_quote(PLAIN_SHELL_COMMAND));
        // Same tmux argv shape as an agent spawn: command token + the
        // remain-on-exit and mouse 6-token trailers.
        assert_eq!(tokens.len(), n + 1 + 6 + 6 + 6);
        // The agent-exit wrapper must NOT appear for a no-agent pane.
        assert!(
            !tokens.iter().any(|t| t.contains("__remora_rc")),
            "no shell-fallback wrapper for a plain shell: {tokens:?}"
        );
        // remain-on-exit trailer is still present and atomic.
        let r = tokens
            .iter()
            .position(|a| a == "remain-on-exit")
            .expect("remain-on-exit");
        assert_eq!(tokens[r + 1], "on");
    }

    // -----------------------------------------------------------------------
    // classifier tests
    // -----------------------------------------------------------------------

    #[test]
    fn dup_new_session_maps_to_session_exists_case_insensitive() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("fix-login").expect("slug");
        let err = classify_new_session_failure("duplicate session: remora_api_fix-login\n", &p, &s);
        assert!(matches!(err, SourceError::SessionExists { .. }));
        let err = classify_new_session_failure("DUPLICATE SESSION", &p, &s);
        assert!(matches!(err, SourceError::SessionExists { .. }));
    }

    #[test]
    fn other_new_session_failure_is_transport() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("fix-login").expect("slug");
        let err = classify_new_session_failure("no server running on /tmp/tmux", &p, &s);
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn existing_worktree_maps_to_session_exists() {
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("fix-login").expect("slug");
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
        let p = ProjectId::new("api").expect("slug");
        let s = SessionId::new("fix-login").expect("slug");
        let err = classify_worktree_add_failure("fatal: not a git repository", &p, &s);
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn stderr_signals_session_absent_only_for_known_tmux_phrases() {
        // Positive: tmux's "no such session" phrasings (case-insensitive).
        assert!(stderr_signals_session_absent(
            "can't find session: remora_api_x"
        ));
        assert!(stderr_signals_session_absent(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(stderr_signals_session_absent("CAN'T FIND SESSION"));
        // Negative: ambiguous transport failures must NOT read as absent.
        assert!(!stderr_signals_session_absent(
            "ssh: connect to host devbox port 22: Connection refused"
        ));
        assert!(!stderr_signals_session_absent(
            "Permission denied (publickey)"
        ));
        assert!(!stderr_signals_session_absent(""));
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

    // -----------------------------------------------------------------------
    // run_spawn orchestration tests
    // -----------------------------------------------------------------------

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
        let result = run_spawn(&fake, &plan);
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
        let err = run_spawn(&fake, &plan).expect_err("worktree already exists");
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
        let err = run_spawn(&fake, &plan).expect_err("duplicate session");
        assert!(matches!(err, SourceError::SessionExists { .. }), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[test]
    fn run_spawn_opens_exactly_one_channel_on_success() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![]);
        let result = run_spawn(&fake, &plan);
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn run_spawn_attach_tokens_end_with_tmux_name() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![]);
        let _ = run_spawn(&fake, &plan);
        let opened = fake.opened.lock().expect("lock");
        let attach = &opened[0];
        assert_eq!(
            attach.last().map(String::as_str),
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
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(result.is_ok());
        let calls = fake.calls.lock().expect("lock");
        // worktree add, new-session (remain-on-exit folded in atomically), then
        // 3x set-environment = 5 blocking cmds — there is no separate set-option.
        assert_eq!(calls.len(), 5);
        assert!(calls[0].iter().any(|a| a == "worktree"));
        assert!(calls[1].iter().any(|a| a == "new-session"));
        // remain-on-exit rides on the new-session call, not a follow-up exec:
        // its `set-option` lives inside calls[1], never as a standalone call.
        assert!(calls[1].iter().any(|a| a == "set-option"));
        assert!(calls[1].iter().any(|a| a == "remain-on-exit"));
        assert!(calls[2].iter().any(|a| a == "set-environment"));
        assert!(
            !calls[2..]
                .iter()
                .any(|c| c.iter().any(|a| a == "set-option")),
            "remain-on-exit is not a standalone follow-up call"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn metadata_failure_is_tolerated_and_still_attaches() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                        // worktree add
            Ok(FakeExec::ok()), // new-session (live! remain-on-exit folded in)
            Ok(FakeExec::fail("set-env boom")), // REMORA_AGENT — tolerated
            Err(SourceError::Transport("net".into())), // REMORA_WORKSPACE — tolerated
            Ok(FakeExec::ok()), // REMORA_CREATED_AT
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(
            result.is_ok(),
            "metadata failures must not fail a live session"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn new_session_generic_failure_is_transport_and_opens_no_channel() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                      // worktree add
            Ok(FakeExec::fail("no server running")), // new-session: generic failure
            Ok(FakeExec::fail("no server running")), // has-session: confirms no session
        ]);
        let err = run_spawn(&fake, &plan).expect_err("should fail");
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
    fn live_session_worktree_is_not_removed_when_set_option_fails() {
        // The atomic new-session+set-option command shares one exit code (#28):
        // a non-zero exit can mean the session WAS created but the trailing
        // set-option failed. The session is then live, so its worktree must NOT
        // be force-removed. A has-session probe gates the orphan cleanup.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                                               // worktree add
            Ok(FakeExec::fail("set-option: unknown option: remain-on-exit")), // created, set-option failed
            Ok(FakeExec::ok()), // has-session: the session IS live
        ]);
        let err = run_spawn(&fake, &plan).expect_err("transport error");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        let calls = fake.calls.lock().expect("lock");
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "remove")),
            "a live session's worktree must never be force-removed"
        );
    }

    #[test]
    fn ambiguous_has_session_probe_failure_does_not_remove_the_worktree() {
        // An ssh/network/auth failure surfaces as a non-zero remote command
        // (`Ok(success=false)`), NOT as `Err`. A bare `!success` probe would
        // read that as "session absent" and remove a possibly-live worktree.
        // Only known tmux "absent" stderr is safe to clean up on; anything
        // ambiguous leaves the worktree.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                                               // worktree add
            Ok(FakeExec::fail("set-option: unknown option: remain-on-exit")), // created, set-option failed
            Ok(FakeExec::fail(
                "ssh: connect to host devbox port 22: Connection refused",
            )), // has-session probe itself failed
        ]);
        let err = run_spawn(&fake, &plan).expect_err("transport error");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        let calls = fake.calls.lock().expect("lock");
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "remove")),
            "an ambiguous probe failure must not trigger worktree removal"
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
        let err = run_spawn(&fake, &plan).expect_err("dup");
        assert!(matches!(err, SourceError::SessionExists { .. }));
        let calls = fake.calls.lock().expect("lock");
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "remove")),
            "must not remove a live session's worktree"
        );
    }

    // -----------------------------------------------------------------------
    // run_list orchestration tests
    // -----------------------------------------------------------------------

    #[test]
    fn list_joins_live_metadata_stopped_and_filters_unconfigured() {
        // Config has project `api` (worktree). `ghost` is NOT configured.
        let config = test_config();
        // Scripted exec, in call order:
        //  1) list-sessions -> api (configured) + ghost (unconfigured) +
        //     `main` & `remora__bad` (unparseable) — only api survives.
        //  2) show-environment for api/fix-login -> metadata
        //  3) git worktree list for api -> fix-login (live) + add-tests (stopped)
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out(
                "remora_api_fix-login\nremora_ghost_x\nmain\nremora__bad\n",
            )),
            Ok(FakeExec::out("REMORA_AGENT=claude\nREMORA_CREATED_AT=1765500000\n")),
            Ok(FakeExec::out(
                "worktree /home/dev/.remora/worktrees/api/fix-login\nbranch refs/heads/remora/fix-login\n\n\
                 worktree /home/dev/.remora/worktrees/api/add-tests\nbranch refs/heads/remora/add-tests\n",
            )),
        ]);
        let metas = run_list(&fake, &config).expect("list");

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

    #[test]
    fn list_treats_no_server_as_empty() {
        let config = test_config();
        let fake = FakeExec::new(vec![Ok(FakeExec::fail(
            "no server running on /tmp/tmux-1000/default",
        ))]);
        assert!(run_list(&fake, &config).expect("list").is_empty());
    }

    #[test]
    fn list_keeps_session_live_when_show_environment_fails() {
        // The session is listed live, but its show-environment read flakes
        // (race: it could die between list-sessions and the metadata read).
        // The session must stay Live with empty metadata, not be downgraded.
        let config = test_config();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // list-sessions
            Ok(FakeExec::fail("connection reset")),      // show-environment flakes
            Ok(FakeExec::out("")),                       // worktree list: empty
        ]);
        let metas = run_list(&fake, &config).expect("list");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].agent, None);
        assert_eq!(metas[0].created_at, None);
    }

    #[test]
    fn list_survives_worktree_list_failure_per_decision_8() {
        // A failed `git worktree list` for one project yields empty for that
        // project, never a failed discovery (decision 8): the live session
        // still lists, just with no Stopped twin.
        let config = test_config();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // list-sessions
            Ok(FakeExec::out("REMORA_AGENT=claude\n")),  // show-environment
            Ok(FakeExec::fail("fatal: not a git repository")), // worktree list fails
        ]);
        let metas = run_list(&fake, &config).expect("list must not fail");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id.as_str(), "fix-login");
        assert_eq!(metas[0].state, SessionState::Live);
        assert!(metas.iter().all(|m| m.state == SessionState::Live));
    }

    // -----------------------------------------------------------------------
    // teardown token-builder + orchestration tests (ported from #50 ssh.rs)
    // -----------------------------------------------------------------------

    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("slug")
    }

    fn sid(s: &str) -> SessionId {
        SessionId::new(s).expect("slug")
    }

    #[test]
    fn kill_session_tokens_targets_the_session() {
        let tokens = kill_session_tokens("remora_api_fix-login");
        let k = tokens
            .iter()
            .position(|a| a == "kill-session")
            .expect("kill-session");
        assert_eq!(tokens[k - 1], "tmux");
        assert_eq!(tokens[k + 1], "-t");
        assert_eq!(tokens[k + 2], "remora_api_fix-login");
    }

    #[test]
    fn dirty_probe_tokens_run_in_the_worktree() {
        let s = status_porcelain_tokens("~/.remora/worktrees/api/x");
        assert_eq!(
            s[s.iter().position(|a| a == "-C").expect("-C") + 1],
            "\"$HOME\"/.remora/worktrees/api/x"
        );
        assert!(s.iter().any(|a| a == "status") && s.iter().any(|a| a == "--porcelain"));
        let r = not_on_remote_tokens("~/.remora/worktrees/api/x");
        assert!(r.iter().any(|a| a == "rev-list") && r.iter().any(|a| a == "--count"));
        assert!(
            r.iter().any(|a| a == "HEAD")
                && r.iter().any(|a| a == "--not")
                && r.iter().any(|a| a == "--remotes")
        );
    }

    #[test]
    fn branch_delete_tokens_force_deletes_in_the_project() {
        let tokens = branch_delete_tokens("/home/dev/api", "remora/fix-login");
        let g = tokens.iter().position(|a| a == "git").expect("git");
        assert_eq!(tokens[g + 1], "-C");
        assert_eq!(tokens[g + 2], "/home/dev/api");
        assert_eq!(tokens[g + 3], "branch");
        assert_eq!(tokens[g + 4], "-D");
        assert_eq!(tokens[g + 5], "remora/fix-login");
    }

    #[test]
    fn teardown_paths_worktree_resolves_without_agent() {
        let config = test_config();
        let p = teardown_paths(&config, &pid("api"), &sid("fix-login")).expect("paths");
        assert_eq!(p.tmux_name, "remora_api_fix-login");
        assert_eq!(p.project_path, "/home/dev/api");
        assert_eq!(p.dir, "~/.remora/worktrees/api/fix-login");
        assert_eq!(p.branch.as_deref(), Some("remora/fix-login"));
        assert_eq!(p.workspace, WorkspaceMode::Worktree);
    }

    #[test]
    fn teardown_paths_shared_has_no_branch() {
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
        let p = teardown_paths(&config, &pid("scratch"), &sid("s1")).expect("paths");
        assert_eq!(p.dir, "~/scratch");
        assert_eq!(p.branch, None);
        assert_eq!(p.workspace, WorkspaceMode::Shared);
    }

    #[test]
    fn teardown_paths_unknown_project_is_transport() {
        let config = Arc::new(Config::default());
        let err = teardown_paths(&config, &pid("ghost"), &sid("x")).expect_err("unknown");
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn kill_session_tolerates_absent_session() {
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("can't find session: remora_api_x"))]);
        assert!(kill_session(&fake, "remora_api_x").is_ok());
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("no server running on /tmp/tmux"))]);
        assert!(kill_session(&fake, "remora_api_x").is_ok());
    }

    #[test]
    fn kill_session_propagates_transport_failure() {
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("Permission denied (publickey)"))]);
        let err = kill_session(&fake, "remora_api_x").expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn worktree_has_work_classifies_each_signal() {
        let dir = "~/.remora/worktrees/api/x";
        // clean tree + 0 not-on-remote → None
        let fake = FakeExec::new(vec![Ok(FakeExec::out("")), Ok(FakeExec::out("0\n"))]);
        assert_eq!(worktree_has_work(&fake, dir).expect("ok"), None);
        // dirty tree + 0 → Uncommitted
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out(" M src/x.rs\n")),
            Ok(FakeExec::out("0\n")),
        ]);
        assert_eq!(
            worktree_has_work(&fake, dir).expect("ok"),
            Some(DirtyReason::Uncommitted)
        );
        // clean + 3 not-on-remote → NotOnRemote
        let fake = FakeExec::new(vec![Ok(FakeExec::out("")), Ok(FakeExec::out("3\n"))]);
        assert_eq!(
            worktree_has_work(&fake, dir).expect("ok"),
            Some(DirtyReason::NotOnRemote)
        );
        // dirty + 3 → Both
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("?? new\n")),
            Ok(FakeExec::out("3\n")),
        ]);
        assert_eq!(
            worktree_has_work(&fake, dir).expect("ok"),
            Some(DirtyReason::Both)
        );
    }

    #[test]
    fn worktree_has_work_fails_safe_on_ambiguous_probe() {
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("fatal: not a git repository"))]);
        let err = worktree_has_work(&fake, "~/x").expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn absent_classifiers_match_git_phrasings() {
        assert!(stderr_signals_worktree_absent(
            "fatal: '~/x' is not a working tree"
        ));
        assert!(!stderr_signals_worktree_absent(
            "fatal: could not lock config file"
        ));
        assert!(stderr_signals_branch_absent(
            "error: branch 'remora/x' not found."
        ));
        assert!(!stderr_signals_branch_absent(
            "error: Cannot delete branch checked out at '~/x'"
        ));
        // Negative: bare "not found" without "branch" must NOT match.
        assert!(!stderr_signals_branch_absent(
            "remote: Repository not found"
        ));
        assert!(!stderr_signals_branch_absent("fatal: 'origin' not found"));
    }

    #[test]
    fn worktree_has_work_fails_safe_when_revlist_probe_fails() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("")),
            Ok(FakeExec::fail("fatal: bad object HEAD")),
        ]);
        let err = worktree_has_work(&fake, "~/x").expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn worktree_has_work_fails_safe_on_unparseable_count() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("")),          // status: clean
            Ok(FakeExec::out("garbage\n")), // rev-list: success but non-numeric
        ]);
        let err = worktree_has_work(&fake, "~/x").expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
    }

    #[test]
    fn run_stop_kills_only_the_session() {
        let fake = FakeExec::new(vec![Ok(FakeExec::ok())]);
        assert!(run_stop(&fake, &test_config(), &pid("api"), &sid("fix-login")).is_ok());
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        assert_eq!(fake.count_calls_with("worktree"), 0);
    }

    #[test]
    fn run_remove_clean_worktree_runs_probe_kill_remove_delete_in_order() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("")),    // status --porcelain (clean)
            Ok(FakeExec::out("0\n")), // rev-list (on remote)
            Ok(FakeExec::ok()),       // kill-session
            Ok(FakeExec::ok()),       // worktree remove
            Ok(FakeExec::ok()),       // branch -D
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok());
        let calls = fake.calls.lock().expect("lock");
        assert!(calls[0].iter().any(|a| a == "status"));
        assert!(calls[1].iter().any(|a| a == "rev-list"));
        assert!(calls[2].iter().any(|a| a == "kill-session"));
        assert!(calls[3].iter().any(|a| a == "remove")); // worktree remove
        assert!(calls[4].iter().any(|a| a == "-D")); // branch -D
    }

    #[test]
    fn run_remove_refuses_dirty_without_force_and_touches_nothing() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out(" M src/x.rs\n")), // status dirty
            Ok(FakeExec::out("0\n")),           // rev-list
        ]);
        let err = run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false)
            .expect_err("dirty");
        assert!(matches!(
            err,
            SourceError::WorkspaceDirty {
                reason: DirtyReason::Uncommitted,
                ..
            }
        ));
        assert_eq!(fake.count_calls_with("kill-session"), 0);
        assert_eq!(fake.count_calls_with("remove"), 0);
    }

    #[test]
    fn run_remove_force_skips_the_probe() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()), // kill-session
            Ok(FakeExec::ok()), // worktree remove
            Ok(FakeExec::ok()), // branch -D
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), true).is_ok());
        assert_eq!(fake.count_calls_with("status"), 0);
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        assert_eq!(fake.count_calls_with("remove"), 1);
        assert_eq!(fake.count_calls_with("-D"), 1);
    }

    #[test]
    fn run_remove_idempotent_when_worktree_and_branch_already_gone() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("")),    // status --porcelain (clean)
            Ok(FakeExec::out("0\n")), // rev-list
            Ok(FakeExec::ok()),       // kill-session
            Ok(FakeExec::fail(
                "fatal: '~/.remora/worktrees/api/fix-login' is not a working tree",
            )), // already gone
            Ok(FakeExec::fail(
                "error: branch 'remora/fix-login' not found.",
            )), // already gone
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok());
    }

    #[test]
    fn run_remove_shared_mode_skips_worktree_and_branch() {
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
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()), // kill-session only
        ]);
        assert!(run_remove(&fake, &config, &pid("scratch"), &sid("s1"), false).is_ok());
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        assert_eq!(fake.count_calls_with("worktree"), 0);
        assert_eq!(fake.count_calls_with("-D"), 0);
    }
}
