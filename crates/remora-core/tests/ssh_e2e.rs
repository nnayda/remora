//! End-to-end tests against a REAL sshd + tmux. Ignored by default — they
//! need external infrastructure. Run by hand (or in a future docker CI):
//!
//! ```sh
//! # For the attach test, create the session first:
//! #   tmux new-session -d -s remora_demo_one
//! REMORA_E2E_SSH_HOST=devbox \
//! REMORA_E2E_PROJECT=demo REMORA_E2E_SESSION=one \
//!   cargo test -p remora-core --test ssh_e2e -- --ignored --nocapture
//! ```
//!
//! Optional env: `REMORA_E2E_SSH_USER`, `REMORA_E2E_SSH_PORT`.
//!
//! Spawn tests also use:
//! - `REMORA_E2E_PATH` — working dir on the host for shared-workspace spawn
//!   (defaults to `~/e2e`).
//! - `REMORA_E2E_GIT_PATH` — path to an existing git repo on the host,
//!   required for the worktree cold-start test.

use std::time::Duration;

use tokio::time::Instant;

use std::sync::Arc;

use remora_core::config::{Config, SshHost};
use remora_core::{
    ChannelOutput, ProjectId, SessionChannel, SessionId, SessionSource, SshSource, TerminalSize,
};

#[tokio::test]
#[ignore = "needs a real sshd + tmux; see module docs"]
async fn attaches_to_a_live_remote_tmux_session() {
    let host = SshHost {
        host: std::env::var("REMORA_E2E_SSH_HOST").expect("REMORA_E2E_SSH_HOST"),
        user: std::env::var("REMORA_E2E_SSH_USER").ok(),
        port: std::env::var("REMORA_E2E_SSH_PORT")
            .ok()
            .map(|p| p.parse().expect("port is a u16")),
    };
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

    let source = SshSource::new(host, Arc::new(Config::default()));
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

/// Builds an `SshHost` from the shared e2e env vars.
fn e2e_host(host_dest: &str) -> SshHost {
    SshHost {
        host: host_dest.to_string(),
        user: std::env::var("REMORA_E2E_SSH_USER").ok(),
        port: std::env::var("REMORA_E2E_SSH_PORT")
            .ok()
            .map(|p| p.parse().expect("port is a u16")),
    }
}

#[tokio::test]
#[ignore = "needs a real sshd + tmux; see module docs"]
async fn e2e_spawn_shared_session_runs_and_blocks_duplicate() {
    let host_dest = match std::env::var("REMORA_E2E_SSH_HOST") {
        Ok(h) => h,
        Err(_) => return,
    };
    let project = std::env::var("REMORA_E2E_PROJECT").unwrap_or_else(|_| "e2e".into());
    let path = std::env::var("REMORA_E2E_PATH").unwrap_or_else(|_| "~/e2e".into());
    let session = format!("spawn-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "ssh"
        host = "{host_dest}"
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
    let source = SshSource::new(e2e_host(&host_dest), config);

    let spec = remora_core::SpawnSpec {
        project_id: ProjectId::new(&project).expect("slug"),
        session_id: SessionId::new(&session).expect("slug"),
        agent: None,
        base: None,
        workspace: None,
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
#[ignore = "needs REMORA_E2E_SSH_HOST + REMORA_E2E_GIT_PATH (a git repo on the host)"]
async fn e2e_spawn_worktree_cold_start_creates_worktree() {
    let (host_dest, git_path) = match (
        std::env::var("REMORA_E2E_SSH_HOST"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(h), Ok(g)) => (h, g),
        _ => return,
    };
    let session = format!("wt-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "ssh"
        host = "{host_dest}"
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
    let source = SshSource::new(e2e_host(&host_dest), config);

    let spec = remora_core::SpawnSpec {
        project_id: ProjectId::new("gitproj").expect("slug"),
        session_id: SessionId::new(&session).expect("slug"),
        agent: None,
        base: None,
        workspace: None,
    };

    // Cold start: ~/.remora/worktrees/gitproj/ need not pre-exist; git creates it.
    let mut channel = source.spawn(spec).await.expect("worktree spawn");
    channel.send_bytes(b"pwd\n".to_vec()).await.expect("send");
    recv_until_contains(&mut channel, &format!("worktrees/gitproj/{session}")).await;
}

#[tokio::test]
#[ignore = "needs REMORA_E2E_SSH_HOST + REMORA_E2E_GIT_PATH (a git repo on the host)"]
async fn e2e_discovery_stopped_then_respawn_reuses_worktree() {
    let (host_dest, git_path) = match (
        std::env::var("REMORA_E2E_SSH_HOST"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(h), Ok(g)) => (h, g),
        _ => return,
    };
    let session = format!("disc-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "ssh"
        host = "{host_dest}"
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
    let source = SshSource::new(e2e_host(&host_dest), config);
    let project = ProjectId::new("gitproj").expect("slug");
    let session_id = SessionId::new(&session).expect("slug");

    // Spawn a worktree session and write a marker file into the worktree.
    let spec = remora_core::SpawnSpec {
        project_id: project.clone(),
        session_id: session_id.clone(),
        agent: None,
        base: None,
        workspace: None,
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
    // Stopped. Raw ssh to the dest (assumes an ssh-config-resolvable host, as
    // the other e2e tests do); the session was just spawned, so it exists.
    std::process::Command::new("ssh")
        .args([
            host_dest.as_str(),
            "tmux",
            "kill-session",
            "-t",
            &format!("remora_gitproj_{session}"),
        ])
        .output()
        .expect("kill-session");

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
#[ignore = "needs REMORA_E2E_SSH_HOST + REMORA_E2E_GIT_PATH (a git repo on the host)"]
async fn e2e_respawn_of_vanished_worktree_is_not_found() {
    let (host_dest, git_path) = match (
        std::env::var("REMORA_E2E_SSH_HOST"),
        std::env::var("REMORA_E2E_GIT_PATH"),
    ) {
        (Ok(h), Ok(g)) => (h, g),
        _ => return,
    };
    let session = format!("vanish-{}", std::process::id());

    let toml = format!(
        r#"
        [hosts.e2e]
        transport = "ssh"
        host = "{host_dest}"
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
    let source = SshSource::new(e2e_host(&host_dest), config);
    let project = ProjectId::new("gitproj").expect("slug");
    let session_id = SessionId::new(&session).expect("slug");

    // Spawn a worktree session, then sync so the worktree has landed.
    let spec = remora_core::SpawnSpec {
        project_id: project.clone(),
        session_id: session_id.clone(),
        agent: None,
        base: None,
        workspace: None,
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
    std::process::Command::new("ssh")
        .args([
            host_dest.as_str(),
            "tmux",
            "kill-session",
            "-t",
            &format!("remora_gitproj_{session}"),
        ])
        .output()
        .expect("kill-session");
    std::process::Command::new("ssh")
        .args([
            host_dest.as_str(),
            "rm",
            "-rf",
            &format!("$HOME/.remora/worktrees/gitproj/{session}"),
        ])
        .output()
        .expect("rm worktree");

    let err = source
        .respawn(&project, &session_id, None)
        .await
        .expect_err("vanished worktree");
    assert!(
        matches!(err, remora_core::SourceError::SessionNotFound { .. }),
        "vanished worktree must be SessionNotFound, got {err}"
    );
}
