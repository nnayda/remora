//! End-to-end tests against a REAL kubectl-reachable pod + tmux. Ignored by
//! default — they need a cluster. Run by hand:
//!
//! ```sh
//! REMORA_E2E_KUBECTL_POD=sandbox-0 \
//! REMORA_E2E_PROJECT=demo REMORA_E2E_SESSION=one \
//!   cargo test -p remora-core --test kubectl_e2e -- --ignored --nocapture
//! ```
//! Optional: REMORA_E2E_KUBECTL_{NAMESPACE,CONTEXT,CONTAINER},
//! REMORA_E2E_PATH (shared-spawn dir, default ~/e2e),
//! REMORA_E2E_GIT_PATH (existing repo for the worktree cold-start).
//!
//! The cold-start test uses a `~/`-based worktree path on purpose: it verifies
//! the pod exec env exports HOME so `"$HOME"/…` expands (Finding 2). The pod
//! must provide sh, tmux, git, and a writable HOME.

use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use remora_core::config::{Config, KubectlField, KubectlHost};
use remora_core::{
    ChannelOutput, KubectlSource, ProjectId, SessionChannel, SessionId, SessionSource, TerminalSize,
};

fn e2e_host() -> KubectlHost {
    KubectlHost {
        pod: KubectlField::Literal(
            std::env::var("REMORA_E2E_KUBECTL_POD").expect("REMORA_E2E_KUBECTL_POD"),
        ),
        namespace: std::env::var("REMORA_E2E_KUBECTL_NAMESPACE")
            .ok()
            .map(KubectlField::Literal),
        context: std::env::var("REMORA_E2E_KUBECTL_CONTEXT")
            .ok()
            .map(KubectlField::Literal),
        container: std::env::var("REMORA_E2E_KUBECTL_CONTAINER")
            .ok()
            .map(KubectlField::Literal),
    }
}

/// Runs `kubectl [--context …] [-n …] exec [-c …] <pod> -- <args…>` directly
/// against the E2E pod, using the SAME targeting options as `e2e_host()` so the
/// out-of-band setup commands can't silently hit a different pod than the
/// transport under test in a non-default cluster. Asserts the command exited
/// zero (not merely that the process spawned) so a failed kill/rm fails the
/// test loudly instead of corrupting later assertions.
fn kubectl_exec_in_pod(args: &[&str]) {
    let host = e2e_host();
    let mut argv: Vec<String> = Vec::new();
    if let Some(KubectlField::Literal(ctx)) = &host.context {
        argv.push("--context".into());
        argv.push(ctx.clone());
    }
    if let Some(KubectlField::Literal(ns)) = &host.namespace {
        argv.push("-n".into());
        argv.push(ns.clone());
    }
    argv.push("exec".into());
    if let Some(KubectlField::Literal(container)) = &host.container {
        argv.push("-c".into());
        argv.push(container.clone());
    }
    if let KubectlField::Literal(pod) = &host.pod {
        argv.push(pod.clone());
    }
    argv.push("--".into());
    argv.extend(args.iter().map(|s| (*s).to_string()));

    let out = std::process::Command::new("kubectl")
        .args(&argv)
        .output()
        .expect("spawn kubectl exec");
    assert!(
        out.status.success(),
        "kubectl exec {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Reads PTY output until `needle` appears, or panics after 10s. Total
/// deadline (not per-recv) so a steady non-matching stream still times out.
async fn recv_until_contains(channel: &mut SessionChannel, needle: &str) {
    let mut acc = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let now = Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for {needle:?}; got {}",
            String::from_utf8_lossy(&acc)
        );
        match tokio::time::timeout(deadline - now, channel.recv()).await {
            Ok(Some(ChannelOutput::Bytes(b))) => {
                acc.extend_from_slice(&b);
                if acc.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    return;
                }
            }
            Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
            Ok(None) => panic!(
                "channel closed before {needle:?}; got {}",
                String::from_utf8_lossy(&acc)
            ),
            Err(_) => panic!("timed out waiting for {needle:?}"),
        }
    }
}

