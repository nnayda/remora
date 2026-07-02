//! Transport-agnostic bridge: spawn a child in a local PTY and expose it as
//! a [`SessionChannel`]. Reused by every PTY-backed transport (ssh now,
//! kubectl later) — the cheap early check that the seam isn't ssh-shaped.
//!
//! Threads are DETACHED and self-reaping. A `SessionChannel` owns only the
//! two mpsc ends, so it cannot hold the child, the PTY master, or the thread
//! handles. Dropping the channel SIGNALS teardown; the child is reaped
//! *eventually* by the writer thread, not synchronously on drop.
//!
//! Teardown is asymmetric by caller activity. The caller's `recv()` returns
//! `None` promptly when the child exits — that is driven by the *reader*
//! thread hitting EOF, independent of caller activity. Reaping the child is
//! the *writer* thread's job, and the writer only checks the reader's death
//! signal before each `blocking_recv` and after each write. So an **active**
//! caller (sending input) gets a prompt reap, while an **idle** caller's
//! writer stays parked in `blocking_recv` until the caller sends or drops the
//! channel (or, for ssh, until `ServerAlive*` keepalive trips). In the local
//! case the child has already exited so nothing leaks; the lingering writer
//! just reaps later. A `tokio::select!`-style writer is the future option if
//! prompt idle-reap is ever required.
//!
//! The writer and detector threads share one `AtomicBool` (#224): the writer
//! sets it after each successful input write, and the detector thread
//! swap-consumes it at every wake to exit a sticky `Awaiting` (see
//! `wake_events`). It is a flag, not a channel: it adds no `output_tx`
//! sender and cannot stall the `recv()→None` teardown.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, sync_channel, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use remora_protocol::{ChannelInput, ChannelOutput};

use crate::activity::{Detector, DetectorEvent};
use crate::{SessionChannel, SourceError};

/// Read buffer per output message. Worst-case queued memory is
/// `CHANNEL_CAPACITY * READ_BUF` (~8 MiB at 256 x 32 KiB).
const READ_BUF: usize = 32 * 1024;

/// Quiet-for-this-long ⇒ settle Working → Idle (parity with #55's 1500ms).
const SETTLE_WINDOW: Duration = Duration::from_millis(1500);
/// Bound for the internal reader→detector hop; backpressure propagates to the PTY.
const DETECT_QUEUE: usize = 64;

/// Initial PTY geometry; the first client `Resize` corrects it. tmux sizes
/// its window to the smallest client, so this only matters until the first
/// resize arrives (stage-7 wiring sends it).
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

/// Spawns `cmd` against a fresh local PTY and bridges it to a
/// `SessionChannel`. See the module docs for the teardown model.
pub fn spawn_pty_channel(cmd: CommandBuilder) -> Result<SessionChannel, SourceError> {
    Ok(spawn_pty_channel_inner(cmd, SETTLE_WINDOW)?.0)
}

/// Test seam: same bridge with an injectable settle window.
#[cfg(test)]
pub(crate) fn spawn_pty_channel_with_settle(
    cmd: CommandBuilder,
    settle: Duration,
) -> Result<SessionChannel, SourceError> {
    Ok(spawn_pty_channel_inner(cmd, settle)?.0)
}

