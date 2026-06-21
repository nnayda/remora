//! Transport-neutral core shared by every PTY-backed transport (ssh, kubectl).
//!
//! Holds the exec seam (`RemoteExec`), the two shared exec tails (`capture`
//! for blocking commands, `open_pty` for interactive channels), the logical
//! remote-command token builders, error classification, and the
//! spawn/respawn/list orchestration. A transport is just a connection adapter:
//! it composes its argv (connection prefix + shell wrap + interactive flag)
//! and delegates the tail here. This is the seam that proves the design isn't
//! ssh-shaped (roadmap stage 12).

use portable_pty::CommandBuilder;

use super::pty_process::spawn_pty_channel;
use crate::{SessionChannel, SourceError};

/// Result of a blocking remote command: success, captured stdout, and stderr.
pub(crate) struct RemoteOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The executor seam every spawn/attach/list step crosses. A transport's impl
/// composes the full argv (connection prefix + tokens) and delegates to
/// [`capture`] / [`open_pty`]. Interactive-ness is implied by which method is
/// called, so the impl owns the interactive flag and any shell wrapping.
pub(crate) trait RemoteExec: Send + Sync {
    /// Run a blocking, non-interactive remote command; capture status+output.
    fn run(&self, remote: &[String]) -> Result<RemoteOutput, SourceError>;
    /// Open an interactive PTY channel running the remote command.
    fn open_channel(&self, remote: &[String]) -> Result<SessionChannel, SourceError>;
}

/// Runs a fully-composed argv to completion and captures its output. The
/// shared tail of every transport's `RemoteExec::run`.
///
/// Precondition: `argv` is non-empty (program + args).
pub(crate) fn capture(argv: &[String]) -> Result<RemoteOutput, SourceError> {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| SourceError::Transport(format!("exec: {e}")))?;
    Ok(RemoteOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Opens a PTY channel running a fully-composed argv. The shared tail of every
/// transport's `RemoteExec::open_channel`.
pub(crate) fn open_pty(argv: &[String]) -> Result<SessionChannel, SourceError> {
    spawn_pty_channel(command_from_argv(argv))
}

/// Turns a pure argv into a `CommandBuilder` (program = argv[0]) with `TERM`
/// pinned so a remote tmux resolves terminfo for xterm.js regardless of the
/// launching shell's `$TERM` (#26). ssh forwards this to the remote PTY; the
/// kubectl transport additionally wraps `env TERM=…` in-container because
/// kubectl does not reliably forward the client TERM into the pod PTY.
///
/// Precondition: `argv` is non-empty.
pub(crate) fn command_from_argv(argv: &[String]) -> CommandBuilder {
    debug_assert!(!argv.is_empty(), "argv must contain at least the program");
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.env("TERM", "xterm-256color");
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn capture_reports_success_and_stdout() {
        let argv = vec!["sh".into(), "-c".into(), "printf remora-cap".into()];
        let out = capture(&argv).expect("capture runs");
        assert!(out.success);
        assert_eq!(out.stdout, "remora-cap");
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn capture_reports_nonzero_exit_and_stderr() {
        let argv = vec!["sh".into(), "-c".into(), "printf boom 1>&2; exit 3".into()];
        let out = capture(&argv).expect("capture runs");
        assert!(!out.success, "non-zero exit is success=false");
        assert_eq!(out.stderr, "boom");
    }

    #[test]
    fn command_from_argv_pins_term() {
        let cmd = command_from_argv(&["ssh".to_string(), "host".to_string()]);
        assert_eq!(cmd.get_env("TERM"), Some("xterm-256color".as_ref()));
    }
}
