//! `SshSource` — the first real transport. Builds the ssh argv from a
//! validated `SshHost` and delegates to the PTY-process bridge.

use async_trait::async_trait;
use portable_pty::CommandBuilder;
use remora_protocol::{ProjectId, SessionId, SessionMeta, SpawnSpec};

use super::pty_process::spawn_pty_channel;
use crate::config::SshHost;
use crate::naming::tmux_session_name;
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

/// One instance = one configured ssh host (matches the `SessionSource`
/// trait doc).
pub struct SshSource {
    host: SshHost,
}

impl SshSource {
    /// Wraps a configured ssh host as a transport.
    pub fn new(host: SshHost) -> Self {
        Self { host }
    }
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
    let mut argv: Vec<String> = vec![
        "ssh".into(),
        "-tt".into(),
        // Detect a half-open connection (laptop sleep / wifi drop) in ~45s
        // so the local ssh exits and the channel reports death promptly.
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(port) = host.port {
        argv.push("-p".into());
        argv.push(port.to_string());
    }
    if let Some(user) = &host.user {
        argv.push("-l".into());
        argv.push(user.clone());
    }
    argv.push(host.host.clone());
    // `-d` evicts every other client on attach (sequential-handoff model;
    // co-view is a post-MVP non-goal — see the design spec).
    argv.push("tmux".into());
    argv.push("attach-session".into());
    argv.push("-d".into());
    argv.push("-t".into());
    argv.push(tmux_name.into());
    argv
}

/// Turns a pure argv into a `CommandBuilder` (program = argv[0]).
///
/// Precondition: `argv` is non-empty. The only caller feeds it
/// [`attach_argv`], which always yields the `ssh` program plus its args.
fn command_from_argv(argv: &[String]) -> CommandBuilder {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

#[async_trait]
impl SessionSource for SshSource {
    /// Not implemented until stage 5 (worktree + branch + tmux new-session +
    /// agent launch). Stubbed rather than `unimplemented!` so a caller gets
    /// a clean error, not a panic.
    async fn spawn(&self, _spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        Err(SourceError::Transport(
            "ssh spawn: not implemented (stage 5)".into(),
        ))
    }

    /// Opens a channel to an existing tmux session over ssh.
    ///
    /// NOTE: stage-4 optimistic attach. Unlike the `SessionSource::attach`
    /// contract, a missing/stopped session is NOT reported as
    /// `SessionNotFound` — it surfaces as tmux error bytes then channel
    /// death. Liveness-checked `SessionNotFound` lands with discovery.
    // TODO(stage 6): preflight liveness -> SessionNotFound.
    async fn attach(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionChannel, SourceError> {
        let tmux_name = tmux_session_name(project_id, session_id);
        let argv = attach_argv(&self.host, &tmux_name);
        let cmd = command_from_argv(&argv);
        // Run the blocking PTY setup (openpty + ssh fork/exec) off the
        // runtime (ADR-0005: nothing in core blocks the runtime).
        tokio::task::spawn_blocking(move || spawn_pty_channel(cmd))
            .await
            .map_err(|e| SourceError::Transport(format!("pty setup task: {e}")))?
    }

    /// Not implemented until stage 6 (discovery lists tmux sessions and
    /// parses the `remora_<p>_<s>` names back to ids).
    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        Err(SourceError::Transport(
            "ssh discovery: not implemented (stage 6)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionSource;

    fn host(host: &str, user: Option<&str>, port: Option<u16>) -> SshHost {
        SshHost {
            host: host.to_string(),
            user: user.map(String::from),
            port,
        }
    }

    fn spec() -> SpawnSpec {
        use remora_protocol::AgentId;
        SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            agent: Some(AgentId::new("claude").expect("slug")),
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
    async fn spawn_is_stubbed_with_its_stage() {
        let source = SshSource::new(host("devbox", None, None));
        let err = source.spawn(spec()).await.expect_err("stubbed");
        assert!(matches!(err, SourceError::Transport(_)));
        assert!(err.to_string().contains("stage 5"), "{err}");
    }

    #[tokio::test]
    async fn list_is_stubbed_with_its_stage() {
        let source = SshSource::new(host("devbox", None, None));
        let err = source.list().await.expect_err("stubbed");
        assert!(matches!(err, SourceError::Transport(_)));
        assert!(err.to_string().contains("stage 6"), "{err}");
    }

    #[tokio::test]
    async fn usable_through_dyn_session_source() {
        let source: Box<dyn SessionSource> = Box::new(SshSource::new(host("devbox", None, None)));
        // spawn is stubbed, but the call must dispatch through the trait
        // object — this pins object-safety for the relay seam.
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
    }
}
