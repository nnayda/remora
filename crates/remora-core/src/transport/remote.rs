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
use crate::naming::{
    derive_session_id, parse_tmux_session_name, tmux_session_name, ENV_AGENT, ENV_CREATED_AT,
    ENV_PREFIX, ENV_WORKSPACE,
};
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
/// session branch after the worktree is removed. The branch is shell-quoted
/// for the same reason every other value is: the ssh/kubectl layers join
/// tokens into a command string that the remote shell re-parses. Git permits
/// shell metacharacters in ref names (`;`, `$`, backtick, `&`, `|`, …), so
/// an unquoted branch from `git worktree list` is a remote code execution
/// vector. `shell_quote` is the same escaping `worktree_add_tokens` uses.
pub(crate) fn branch_delete_tokens(project_path: &str, branch: &str) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(project_path),
        "branch".into(),
        "-D".into(),
        shell_quote(branch),
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
/// expansion — the stored value must round-trip via the inline `#{E:VAR}`
/// read in `list_sessions_tokens`, #108).
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

/// Tokens for `tmux list-sessions -F '#{session_name}'` — the TRUSTED live set.
/// The format carries no env expansion, so a forged `#{E:}` value (which could
/// contain a newline and fabricate a phantom row, #108) cannot pollute it; this
/// is the same names-only listing the pre-#108 code trusted. Shell-quoted: a
/// bare `#` starts a comment in the remote login shell.
pub(crate) fn list_session_names_tokens() -> Vec<String> {
    vec![
        "tmux".into(),
        "list-sessions".into(),
        "-F".into(),
        shell_quote("#{session_name}"),
    ]
}

