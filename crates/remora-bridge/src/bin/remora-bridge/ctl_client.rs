//! Ctl-socket client: the one-shot subcommands (`status`, `devices`, `revoke`,
//! `fingerprint`) that talk to a running daemon over `ctl.sock`. Each sends one
//! request line and renders the single response line. The interactive `pair`
//! loop is plan Task 10; its arm here is a named stub.

use std::io::ErrorKind;
use std::path::Path;
use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::args::Command;
use crate::proto::{CtlRequest, CtlResponse, DeviceDto, RelayStateDto};

pub async fn run(command: Command, state_dir: &Path) -> Result<ExitCode, String> {
    match command {
        Command::Status { require_relay } => status(state_dir, require_relay).await,
        Command::Devices => devices(state_dir).await,
        Command::Revoke { device_id } => revoke(state_dir, device_id).await,
        Command::Fingerprint => fingerprint(state_dir).await,
        // The interactive pairing loop lands in plan Task 10.
        Command::Pair { .. } => Err("not implemented yet (plan Task 10)".to_string()),
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
