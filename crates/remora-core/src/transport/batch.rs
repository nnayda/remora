//! Transport-neutral batching primitive (#182 / ADR-0017). Builds one POSIX
//! `sh` script from a list of typed steps, each step's combined (`2>&1`) output
//! framed by ASCII control-byte record delimiters, and parses the script's
//! stdout back into per-step results. Runs through the existing
//! `RemoteExec::run` (as `["sh","-c", <quoted script>]`), so no transport knows
//! it exists. Reused by spawn/respawn (this PR) and list() (follow-up).

// Task 2 wires in the callers; suppress dead_code until then.
#![allow(dead_code)]

use crate::SourceError;

/// Unit separator between a record's fields; record separator between records.
/// Control bytes, never present in a stored/sanitized value (`clean_metadata`
/// bans control chars), so a malicious tmux `#{E:}` value cannot forge a record
/// boundary — the #182/#108 framing-integrity invariant, no entropy needed.
pub(crate) const US: char = '\u{1f}';
pub(crate) const RS: char = '\u{1e}';

/// Stable identifier for a batched step. Emitted into each record and matched
/// back here. (PR B — list() — extends this enum with its section ids.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepId {
    Fetch,
    WorktreeAdd,
    NewSession,
    Passthrough,
    SetEnv,
}

impl StepId {
    /// The short ASCII token written into the record (no whitespace / control
    /// chars, so it never needs quoting and never collides with a delimiter).
    pub(crate) fn token(self) -> &'static str {
        match self {
            StepId::Fetch => "fetch",
            StepId::WorktreeAdd => "worktree_add",
            StepId::NewSession => "new_session",
            StepId::Passthrough => "passthrough",
            StepId::SetEnv => "set_env",
        }
    }

    fn from_token(tok: &str) -> Option<StepId> {
        match tok {
            "fetch" => Some(StepId::Fetch),
            "worktree_add" => Some(StepId::WorktreeAdd),
            "new_session" => Some(StepId::NewSession),
            "passthrough" => Some(StepId::Passthrough),
            "set_env" => Some(StepId::SetEnv),
            _ => None,
        }
    }
}

/// One parsed step record: which step, its combined output, and its exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepResult {
    pub id: StepId,
    pub output: String,
    pub rc: i32,
}

