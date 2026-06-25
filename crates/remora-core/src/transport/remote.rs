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
use crate::config::Config;
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

/// Tokens for `git -C <project> worktree add -b <branch> <worktree> [<start>]`.
/// `start_point` (a resolved base ref, #54) is appended last and shell-quoted
/// when present; `None` reproduces the legacy local-HEAD behavior.
pub(crate) fn worktree_add_tokens(plan: &SpawnPlan, start_point: Option<&str>) -> Vec<String> {
    let branch = plan.branch.as_deref().unwrap_or_default();
    let mut tokens = vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(&plan.project_path),
        "worktree".into(),
        "add".into(),
        "-b".into(),
        shell_quote(branch),
        quote_remote_path(&plan.dir),
    ];
    if let Some(sp) = start_point {
        tokens.push(shell_quote(sp));
    }
    tokens
}

/// Tokens for `git -C <project> fetch origin` — refreshes origin's
/// remote-tracking refs before a new worktree is based off them (#54). Always
/// origin: deriving a remote from a base ref's text is ambiguous, and
/// multi-remote is out of scope.
pub(crate) fn fetch_tokens(project_path: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "fetch".into(),
        "origin".into(),
    ]
}

/// Tokens for `git -C <project> symbolic-ref --short refs/remotes/origin/HEAD`
/// — prints origin's default branch (e.g. `origin/main`).
pub(crate) fn remote_head_tokens(project_path: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "symbolic-ref".into(),
        "--short".into(),
        "refs/remotes/origin/HEAD".into(),
    ]
}

