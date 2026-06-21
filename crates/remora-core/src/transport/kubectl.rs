//! `KubectlSource` — the second transport. Builds the kubectl argv from a
//! validated `KubectlHost` and delegates the exec tail to `remote.rs`.
//!
//! Unlike ssh, kubectl runs a raw argv in the container with no implicit
//! shell, so the logical tokens are joined and run under `sh -c`. kubectl
//! globals (`--context`, `-n`) go before the `exec` subcommand (kubectl
//! convention); `-c`/`-i`/`-t` are `exec`-local and follow it.

use super::remote::{capture, open_pty, RemoteExec, RemoteOutput};
use crate::config::KubectlHost;
use crate::{SessionChannel, SourceError};

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

#[allow(dead_code)]
struct RealKubectlExec {
    host: KubectlHost,
}

#[allow(dead_code)]
impl RemoteExec for RealKubectlExec {
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError> {
        capture(&kubectl_run_argv(&self.host, remote))
    }

    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError> {
        open_pty(&kubectl_channel_argv(&self.host, remote))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KubectlHost;

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
}