/// One step of a batch: its id, the assembled shell command, and whether a
/// non-zero exit must halt a `StopOnError` chain. `cmd` is built by the caller
/// from already-quoted tokens joined with spaces; `build` quotes the *whole*
/// script exactly once, so the caller must NOT re-quote a step's `cmd`.
pub(crate) struct Step {
    pub id: StepId,
    pub cmd: String,
    pub fatal: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum BatchMode {
    /// Spawn/respawn: a fatal step's non-zero exit halts the chain.
    StopOnError,
    /// list(): every step runs regardless of any individual exit.
    RunAll,
}

/// Frames one step into the script. Emits `<id><US>` first (using octal `\037`
/// for the US byte so the script is pure printable ASCII), runs the step in a
/// subshell with `2>&1`, captures `$?` IMMEDIATELY (nothing between the
/// subshell and the capture), then emits `<US><rc><RS>`. In `StopOnError`, a
/// fatal step appends a halt guard. No `set -e` is ever emitted. `$__rc` is
/// double-quoted in the tail printf for consistency (behavior is identical
/// since it is always a decimal 0–255 integer). The whole assembled script is
/// shell-quoted once by `build()`, so the per-step printf args here are
/// written as bare single-quoted literals — do NOT quote the printf command
/// name itself.
fn frame_step(step: &Step, mode: BatchMode) -> String {
    let id = step.id.token();
    let mut s = String::new();
    // Record head: id + US (\037 = 0x1f).
    s.push_str(&format!("printf '{id}\\037';"));
    // The step body: stdout+stderr combined via 2>&1. A SUBSHELL (not brace
    // group) is used so that `exit` inside `cmd` terminates the subshell rather
    // than the whole batch script — the tail printf still runs and the record
    // is fully emitted before StopOnError's halt guard fires.
    s.push_str(&format!("({cmd}) 2>&1;", cmd = step.cmd));
    // Capture $? IMMEDIATELY — nothing between the group and the capture.
    s.push_str("__rc=$?;");
    // Record tail: US (\037) + rc + RS (\036 = 0x1e).
    s.push_str("printf '\\037%s\\036' \"$__rc\"");
    if matches!(mode, BatchMode::StopOnError) && step.fatal {
        // Halt: exit(0) after recording so the record stream stays parseable.
        s.push_str(";[ $__rc -ne 0 ]&&exit 0");
    }
    s.push('\n');
    s
}

/// POSIX single-quotes a shell script for use as the `-c` argument to an
/// enclosing shell. Unlike the per-token `shell_quote` (which delegates to
/// `shlex::try_quote` and may emit a mixed-quoting result — a concatenation
/// of bare, single-, and double-quoted chunks depending on character class —
/// making the whole-script token hard to audit), this always wraps the ENTIRE
/// script in a single outer `'…'` pair, which is trivially auditable and
/// provably correct for passing a whole script verbatim to the inner `sh`.
/// Embedded single quotes are escaped via the `'\''` idiom (close-quote,
/// backslash-escaped literal quote, reopen-quote). Starts with `'` by
/// construction, satisfying the argv-shape invariant. `$__rc` inside the
/// script is NOT expanded by the outer shell (single-quoting prevents it),
/// so the inner `sh -c` receives the script verbatim and expands `$__rc` in
/// context — which is exactly correct.
fn quote_script(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Builds the batched script and returns `["sh", "-c", <quoted-script>]` for
/// `RemoteExec::run`. The script token (`argv[2]`) is shell-quoted exactly
/// once with `quote_script` so it survives a surrounding shell parse — both
/// transports space-join these tokens into a command the enclosing shell
/// re-parses (kubectl: `sh -c "export …; sh -c <token>"`; ssh: the remote
/// login shell), so the token MUST be a single properly-quoted arg. A bare
/// (unquoted) script would be word-split by that enclosing shell and `sh -c`
/// would only see the first word.
pub(crate) fn build(steps: &[Step], mode: BatchMode) -> Vec<String> {
    let mut script = String::new();
    for step in steps {
        script.push_str(&frame_step(step, mode));
    }
    vec!["sh".into(), "-c".into(), quote_script(&script)]
}

/// Parses a batched script's stdout into per-step results. Each record is
/// `<id><US><output><US><rc><RS>`; `output` may itself contain `US` (split on
/// the first and last `US`). A record that ran but is malformed/truncated
/// (channel death mid-stream, an unknown id, a non-numeric rc) is a fail-safe
/// `Transport` error rather than a silent skip. An empty stdout parses to no
/// records (the caller treats "no records" as a transport failure when the exec
/// also reported failure).
pub(crate) fn parse_records(stdout: &str) -> Result<Vec<StepResult>, SourceError> {
    let mut out = Vec::new();
    // Records are RS-terminated; the trailing empty segment after the final RS
    // (and any stray whitespace the transport appends) is skipped.
    for segment in stdout.split(RS) {
        if segment.is_empty() {
            continue;
        }
        let (id_tok, rest) = segment.split_once(US).ok_or_else(|| {
            SourceError::Transport(format!(
                "batch: malformed record (no field separator): {segment:?}"
            ))
        })?;
        let (output, rc_str) = rest.rsplit_once(US).ok_or_else(|| {
            SourceError::Transport(format!(
                "batch: malformed record (no rc separator): {segment:?}"
            ))
        })?;
        let id = StepId::from_token(id_tok)
            .ok_or_else(|| SourceError::Transport(format!("batch: unknown step id {id_tok:?}")))?;
        let rc = rc_str.trim().parse::<i32>().map_err(|_| {
            SourceError::Transport(format!(
                "batch: non-numeric rc {rc_str:?} for step {id_tok}"
            ))
        })?;
        out.push(StepResult {
            id,
            output: output.to_string(),
            rc,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::remote::capture;
    use super::*;

    /// Runs `build()`'s output the way a transport does: the tokens
    /// `["sh", "-c", <quoted-script>]` are space-joined and handed to ONE
    /// enclosing shell (kubectl's outer `sh -c`, or ssh's remote login shell),
    /// which unquotes the script token and runs the inner `sh -c <script>`.
    /// `capture()` alone would execve `sh -c <quoted>` directly with no
    /// enclosing shell, which is NOT the production path.
    fn run_batch(argv: &[String]) -> super::super::remote::RemoteOutput {
        capture(&["sh".into(), "-c".into(), argv.join(" ")]).expect("sh runs")
    }

    #[test]
    fn build_returns_sh_dash_c_with_one_quoted_arg() {
        let steps = [Step {
            id: StepId::Fetch,
            cmd: "true".into(),
            fatal: false,
        }];
        let argv = build(&steps, BatchMode::StopOnError);
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        // The whole script is a single shell-quoted arg.
        assert!(argv[2].starts_with('\'') || !argv[2].contains(' '));
    }

    #[test]
    fn run_through_real_sh_frames_each_step() {
        // Two non-fatal steps with known output run through a real sh; the
        // parsed records must carry their output and rc.
        let steps = [
            Step {
                id: StepId::Fetch,
                cmd: "echo hello".into(),
                fatal: false,
            },
            Step {
                id: StepId::SetEnv,
                cmd: "echo world; false".into(),
                fatal: false,
            },
        ];
        let out = run_batch(&build(&steps, BatchMode::RunAll));
        let recs = parse_records(&out.stdout).expect("parse");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].id, StepId::Fetch);
        assert_eq!(recs[0].output.trim(), "hello");
        assert_eq!(recs[0].rc, 0);
        assert_eq!(recs[1].id, StepId::SetEnv);
        assert_eq!(recs[1].output.trim(), "world");
        assert_eq!(recs[1].rc, 1); // `false` exits 1; RunAll did not halt
    }

    #[test]
    fn stop_on_error_halts_after_a_fatal_failure() {
        let steps = [
            Step {
                id: StepId::WorktreeAdd,
                cmd: "echo boom >&2; exit 3".into(),
                fatal: true,
            },
            Step {
                id: StepId::NewSession,
                cmd: "echo should-not-run".into(),
                fatal: false,
            },
        ];
        let out = run_batch(&build(&steps, BatchMode::StopOnError));
        let recs = parse_records(&out.stdout).expect("parse");
        // Only the fatal step's record is present; the chain exited before
        // new_session. The record stream is still valid and parseable.
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, StepId::WorktreeAdd);
        assert_eq!(recs[0].rc, 3);
        assert!(
            recs[0].output.contains("boom"),
            "2>&1 captured stderr: {:?}",
            recs[0].output
        );
    }

    #[test]
    fn non_fatal_failure_does_not_halt_stop_on_error_chain() {
        let steps = [
            Step {
                id: StepId::Fetch,
                cmd: "exit 7".into(),
                fatal: false,
            },
            Step {
                id: StepId::NewSession,
                cmd: "echo ok".into(),
                fatal: true,
            },
        ];
        let out = run_batch(&build(&steps, BatchMode::StopOnError));
        let recs = parse_records(&out.stdout).expect("parse");
        assert_eq!(recs.len(), 2, "non-fatal fetch failure must not halt");
        assert_eq!(recs[0].rc, 7);
        assert_eq!(recs[1].id, StepId::NewSession);
        assert_eq!(recs[1].rc, 0);
    }

    #[test]
    fn no_trailing_newline_value_is_exact() {
        let steps = [Step {
            id: StepId::Fetch,
            cmd: "printf %s /home/dev".into(),
            fatal: false,
        }];
        let out = run_batch(&build(&steps, BatchMode::RunAll));
        let recs = parse_records(&out.stdout).expect("parse");
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].output, "/home/dev",
            "no-newline value must be exact"
        );
        assert_eq!(recs[0].rc, 0);
    }

