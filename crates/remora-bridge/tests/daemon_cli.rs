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

// status against a daemon whose relay never answers: daemon-alive semantics
// (exit 0) vs --require-relay (exit 1). G10.
#[test]
fn status_semantics_during_relay_outage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_config(dir.path(), "ws://127.0.0.1:1");
    let mut child = spawn_serve(&config, dir.path());
    let sock = dir.path().join("ctl.sock");
    wait_for("ctl.sock", || sock.exists());

    let plain = Command::new(BIN)
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("status");
    assert!(plain.status.success(), "plain status must be liveness-true");
    assert!(String::from_utf8_lossy(&plain.stdout).contains("reconnecting"));

    let strict = Command::new(BIN)
        .args(["status", "--require-relay", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("status strict");
    assert!(
        !strict.status.success(),
        "--require-relay must fail while disconnected"
    );

    // G7: revoke of anything is health-gated while disconnected — fast, clear.
    let revoke = Command::new(BIN)
        .args(["revoke", "feedbeef", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("revoke");
    assert!(!revoke.status.success());
    assert!(String::from_utf8_lossy(&revoke.stderr).contains("not connected"));

    // devices works offline (roster is local).
    let devices = Command::new(BIN)
        .args(["devices", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("devices");
    assert!(devices.status.success());
    assert!(String::from_utf8_lossy(&devices.stdout).contains("no paired devices"));

    // G6: an oversize first line gets an error, and the daemon survives.
    {
        use std::io::{Read, Write};
        let mut conn = std::os::unix::net::UnixStream::connect(&sock).expect("connect");
        let big = vec![b'a'; 100 * 1024];
        // The server may close mid-write once the cap trips; a broken pipe
        // here IS the bound working.
        let _ = conn.write_all(&big);
        let _ = conn.write_all(b"\n");
        let mut out = String::new();
        let _ = conn.read_to_string(&mut out);
    }
    let after = Command::new(BIN)
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("status after oversize");
    assert!(
        after.status.success(),
        "daemon must survive an oversize request"
    );

    kill_term(child.id());
    child.wait().expect("wait");
}

// G6/D1: a no-newline flood must be stopped by the byte cap itself, not the
// 10s first-line timeout — the "too large" error (or a close) arrives
// promptly, memory never grows past the cap, and the daemon survives.
#[test]
fn no_newline_flood_is_bounded_by_the_cap_not_the_timeout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_config(dir.path(), "ws://127.0.0.1:1");
    let mut child = spawn_serve(&config, dir.path());
    let sock = dir.path().join("ctl.sock");
    wait_for("ctl.sock", || sock.exists());

    {
        use std::io::{Read, Write};
        let mut conn = std::os::unix::net::UnixStream::connect(&sock).expect("connect");
        // Deadline shorter than the server's 10s first-line timeout: if the
        // response only arrives via that timeout (or never), this read errs
        // and the assertion below fails.
        conn.set_read_timeout(Some(Duration::from_secs(8)))
            .expect("set timeout");
        // Stream 128 KiB with NO trailing newline. A mid-write broken pipe
        // IS the bound working (the server closed at the cap).
        let chunk = vec![b'a'; 8 * 1024];
        for _ in 0..16 {
            if conn.write_all(&chunk).is_err() {
                break;
            }
        }
        let mut out = Vec::new();
        let read = conn.read_to_end(&mut out);
        // Three acceptable prompt outcomes, all meaning the cap answered:
        // the "too large" error line, a clean close (FIN → Ok with no
        // data), or a reset (RST → ECONNRESET): a server that closes while
        // our unread flood bytes are still queued resets rather than
        // FIN-closing, which is timing/kernel-dependent (seen on CI).
        // Only a *timeout* here would mean the 10s deadline answered
        // instead of the cap.
        match read {
            Ok(_) => {
                let text = String::from_utf8_lossy(&out);
                assert!(
                    text.is_empty() || text.contains("too large"),
                    "expected the cap (not the timeout) to answer, got: {text}"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("expected an error line, close, or reset, got {e:?}"),
        }
    }

    let after = Command::new(BIN)
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("status after flood");
    assert!(after.status.success(), "daemon must survive the flood");

    kill_term(child.id());
    child.wait().expect("wait");
}

// Client against no daemon: named error, nonzero exit.
#[test]
fn ctl_against_dead_daemon_is_a_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(BIN)
        .args(["status", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("status");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("daemon not running"));
}

// G11: identity/roster files written by one host (the desktop uses the same
// library paths) load in the headless daemon's state dir — the file-move
// migration contract (spec D2).
#[test]
fn migrated_identity_files_load_verbatim() {
    let desktop = tempfile::tempdir().expect("desktop dir");
    let headless = tempfile::tempdir().expect("headless dir");

    // "Desktop" writes identity via the same lib call it really uses.
    let init = Command::new(BIN)
        .args(["init", "--state-dir"])
        .arg(desktop.path())
        .output()
        .expect("init");
    assert!(init.status.success());
    let original = String::from_utf8_lossy(&init.stdout).to_string();

    // The migration: move (not copy) the identity file.
    std::fs::rename(
        desktop.path().join("bridge_identity.toml"),
        headless.path().join("bridge_identity.toml"),
    )
    .expect("move identity");

    // Headless init loads the moved identity — same device_id, same fingerprint.
    let again = Command::new(BIN)
        .args(["init", "--state-dir"])
        .arg(headless.path())
        .output()
        .expect("init again");
    assert!(again.status.success());
    assert_eq!(original, String::from_utf8_lossy(&again.stdout));
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
