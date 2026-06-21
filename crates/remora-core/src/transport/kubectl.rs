//! `KubectlSource` — the second transport. Builds the kubectl argv from a
//! validated `KubectlHost` and delegates the exec tail to `remote.rs`.
//!
//! Unlike ssh, kubectl runs a raw argv in the container with no implicit
//! shell, so the logical tokens are joined and run under `sh -c`. kubectl
//! globals (`--context`, `-n`) go before the `exec` subcommand (kubectl
//! convention); `-c`/`-i`/`-t` are `exec`-local and follow it.

use std::sync::Arc;

use async_trait::async_trait;
use remora_protocol::{ProjectId, SessionId, SessionMeta, SpawnSpec};

use super::remote::{
    attach_channel, capture, has_session_tokens, open_pty, run_list, run_respawn, run_spawn,
    RemoteExec, RemoteOutput,
};
use crate::config::{Config, KubectlHost};
use crate::naming::tmux_session_name;
use crate::spawn_plan::plan_spawn;
use crate::{SessionChannel, SessionSource, SourceError};

/// `kubectl [--context X] [-n NS] exec [-c C] [-i -t] <pod> --` — the
/// connection prefix. Globals precede `exec`; exec-local flags follow it.
fn kubectl_base_argv(host: &KubectlHost, interactive: bool) -> Vec<String> {
    let mut argv: Vec<String> = vec!["kubectl".into()];
    if let Some(ctx) = &host.context {
        argv.push("--context".into());
        argv.push(ctx.clone());
    }
    if let Some(ns) = &host.namespace {
        argv.push("-n".into());
        argv.push(ns.clone());
    }
    argv.push("exec".into());
    if let Some(container) = &host.container {
        argv.push("-c".into());
        argv.push(container.clone());
    }
    if interactive {
        argv.push("-i".into());
        argv.push("-t".into());
    }
    argv.push(host.pod.clone());
    argv.push("--".into());
    argv
}

/// Non-interactive: join the logical tokens into one `sh -c` string. No
/// `--request-timeout` — for `kubectl exec` the streamed command is one long
/// API request, so a connect-style timeout would sever a legitimately-slow
/// `git worktree add` (Finding 1). Execution is unbounded, matching ssh.
fn kubectl_run_argv(host: &KubectlHost, tokens: &[String]) -> Vec<String> {
    let mut argv = kubectl_base_argv(host, false);
    argv.push("sh".into());
    argv.push("-c".into());
    argv.push(tokens.join(" "));
    argv
}

/// Interactive: same join, but prefix `env TERM=xterm-256color` inside the
/// container so the pod's tmux deterministically sees xterm-256color (kubectl
/// does not reliably forward the client TERM into the pod PTY — Finding 9).
fn kubectl_channel_argv(host: &KubectlHost, tokens: &[String]) -> Vec<String> {
    let mut argv = kubectl_base_argv(host, true);
    argv.push("sh".into());
    argv.push("-c".into());
    argv.push(format!("env TERM=xterm-256color {}", tokens.join(" ")));
    argv
}

struct RealKubectlExec {
    host: KubectlHost,
}

impl RemoteExec for RealKubectlExec {
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError> {
        capture(&kubectl_run_argv(&self.host, remote))
    }

    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError> {
        open_pty(&kubectl_channel_argv(&self.host, remote))
    }
}

/// Pod-requirements preflight (Finding 7). Runs `command -v sh tmux git`;
/// a missing binary surfaces as a clear error instead of a downstream
/// `command not found`. Only the spawn path probes — respawn reuses an
/// existing worktree and surfaces a missing binary the same way ssh does.
fn probe_pod(exec: &dyn RemoteExec) -> Result<(), SourceError> {
    let out = exec.run(&[
        "command".into(),
        "-v".into(),
        "sh".into(),
        "tmux".into(),
        "git".into(),
    ])?;
    if out.success {
        Ok(())
    } else {
        Err(SourceError::Transport(format!(
            "kubectl pod missing a required binary (need sh, tmux, git): {}",
            out.stderr.trim()
        )))
    }
}

