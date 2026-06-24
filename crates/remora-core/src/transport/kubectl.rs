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
    attach_channel, capture, has_session_tokens, open_pty, resolve_local_command, run_list,
    run_remove, run_respawn, run_spawn, run_stop, stderr_signals_session_absent, LocalRunner,
    RemoteExec, RemoteOutput, ShellRunner,
};
use crate::config::{Config, KubectlField, KubectlHost};
use crate::naming::tmux_session_name;
use crate::spawn_plan::plan_spawn;
use crate::{SessionChannel, SessionSource, SourceError};

/// A `KubectlHost` with every field resolved to a literal string, ready to drop
/// into the kubectl argv. Produced once per `SessionSource` method.
#[derive(Debug)]
struct ResolvedKubectlHost {
    pod: String,
    namespace: Option<String>,
    context: Option<String>,
    container: Option<String>,
}

fn resolve_field(
    name: &str,
    field: &KubectlField,
    runner: &dyn LocalRunner,
) -> Result<String, SourceError> {
    match field {
        KubectlField::Literal(v) => Ok(v.clone()),
        KubectlField::Command(c) => resolve_local_command(runner, name, c),
    }
}

fn resolve_opt(
    name: &str,
    field: Option<&KubectlField>,
    runner: &dyn LocalRunner,
) -> Result<Option<String>, SourceError> {
    field.map(|f| resolve_field(name, f, runner)).transpose()
}

/// Resolves every field once. A single `spawn` issues several kubectl exec
/// sub-commands; they must all target the SAME resolved pod, so resolution
/// happens here, once, not per sub-command.
fn resolve_host(
    host: &KubectlHost,
    runner: &dyn LocalRunner,
) -> Result<ResolvedKubectlHost, SourceError> {
    Ok(ResolvedKubectlHost {
        pod: resolve_field("pod", &host.pod, runner)?,
        namespace: resolve_opt("namespace", host.namespace.as_ref(), runner)?,
        context: resolve_opt("context", host.context.as_ref(), runner)?,
        container: resolve_opt("container", host.container.as_ref(), runner)?,
    })
}

