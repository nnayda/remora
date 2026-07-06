//! Black-box tests of the remora-bridge binary: startup validation, the
//! single-instance guard, socket permissions, and signal shutdown
//! (spec G2–G5, #234).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_remora-bridge");

fn write_config(dir: &Path, relay_url: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!("[relay]\nrelay_url = \"{relay_url}\"\nregistration_token = \"tok\"\n"),
    )
    .expect("write config");
    path
}

fn wait_for<F: FnMut() -> bool>(what: &str, mut cond: F) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn spawn_serve(config: &Path, state: &Path) -> Child {
    Command::new(BIN)
        .args(["serve"])
        .arg(config)
        .args(["--state-dir"])
        .arg(state)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve")
}

// G2: missing [relay] is a named, pre-daemonization exit 1.
#[test]
fn serve_without_relay_section_exits_nonzero_with_named_cause() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").expect("write");
    let out = Command::new(BIN)
        .args(["serve"])
        .arg(&config)
        .args(["--state-dir"])
        .arg(dir.path())
        .output()
        .expect("run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[relay]"), "stderr: {stderr}");
}

// G2: bad URL scheme named before daemonizing.
#[test]
fn serve_with_http_relay_url_exits_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_config(dir.path(), "http://relay.example");
    let out = Command::new(BIN)
        .args(["serve"])
        .arg(&config)
        .args(["--state-dir"])
        .arg(dir.path())
        .output()
        .expect("run");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ws://"));
}

// G3 + G5 + G4: socket appears with mode 0600; a second daemon is refused;
// SIGTERM exits 0 and removes the socket.
#[test]
fn serve_lifecycle_socket_mode_single_instance_and_sigterm() {
    let dir = tempfile::tempdir().expect("tempdir");
    // ws:// URL that never answers — the daemon must still start and serve ctl.
    let config = write_config(dir.path(), "ws://127.0.0.1:1");
    let mut child = spawn_serve(&config, dir.path());

    let sock = dir.path().join("ctl.sock");
    wait_for("ctl.sock to appear", || sock.exists());

    // G5: born 0600.
    let mode = std::fs::metadata(&sock).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode {mode:o}");

    // G4: second instance refused while the first runs.
    let second = Command::new(BIN)
        .args(["serve"])
        .arg(&config)
        .args(["--state-dir"])
        .arg(dir.path())
        .output()
        .expect("run second");
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    // G3: SIGTERM → clean exit → socket removed.
    kill_term(child.id());
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit {status:?}");
    assert!(!sock.exists(), "socket not cleaned up");
}

// G4: a stale socket left by a killed daemon is recovered.
#[test]
fn stale_socket_is_recovered_on_next_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_config(dir.path(), "ws://127.0.0.1:1");
    let mut child = spawn_serve(&config, dir.path());
    let sock = dir.path().join("ctl.sock");
    wait_for("ctl.sock", || sock.exists());
    child.kill().expect("SIGKILL"); // leaves the socket file behind
    child.wait().expect("wait");
    assert!(sock.exists(), "SIGKILL should leave a stale socket");

    let mut second = spawn_serve(&config, dir.path());
    wait_for("recovered ctl.sock", || {
        // A fresh bind replaces the inode; connectability is asserted in the
        // ctl tests — existence + the daemon staying alive suffices here.
        sock.exists() && second.try_wait().expect("try_wait").is_none()
    });
    kill_term(second.id());
    second.wait().expect("wait");
}

/// Send SIGTERM without unsafe: /bin/kill is universally present.
fn kill_term(pid: u32) {
    let ok = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill")
        .success();
    assert!(ok, "kill -TERM failed");
}