/// Tokens for `tmux list-sessions -F '<name>\t<agent>\t<workspace>\t<created_at>'`,
/// carrying each session's `REMORA_*` metadata inline via tmux ≥ 3.0's `#{E:VAR}`
/// env-expansion. This is the metadata ENRICHMENT call — one round-trip for the
/// whole live set, replacing the old `1 + N` show-environment fan-out (the bulk
/// of discovery latency on a high-RTT link, #108). It is paired with
/// [`list_session_names_tokens`]: `run_list` keys this metadata by a name already
/// proven live there, so an injected phantom row (forged `#{E:}` newline) is
/// dropped rather than fabricating a session. Fields are joined by
/// `discovery::SESSION_FIELD_SEP` (a tab, which `clean_metadata` guarantees no
/// sanitized value contains) and parsed back by `discovery::parse_session_line`;
/// the field order here IS that parser's contract. On tmux < 3.0 the `#{E:}`
/// fields expand empty — the session still lists Live (from the names call),
/// just without metadata (a graceful version cliff). The whole format is
/// shell-quoted: a bare `#` starts a comment in the remote login shell.
pub(crate) fn list_sessions_tokens() -> Vec<String> {
    let format = [
        "#{session_name}".to_string(),
        format!("#{{E:{ENV_AGENT}}}"),
        format!("#{{E:{ENV_WORKSPACE}}}"),
        format!("#{{E:{ENV_CREATED_AT}}}"),
    ]
    .join(discovery::SESSION_FIELD_SEP);
    vec![
        "tmux".into(),
        "list-sessions".into(),
        "-F".into(),
        shell_quote(&format),
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

/// Tokens for `printf %s $HOME` — the remote home directory, used to
/// canonicalize `~/…` worktree paths for the discovery join (A1, #124).
/// Cheap; one per poll. Both the SSH transport (which runs tokens through
/// the remote login shell via `ssh host cmd args…`) and the kubectl transport
/// (which joins tokens and runs them as `sh -c "…"`) expand `$HOME`; the bare
/// token is correct for both.
pub(crate) fn remote_home_tokens() -> Vec<String> {
    vec!["printf".into(), "%s".into(), "$HOME".into()]
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

/// Tokens for `tmux has-session -t <name>` — the liveness probe used by the
/// spawn cleanup gate (`run_spawn`). Attach uses `show_environment_tokens`
/// instead: one round-trip that is both a liveness preflight and the identity
/// fingerprint (#105).
pub(crate) fn has_session_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "has-session".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

/// Tokens for `tmux show-environment -t <name>` — the attach preflight. One
/// round-trip does double duty: a non-zero exit is the liveness check (a
/// missing/stopped session prints a known tmux "no such session" stderr), and
/// the printed session environment carries the `REMORA_*` fingerprint that
/// proves the session is one Remora spawned (#105). Same round-trip count as
/// the old `has-session` preflight, so the fingerprint is free on the hot path.
pub(crate) fn show_environment_tokens(tmux_name: &str) -> Vec<String> {
    vec![
        "tmux".into(),
        "show-environment".into(),
        "-t".into(),
        tmux_name.into(),
    ]
}

/// Whether `tmux show-environment` output proves the session is one Remora
/// spawned: it carries at least one *set* `REMORA_*` variable (#105). tmux
/// prints each session variable as `KEY=value` and a variable marked for
/// removal as `-KEY`, so the leading-`-` form is not a match. A session that
/// only reuses the `remora_<project>_<session>` name — a tmux server restart
/// with foreign recreation, or a manually reused name — carries none; the name
/// is a hint, not proof (ADR-0004). Tolerant of a partial metadata write
/// (`write_metadata` is best-effort): any single `REMORA_*` variable suffices.
pub(crate) fn has_remora_fingerprint(show_environment_stdout: &str) -> bool {
    show_environment_stdout.lines().any(|line| {
        line.strip_prefix(ENV_PREFIX)
            .and_then(|rest| rest.split_once('='))
            .is_some_and(|(name, _value)| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
    })
}

// ---------------------------------------------------------------------------
// Error classifiers
// ---------------------------------------------------------------------------

/// Classifies `tmux list-sessions`: success → the stdout rows (each a
/// `name<SEP>agent<SEP>workspace<SEP>created_at` line, parsed by
/// `discovery::parse_session_line`); a no-server / no-sessions stderr → empty
/// (the normal cold state, decision 9, matched case-insensitively); any other
/// failure → `Transport`.
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

/// Whether a failed `has-session`/`new-session`/`show-environment` stderr
/// positively means the session does not exist (server up but name unknown, or
/// no server at all), as opposed to an ambiguous transport failure
/// (ssh/auth/network) that also exits non-zero. Matched case-insensitively to
/// survive a non-English `LC_MESSAGES`. Mirrors the cold-state phrases in
/// `classify_list_sessions`, plus the two distinct "name unknown" phrasings
/// tmux uses for the same condition: `has-session` says `can't find session`
/// while `show-environment` (the attach preflight, #105) says `no such session`
/// — both mean "server up, this session is gone", which is genuinely absent for
/// every caller (attach → `SessionNotFound`; the spawn cleanup gate → safe to
/// reclaim the orphaned worktree).
pub(crate) fn stderr_signals_session_absent(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("can't find session")
        || lower.contains("no such session")
        || lower.contains("no server running")
        || lower.contains("no sessions")
}

/// Attach-only "session absent" classifier: the shared
/// [`stderr_signals_session_absent`] plus the fully-torn-down-server case.
///
/// When the tmux server itself is gone (socket missing), tmux prints
/// `error connecting to <sock> (No such file or directory)` rather than a
/// per-session "not found" — verified on tmux 3.4 for both `has-session` and
/// `show-environment`. No server means no session, so for *attach* that is
/// genuinely absent → `SessionNotFound`. The phrase is tmux-specific: ssh prints
/// `connect to host …` and kubectl `unable to connect to the server`, so a real
/// connection failure still surfaces as `Transport`, not a misleading "not
/// found". This is deliberately NOT folded into the shared classifier: the spawn
/// cleanup gate must stay conservative, since a connection error there could
/// mean the session was created just before the link dropped, and reclaiming its
/// worktree would yank a live session's cwd (#105 review).
fn attach_stderr_signals_absent(stderr: &str) -> bool {
    stderr_signals_session_absent(stderr)
        || stderr.to_ascii_lowercase().contains("error connecting to")
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
/// failure must never fail an already-live session (values are untrusted /
/// display-only — ADR-0004). Returns whether *all* writes succeeded so the
/// caller can confirm-and-re-stamp on failure: the *presence* of this env is
/// load-bearing for attach's identity fingerprint (#105), even though the values
/// stay untrusted. `remain-on-exit` is no longer written here — it is applied
/// atomically inside `new_session_tokens` so a pane whose process exits at startup
/// (a bad start dir, a missing agent binary) is retained before it can
/// self-destruct the session, instead of racing these env-var round-trips and
/// turning the follow-up attach into a spurious `SessionNotFound` (#28).
pub(crate) fn write_metadata(exec: &dyn RemoteExec, plan: &SpawnPlan) -> bool {
    let mut all_ok = true;
    for (key, value) in &plan.env {
        let ok = exec
            .run(&set_environment_tokens(&plan.tmux_name, key, value))
            .map(|out| out.success)
            .unwrap_or(false);
        all_ok &= ok;
    }
    all_ok
}

/// Best-effort check that the session carries the `REMORA_*` fingerprint (#105).
/// Any transport error or non-zero exit reads as "not confirmed" so the caller
/// re-stamps rather than leaving a live-but-unreconnectable session.
fn fingerprint_present(exec: &dyn RemoteExec, tmux_name: &str) -> bool {
    exec.run(&show_environment_tokens(tmux_name))
        .map(|out| out.success && has_remora_fingerprint(&out.stdout))
        .unwrap_or(false)
}

/// The shared spawn/respawn tail: enable passthrough, write the `REMORA_*`
/// metadata, ensure the fingerprint actually landed, then open the channel.
///
/// allow-passthrough is best-effort (absent on tmux < 3.3, degrades to
/// quiescence-only activity detection). The metadata write is best-effort too,
/// but its `REMORA_*` env is load-bearing: attach/reconnect fingerprints the
/// session by it (#105). So when a write reports failure, confirm at least one
/// var stuck and re-stamp once on a total miss — otherwise a transient blip at
/// spawn would leave a live but permanently unreconnectable session. The verify
/// round-trip is paid only on a write failure; the happy path costs nothing
/// extra. Still never fails an already-live session on a metadata hiccup: a
/// fingerprint that refuses to stick degrades to pre-#105 behavior, not a failed
/// spawn.
fn stamp_and_attach(
    exec: &dyn RemoteExec,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    let _ = exec.run(&set_passthrough_tokens(&plan.tmux_name));
    if !write_metadata(exec, plan) && !fingerprint_present(exec, &plan.tmux_name) {
        let _ = write_metadata(exec, plan);
    }
    attach_channel(exec, &plan.tmux_name)
}

/// Opens the PTY attach channel to an existing session (no liveness
/// preflight — callers that need one do it first; see `run_attach`).
pub(crate) fn attach_channel(
    exec: &dyn RemoteExec,
    tmux_name: &str,
) -> Result<SessionChannel, SourceError> {
    exec.open_channel(&attach_tokens(tmux_name))
}

/// Opens a channel to a live session after proving it is one Remora spawned —
/// the attach orchestration shared by every transport.
///
/// A single `tmux show-environment` round-trip is both the liveness preflight
/// AND the identity fingerprint (#105). A missing or stopped session exits
/// non-zero with a known tmux absent-stderr (`no such session`, or
/// `error connecting …` when the whole server is gone — see
/// [`attach_stderr_signals_absent`]) → `SessionNotFound` (honoring the trait
/// contract); an ssh/kubectl/network failure also exits non-zero, so only those
/// known phrasings map to `SessionNotFound` while anything ambiguous surfaces as
/// `Transport` rather than a misleading "not found". A
/// session that is live but carries no `REMORA_*` env is a same-named impostor
/// (a tmux server restart with foreign recreation, or a manually reused name):
/// the name is a hint, not proof (ADR-0004), so attach refuses it as
/// `SessionNotFound` rather than piping the client's input into an unknown
/// process. The TOCTOU window (the session dies between preflight and attach)
/// degrades to channel death. A dead-pane session still exists and attaches.
pub(crate) fn run_attach(
    exec: &dyn RemoteExec,
    project_id: &ProjectId,
    session_id: &SessionId,
) -> Result<SessionChannel, SourceError> {
    let tmux_name = tmux_session_name(project_id, session_id);
    let out = exec.run(&show_environment_tokens(&tmux_name))?;
    if !out.success {
        return if attach_stderr_signals_absent(&out.stderr) {
            Err(SourceError::SessionNotFound {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            })
        } else {
            Err(SourceError::Transport(out.stderr))
        };
    }
    if !has_remora_fingerprint(&out.stdout) {
        // Live and name-matched, but unfingerprinted: not a session we spawned.
        // Treat as unknown rather than attach (#105).
        return Err(SourceError::SessionNotFound {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        });
    }
    attach_channel(exec, &tmux_name)
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

    stamp_and_attach(exec, plan)
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
///
/// Resolves the real worktree dir/branch from `git worktree list` (ADR-0015,
/// #124): sessions spawned with a custom `worktree_root` or `branch` live at
/// a non-convention path, so the plan's convention dir/branch would be wrong.
/// Falls back to the convention plan when the session isn't found in the
/// listing (shared session, or worktree already gone — the `test -d` preflight
/// maps the latter to `SessionNotFound`).
pub(crate) fn run_respawn(
    exec: &dyn RemoteExec,
    plan: &SpawnPlan,
) -> Result<SessionChannel, SourceError> {
    if plan.branch.is_none() {
        return Err(PlanError::NotWorktreeProject(plan.project_id.clone()).into());
    }

    // Best-effort home fetch for path canonicalization (same policy as
    // run_remove): empty/non-absolute result falls back to "~".
    let home = exec
        .run(&remote_home_tokens())
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout.trim().to_string())
        .filter(|h| h.starts_with('/'))
        .unwrap_or_else(|| "~".to_string());

    // Build the effective plan: real dir/branch if found, convention otherwise.
    let overridden;
    let effective = match resolve_worktree(exec, &plan.project_path, &plan.session_id, &home)? {
        Some((real_dir, real_branch)) => {
            // Also update REMORA_WORKSPACE so post-respawn discovery reports
            // the correct path (metadata is display-only — ADR-0004 — but
            // showing the convention path for a custom-root session is wrong).
            let env = plan
                .env
                .iter()
                .map(|(k, v)| {
                    if k == ENV_WORKSPACE {
                        (k.clone(), real_dir.clone())
                    } else {
                        (k.clone(), v.clone())
                    }
                })
                .collect();
            overridden = SpawnPlan {
                dir: real_dir,
                branch: Some(real_branch),
                env,
                ..plan.clone()
            };
            &overridden
        }
        None => plan,
    };

    let probe = exec.run(&dir_exists_tokens(&effective.dir))?;
    if !probe.success {
        // `test -d` is silent; empty stderr means the dir is gone (nothing to
        // respawn), non-empty means the probe itself couldn't run.
        return if probe.stderr.trim().is_empty() {
            Err(SourceError::SessionNotFound {
                project_id: effective.project_id.clone(),
                session_id: effective.session_id.clone(),
            })
        } else {
            Err(SourceError::Transport(probe.stderr))
        };
    }
    match create_session(exec, effective) {
        Ok(()) => stamp_and_attach(exec, effective),
        // Concurrent respawner already created it: attach to the live session.
        Err(SourceError::SessionExists { .. }) => attach_channel(exec, &effective.tmux_name),
        Err(err) => Err(err),
    }
}

/// Reads the inline `#{E:}` session metadata into a `name → env` map
/// (best-effort; a failed/`no-server` read yields an empty map). This is
/// ENRICHMENT only: `run_list` keys it by a name already proven live by the
/// trusted [`list_session_names_tokens`] listing, so a forged env value whose
/// embedded newline fabricates an extra row here cannot introduce a session —
/// the phantom name simply isn't in the trusted set. A forged row that reuses a
/// real session's name can at worst set that session's own (already untrusted,
/// display-only — ADR-0004) metadata, which its real env could do anyway. Last
/// row wins on a duplicate name.
fn read_inline_metadata(exec: &dyn RemoteExec) -> std::collections::HashMap<String, DiscoveredEnv> {
    let mut map = std::collections::HashMap::new();
    if let Ok(out) = exec.run(&list_sessions_tokens()) {
        if out.success {
            for row in out.stdout.lines() {
                let (name, env) = discovery::parse_session_line(row);
                if !name.is_empty() {
                    map.insert(name.to_string(), env);
                }
            }
        }
    }
    map
}

/// Discovers sessions on the host and joins them to local config. Config-
/// scoped throughout (R1): the live set keeps only configured projects, and
/// the worktree scan runs for every configured project (a worktree-override
/// session can live on a shared-default project).
///
/// Two listings, both one round-trip each (cheap over the shared ControlMaster,
/// #63), replace the old `1 + N` per-session show-environment fan-out (#108):
/// the TRUSTED names-only [`list_session_names_tokens`] decides the live set
/// (env-free, so unforgeable), and [`read_inline_metadata`] enriches each by
/// name. A live session whose metadata is missing (tmux < 3.0, or a metadata
/// read flake) still lists, just with empty metadata.
pub(crate) fn run_list(
    exec: &dyn RemoteExec,
    config: &Config,
) -> Result<Vec<SessionMeta>, SourceError> {
    let names = classify_list_sessions(&exec.run(&list_session_names_tokens())?)?;

    // Skip the metadata round-trip entirely when nothing is live (matches the
    // pre-#108 "no names ⇒ no per-session reads" behavior).
    let metadata = if names.is_empty() {
        std::collections::HashMap::new()
    } else {
        read_inline_metadata(exec)
    };

    let mut live = Vec::new();
    for name in &names {
        let Some((project, session)) = parse_tmux_session_name(name) else {
            continue; // forged / non-remora name dropped
        };
        if !config.projects.contains_key(&project) {
            continue; // R1: configured projects only
        }
        let env = metadata.get(name.as_str()).cloned().unwrap_or_default();
        live.push((project, session, env));
    }

    // Remote $HOME for path canonicalization (A1, #124). Best-effort: on
    // failure (exec error, non-zero exit, or a non-absolute result), fall back
    // to "~", which makes only `~/…` logical paths fail to canonicalize —
    // they won't match any worktree, which is acceptable degradation and never
    // a panic. Validates `starts_with('/')` per ADR-0004's never-trust rule.
    let home = exec
        .run(&remote_home_tokens())
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout.trim().to_string())
        .filter(|h| h.starts_with('/'))
        .unwrap_or_else(|| "~".to_string());

    let mut worktrees: Vec<(ProjectId, discovery::WorktreeInfo)> = Vec::new();
    let mut project_paths: std::collections::HashMap<ProjectId, String> =
        std::collections::HashMap::new();
    let mut scanned = std::collections::HashSet::new();
    for (project_id, project) in &config.projects {
        // Scan EVERY project: a worktree-override session can live on a
        // shared-default project. Record which projects scanned cleanly so
        // `join` can tell "scanned, no worktree" (⇒ Shared) apart from "scan
        // failed" (⇒ unknown), instead of conflating a transient failure with
        // Shared.
        if let Ok(out) = exec.run(&worktree_list_tokens(&project.path)) {
            if out.success {
                scanned.insert(project_id.clone());
                project_paths.insert(
                    project_id.clone(),
                    discovery::canonicalize_remote_path(&project.path, &home),
                );
                for wt in discovery::parse_worktree_porcelain(&out.stdout) {
                    worktrees.push((project_id.clone(), wt));
                }
            }
        }
    }

    Ok(discovery::join(
        live,
        worktrees,
        &project_paths,
        &home,
        &scanned,
    ))
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
            "kubectl `{field}` resolution command failed (`{command}`): {}",
            out.stderr.trim()
        )));
    }
    let value = out.stdout.trim();
    if value.is_empty() {
        return Err(SourceError::Transport(format!(
            "kubectl `{field}` resolution command produced no output"
        )));
    }
    // A selector matching N pods resolves to N whitespace-separated tokens —
    // one-per-line from `-o name`, or space-separated from a jsonpath list. No
    // valid kubectl field (a DNS label) contains interior whitespace, so >1
    // token is unambiguously a multi-match. Catch it before the generic
    // literal-field guard, which would otherwise reject a newline as an opaque
    // "control character" (or, worse, accept a space-joined token verbatim) —
    // surface the ambiguity instead (ADR-0009 single-active-pod, #115).
    let matches: Vec<&str> = value.split_whitespace().collect();
    if matches.len() > 1 {
        return Err(SourceError::Transport(ambiguous_selector_detail(
            field, &matches,
        )));
    }
    if let Some(reason) = crate::config::literal_field_problem(value) {
        return Err(SourceError::Transport(format!(
            "kubectl `{field}` resolved value {reason}"
        )));
    }
    Ok(value.to_owned())
}