#[tokio::test]
#[ignore = "needs a real kubectl-reachable pod + tmux; see module docs"]
async fn attaches_to_a_live_remote_tmux_session() {
    let project = ProjectId::new(
        std::env::var("REMORA_E2E_PROJECT")
            .as_deref()
            .unwrap_or("demo"),
    )
    .expect("valid project slug");
    let session = SessionId::new(
        std::env::var("REMORA_E2E_SESSION")
            .as_deref()
            .unwrap_or("one"),
    )
    .expect("valid session slug");

    let source = KubectlSource::new(e2e_host(), Arc::new(Config::default()));
    let mut channel = source.attach(&project, &session).await.expect("attach");

    // Drive a resize, then a shell command, and expect to see its marker in
    // the PTY output (tmux repaint + echo + command output).
    channel
        .resize(TerminalSize::new(40, 120).expect("nonzero"))
        .await
        .expect("resize");
    channel
        .send_bytes(b"echo remora-e2e-ok\n".to_vec())
        .await
        .expect("send");

    // Total deadline across all recvs, not a per-recv timeout — otherwise a
    // steady byte stream that never carries the marker resets the clock every
    // iteration and the loop could run far past 10s.
    let mut acc = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        let now = Instant::now();
        if now >= deadline {
            break false;
        }
        match tokio::time::timeout(deadline - now, channel.recv()).await {
            Ok(Some(ChannelOutput::Bytes(b))) => {
                acc.extend_from_slice(&b);
                if acc.windows(13).any(|w| w == b"remora-e2e-ok") {
                    break true;
                }
            }
            Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
            Ok(None) => break false,
            Err(_) => break false,
        }
    };
    assert!(found, "did not observe command output; got {acc:?}");
}

#[tokio::test]
#[ignore = "needs a real kubectl-reachable pod + tmux; see module docs"]
async fn e2e_spawn_shared_session_runs_and_blocks_duplicate() {
    // Note: end-to-end no-agent (command = []) plain-shell render is covered
    // by the unit test `remote.rs::new_session_tokens_no_agent_runs_a_login_shell`.
    // This suite has no fake-exec seam to capture tmux tokens, so we don't
    // duplicate that assertion here.
    let pod = match std::env::var("REMORA_E2E_KUBECTL_POD") {
        Ok(p) => p,
        Err(_) => return,
    };
    let project = std::env::var("REMORA_E2E_PROJECT").unwrap_or_else(|_| "e2e".into());
    let path = std::env::var("REMORA_E2E_PATH").unwrap_or_else(|_| "~/e2e".into());
    let session = format!("spawn-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "kubectl"
        pod = "{pod}"
        [projects.{project}]
        host = "e2e"
        path = "{path}"
        workspace = "shared"
        agent = "sh"
        [agents.sh]
        command = ["sh"]
    "#
    );
    let config = Arc::new(Config::from_toml_str(&toml).expect("e2e config"));
    let source = KubectlSource::new(e2e_host(), config);

    let spec = remora_core::SpawnSpec {
        project_id: ProjectId::new(&project).expect("slug"),
        session_id: SessionId::new(&session).expect("slug"),
        agent: None,
    };

    // First spawn succeeds; the agent (sh) is interactive in the session.
    let mut channel = source.spawn(spec.clone()).await.expect("spawn");
    channel
        .send_bytes(b"echo SPAWN_OK_$((6*7))\n".to_vec())
        .await
        .expect("send");
    recv_until_contains(&mut channel, "SPAWN_OK_42").await;

    // Second spawn of the same name is rejected (tmux new-session is the lock).
    let err = source.spawn(spec).await.expect_err("duplicate");
    assert!(
        matches!(err, remora_core::SourceError::SessionExists { .. }),
        "{err}"
    );
}

#[tokio::test]
#[ignore = "needs REMORA_E2E_KUBECTL_POD + REMORA_E2E_GIT_PATH (a git repo in the pod)"]
async fn e2e_spawn_worktree_cold_start_creates_worktree() {
    let (pod, git_path) = match (
        std::env::var("REMORA_E2E_KUBECTL_POD"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(p), Ok(g)) => (p, g),
        _ => return,
    };
    let session = format!("wt-{}", std::process::id());

    // Use a `~/`-based path (not an absolute path) on purpose: this exercises
    // HOME expansion inside the pod exec environment (Finding 2 — kubectl exec
    // does not guarantee HOME is set, so the transport must export it, and
    // using `~/…` here is the only way to verify that guarantee holds end-to-end).
    let worktree_path = format!("~/.remora-e2e-repos/{}", git_path.trim_start_matches('/'));

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "kubectl"
        pod = "{pod}"
        [projects.gitproj]
        host = "e2e"
        path = "{worktree_path}"
        workspace = "worktree"
        agent = "sh"
        [agents.sh]
        command = ["sh"]
    "#
    );
    let config = Arc::new(Config::from_toml_str(&toml).expect("e2e config"));
    let source = KubectlSource::new(e2e_host(), config);

    let spec = remora_core::SpawnSpec {
        project_id: ProjectId::new("gitproj").expect("slug"),
        session_id: SessionId::new(&session).expect("slug"),
        agent: None,
    };

    // Cold start: ~/.remora/worktrees/gitproj/ need not pre-exist; git creates it.
    let mut channel = source.spawn(spec).await.expect("worktree spawn");
    channel.send_bytes(b"pwd\n".to_vec()).await.expect("send");
    recv_until_contains(&mut channel, &format!("worktrees/gitproj/{session}")).await;
}

