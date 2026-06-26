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

use std::io::{Read, Write};
use std::sync::mpsc::{self as std_mpsc, sync_channel, RecvTimeoutError};
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
                    // Raw stream first, byte-exact and never stripped.
                    if output_tx
                        .blocking_send(ChannelOutput::Bytes(bytes.clone()))
                        .is_err()
                    {
                        break; // caller dropped the channel
                    }
                    // Then the ordered status/preview events for this chunk.
                    let mut closed = false;
                    for ev in detector.on_bytes(&bytes) {
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
                    let mut closed = false;
                    for ev in detector.on_tick() {
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

impl From<DetectorEvent> for ChannelOutput {
    fn from(ev: DetectorEvent) -> Self {
        match ev {
            DetectorEvent::Status(s) => ChannelOutput::StatusChange(s),
            DetectorEvent::Preview(t) => ChannelOutput::PreviewUpdate(t.into_string()),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use remora_protocol::TerminalSize;
    use std::process::Command as StdCommand;
    use std::time::{Duration, Instant};

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

    use std::time::Duration as Dur;

    /// Drain until a StatusChange of `want` is seen; record whether any Bytes
    /// preceded it. Panics on close/timeout.
    async fn recv_status(
        channel: &mut SessionChannel,
        want: remora_protocol::SessionStatus,
    ) -> bool {
        let mut saw_bytes_first = false;
        loop {
            match tokio::time::timeout(Dur::from_secs(5), channel.recv()).await {
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
        let mut channel = spawn_pty_channel_with_settle(cmd, Dur::from_millis(150)).expect("spawn");
        recv_status(&mut channel, remora_protocol::SessionStatus::Working).await;
        recv_status(&mut channel, remora_protocol::SessionStatus::Idle).await;
        // After the child exits, the channel must still close (recv -> None),
        // proving the detector thread drops output_tx on reader EOF.
        loop {
            match tokio::time::timeout(Dur::from_secs(5), channel.recv()).await {
                Ok(Some(_)) => {}
                Ok(None) => break, // closed — teardown preserved
                Err(_) => panic!("channel never closed after child exit"),
            }
        }
    }
}
