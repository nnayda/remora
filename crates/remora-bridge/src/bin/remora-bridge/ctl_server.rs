//! The ctl server (`ctl.sock`, spec D1): one accept loop, a per-connection
//! task, a bounded first-request line, then command dispatch. Most commands
//! are one request line → one response line; a `pair` session holds a
//! process-wide single-flight guard and streams pairing events while reading
//! further request lines from the same connection.
//!
//! Hostile input is handled by construction: the first line is bounded by
//! [`FIRST_LINE_TIMEOUT`] and [`MAX_REQUEST_LINE`], malformed JSON gets one
//! `Error` line then close, and no path can panic on client bytes.

use std::sync::Arc;

use remora_bridge::{fingerprint, BridgeEvent, BridgeHealth, PairingCommand, PairingOutcome};
use remora_protocol::DeviceId;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::daemon::{DaemonState, FIRST_LINE_TIMEOUT, MAX_REQUEST_LINE};
use crate::proto::{CtlRequest, CtlResponse, DeviceDto, RelayStateDto};

pub async fn serve_ctl(listener: UnixListener, state: DaemonState, shutdown: CancellationToken) {
    // Process-wide single-flight guard for pairing sessions (D13a): at most one
    // `pair` may hold the relay's single pairing window at a time. Held across
    // the WHOLE pair session (not just the open), so a second `pair` is refused
    // until the first connection ends.
    let pair_lock = Arc::new(Mutex::new(()));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let state = state.clone();
                        let pair_lock = Arc::clone(&pair_lock);
                        let conn_shutdown = shutdown.clone();
                        tokio::spawn(handle_conn(stream, state, pair_lock, conn_shutdown));
                    }
                    // A transient accept error (e.g. EMFILE) must not kill the
                    // server; the next loop iteration re-arms accept, and the
                    // shutdown branch still wins a cancel.
                    Err(_) => continue,
                }
            }
        }
    }
}

/// D1/G6: reads one request line with the byte cap enforced at the reader
/// itself. `take(MAX_REQUEST_LINE + 1)` stops the kernel-level reads at the
/// cap, so a no-newline flood can never grow the buffer past it — `read_line`
/// sees EOF at the limit and returns. The caller detects the breach as
/// `line.len() > MAX_REQUEST_LINE`; a shorter newline-less line is a genuine
/// client close. The per-call `take` on `&mut reader` resets the budget each
/// line and keeps the BufReader (and any pipelined bytes) intact.
async fn bounded_read_line<R>(reader: &mut R, line: &mut String) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    (&mut *reader)
        .take(MAX_REQUEST_LINE as u64 + 1)
        .read_line(line)
        .await
}

/// Writes one response as a single JSON line and flushes (the client blocks on
/// lines). Serialization of these owned types cannot realistically fail; a
/// fallback line keeps the signature infallible-of-serde.
async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, resp: &CtlResponse) -> std::io::Result<()> {
    let mut line = serde_json::to_string(resp).unwrap_or_else(|_| {
        "{\"event\":\"error\",\"message\":\"internal serialization error\"}".to_string()
    });
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// Human label for the current relay state, used inside the health-gate error
/// messages (D13c).
fn health_label(health: &BridgeHealth) -> &'static str {
    match health {
        BridgeHealth::Starting => "starting",
        BridgeHealth::Connected { .. } => "connected",
        BridgeHealth::Reconnecting { .. } => "reconnecting",
        BridgeHealth::Rejected { .. } => "rejected",
        // `BridgeHealth` is #[non_exhaustive]; a future variant added in this
        // same image reads as "unknown" until this arm is taught about it.
        _ => "unknown",
    }
}

fn health_to_dto(health: &BridgeHealth) -> RelayStateDto {
    match *health {
        BridgeHealth::Starting => RelayStateDto::Starting,
        BridgeHealth::Connected { since } => RelayStateDto::Connected { since },
        BridgeHealth::Reconnecting { since, attempts } => {
            RelayStateDto::Reconnecting { since, attempts }
        }
        BridgeHealth::Rejected { at, ref detail } => RelayStateDto::Rejected {
            at,
            detail: detail.clone(),
        },
        // `BridgeHealth` is #[non_exhaustive]: a not-yet-mapped future variant
        // reads as the neutral "starting" until this mirror is extended.
        _ => RelayStateDto::Starting,
    }
}

