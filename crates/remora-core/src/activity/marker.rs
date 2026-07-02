//! Recognize the Remora OSC-7366 activity marker in the PTY stream (ADR-0010).
//! Generic, never agent-specific: we match the Remora `7366` code + `remora`
//! token, nothing about any particular agent. `vte` does the cross-read-boundary
//! OSC reassembly and bounds its own buffers (the untrusted-input DoS cap).
//!
//! Grammar (inner, after tmux passthrough unwraps it):
//!   state:  ESC ] 7366 ; remora ; <ver> ; state ; <state-b64> [ ; <msg-b64> ] BEL
//!   ping:   ESC ] 7366 ; remora ; <ver> ; ping BEL   (payload-free liveness marker, #198/ADR-0019)
//! vte splits the OSC string on ';' and hands us the segments (the `7366` code
//! is the first segment).

use base64::Engine as _;
use remora_protocol::SessionStatus;

use super::{sanitize, SanitizedText};

const CODE: &[u8] = b"7366";
const TOKEN: &[u8] = b"remora";
const VERSION: &[u8] = b"1";
const TYPE_STATE: &[u8] = b"state";
const TYPE_PING: &[u8] = b"ping";
const PAYLOAD_CAP: usize = 80;

/// A recognized marker. `State` carries the asserted status + optional preview
/// (ADR-0010/0013). `Liveness` is the payload-free `ping` (#198): it proves the
/// agent's hook pipeline is wired without asserting any activity state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerHit {
    State {
        status: SessionStatus,
        preview: Option<SanitizedText>,
    },
    Liveness,
}

/// Incremental scanner. Feed PTY chunks; vte buffers partial markers between
/// feeds. `feed` returns the markers completed during that call.
pub struct MarkerScanner {
    parser: vte::Parser,
    sink: MarkerSink,
}

impl MarkerScanner {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            sink: MarkerSink { hits: Vec::new() },
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<MarkerHit> {
        // vte 0.14: advance consumes the whole slice in one call.
        self.parser.advance(&mut self.sink, chunk);
        // std::mem::take leaves self.sink.hits empty for the next call.
        std::mem::take(&mut self.sink.hits)
    }
}

impl Default for MarkerScanner {
    fn default() -> Self {
        Self::new()
    }
}

// vte::Parser<1024> does not implement Debug, so we provide a manual impl that
// omits the opaque parser state.
impl std::fmt::Debug for MarkerScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkerScanner")
            .field("hits_pending", &self.sink.hits.len())
            .finish_non_exhaustive()
    }
}

struct MarkerSink {
    hits: Vec<MarkerHit>,
}

impl vte::Perform for MarkerSink {
    // We only care about OSC. `vte::Perform` provides default no-op bodies for
    // every method, so `osc_dispatch` is the only override needed.
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let Some(hit) = parse_marker(params) {
            self.hits.push(hit);
        }
    }
}

fn parse_marker(params: &[&[u8]]) -> Option<MarkerHit> {
    // Common prefix: [7366, remora, ver, type, ...]
    if params.len() < 4 || params[0] != CODE || params[1] != TOKEN || params[2] != VERSION {
        return None;
    }
    // Liveness ping: exactly [7366, remora, 1, ping] — no payload. A ping with
    // any trailing segment is a forgery attempt and is dropped.
    if params[3] == TYPE_PING {
        return (params.len() == 4).then_some(MarkerHit::Liveness);
    }
    // State marker: [7366, remora, 1, state, state-b64, (msg-b64)?].
    if params[3] != TYPE_STATE || params.len() < 5 || params.len() > 6 {
        return None;
    }
    // The state token is matched against a fixed allowlist, so it is decoded
    // RAW (no sanitize): scrubbing control chars first would let a forged
    // `working\x00` normalize to a valid `working`. Only the free-text preview
    // is sanitized (control/format-stripped, capped).
    let status = decode_utf8(params[4])
        .as_deref()
        .and_then(status_from_token)?;
    let preview = params.get(5).and_then(|seg| {
        let text = decode_utf8(seg)?;
        let s = sanitize(&text, PAYLOAD_CAP);
        (!s.is_empty()).then_some(s)
    });
    Some(MarkerHit::State { status, preview })
}

