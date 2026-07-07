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

    // The authoritative `expires_at` and window generation ride on the
    // PairingWindowOpened event, emitted before any arrival on the same
    // in-order channel. Match OUR window by its code (each mint is a fresh
    // random secret): a stale WindowOpened still in flight from an earlier
    // session must not be adopted, or this session would record the wrong
    // generation and expiry (#299).
    let (expires_at, generation) = loop {
        match events.recv().await {
            Ok(BridgeEvent::PairingWindowOpened {
                code: opened,
                expires_at,
                generation,
            }) if opened == code => break (expires_at, generation),
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
                Ok(event) => match classify_pair_event(event, generation) {
                    PairEvent::Arrived { device_id, name, fingerprint } => {
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
                    PairEvent::Result(outcome) => {
                        let _ = send(&mut wr, &pair_result_dto(&outcome)).await;
                        got_result = true;
                        break;
                    }
                    // Stale-generation pairing events (a replaced window's
                    // async Expired, #299), window replacements, roster pings:
                    // not this session's stream — keep waiting.
                    PairEvent::Ignore => {}
                },
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

/// What one [`BridgeEvent`] means for a pair session bound to a window
/// generation (#299).
#[derive(Debug, PartialEq)]
enum PairEvent {
    /// An arrival on THIS session's window: prompt the operator.
    Arrived {
        device_id: DeviceId,
        name: String,
        fingerprint: String,
    },
    /// THIS session's window reached a terminal state: report and end.
    Result(PairingOutcome),
    /// Anything else — pairing events tagged with another window's generation
    /// (a replaced window's responder task reports asynchronously, so its
    /// `Expired` or a late arrival can land after this session's own
    /// WindowOpened), window replacements, roster pings, future variants.
    Ignore,
}

/// Classifies one bridge event against the session's window generation
/// (learned from its own `PairingWindowOpened`). A stale event from another
/// generation must never terminalize — or prompt inside — a fresh session; the
/// caller keeps waiting on `Ignore`, still bounded by the client-side window
/// deadline (D13a).
fn classify_pair_event(event: BridgeEvent, session_generation: u64) -> PairEvent {
    match event {
        BridgeEvent::PairingDeviceArrived {
            generation,
            device_id,
            name,
            fingerprint,
        } if generation == session_generation => PairEvent::Arrived {
            device_id,
            name,
            fingerprint,
        },
        BridgeEvent::PairingResult {
            generation,
            outcome,
        } if generation == session_generation => PairEvent::Result(outcome),
        _ => PairEvent::Ignore,
    }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use remora_bridge::Roster;
    use remora_protocol::{PairingCode, PROTOCOL_VERSION};
    use tokio::io::AsyncBufReadExt;
    use tokio::sync::{watch, RwLock};

    use super::*;

    fn device(seed: u8) -> DeviceId {
        DeviceId([seed; 32])
    }

    fn arrived(generation: u64, seed: u8) -> BridgeEvent {
        BridgeEvent::PairingDeviceArrived {
            generation,
            device_id: device(seed),
            name: "phone".to_string(),
            fingerprint: "aa:bb".to_string(),
        }
    }

    fn result(generation: u64, outcome: PairingOutcome) -> BridgeEvent {
        BridgeEvent::PairingResult {
            generation,
            outcome,
        }
    }

    // ---- classify_pair_event: the #299 filter decision, in isolation. ----

    #[test]
    fn stale_generation_result_is_ignored() {
        // The issue's exact hazard: the replaced window's async Expired must
        // not terminalize a session bound to a newer generation.
        assert_eq!(
            classify_pair_event(result(1, PairingOutcome::Expired), 2),
            PairEvent::Ignore
        );
    }

    #[test]
    fn own_generation_result_terminates() {
        assert_eq!(
            classify_pair_event(result(2, PairingOutcome::Expired), 2),
            PairEvent::Result(PairingOutcome::Expired)
        );
    }

    #[test]
    fn stale_generation_arrival_is_ignored() {
        assert_eq!(classify_pair_event(arrived(1, 0xEE), 2), PairEvent::Ignore);
    }

    #[test]
    fn own_generation_arrival_prompts() {
        match classify_pair_event(arrived(2, 0xAB), 2) {
            PairEvent::Arrived { device_id, .. } => assert_eq!(device_id, device(0xAB)),
            other => panic!("expected Arrived, got {other:?}"),
        }
    }

    #[test]
    fn non_pairing_events_are_ignored() {
        assert_eq!(
            classify_pair_event(BridgeEvent::RosterChanged, 2),
            PairEvent::Ignore
        );
        // A window replacement mid-session (any generation) is not part of
        // this session's stream either.
        assert_eq!(
            classify_pair_event(
                BridgeEvent::PairingWindowOpened {
                    code: test_code(9),
                    expires_at: far_future(),
                    generation: 3,
                },
                2
            ),
            PairEvent::Ignore
        );
    }

    // ---- handle_pair end-to-end over a socketpair, with a scripted engine:
    // the back-to-back scenario from #299, made deterministic. ----

    fn test_code(seed: u8) -> PairingCode {
        PairingCode {
            relay_url: Some("ws://127.0.0.1:1".to_string()),
            rendezvous_token: Some(format!("tok-{seed}")),
            mesh_addr: None,
            psk: [seed; 32],
            bridge_id: device(9),
            bridge_key: [8u8; 32],
            bridge_name: None,
            min_protocol: PROTOCOL_VERSION,
        }
    }

    fn far_future() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + 3600
    }

    /// A DaemonState backed by a scripted fake engine: the test drives
    /// `commands_rx` (replies to OpenWindow) and `events_tx` (the broadcast
    /// stream) by hand.
    fn test_state() -> (
        DaemonState,
        mpsc::Receiver<PairingCommand>,
        broadcast::Sender<BridgeEvent>,
    ) {
        let (commands_tx, commands_rx) = mpsc::channel::<PairingCommand>(8);
        let (events_tx, _) = broadcast::channel::<BridgeEvent>(64);
        // The sender may drop: a watch receiver keeps serving the last value.
        let (_health_tx, health_rx) = watch::channel(BridgeHealth::Connected { since: 0 });
        let state = DaemonState {
            commands: commands_tx,
            events: events_tx.clone(),
            health: health_rx,
            roster: Arc::new(RwLock::new(Roster::default())),
            device_id: "bridge-id".to_string(),
            fingerprint: "br:fp".to_string(),
        };
        (state, commands_rx, events_tx)
    }

    /// Spawns `handle_pair` over a fresh socketpair, returning the client end
    /// (as a line reader) and the session task handle.
    fn spawn_session(
        state: DaemonState,
        ttl_secs: u64,
    ) -> (
        tokio::io::Lines<tokio::io::BufReader<tokio::net::UnixStream>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (client, server) = tokio::net::UnixStream::pair().expect("socketpair");
        let (srd, swr) = server.into_split();
        let session = tokio::spawn(handle_pair(
            BufReader::new(srd),
            swr,
            state,
            Arc::new(Mutex::new(())),
            ttl_secs,
            CancellationToken::new(),
        ));
        let lines = tokio::io::BufReader::new(client).lines();
        (lines, session)
    }

    /// Reads and decodes the next ctl response line, bounded so a regression
    /// hangs the assertion, not CI.
    async fn next_resp(
        lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::UnixStream>>,
    ) -> CtlResponse {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("response within deadline")
            .expect("read response line")
            .expect("connection stayed open");
        serde_json::from_str(&line).expect("decode ctl response")
    }

    /// Receives the session's OpenWindow command and replies with `code`,
    /// completing the scripted engine's open phase. Events may only be
    /// broadcast after this returns: the session subscribes before sending
    /// OpenWindow, so ordering is deterministic.
    async fn answer_open_window(
        commands_rx: &mut mpsc::Receiver<PairingCommand>,
        code: &PairingCode,
    ) {
        let cmd = tokio::time::timeout(Duration::from_secs(10), commands_rx.recv())
            .await
            .expect("OpenWindow within deadline")
            .expect("command channel open");
        match cmd {
            PairingCommand::OpenWindow { reply, .. } => {
                reply.send(Ok(code.clone())).expect("reply accepted");
            }
            other => panic!("expected OpenWindow, got {other:?}"),
        }
    }

    // #299: session 2 opens its window, then the OLD window's asynchronous
    // Expired (and a stale arrival) land after its own WindowOpened. The
    // session must NOT report "expired" — it keeps waiting and reaches its
    // real outcome.
    #[tokio::test]
    async fn stale_expired_from_a_replaced_window_does_not_terminalize_a_fresh_session() {
        let (state, mut commands_rx, events_tx) = test_state();
        let (mut lines, session) = spawn_session(state, 30);

        let code = test_code(2);
        answer_open_window(&mut commands_rx, &code).await;

        // This session's window: generation 2.
        events_tx
            .send(BridgeEvent::PairingWindowOpened {
                code: code.clone(),
                expires_at: far_future(),
                generation: 2,
            })
            .expect("send opened");
        // The replaced window's responder task reports asynchronously — the
        // millisecond race from #299, scripted deterministically: its Expired
        // (and a stale arrival) arrive AFTER the fresh WindowOpened.
        events_tx
            .send(result(1, PairingOutcome::Expired))
            .expect("send stale expired");
        events_tx
            .send(arrived(1, 0xEE))
            .expect("send stale arrival");
        // The fresh window's real ceremony.
        events_tx.send(arrived(2, 0xAB)).expect("send arrival");
        events_tx
            .send(result(
                2,
                PairingOutcome::Paired {
                    device_id: device(0xAB),
                    name: "phone".to_string(),
                },
            ))
            .expect("send paired");

        match next_resp(&mut lines).await {
            CtlResponse::WindowOpened { code: c, .. } => assert_eq!(c, code.encode()),
            other => panic!("expected WindowOpened, got {other:?}"),
        }
        // The very next line must be THIS window's arrival — a stale Expired
        // (or stale arrival) surfacing here is the #299 bug.
        match next_resp(&mut lines).await {
            CtlResponse::DeviceArrived { device_id, .. } => {
                assert_eq!(device_id, device(0xAB).to_string());
            }
            other => panic!("stale event leaked into the fresh session: {other:?}"),
        }
        match next_resp(&mut lines).await {
            CtlResponse::PairResult { outcome, .. } => assert_eq!(outcome, "paired"),
            other => panic!("expected PairResult, got {other:?}"),
        }

        session.await.expect("session task");
        // The session resolved for real, so the fail-safe must NOT have
        // cancelled the (already consumed) window.
        assert!(
            commands_rx.try_recv().is_err(),
            "no CancelWindow after a genuine result"
        );
    }

    // A stale WindowOpened still in flight from an earlier session must not be
    // adopted: the session waits for the event carrying ITS code, so it binds
    // to the right generation and deadline.
    #[tokio::test]
    async fn stale_window_opened_is_not_adopted_by_a_fresh_session() {
        let (state, mut commands_rx, events_tx) = test_state();
        let (mut lines, session) = spawn_session(state, 30);

        let code = test_code(2);
        answer_open_window(&mut commands_rx, &code).await;

        let ours = far_future();
        // An earlier window's WindowOpened (different code, older generation)
        // delivered late — before our own.
        events_tx
            .send(BridgeEvent::PairingWindowOpened {
                code: test_code(1),
                expires_at: 111,
                generation: 1,
            })
            .expect("send stale opened");
        events_tx
            .send(BridgeEvent::PairingWindowOpened {
                code: code.clone(),
                expires_at: ours,
                generation: 2,
            })
            .expect("send opened");
        // Prove the session bound generation 2, not 1: a gen-1 Expired is
        // ignored, a gen-2 result terminates.
        events_tx
            .send(result(1, PairingOutcome::Expired))
            .expect("send stale expired");
        events_tx
            .send(result(
                2,
                PairingOutcome::Rejected {
                    device_id: device(0xAB),
                },
            ))
            .expect("send rejected");

        match next_resp(&mut lines).await {
            CtlResponse::WindowOpened {
                code: c,
                expires_at,
            } => {
                assert_eq!(c, code.encode(), "the session announces ITS window");
                assert_eq!(
                    expires_at, ours,
                    "the deadline is ITS window's, not the stale one"
                );
            }
            other => panic!("expected WindowOpened, got {other:?}"),
        }
        match next_resp(&mut lines).await {
            CtlResponse::PairResult { outcome, .. } => {
                assert_eq!(
                    outcome, "rejected",
                    "gen-2 result, not the stale gen-1 expired"
                );
            }
            other => panic!("expected PairResult, got {other:?}"),
        }

        session.await.expect("session task");
    }
}