async fn handle_conn(
    stream: tokio::net::UnixStream,
    state: DaemonState,
    pair_lock: Arc<Mutex<()>>,
    shutdown: CancellationToken,
) {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();

    // First request line: bounded by both a timeout and a size cap (G6). A
    // breach of either gets one Error line then close. The cap is enforced
    // inside the read itself (see `bounded_read_line`), not merely checked
    // after — a no-newline flood stops at the cap, not at the timeout.
    let read = tokio::time::timeout(
        FIRST_LINE_TIMEOUT,
        bounded_read_line(&mut reader, &mut line),
    )
    .await;
    let n = match read {
        Err(_elapsed) => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: "request timed out".to_string(),
                },
            )
            .await;
            return;
        }
        // A read error before any bytes: nothing to say, just close.
        Ok(Err(_)) => return,
        Ok(Ok(n)) => n,
    };
    if n == 0 {
        return; // client closed without sending a request
    }
    if line.len() > MAX_REQUEST_LINE {
        let _ = send(
            &mut wr,
            &CtlResponse::Error {
                message: "request line too large".to_string(),
            },
        )
        .await;
        return;
    }

    let request: CtlRequest = match serde_json::from_str(line.trim_end()) {
        Ok(req) => req,
        Err(_) => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: "malformed request".to_string(),
                },
            )
            .await;
            return;
        }
    };

    match request {
        CtlRequest::Status => {
            let health = state.health.borrow().clone();
            let _ = send(
                &mut wr,
                &CtlResponse::Status {
                    relay: health_to_dto(&health),
                    device_id: state.device_id.clone(),
                    fingerprint: state.fingerprint.clone(),
                },
            )
            .await;
        }
        CtlRequest::Devices => {
            let devices = {
                let roster = state.roster.read().await;
                roster
                    .entries
                    .iter()
                    .map(|e| DeviceDto {
                        device_id: e.device_id.to_string(),
                        name: e.name.clone(),
                        fingerprint: fingerprint(&e.static_pubkey),
                        enrolled_at: e.enrolled_at,
                        last_connected_at: e.last_connected_at,
                    })
                    .collect()
            };
            let _ = send(&mut wr, &CtlResponse::Devices { devices }).await;
        }
        CtlRequest::Fingerprint => {
            let _ = send(
                &mut wr,
                &CtlResponse::Fingerprint {
                    device_id: state.device_id.clone(),
                    fingerprint: state.fingerprint.clone(),
                },
            )
            .await;
        }
        CtlRequest::Revoke { device_id } => {
            handle_revoke(&mut wr, &state, device_id).await;
        }
        CtlRequest::PairOpen { ttl_secs } => {
            handle_pair(reader, wr, state, pair_lock, ttl_secs, shutdown).await;
        }
        // A pairing sub-command is only meaningful inside an open pair session,
        // which starts with `PairOpen` on this same connection.
        CtlRequest::PairConfirm { .. } | CtlRequest::PairReject { .. } | CtlRequest::PairCancel => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: "no pairing session is open on this connection".to_string(),
                },
            )
            .await;
        }
    }
}

