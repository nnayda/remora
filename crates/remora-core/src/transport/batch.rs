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
    use super::*;

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