/// Like [`spawn_pty_channel`] but also returns the child's OS pid (when the
/// platform exposes one) so tests can assert the no-leaked-process
/// guarantee. Internal: the public bridge contract is just the channel.
fn spawn_pty_channel_inner(
    cmd: CommandBuilder,
    settle: Duration,
) -> Result<(SessionChannel, Option<u32>), SourceError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| SourceError::Transport(format!("openpty: {e}")))?;

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| SourceError::Transport(format!("spawn: {e}")))?;
    let pid = child.process_id();

    // Release our slave handle so the child holds the only slave fds; when
    // the child exits, the slave closes and the reader observes EOF.
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| SourceError::Transport(format!("clone reader: {e}")))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| SourceError::Transport(format!("take writer: {e}")))?;
    let master = pair.master;

    let (channel, mut input_rx, output_tx) = SessionChannel::pair();

    // Death signal: reader fires this when it exits (EOF from child or
    // caller dropped output). Writer checks it so it can tear down even
    // on platforms where PTY master writes succeed after slave close.
    let (death_tx, death_rx) = std_mpsc::channel::<()>();

    // Internal hop: PTY reader → detector. Bounded so backpressure reaches the
    // PTY (the detector blocks on output_tx under a slow consumer, stops draining
    // this, the reader blocks here, the PTY fills). SyncSender::send blocks when full.
    let (raw_tx, raw_rx) = sync_channel::<Vec<u8>>(DETECT_QUEUE);

    // #224: "user typed since the detector's last wake". Writer sets it after
    // a successful non-empty write (keystroke rate); detector swap-consumes it
    // once per wake (chunk rate — one atomic RMW, noise next to the chunk's
    // vte parse). Correctness never depends on the ordering: the flag is
    // level-triggered, so a missed pairing is observed at the next wake
    // (≤1 settle window). NOT a second output_tx sender and NOT a raw_tx
    // clone — both were rejected to preserve ADR-0013's ordering + teardown
    // invariants.
    let user_input = Arc::new(AtomicBool::new(false));
    let user_input_writer = Arc::clone(&user_input);

    // Reader thread: master output -> raw_tx (NO direct output_tx anymore).
    // The child's stderr is wired to the PTY slave by portable-pty's default,
    // so ssh/tmux diagnostics arrive here in-band (the mechanism optimistic
    // attach relies on).
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if raw_tx.send(buf[..n].to_vec()).is_err() {
                        break; // detector gone (caller dropped the channel)
                    }
                }
            }
        }
        let _ = death_tx.send(()); // signal writer to reap
                                   // raw_tx dropped here -> detector sees Disconnected.
    });

    // Detector thread: SOLE sender to output_tx. recv_timeout is the settle clock.
    std::thread::spawn(move || {
        let mut detector = Detector::new();
        loop {
            match raw_rx.recv_timeout(settle) {
                Ok(bytes) => {
                    // Consume the flag at the wake (after recv returns) so a
                    // keystroke's echo pairs with its own flag in the SAME
                    // wake — the pulse flips promptly instead of one settle
                    // window later. Compute events first (borrow ends), then
                    // move-send bytes (no clone needed). Bytes is still
                    // delivered before the status/preview events that
                    // accompany it. An input-caused Status(Working) may
                    // precede queued cosmetic bytes from before the keystroke
                    // — harmless; ADR-0013's ordering guarantee is about
                    // byte-caused status events.
                    let typed = user_input.swap(false, Ordering::SeqCst);
                    let events = wake_events(&mut detector, typed, Some(&bytes));
                    if output_tx
                        .blocking_send(ChannelOutput::Bytes(bytes))
                        .is_err()
                    {
                        break; // caller dropped the channel
                    }
                    let mut closed = false;
                    for ev in events {
                        if output_tx.blocking_send(ev.into()).is_err() {
                            closed = true;
                            break;
                        }
                    }
                    if closed {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let typed = user_input.swap(false, Ordering::SeqCst);
                    let mut closed = false;
                    for ev in wake_events(&mut detector, typed, None) {
                        if output_tx.blocking_send(ev.into()).is_err() {
                            closed = true;
                            break;
                        }
                    }
                    if closed {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break, // reader EOF
            }
        }
        // output_tx dropped here -> caller's recv() returns None.
    });

    // Writer thread: channel input -> master; owns and reaps the child. The
    // reader unblocks on slave close after this thread's kill+wait (EOF is
    // not delivered by child.kill() alone in portable-pty 0.9). Also checks
    // the death signal from the reader so teardown works even on platforms
    // where PTY master writes do not return EIO after slave close.
    std::thread::spawn(move || {
        let mut child = child;
        loop {
            // Check death signal before blocking; use try_recv so we don't
            // stall. If the signal arrives between this check and blocking_recv
            // the worst case is one extra round-trip before we notice.
            if death_rx.try_recv().is_ok() {
                break;
            }
            match input_rx.blocking_recv() {
                Some(ChannelInput::Bytes(bytes)) => {
                    if writer.write_all(&bytes).is_err() {
                        break; // child/PTY is gone (write returned EIO)
                    }
                    let _ = writer.flush();
                    // #224: signal AFTER the successful write — a failed or
                    // torn-down write must not read as "the user responded".
                    // An empty write carries no user intent (empty paste,
                    // programmatic flush), so it must not exit awaiting.
                    // Resize below deliberately does NOT set this: a resize
                    // causes repaints but is not the user answering.
                    if !bytes.is_empty() {
                        user_input_writer.store(true, Ordering::SeqCst);
                    }
                    // Re-check death signal after each write so we don't spin
                    // sending to a dead PTY on platforms that swallow writes.
                    if death_rx.try_recv().is_ok() {
                        break;
                    }
                }
                Some(ChannelInput::Resize(size)) => {
                    let _ = master.resize(PtySize {
                        rows: size.rows(),
                        cols: size.cols(),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                // ChannelInput is #[non_exhaustive]; ignore unknown messages.
                Some(_) => {}
                None => break, // caller dropped the channel
            }
        }
        // Teardown: kill and reap the child so no ssh process leaks.
        let _ = child.kill();
        let _ = child.wait();
        // writer + master dropped here; input_rx drop signals ChannelClosed
        // to callers of SessionChannel::send_bytes/resize.
    });

    Ok((channel, pid))
}

/// One detector-thread wake: dispatch the received bytes (if any), then the
/// consumed user-input flag, to the detector. Pure so the wake semantics are
/// unit-testable without threads.
///
/// Two subtle rules:
///
/// - **Bytes before input.** A keystroke and an `awaiting` marker can land in
///   the same wake (type-ahead: the user answers just before the marker
///   arrives). Input-first would consume the keystroke as a no-op and then
///   apply the marker — a sticky `Awaiting` that nothing self-heals, the
///   false-hold direction the ADR names as worst. Bytes-first lets the
///   keystroke exit whatever `Awaiting` stands at the end of the wake; at
///   worst that's a fail-soft false exit, corrected by the next marker.
///
/// - **The `None` arm:** a silent wake WITH user input calls `on_user_input`
///   INSTEAD of `on_tick`. Typing counts as activity — settling in the same
///   wake would churn a sticky `Awaiting` through `Working` straight to
///   `Idle` when the keystroke produced no output (echo-off TUIs), and would
///   decay a live `Working` under a quietly typing user.
fn wake_events(
    detector: &mut Detector,
    user_input: bool,
    bytes: Option<&[u8]>,
) -> Vec<DetectorEvent> {
    let mut events = Vec::new();
    match bytes {
        Some(chunk) => events.extend(detector.on_bytes(chunk)),
        None if !user_input => events.extend(detector.on_tick()),
        None => {} // user input counts as activity; skip this wake's settle
    }
    if user_input {
        events.extend(detector.on_user_input());
    }
    events
}

#[cfg(test)]
mod wake_tests {
    use super::*;
    use remora_protocol::SessionStatus;

    const AWAITING_MARKER: &[u8] = b"\x1b]7366;remora;1;state;YXdhaXRpbmdfaW5wdXQ=\x07";

    fn statuses(evs: Vec<DetectorEvent>) -> Vec<SessionStatus> {
        evs.into_iter()
            .filter_map(|e| match e {
                DetectorEvent::Status(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn awaiting_detector() -> Detector {
        let mut d = Detector::new();
        d.on_bytes(AWAITING_MARKER);
        d
    }

    #[test]
    fn user_input_on_a_silent_wake_exits_awaiting_without_settling() {
        // Timeout arm with the flag set: on_user_input INSTEAD of on_tick,
        // or a no-echo keystroke would churn Awaiting→Working→Idle in one
        // wake. Idle arrives at the NEXT silent wake.
        let mut d = awaiting_detector();
        assert_eq!(
            statuses(wake_events(&mut d, true, None)),
            vec![SessionStatus::Working]
        );
        assert_eq!(
            statuses(wake_events(&mut d, false, None)),
            vec![SessionStatus::Idle]
        );
    }

    #[test]
    fn user_input_with_bytes_emits_working_exactly_once() {
        // The echo path: on_bytes runs first (sticky no-op while Awaiting),
        // then on_user_input exits — one Status(Working), not two.
        let mut d = awaiting_detector();
        assert_eq!(
            statuses(wake_events(&mut d, true, Some(b"echoed keystroke"))),
            vec![SessionStatus::Working]
        );
    }

    #[test]
    fn silent_wake_without_user_input_still_settles_working_to_idle() {
        let mut d = Detector::new();
        wake_events(&mut d, false, Some(b"output")); // → Working
        assert_eq!(
            statuses(wake_events(&mut d, false, None)),
            vec![SessionStatus::Idle]
        );
    }

    #[test]
    fn user_input_while_working_skips_the_settle_tick() {
        // Typing counts as activity: a silent wake with input must not decay
        // Working → Idle (parity with how echo bytes would refresh Working).
        let mut d = Detector::new();
        wake_events(&mut d, false, Some(b"output")); // → Working
        assert_eq!(statuses(wake_events(&mut d, true, None)), vec![]);
        // The following silent wake settles as usual.
        assert_eq!(
            statuses(wake_events(&mut d, false, None)),
            vec![SessionStatus::Idle]
        );
    }

    #[test]
    fn user_input_exits_a_marker_asserted_in_the_same_wake() {
        // Type-ahead: keystroke and awaiting marker land in one wake. Bytes
        // dispatch first (marker re-asserts Awaiting — no churn from the
        // already-Awaiting state), then the input exits it. Preferring the
        // exit is the fail-soft direction: a swallowed answer would leave a
        // stuck-red Awaiting nothing self-heals, while a premature exit is
        // corrected by the agent's next marker.
        let mut d = awaiting_detector();
        assert_eq!(
            statuses(wake_events(&mut d, true, Some(AWAITING_MARKER))),
            vec![SessionStatus::Working]
        );
    }

    #[test]
    fn terminal_marker_in_the_wake_still_wins_over_user_input() {
        // A chunk carrying an idle marker + the input flag: the marker's exit
        // stands (input is a no-op outside Awaiting) — input never overrides
        // an agent-asserted terminal state.
        let mut d = awaiting_detector();
        let idle_marker: &[u8] = b"\x1b]7366;remora;1;state;aWRsZQ==\x07";
        assert_eq!(
            statuses(wake_events(&mut d, true, Some(idle_marker))),
            vec![SessionStatus::Idle]
        );
    }
}

impl From<DetectorEvent> for ChannelOutput {
    fn from(ev: DetectorEvent) -> Self {
        match ev {
            DetectorEvent::Status(s) => ChannelOutput::StatusChange(s),
            DetectorEvent::Preview(t) => ChannelOutput::PreviewUpdate(t.into_string()),
            DetectorEvent::MarkerSeen => ChannelOutput::MarkerSeen,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use remora_protocol::TerminalSize;
    use std::process::Command as StdCommand;
    use std::time::{Duration, Instant};

    #[test]
    fn marker_seen_event_maps_to_channel_output() {
        use crate::activity::DetectorEvent;
        assert_eq!(
            ChannelOutput::from(DetectorEvent::MarkerSeen),
            ChannelOutput::MarkerSeen
        );
    }

    /// POSIX liveness probe without `libc`/`unsafe`: `kill -0` succeeds iff
    /// the process still exists.
    fn pid_alive(pid: u32) -> bool {
        StdCommand::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Drains output (bounded by a timeout) until `needle` appears; panics on
    /// early close or timeout.
    async fn recv_until_contains(channel: &mut SessionChannel, needle: &[u8]) -> Vec<u8> {
        let mut acc = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(b))) => {
                    acc.extend_from_slice(&b);
                    if acc.windows(needle.len()).any(|w| w == needle) {
                        return acc;
                    }
                }
                Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
                Ok(None) => panic!("channel closed before seeing {needle:?}; got {acc:?}"),
                Err(_) => panic!("timed out waiting for {needle:?}; got {acc:?}"),
            }
        }
    }

    #[tokio::test]
    async fn bytes_round_trip_through_pty() {
        // `cat` keeps the PTY open and returns what we send (via the line
        // discipline echo and/or cat itself) — either way the bytes make the
        // round trip channel -> master -> channel.
        let channel = spawn_pty_channel(CommandBuilder::new("cat")).expect("spawn");
        let mut channel = channel;
        channel
            .send_bytes(b"remora-ping\n".to_vec())
            .await
            .expect("send");
        let got = recv_until_contains(&mut channel, b"remora-ping").await;
        assert!(got.windows(11).any(|w| w == b"remora-ping"));
    }

    #[tokio::test]
    async fn clean_child_exit_closes_channel() {
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("remora-done");
        let mut channel = spawn_pty_channel(cmd).expect("spawn");

        // Output is delivered, then the channel reports death (None). tokio
        // mpsc drains queued items before reporting closed (EOF-flush order).
        let mut acc = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(b))) => acc.extend_from_slice(&b),
                Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
                Ok(None) => break,
                Err(_) => panic!("timed out; got {acc:?}"),
            }
        }
        assert!(acc.windows(11).any(|w| w == b"remora-done"), "got {acc:?}");

        // Death on the send side is EVENTUAL: the writer notices the dead
        // PTY only on its next write, so the first queued send may succeed
        // and a later one returns ChannelClosed. We allow a generous window
        // because the PTY slave closes asynchronously after child exit and
        // the writer thread may not observe EIO immediately.
        let mut closed = false;
        for _ in 0..40 {
            if matches!(
                channel.send_bytes(b"x".to_vec()).await,
                Err(SourceError::ChannelClosed)
            ) {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(closed, "send did not report ChannelClosed after child exit");

        // Once the writer thread is gone, resize is closed too.
        let size = TerminalSize::new(30, 100).expect("nonzero");
        assert!(matches!(
            channel.resize(size).await,
            Err(SourceError::ChannelClosed)
        ));
    }

    #[tokio::test]
    async fn child_stderr_is_in_band() {
        // tmux writes "can't find session" to stderr; optimistic attach
        // relies on stderr being visible on the channel. Pin that here.
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "printf remora-err 1>&2"]);
        let mut channel = spawn_pty_channel(cmd).expect("spawn");
        let got = recv_until_contains(&mut channel, b"remora-err").await;
        assert!(got.windows(10).any(|w| w == b"remora-err"));
    }

    #[tokio::test]
    async fn slow_reader_loses_no_output() {
        // 12 MiB of NUL > CHANNEL_CAPACITY * READ_BUF, so the reader fills
        // the output queue and BLOCKS while we don't read; nothing may be
        // dropped or reordered. NUL avoids PTY output translation (ONLCR).
        const TOTAL: usize = 12 * 1024 * 1024;
        let mut cmd = CommandBuilder::new("head");
        cmd.args(["-c", &TOTAL.to_string(), "/dev/zero"]);
        let mut channel = spawn_pty_channel(cmd).expect("spawn");

        // Pause without reading so the reader hits the full-queue block.
        std::thread::sleep(Duration::from_millis(200));

        let mut total = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(b))) => {
                    assert!(b.iter().all(|&x| x == 0), "unexpected non-NUL byte");
                    total += b.len();
                }
                Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
                Ok(None) => break,
                Err(_) => panic!("timed out draining; got {total} of {TOTAL}"),
            }
        }
        assert_eq!(total, TOTAL, "lost output under backpressure");
    }

    #[tokio::test]
    async fn dropping_channel_reaps_child() {
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("30");
        let (channel, pid) = spawn_pty_channel_inner(cmd, SETTLE_WINDOW).expect("spawn");
        let pid = pid.expect("child pid on unix");
        assert!(pid_alive(pid), "child should be alive after spawn");

        drop(channel);

        // Reap is eventual: the detached writer thread kills the child after
        // it observes the dropped input. Poll for liveness with a timeout.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "child {pid} not reaped within 5s"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[tokio::test]
    async fn resize_accepted_while_live() {
        // Propagation to the remote TTY is covered by the ignored e2e test;
        // here we only assert resize is accepted on a live channel.
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("30");
        let channel = spawn_pty_channel(cmd).expect("spawn");
        let size = TerminalSize::new(40, 120).expect("nonzero");
        channel
            .resize(size)
            .await
            .expect("resize accepted while live");
        drop(channel);
    }

    #[tokio::test]
    async fn idle_caller_sees_close_on_child_exit() {
        // The optimistic-attach scenario: a viewer attaches, the remote
        // session is already gone, and the user types nothing. The channel
        // must still close (recv -> None) on its own. This path is driven by
        // the reader hitting EOF, independent of whether the caller ever
        // sends input — so this test never calls send_bytes.
        let mut cmd = CommandBuilder::new("printf");
        cmd.arg("remora-bye");
        let mut channel = spawn_pty_channel(cmd).expect("spawn");

        let mut acc = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(b))) => acc.extend_from_slice(&b),
                Ok(Some(_)) => {} // ChannelOutput is #[non_exhaustive]
                Ok(None) => break,
                Err(_) => panic!("idle caller never saw the channel close; got {acc:?}"),
            }
        }
        assert!(acc.windows(10).any(|w| w == b"remora-bye"), "got {acc:?}");
    }

    /// Drain until a StatusChange of `want` is seen; record whether any Bytes
    /// preceded it. Panics on close/timeout.
    async fn recv_status(
        channel: &mut SessionChannel,
        want: remora_protocol::SessionStatus,
    ) -> bool {
        let mut saw_bytes_first = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(_))) => saw_bytes_first = true,
                Ok(Some(ChannelOutput::StatusChange(s))) if s == want => return saw_bytes_first,
                Ok(Some(_)) => {}
                Ok(None) => panic!("closed before {want:?}"),
                Err(_) => panic!("timed out before {want:?}"),
            }
        }
    }

    #[tokio::test]
    async fn working_status_follows_the_bytes_that_triggered_it() {
        // `echo hi` produces output → Working must arrive AFTER the bytes.
        let mut cmd = CommandBuilder::new("echo");
        cmd.arg("hi");
        let mut channel = spawn_pty_channel(cmd).expect("spawn");
        let bytes_first = recv_status(&mut channel, remora_protocol::SessionStatus::Working).await;
        assert!(
            bytes_first,
            "StatusChange(Working) must be ordered after the Bytes"
        );
    }

    #[tokio::test]
    async fn idle_settles_after_silence_and_channel_closes() {
        // Short settle so the test is fast and not flaky.
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg("printf hi; sleep 2");
        let mut channel =
            spawn_pty_channel_with_settle(cmd, Duration::from_millis(150)).expect("spawn");
        recv_status(&mut channel, remora_protocol::SessionStatus::Working).await;
        recv_status(&mut channel, remora_protocol::SessionStatus::Idle).await;
        // After the child exits, the channel must still close (recv -> None),
        // proving the detector thread drops output_tx on reader EOF.
        loop {
            match tokio::time::timeout(Duration::from_secs(5), channel.recv()).await {
                Ok(Some(_)) => {}
                Ok(None) => break, // closed — teardown preserved
                Err(_) => panic!("channel never closed after child exit"),
            }
        }
    }

    #[tokio::test]
    async fn no_spurious_idle_under_backpressure() {
        // Invariant: while the detector thread is blocked on output_tx.blocking_send
        // (because the consumer is not reading), it cannot be in recv_timeout, so
        // no spurious StatusChange(Idle) fires during the stall.
        //
        // Technique mirrors slow_reader_loses_no_output: 12 MiB of NUL bytes
        // exceeds total in-flight capacity (CHANNEL_CAPACITY*READ_BUF +
        // DETECT_QUEUE*READ_BUF ≈ 10 MiB), guaranteeing the detector is blocked
        // on blocking_send for the entire stall window rather than idle in
        // recv_timeout. Once all data is drained and the child exits, the
        // detector gets RecvTimeoutError::Disconnected (not Timeout), so Idle
        // is also never emitted at teardown.
        const TOTAL: usize = 12 * 1024 * 1024;
        let settle = Duration::from_millis(100);
        let mut cmd = CommandBuilder::new("head");
        cmd.args(["-c", &TOTAL.to_string(), "/dev/zero"]);
        let mut channel = spawn_pty_channel_with_settle(cmd, settle).expect("spawn");

        // Stall 4× the settle window without draining; detector must be blocked
        // on blocking_send (not in recv_timeout), so no Idle fires.
        std::thread::sleep(Duration::from_millis(400));

        // Drain everything that accumulated while we stalled.
        let mut saw_working = false;
        let mut saw_idle = false;
        let mut bytes_total = 0usize;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await {
                Ok(Some(ChannelOutput::Bytes(b))) => {
                    assert!(b.iter().all(|&x| x == 0), "unexpected non-NUL byte");
                    bytes_total += b.len();
                }
                Ok(Some(ChannelOutput::StatusChange(remora_protocol::SessionStatus::Working))) => {
                    saw_working = true;
                }
                Ok(Some(ChannelOutput::StatusChange(remora_protocol::SessionStatus::Idle))) => {
                    saw_idle = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => panic!("timed out draining; got {bytes_total} of {TOTAL} bytes"),
            }
        }

        assert!(
            saw_working,
            "expected StatusChange(Working) during active output"
        );
        assert!(
            !saw_idle,
            "spurious StatusChange(Idle) fired while detector was blocked under backpressure"
        );
        assert_eq!(bytes_total, TOTAL, "lost output under backpressure");
    }

    /// Shared setup for the #224 sticky-awaiting tests: a `sh` child that
    /// fires the awaiting marker (wire twin of the module-level
    /// `AWAITING_MARKER` scanner input), then runs `rest`; 100ms settle.
    fn spawn_after_awaiting_marker(rest: &str) -> SessionChannel {
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(format!(
            "printf '\\033]7366;remora;1;state;YXdhaXRpbmdfaW5wdXQ=\\007'; {rest}"
        ));
        spawn_pty_channel_with_settle(cmd, Duration::from_millis(100)).expect("spawn")
    }

    /// Child fires an awaiting marker, then keeps printing cosmetic noise
    /// across several settle windows, then exits. After `Awaiting` no
    /// Working/Idle may follow (#224 sticky) — pre-fix, the noise chunks
    /// flipped Working and the settle decayed to Idle.
    #[tokio::test]
    async fn awaiting_survives_marker_less_output() {
        let mut channel = spawn_after_awaiting_marker(
            "sleep 0.3; printf 'clock repaint'; sleep 0.3; printf 'spinner'",
        );

        let mut saw_awaiting = false;
        let mut after_awaiting = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await {
                Ok(Some(ChannelOutput::StatusChange(s))) => {
                    if s == remora_protocol::SessionStatus::Awaiting {
                        saw_awaiting = true;
                    } else if saw_awaiting {
                        after_awaiting.push(s);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => panic!("timed out draining"),
            }
        }
        assert!(saw_awaiting, "never saw Awaiting");
        assert!(
            after_awaiting.is_empty(),
            "awaiting was stomped by cosmetic output: {after_awaiting:?}"
        );
    }

    /// Child fires the marker then `cat`s (stays alive). Typing through the
    /// channel is "the user is responding": Working must follow (#224).
    #[tokio::test]
    async fn user_input_exits_awaiting() {
        let mut channel = spawn_after_awaiting_marker("cat");

        recv_status(&mut channel, remora_protocol::SessionStatus::Awaiting).await;
        channel.send_bytes(b"y\n".to_vec()).await.expect("send");
        recv_status(&mut channel, remora_protocol::SessionStatus::Working).await;
        drop(channel); // teardown reaps cat
    }

    /// Resize is repaint-causing but is NOT "the user answered": neither the
    /// resize nor output arriving after it may exit `Awaiting` (#224).
    #[tokio::test]
    async fn resize_does_not_exit_awaiting() {
        let mut channel = spawn_after_awaiting_marker("sleep 0.5; printf 'post-resize repaint'");

        recv_status(&mut channel, remora_protocol::SessionStatus::Awaiting).await;
        let size = TerminalSize::new(40, 120).expect("nonzero");
        channel.resize(size).await.expect("resize while live");

        // Drain to close; the repaint printf lands AFTER the resize, so if
        // Resize wrongly set the user-input flag, that chunk would flip
        // Working. Nothing may follow Awaiting.
        let mut after_awaiting = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), channel.recv()).await {
                Ok(Some(ChannelOutput::StatusChange(s))) => after_awaiting.push(s),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => panic!("timed out draining"),
            }
        }
        assert!(
            after_awaiting.is_empty(),
            "resize path exited awaiting: {after_awaiting:?}"
        );
    }
}
