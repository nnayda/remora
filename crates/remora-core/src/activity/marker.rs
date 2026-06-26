//! Recognize the Remora OSC-7366 activity marker in the PTY stream (ADR-0010).
//! Generic, never agent-specific: we match the Remora `7366` code + `remora`
//! token, nothing about any particular agent. `vte` does the cross-read-boundary
//! OSC reassembly and bounds its own buffers (the untrusted-input DoS cap).
//!
//! Grammar (inner, after tmux passthrough unwraps it):
//!   ESC ] 7366 ; remora ; <ver> ; <type> ; <state-b64> [ ; <msg-b64> ] BEL
//! vte splits the OSC string on ';' and hands us the segments (the `7366` code
//! is the first segment).

use base64::Engine as _;
use remora_protocol::SessionStatus;

use super::{sanitize, SanitizedText};

const CODE: &[u8] = b"7366";
const TOKEN: &[u8] = b"remora";
const VERSION: &[u8] = b"1";
const TYPE_STATE: &[u8] = b"state";
const PAYLOAD_CAP: usize = 80;

/// A recognized marker: the asserted status and an optional preview message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerHit {
    pub status: SessionStatus,
    pub preview: Option<SanitizedText>,
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
        // vte 0.13.1: advance takes a single byte, not a slice.
        for &byte in chunk {
            self.parser.advance(&mut self.sink, byte);
        }
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
    // [7366, remora, ver, type, state-b64, (msg-b64)?]
    if params.len() < 5 || params.len() > 6 {
        return None;
    }
    if params[0] != CODE || params[1] != TOKEN || params[2] != VERSION || params[3] != TYPE_STATE {
        return None;
    }
    let status = decode_token(params[4])
        .as_deref()
        .and_then(status_from_token)?;
    let preview = params.get(5).and_then(|seg| {
        let text = decode_utf8(seg)?;
        let s = sanitize(&text, PAYLOAD_CAP);
        (!s.is_empty()).then_some(s)
    });
    Some(MarkerHit { status, preview })
}

/// base64 → UTF-8 → sanitized token string (control-stripped, capped).
fn decode_token(seg: &[u8]) -> Option<String> {
    let text = decode_utf8(seg)?;
    Some(sanitize(&text, PAYLOAD_CAP).into_string())
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

    #[test]
    fn parses_a_whole_state_marker() {
        let mut s = MarkerScanner::new();
        let hits = s.feed(&marker("d29ya2luZw==")); // "working"
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].status, SessionStatus::Working);
        assert!(hits[0].preview.is_none());
    }

    #[test]
    fn awaiting_token_maps_to_awaiting() {
        let mut s = MarkerScanner::new();
        let hits = s.feed(&marker("YXdhaXRpbmdfaW5wdXQ=")); // "awaiting_input"
        assert_eq!(hits[0].status, SessionStatus::Awaiting);
    }

    #[test]
    fn reassembles_marker_split_across_feeds() {
        let bytes = marker("aWRsZQ=="); // "idle"
        let (a, b) = bytes.split_at(10);
        let mut s = MarkerScanner::new();
        assert!(s.feed(a).is_empty()); // incomplete: vte buffers internally
        let hits = s.feed(b);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].status, SessionStatus::Idle);
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
        assert_eq!(hits[0].status, SessionStatus::Working);
        assert_eq!(
            hits[0].preview.as_ref().expect("preview").as_str(),
            "Run tests?"
        );
    }

    #[test]
    fn forged_oversized_payload_is_capped_or_rejected() {
        // A huge non-token state payload does not match → no hit, no unbounded growth.
        let big = base64::engine::general_purpose::STANDARD.encode("x".repeat(10_000));
        let mut s = MarkerScanner::new();
        let seq = format!("\x1b]7366;remora;1;state;{big}\x07");
        assert!(s.feed(seq.as_bytes()).is_empty());
    }
}
