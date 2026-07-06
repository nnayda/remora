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
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
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
                    // Loop back to await the daemon's authoritative PairResult.
                }
                Some(Ok(CtlResponse::PairResult { outcome, .. })) => {
                    return match outcome.as_str() {
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
                    };
                }
                Some(Ok(CtlResponse::Error { message })) => return Err(message),
                Some(Ok(other)) => {
                    return Err(format!("unexpected response during pairing: {other:?}"))
                }
                Some(Err(e)) => return Err(e),
                // Daemon dropped the connection with no result — treat as expiry.
                None => return expired(),
            }
        }
    }
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
