//! End-to-end attach against a REAL sshd + tmux. Ignored by default — it
//! needs external infrastructure. Run by hand (or in a future docker CI):
//!
//! ```sh
//! # On the target host, create the session first:
//! #   tmux new-session -d -s remora_demo_one
//! REMORA_E2E_SSH_HOST=devbox \
//! REMORA_E2E_PROJECT=demo REMORA_E2E_SESSION=one \
//!   cargo test -p remora-core --test ssh_e2e -- --ignored --nocapture
//! ```
//!
//! Optional env: `REMORA_E2E_SSH_USER`, `REMORA_E2E_SSH_PORT`.

use std::time::Duration;

use remora_core::config::SshHost;
use remora_core::{ChannelOutput, ProjectId, SessionId, SessionSource, SshSource, TerminalSize};

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

    let source = SshSource::new(host);
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

    let mut acc = Vec::new();
    let found = loop {
        match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await {
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
