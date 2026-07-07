//! End-to-end pairing ceremony through the headless daemon (#234):
//! in-process blind relay ⇄ remora-bridge serve (real binary) ⇄ ctl.sock,
//! with the device side running the real `run_pairing` driver.
//! Covers: happy path (confirm), EOF-at-prompt = reject (G9),
//! window expiry exit (G8), concurrent-pair refusal.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_remora-bridge");

/// Stands up a real relay on 127.0.0.1:0 that admits one bridge, and writes
/// the matching config.toml. Returns (config_path, relay accept task handle).
async fn relay_and_config(dir: &Path) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    // The bridge identity must exist FIRST so we can admit its device_id:
    // run `remora-bridge init` offline (D12 — this test doubles as G13).
    let init = Command::new(BIN)
        .args(["init", "--state-dir"])
        .arg(dir)
        .output()
        .expect("init");
    assert!(init.status.success());
    let stdout = String::from_utf8_lossy(&init.stdout);
    let device_id = stdout
        .lines()
        .find_map(|l| l.strip_prefix("device_id"))
        .expect("device_id line")
        .trim()
        .to_string();

    let relay_cfg = Arc::new(remora_relay::RelayConfig {
        listen: "127.0.0.1:0".to_string(),
        bridges: vec![remora_relay::BridgeEntry {
            token: "test-reg-token".to_string(),
            device_id: device_id.parse().expect("device id"),
        }],
        buffer_bytes: 1 << 20,
        handshake_timeout_secs: 10,
        max_connections: 64,
        audit: None,
        push: remora_relay::PushConfig::default(),
    });
    let audit = remora_relay::AuditSink::new(&relay_cfg).expect("audit");
    let (addr, _router, accept) = remora_relay::serve(relay_cfg, audit).await.expect("relay");

    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!("[relay]\nrelay_url = \"ws://{addr}\"\nregistration_token = \"test-reg-token\"\n"),
    )
    .expect("config");
    (config, accept)
}

fn wait_for<F: Fn() -> bool>(what: &str, cond: F) {
    let deadline = Instant::now() + Duration::from_secs(15);
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

/// Blocks until the daemon reports a live relay connection (pair is
/// health-gated, D13c).
fn wait_for_relay(dir: &Path) {
    wait_for("relay connected", || {
        Command::new(BIN)
            .args(["status", "--require-relay", "--state-dir"])
            .arg(dir)
            .output()
            .expect("status")
            .status
            .success()
    });
}

/// Reads `pair` stdout until the remora-pair code line appears, returning it
/// trimmed (indentation stripped).
fn read_until_code(reader: &mut impl BufRead) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).expect("read pair stdout") == 0 {
            panic!("pair exited before printing a code");
        }
        if line.trim().strip_prefix("remora-pair:").is_some() {
            return line.trim().to_string();
        }
    }
    panic!("no pairing code within deadline");
}

