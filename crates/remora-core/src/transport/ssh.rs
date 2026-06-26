//! `SshSource` — the first real transport. Builds the ssh argv from a
//! validated `SshHost` and delegates to the PTY-process bridge.

use std::sync::Arc;

use async_trait::async_trait;
use remora_protocol::{ProjectId, SessionId, SessionMeta, SpawnSpec};

use super::remote::{
    attach_channel, capture, has_session_tokens, open_pty, run_list, run_remove, run_respawn,
    run_spawn, run_stop, stderr_signals_session_absent, RemoteExec, RemoteOutput,
};
use crate::config::{Config, SshHost};
use crate::naming::tmux_session_name;
use crate::spawn_plan::plan_spawn;
use crate::{SessionChannel, SessionSource, SourceError};

/// Pins a UTF-8 locale on every remote command so the session's tmux runs in
/// UTF-8 mode and doesn't mangle the agent's box-drawing output. ssh runs
/// `$SHELL -c <cmd>` non-interactively (no profile sourced), so a locale
/// reaches the remote only via `SendEnv LANG LC_*` locally **and** `AcceptEnv`
/// on the server — a fragile default that silently no-ops when either is
/// absent. Prefixing `env LANG=… LC_ALL=…` sets it unconditionally instead:
/// the ssh analogue of the kubectl pod-shell preamble. `C.UTF-8` is present
/// without `locale-gen` and keeps diagnostics English (so it also satisfies
/// the deferred `LC_ALL=C` stderr-hardening intent). No `TERM` — ssh forwards
/// the client TERM to the remote PTY on its own.
const REMOTE_LOCALE_PREFIX: [&str; 3] = ["env", "LANG=C.UTF-8", "LC_ALL=C.UTF-8"];

/// Builds the full ssh argv: connection flags + UTF-8 locale prefix + the
/// logical remote tokens. `interactive` adds `-tt`. This is the single place
/// ssh argvs are composed, so the byte-for-byte shape the tests pin lives here.
fn ssh_compose(host: &SshHost, interactive: bool, tokens: &[String]) -> Vec<String> {
    let mut argv = ssh_base_argv(host, interactive);
    argv.extend(REMOTE_LOCALE_PREFIX.iter().map(|s| (*s).to_string()));
    argv.extend_from_slice(tokens);
    argv
}

struct RealSshExec {
    host: SshHost,
}

impl RemoteExec for RealSshExec {
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError> {
        capture(&ssh_compose(&self.host, false, remote))
    }

    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError> {
        open_pty(&ssh_compose(&self.host, true, remote))
    }
}

/// One instance = one configured ssh host (matches the `SessionSource`
/// trait doc).
pub struct SshSource {
    config: Arc<Config>,
    exec: Arc<dyn RemoteExec>,
}

impl SshSource {
    /// Wraps a configured ssh host as a transport.
    pub fn new(host: SshHost, config: Arc<Config>) -> Self {
        Self {
            config,
            exec: Arc::new(RealSshExec { host }),
        }
    }