#[tokio::test]
#[ignore = "needs REMORA_E2E_KUBECTL_POD + REMORA_E2E_GIT_PATH (a git repo in the pod)"]
async fn e2e_discovery_stopped_then_respawn_reuses_worktree() {
    let (pod, git_path) = match (
        std::env::var("REMORA_E2E_KUBECTL_POD"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(p), Ok(g)) => (p, g),
        _ => return,
    };
    let session = format!("disc-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "kubectl"
        pod = "{pod}"
        [projects.gitproj]
        host = "e2e"
        path = "{git_path}"
        workspace = "worktree"
        agent = "sh"
        [agents.sh]
        command = ["sh"]
    "#
    );
    let config = Arc::new(Config::from_toml_str(&toml).expect("e2e config"));
    let source = KubectlSource::new(e2e_host(), config);
    let project = ProjectId::new("gitproj").expect("slug");
    let session_id = SessionId::new(&session).expect("slug");

    // Spawn a worktree session and write a marker file into the worktree.
    let spec = remora_core::SpawnSpec {
        project_id: project.clone(),
        session_id: session_id.clone(),
        agent: None,
    };
    let mut channel = source.spawn(spec).await.expect("spawn");
    let marker = format!("REMORA_MARKER_{session}");
    channel
        .send_bytes(format!("echo present > {marker}\n").into_bytes())
        .await
        .expect("send");
    // Round-trip a command so the file write has landed before we kill tmux.
    channel
        .send_bytes(b"echo SYNC_$((1+1))\n".to_vec())
        .await
        .expect("send");
    recv_until_contains(&mut channel, "SYNC_2").await;
    drop(channel);

    // Kill the tmux session (the worktree survives) -> discovery must report
    // Stopped. Use kubectl exec to reach the pod directly.
    let target = format!("remora_gitproj_{session}");
    kubectl_exec_in_pod(&["tmux", "kill-session", "-t", &target]);

    let listed = source.list().await.expect("list");
    let me = listed
        .iter()
        .find(|m| m.session_id.as_str() == session)
        .expect("session present after kill");
    assert_eq!(
        me.state,
        remora_core::SessionState::Stopped,
        "should be stopped"
    );

    // Respawn -> Live again, and the marker file (in-progress work) survived.
    let mut channel = source
        .respawn(&project, &session_id, None)
        .await
        .expect("respawn");
    channel
        .send_bytes(format!("cat {marker}\n").into_bytes())
        .await
        .expect("send");
    recv_until_contains(&mut channel, "present").await;

    let listed = source.list().await.expect("list");
    let me = listed
        .iter()
        .find(|m| m.session_id.as_str() == session)
        .expect("session present after respawn");
    assert_eq!(
        me.state,
        remora_core::SessionState::Live,
        "should be live again"
    );
}

#[tokio::test]
#[ignore = "needs REMORA_E2E_KUBECTL_POD + REMORA_E2E_GIT_PATH (a git repo in the pod)"]
async fn e2e_respawn_of_vanished_worktree_is_not_found() {
    let (pod, git_path) = match (
        std::env::var("REMORA_E2E_KUBECTL_POD"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(p), Ok(g)) => (p, g),
        _ => return,
    };
    let session = format!("vanish-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "kubectl"
        pod = "{pod}"
        [projects.gitproj]
        host = "e2e"
        path = "{git_path}"
        workspace = "worktree"
        agent = "sh"
        [agents.sh]
        command = ["sh"]
    "#
    );
    let config = Arc::new(Config::from_toml_str(&toml).expect("e2e config"));
    let source = KubectlSource::new(e2e_host(), config);
    let project = ProjectId::new("gitproj").expect("slug");
    let session_id = SessionId::new(&session).expect("slug");

    // Spawn a worktree session, then sync so the worktree has landed.
    let spec = remora_core::SpawnSpec {
        project_id: project.clone(),
        session_id: session_id.clone(),
        agent: None,
    };
    let mut channel = source.spawn(spec).await.expect("spawn");
    channel
        .send_bytes(b"echo SYNC_$((2+2))\n".to_vec())
        .await
        .expect("send");
    recv_until_contains(&mut channel, "SYNC_4").await;
    drop(channel);

    // Kill the tmux session AND remove the worktree directory (the dangerous
    // case: the git admin entry survives a bare `rm -rf`, so discovery still
    // lists it, but the directory is gone). Respawn must fail closed with
    // SessionNotFound rather than spawning into a vanished dir.
    let target = format!("remora_gitproj_{session}");
    kubectl_exec_in_pod(&["tmux", "kill-session", "-t", &target]);
    let rm = format!("rm -rf $HOME/.remora/worktrees/gitproj/{session}");
    kubectl_exec_in_pod(&["sh", "-c", &rm]);

    let err = source
        .respawn(&project, &session_id, None)
        .await
        .expect_err("vanished worktree");
    assert!(
        matches!(err, remora_core::SourceError::SessionNotFound { .. }),
        "vanished worktree must be SessionNotFound, got {err}"
    );
}