/// Detail for a `{ command }` selector that matched more than one value where
/// exactly one is required (ADR-0009 single-active-pod). Lists up to `SAMPLE`
/// values (each clipped to `MAX_NAME`) and summarises the rest; kept quote-free
/// and length-bounded so the count and guidance survive `SourceError::Transport`'s
/// escaping/256-char truncation pass even with pathologically long match tokens.
fn ambiguous_selector_detail(field: &str, matches: &[&str]) -> String {
    const SAMPLE: usize = 3;
    const MAX_NAME: usize = 40;
    let shown: Vec<String> = matches
        .iter()
        .take(SAMPLE)
        .map(|m| match m.char_indices().nth(MAX_NAME) {
            Some((cut, _)) => format!("{}...", &m[..cut]),
            None => (*m).to_owned(),
        })
        .collect();
    let mut sample = shown.join(", ");
    if matches.len() > SAMPLE {
        sample.push_str(&format!(" (+{} more)", matches.len() - SAMPLE));
    }
    format!(
        "kubectl `{field}` selector matched {} values ({sample}), expected exactly 1; \
         tighten the selector or pipe through `head -n1` to pick one",
        matches.len()
    )
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

/// Resolves the real worktree path and branch for `session_id` by querying
/// `git worktree list --porcelain` for the project. Returns the first entry
/// whose branch derives (via [`derive_session_id`]) to `session_id`, as
/// `(canonical_path, branch)`.
///
/// Returns `Ok(None)` when no matching entry is found (shared or already-gone
/// session). Returns `Err(Transport)` if the listing fails with non-empty
/// stderr — fail-safe: an ambiguous probe must never read as "no worktree",
/// which would silently orphan a live worktree + branch (ADR-0015, #124).
fn resolve_worktree(
    exec: &dyn RemoteExec,
    project_path: &str,
    session_id: &SessionId,
    home: &str,
) -> Result<Option<(String, String)>, SourceError> {
    let out = exec.run(&worktree_list_tokens(project_path))?;
    if !out.success {
        // Non-empty stderr → transport/auth/git error; fail closed.
        // Empty stderr → no worktrees or empty output; treat as not found.
        if !out.stderr.trim().is_empty() {
            return Err(SourceError::Transport(out.stderr));
        }
        return Ok(None);
    }
    for wt in discovery::parse_worktree_porcelain(&out.stdout) {
        if let Some(branch) = wt.branch {
            if derive_session_id(Some(&branch)) == Some(session_id.clone()) {
                let canonical = discovery::canonicalize_remote_path(&wt.path, home);
                return Ok(Some((canonical, branch)));
            }
        }
    }
    Ok(None)
}

/// Ends a session for good. Mode is determined from REAL remote git state (not
/// project config or naming convention): `git worktree list --porcelain` finds
/// the worktree whose branch derives to `session_id`. This is the correct
/// approach for sessions with a custom `worktree_root` or `branch` — the
/// convention path would be wrong and would silently orphan the real worktree.
///
/// Outcomes:
/// - `Some((real_dir, real_branch))` and `real_dir == project.path` → **A2′**:
///   the matched worktree IS the primary checkout; kill tmux only, never
///   `worktree remove` or `branch -D` it.
/// - `Some((real_dir, real_branch))` (non-primary) → dirty gate (unless
///   `force`) → kill tmux → idempotent `worktree remove real_dir` → idempotent
///   `branch -D real_branch`.
/// - `None` (no matching worktree in the listing) → shared or already-gone
///   session; kill tmux only.
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

    // Remote $HOME for path canonicalization. Best-effort: on failure (exec
    // error, non-zero exit, or a non-absolute result), fall back to "~" —
    // same degradation policy as `run_list`. Validates `starts_with('/')` per
    // ADR-0004's never-trust rule. Fetched once; passed into `resolve_worktree`
    // and reused for the A2' primary-path comparison below.
    let home = exec
        .run(&remote_home_tokens())
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout.trim().to_string())
        .filter(|h| h.starts_with('/'))
        .unwrap_or_else(|| "~".to_string());

    // Determine mode from git's authoritative listing (ADR-0015, #124): find
    // the worktree whose branch derives to this session_id. A convention-path
    // recompute (the old approach) breaks when the session was spawned with a
    // custom worktree_root or branch.
    let resolved = resolve_worktree(exec, &paths.project_path, session_id, &home)?;

    match resolved {
        None => {
            // No discoverable worktree: shared session or worktree already gone.
            // Kill tmux only (idempotent — absent session is tolerated).
            kill_session(exec, &paths.tmux_name)
        }
        Some((real_dir, real_branch)) => {
            // A2' (ADR-0015): the worktree at `project.path` is the PRIMARY
            // checkout. Never `worktree remove` or `branch -D` it — killing
            // tmux is all teardown should do here.
            let primary_path = discovery::canonicalize_remote_path(&paths.project_path, &home);
            if real_dir == primary_path {
                return kill_session(exec, &paths.tmux_name);
            }

            // Non-primary worktree: dirty gate (unless force), kill, remove, delete.
            if !force {
                if let Some(reason) = worktree_has_work(exec, &real_dir)? {
                    return Err(SourceError::WorkspaceDirty {
                        project_id: project_id.clone(),
                        session_id: session_id.clone(),
                        reason,
                    });
                }
            }
            kill_session(exec, &paths.tmux_name)?;
            let rm = exec.run(&worktree_remove_tokens(&paths.project_path, &real_dir))?;
            if !rm.success && !stderr_signals_worktree_absent(&rm.stderr) {
                return Err(SourceError::Transport(rm.stderr));
            }
            let del = exec.run(&branch_delete_tokens(&paths.project_path, &real_branch))?;
            if !del.success && !stderr_signals_branch_absent(&del.stderr) {
                return Err(SourceError::Transport(del.stderr));
            }
            Ok(())
        }
    }
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
    fn remote_home_tokens_contains_home_for_shell_expansion() {
        // The exec layer (SSH: joins tokens and passes to remote login shell;
        // kubectl: joins under `sh -c`) expands `$HOME`. The token must be the
        // bare unquoted form so the remote shell resolves it to the actual home
        // directory; a double-quoted or escaped form would also work but the bare
        // form is idiomatic for shell-expanded invocations.
        let tokens = remote_home_tokens();
        assert_eq!(tokens[0], "printf", "must invoke printf");
        assert!(
            tokens.iter().any(|t| t.contains("$HOME")),
            "tokens must reference $HOME for shell expansion: {tokens:?}"
        );
    }

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
    fn list_sessions_tokens_quotes_the_inline_metadata_format() {
        let tokens = list_sessions_tokens();
        let format = tokens.last().expect("format arg");
        // The whole format is shell-quoted (a bare `#` would start a comment).
        assert!(format.starts_with('\''), "must be quoted: {format}");
        // It carries the name plus the three inline `#{E:}` metadata fields (#108).
        for needle in [
            "#{session_name}",
            "#{E:REMORA_AGENT}",
            "#{E:REMORA_WORKSPACE}",
            "#{E:REMORA_CREATED_AT}",
        ] {
            assert!(format.contains(needle), "format missing {needle}: {format}");
        }
        let l = tokens
            .iter()
            .position(|a| a == "list-sessions")
            .expect("list-sessions");
        assert_eq!(tokens[l + 1], "-F");
    }

    #[test]
    fn list_session_names_tokens_is_env_free_and_quoted() {
        let tokens = list_session_names_tokens();
        // Names-only format: NO `#{E:}` env expansion (that's the trusted-set
        // listing, #108), and shell-quoted against the bare-`#` comment trap.
        assert_eq!(tokens.last().map(String::as_str), Some("'#{session_name}'"));
        assert!(
            !tokens.iter().any(|t| t.contains("#{E:")),
            "names listing must carry no env expansion: {tokens:?}"
        );
        let l = tokens
            .iter()
            .position(|a| a == "list-sessions")
            .expect("list-sessions");
        assert_eq!(tokens[l + 1], "-F");
    }

    /// Couples the format-string field ORDER to `parse_session_line`'s positional
    /// read: name, then agent/workspace/created_at, built from `naming::ENV_*`.
    /// A reorder or rename moves both sides together or fails here (#108).
    #[test]
    fn list_sessions_format_orders_fields_for_the_parser() {
        let tokens = list_sessions_tokens();
        let format = tokens.last().expect("format arg");
        let mut last = 0usize;
        for needle in ["session_name", ENV_AGENT, ENV_WORKSPACE, ENV_CREATED_AT] {
            let at = format
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle} in {format}"));
            assert!(at >= last, "field {needle} out of order in {format}");
            last = at;
        }
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
    fn show_environment_tokens_targets_the_name() {
        let tokens = show_environment_tokens("remora_api_fix-login");
        let s = tokens
            .iter()
            .position(|a| a == "show-environment")
            .expect("show-environment");
        assert_eq!(tokens[s + 1], "-t");
        assert_eq!(tokens[s + 2], "remora_api_fix-login");
    }

    #[test]
    fn fingerprint_accepts_a_session_carrying_remora_env() {
        // A realistic `show-environment` dump: the login env plus the REMORA_*
        // metadata spawn wrote.
        let env = "PATH=/usr/bin\nHOME=/home/dev\nREMORA_AGENT=claude\n\
                   REMORA_WORKSPACE=/home/dev/wt\nREMORA_CREATED_AT=1700000000\n";
        assert!(has_remora_fingerprint(env));
    }

    #[test]
    fn fingerprint_tolerates_a_partial_metadata_write() {
        // write_metadata is best-effort (ADR-0004): a single surviving REMORA_*
        // variable still proves the session is ours.
        assert!(has_remora_fingerprint(
            "PATH=/usr/bin\nREMORA_CREATED_AT=1700000000\n"
        ));
    }

    #[test]
    fn fingerprint_rejects_a_same_named_impostor() {
        // A session reusing the `remora_*` name but carrying no REMORA_* env is
        // not one we spawned.
        let env = "PATH=/usr/bin\nHOME=/home/dev\nTERM=xterm-256color\n";
        assert!(!has_remora_fingerprint(env));
    }

    #[test]
    fn fingerprint_ignores_the_removal_form() {
        // tmux prints a variable marked for removal as `-KEY`; that is not a set
        // variable, so it must not satisfy the fingerprint.
        assert!(!has_remora_fingerprint("-REMORA_AGENT\nPATH=/usr/bin\n"));
    }

    #[test]
    fn fingerprint_rejects_a_bare_prefix_with_empty_name() {
        // A line `REMORA_=...` has an empty key after the prefix — not a real
        // metadata var. The empty-name guard must reject it (else a stray value
        // beginning with the prefix could forge a fingerprint).
        assert!(!has_remora_fingerprint("REMORA_=value\n"));
    }

    #[test]
    fn fingerprint_rejects_a_non_identifier_key() {
        // The key between the prefix and `=` must be a valid identifier. A line
        // whose "name" carries a space or punctuation is not a tmux variable.
        assert!(!has_remora_fingerprint("REMORA_ FOO=1\n"));
        assert!(!has_remora_fingerprint("REMORA_A-B=1\n"));
    }

    #[test]
    fn fingerprint_accepts_a_value_containing_equals() {
        // `split_once('=')` keeps the first `=`, so a value that itself contains
        // `=` (e.g. a base64 token) still leaves a valid key and matches.
        assert!(has_remora_fingerprint("REMORA_CREATED_AT=a=b\n"));
    }

    #[test]
    fn run_attach_opens_a_channel_for_a_fingerprinted_session() {
        let fake = FakeExec::new(vec![Ok(FakeExec::out(
            "PATH=/usr/bin\nREMORA_AGENT=claude\nREMORA_WORKSPACE=/w\nREMORA_CREATED_AT=1\n",
        ))]);
        let project = ProjectId::new("api").expect("slug");
        let session = SessionId::new("fix-login").expect("slug");
        let result = run_attach(&fake, &project, &session);
        assert!(result.is_ok(), "{result:?}");
        // Exactly one channel opened, and it is the attach.
        let opened = fake.opened.lock().expect("lock");
        assert_eq!(opened.len(), 1);
        assert!(opened[0].iter().any(|a| a == "attach-session"));
        // One round-trip preflight (show-environment), no separate has-session.
        assert_eq!(fake.count_calls_with("show-environment"), 1);
        assert_eq!(fake.count_calls_with("has-session"), 0);
    }

    #[test]
    fn run_attach_refuses_a_same_named_impostor_before_opening_a_channel() {
        // Session exists (show-environment succeeds) but carries no REMORA_* env.
        let fake = FakeExec::new(vec![Ok(FakeExec::out("PATH=/usr/bin\nTERM=xterm\n"))]);
        let project = ProjectId::new("api").expect("slug");
        let session = SessionId::new("fix-login").expect("slug");
        let result = run_attach(&fake, &project, &session);
        assert!(
            matches!(result, Err(SourceError::SessionNotFound { .. })),
            "{result:?}"
        );
        // No channel opened: the client's input never reaches the impostor.
        assert!(fake.opened.lock().expect("lock").is_empty());
    }

    #[test]
    fn run_attach_maps_a_missing_session_to_not_found() {
        // The real wording `tmux show-environment` (the attach preflight) emits
        // for a missing session — verified against tmux 3.4. It differs from
        // has-session's `can't find session`, so the classifier must match both.
        let fake = FakeExec::new(vec![Ok(FakeExec::fail(
            "no such session: remora_api_fix-login",
        ))]);
        let project = ProjectId::new("api").expect("slug");
        let session = SessionId::new("fix-login").expect("slug");
        let result = run_attach(&fake, &project, &session);
        assert!(
            matches!(result, Err(SourceError::SessionNotFound { .. })),
            "{result:?}"
        );
        assert!(fake.opened.lock().expect("lock").is_empty());
    }

    #[test]
    fn run_attach_surfaces_a_transport_failure_not_not_found() {
        // An ssh/kubectl/network failure also exits non-zero; it must not
        // masquerade as SessionNotFound.
        let fake = FakeExec::new(vec![Ok(FakeExec::fail(
            "kex_exchange_identification: connection closed by remote host",
        ))]);
        let project = ProjectId::new("api").expect("slug");
        let session = SessionId::new("fix-login").expect("slug");
        let result = run_attach(&fake, &project, &session);
        assert!(
            matches!(result, Err(SourceError::Transport(_))),
            "{result:?}"
        );
        assert!(fake.opened.lock().expect("lock").is_empty());
    }

    #[test]
    fn run_attach_maps_a_torn_down_server_to_not_found() {
        // When the whole tmux server is gone, show-environment prints
        // `error connecting to <sock> (No such file or directory)` (verified on
        // tmux 3.4) — no server means no session, so attach treats it as absent.
        let fake = FakeExec::new(vec![Ok(FakeExec::fail(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)",
        ))]);
        let project = ProjectId::new("api").expect("slug");
        let session = SessionId::new("fix-login").expect("slug");
        let result = run_attach(&fake, &project, &session);
        assert!(
            matches!(result, Err(SourceError::SessionNotFound { .. })),
            "{result:?}"
        );
        assert!(fake.opened.lock().expect("lock").is_empty());
    }

    #[test]
    fn attach_absent_classifier_separates_tmux_server_gone_from_ssh_down() {
        // tmux's "error connecting" (server socket gone) is absent for attach...
        assert!(attach_stderr_signals_absent(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
        assert!(attach_stderr_signals_absent(
            "no such session: remora_api_x"
        ));
        // ...but a real ssh/kubectl connection failure is NOT absent — it must
        // stay a transport error, never a misleading "not found".
        assert!(!attach_stderr_signals_absent(
            "ssh: connect to host devbox port 22: Connection refused"
        ));
        assert!(!attach_stderr_signals_absent(
            "Unable to connect to the server: dial tcp: lookup api timed out"
        ));
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
        // Positive: tmux's "session absent" phrasings (case-insensitive). The
        // two callers emit DIFFERENT wording for the same condition —
        // has-session says "can't find session", show-environment (the attach
        // preflight, #105) says "no such session" — both must read as absent.
        assert!(stderr_signals_session_absent(
            "can't find session: remora_api_x"
        ));
        assert!(stderr_signals_session_absent(
            "no such session: remora_api_x"
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
        // Happy path pays no verify round-trip: when every set-environment
        // succeeds, the fingerprint confirm/re-stamp (#105) is skipped entirely.
        assert!(
            !calls
                .iter()
                .any(|c| c.iter().any(|a| a == "show-environment")),
            "no fingerprint verify when metadata writes all succeed"
        );
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn spawn_restamps_metadata_when_every_write_fails_so_session_stays_reconnectable() {
        // All three set-environment writes fail at spawn (transient blip), but
        // the session is live. The REMORA_* env is load-bearing for reconnect
        // (#105), so the tail confirms the fingerprint, finds it absent, and
        // re-stamps once — otherwise every later attach would reject this live
        // session as SessionNotFound forever.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                   // fetch
            Ok(FakeExec::out("origin/main\n")),   // symbolic-ref
            Ok(FakeExec::ok()),                   // verify
            Ok(FakeExec::ok()),                   // worktree add
            Ok(FakeExec::ok()),                   // new-session
            Ok(FakeExec::ok()),                   // allow-passthrough
            Ok(FakeExec::fail("blip")),           // set-environment REMORA_AGENT (FAIL)
            Ok(FakeExec::fail("blip")),           // set-environment REMORA_WORKSPACE (FAIL)
            Ok(FakeExec::fail("blip")),           // set-environment REMORA_CREATED_AT (FAIL)
            Ok(FakeExec::out("PATH=/usr/bin\n")), // show-environment: no fingerprint
            Ok(FakeExec::ok()),                   // re-stamp REMORA_AGENT
            Ok(FakeExec::ok()),                   // re-stamp REMORA_WORKSPACE
            Ok(FakeExec::ok()),                   // re-stamp REMORA_CREATED_AT
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(
            result.is_ok(),
            "a metadata blip must not fail spawn: {result:?}"
        );
        // One verify round-trip, and the three writes were retried (3 + 3 = 6).
        assert_eq!(fake.count_calls_with("show-environment"), 1);
        assert_eq!(fake.count_calls_with("set-environment"), 6);
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[test]
    fn spawn_does_not_restamp_when_one_metadata_write_survives() {
        // A partial write still leaves a usable fingerprint (any single REMORA_*
        // var suffices), so the tail confirms it and skips the re-stamp.
        let plan = worktree_plan();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::ok()),                         // fetch
            Ok(FakeExec::out("origin/main\n")),         // symbolic-ref
            Ok(FakeExec::ok()),                         // verify
            Ok(FakeExec::ok()),                         // worktree add
            Ok(FakeExec::ok()),                         // new-session
            Ok(FakeExec::ok()),                         // allow-passthrough
            Ok(FakeExec::ok()),                         // set-environment REMORA_AGENT (ok)
            Ok(FakeExec::fail("blip")),                 // set-environment REMORA_WORKSPACE (FAIL)
            Ok(FakeExec::fail("blip")),                 // set-environment REMORA_CREATED_AT (FAIL)
            Ok(FakeExec::out("REMORA_AGENT=claude\n")), // show-environment: fingerprint present
        ]);
        let result = run_spawn(&fake, &plan);
        assert!(result.is_ok(), "{result:?}");
        // Verified once, NOT re-stamped: only the original 3 writes happened.
        assert_eq!(fake.count_calls_with("show-environment"), 1);
        assert_eq!(fake.count_calls_with("set-environment"), 3);
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
        // Config has project `api` (worktree, path /home/dev/api). `ghost` is NOT configured.
        let config = test_config();
        // Scripted exec, in call order (two listings, HOME fetch, then worktree scan, #108, #124):
        //  1) list-sessions names -> api (configured) + ghost (unconfigured) +
        //     `main` & `remora__bad` (unparseable). Only api survives.
        //  2) list-sessions inline metadata -> enrichment keyed by trusted name.
        //     workspace_path is absolute so the path-anchored join can match (#124).
        //  3) printf $HOME -> "/home/dev" so paths beginning with /home/dev/api
        //     canonicalize correctly for the A2′ primary-checkout detection.
        //  4) git worktree list for api -> realistic output: primary checkout first
        //     (as real git always emits), then fix-login (live) + add-tests (stopped).
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out(
                "remora_api_fix-login\nremora_ghost_x\nmain\nremora__bad\n",
            )),
            Ok(FakeExec::out(
                "remora_api_fix-login\tclaude\t/home/dev/.remora/worktrees/api/fix-login\t1765500000\n",
            )),
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(
                "worktree /home/dev/api\nHEAD abc\nbranch refs/heads/main\n\n\
                 worktree /home/dev/.remora/worktrees/api/fix-login\nbranch refs/heads/remora/fix-login\n\n\
                 worktree /home/dev/.remora/worktrees/api/add-tests\nbranch refs/heads/remora/add-tests\n",
            )),
        ]);
        let metas = run_list(&fake, &config).expect("list");

        // ghost filtered out (R1). api/add-tests is Stopped+Worktree; api/fix-login is Live+Worktree.
        // The primary checkout (/home/dev/api == project path) surfaces as Stopped+Shared (A2′).
        assert_eq!(
            metas.len(),
            3,
            "expected 3 rows (add-tests, fix-login, main-checkout), got: {metas:?}"
        );
        let main_row = metas
            .iter()
            .find(|m| m.branch.as_deref() == Some("main"))
            .expect("primary-checkout main row missing");
        assert_eq!(
            main_row.workspace,
            Some(WorkspaceMode::Shared),
            "primary checkout must be Shared (A2′): {main_row:?}"
        );
        let add_tests = metas
            .iter()
            .find(|m| m.session_id.as_str() == "add-tests")
            .expect("add-tests row");
        let fix_login = metas
            .iter()
            .find(|m| m.session_id.as_str() == "fix-login")
            .expect("fix-login row");
        assert_eq!(add_tests.state, SessionState::Stopped);
        assert_eq!(fix_login.state, SessionState::Live);
        assert_eq!(fix_login.agent.as_deref(), Some("claude"));
        // Stopped carries the real discovered worktree path (R6).
        assert_eq!(
            add_tests.workspace_path.as_deref(),
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
    fn list_keeps_session_live_when_metadata_read_flakes() {
        // The session is in the trusted names listing, but the inline-metadata
        // read flakes (transient, or tmux < 3.0). It must stay Live with empty
        // metadata, not be downgraded — metadata is best-effort enrichment (#108).
        // Call order: 1) names, 2) metadata (flakes), 3) printf $HOME, 4) worktree list.
        let config = test_config();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // 1) names: live set
            Ok(FakeExec::fail("connection reset")),      // 2) metadata read flakes
            Ok(FakeExec::out("/home/dev")),              // 3) printf $HOME (#124)
            Ok(FakeExec::out("")),                       // 4) worktree list: empty
        ]);
        let metas = run_list(&fake, &config).expect("list");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].state, SessionState::Live);
        assert_eq!(metas[0].agent, None);
        assert_eq!(metas[0].created_at, None);
    }

    #[test]
    fn list_ignores_phantom_row_not_in_trusted_names() {
        // A forged env value with an embedded newline fabricates an extra inline
        // metadata row (`remora_api_evil...`). Because the live set comes from the
        // trusted names-only listing, the phantom name — absent there — must be
        // dropped, never surfaced as a Live session (#108 regression guard).
        // Call order: 1) names, 2) metadata (with injected phantom), 3) printf $HOME, 4) worktree list.
        let config = test_config();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // 1) names: ONLY the real session
            Ok(FakeExec::out(
                "remora_api_fix-login\tx\t\t\n\
                 remora_api_evil\t/ws\t9999999999\t\n", // 2) metadata: real + injected phantom
            )),
            Ok(FakeExec::out("/home/dev")), // 3) printf $HOME (#124)
            Ok(FakeExec::out("")),          // 4) worktree list: empty
        ]);
        let metas = run_list(&fake, &config).expect("list");
        assert_eq!(metas.len(), 1, "phantom must not appear: {metas:?}");
        assert_eq!(metas[0].session_id.as_str(), "fix-login");
    }

    #[test]
    fn list_survives_worktree_list_failure_per_decision_8() {
        // A failed `git worktree list` for one project yields empty for that
        // project, never a failed discovery (decision 8): the live session
        // still lists, just with no Stopped twin.
        // Call order: 1) names, 2) metadata, 3) printf $HOME, 4) worktree list (FAILS).
        // The FakeExec::fail at position 4 must land on the WORKTREE LIST call —
        // not on $HOME — so decision 8 is actually exercised.
        let config = test_config();
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // 1) names: live set
            Ok(FakeExec::out("remora_api_fix-login\tclaude\t\t\n")), // 2) inline metadata
            Ok(FakeExec::out("/home/dev")),              // 3) printf $HOME (#124)
            Ok(FakeExec::fail("fatal: not a git repository")), // 4) worktree list FAILS → decision 8
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
        //   2) printf $HOME         -> "/home/dev" for path canonicalization (#124)
        //   3) git worktree list for `api`     -> empty
        //   4) git worktree list for `scratch` -> s1 worktree entry
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
            Ok(FakeExec::out("")),          // list-sessions: no live sessions
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out("")),          // worktree list for api: empty
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
        // shell_quote leaves clean slug chars unquoted, so the token is unchanged.
        assert_eq!(tokens[g + 5], "remora/fix-login");
    }

    #[test]
    fn branch_delete_tokens_shell_quotes_the_branch() {
        // A branch with shell metacharacters (e.g. from a hand-crafted worktree
        // surfaced as a Stopped session) must be quoted so the remote shell cannot
        // execute the metacharacters as code. `a;id` is the canonical injection
        // probe: unquoted it runs `id` as a separate command.
        let tokens = branch_delete_tokens("/p", "a;id");
        let g = tokens.iter().position(|a| a == "git").expect("git");
        let branch_token = &tokens[g + 5];
        // Must NOT be the raw unquoted injection string.
        assert_ne!(
            branch_token, "a;id",
            "unquoted branch is a code-injection vector"
        );
        // Must equal what shell_quote produces — `'a;id'` — so the remote shell
        // treats it as a literal argument rather than splitting on the `;`.
        assert_eq!(branch_token, &shell_quote("a;id"));
    }

    #[test]
    fn run_remove_metachar_branch_is_shell_quoted_in_branch_delete() {
        // If `git worktree list` returns a worktree whose branch contains shell
        // metacharacters, the `branch -D` call emitted by run_remove must
        // shell-quote the branch token — no raw `a;id` in the argv. This is the
        // end-to-end guard for the injection path discovered in the final review.
        let porcelain = "worktree /home/dev/.remora/worktrees/api/fix-login\n\
                         HEAD abc\n\
                         branch refs/heads/a;id\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list
            Ok(FakeExec::out("")),          // status --porcelain (clean)
            Ok(FakeExec::out("0\n")),       // rev-list (on remote)
            Ok(FakeExec::ok()),             // kill-session
            Ok(FakeExec::ok()),             // worktree remove
            Ok(FakeExec::ok()),             // branch -D
        ]);
        // The session_id derives from the raw branch name "a;id".
        let session_id = derive_session_id(Some("a;id")).expect("slug");
        assert!(run_remove(&fake, &test_config(), &pid("api"), &session_id, false).is_ok());
        let calls = fake.calls.lock().expect("lock");
        let del_call = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "-D"))
            .expect("branch -D call must be present");
        // The branch token following "-D" must NOT be the raw "a;id" string.
        let d_pos = del_call.iter().position(|a| a == "-D").expect("-D");
        let branch_token = &del_call[d_pos + 1];
        assert_ne!(
            branch_token, "a;id",
            "raw unquoted branch would allow remote code execution: {del_call:?}"
        );
        assert_eq!(
            branch_token,
            &shell_quote("a;id"),
            "branch token must be shell-quoted: {del_call:?}"
        );
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
        // Call order: printf $HOME, git worktree list, status, rev-list,
        // kill-session, worktree remove, branch -D.
        let porcelain = "worktree /home/dev/.remora/worktrees/api/fix-login\n\
                         HEAD abc\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list
            Ok(FakeExec::out("")),          // status --porcelain (clean)
            Ok(FakeExec::out("0\n")),       // rev-list (on remote)
            Ok(FakeExec::ok()),             // kill-session
            Ok(FakeExec::ok()),             // worktree remove
            Ok(FakeExec::ok()),             // branch -D
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok());
        let calls = fake.calls.lock().expect("lock");
        // call 0: home fetch (printf)
        assert!(
            calls[0].iter().any(|a| a == "printf"),
            "call 0 must be printf: {:?}",
            calls[0]
        );
        // call 1: git worktree list
        assert!(
            calls[1].iter().any(|a| a == "worktree") && calls[1].iter().any(|a| a == "list"),
            "call 1 must be worktree list: {:?}",
            calls[1]
        );
        // calls 2–3: dirty probe
        assert!(calls[2].iter().any(|a| a == "status"));
        assert!(calls[3].iter().any(|a| a == "rev-list"));
        // call 4: kill-session
        assert!(calls[4].iter().any(|a| a == "kill-session"));
        // call 5: worktree remove (uses real path from git)
        assert!(
            calls[5].iter().any(|a| a == "remove"),
            "call 5 must be worktree remove: {:?}",
            calls[5]
        );
        // call 6: branch -D (uses real branch from git)
        assert!(
            calls[6].iter().any(|a| a == "-D"),
            "call 6 must be branch -D: {:?}",
            calls[6]
        );
    }

    #[test]
    fn run_remove_refuses_dirty_without_force_and_touches_nothing() {
        // Call order: printf $HOME, git worktree list (found), status (dirty), rev-list.
        let porcelain = "worktree /home/dev/.remora/worktrees/api/fix-login\n\
                         HEAD abc\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")),     // printf $HOME
            Ok(FakeExec::out(porcelain)),       // git worktree list: found
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
    fn run_remove_force_skips_the_dirty_probe() {
        // force=true skips the dirty-check (status/rev-list); the worktree is
        // discovered via `git worktree list`, not `test -d`.
        // Call order: printf $HOME, git worktree list (found), kill-session,
        // worktree remove, branch -D.
        let porcelain = "worktree /home/dev/.remora/worktrees/api/fix-login\n\
                         HEAD abc\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list: found
            Ok(FakeExec::ok()),             // kill-session
            Ok(FakeExec::ok()),             // worktree remove
            Ok(FakeExec::ok()),             // branch -D
        ]);
        assert!(run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), true).is_ok());
        assert_eq!(fake.count_calls_with("status"), 0);
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        assert_eq!(fake.count_calls_with("remove"), 1);
        assert_eq!(fake.count_calls_with("-D"), 1);
    }

    #[test]
    fn run_remove_idempotent_when_worktree_and_branch_already_gone() {
        // `git worktree list` still lists the entry (admin entry survives a bare
        // `rm -rf`), but the subsequent `worktree remove` and `branch -D` find
        // nothing and emit the "already gone" stderr — both must be tolerated.
        // Call order: printf $HOME, git worktree list (found), status, rev-list,
        // kill-session, worktree remove (gone), branch -D (gone).
        let porcelain = "worktree /home/dev/.remora/worktrees/api/fix-login\n\
                         HEAD abc\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list: found
            Ok(FakeExec::out("")),          // status --porcelain (clean)
            Ok(FakeExec::out("0\n")),       // rev-list
            Ok(FakeExec::ok()),             // kill-session
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
        // No matching worktree in `git worktree list` → shared/gone session:
        // kill tmux only (no worktree remove, no branch delete).
        // Call order: printf $HOME, git worktree list (no match), kill-session.
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
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out("")),          // git worktree list: no matching entry
            Ok(FakeExec::ok()),             // kill-session
        ]);
        assert!(run_remove(&fake, &config, &pid("scratch"), &sid("s1"), false).is_ok());
        assert_eq!(fake.count_calls_with("kill-session"), 1);
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
    fn resolve_local_command_reports_ambiguous_selector_clearly() {
        // A selector matching N pods (the multi-replica/HPA case) must surface a
        // clear "matched N, expected 1" signal — not the opaque control-char
        // rejection. ADR-0009 single-active-pod, #115.
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "printf 'web-0\\nweb-1\\nweb-2\\n'")
            .expect_err("three pods, expected one");
        let msg = format!("{err}");
        assert!(msg.contains("matched 3 values"), "{msg}");
        assert!(msg.contains("web-0") && msg.contains("web-2"), "{msg}");
        assert!(msg.contains("expected exactly 1"), "{msg}");
    }

    #[test]
    fn resolve_local_command_reports_space_separated_matches() {
        // A jsonpath selector (`-o jsonpath='{.items[*].metadata.name}'`) emits
        // space-separated names on ONE line; without whitespace-splitting this
        // slips past both the line check and the literal-field guard and
        // resolves to a bogus joined token. #115.
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "printf 'web-0 web-1 web-2'")
            .expect_err("three space-separated pods");
        let msg = format!("{err}");
        assert!(msg.contains("matched 3 values"), "{msg}");
        assert!(msg.contains("web-0") && msg.contains("web-2"), "{msg}");
    }

    #[test]
    fn resolve_local_command_caps_ambiguous_sample() {
        // Many matches: list only the first few, summarise the rest, so a 64 KiB
        // selector can't blow the bounded error detail (count of names capped).
        let runner = ShellRunner::new();
        let err = resolve_local_command(&runner, "pod", "printf 'a\\nb\\nc\\nd\\ne\\n'")
            .expect_err("five pods");
        let msg = format!("{err}");
        assert!(msg.contains("matched 5 values"), "{msg}");
        assert!(msg.contains("(+2 more)"), "{msg}");
        assert!(
            !msg.contains(", d"),
            "must not list past the sample cap: {msg}"
        );
    }

    #[test]
    fn resolve_local_command_clips_long_match_names() {
        // A pathologically long match token is clipped (length cap) so the count
        // and the actionable guidance still fit under the 256-char display cap.
        let runner = ShellRunner::new();
        let err = resolve_local_command(
            &runner,
            "pod",
            "printf 'a%.0s' $(seq 1 200); printf '\\nb\\n'",
        )
        .expect_err("oversized first match");
        let msg = format!("{err}");
        assert!(msg.contains("matched 2 values"), "{msg}");
        assert!(msg.contains("..."), "long name should be clipped: {msg}");
        assert!(
            msg.contains("expected exactly 1"),
            "guidance must survive: {msg}"
        );
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
        let msg = format!("{err}");
        // Surfaces the stderr AND the command that ran, so a failure reads as a
        // command problem rather than a mysterious cluster error (#127).
        assert!(msg.contains("boom"), "{msg}");
        assert!(msg.contains("echo boom"), "{msg}");
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
        // `git worktree list` fails with a non-empty stderr (ssh/auth/shell
        // error, not "no results"). run_remove must NOT mistake that for "no
        // worktree" and proceed to kill tmux while skipping cleanup — that would
        // orphan a live worktree + branch. It fails closed with Transport.
        // Call order: printf $HOME (best-effort, falls back), git worktree list
        // (transport error → Err(Transport)).
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
            Ok(FakeExec::out("/home/dev")),                         // printf $HOME
            Ok(FakeExec::fail("ssh: connect: connection refused")), // git worktree list: error
        ]);
        let err = run_remove(&fake, &config, &pid("scratch"), &sid("s1"), true).expect_err("err");
        assert!(matches!(err, SourceError::Transport(_)));
        // Failed closed: tmux was never killed, nothing cleaned up.
        assert_eq!(fake.count_calls_with("kill-session"), 0);
        assert_eq!(fake.count_calls_with("remove"), 0);
    }

    #[test]
    fn run_remove_worktree_session_on_shared_config_project_cleans_up_worktree() {
        // A worktree session spawned on a shared-default project must still be
        // cleaned up: `git worktree list` finds the worktree, so run_remove
        // issues worktree remove + branch -D regardless of the project config's
        // workspace setting.
        // Call order: printf $HOME, git worktree list (found), kill-session
        // (force=true, no dirty-check), worktree remove, branch -D.
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
        let porcelain = "worktree /home/dev/.remora/worktrees/scratch/s1\n\
                         HEAD abc\n\
                         branch refs/heads/remora/s1\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list: found
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
            "worktree remove must run for an existing worktree"
        );
        assert_eq!(
            fake.count_calls_with("-D"),
            1,
            "branch delete must run for an existing worktree"
        );
    }

    // -----------------------------------------------------------------------
    // Task 5: resolve_worktree — custom path, A2′, not-found (#124)
    // -----------------------------------------------------------------------

    #[test]
    fn run_remove_custom_path_session_uses_real_worktree_path_and_branch() {
        // Task 5 RED→GREEN: a session with a non-convention worktree path (e.g.
        // spawned with a custom `worktree_root`). `git worktree list` returns the
        // REAL path; run_remove must remove that path, not the convention one.
        //
        // FakeExec call order:
        //   1) printf $HOME → "/home/dev"
        //   2) git worktree list → primary checkout at /home/dev/api (main branch)
        //      + custom-path worktree at /home/dev/mywork/fix-login (remora/fix-login)
        //   3) git status (clean)
        //   4) git rev-list (0)
        //   5) kill-session
        //   6) git worktree remove /home/dev/mywork/fix-login  ← REAL path, not convention
        //   7) git branch -D remora/fix-login
        let porcelain = "worktree /home/dev/api\n\
                         HEAD abc\n\
                         branch refs/heads/main\n\
                         \n\
                         worktree /home/dev/mywork/fix-login\n\
                         HEAD def\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list
            Ok(FakeExec::out("")),          // git status (clean)
            Ok(FakeExec::out("0\n")),       // git rev-list
            Ok(FakeExec::ok()),             // kill-session
            Ok(FakeExec::ok()),             // git worktree remove
            Ok(FakeExec::ok()),             // git branch -D
        ]);
        assert!(
            run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok(),
            "custom-path session must be fully cleaned up"
        );
        let calls = fake.calls.lock().expect("lock");
        // worktree remove must use the REAL path, NOT the convention path.
        let rm_call = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "remove"))
            .expect("worktree remove call must be present");
        assert!(
            rm_call.iter().any(|a| a.contains("mywork/fix-login")),
            "worktree remove must target the real path: {rm_call:?}"
        );
        assert!(
            !rm_call.iter().any(|a| a.contains(".remora/worktrees")),
            "worktree remove must NOT use the convention path: {rm_call:?}"
        );
        // branch -D must use the REAL branch from git.
        let del_call = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "-D"))
            .expect("branch -D call must be present");
        assert!(
            del_call.iter().any(|a| a == "remora/fix-login"),
            "branch -D must use the real branch: {del_call:?}"
        );
    }

    #[test]
    fn run_remove_primary_checkout_kills_tmux_only_never_removes_worktree() {
        // Task 5 A2′ guard: if the session's resolved worktree path equals
        // `project.path` (the primary checkout), teardown must kill tmux only —
        // never `git worktree remove` or `branch -D` the primary checkout.
        //
        // Scenario: project `api` at `/home/dev/api`; the primary checkout happens
        // to be on branch `remora/fix-login` (maps to session_id `fix-login`).
        //
        // FakeExec call order:
        //   1) printf $HOME → "/home/dev"
        //   2) git worktree list → one entry: /home/dev/api on remora/fix-login
        //      → real_dir == primary_path → A2′ path
        //   3) kill-session (only op)
        let porcelain = "worktree /home/dev/api\n\
                         HEAD abc\n\
                         branch refs/heads/remora/fix-login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list
            Ok(FakeExec::ok()),             // kill-session
        ]);
        assert!(
            run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok(),
            "A2' path must succeed with kill-session only"
        );
        assert_eq!(
            fake.count_calls_with("kill-session"),
            1,
            "kill-session must run exactly once"
        );
        assert_eq!(
            fake.count_calls_with("remove"),
            0,
            "A2': must NOT git-worktree-remove the primary checkout"
        );
        assert_eq!(
            fake.count_calls_with("-D"),
            0,
            "A2': must NOT branch -D the primary checkout"
        );
        // Exactly 3 calls total: printf, worktree list, kill-session.
        assert_eq!(
            fake.calls.lock().expect("lock").len(),
            3,
            "A2' must make exactly 3 remote calls (printf, worktree list, kill-session)"
        );
    }

    #[test]
    fn run_remove_not_found_in_worktree_list_kills_tmux_only() {
        // If `git worktree list` returns no entry matching the session_id (shared
        // session, or worktree already physically gone), run_remove kills tmux
        // only and does not attempt worktree remove or branch delete.
        //
        // FakeExec call order:
        //   1) printf $HOME → "/home/dev"
        //   2) git worktree list → only the primary checkout (no remora/fix-login)
        //   3) kill-session
        let porcelain = "worktree /home/dev/api\n\
                         HEAD abc\n\
                         branch refs/heads/main\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list: no match for fix-login
            Ok(FakeExec::ok()),             // kill-session
        ]);
        assert!(
            run_remove(&fake, &test_config(), &pid("api"), &sid("fix-login"), false).is_ok(),
            "not-found session must succeed with kill-session only"
        );
        assert_eq!(fake.count_calls_with("kill-session"), 1);
        assert_eq!(
            fake.count_calls_with("remove"),
            0,
            "must not worktree-remove on not-found"
        );
        assert_eq!(
            fake.count_calls_with("-D"),
            0,
            "must not branch -D on not-found"
        );
        assert_eq!(fake.calls.lock().expect("lock").len(), 3);
    }

    // -----------------------------------------------------------------------
    // Task 6: run_respawn uses the real worktree path for custom-root sessions
    // -----------------------------------------------------------------------

    #[test]
    fn run_respawn_uses_real_worktree_path_for_custom_path_session() {
        // Task 6 RED→GREEN: a session spawned with a custom `worktree_root`
        // lives at a non-convention path. `resolve_worktree` finds it via
        // `git worktree list`; `run_respawn` must probe/attach at the REAL
        // path, not the convention path.
        //
        // Session: branch `feat/login` → session_id `feat-login-<hash>`.
        // Convention plan dir: `~/.remora/worktrees/api/feat-login-<hash>`.
        // Real worktree dir: `/mnt/work/feat/login`.
        //
        // FakeExec call order:
        //   1) printf $HOME  → "/home/dev"
        //   2) git worktree list → primary (main) + custom path (feat/login)
        //   3) test -d /mnt/work/feat/login → success
        //   4) tmux new-session  → success
        //   (5-N) best-effort set-option + setenv calls use FakeExec default ok
        let session_id = derive_session_id(Some("feat/login")).expect("slug");
        let project_id = ProjectId::new("api").expect("slug");
        let tmux_name = tmux_session_name(&project_id, &session_id);
        let convention_dir = format!("~/.remora/worktrees/api/{}", session_id.as_str());
        let plan = SpawnPlan {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            tmux_name: tmux_name.clone(),
            workspace: WorkspaceMode::Worktree,
            project_path: "/home/dev/api".into(),
            dir: convention_dir.clone(), // CONVENTION path — wrong for this session
            branch: Some(format!("remora/{}", session_id.as_str())), // CONVENTION branch
            base: None,
            env: vec![
                ("REMORA_AGENT".into(), "claude".into()),
                ("REMORA_WORKSPACE".into(), convention_dir.clone()),
                ("REMORA_CREATED_AT".into(), "1700000000".into()),
            ],
            agent_argv: vec!["claude".into(), "--continue".into()],
        };
        let porcelain = "worktree /home/dev/api\n\
                         HEAD abc\n\
                         branch refs/heads/main\n\
                         \n\
                         worktree /mnt/work/feat/login\n\
                         HEAD def\n\
                         branch refs/heads/feat/login\n";
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list
            Ok(FakeExec::ok()),             // test -d REAL path
            Ok(FakeExec::ok()),             // tmux new-session
        ]);
        assert!(
            run_respawn(&fake, &plan).is_ok(),
            "respawn of custom-path session must succeed"
        );
        let calls = fake.calls.lock().expect("lock");
        // The `test -d` probe MUST target the REAL path, not the convention one.
        let probe = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "test"))
            .expect("test -d probe must run");
        assert!(
            probe.iter().any(|a| a.contains("mnt/work/feat/login")),
            "preflight must probe the REAL path: {probe:?}"
        );
        assert!(
            !probe.iter().any(|a| a.contains(".remora/worktrees")),
            "preflight must NOT probe the convention path: {probe:?}"
        );
        // new-session must also reference the REAL path as the start directory.
        let new_session = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "new-session"))
            .expect("new-session must run");
        assert!(
            new_session
                .iter()
                .any(|a| a.contains("mnt/work/feat/login")),
            "new-session must target the REAL path: {new_session:?}"
        );
        assert_eq!(
            fake.opened.lock().expect("lock").len(),
            1,
            "must open exactly one channel"
        );
    }

    #[test]
    fn run_respawn_falls_back_to_convention_when_worktree_not_found() {
        // If `git worktree list` has no entry matching the session_id (shared
        // session, convention session without a custom root, or already-gone
        // worktree), run_respawn must fall back to the convention plan dir.
        //
        // FakeExec call order:
        //   1) printf $HOME → "/home/dev"
        //   2) git worktree list → no entry for remora/fix-login → None
        //   3) test -d CONVENTION dir → success
        //   4) tmux new-session → success
        let plan = worktree_plan(); // convention dir: ~/.remora/worktrees/api/fix-login
        let porcelain = "worktree /home/dev/api\n\
                         HEAD abc\n\
                         branch refs/heads/main\n"; // no remora/fix-login entry
        let fake = FakeExec::new(vec![
            Ok(FakeExec::out("/home/dev")), // printf $HOME
            Ok(FakeExec::out(porcelain)),   // git worktree list → no match → None
            Ok(FakeExec::ok()),             // test -d convention dir
            Ok(FakeExec::ok()),             // tmux new-session
        ]);
        assert!(
            run_respawn(&fake, &plan).is_ok(),
            "convention fallback must succeed"
        );
        let calls = fake.calls.lock().expect("lock");
        let probe = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "test"))
            .expect("test -d probe must run");
        assert!(
            probe
                .iter()
                .any(|a| a.contains(".remora/worktrees/api/fix-login")),
            "fallback must probe the CONVENTION path: {probe:?}"
        );
        assert_eq!(
            fake.opened.lock().expect("lock").len(),
            1,
            "must open exactly one channel"
        );
    }
}