#[tokio::test(flavor = "multi_thread")]
async fn full_ceremony_confirm_then_status_connected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config, _relay) = relay_and_config(dir.path()).await;
    let mut daemon = spawn_serve(&config, dir.path());
    let sock = dir.path().join("ctl.sock");
    wait_for("ctl.sock", || sock.exists());
    wait_for_relay(dir.path());

    // Start `pair` with piped stdio; capture the printed code.
    let mut pair = Command::new(BIN)
        .args(["pair", "--ttl", "30", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pair");
    let mut pair_out = BufReader::new(pair.stdout.take().expect("stdout"));
    let code_str = read_until_code(&mut pair_out);
    let code = remora_protocol::PairingCode::parse(&code_str).expect("parse pairing code");

    // Device side: the real ceremony driver.
    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<remora_bridge::PairingProgress>(8);
    tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });
    let device = tokio::spawn(remora_bridge::run_pairing(
        code,
        "test phone".to_string(),
        progress_tx,
    ));

    // Operator side: wait for the fingerprint prompt, answer y.
    let mut line = String::new();
    let mut prompted = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        line.clear();
        if pair_out.read_line(&mut line).expect("read") == 0 {
            break;
        }
        if line.contains("Confirm enrollment?") || line.contains("fingerprint:") {
            prompted = true;
        }
        if prompted && line.contains("Confirm enrollment?") {
            pair.stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"y\n")
                .expect("write y");
            break;
        }
    }
    assert!(prompted, "never saw the fingerprint prompt");

    let pair_status = pair.wait().expect("pair wait");
    assert!(pair_status.success(), "pair should exit 0 on enrollment");
    let pairing_file = device
        .await
        .expect("join")
        .expect("device pairing succeeds");
    drop(pairing_file);

    // The roster persisted: devices lists the phone.
    let devices = Command::new(BIN)
        .args(["devices", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("devices");
    assert!(String::from_utf8_lossy(&devices.stdout).contains("test phone"));

    kill_term(&mut daemon);
}

// G9: EOF at the confirm prompt must REJECT, never enroll.
#[tokio::test(flavor = "multi_thread")]
async fn eof_at_confirm_prompt_rejects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config, _relay) = relay_and_config(dir.path()).await;
    let mut daemon = spawn_serve(&config, dir.path());
    wait_for_relay(dir.path());

    let mut pair = Command::new(BIN)
        .args(["pair", "--ttl", "30", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pair");
    let mut pair_out = BufReader::new(pair.stdout.take().expect("stdout"));
    let code_str = read_until_code(&mut pair_out);
    let code = remora_protocol::PairingCode::parse(&code_str).expect("code");

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::channel::<remora_bridge::PairingProgress>(8);
    tokio::spawn(async move { while progress_rx.recv().await.is_some() {} });
    let device = tokio::spawn(remora_bridge::run_pairing(
        code,
        "eof phone".to_string(),
        progress_tx,
    ));

    // The moment the prompt appears, close stdin (the dropped-exec case).
    let mut line = String::new();
    loop {
        line.clear();
        if pair_out.read_line(&mut line).expect("read") == 0 {
            break;
        }
        if line.contains("Confirm enrollment?") {
            drop(pair.stdin.take());
            break;
        }
    }
    let _ = pair.wait().expect("pair wait");

    // Device side must NOT have been enrolled.
    let result = device.await.expect("join");
    assert!(result.is_err(), "device pairing must fail after EOF-reject");
    let devices = Command::new(BIN)
        .args(["devices", "--state-dir"])
        .arg(dir.path())
        .output()
        .expect("devices");
    assert!(
        !String::from_utf8_lossy(&devices.stdout).contains("eof phone"),
        "EOF must never enroll"
    );

    kill_term(&mut daemon);
}

// G8: a window nobody joins exits ≠0 on the client-side deadline.
#[tokio::test(flavor = "multi_thread")]
async fn unjoined_window_expires_with_nonzero_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (config, _relay) = relay_and_config(dir.path()).await;
    let mut daemon = spawn_serve(&config, dir.path());
    wait_for_relay(dir.path());

    let out = Command::new(BIN)
        .args(["pair", "--ttl", "1", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("pair");
    assert!(!out.status.success(), "expiry must exit nonzero");
    assert!(String::from_utf8_lossy(&out.stderr).contains("expired"));

    // Concurrent-pair refusal is cheap to piggyback here: while a fresh pair
    // runs, a second one is refused.
    let mut first = Command::new(BIN)
        .args(["pair", "--ttl", "10", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("first pair");
    let mut first_out = BufReader::new(first.stdout.take().expect("stdout"));
    let _ = read_until_code(&mut first_out);
    let second = Command::new(BIN)
        .args(["pair", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("second pair");
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("another pairing session"));
    first.kill().expect("kill first");
    first.wait().expect("wait");

    kill_term(&mut daemon);
}

// #300: a Confirm answered near the deadline must NOT report "expired" when
// the enrollment committed — after the decision is sent the client waits a
// bounded grace for the daemon's authoritative PairResult. A scripted fake
// ctl server controls the timing exactly: it withholds the result until the
// client-side window deadline (expires_at + 5s skew grace) has fired, then
// delivers "paired" inside the post-decision grace.
#[tokio::test(flavor = "multi_thread")]
async fn confirm_near_deadline_reports_the_authoritative_result() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("ctl.sock");
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake ctl.sock");

    let server = tokio::spawn(async move {
        let (stream, _addr) = listener.accept().await.expect("accept");
        let (rd, mut wr) = stream.into_split();
        let mut lines = TokioBufReader::new(rd).lines();

        let open = lines
            .next_line()
            .await
            .expect("read pair_open")
            .expect("pair_open line");
        assert!(open.contains("pair_open"), "first request: {open}");

        // A window that is nearly over: the client's local deadline lands
        // ~6s out (expires_at + its 5s clock-skew grace).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let opened = format!(
            "{{\"event\":\"window_opened\",\"code\":\"remora-pair:fake\",\"expires_at\":{}}}\n",
            now + 1
        );
        wr.write_all(opened.as_bytes()).await.expect("write opened");
        wr.write_all(
            b"{\"event\":\"device_arrived\",\"device_id\":\"feedbeef\",\
              \"name\":\"race phone\",\"fingerprint\":\"aa:bb\"}\n",
        )
        .await
        .expect("write arrived");

        let decision = lines
            .next_line()
            .await
            .expect("read decision")
            .expect("decision line");
        assert!(decision.contains("pair_confirm"), "decision: {decision}");

        // Withhold the result until well past the client's window deadline
        // (~6s) but well inside its post-decision grace (deadline + 5s).
        tokio::time::sleep(Duration::from_secs(8)).await;
        wr.write_all(
            b"{\"event\":\"pair_result\",\"outcome\":\"paired\",\
              \"device_id\":\"feedbeef\",\"name\":\"race phone\"}\n",
        )
        .await
        .expect("write result");
        // Hold the connection open briefly so the close never races the
        // client's read of the result line.
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let mut pair = Command::new(BIN)
        .args(["pair", "--ttl", "1", "--state-dir"])
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pair");
    let mut pair_out = BufReader::new(pair.stdout.take().expect("stdout"));
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            pair_out.read_line(&mut line).expect("read pair stdout") > 0,
            "pair exited before prompting"
        );
        if line.contains("Confirm enrollment?") {
            pair.stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"y\n")
                .expect("write y");
            break;
        }
    }

    let mut rest = String::new();
    std::io::Read::read_to_string(&mut pair_out, &mut rest).expect("drain stdout");
    let status = pair.wait().expect("pair wait");
    let mut stderr = String::new();
    std::io::Read::read_to_string(pair.stderr.as_mut().expect("stderr"), &mut stderr)
        .expect("drain stderr");

    assert!(
        status.success(),
        "pair must exit 0 on the late authoritative result; stderr: {stderr}"
    );
    assert!(
        rest.contains("Device enrolled."),
        "expected the authoritative outcome, got stdout: {rest} stderr: {stderr}"
    );
    assert!(
        !stderr.contains("expired"),
        "must not report expired for a committed enrollment: {stderr}"
    );

    server.await.expect("fake server");
}

fn kill_term(child: &mut Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
}
