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

use super::pty_process::spawn_pty_channel;
use crate::spawn_plan::SpawnPlan;
use crate::{SessionChannel, SourceError};

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
/// kubectl transport additionally wraps `env TERM=…` in-container because
/// kubectl does not reliably forward the client TERM into the pod PTY.
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
pub(crate) fn wrap_with_shell_fallback(agent_command: &str) -> String {
    format!(
        r#"{agent_command}; __remora_rc=$?; case "$__remora_rc" in 0|130|143) exec "${{SHELL:-/bin/sh}}" -l ;; *) exit "$__remora_rc" ;; esac"#
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
/// branch and no commits yet.
pub(crate) fn worktree_remove_tokens(plan: &SpawnPlan) -> Vec<String> {
    vec![
        "git".into(),
        "-C".into(),
        quote_remote_path(&plan.project_path),
        "worktree".into(),
        "remove".into(),
        "--force".into(),
        quote_remote_path(&plan.dir),
    ]
}

/// Tokens for `tmux new-session -d -s <name> -c <dir> <agent…> ';' set-option
/// -t <name> remain-on-exit on` — the atomic creation lock, with
/// `remain-on-exit` applied in the **same tmux invocation** via tmux's own
/// argv command separator.
///
/// The `;` is **shell-quoted** (`';'`) so the remote login shell passes it to
/// tmux as a literal separator token rather than eating it as a shell statement
/// separator — this is tmux's argv `;`, distinct from ADR-0004's un-batching of
/// shell-`;`-joined remote commands.
pub(crate) fn new_session_tokens(plan: &SpawnPlan) -> Vec<String> {
    vec![
        "tmux".into(),
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        plan.tmux_name.clone(),
        "-c".into(),
        quote_remote_path(&plan.dir),
        shell_quote(&wrap_with_shell_fallback(&join_agent_command(&plan.agent_argv))),
        shell_quote(";"),
        "set-option".into(),
        "-t".into(),
        plan.tmux_name.clone(),
        "remain-on-exit".into(),
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::WorkspaceMode;
    use crate::spawn_plan::SpawnPlan;
    use remora_protocol::{ProjectId, SessionId};

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

    #[test]
    fn attach_tokens_builds_correct_tmux_command() {
        let tokens = attach_tokens("remora_api_fix-login");
        assert_eq!(tokens, vec!["tmux", "attach-session", "-d", "-t", "remora_api_fix-login"]);
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
        let n = tokens.iter().position(|a| a == "new-session").expect("new-session");
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
        let sep = tokens.iter().position(|a| a == "';'").expect("shell-quoted tmux separator");
        assert_eq!(tokens[sep + 1], "set-option");
        assert_eq!(tokens[sep + 2], "-t");
        assert_eq!(tokens[sep + 3], "remora_api_fix-login");
        assert_eq!(tokens[sep + 4], "remain-on-exit");
        assert_eq!(tokens[sep + 5], "on");
        let new_session = tokens.iter().position(|a| a == "new-session").expect("new-session");
        assert!(sep > new_session, "separator follows new-session");
    }

    #[test]
    fn set_environment_tokens_quotes_logical_value() {
        let tokens = set_environment_tokens(
            "remora_api_fix-login",
            "REMORA_WORKSPACE",
            "~/.remora/worktrees/api/fix-login",
        );
        let s = tokens.iter().position(|a| a == "set-environment").expect("set-env");
        assert_eq!(tokens[s + 1], "-t");
        assert_eq!(tokens[s + 2], "remora_api_fix-login");
        assert_eq!(tokens[s + 3], "REMORA_WORKSPACE");
        assert_eq!(tokens[s + 4], "'~/.remora/worktrees/api/fix-login'");
    }

    #[test]
    fn list_sessions_tokens_quotes_the_format_string() {
        let tokens = list_sessions_tokens();
        assert_eq!(tokens.last().map(String::as_str), Some("'#{session_name}'"));
        let l = tokens.iter().position(|a| a == "list-sessions").expect("list-sessions");
        assert_eq!(tokens[l + 1], "-F");
    }

    #[test]
    fn has_session_tokens_targets_the_name() {
        let tokens = has_session_tokens("remora_api_fix-login");
        let h = tokens.iter().position(|a| a == "has-session").expect("has-session");
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
        let n = tokens.iter().position(|a| a == "new-session").expect("new-session");
        // Still exactly one agent-command token after `-c <dir>` (wrapped, not
        // per-token), followed only by the 6-token remain-on-exit trailer.
        // tmux + new-session + 4 flags + dir + agent-cmd + sep + 5 set-option args = 14 tokens
        assert_eq!(tokens.len(), n + 1 + 6 + 6);
        let fragment = join_agent_command(&plan.agent_argv);
        let inner = wrap_with_shell_fallback(&fragment);
        assert_eq!(tokens[n + 6], shell_quote(&inner));
        assert!(
            inner.starts_with("claude --append-system-prompt 'Be concise';"),
            "got: {inner}"
        );
    }
}
