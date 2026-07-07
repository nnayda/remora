//! Ctl-socket client: the one-shot subcommands (`status`, `devices`, `revoke`,
//! `fingerprint`) that talk to a running daemon over `ctl.sock`. Each sends one
//! request line and renders the single response line. The interactive `pair`
//! flow (below) is the one long-lived session: one request, then a stream of
//! events driven to a terminal enrollment decision.

use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;

use crate::args::Command;
use crate::proto::{CtlRequest, CtlResponse, DeviceDto, RelayStateDto};

pub async fn run(command: Command, state_dir: &Path) -> Result<ExitCode, String> {
    match command {
        Command::Status { require_relay } => status(state_dir, require_relay).await,
        Command::Devices => devices(state_dir).await,
        Command::Revoke { device_id } => revoke(state_dir, device_id).await,
        Command::Fingerprint => fingerprint(state_dir).await,
        Command::Pair { ttl_secs } => pair(state_dir, ttl_secs).await,
        // `serve`/`init` never reach the ctl client (dispatched in main).
        Command::Serve { .. } | Command::Init => {
            Err("internal error: serve/init are handled before the ctl client".to_string())
        }
    }
}

/// Connects to `<state_dir>/ctl.sock`, translating "no daemon" errors into one
/// actionable line.
async fn connect(state_dir: &Path) -> Result<UnixStream, String> {
    let path = state_dir.join(crate::daemon::CTL_SOCKET);
    match UnixStream::connect(&path).await {
        Ok(stream) => Ok(stream),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            Err(format!(
                "daemon not running at {} (is remora-bridge serve up?)",
                path.display()
            ))
        }
        Err(e) => Err(format!("connecting to {}: {e}", path.display())),
    }
}

/// Sends one request line and reads the single response line.
async fn request(stream: UnixStream, req: &CtlRequest) -> Result<CtlResponse, String> {
    let (rd, mut wr) = stream.into_split();
    let mut line = serde_json::to_string(req).map_err(|e| format!("encoding request: {e}"))?;
    line.push('\n');
    wr.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("sending request: {e}"))?;
    wr.flush()
        .await
        .map_err(|e| format!("sending request: {e}"))?;

    let mut reader = BufReader::new(rd);
    let mut resp_line = String::new();
    let n = reader
        .read_line(&mut resp_line)
        .await
        .map_err(|e| format!("reading response: {e}"))?;
    if n == 0 {
        return Err("daemon closed the connection without responding".to_string());
    }
    serde_json::from_str(resp_line.trim_end())
        .map_err(|e| format!("unrecognized response from daemon: {e}"))
}

async fn status(state_dir: &Path, require_relay: bool) -> Result<ExitCode, String> {
    let stream = connect(state_dir).await?;
    match request(stream, &CtlRequest::Status).await? {
        CtlResponse::Status {
            relay,
            device_id,
            fingerprint,
        } => {
            let connected = matches!(relay, RelayStateDto::Connected { .. });
            match relay {
                RelayStateDto::Starting => println!("relay: starting"),
                RelayStateDto::Connected { since } => {
                    println!("relay: connected (since {since})");
                }
                RelayStateDto::Reconnecting { since, attempts } => {
                    println!("relay: reconnecting since {since} ({attempts} attempts)");
                }
                RelayStateDto::Rejected { at, detail } => {
                    println!("relay: rejected at {at} — {detail}");
                }
            }
            println!("bridge {device_id} ({fingerprint})");
            // Liveness-true by default (the daemon answered); `--require-relay`
            // (G10) additionally demands a live relay connection.
            if require_relay && !connected {
                return Ok(ExitCode::FAILURE);
            }
            Ok(ExitCode::SUCCESS)
        }
        CtlResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response to status: {other:?}")),
    }
}

async fn devices(state_dir: &Path) -> Result<ExitCode, String> {
    let stream = connect(state_dir).await?;
    match request(stream, &CtlRequest::Devices).await? {
        CtlResponse::Devices { devices } => {
            if devices.is_empty() {
                println!("no paired devices");
            } else {
                print_device_table(&devices);
            }
            Ok(ExitCode::SUCCESS)
        }
        CtlResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response to devices: {other:?}")),
    }
}

async fn revoke(state_dir: &Path, device_id: String) -> Result<ExitCode, String> {
    let stream = connect(state_dir).await?;
    match request(
        stream,
        &CtlRequest::Revoke {
            device_id: device_id.clone(),
        },
    )
    .await?
    {
        CtlResponse::Ok => {
            println!("revoked {device_id}");
            Ok(ExitCode::SUCCESS)
        }
        // `Error` → stderr + exit 1 (main prints the returned message to stderr).
        CtlResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response to revoke: {other:?}")),
    }
}