    #[cfg(test)]
    fn with_exec(_host: SshHost, config: Arc<Config>, exec: Arc<dyn RemoteExec>) -> Self {
        Self { config, exec }
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
    // remote git/tmux) are not covered — see #99 (execution watchdog).
    argv.push("-o".into());
    argv.push("ConnectTimeout=10".into());
    // Connection multiplexing (#63): discovery + spawn fan out into many
    // short-lived ssh calls plus the long-lived attach. Sharing one
    // authenticated master means the user authenticates once (one FIDO touch /
    // bastion hop) and the rest skip the handshake. `auto` — not `yes` — is the
    // safety choice: a stale or orphaned socket (host rebooted, master killed)
    // makes ssh fall back to a fresh connection rather than wedge, and a normal
    // idle `ControlPersist` exit removes the socket itself. `%C` is a
    // fixed-length hash of (local-host, remote-host, port, user): unique per
    // host and immune to the ~104-char unix-socket path limit; ssh expands it
    // and `~` itself (this argv is exec'd directly, no shell). The warm socket
    // lingers `ControlPersist=60s` after the last connection — a small, bounded
    // security surface documented in ADR-0011. Scoped to direct ssh only; the
    // relay would own masters for many users/hosts and is deferred there.
    argv.push("-o".into());
    argv.push("ControlMaster=auto".into());
    argv.push("-o".into());
    argv.push("ControlPath=~/.ssh/remora-%C".into());
    argv.push("-o".into());
    argv.push("ControlPersist=60s".into());
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

#[async_trait]
impl SessionSource for SshSource {
    /// Resolves the spawn plan from config, then runs the full spawn
    /// orchestration (worktree add → tmux new-session → env metadata →
    /// attach) via the injectable `RemoteExec` seam.
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        tokio::task::spawn_blocking(move || run_spawn(exec.as_ref(), &plan))
            .await
            .map_err(|e| SourceError::Transport(format!("spawn task: {e}")))?
    }

    /// Opens a channel to an existing *live* session over ssh.
    ///
    /// A `tmux has-session` preflight returns `SessionNotFound` for a missing
    /// or stopped session (honoring the trait contract stage 4 deferred); a
    /// dead-pane session still exists and is attachable. A failed preflight is
    /// only treated as absent for a known tmux no-such-session stderr — an
    /// ssh/auth/network failure also exits non-zero and surfaces as `Transport`
    /// rather than a misleading `SessionNotFound`. The TOCTOU window (session
    /// dies between preflight and attach) degrades to channel death.
    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let tmux_name = tmux_session_name(project_id, session_id);
        let exec = Arc::clone(&self.exec);
        let project_id = project_id.clone();
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let out = exec.run(&has_session_tokens(&tmux_name))?;
            if !out.success {
                return if stderr_signals_session_absent(&out.stderr) {
                    Err(SourceError::SessionNotFound {
                        project_id,
                        session_id,
                    })
                } else {
                    Err(SourceError::Transport(out.stderr))
                };
            }
            attach_channel(exec.as_ref(), &tmux_name)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("attach task: {e}")))?
    }

    /// Discovers sessions on the host and joins them to local config.
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let exec = Arc::clone(&self.exec);
        let config = Arc::clone(&self.config);
        tokio::task::spawn_blocking(move || run_list(exec.as_ref(), &config))
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
            base: None,
            // Respawn only ever targets a session whose worktree survived, so
            // plan worktree mode regardless of the project's current default.
            // The `test -d` preflight in run_respawn maps a gone worktree to
            // SessionNotFound.
            workspace: Some(remora_protocol::WorkspaceMode::Worktree),
        };
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        tokio::task::spawn_blocking(move || run_respawn(exec.as_ref(), &plan))
            .await
            .map_err(|e| SourceError::Transport(format!("respawn task: {e}")))?
    }

    async fn stop(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<(), SourceError> {
        let exec = Arc::clone(&self.exec);
        let config = Arc::clone(&self.config);
        let (p, s) = (project_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || run_stop(exec.as_ref(), &config, &p, &s))
            .await
            .map_err(|e| SourceError::Transport(format!("stop task: {e}")))?
    }

    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError> {
        let exec = Arc::clone(&self.exec);
        let config = Arc::clone(&self.config);
        let (p, s) = (project_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || run_remove(exec.as_ref(), &config, &p, &s, force))
            .await
            .map_err(|e| SourceError::Transport(format!("remove task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::remote::tests::{test_config, FakeExec};
    use super::super::remote::{
        attach_tokens, has_session_tokens, list_sessions_tokens, new_session_tokens,
        set_environment_tokens, worktree_add_tokens,
    };
    use super::super::remote::{join_agent_command, shell_quote, wrap_with_shell_fallback};
    use super::*;
    use crate::config::WorkspaceMode;
    use crate::spawn_plan::SpawnPlan;
    use crate::SessionSource;
    use remora_protocol::{AgentId, ProjectId, SessionId, SessionState, SpawnSpec};

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
        SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            agent: Some(AgentId::new("claude").expect("slug")),
            base: None,
            workspace: None,
        }
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

    // -----------------------------------------------------------------------
    // ssh_compose tests: pin the exact ssh argv shape
    // -----------------------------------------------------------------------

    #[test]
    fn ssh_compose_attach_minimal_host_has_keepalive_no_dashdash() {
        let argv = ssh_compose(
            &host("devbox", None, None),
            true,
            &attach_tokens("remora_api_fix-login"),
        );
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
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=~/.ssh/remora-%C",
                "-o",
                "ControlPersist=60s",
                "devbox",
                "env",
                "LANG=C.UTF-8",
                "LC_ALL=C.UTF-8",
                "tmux",
                "attach-session",
                "-d",
                "-t",
                "remora_api_fix-login",
            ]
        );
        assert!(!argv.iter().any(|a| a == "--"), "no options terminator");
    }

    /// True if `argv` carries the OpenSSH option `-o <opt>` as an adjacent pair.
    fn has_ssh_opt(argv: &[String], opt: &str) -> bool {
        argv.windows(2).any(|w| w[0] == "-o" && w[1] == opt)
    }

    #[test]
    fn ssh_compose_carries_connection_multiplexing_options() {
        // #63: every ssh call multiplexes over one authenticated master so the
        // user authenticates once. `auto` (not `yes`) means a stale/dead master
        // gracefully degrades to a fresh connection instead of wedging. `%C` is
        // a fixed-length hash of (local-host, remote-host, port, user) — unique
        // per host and immune to the unix-socket path-length limit. ssh expands
        // `~` and `%C` itself (the argv is exec'd directly, no shell). The flags
        // ride BOTH the blocking setup calls and the long-lived attach so they
        // share the same master.
        for interactive in [true, false] {
            let argv = ssh_compose(
                &host("devbox", None, None),
                interactive,
                &attach_tokens("remora_api_s"),
            );
            assert!(
                has_ssh_opt(&argv, "ControlMaster=auto"),
                "interactive={interactive}: {argv:?}"
            );
            assert!(
                has_ssh_opt(&argv, "ControlPath=~/.ssh/remora-%C"),
                "interactive={interactive}: {argv:?}"
            );
            assert!(
                has_ssh_opt(&argv, "ControlPersist=60s"),
                "interactive={interactive}: {argv:?}"
            );
        }
    }

    #[test]
    fn ssh_compose_pins_utf8_locale_on_both_paths() {
        // Regression: without a UTF-8 locale the remote tmux runs non-UTF-8 and
        // mangles the agent's box-drawing output. ssh's non-interactive
        // `$SHELL -c` sources no profile, and SendEnv/AcceptEnv is fragile, so
        // every remote command — interactive attach and blocking setup alike —
        // carries an `env LANG=… LC_ALL=…` prefix. The locale must precede the
        // remote command so `env` execs it with the locale set.
        for interactive in [true, false] {
            let argv = ssh_compose(
                &host("devbox", None, None),
                interactive,
                &new_session_tokens(&worktree_plan()),
            );
            let env = argv.iter().position(|a| a == "env").expect("env prefix");
            assert_eq!(argv[env + 1], "LANG=C.UTF-8");
            assert_eq!(argv[env + 2], "LC_ALL=C.UTF-8");
            let tmux = argv.iter().position(|a| a == "tmux").expect("tmux");
            assert!(env < tmux, "locale prefix precedes the remote command");
        }
    }

    #[test]
    fn ssh_compose_inserts_port_then_user_before_host() {
        let argv = ssh_compose(
            &host("devbox", Some("dev"), Some(2222)),
            true,
            &attach_tokens("remora_api_s"),
        );
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
    fn ssh_compose_omits_absent_optional_flags() {
        let argv = ssh_compose(
            &host("devbox", None, None),
            true,
            &attach_tokens("remora_api_s"),
        );
        assert!(!argv.iter().any(|a| a == "-p"), "no port flag");
        assert!(!argv.iter().any(|a| a == "-l"), "no user flag");
    }

    #[test]
    fn ssh_compose_attach_carries_tmux_name_and_eviction_flags() {
        let argv = ssh_compose(
            &host("devbox", None, None),
            true,
            &attach_tokens("remora_web_zeta"),
        );
        assert_eq!(argv.last().map(String::as_str), Some("remora_web_zeta"));
        assert!(argv.iter().any(|a| a == "-tt"), "forces remote PTY");
        // `-d` is the tmux eviction flag, positioned after attach-session.
        let attach = argv
            .iter()
            .position(|a| a == "attach-session")
            .expect("attach");
        assert_eq!(argv[attach + 1], "-d");
    }

    #[test]
    fn ssh_compose_worktree_add_builds_git_command() {
        let plan = worktree_plan();
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &worktree_add_tokens(&plan, None),
        );
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
    fn ssh_compose_new_session_is_the_lock_with_no_env_trailer() {
        let plan = worktree_plan();
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &new_session_tokens(&plan),
        );
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
        // The only trailer is the atomic remain-on-exit (#28); env metadata
        // (`set-environment`) still runs separately so it can't fail the lock.
        assert!(!argv.iter().any(|a| a == "set-environment"));
        // A bare `;` would be eaten by the remote login shell; the separator is
        // the shell-quoted form so it reaches tmux intact.
        assert!(!argv.iter().any(|a| a == ";"), "no bare shell separator");
    }

    #[test]
    fn ssh_compose_new_session_applies_remain_on_exit_atomically() {
        // #28: remain-on-exit must land in the SAME tmux invocation as
        // new-session, via tmux's own argv command separator, so a fast-exiting
        // agent's pane is retained before it can self-destruct the session.
        let plan = worktree_plan();
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &new_session_tokens(&plan),
        );
        // remain-on-exit lands as `';' set-option -t <name> remain-on-exit on`.
        // The separator is a shell-quoted `;` (`';'`) so the remote login shell
        // hands it to tmux as a literal separator token instead of eating it as
        // a shell statement separator.
        let r = argv
            .iter()
            .position(|a| a == "remain-on-exit")
            .expect("remain-on-exit present");
        assert_eq!(
            argv[r - 4],
            "';'",
            "shell-quoted tmux separator precedes it"
        );
        assert_eq!(argv[r - 3], "set-option");
        assert_eq!(argv[r - 2], "-t");
        assert_eq!(argv[r - 1], "remora_api_fix-login");
        assert_eq!(argv[r + 1], "on");
        // remain-on-exit follows the agent command (the new-session payload),
        // so it is a trailer, not part of the launch.
        let new_session = argv
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        assert!(r > new_session, "remain-on-exit follows new-session");
    }

    #[test]
    fn ssh_compose_set_environment_quotes_logical_value() {
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &set_environment_tokens(
                "remora_api_fix-login",
                "REMORA_WORKSPACE",
                "~/.remora/worktrees/api/fix-login",
            ),
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
    fn ssh_compose_has_session_targets_the_name() {
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &has_session_tokens("remora_api_fix-login"),
        );
        let h = argv
            .iter()
            .position(|a| a == "has-session")
            .expect("has-session");
        assert_eq!(argv[h + 1], "-t");
        assert_eq!(argv[h + 2], "remora_api_fix-login");
    }

    #[test]
    fn ssh_compose_list_sessions_quotes_the_format_string() {
        let argv = ssh_compose(&host("devbox", None, None), false, &list_sessions_tokens());
        // The whole `-F` format (`#{session_name}` + the inline `#{E:}` metadata
        // fields, #108) MUST be shell-quoted: a bare `#` starts a comment in the
        // remote login shell and would swallow the format argument.
        let format = argv.last().expect("format arg");
        assert!(
            format.starts_with('\''),
            "format must be shell-quoted: {format}"
        );
        assert!(format.contains("#{session_name}"), "{format}");
        assert!(format.contains("#{E:REMORA_AGENT}"), "{format}");
        let l = argv
            .iter()
            .position(|a| a == "list-sessions")
            .expect("list-sessions");
        assert_eq!(argv[l + 1], "-F");
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
        let argv = ssh_compose(
            &host("devbox", None, None),
            false,
            &new_session_tokens(&plan),
        );
        let n = argv
            .iter()
            .position(|a| a == "new-session")
            .expect("new-session");
        // Still exactly one agent-command arg after `-c <dir>` (wrapped, not
        // per-token), followed by the remain-on-exit and mouse 6-token trailers.
        assert_eq!(argv.len(), n + 7 + 6 + 6);
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

    // -----------------------------------------------------------------------
    // SshSource wiring tests (use FakeExec from remote::tests)
    // -----------------------------------------------------------------------

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

    #[tokio::test]
    async fn list_joins_live_metadata_stopped_and_filters_unconfigured() {
        // Config has project `api` (worktree, path /home/dev/api). `ghost` is NOT configured.
        let config = test_config();
        // Discovery call order: names, metadata, printf $HOME, worktree scan (#108, #124):
        //  1) list-sessions names -> api (configured) + ghost (unconfigured) +
        //     `main` & `remora__bad` (unparseable). Only api survives.
        //  2) list-sessions inline metadata -> enrichment keyed by trusted name.
        //     workspace_path is absolute so the path-anchored join can match (#124).
        //  3) printf $HOME -> "/home/dev" for A2′ primary-checkout detection.
        //  4) git worktree list for api -> realistic output: primary checkout first
        //     (as real git always emits), followed by worktree entries.
        let fake = Arc::new(FakeExec::new(vec![
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
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let metas = source.list().await.expect("list");

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
        assert_eq!(
            add_tests.workspace_path.as_deref(),
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
    async fn list_keeps_session_live_when_metadata_read_flakes() {
        // The session is in the trusted names listing, but the inline-metadata
        // read flakes (transient, or tmux < 3.0). It must still list as Live with
        // empty metadata — metadata is best-effort enrichment (#108).
        // Call order: 1) names, 2) metadata (flakes), 3) printf $HOME, 4) worktree list.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // 1) names
            Ok(FakeExec::fail("connection reset")),      // 2) metadata flakes
            Ok(FakeExec::out("/home/dev")),              // 3) printf $HOME (#124)
            Ok(FakeExec::out("")),                       // 4) worktree empty
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
        // Call order: 1) names, 2) metadata, 3) printf $HOME, 4) worktree list (FAILS).
        // The FakeExec::fail at position 4 must land on the WORKTREE LIST call —
        // not on $HOME — so decision 8 is actually exercised.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("remora_api_fix-login\n")), // 1) names
            Ok(FakeExec::out("remora_api_fix-login\tclaude\t\t\n")), // 2) metadata
            Ok(FakeExec::out("/home/dev")),              // 3) printf $HOME (#124)
            Ok(FakeExec::fail("fatal: not a git repository")), // 4) worktree list FAILS → decision 8
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let metas = source.list().await.expect("list must not fail");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].session_id.as_str(), "fix-login");
        assert_eq!(metas[0].state, SessionState::Live);
        assert!(metas.iter().all(|m| m.state == SessionState::Live));
    }

    #[tokio::test]
    async fn attach_returns_not_found_when_session_is_absent() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "can't find session",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
        let err = source.attach(&project, &session).await.expect_err("absent");
        assert!(matches!(err, SourceError::SessionNotFound { .. }), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn attach_ssh_failure_is_transport_not_not_found() {
        // An ssh/auth/network failure on has-session must surface as Transport,
        // never be misclassified as a missing session.
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "ssh: connect to host devbox port 22: Connection refused",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let err = source
            .attach(&pid("api"), &sid("fix-login"))
            .await
            .expect_err("transport failure");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn attach_opens_channel_when_session_is_live() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::ok())]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
        source.attach(&project, &session).await.expect("attach");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn respawn_creates_without_worktree_add_and_attaches() {
        let config = test_config(); // api is worktree-mode
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d preflight: dir exists
            Ok(FakeExec::ok()), // new-session ok
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
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
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d preflight: dir exists
            Ok(FakeExec::fail("duplicate session: remora_api_fix-login")),
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
        source
            .respawn(&project, &session, None)
            .await
            .expect("respawn attaches");
        assert_eq!(fake.opened.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn respawn_of_vanished_worktree_is_not_found() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(""))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
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
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "ssh: connect to host devbox port 22: Connection refused",
        ))]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("probe failed");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn respawn_generic_new_session_failure_is_transport_and_opens_no_channel() {
        let config = test_config();
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()),                      // test -d preflight: dir exists
            Ok(FakeExec::fail("no server running")), // new-session: generic failure
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("api"), sid("fix-login"));
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("generic");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(fake.opened.lock().expect("lock").len(), 0);
    }

    #[tokio::test]
    async fn respawn_of_shared_config_project_with_no_surviving_worktree_is_not_found() {
        // RED → GREEN: respawn always plans worktree mode (only worktree sessions
        // survive to be respawned). A shared-default project with no surviving
        // worktree dir maps to SessionNotFound via the `test -d` preflight, NOT
        // NotWorktreeProject (which was the pre-fix behavior that wrongly blocked
        // every respawn on shared-default projects).
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
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::fail("")), // test -d: worktree dir gone → SessionNotFound
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("scratch"), sid("s1"));
        let err = source
            .respawn(&project, &session, None)
            .await
            .expect_err("no surviving worktree");
        assert!(
            matches!(err, SourceError::SessionNotFound { .. }),
            "expected SessionNotFound (worktree gone), got: {err}"
        );
        // The preflight `test -d` must have run.
        let calls = fake.calls.lock().expect("lock");
        assert_eq!(calls.len(), 1, "only the preflight probe runs");
        assert!(
            calls[0].iter().any(|a| a == "test") && calls[0].iter().any(|a| a == "-d"),
            "preflight must be a `test -d` probe"
        );
    }

    #[tokio::test]
    async fn respawn_of_shared_config_project_with_surviving_worktree_attaches() {
        // RED → GREEN: a worktree-override session on a shared-default project
        // must respawn successfully when the worktree dir still exists.
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
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // test -d: worktree dir exists
            Ok(FakeExec::ok()), // new-session ok
        ]));
        let source = SshSource::with_exec(host("devbox", None, None), config, fake.clone());
        let (project, session) = (pid("scratch"), sid("s1"));
        source
            .respawn(&project, &session, None)
            .await
            .expect("respawn must succeed when worktree exists on shared-config project");
        assert_eq!(
            fake.opened.lock().expect("lock").len(),
            1,
            "must open exactly one channel"
        );
    }

    #[tokio::test]
    async fn respawn_uses_the_supplied_agent_not_the_project_default() {
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