    #[test]
    fn tmux_separator_and_spaced_arg_survive_single_quote_assembly() {
        // Builder discipline (outside-voice #7): a step cmd carrying tmux's
        // shell-quoted ';' separators and a spaced, single-quoted argument must
        // run through a real sh unbroken when build() quotes the whole script
        // once. We emulate tmux with `printf` and assert the bytes round-trip.
        // cmd echoes: literal `;` (from ';'), then a spaced arg, proving neither
        // the inner per-token quoting nor build()'s outer quote mangled them.
        let cmd = "printf '%s|%s' ';' 'Be concise'".to_string();
        let steps = [Step {
            id: StepId::NewSession,
            cmd,
            fatal: true,
        }];
        let out = run_batch(&build(&steps, BatchMode::StopOnError));
        let recs = parse_records(&out.stdout).expect("parse");
        assert_eq!(recs[0].output, ";|Be concise");
        assert_eq!(recs[0].rc, 0);
    }

    fn rec(id: &str, output: &str, rc: i32) -> String {
        format!("{id}{US}{output}{US}{rc}{RS}")
    }

    #[test]
    fn parses_well_formed_records_in_order() {
        let stream = format!(
            "{}{}",
            rec("worktree_add", "Preparing worktree", 0),
            rec("new_session", "", 0),
        );
        let got = parse_records(&stream).expect("parse");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, StepId::WorktreeAdd);
        assert_eq!(got[0].output, "Preparing worktree");
        assert_eq!(got[0].rc, 0);
        assert_eq!(got[1].id, StepId::NewSession);
        assert_eq!(got[1].rc, 0);
    }

    #[test]
    fn stopped_early_chain_yields_only_emitted_records() {
        // A fatal worktree_add failed and the script exited: only its record is
        // present, new_session never ran.
        let stream = rec("worktree_add", "fatal: already exists", 128);
        let got = parse_records(&stream).expect("parse");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, StepId::WorktreeAdd);
        assert_eq!(got[0].rc, 128);
        assert!(got[0].output.contains("already exists"));
    }

    #[test]
    fn output_containing_unit_separator_is_preserved() {
        // Pathological: a step's output contains a US byte. Splitting on the
        // first+last US keeps the middle intact.
        let stream = rec("fetch", &format!("a{US}b"), 0);
        let got = parse_records(&stream).expect("parse");
        assert_eq!(got[0].output, format!("a{US}b"));
        assert_eq!(got[0].rc, 0);
    }

    #[test]
    fn empty_stdout_parses_to_no_records() {
        assert!(parse_records("").expect("parse").is_empty());
    }

    #[test]
    fn unknown_id_is_transport_error() {
        let stream = rec("bogus", "x", 0);
        assert!(matches!(
            parse_records(&stream),
            Err(SourceError::Transport(_))
        ));
    }

    #[test]
    fn non_numeric_rc_is_transport_error() {
        let stream = format!("fetch{US}out{US}notanint{RS}");
        assert!(matches!(
            parse_records(&stream),
            Err(SourceError::Transport(_))
        ));
    }

    #[test]
    fn truncated_record_missing_separators_is_transport_error() {
        // Channel died mid-record: an id with no US/rc fields.
        let stream = format!("worktree_add partial output no seps{RS}");
        assert!(matches!(
            parse_records(&stream),
            Err(SourceError::Transport(_))
        ));
    }
}