async fn fingerprint(state_dir: &Path) -> Result<ExitCode, String> {
    let stream = connect(state_dir).await?;
    match request(stream, &CtlRequest::Fingerprint).await? {
        CtlResponse::Fingerprint {
            device_id,
            fingerprint,
        } => {
            // Same shape as `init`.
            println!("device_id   {device_id}");
            println!("fingerprint {fingerprint}");
            Ok(ExitCode::SUCCESS)
        }
        CtlResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response to fingerprint: {other:?}")),
    }
}

/// The interactive pairing ceremony (spec D6, D13a). One session over the
/// socket: send `PairOpen`, print the code block, then drive the event stream
/// to a terminal enrollment decision.
///
/// Ctrl-C needs no handling here: the default SIGINT kills the process, which
/// closes this connection, and the daemon's connection-close handler cancels
/// the relay window (Task 9's fail-safe `CancelWindow`).
async fn pair(state_dir: &Path, ttl_secs: u64) -> Result<ExitCode, String> {
    let stream = connect(state_dir).await?;
    let (rd, mut wr) = stream.into_split();
    write_req(&mut wr, &CtlRequest::PairOpen { ttl_secs }).await?;

    // Read responses off the socket in a dedicated task so the `select!` below
    // never drops a mid-flight (not cancellation-safe) `read_line`; the branch
    // becomes a cancellation-safe `mpsc::recv`.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<CtlResponse, String>>(8);
    tokio::spawn(async move {
        let mut reader = BufReader::new(rd);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return, // EOF: dropping `tx` surfaces as `recv() -> None`.
                Ok(_) => {
                    let parsed = serde_json::from_str::<CtlResponse>(line.trim_end())
                        .map_err(|e| format!("unrecognized response from daemon: {e}"));
                    if tx.send(parsed).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(format!("reading response: {e}"))).await;
                    return;
                }
            }
        }
    });

    // First line must announce the window (or explain why it could not open).
    let (code, expires_at) = match rx.recv().await {
        Some(Ok(CtlResponse::WindowOpened { code, expires_at })) => (code, expires_at),
        Some(Ok(CtlResponse::Error { message })) => return Err(message),
        Some(Ok(other)) => return Err(format!("unexpected response to pair: {other:?}")),
        Some(Err(e)) => return Err(e),
        None => return Err("daemon closed the connection before opening a window".to_string()),
    };

    // The no-camera pairing story: the code is printed verbatim on its own line.
    println!(
        "Pairing window open ({}). Scan or paste on your device:",
        humanize_secs(ttl_secs)
    );
    println!();
    println!("  {code}");
    println!();
    println!("Waiting for device...");

    // D13a: the daemon emits no event for a window nobody joins, so the client
    // owns the expiry. The deadline rides on the wire `expires_at` (not the
    // local TTL, which ignores clock skew), plus a small grace.
    let deadline = window_deadline(expires_at);

    let mut stdin = BufReader::new(tokio::io::stdin());
    // Set once a Confirm/Reject has gone out. From then on the daemon owns
    // the outcome, so the local deadline stops meaning "expired" (#300): a
    // decision answered near the deadline may have committed the enrollment
    // daemon-side even though the authoritative PairResult is still in flight.
    let mut decision_sent = false;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if decision_sent {
                    // Prefer the event channel for a bounded moment so the
                    // authoritative result can arrive and be reported truthfully.
                    let wait =
                        await_result_after_decision(&mut rx, DECISION_RESULT_GRACE).await;
                    return report_decision_wait(wait);
                }
                let _ = write_req(&mut wr, &CtlRequest::PairCancel).await;
                return expired();
            }
            msg = rx.recv() => match msg {
                Some(Ok(CtlResponse::DeviceArrived { device_id, name, fingerprint })) => {
                    println!("Device arrived: \"{name}\"");
                    println!("  fingerprint: {fingerprint}");
                    // Newline-terminated so a line-buffered reader (an operator's
                    // terminal, or the ceremony test) sees the prompt at once.
                    println!("Confirm enrollment? [y/N] ");

                    // Read exactly one decision line; a confirmation is pending
                    // only here, so stdin is read only here.
                    let mut answer = String::new();
                    let confirm = tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {
                            if decision_sent {
                                // A prior decision is already with the daemon
                                // (this prompt was for a later arrival); its
                                // outcome may have committed — same grace as
                                // the outer deadline branch (#300).
                                let wait = await_result_after_decision(
                                    &mut rx,
                                    DECISION_RESULT_GRACE,
                                )
                                .await;
                                return report_decision_wait(wait);
                            }
                            let _ = write_req(&mut wr, &CtlRequest::PairCancel).await;
                            return expired();
                        }
                        read = stdin.read_line(&mut answer) => match read {
                            // G9: EOF (a dropped exec session) must reject, never enroll.
                            Ok(0) => {
                                eprintln!("stdin closed — rejecting");
                                false
                            }
                            Ok(_) => matches!(answer.trim(), "y" | "Y" | "yes"),
                            Err(e) => return Err(format!("reading stdin: {e}")),
                        }
                    };
                    let decision = if confirm {
                        CtlRequest::PairConfirm { device_id }
                    } else {
                        CtlRequest::PairReject { device_id }
                    };
                    write_req(&mut wr, &decision).await?;
                    decision_sent = true;
                    // Loop back to await the daemon's authoritative PairResult.
                }
                Some(Ok(CtlResponse::PairResult { outcome, .. })) => {
                    return report_outcome(&outcome);
                }
                Some(Ok(CtlResponse::Error { message })) => return Err(message),
                Some(Ok(other)) => {
                    return Err(format!("unexpected response during pairing: {other:?}"))
                }
                Some(Err(e)) => return Err(e),
                // Daemon dropped the connection with no result: before any
                // decision that reads as expiry; after one it is indeterminate
                // (the enrollment may have committed before the drop).
                None => {
                    return if decision_sent {
                        no_result_after_decision()
                    } else {
                        expired()
                    };
                }
            }
        }
    }
}