async fn handle_revoke<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    state: &DaemonState,
    device_id: String,
) {
    // Health-gate FIRST (D13c): revoke needs the live relay connection to kick
    // the device, so a disconnected bridge fails fast and clearly — before we
    // even parse the id.
    let health = state.health.borrow().clone();
    if !matches!(health, BridgeHealth::Connected { .. }) {
        let _ = send(
            wr,
            &CtlResponse::Error {
                message: format!(
                    "bridge is not connected to the relay ({}); revoke needs the live \
                     connection to kick the device",
                    health_label(&health)
                ),
            },
        )
        .await;
        return;
    }
    let id = match device_id.parse::<DeviceId>() {
        Ok(id) => id,
        Err(e) => {
            let _ = send(
                wr,
                &CtlResponse::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .commands
        .send(PairingCommand::Revoke {
            device_id: id,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        let _ = send(
            wr,
            &CtlResponse::Error {
                message: "bridge engine is not running".to_string(),
            },
        )
        .await;
        return;
    }
    let resp = match reply_rx.await {
        Ok(Ok(())) => CtlResponse::Ok,
        // The engine's error (e.g. unknown device id) surfaces verbatim (G7).
        Ok(Err(e)) => CtlResponse::Error {
            message: e.to_string(),
        },
        Err(_) => CtlResponse::Error {
            message: "bridge engine dropped the request".to_string(),
        },
    };
    let _ = send(wr, &resp).await;
}

/// One reader-task message: read requests off the connection in a dedicated
/// task so the main `select!` never cancels a mid-flight `read_line` (which is
/// not cancellation-safe).
enum PairRead {
    Req(CtlRequest),
    Malformed,
    Oversize,
    Closed,
}

async fn handle_pair(
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut wr: tokio::net::unix::OwnedWriteHalf,
    state: DaemonState,
    pair_lock: Arc<Mutex<()>>,
    ttl_secs: u64,
    shutdown: CancellationToken,
) {
    // Single-flight: hold the guard for the whole session. A second `pair`
    // while one is live is refused, not queued.
    let _guard = match pair_lock.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: "another pairing session is active".to_string(),
                },
            )
            .await;
            return;
        }
    };

    // Health-gate (D13c): opening a window needs the live relay connection.
    let health = state.health.borrow().clone();
    if !matches!(health, BridgeHealth::Connected { .. }) {
        let _ = send(
            &mut wr,
            &CtlResponse::Error {
                message: format!(
                    "bridge is not connected to the relay ({}); pairing needs the live \
                     relay connection",
                    health_label(&health)
                ),
            },
        )
        .await;
        return;
    }

    // Subscribe BEFORE sending OpenWindow so no arrival can slip in between the
    // command and our subscription (closes the arrival-before-subscribe race).
    let mut events = state.events.subscribe();

    let (reply_tx, reply_rx) = oneshot::channel();
    if state
        .commands
        .send(PairingCommand::OpenWindow {
            ttl_secs,
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        let _ = send(
            &mut wr,
            &CtlResponse::Error {
                message: "bridge engine is not running".to_string(),
            },
        )
        .await;
        return;
    }
    // The window did not open on error, so there is nothing to cancel here.
    let code = match reply_rx.await {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
        Err(_) => {
            let _ = send(
                &mut wr,
                &CtlResponse::Error {
                    message: "bridge engine dropped the request".to_string(),
                },
            )
            .await;
            return;
        }
    };

    // The authoritative `expires_at` rides on the PairingWindowOpened event,
    // emitted before any arrival on the same in-order channel.
    let expires_at = loop {
        match events.recv().await {
            Ok(BridgeEvent::PairingWindowOpened { expires_at, .. }) => break expires_at,
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let _ = send(
                    &mut wr,
                    &CtlResponse::Error {
                        message: "event stream lagged; re-run pair".to_string(),
                    },
                )
                .await;
                let _ = state.commands.send(PairingCommand::CancelWindow).await;
                return;
            }
            Err(broadcast::error::RecvError::Closed) => {
                let _ = state.commands.send(PairingCommand::CancelWindow).await;
                return;
            }
        }
    };

    if send(
        &mut wr,
        &CtlResponse::WindowOpened {
            code: code.encode(),
            expires_at,
        },
    )
    .await
    .is_err()
    {
        // The client vanished before we could announce the window; fail safe.
        let _ = state.commands.send(PairingCommand::CancelWindow).await;
        return;
    }

    // Read further request lines in a dedicated task (no read timeout in this
    // human-paced phase — bounded by the client-side window deadline, D13a;
    // each line is still byte-capped via `bounded_read_line`).
    let (line_tx, mut line_rx) = mpsc::channel::<PairRead>(4);
    let reader_task = tokio::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            line.clear();
            match bounded_read_line(&mut reader, &mut line).await {
                Ok(0) => {
                    let _ = line_tx.send(PairRead::Closed).await;
                    return;
                }
                Ok(_) => {
                    if line.len() > MAX_REQUEST_LINE {
                        let _ = line_tx.send(PairRead::Oversize).await;
                        return;
                    }
                    let msg = match serde_json::from_str::<CtlRequest>(line.trim_end()) {
                        Ok(req) => PairRead::Req(req),
                        Err(_) => PairRead::Malformed,
                    };
                    if line_tx.send(msg).await.is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = line_tx.send(PairRead::Closed).await;
                    return;
                }
            }
        }
    });

    // `got_result` gates the fail-safe: a PairResult means the window already
    // resolved, so we must NOT cancel it; every other exit path (close, cancel,
    // lag, shutdown) sends CancelWindow.
    let mut got_result = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            event = events.recv() => match event {
                Ok(BridgeEvent::PairingDeviceArrived { device_id, name, fingerprint }) => {
                    if send(
                        &mut wr,
                        &CtlResponse::DeviceArrived {
                            device_id: device_id.to_string(),
                            name,
                            fingerprint,
                        },
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
                Ok(BridgeEvent::PairingResult(outcome)) => {
                    let _ = send(&mut wr, &pair_result_dto(&outcome)).await;
                    got_result = true;
                    break;
                }
                // PairingWindowOpened (a replacement) / RosterChanged: not part
                // of this session's stream — ignore.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = send(
                        &mut wr,
                        &CtlResponse::Error {
                            message: "event stream lagged; re-run pair".to_string(),
                        },
                    )
                    .await;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            read = line_rx.recv() => match read {
                None | Some(PairRead::Closed) => break,
                Some(PairRead::Oversize) => {
                    let _ = send(
                        &mut wr,
                        &CtlResponse::Error {
                            message: "request line too large".to_string(),
                        },
                    )
                    .await;
                    break;
                }
                Some(PairRead::Malformed) => {
                    let _ = send(
                        &mut wr,
                        &CtlResponse::Error {
                            message: "malformed request".to_string(),
                        },
                    )
                    .await;
                }
                Some(PairRead::Req(CtlRequest::PairConfirm { device_id })) => {
                    route_decision(&mut wr, &state, device_id, true).await;
                }
                Some(PairRead::Req(CtlRequest::PairReject { device_id })) => {
                    route_decision(&mut wr, &state, device_id, false).await;
                }
                Some(PairRead::Req(CtlRequest::PairCancel)) => break,
                Some(PairRead::Req(_)) => {
                    let _ = send(
                        &mut wr,
                        &CtlResponse::Error {
                            message: "unexpected request during pairing".to_string(),
                        },
                    )
                    .await;
                }
            },
        }
    }

    // The session is over: kill the reader task outright (dropping `wr` below
    // closes only the write half — a stalled client that never closes its end
    // would otherwise keep the task parked in read_line forever).
    reader_task.abort();

    // Fail-safe: any abnormal exit (connection close, explicit cancel, lag,
    // shutdown) closes the relay window so it never dangles.
    if !got_result {
        let _ = state.commands.send(PairingCommand::CancelWindow).await;
    }
}