/// One instance = one configured kubectl host.
pub struct KubectlSource {
    config: Arc<Config>,
    exec: Arc<dyn RemoteExec>,
}

impl KubectlSource {
    /// Wraps a configured kubectl host as a transport.
    pub fn new(host: KubectlHost, config: Arc<Config>) -> Self {
        Self {
            config,
            exec: Arc::new(RealKubectlExec { host }),
        }
    }

    #[cfg(test)]
    fn with_exec(_host: KubectlHost, config: Arc<Config>, exec: Arc<dyn RemoteExec>) -> Self {
        Self { config, exec }
    }
}

#[async_trait]
impl SessionSource for KubectlSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        tokio::task::spawn_blocking(move || {
            probe_pod(exec.as_ref())?;
            run_spawn(exec.as_ref(), &plan)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("spawn task: {e}")))?
    }

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
                return Err(SourceError::SessionNotFound {
                    project_id,
                    session_id,
                });
            }
            attach_channel(exec.as_ref(), &tmux_name)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("attach task: {e}")))?
    }

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
        let spec = SpawnSpec {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            agent,
        };
        let plan = plan_spawn(&self.config, &spec)?;
        let exec = Arc::clone(&self.exec);
        tokio::task::spawn_blocking(move || run_respawn(exec.as_ref(), &plan))
            .await
            .map_err(|e| SourceError::Transport(format!("respawn task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, KubectlHost, WorkspaceMode};

    struct FakeExec {
        results: std::sync::Mutex<std::collections::VecDeque<Result<RemoteOutput, SourceError>>>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        opened: std::sync::Mutex<usize>,
    }

    impl FakeExec {
        fn new(results: Vec<Result<RemoteOutput, SourceError>>) -> Self {
            Self {
                results: std::sync::Mutex::new(results.into_iter().collect()),
                calls: std::sync::Mutex::new(vec![]),
                opened: std::sync::Mutex::new(0),
            }
        }
        fn ok() -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: String::new(),
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
    }

    impl RemoteExec for FakeExec {
        fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError> {
            self.calls.lock().expect("lock").push(remote.to_vec());
            self.results
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| Ok(FakeExec::ok()))
        }
        fn open_channel(&self, _remote: &[String]) -> Result<SessionChannel, SourceError> {
            *self.opened.lock().expect("lock") += 1;
            let (channel, _rx, _tx) = SessionChannel::pair();
            Ok(channel)
        }
    }

    fn host(
        pod: &str,
        ns: Option<&str>,
        ctx: Option<&str>,
        container: Option<&str>,
    ) -> KubectlHost {
        KubectlHost {
            pod: pod.into(),
            namespace: ns.map(String::from),
            context: ctx.map(String::from),
            container: container.map(String::from),
        }
    }

    fn test_config() -> Arc<Config> {
        let toml = r#"
            [hosts.k8s]
            transport = "kubectl"
            pod = "sandbox-0"
            [projects.api]
            host = "k8s"
            path = "/home/dev/api"
            workspace = "worktree"
            agent = "claude"
            [agents.claude]
            command = ["claude"]
        "#;
        Arc::new(Config::from_toml_str(toml).expect("config"))
    }

    #[test]
    fn base_argv_puts_globals_before_exec_and_locals_after() {
        let argv = kubectl_base_argv(
            &host("sandbox-0", Some("agents"), Some("staging"), Some("main")),
            true,
        );
        // kubectl --context staging -n agents exec -c main -i -t sandbox-0 --
        assert_eq!(
            argv,
            vec![
                "kubectl",
                "--context",
                "staging",
                "-n",
                "agents",
                "exec",
                "-c",
                "main",
                "-i",
                "-t",
                "sandbox-0",
                "--",
            ]
        );
    }

    #[test]
    fn base_argv_omits_absent_optionals_and_interactive_flags() {
        let argv = kubectl_base_argv(&host("sandbox-0", None, None, None), false);
        // kubectl exec sandbox-0 --   (no globals, no -c, no -i -t)
        assert_eq!(argv, vec!["kubectl", "exec", "sandbox-0", "--"]);
    }

    #[test]
    fn run_argv_joins_tokens_under_sh_c_without_request_timeout() {
        let tokens = vec![
            "tmux".to_string(),
            "has-session".to_string(),
            "-t".to_string(),
            "remora_api_x".to_string(),
        ];
        let argv = kubectl_run_argv(&host("p", None, None, None), &tokens);
        assert_eq!(
            argv,
            vec![
                "kubectl",
                "exec",
                "p",
                "--",
                "sh",
                "-c",
                "tmux has-session -t remora_api_x",
            ]
        );
        assert!(
            !argv.iter().any(|a| a.contains("request-timeout")),
            "no execution timeout (Finding 1)"
        );
    }

    #[test]
    fn channel_argv_wraps_term_in_container_and_is_interactive() {
        let tokens = vec!["tmux".to_string(), "attach-session".to_string()];
        let argv = kubectl_channel_argv(&host("p", None, None, None), &tokens);
        assert_eq!(
            argv,
            vec![
                "kubectl",
                "exec",
                "-i",
                "-t",
                "p",
                "--",
                "sh",
                "-c",
                "env TERM=xterm-256color tmux attach-session",
            ]
        );
    }

    #[test]
    fn probe_pod_ok_when_binaries_present() {
        let fake = FakeExec::new(vec![Ok(FakeExec::ok())]);
        assert!(probe_pod(&fake).is_ok());
        // The probe runs `command -v sh tmux git`.
        let call = &fake.calls.lock().expect("lock")[0];
        assert!(
            call.iter()
                .any(|a| a.contains("command -v") || a == "command")
                && call.iter().any(|a| a.contains("tmux"))
        );
    }

    #[test]
    fn probe_pod_missing_binary_is_clear_transport_error() {
        let fake = FakeExec::new(vec![Ok(FakeExec::fail("sh: tmux: not found"))]);
        let err = probe_pod(&fake).expect_err("missing tmux");
        match err {
            SourceError::Transport(msg) => {
                assert!(
                    msg.contains("pod") && msg.contains("required"),
                    "got: {msg}"
                )
            }
            other => panic!("expected Transport, got {other}"),
        }
    }

    #[tokio::test]
    async fn spawn_through_fake_exec_probes_then_attaches() {
        // probe ok, worktree add ok, new-session ok, 3x set-env ok -> attach.
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // probe
            Ok(FakeExec::ok()), // worktree add
            Ok(FakeExec::ok()), // new-session
        ]));
        let kh = KubectlHost {
            pod: "sandbox-0".into(),
            namespace: None,
            context: None,
            container: None,
        };
        let source = KubectlSource::with_exec(kh, test_config(), fake.clone());
        let spec = SpawnSpec {
            project_id: ProjectId::new("api").expect("slug"),
            session_id: SessionId::new("fix-login").expect("slug"),
            agent: Some(remora_protocol::AgentId::new("claude").expect("slug")),
        };
        source.spawn(spec).await.expect("spawn");
        assert_eq!(*fake.opened.lock().expect("lock"), 1);
        // First call is the probe, second is the worktree add.
        let calls = fake.calls.lock().expect("lock");
        assert!(calls[0].iter().any(|a| a == "command"));
        assert!(calls[1].iter().any(|a| a == "worktree"));
    }

    #[tokio::test]
    async fn list_classifies_kubectl_connection_error_as_transport_not_absent() {
        // A kubectl API error on list-sessions must surface as Transport, never
        // as the empty cold-state (which is reserved for tmux "no server").
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "Unable to connect to the server: dial tcp: lookup api timed out",
        ))]));
        let kh = KubectlHost {
            pod: "sandbox-0".into(),
            namespace: None,
            context: None,
            container: None,
        };
        let source = KubectlSource::with_exec(kh, test_config(), fake.clone());
        let err = source.list().await.expect_err("connection error");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
    }

    #[test]
    fn workspace_mode_is_used_in_test_config() {
        // Sanity: the test_config is worktree mode so the spawn smoke test
        // exercises the worktree add path.
        let config = test_config();
        let pid = ProjectId::new("api").expect("slug");
        let project = config.projects.get(&pid).expect("api project");
        assert_eq!(project.workspace, WorkspaceMode::Worktree);
    }
}