/// How long the client keeps listening for the daemon's authoritative
/// `PairResult` after a Confirm/Reject decision has been sent but the local
/// window deadline has fired (#300). Distinct from the clock-skew grace in
/// [`window_deadline`]: this one only covers the last hop — the result
/// normally arrives within milliseconds (local socket plus one engine round
/// trip), so a few seconds absorbs a loaded host without letting a wedged
/// daemon hang the CLI. Purely a client-side wait: the daemon-side window
/// lifetime is untouched and stays fail-closed.
const DECISION_RESULT_GRACE: Duration = Duration::from_secs(5);

/// Terminal reading of the post-decision grace wait (#300).
#[derive(Debug, PartialEq)]
enum DecisionWait {
    /// The authoritative `PairResult` outcome arrived within the grace.
    Outcome(String),
    /// The daemon reported an error (or the stream broke) during the wait.
    Failed(String),
    /// No result before the grace elapsed (or the daemon dropped the
    /// connection): the enrollment may or may not have committed.
    NoResult,
}

/// Drains the event channel for a bounded grace, looking for the
/// authoritative `PairResult`. Factored off the socket loop so the
/// decision-then-deadline race is unit-testable with a plain channel.
async fn await_result_after_decision(
    rx: &mut tokio::sync::mpsc::Receiver<Result<CtlResponse, String>>,
    grace: Duration,
) -> DecisionWait {
    let grace_deadline = tokio::time::Instant::now() + grace;
    loop {
        let msg = tokio::select! {
            _ = tokio::time::sleep_until(grace_deadline) => return DecisionWait::NoResult,
            msg = rx.recv() => msg,
        };
        match msg {
            Some(Ok(CtlResponse::PairResult { outcome, .. })) => {
                return DecisionWait::Outcome(outcome);
            }
            Some(Ok(CtlResponse::Error { message })) => return DecisionWait::Failed(message),
            // Anything else on a window already past its deadline is stale
            // chatter (e.g. a late second arrival we can no longer prompt
            // for); only the result or an error terminates the wait.
            Some(Ok(_)) => {}
            Some(Err(e)) => return DecisionWait::Failed(e),
            None => return DecisionWait::NoResult,
        }
    }
}

/// Maps the grace-wait reading to the process outcome.
fn report_decision_wait(wait: DecisionWait) -> Result<ExitCode, String> {
    match wait {
        DecisionWait::Outcome(outcome) => report_outcome(&outcome),
        DecisionWait::Failed(message) => Err(message),
        DecisionWait::NoResult => no_result_after_decision(),
    }
}

/// Renders the daemon's authoritative pairing outcome (shared by the normal
/// event-stream path and the post-decision grace path).
fn report_outcome(outcome: &str) -> Result<ExitCode, String> {
    match outcome {
        "paired" => {
            println!("Device enrolled.");
            Ok(ExitCode::SUCCESS)
        }
        // The operator's reject is a completed ceremony, not a failure.
        "rejected" => {
            println!("Enrollment rejected.");
            Ok(ExitCode::SUCCESS)
        }
        "expired" => expired(),
        other => Err(format!("unexpected pairing outcome: {other}")),
    }
}

/// A decision went out but no authoritative result came back before the
/// window deadline plus grace: the enrollment may have committed daemon-side,
/// so calling it "expired" would lie (#300). Report indeterminate, point at
/// the roster, and exit nonzero like `expired` (a user-facing outcome, not an
/// internal error — no `remora-bridge:` prefix).
fn no_result_after_decision() -> Result<ExitCode, String> {
    eprintln!(
        "no pairing result before the window deadline — the decision was sent; \
         run `remora-bridge devices` to see whether the device enrolled"
    );
    Ok(ExitCode::FAILURE)
}