/// `kubectl [--context X] [-n NS] exec [-c C] [-i -t] <pod> --` — the
/// connection prefix. Globals precede `exec`; exec-local flags follow it.
fn kubectl_base_argv(host: &ResolvedKubectlHost, interactive: bool) -> Vec<String> {
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
fn kubectl_run_argv(host: &ResolvedKubectlHost, tokens: &[String]) -> Vec<String> {
    let mut argv = kubectl_base_argv(host, false);
    argv.push("sh".into());
    argv.push("-c".into());
    argv.push(tokens.join(" "));
    argv
}

/// Interactive: same join, but prefix `env TERM=xterm-256color` inside the
/// container so the pod's tmux deterministically sees xterm-256color (kubectl
/// does not reliably forward the client TERM into the pod PTY — Finding 9).
fn kubectl_channel_argv(host: &ResolvedKubectlHost, tokens: &[String]) -> Vec<String> {
    let mut argv = kubectl_base_argv(host, true);
    argv.push("sh".into());
    argv.push("-c".into());
    argv.push(format!("env TERM=xterm-256color {}", tokens.join(" ")));
    argv
}

struct RealKubectlExec {
    host: ResolvedKubectlHost,
}

impl RemoteExec for RealKubectlExec {
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError> {
        capture(&kubectl_run_argv(&self.host, remote))
    }

    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError> {
        open_pty(&kubectl_channel_argv(&self.host, remote))
    }
}

/// Pod-requirements preflight (Finding 7). POSIX `command -v` only checks its
/// first argument, so we loop and fail closed on the first missing binary,
/// echoing its name so the error can name it. Only `spawn` probes — respawn
/// reuses an existing (already-probed) pod + worktree.
fn probe_pod(exec: &dyn RemoteExec) -> Result<(), SourceError> {
    // One shell command, run in-container via `sh -c`. Single token so the
    // RemoteExec space-join is a no-op.
    let probe = "for b in sh tmux git; do \
                 command -v \"$b\" >/dev/null 2>&1 || { echo \"$b\"; exit 1; }; done";
    let out = exec.run(&[probe.to_string()])?;
    if out.success {
        return Ok(());
    }
    // The loop echoes the missing binary name to stdout and exits 1. But a
    // kubectl-level failure (auth, RBAC, API server unreachable, missing pod)
    // fails *before* the loop runs: stdout is empty and stderr carries the real
    // cause. Only a non-empty stdout is a genuine missing-binary report; an
    // empty stdout means the probe itself never ran, so surface the transport
    // stderr instead of masking it as a phantom missing binary.
    let missing = out.stdout.trim();
    if missing.is_empty() {
        Err(SourceError::Transport(out.stderr.trim().to_string()))
    } else {
        Err(SourceError::Transport(format!(
            "kubectl pod missing required binary `{missing}` (need sh, tmux, git)"
        )))
    }
}

/// How a method turns a resolved host into a remote exec. `Real` builds the
/// kubectl exec; `Fake` (tests) ignores the resolved host and uses an injected
/// exec — resolution still runs, so the resolve->build wiring is exercised.
#[derive(Clone)]
enum ExecFactory {
    Real,
    // Constructed only in #[cfg(test)] via `with_exec`; the non-test build
    // sees it as dead but it is the primary injection point for unit tests.
    #[cfg_attr(not(test), allow(dead_code))]
    Fake(Arc<dyn RemoteExec>),
}

/// One instance = one configured kubectl host (unresolved).
pub struct KubectlSource {
    config: Arc<Config>,
    host: KubectlHost,
    runner: Arc<dyn LocalRunner>,
    exec_factory: ExecFactory,
}

impl KubectlSource {
    /// Wraps a configured kubectl host as a transport.
    pub fn new(host: KubectlHost, config: Arc<Config>) -> Self {
        Self {
            config,
            host,
            runner: Arc::new(ShellRunner::new()),
            exec_factory: ExecFactory::Real,
        }
    }

    #[cfg(test)]
    fn with_exec(host: KubectlHost, config: Arc<Config>, exec: Arc<dyn RemoteExec>) -> Self {
        Self {
            config,
            host,
            runner: Arc::new(ShellRunner::new()),
            exec_factory: ExecFactory::Fake(exec),
        }
    }
}

/// Resolves the host (running local commands) then materializes the exec.
fn build_exec(
    host: &KubectlHost,
    runner: &dyn LocalRunner,
    factory: &ExecFactory,
) -> Result<Arc<dyn RemoteExec>, SourceError> {
    let resolved = resolve_host(host, runner)?;
    Ok(match factory {
        ExecFactory::Real => Arc::new(RealKubectlExec { host: resolved }),
        ExecFactory::Fake(exec) => Arc::clone(exec),
    })
}

#[async_trait]
impl SessionSource for KubectlSource {
    async fn spawn(&self, spec: SpawnSpec) -> Result<SessionChannel, SourceError> {
        let plan = plan_spawn(&self.config, &spec)?;
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
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
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        let project_id = project_id.clone();
        let session_id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
            let out = exec.run(&has_session_tokens(&tmux_name))?;
            if !out.success {
                // A non-zero `has-session` only means "absent" for a known tmux
                // no-such-session stderr; a kubectl/auth/network failure also
                // exits non-zero and must surface as Transport, not as a
                // misleading SessionNotFound (mirrors the run_spawn cleanup gate).
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

    async fn list(&self) -> Result<Vec<SessionMeta>, SourceError> {
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        let config = Arc::clone(&self.config);
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
            run_list(exec.as_ref(), &config)
        })
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
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
            run_respawn(exec.as_ref(), &plan)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("respawn task: {e}")))?
    }

    async fn stop(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<(), SourceError> {
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        let config = Arc::clone(&self.config);
        let (p, s) = (project_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
            run_stop(exec.as_ref(), &config, &p, &s)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("stop task: {e}")))?
    }

    async fn remove(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        force: bool,
    ) -> Result<(), SourceError> {
        let host = self.host.clone();
        let runner = Arc::clone(&self.runner);
        let factory = self.exec_factory.clone();
        let config = Arc::clone(&self.config);
        let (p, s) = (project_id.clone(), session_id.clone());
        tokio::task::spawn_blocking(move || {
            let exec = build_exec(&host, runner.as_ref(), &factory)?;
            run_remove(exec.as_ref(), &config, &p, &s, force)
        })
        .await
        .map_err(|e| SourceError::Transport(format!("remove task: {e}")))?
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

        fn out(stdout: &str) -> RemoteOutput {
            RemoteOutput {
                success: true,
                stdout: stdout.into(),
                stderr: String::new(),
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
    ) -> ResolvedKubectlHost {
        ResolvedKubectlHost {
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
        // The probe is a single loop token — assert the real payload was sent.
        let call = &fake.calls.lock().expect("lock")[0];
        assert_eq!(call.len(), 1, "probe must be a single token");
        let token = &call[0];
        assert!(token.contains("for b in"), "got: {token}");
        assert!(token.contains("tmux"), "got: {token}");
        assert!(token.contains("git"), "got: {token}");
    }

    #[test]
    fn probe_pod_missing_binary_is_clear_transport_error() {
        // The loop echoes the missing binary name to stdout and exits 1.
        let fake = FakeExec::new(vec![Ok(RemoteOutput {
            success: false,
            stdout: "tmux\n".into(),
            stderr: String::new(),
        })]);
        let err = probe_pod(&fake).expect_err("missing tmux");
        match err {
            SourceError::Transport(msg) => {
                assert!(
                    msg.contains("tmux") && msg.contains("required"),
                    "got: {msg}"
                )
            }
            other => panic!("expected Transport, got {other}"),
        }
    }

    #[test]
    fn probe_pod_kubectl_failure_surfaces_stderr_not_phantom_binary() {
        // kubectl exec fails before the probe loop runs (auth/RBAC/API/pod):
        // stdout is empty, stderr carries the cause. The error must preserve
        // that cause, never a misleading missing-binary message.
        let fake = FakeExec::new(vec![Ok(RemoteOutput {
            success: false,
            stdout: String::new(),
            stderr: "Error from server (Forbidden): pods \"sandbox-0\" is forbidden".into(),
        })]);
        let err = probe_pod(&fake).expect_err("kubectl forbidden");
        match err {
            SourceError::Transport(msg) => {
                assert!(msg.contains("Forbidden"), "got: {msg}");
                assert!(!msg.contains("missing required binary"), "got: {msg}");
            }
            other => panic!("expected Transport, got {other}"),
        }
    }

    #[test]
    fn probe_loop_fails_closed_on_missing_binary() {
        // Real local sh: the loop must exit non-zero and name the absent binary,
        // independent of FakeExec. Proves the shell construct itself works.
        let snippet = "for b in sh remora-not-a-real-binary-zzz; do \
                       command -v \"$b\" >/dev/null 2>&1 || { echo \"$b\"; exit 1; }; done";
        let out = crate::transport::remote::capture(&["sh".into(), "-c".into(), snippet.into()])
            .expect("capture runs");
        assert!(!out.success, "loop must fail closed on a missing binary");
        assert_eq!(out.stdout.trim(), "remora-not-a-real-binary-zzz");
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
            pod: KubectlField::Literal("sandbox-0".into()),
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
        // First call is the probe (single loop token), second is the worktree add.
        let calls = fake.calls.lock().expect("lock");
        assert!(
            calls[0].len() == 1 && calls[0][0].contains("for b in"),
            "first call must be the probe loop token"
        );
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
            pod: KubectlField::Literal("sandbox-0".into()),
            namespace: None,
            context: None,
            container: None,
        };
        let source = KubectlSource::with_exec(kh, test_config(), fake.clone());
        let err = source.list().await.expect_err("connection error");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
    }

    #[tokio::test]
    async fn attach_absent_session_is_not_found() {
        // tmux's own "can't find session" stderr → SessionNotFound.
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "can't find session: remora_api_fix-login",
        ))]));
        let source = KubectlSource::with_exec(kubectl_host(), test_config(), fake.clone());
        let err = source
            .attach(&pid("api"), &sid("fix-login"))
            .await
            .expect_err("absent");
        assert!(matches!(err, SourceError::SessionNotFound { .. }), "{err}");
        assert_eq!(*fake.opened.lock().expect("lock"), 0);
    }

    #[tokio::test]
    async fn attach_kubectl_failure_is_transport_not_not_found() {
        // A kubectl/auth/API failure on has-session must surface as Transport,
        // never be misclassified as a missing session.
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::fail(
            "Unable to connect to the server: dial tcp: lookup api timed out",
        ))]));
        let source = KubectlSource::with_exec(kubectl_host(), test_config(), fake.clone());
        let err = source
            .attach(&pid("api"), &sid("fix-login"))
            .await
            .expect_err("transport failure");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(*fake.opened.lock().expect("lock"), 0);
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

    // -----------------------------------------------------------------------
    // stop/remove delegation smoke tests: prove KubectlSource wires through
    // to run_stop / run_remove the same way SshSource does (parallel pattern).
    // -----------------------------------------------------------------------

    fn kubectl_host() -> KubectlHost {
        KubectlHost {
            pod: KubectlField::Literal("sandbox-0".into()),
            namespace: None,
            context: None,
            container: None,
        }
    }

    fn pid(s: &str) -> ProjectId {
        ProjectId::new(s).expect("slug")
    }

    fn sid(s: &str) -> SessionId {
        SessionId::new(s).expect("slug")
    }

    #[tokio::test]
    async fn stop_delegates_to_run_stop_via_spawn_blocking() {
        // kill-session succeeds → stop is Ok.
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::ok())]));
        let source = KubectlSource::with_exec(kubectl_host(), test_config(), fake.clone());
        source
            .stop(&pid("api"), &sid("fix-login"))
            .await
            .expect("stop");
        let calls = fake.calls.lock().expect("lock");
        assert!(
            calls.iter().any(|c| c.iter().any(|a| a == "kill-session")),
            "stop must issue kill-session; got: {calls:?}"
        );
        // No worktree-remove or branch-delete (stop is tmux-only).
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "remove")),
            "stop must not touch the worktree"
        );
    }

    #[tokio::test]
    async fn remove_delegates_to_run_remove_via_spawn_blocking() {
        // For a worktree project: probe (clean) → kill → worktree remove → branch -D.
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::out("")),    // status --porcelain (clean)
            Ok(FakeExec::out("0\n")), // rev-list (on remote)
            Ok(FakeExec::ok()),       // kill-session
            Ok(FakeExec::ok()),       // worktree remove
            Ok(FakeExec::ok()),       // branch -D
        ]));
        let source = KubectlSource::with_exec(kubectl_host(), test_config(), fake.clone());
        source
            .remove(&pid("api"), &sid("fix-login"), false)
            .await
            .expect("remove");
        let calls = fake.calls.lock().expect("lock");
        assert!(calls.iter().any(|c| c.iter().any(|a| a == "kill-session")));
        assert!(calls.iter().any(|c| c.iter().any(|a| a == "remove")));
        assert!(calls.iter().any(|c| c.iter().any(|a| a == "-D")));
    }

    // -----------------------------------------------------------------------
    // New Task 4 tests: resolve_host wiring
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_resolves_command_form_pod_through_real_runner() {
        // pod is a COMMAND; resolution runs the real local runner (`echo`), and the
        // remote kubectl exec is faked. Proves resolve->build runs per method.
        let fake = Arc::new(FakeExec::new(vec![
            Ok(FakeExec::ok()), // probe
            Ok(FakeExec::ok()), // worktree add
            Ok(FakeExec::ok()), // new-session
        ]));
        let kh = KubectlHost {
            pod: KubectlField::Command("echo sandbox-9".into()),
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
        source.spawn(spec).await.expect("spawn resolves + attaches");
        assert_eq!(*fake.opened.lock().expect("lock"), 1);
    }

    #[tokio::test]
    async fn spawn_aborts_when_pod_command_fails() {
        let fake = Arc::new(FakeExec::new(vec![Ok(FakeExec::ok())]));
        let kh = KubectlHost {
            pod: KubectlField::Command("exit 1".into()),
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
        let err = source.spawn(spec).await.expect_err("resolution fails");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
        assert_eq!(
            *fake.opened.lock().expect("lock"),
            0,
            "no exec after a failed resolve"
        );
    }

    #[test]
    fn resolve_host_maps_literal_and_command() {
        let runner = crate::transport::remote::ShellRunner::new();
        let kh = KubectlHost {
            pod: KubectlField::Command("echo p0".into()),
            namespace: Some(KubectlField::Literal("ns".into())),
            context: None,
            container: Some(KubectlField::Command("echo c0".into())),
        };
        let resolved = resolve_host(&kh, &runner).expect("resolve");
        assert_eq!(resolved.pod, "p0");
        assert_eq!(resolved.namespace.as_deref(), Some("ns"));
        assert_eq!(resolved.context, None);
        assert_eq!(resolved.container.as_deref(), Some("c0"));
    }

    #[test]
    fn resolve_host_aborts_when_a_later_field_command_fails() {
        // A later-field command failure (here `namespace`) must abort the whole
        // resolve, not silently drop the field.
        let runner = crate::transport::remote::ShellRunner::new();
        let kh = KubectlHost {
            pod: KubectlField::Command("echo p0".into()),
            namespace: Some(KubectlField::Command("exit 1".into())),
            context: None,
            container: None,
        };
        let err = resolve_host(&kh, &runner).expect_err("later-field failure aborts resolve");
        assert!(matches!(err, SourceError::Transport(_)), "{err}");
    }
}