/// Parses the CLI-supplied device id and routes a confirm/reject decision into
/// the engine. A malformed id is reported as an Error line, never a panic.
async fn route_decision<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    state: &DaemonState,
    device_id: String,
    confirm: bool,
) {
    let id = match device_id.parse::<DeviceId>() {
        Ok(id) => id,
        Err(e) => {
            let _ = send(
                wr,
                &CtlResponse::Error {
                    message: e.to_string(),
                },
            )
            .await;
            return;
        }
    };
    let cmd = if confirm {
        PairingCommand::Confirm { device_id: id }
    } else {
        PairingCommand::Reject { device_id: id }
    };
    let _ = state.commands.send(cmd).await;
}

fn pair_result_dto(outcome: &PairingOutcome) -> CtlResponse {
    match outcome {
        PairingOutcome::Paired { device_id, name } => CtlResponse::PairResult {
            outcome: "paired".to_string(),
            device_id: Some(device_id.to_string()),
            name: Some(name.clone()),
        },
        PairingOutcome::Rejected { device_id } => CtlResponse::PairResult {
            outcome: "rejected".to_string(),
            device_id: Some(device_id.to_string()),
            name: None,
        },
        PairingOutcome::Expired => CtlResponse::PairResult {
            outcome: "expired".to_string(),
            device_id: None,
            name: None,
        },
        // `PairingOutcome` is #[non_exhaustive]: any future terminal state maps
        // to the safe "window is gone" reading until taught otherwise.
        _ => CtlResponse::PairResult {
            outcome: "expired".to_string(),
            device_id: None,
            name: None,
        },
    }
}