/// Tokens for `git -C <project> rev-parse --verify --quiet <ref>^{commit}` —
/// confirms `git_ref` resolves to a commit. The exact (fully-qualified) ref
/// plus the `^{commit}` peel defeats DWIM resolution against a tag like
/// `refs/tags/origin/main` and rejects a dangling symbolic ref. The ref is
/// shell-quoted because `^{}` are shell-special.
pub(crate) fn verify_commit_tokens(project_path: &str, git_ref: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "rev-parse".into(),
        "--verify".into(),
        "--quiet".into(),
        shell_quote(&format!("{git_ref}^{{commit}}")),
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
/// `allow-passthrough` is intentionally NOT chained here: it is absent on
/// tmux < 3.3 and would cause the whole invocation to fail on those versions,
/// orphaning the just-created session. It is applied after `create_session`
/// succeeds via [`set_passthrough_tokens`] and tolerated on failure.
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

/// Tokens for `tmux set-option -t <name> allow-passthrough on`. Applied
/// AFTER `create_session` succeeds — best-effort, absent on tmux < 3.3,
/// degrades to quiescence-only activity detection, must never fail the spawn.
/// Called by `run_spawn` and `run_respawn` with its result tolerated (ignored).
pub(crate) fn set_passthrough_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "set-option".into(),
        "-t".into(),
        tmux_name.into(),
        "allow-passthrough".into(),
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

/// Resolves the git start-point for a new worktree (#54): fetch origin
/// (best-effort), then the cascade — explicit `plan.base` → detected
/// `origin/HEAD` (verified) → `origin/main` → `origin/master` → `None` (omit
/// the start-point, branch off local `HEAD`). Only the fetch swallows errors;
/// a detection exec `Err` (transport down) propagates, while a non-zero exit
/// (ref absent) falls through.
pub(crate) fn resolve_base(
    exec: &dyn RemoteExec,
    plan: &SpawnPlan,
) -> Result<Option<String>, SourceError> {
    // 1. Fetch — best-effort: swallow both Err and a non-zero exit.
    let _ = exec.run(&fetch_tokens(&plan.project_path));

    // 2. Explicit per-session/per-project base wins; no detection round-trips.
    if let Some(base) = &plan.base {
        return Ok(Some(base.clone()));
    }

    // 3. origin/HEAD, verified to resolve to a commit (guards stale/dangling).
    let head = exec.run(&remote_head_tokens(&plan.project_path))?;
    if head.success {
        let candidate = head.stdout.trim();
        if !candidate.is_empty()
            && ref_resolves(
                exec,
                &plan.project_path,
                &format!("refs/remotes/{candidate}"),
            )?
        {
            return Ok(Some(candidate.to_string()));
        }
    }

    // 4. Probe origin/main then origin/master (exact refspec, exit-only fall-through).
    for short in ["origin/main", "origin/master"] {
        if ref_resolves(exec, &plan.project_path, &format!("refs/remotes/{short}"))? {
            return Ok(Some(short.to_string()));
        }
    }

    // 5. Nothing resolved — omit the start-point (legacy local-HEAD base).
    Ok(None)
}

/// Whether `git_ref^{commit}` resolves. Exec `Err` (transport) propagates; a
/// non-zero exit (ref absent / dangling) is `false`.
fn ref_resolves(
    exec: &dyn RemoteExec,
    project_path: &str,
    git_ref: &str,
) -> Result<bool, SourceError> {
    Ok(exec
        .run(&verify_commit_tokens(project_path, git_ref))?
        .success)
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
        let start_point = resolve_base(exec, plan)?;
        let out = exec.run(&worktree_add_tokens(plan, start_point.as_deref()))?;
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

    // Best-effort: allow-passthrough is absent on tmux < 3.3 and degrades to
    // quiescence-only activity detection. Must never fail the spawn.
    let _ = exec.run(&set_passthrough_tokens(&plan.tmux_name));
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
            // Best-effort: allow-passthrough is absent on tmux < 3.3 and
            // degrades to quiescence-only activity detection. Must never fail.
            let _ = exec.run(&set_passthrough_tokens(&plan.tmux_name));
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
/// the worktree scan runs for every configured project (a worktree-override
/// session can live on a shared-default project).
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

    let mut worktrees = Vec::new();
    let mut scanned = std::collections::HashSet::new();
    for (project_id, project) in &config.projects {
        // Scan EVERY project: a worktree-override session can live on a
        // shared-default project. `parse_worktree_list` rejects the main
        // checkout and foreign paths, so a project with no remora worktrees
        // yields nothing. Record which projects scanned cleanly so `join` can
        // tell "scanned, no worktree" (⇒ Shared) apart from "scan failed"
        // (⇒ unknown), instead of conflating a transient failure with Shared.
        if let Ok(out) = exec.run(&worktree_list_tokens(&project.path)) {
            if out.success {
                scanned.insert(project_id.clone());
                for (session, path) in discovery::parse_worktree_list(&out.stdout, project_id) {
                    worktrees.push((project_id.clone(), session, path));
                }
            }
        }
    }

    Ok(discovery::join(live, worktrees, &scanned))
}

// ---------------------------------------------------------------------------
// Local command resolution (opt-in crossing of ADR-0004 for `{ command }` fields)
// ---------------------------------------------------------------------------

use std::io::Read;
use std::time::{Duration, Instant};

/// Wall-clock and output bounds for a local resolution command. A hung
/// selector (e.g. `kubectl get` against an unreachable API) must NOT block the
/// discovery poll forever, and runaway output must not exhaust memory.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const RESOLVE_MAX_OUTPUT: usize = 64 * 1024;

/// Seam for running a user-authored command line LOCALLY. Behind a trait so the
/// timeout / output-cap / exit-status paths are testable deterministically and
/// transport tests can resolve through a real (or scripted) runner without the
/// kubectl exec tail.
pub(crate) trait LocalRunner: Send + Sync {
    fn run_local(&self, command: &str) -> Result<RemoteOutput, SourceError>;
}

/// The real runner: `sh -c <command>` with a timeout and an output cap.
pub(crate) struct ShellRunner {
    timeout: Duration,
    max_output: usize,
}

impl ShellRunner {
    pub(crate) fn new() -> Self {
        Self {
            timeout: RESOLVE_TIMEOUT,
            max_output: RESOLVE_MAX_OUTPUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(timeout: Duration, max_output: usize) -> Self {
        Self {
            timeout,
            max_output,
        }
    }
}

impl LocalRunner for ShellRunner {
    fn run_local(&self, command: &str) -> Result<RemoteOutput, SourceError> {
        run_shell_bounded(command, self.timeout, self.max_output)
    }
}

/// Reads to EOF, retaining at most `cap` bytes but always draining the pipe so
/// the child can't deadlock on a full buffer after we stop retaining. Returns
/// (lossy string, whether the cap was exceeded).
fn read_capped(reader: &mut impl Read, cap: usize) -> (String, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut over = false;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                // `over` means MORE than `cap` bytes were actually produced:
                // exactly `cap` bytes total must never trip it (off-by-one).
                if kept.len() + n > cap {
                    over = true;
                }
                if kept.len() < cap {
                    let take = (cap - kept.len()).min(n);
                    kept.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), over)
}

/// SIGKILLs the `child`'s entire process group. On Unix the child is the leader
/// of its own group (pgid == child pid), so this also kills pipeline members
/// (`kubectl`, `head`) and backgrounded descendants that outlived `sh` while
/// holding the stdout/stderr pipe open — closing the pipe write-ends so the
/// reader threads hit EOF and their joins return instead of blocking forever.
/// `ESRCH` (group already empty) is the expected no-op, so the Result is
/// ignored. Does NOT reap `sh` itself; the caller `wait`s for that.
#[cfg(unix)]
fn reap_group(child: &std::process::Child) {
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Non-Unix fallback: no process groups, so kill only the child. (The whole
/// function assumes `sh`, so Unix is the real target; this just keeps a
/// non-unix `cargo check` compiling.)
#[cfg(not(unix))]
fn reap_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// Spawns `sh -c command` with bounded time and output. Reader threads prevent
/// pipe-buffer deadlock; a poll loop enforces the timeout. On both timeout AND
/// normal exit the child's whole process group is SIGKILLed (on Unix) before
/// the reader threads are joined: pipeline members and backgrounded
/// descendants can outlive `sh` and keep the stdout pipe open, which would
/// otherwise block the reader-thread joins indefinitely (leaking threads/FDs/
/// processes on every discovery poll). Reaping the group closes those
/// write-ends so the joins always return.
fn run_shell_bounded(
    command: &str,
    timeout: Duration,
    max_output: usize,
) -> Result<RemoteOutput, SourceError> {
    use std::process::{Command, Stdio};

    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Put the child in its own process group (pgid == child pid) so `reap_group`
    // can SIGKILL the whole pipeline, not just `sh`.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    let mut child = builder
        .spawn()
        .map_err(|e| SourceError::Transport(format!("resolve: spawn sh: {e}")))?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let out_handle = std::thread::spawn(move || read_capped(&mut stdout_pipe, max_output));
    let err_handle = std::thread::spawn(move || read_capped(&mut stderr_pipe, max_output));

    let deadline = Instant::now() + timeout;
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break Ok(None); // timed out
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => break Err(e),
        }
    };

    // ALWAYS kill the group, then `wait` for `sh`, BEFORE joining the readers:
    // on timeout this kills the hung pipeline; on normal exit it reaps any
    // backgrounded descendant still holding the pipe (harmless if the group is
    // already empty). Either way the pipe write-ends close → readers hit EOF →
    // joins return promptly.
    #[cfg(unix)]
    reap_group(&child);
    #[cfg(not(unix))]
    reap_group(&mut child);
    let _ = child.wait();

    let (stdout, stdout_over) = out_handle.join().unwrap_or_else(|_| (String::new(), false));
    let (stderr, _) = err_handle.join().unwrap_or_else(|_| (String::new(), false));

    let status = match outcome {
        Ok(Some(status)) => status,
        Ok(None) => {
            return Err(SourceError::Transport(format!(
                "resolution command timed out after {}s",
                timeout.as_secs()
            )));
        }
        Err(e) => return Err(SourceError::Transport(format!("resolve: wait: {e}"))),
    };

    if stdout_over {
        return Err(SourceError::Transport(format!(
            "resolution command produced more than {max_output} bytes"
        )));
    }
    Ok(RemoteOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

/// Resolves a user-authored command line LOCALLY and returns its trimmed
/// stdout. The deliberate, opt-in crossing of ADR-0004's never-shell-evaluate
/// line. Nonzero exit, empty output, or output that fails the literal-field
/// guard (control chars, embedded newline, leading `-`, edge whitespace) is a
/// hard error — never silently target "".
pub(crate) fn resolve_local_command(
    runner: &dyn LocalRunner,
    field: &str,
    command: &str,
) -> Result<String, SourceError> {
    let out = runner.run_local(command)?;
    if !out.success {
        return Err(SourceError::Transport(format!(
            "kubectl `{field}` resolution command failed: {}",
            out.stderr.trim()
        )));
    }
    let value = out.stdout.trim();
    if value.is_empty() {
        return Err(SourceError::Transport(format!(
            "kubectl `{field}` resolution command produced no output"
        )));
    }
    if let Some(reason) = crate::config::literal_field_problem(value) {
        return Err(SourceError::Transport(format!(
            "kubectl `{field}` resolved value {reason}"
        )));
    }
    Ok(value.to_owned())
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
/// (no network round-trip): tmux name and project dir on the host. Worktree
/// path and branch are now computed on-demand in `run_remove` (from ids, not
/// config) to correctly handle worktree-override sessions on shared-default
/// projects.
#[derive(Debug)]
struct TeardownPaths {
    tmux_name: String,
    project_path: String,
}

/// Resolves teardown paths from config for `(project_id, session_id)`.
/// Returns `Transport` if the project is unknown (consistent with spawn
/// behavior — unknown project is a config error, not a protocol error).
/// Does NOT consult agents (D3): teardown needs only the project path.
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
    Ok(TeardownPaths {
        tmux_name,
        project_path: project.path.clone(),
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

/// Ends a session for good. Mode is determined from REAL remote state (not
/// project config): a worktree-override session on a shared-default project
/// must still be cleaned up. If the canonical worktree directory exists →
/// worktree session: optional dirty gate (unless force) → kill tmux →
/// idempotent worktree remove → idempotent branch delete. If not → kill tmux
/// only (shared session, or worktree already gone).
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
    // Determine mode from REAL remote state, not project config: a per-session
    // override means the project's default doesn't decide whether this session
    // has a worktree. The canonical path is recomputed from validated ids
    // (never from discovered metadata — ADR-0004).
    let worktree_dir = worktree_path(project_id, session_id);
    let probe = exec.run(&dir_exists_tokens(&worktree_dir))?;
    // `test -d` is silent: a clean non-zero exit (empty stderr) means the dir is
    // absent (shared session, or worktree already gone). A non-empty stderr
    // means the probe itself couldn't run (ssh/kubectl/auth/shell error) — fail
    // closed rather than mistaking a transport error for "no worktree" and
    // orphaning a live worktree + branch (mirrors `run_respawn`).
    if !probe.success && !probe.stderr.trim().is_empty() {
        return Err(SourceError::Transport(probe.stderr));
    }
    let has_worktree = probe.success;

    if has_worktree && !force {
        if let Some(reason) = worktree_has_work(exec, &worktree_dir)? {
            return Err(SourceError::WorkspaceDirty {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                reason,
            });
        }
    }

    kill_session(exec, &paths.tmux_name)?;

    if has_worktree {
        let rm = exec.run(&worktree_remove_tokens(&paths.project_path, &worktree_dir))?;
        if !rm.success && !stderr_signals_worktree_absent(&rm.stderr) {
            return Err(SourceError::Transport(rm.stderr));
        }
        let branch = branch_name(session_id);
        let del = exec.run(&branch_delete_tokens(&paths.project_path, &branch))?;
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
            base: None,
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
        let tokens = worktree_add_tokens(&plan, None);
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
    fn new_session_tokens_does_not_contain_allow_passthrough() {
        // allow-passthrough is intentionally NOT in the atomic new-session chain:
        // it is absent on tmux < 3.3 and would cause the whole invocation to fail
        // on those versions, orphaning the just-created session. It is applied
        // separately (best-effort) via set_passthrough_tokens after create_session
        // succeeds.
        let plan = worktree_plan();
        let tokens = new_session_tokens(&plan);
        assert!(
            !tokens.iter().any(|a| a == "allow-passthrough"),
            "allow-passthrough must not appear in the atomic new-session chain: {tokens:?}"
        );
    }

    #[test]
    fn set_passthrough_tokens_shape() {
        // 6 tokens: tmux set-option -t <name> allow-passthrough on
        let tokens = set_passthrough_tokens("remora_api_fix-login");
        assert_eq!(tokens.len(), 6);
        assert_eq!(tokens[0], "tmux");
        assert_eq!(tokens[1], "set-option");
        assert_eq!(tokens[2], "-t");
        assert_eq!(tokens[3], "remora_api_fix-login");
        assert_eq!(tokens[4], "allow-passthrough");
        assert_eq!(tokens[5], "on");
    }

    #[test]
    fn failing_passthrough_set_does_not_fail_spawn() {
        // allow-passthrough is tolerated: a non-zero exit (tmux < 3.3 "unknown
        // option") must never fail an already-live session.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify
            Ok(FakeExec::ok()),                 // worktree add
            Ok(FakeExec::ok()),                 // new-session (success)
            Ok(FakeExec::fail("unknown option: allow-passthrough")), // passthrough — FAILS
                                                // remaining set-environment calls succeed by default
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(
            result.is_ok(),
            "a failing allow-passthrough must not fail spawn: {result:?}"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
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
        // per-token), followed by two 6-token trailers (remain-on-exit and mouse).
        // new-session + 6 (down to agent-cmd) + 6 + 6.
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
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                                   // fetch
            Ok(FakeExec::out("origin/main\n")),                   // symbolic-ref
            Ok(FakeExec::ok()),                                   // verify
            Ok(FakeExec::fail("fatal: '<path>' already exists")), // worktree add FAILS
        ]);
        let err = run_spawn(&fake, &plan).expect_err("worktree already exists");
        assert!(matches!(err, SourceError::SessionExists { .. }), "{err}");
        // 4 calls: fetch + symbolic-ref + verify + worktree-add, no channel opened.
        assert_eq!(fake.calls.lock().expect("lock").len(), 4);
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[test]
    fn duplicate_session_does_not_open_a_channel() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify
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
    fn worktree_spawn_runs_add_create_passthrough_metadata_then_attaches_in_order() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify
            Ok(FakeExec::ok()),                 // worktree add
            Ok(FakeExec::ok()),                 // new-session
            Ok(FakeExec::ok()),                 // set-option allow-passthrough (best-effort)
            Ok(FakeExec::ok()),                 // set-environment REMORA_AGENT
            Ok(FakeExec::ok()),                 // set-environment REMORA_WORKSPACE
            Ok(FakeExec::ok()),                 // set-environment REMORA_CREATED_AT
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(result.is_ok());
        let calls = fake.calls.lock().expect("lock");
        // fetch + symbolic-ref + verify + worktree add + new-session
        // (remain-on-exit folded in atomically) + allow-passthrough (best-effort)
        // + 3x set-environment = 9 blocking cmds.
        assert_eq!(calls.len(), 9);
        assert!(calls[3].iter().any(|a| a == "worktree"));
        assert!(calls[4].iter().any(|a| a == "new-session"));
        // remain-on-exit rides on the new-session call, not a follow-up exec:
        // its `set-option` lives inside calls[4], never as a standalone call.
        assert!(calls[4].iter().any(|a| a == "set-option"));
        assert!(calls[4].iter().any(|a| a == "remain-on-exit"));
        // calls[5] is the best-effort allow-passthrough set-option.
        assert!(calls[5].iter().any(|a| a == "allow-passthrough"));
        // calls[6..] are the set-environment metadata writes.
        assert!(calls[6].iter().any(|a| a == "set-environment"));
        assert!(
            !calls[6..]
                .iter()
                .any(|c| c.iter().any(|a| a == "set-option")),
            "remain-on-exit is not a standalone follow-up call after metadata"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn metadata_failure_is_tolerated_and_still_attaches() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                                      // fetch
            Ok(FakeExec::out("origin/main\n")),                      // symbolic-ref
            Ok(FakeExec::ok()),                                      // verify
            Ok(FakeExec::ok()),                                      // worktree add
            Ok(FakeExec::ok()), // new-session (live! remain-on-exit folded in)
            Ok(FakeExec::fail("unknown option: allow-passthrough")), // passthrough — tolerated
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
            Ok(FakeExec::ok()),                      // fetch
            Ok(FakeExec::out("origin/main\n")),      // symbolic-ref
            Ok(FakeExec::ok()),                      // verify
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
            Ok(FakeExec::ok()),                                               // fetch
            Ok(FakeExec::out("origin/main\n")),                               // symbolic-ref
            Ok(FakeExec::ok()),                                               // verify
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
            Ok(FakeExec::ok()),                                               // fetch
            Ok(FakeExec::out("origin/main\n")),                               // symbolic-ref
            Ok(FakeExec::ok()),                                               // verify
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
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify
            Ok(FakeExec::ok()),                 // worktree add
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

    // -----------------------------------------------------------------------
    // resolve_base tests (#54)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_base_uses_explicit_plan_base_without_detection() {
        let plan = SpawnPlan {
            base: Some("origin/dev".into()),
            ..worktree_plan()
        };
        let fake = FakeExec::new(vec![Ok(FakeExec::ok())]); // only the fetch
        let got = resolve_base(&fake, &plan).expect("ok");
        assert_eq!(got.as_deref(), Some("origin/dev"));
        // fetch happened, no symbolic-ref/rev-parse probes.
        assert_eq!(fake.count_calls_with("fetch"), 1);
        assert_eq!(fake.count_calls_with("symbolic-ref"), 0);
    }

    #[test]
    fn resolve_base_detects_origin_head_when_verified() {
        let plan = worktree_plan(); // base: None
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify refs/remotes/origin/main^{commit}
        ]);
        assert_eq!(
            resolve_base(&fake, &plan).expect("ok").as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn resolve_base_falls_through_dangling_origin_head_to_main() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/gone\n")), // symbolic-ref succeeds
            Ok(FakeExec::fail("")),             // verify origin/gone -> dangling
            Ok(FakeExec::ok()),                 // verify refs/remotes/origin/main -> ok
        ]);
        assert_eq!(
            resolve_base(&fake, &plan).expect("ok").as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn resolve_base_probes_master_then_omits() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),     // fetch
            Ok(FakeExec::fail("")), // symbolic-ref: origin/HEAD unset (non-zero exit)
            Ok(FakeExec::fail("")), // verify origin/main -> absent
            Ok(FakeExec::fail("")), // verify origin/master -> absent
        ]);
        assert_eq!(resolve_base(&fake, &plan).expect("ok"), None);
    }

    #[test]
    fn resolve_base_propagates_detection_transport_error() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                             // fetch
            Err(SourceError::Transport("ssh down".into())), // symbolic-ref Err
        ]);
        assert!(matches!(
            resolve_base(&fake, &plan),
            Err(SourceError::Transport(_))
        ));
    }

    #[test]
    fn resolve_base_swallows_fetch_failure() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Err(SourceError::Transport("offline".into())), // fetch Err -> swallowed
            Ok(FakeExec::out("origin/main\n")),            // symbolic-ref
            Ok(FakeExec::ok()),                            // verify
        ]);
        assert_eq!(
            resolve_base(&fake, &plan).expect("ok").as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn resolve_base_swallows_fetch_nonzero_exit() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::fail("error: could not fetch origin")), // fetch Ok(non-zero) -> swallowed
            Ok(FakeExec::out("origin/main\n")),                  // symbolic-ref
            Ok(FakeExec::ok()), // verify refs/remotes/origin/main^{commit}
        ]);
        assert_eq!(
            resolve_base(&fake, &plan).expect("ok").as_deref(),
            Some("origin/main")
        );
    }

    #[test]
    fn resolve_base_probes_master_when_main_absent() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),     // fetch
            Ok(FakeExec::fail("")), // symbolic-ref: origin/HEAD unset (non-zero exit)
            Ok(FakeExec::fail("")), // verify refs/remotes/origin/main -> absent
            Ok(FakeExec::ok()),     // verify refs/remotes/origin/master -> resolves
        ]);
        assert_eq!(
            resolve_base(&fake, &plan).expect("ok").as_deref(),
            Some("origin/master")
        );
    }

    #[test]
    fn worktree_add_appends_quoted_start_point_last() {
        let plan = worktree_plan();
        let with = worktree_add_tokens(&plan, Some("origin/main"));
        assert_eq!(
            with.last().map(String::as_str),
            Some(shell_quote("origin/main").as_str())
        );
        let without = worktree_add_tokens(&plan, None);
        assert_eq!(
            without.last().map(String::as_str),
            Some("\"$HOME\"/.remora/worktrees/api/fix-login")
        );
    }

    #[test]
    fn run_spawn_fetches_before_worktree_add() {
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // fetch
            Ok(FakeExec::out("origin/main\n")), // symbolic-ref
            Ok(FakeExec::ok()),                 // verify
            Ok(FakeExec::ok()),                 // worktree add
            Ok(FakeExec::ok()),                 // new-session
        ]);
        let _ = run_spawn(&fake, &plan);
        let calls = fake.calls.lock().expect("lock");
        let fetch_i = calls
            .iter()
            .position(|a| a.iter().any(|t| t == "fetch"))
            .expect("fetch");
        let add_i = calls
            .iter()
            .position(|a| a.iter().any(|t| t == "worktree"))
            .expect("add");
        assert!(fetch_i < add_i, "fetch must precede worktree add");
    }

    #[test]
    fn fetch_tokens_targets_origin() {
        assert_eq!(
            fetch_tokens("/home/dev/api"),
            vec!["git", "-C", "/home/dev/api", "fetch", "origin"]
        );
    }

    #[test]
    fn remote_head_tokens_reads_origin_head() {
        let t = remote_head_tokens("/home/dev/api");
        assert_eq!(t[3], "symbolic-ref");
        assert_eq!(t[4], "--short");
        assert_eq!(t[5], "refs/remotes/origin/HEAD");
    }

    #[test]
    fn verify_commit_tokens_peels_to_commit_with_exact_ref() {
        let t = verify_commit_tokens("/home/dev/api", "refs/remotes/origin/main");
        assert_eq!(t[3], "rev-parse");
        assert_eq!(t[4], "--verify");
        assert_eq!(t[5], "--quiet");
        // exact ref + ^{commit} peel defeats DWIM tag collisions / dangling refs.
        assert_eq!(t[6], shell_quote("refs/remotes/origin/main^{commit}"));
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

    #[test]
    fn list_discovers_worktree_session_on_shared_default_project() {
        // Before the fix, the worktree scan was guarded by
        // `project.workspace == WorkspaceMode::Worktree`, so a worktree-override
        // session on a shared-default project was silently lost. This test is the
        // RED proof: on the old code the assertion fails (session not discovered);
        // on the new code it passes.
        //
        // Config: `api` (worktree-default) + `scratch` (shared-default).
        // No live sessions. The `scratch` project has one surviving worktree at
        // ~/.remora/worktrees/scratch/s1.
        //
        // FakeExec call order (config is a BTreeMap, sorted: api first, scratch second):
        //   1) list-sessions        -> empty (no live sessions)
        //   2) git worktree list for `api`     -> empty
        //   3) git worktree list for `scratch` -> s1 worktree entry
        let toml = r#"
            [hosts.devbox]
            transport = "ssh"
            host = "devbox"
            [projects.api]
            host = "devbox"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"
            [projects.scratch]
            host = "devbox"
            path = "/home/dev/scratch"
            workspace = "shared"
            agent = "claude"
            [agents.claude]
            command = ["claude"]
        "#;
        let config = Arc::new(Config::from_toml_str(toml).expect("config"));
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("")), // list-sessions: no live sessions
            Ok(FakeExec::out("")), // worktree list for api: empty
            Ok(FakeExec::out(
                "worktree /home/dev/.remora/worktrees/scratch/s1\nbranch refs/heads/remora/s1\n",
            )), // worktree list for scratch: one worktree session
        ]);
        let metas = run_list(&fake, &config).expect("list");
        // The scratch/s1 worktree session must be discovered as Stopped + Worktree.
        assert_eq!(
            metas.len(),
            1,
            "expected one discovered session, got: {metas:?}"
        );
        assert_eq!(metas[0].project_id.as_str(), "scratch");
        assert_eq!(metas[0].session_id.as_str(), "s1");
        assert_eq!(metas[0].state, SessionState::Stopped);
        assert_eq!(metas[0].workspace, Some(WorkspaceMode::Worktree));
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
    }

    #[test]
    fn teardown_paths_shared_resolves_project_path() {
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
        assert_eq!(p.project_path, "~/scratch");
        assert_eq!(p.tmux_name, "remora_scratch_s1");
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
            Ok(FakeExec::ok()),       // test -d worktree probe: exists
            Ok(FakeExec::out("")),    // status --porcelain (clean)
            Ok(FakeExec::out("0\n")), // rev-list (on remote)
            Ok(FakeExec::ok()),       // kill-session
            Ok(FakeExec::ok()),       // worktree remove
            Ok(FakeExec::ok()),       // branch -D
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok());
        let calls = fake.calls.lock().expect("lock");
        assert!(calls[0].iter().any(|a| a == "test") && calls[0].iter().any(|a| a == "-d"));
        assert!(calls[1].iter().any(|a| a == "status"));
        assert!(calls[2].iter().any(|a| a == "rev-list"));
        assert!(calls[3].iter().any(|a| a == "kill-session"));
        assert!(calls[4].iter().any(|a| a == "remove")); // worktree remove
        assert!(calls[5].iter().any(|a| a == "-D")); // branch -D
    }

    #[test]
    fn run_remove_refuses_dirty_without_force_and_touches_nothing() {
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                 // test -d worktree probe: exists
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
    fn run_remove_force_skips_the_dirty_probe_but_still_probes_existence() {
        // force=true skips the dirty-check (status/rev-list) but the `test -d`
        // existence probe still runs — it decides whether to do worktree cleanup.
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d worktree probe: exists
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
            Ok(FakeExec::ok()),       // test -d worktree probe: exists
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
    fn run_remove_shared_session_skips_worktree_and_branch() {
        // A genuinely shared session (no worktree dir on disk): `test -d` fails,
        // so only kill-session runs — no worktree remove, no branch delete.
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
            Ok(FakeExec::fail("")), // test -d: no worktree dir
            Ok(FakeExec::ok()),     // kill-session
        ]);
        assert!(run_remove(&fake, &config, &pid("scratch"), &sid("s1"), false).is_ok());
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        // Use "worktree remove" (the git subcommand pair) rather than the path
        // substring "worktree", which also appears in the `test -d` probe argv.
        assert_eq!(fake.count_calls_with("remove"), 0);
        assert_eq!(fake.count_calls_with("-D"), 0);
    }

    // -----------------------------------------------------------------------
    // LocalRunner / ShellRunner / resolve_local_command tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_local_command_trims_and_returns_stdout() {
        let runner = ShellRunner::new();
        let pod = resolve_local_command(&runner, "pod", "echo sandbox-7").expect("ok");
        assert_eq!(pod, "sandbox-7");
    }

    #[test]
    fn resolve_local_command_rejects_multiline_output() {
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "printf 'a\\nb\\n'")
            .expect_err("embedded newline is a control char");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
    }

    #[test]
    fn resolve_local_command_rejects_empty_output() {
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "true").expect_err("no output");
        assert!(format!("{err}").contains("no output"), "{err}");
    }

    #[test]
    fn resolve_local_command_nonzero_exit_is_transport_error() {
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "echo boom >&2; exit 1")
            .expect_err("nonzero exit");
        assert!(format!("{err}").contains("boom"), "{err}");
    }

    #[test]
    fn resolve_local_command_rejects_leading_dash() {
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "echo -- -bad | tr -d ' '")
            .expect_err("leading dash would be a flag");
        // Simpler, deterministic form:
        let err2 =
            resolve_local_command(&runner, "pod", "printf -- '-bad'").expect_err("leading dash");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert!(matches!(err2, SourceError::Transport(_)), "{err2}");
    }

    #[test]
    fn shell_runner_times_out_and_kills() {
        // Tiny timeout so the test is fast; sleep would otherwise hang.
        // 500ms (not 50ms) stays well clear of process-spawn jitter on a loaded
        // CI runner while remaining 10x under `sleep 5`, so the kill-on-timeout
        // path fires deterministically without flaking.
        let runner = ShellRunner::with_limits(std::time::Duration::from_millis(500), 64 * 1024);
        let err = resolve_local_command(&runner, "pod", "sleep 5").expect_err("must time out");
        assert!(format!("{err}").contains("timed out"), "{err}");
    }

    #[test]
    fn shell_runner_caps_output() {
        // 200 KB of output against a 1 KB cap. `yes | head -c` terminates.
        let runner = ShellRunner::with_limits(std::time::Duration::from_secs(5), 1024);
        let err = resolve_local_command(&runner, "pod", "yes | head -c 200000")
            .expect_err("must exceed the cap");
        assert!(format!("{err}").contains("bytes"), "{err}");
    }

    #[test]
    fn run_shell_bounded_does_not_hang_on_backgrounded_pipe_holder() {
        // `sleep 30 &` inherits the stdout pipe and outlives sh. Without the
        // process-group kill, the reader-thread join blocks ~30s (leak). With it,
        // the group is reaped and the call returns promptly with the real output.
        let runner = ShellRunner::with_limits(std::time::Duration::from_secs(5), 64 * 1024);
        let out = resolve_local_command(&runner, "pod", "sleep 30 & printf sandbox-1")
            .expect("returns promptly with output, not a 30s hang");
        assert_eq!(out, "sandbox-1");
    }

    #[test]
    fn read_capped_exactly_at_cap_is_not_flagged() {
        let data = vec![b'x'; 8];
        let (s, over) = read_capped(&mut data.as_slice(), 8);
        assert_eq!(s.len(), 8);
        assert!(!over, "exactly cap must not be flagged as over");
    }

    #[test]
    fn read_capped_over_cap_is_flagged_and_truncates() {
        let data = vec![b'x'; 9];
        let (s, over) = read_capped(&mut data.as_slice(), 8);
        assert_eq!(s.len(), 8);
        assert!(over);
    }

    #[test]
    fn run_remove_fails_closed_when_the_worktree_probe_errors() {
        // The `test -d` probe fails with a non-empty stderr (ssh/auth/shell
        // error, not a clean "dir absent"). run_remove must NOT mistake that for
        // "no worktree" and proceed to kill tmux while skipping cleanup — that
        // would orphan a live worktree + branch. It fails closed with Transport.
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
            Ok(FakeExec::fail("ssh: connect: connection refused")), // test -d: probe error
        ]);
        let err = run_remove(&fake, &config, &pid("scratch"), &sid("s1"), true).expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
        // Failed closed: tmux was never killed, nothing cleaned up.
        assert_eq!(fake.count_calls_with("kill-session"), 0);
        assert_eq!(fake.count_calls_with("remove"), 0);
    }

    #[test]
    fn run_remove_worktree_session_on_shared_config_project_cleans_up_worktree() {
        // RED → GREEN: a worktree session spawned on a shared-default project
        // must still be cleaned up. The probe (`test -d`) finds the worktree dir
        // exists, so run_remove issues worktree remove + branch -D regardless of
        // the project config's workspace setting.
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
            Ok(FakeExec::ok()), // test -d: worktree dir EXISTS (override case)
            // force=true: no dirty-check
            Ok(FakeExec::ok()), // kill-session
            Ok(FakeExec::ok()), // worktree remove
            Ok(FakeExec::ok()), // branch -D
        ]);
        assert!(
            run_remove(&fake, &config, &pid("scratch"), &sid("s1"), true).is_ok(),
            "worktree session on shared-config project must be cleaned up"
        );
        assert_eq!(
            fake.count_calls_with("kill-session"),
            1,
            "kill-session must run"
        );
        assert_eq!(
            fake.count_calls_with("remove"),
            1,
            "worktree remove must run for an existing worktree dir"
        );
        assert_eq!(
            fake.count_calls_with("-D"),
            1,
            "branch delete must run for an existing worktree"
        );
    }
}