fn decode_utf8(seg: &[u8]) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(seg).ok()?;
    String::from_utf8(bytes).ok()
}

fn status_from_token(token: &str) -> Option<SessionStatus> {
    match token {
        "working" => Some(SessionStatus::Working),
        "idle" => Some(SessionStatus::Idle),
        "awaiting_input" => Some(SessionStatus::Awaiting),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_protocol::SessionStatus;

    // base64 of "working" = "d29ya2luZw==", "idle" = "aWRsZQ==",
    // "awaiting_input" = "YXdhaXRpbmdfaW5wdXQ=".
    fn marker(state_b64: &str) -> Vec<u8> {
        format!("\x1b]7366;remora;1;state;{state_b64}\x07").into_bytes()
    }

    const AWAITING_INPUT_B64: &str = "YXdhaXRpbmdfaW5wdXQ="; // base64("awaiting_input")

    fn make_wrapped(state_b64: &str, msg_b64: &str) -> String {
        // tmux passthrough envelope (inner ESC doubled), ADR-0010 on-wire form.
        format!("\x1bPtmux;\x1b\x1b]7366;remora;1;state;{state_b64};{msg_b64}\x07\x1b\\")
    }

    fn strip_tmux_passthrough(wrapped: &str) -> String {
        wrapped
            .strip_prefix("\x1bPtmux;")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("tmux passthrough envelope")
            .replace("\x1b\x1b", "\x1b")
    }

    #[test]
    fn parses_a_whole_state_marker() {
        let mut s = MarkerScanner::new();
        let hits = s.feed(&marker("d29ya2luZw==")); // "working"
        assert_eq!(hits.len(), 1);
        let MarkerHit::State { status, preview } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Working);
        assert!(preview.is_none());
    }

    #[test]
    fn awaiting_token_maps_to_awaiting() {
        let mut s = MarkerScanner::new();
        let hits = s.feed(&marker("YXdhaXRpbmdfaW5wdXQ=")); // "awaiting_input"
        let MarkerHit::State { status, .. } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Awaiting);
    }

    #[test]
    fn reassembles_marker_split_across_feeds() {
        let bytes = marker("aWRsZQ=="); // "idle"
        let (a, b) = bytes.split_at(10);
        let mut s = MarkerScanner::new();
        assert!(s.feed(a).is_empty()); // incomplete: vte buffers internally
        let hits = s.feed(b);
        assert_eq!(hits.len(), 1);
        let MarkerHit::State { status, .. } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Idle);
    }

    fn ping_marker() -> Vec<u8> {
        b"\x1b]7366;remora;1;ping\x07".to_vec()
    }

    fn make_wrapped_ping() -> String {
        // tmux passthrough envelope (inner ESC doubled), ADR-0010 on-wire form.
        "\x1bPtmux;\x1b\x1b]7366;remora;1;ping\x07\x1b\\".to_string()
    }

    #[test]
    fn parses_a_liveness_ping() {
        let mut s = MarkerScanner::new();
        let hits = s.feed(&ping_marker());
        assert_eq!(hits, vec![MarkerHit::Liveness]);
    }

    #[test]
    fn reassembles_ping_split_across_feeds() {
        // The ping shares the vte reassembly path with state markers; guard it
        // with its own regression test so a split read boundary still yields one
        // Liveness hit (parity with reassembles_marker_split_across_feeds).
        let bytes = ping_marker();
        let (a, b) = bytes.split_at(10);
        let mut s = MarkerScanner::new();
        assert!(s.feed(a).is_empty()); // incomplete: vte buffers internally
        assert_eq!(s.feed(b), vec![MarkerHit::Liveness]);
    }

    #[test]
    fn ping_with_extra_segment_is_rejected() {
        // A forged ping carrying a payload is not the 4-segment liveness form.
        let mut s = MarkerScanner::new();
        assert!(s.feed(b"\x1b]7366;remora;1;ping;Ym9ndXM=\x07").is_empty());
    }

    #[test]
    fn remora_ping_recipe_round_trip() {
        // The exact WRAPPED bytes remora-ping.sh emits (keep in sync with the script).
        let wrapped = make_wrapped_ping();
        let inner = strip_tmux_passthrough(&wrapped);
        let mut s = MarkerScanner::new();
        let hits = s.feed(inner.as_bytes());
        assert_eq!(
            hits,
            vec![MarkerHit::Liveness],
            "exactly one liveness marker"
        );
    }

    #[test]
    fn ignores_unknown_token_version_and_type() {
        let mut s = MarkerScanner::new();
        assert!(s.feed(b"\x1b]7366;remora;1;state;Ym9ndXM=\x07").is_empty()); // "bogus"
        assert!(s.feed(b"\x1b]7366;remora;2;state;aWRsZQ==\x07").is_empty()); // ver 2
        assert!(s.feed(b"\x1b]7366;remora;1;notify;aWRsZQ==\x07").is_empty()); // type notify
        assert!(s
            .feed(b"\x1b]7366;NOTREMORA;1;state;aWRsZQ==\x07")
            .is_empty());
    }

    #[test]
    fn ignores_non_7366_osc_and_plain_text() {
        let mut s = MarkerScanner::new();
        assert!(s.feed(b"\x1b]0;a window title\x07hello world\n").is_empty());
    }

    #[test]
    fn extracts_optional_preview_message_segment() {
        // 6th ;-segment is an optional base64 preview message.
        // "working" + "Run tests?" (base64 "UnVuIHRlc3RzPw==").
        let mut s = MarkerScanner::new();
        let hits = s.feed(b"\x1b]7366;remora;1;state;d29ya2luZw==;UnVuIHRlc3RzPw==\x07");
        let MarkerHit::State { status, preview } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Working);
        assert_eq!(preview.as_ref().expect("preview").as_str(), "Run tests?");
    }

    #[test]
    fn forged_oversized_payload_is_capped_or_rejected() {
        // A huge non-token state payload does not match → no hit, no unbounded growth.
        let big = base64::engine::general_purpose::STANDARD.encode("x".repeat(10_000));
        let mut s = MarkerScanner::new();
        let seq = format!("\x1b]7366;remora;1;state;{big}\x07");
        assert!(s.feed(seq.as_bytes()).is_empty());
    }

    #[test]
    fn state_token_with_embedded_control_does_not_match() {
        // The state token is matched RAW against the allowlist, so "working\0"
        // is not "working" → no hit. (Sanitizing the token first would strip the
        // NUL and falsely accept the forged value.)
        let b64 = base64::engine::general_purpose::STANDARD.encode("working\0");
        let mut s = MarkerScanner::new();
        assert!(s.feed(&marker(&b64)).is_empty());
    }

    #[test]
    fn remora_notify_recipe_round_trip() {
        use base64::Engine as _;
        // The literal message a Notification hook would carry.
        let msg = "Approve running tests?";
        let enc = base64::engine::general_purpose::STANDARD.encode(msg);

        // The exact WRAPPED bytes remora-notify.sh emits (keep in sync with the script).
        let wrapped = make_wrapped(AWAITING_INPUT_B64, &enc);

        // Reverse what tmux does on the way out: drop the envelope, un-double ESC.
        let inner = strip_tmux_passthrough(&wrapped);

        // Core only ever sees the inner form; assert the scanner accepts it.
        let mut s = MarkerScanner::new();
        let hits = s.feed(inner.as_bytes());
        assert_eq!(hits.len(), 1, "exactly one marker, got {hits:?}");
        let MarkerHit::State { status, preview } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Awaiting);
        assert_eq!(
            preview.as_ref().expect("preview").as_str(),
            "Approve running tests?"
        );
    }

    #[test]
    fn wrapped_envelope_shape_is_tmux_passthrough() {
        // Guards against regressing the recipe back to a bare OSC (which tmux eats).
        // Built via make_wrapped so the shape test is tied to the real construction helper.
        let wrapped = make_wrapped(AWAITING_INPUT_B64, "QQ==");
        assert!(
            wrapped.starts_with("\x1bPtmux;"),
            "missing passthrough prefix"
        );
        assert!(wrapped.ends_with("\x1b\\"), "missing ST terminator");
        assert!(
            wrapped.contains("\x1b\x1b]7366;"),
            "inner ESC must be doubled"
        );
    }

    /// Regression guard: executes the remora-ping.sh script and asserts its
    /// stdout bytes match the wire contract. A no-op in minimal environments
    /// (no bash), but is the live guard where tooling is present.
    ///
    /// This test ties the SCRIPT to the wire contract — editing the script
    /// back to a bare OSC or stdout would fail here. The manual hermes
    /// dogfood (/dev/tty + real tmux) remains the true e2e gate.
    #[test]
    fn ping_script_output_matches_wire_contract() {
        use std::process::{Command, Stdio};

        let script_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contrib/agent-hooks/claude-code/remora-ping.sh"
        );
        if !std::path::Path::new(script_path).exists() {
            eprintln!("skip: script not found at {script_path}");
            return;
        }
        let has_bash = Command::new("bash")
            .arg("-c")
            .arg("true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_bash {
            eprintln!("skip: bash not found");
            return;
        }

        let output = Command::new("bash")
            .arg(script_path)
            .env("REMORA_MARKER_OUT", "/dev/stdout")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("bash should spawn")
            .wait_with_output()
            .expect("script should exit");
        assert!(
            output.status.success(),
            "script exited non-zero: {:?}",
            output.status
        );

        let stdout = String::from_utf8(output.stdout).expect("utf-8");
        let stdout = stdout.trim_end_matches('\n');
        assert_eq!(
            stdout,
            make_wrapped_ping(),
            "ping script output does not match wire contract"
        );

        // Tie script output to the scanner: it must parse to Liveness.
        let inner = strip_tmux_passthrough(&make_wrapped_ping());
        let mut s = MarkerScanner::new();
        assert_eq!(s.feed(inner.as_bytes()), vec![MarkerHit::Liveness]);
    }

    /// Regression guard: executes the actual shell script and asserts its
    /// stdout bytes match the wire contract. A no-op in minimal environments
    /// (no bash/jq), but is the live guard where tooling is present.
    ///
    /// This test ties the SCRIPT to the wire contract — editing the script
    /// back to a bare OSC or stdout would fail here. The manual hermes
    /// dogfood (/dev/tty + real tmux) remains the true e2e gate.
    #[test]
    fn script_output_matches_wire_contract() {
        use base64::Engine as _;
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let script_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contrib/agent-hooks/claude-code/remora-notify.sh"
        );

        // Skip if the script file doesn't exist (e.g. partial checkout).
        if !std::path::Path::new(script_path).exists() {
            eprintln!("skip: script not found at {script_path}");
            return;
        }

        // Skip if bash or jq is unavailable (minimal CI environments).
        let has_jq = Command::new("bash")
            .arg("-c")
            .arg("command -v jq")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_jq {
            eprintln!("skip: jq not found — skipping script round-trip test");
            return;
        }

        // Run the script with REMORA_MARKER_OUT=/dev/stdout so the marker
        // bytes go to captured stdout rather than /dev/tty.
        let mut child = Command::new("bash")
            .arg(script_path)
            .env("REMORA_MARKER_OUT", "/dev/stdout")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("bash should spawn");

        let msg = "Approve running tests?";
        // Use serde_json to build the JSON so quotes/backslashes in msg can't malform it.
        let stdin_json = serde_json::json!({ "message": msg }).to_string();
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(format!("{stdin_json}\n").as_bytes())
            .expect("write stdin");

        let output = child.wait_with_output().expect("script should exit");
        assert!(
            output.status.success(),
            "script exited non-zero: {:?}",
            output.status
        );

        let enc = base64::engine::general_purpose::STANDARD.encode(msg);
        let expected = make_wrapped(AWAITING_INPUT_B64, &enc);

        let stdout = String::from_utf8(output.stdout).expect("script output is utf-8");
        // The script's printf emits no trailing newline; trim at most one in
        // case the environment's printf adds one (some bash/base64 combos do).
        let stdout = stdout.trim_end_matches('\n');

        assert_eq!(
            stdout, expected,
            "script output does not match wire contract"
        );

        // Feed through strip_tmux_passthrough and assert the scanner produces
        // the expected hit, tying the script output to the full scanner pipeline.
        let inner = strip_tmux_passthrough(&expected);

        let mut s = MarkerScanner::new();
        let hits = s.feed(inner.as_bytes());
        assert_eq!(hits.len(), 1, "exactly one marker from script output");
        let MarkerHit::State { status, preview } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Awaiting);
        assert_eq!(
            preview.as_ref().expect("preview").as_str(),
            "Approve running tests?"
        );
    }

    /// Same regression guard, for the PreToolUse(AskUserQuestion) input shape:
    /// the hook input carries the question at `.tool_input.questions[0].question`
    /// instead of `.message`. Claude Code's AskUserQuestion menu fires no
    /// immediate Notification (only a delayed generic permission_prompt nag),
    /// so PreToolUse is the recipe's only prompt-time signal for it.
    #[test]
    fn script_output_matches_wire_contract_for_askuserquestion() {
        use base64::Engine as _;
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let script_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contrib/agent-hooks/claude-code/remora-notify.sh"
        );
        if !std::path::Path::new(script_path).exists() {
            eprintln!("skip: script not found at {script_path}");
            return;
        }
        let has_jq = Command::new("bash")
            .arg("-c")
            .arg("command -v jq")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !has_jq {
            eprintln!("skip: jq not found — skipping script round-trip test");
            return;
        }

        let mut child = Command::new("bash")
            .arg(script_path)
            .env("REMORA_MARKER_OUT", "/dev/stdout")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("bash should spawn");

        let question = "Which option would you prefer?";
        // The captured shape of a real PreToolUse hook input for AskUserQuestion
        // (verified against Claude Code 2.1.198), minus irrelevant fields.
        let stdin_json = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion",
            "tool_input": {
                "questions": [{
                    "question": question,
                    "header": "Choice",
                    "options": [
                        { "label": "Option A", "description": "first" },
                        { "label": "Option B", "description": "second" }
                    ],
                    "multiSelect": false
                }]
            }
        })
        .to_string();
        child
            .stdin
            .take()
            .expect("stdin piped")
            .write_all(format!("{stdin_json}\n").as_bytes())
            .expect("write stdin");

        let output = child.wait_with_output().expect("script should exit");
        assert!(
            output.status.success(),
            "script exited non-zero: {:?}",
            output.status
        );

        let enc = base64::engine::general_purpose::STANDARD.encode(question);
        let expected = make_wrapped(AWAITING_INPUT_B64, &enc);

        let stdout = String::from_utf8(output.stdout).expect("script output is utf-8");
        let stdout = stdout.trim_end_matches('\n');
        assert_eq!(
            stdout, expected,
            "script output does not match wire contract for AskUserQuestion input"
        );

        let inner = strip_tmux_passthrough(&expected);
        let mut s = MarkerScanner::new();
        let hits = s.feed(inner.as_bytes());
        assert_eq!(hits.len(), 1, "exactly one marker from script output");
        let MarkerHit::State { status, preview } = &hits[0] else {
            panic!("expected State, got {:?}", hits[0]);
        };
        assert_eq!(*status, SessionStatus::Awaiting);
        assert_eq!(preview.as_ref().expect("preview").as_str(), question);
    }
}