/// The client-side expiry outcome (D13a): a user-facing next step on stderr and
/// a nonzero exit, but not an internal error (so no `remora-bridge:` prefix).
fn expired() -> Result<ExitCode, String> {
    eprintln!("pairing window expired — re-run pair for a fresh code");
    Ok(ExitCode::FAILURE)
}

/// Absolute deadline from the wire `expires_at` (unix seconds) plus grace.
fn window_deadline(expires_at: u64) -> tokio::time::Instant {
    const GRACE_SECS: u64 = 5;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let remaining = expires_at.saturating_add(GRACE_SECS).saturating_sub(now);
    tokio::time::Instant::now() + Duration::from_secs(remaining)
}

/// Renders a window duration as `30s` / `5m0s`, matching the ceremony spec.
fn humanize_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Writes one request line (newline-framed, flushed) to the socket write half.
async fn write_req(wr: &mut OwnedWriteHalf, req: &CtlRequest) -> Result<(), String> {
    let mut line = serde_json::to_string(req).map_err(|e| format!("encoding request: {e}"))?;
    line.push('\n');
    wr.write_all(line.as_bytes())
        .await
        .map_err(|e| format!("sending request: {e}"))?;
    wr.flush()
        .await
        .map_err(|e| format!("sending request: {e}"))?;
    Ok(())
}

/// Renders the paired-device roster as a left-aligned, space-padded table.
fn print_device_table(devices: &[DeviceDto]) {
    fn ts(v: Option<u64>) -> String {
        v.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string())
    }
    let headers = ["ID", "NAME", "FINGERPRINT", "ENROLLED", "LAST SEEN"];
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|d| {
            [
                d.device_id.clone(),
                d.name.clone(),
                d.fingerprint.clone(),
                ts(d.enrolled_at),
                ts(d.last_connected_at),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let print_row = |cells: &[String; 5]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            // The final column needs no trailing pad.
            if i == cells.len() - 1 {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}", width = widths[i]));
            }
        }
        println!("{out}");
    };

    print_row(&headers.map(str::to_string));
    for row in &rows {
        print_row(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn result_line(outcome: &str) -> Result<CtlResponse, String> {
        Ok(CtlResponse::PairResult {
            outcome: outcome.to_string(),
            device_id: None,
            name: None,
        })
    }

    // #300: the authoritative result beats the grace deadline and is reported,
    // not swallowed by a flat "expired".
    #[tokio::test(start_paused = true)]
    async fn grace_reports_a_late_pair_result() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(result_line("paired")).await.expect("send");
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::Outcome("paired".to_string()));
    }

    // A daemon-asserted "expired" arriving during the grace is still honored —
    // the grace defers to the authoritative outcome, whatever it is.
    #[tokio::test(start_paused = true)]
    async fn grace_defers_to_an_authoritative_expired() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(result_line("expired")).await.expect("send");
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::Outcome("expired".to_string()));
    }

    // Stale chatter (e.g. a late second arrival) must not terminate the wait;
    // only the result does.
    #[tokio::test(start_paused = true)]
    async fn grace_skips_stale_chatter_before_the_result() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(Ok(CtlResponse::DeviceArrived {
            device_id: "feedbeef".to_string(),
            name: "late phone".to_string(),
            fingerprint: "aa:bb".to_string(),
        }))
        .await
        .expect("send");
        tx.send(result_line("paired")).await.expect("send");
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::Outcome("paired".to_string()));
    }

    // The grace is bounded: with no result it ends as the indeterminate
    // NoResult, never hanging the CLI on a wedged daemon.
    #[tokio::test(start_paused = true)]
    async fn grace_elapsing_without_a_result_is_indeterminate() {
        // tx stays alive so the channel never closes; the paused clock
        // auto-advances to the grace deadline once recv() is the only waiter.
        let (tx, mut rx) = mpsc::channel::<Result<CtlResponse, String>>(8);
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::NoResult);
        drop(tx);
    }

    // A daemon that drops the connection after the decision is indeterminate
    // (the enrollment may have committed before the drop), not "expired".
    #[tokio::test(start_paused = true)]
    async fn daemon_drop_during_grace_is_indeterminate() {
        let (tx, mut rx) = mpsc::channel::<Result<CtlResponse, String>>(8);
        drop(tx);
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::NoResult);
    }

    // An explicit daemon error (or stream decode failure) still surfaces as an
    // error, not as the indeterminate outcome.
    #[tokio::test(start_paused = true)]
    async fn daemon_error_during_grace_fails() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(Ok(CtlResponse::Error {
            message: "boom".to_string(),
        }))
        .await
        .expect("send");
        let wait = await_result_after_decision(&mut rx, Duration::from_secs(5)).await;
        assert_eq!(wait, DecisionWait::Failed("boom".to_string()));
    }
}
