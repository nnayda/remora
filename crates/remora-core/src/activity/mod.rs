//! Core-side agent-activity detection (ADR-0013): a pure, clock-free state
//! machine that turns the PTY byte stream into `SessionStatus` transitions and
//! sanitized previews. The settle clock lives in the bridge thread that drives
//! it (`transport::pty_process`), not here.

mod marker;
mod sanitize;

pub use marker::{MarkerHit, MarkerScanner};
pub use sanitize::{sanitize, SanitizedText};

use remora_protocol::SessionStatus;

/// One thing the detector wants the bridge to emit, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectorEvent {
    Status(SessionStatus),
    Preview(SanitizedText),
    /// First well-formed marker of any kind seen this attach (#198). One-shot:
    /// emitted once on the false→true edge of the `marker_seen` latch. Proves
    /// the agent's hook pipeline is wired, independent of activity state.
    MarkerSeen,
}

/// Clock-free per-session activity state machine. The settle timing lives in the
/// caller (the bridge thread calls `on_tick` once per silent settle window).
#[derive(Debug)]
pub struct Detector {
    state: SessionStatus,
    scanner: MarkerScanner,
    marker_seen: bool,
}

impl Detector {
    pub fn new() -> Self {
        Self {
            state: SessionStatus::Unknown,
            scanner: MarkerScanner::new(),
            marker_seen: false,
        }
    }

    /// Bytes arrived: activity ⇒ `Working`, unless a terminal marker
    /// (`idle`/`awaiting`) completed in this chunk, which wins. Emits a status
    /// only on transition, plus any preview messages in order.
    pub fn on_bytes(&mut self, chunk: &[u8]) -> Vec<DetectorEvent> {
        let hits = self.scanner.feed(chunk);
        let mut out = Vec::new();

        // No marker in this chunk: the mere arrival of bytes means Working
        // (emitted only on transition, so a byte firehose doesn't churn).
        if hits.is_empty() {
            if self.state != SessionStatus::Working {
                self.state = SessionStatus::Working;
                out.push(DetectorEvent::Status(SessionStatus::Working));
            }
            return out;
        }

        // Markers present: replay them in arrival order. The first marker of any
        // kind (this attach) also latches `marker_seen` and emits a one-shot
        // MarkerSeen, ordered ahead of that hit's own events.
        for h in hits {
            if !self.marker_seen {
                self.marker_seen = true;
                out.push(DetectorEvent::MarkerSeen);
            }
            match h {
                MarkerHit::State { status, preview } => {
                    if self.state != status {
                        self.state = status;
                        out.push(DetectorEvent::Status(status));
                    }
                    if let Some(preview) = preview {
                        out.push(DetectorEvent::Preview(preview));
                    }
                }
                MarkerHit::Liveness => {} // latch only; no status/preview
            }
        }
        out
    }

    /// A settle window elapsed with no bytes: `Working` ⇒ `Idle`. `awaiting` and
    /// `idle` are left as-is (never re-emitted), and `awaiting` is never produced
    /// here — it is marker-only.
    pub fn on_tick(&mut self) -> Vec<DetectorEvent> {
        if self.state == SessionStatus::Working {
            self.state = SessionStatus::Idle;
            vec![DetectorEvent::Status(SessionStatus::Idle)]
        } else {
            Vec::new()
        }
    }
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod detector_tests {
    use super::*;
    use remora_protocol::SessionStatus;

    fn statuses(evs: Vec<DetectorEvent>) -> Vec<SessionStatus> {
        evs.into_iter()
            .filter_map(|e| match e {
                DetectorEvent::Status(s) => Some(s),
                DetectorEvent::Preview(_) => None,
                DetectorEvent::MarkerSeen => None,
            })
            .collect()
    }

    fn marker_seen_count(evs: &[DetectorEvent]) -> usize {
        evs.iter()
            .filter(|e| matches!(e, DetectorEvent::MarkerSeen))
            .count()
    }

    #[test]
    fn first_bytes_emit_working_once_then_quiet() {
        let mut d = Detector::new();
        assert_eq!(statuses(d.on_bytes(b"hello")), vec![SessionStatus::Working]);
        // Still working: no churn while already Working.
        assert_eq!(statuses(d.on_bytes(b" world")), vec![]);
    }

    #[test]
    fn tick_settles_working_to_idle_once() {
        let mut d = Detector::new();
        d.on_bytes(b"x");
        assert_eq!(statuses(d.on_tick()), vec![SessionStatus::Idle]);
        assert_eq!(statuses(d.on_tick()), vec![]); // already Idle
    }

    #[test]
    fn bytes_after_idle_re_emit_working() {
        let mut d = Detector::new();
        d.on_bytes(b"x");
        d.on_tick(); // -> Idle
        assert_eq!(statuses(d.on_bytes(b"y")), vec![SessionStatus::Working]);
    }

    #[test]
    fn idle_marker_does_not_flash_working() {
        // base64 "idle" = "aWRsZQ==". A chunk that is only the marker must
        // settle to Idle without a spurious Working first.
        let mut d = Detector::new();
        let evs = statuses(d.on_bytes(b"\x1b]7366;remora;1;state;aWRsZQ==\x07"));
        assert_eq!(evs, vec![SessionStatus::Idle]);
    }

    #[test]
    fn awaiting_is_never_inferred_from_quiescence() {
        let mut d = Detector::new();
        d.on_bytes(b"some output");
        // Many ticks of silence: stays Idle, never Awaiting.
        for _ in 0..5 {
            let s = statuses(d.on_tick());
            assert!(!s.contains(&SessionStatus::Awaiting));
        }
    }

    #[test]
    fn marker_preview_is_emitted() {
        // "working" + "Run tests?" message segment.
        let mut d = Detector::new();
        let evs = d.on_bytes(b"\x1b]7366;remora;1;state;d29ya2luZw==;UnVuIHRlc3RzPw==\x07");
        let previews: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                DetectorEvent::Preview(t) => Some(t.as_str().to_string()),
                DetectorEvent::Status(_) | DetectorEvent::MarkerSeen => None,
            })
            .collect();
        assert_eq!(previews, vec!["Run tests?".to_string()]);
    }

    #[test]
    fn multi_marker_chunk_preserves_order() {
        // Two complete markers coalesced into one chunk:
        //   working + preview "A"  (b64 "A" = "QQ==")
        //   idle    + preview "B"  (b64 "B" = "Qg==")
        // must stay ordered: MarkerSeen, Status(Working), Preview(A), Status(Idle), Preview(B)
        // — not Status(Idle) then both previews. MarkerSeen latches once, ahead
        // of the first hit's own events (#198).
        let mut d = Detector::new();
        let chunk = b"\x1b]7366;remora;1;state;d29ya2luZw==;QQ==\x07\
\x1b]7366;remora;1;state;aWRsZQ==;Qg==\x07";
        let evs = d.on_bytes(chunk);
        assert_eq!(evs.len(), 5, "got {evs:?}");
        assert!(matches!(evs[0], DetectorEvent::MarkerSeen));
        assert!(matches!(
            evs[1],
            DetectorEvent::Status(SessionStatus::Working)
        ));
        assert!(matches!(&evs[2], DetectorEvent::Preview(t) if t.as_str() == "A"));
        assert!(matches!(evs[3], DetectorEvent::Status(SessionStatus::Idle)));
        assert!(matches!(&evs[4], DetectorEvent::Preview(t) if t.as_str() == "B"));
    }

    #[test]
    fn lone_ping_latches_marker_seen_without_status() {
        // base64-free ping marker, alone in a chunk.
        let mut d = Detector::new();
        let evs = d.on_bytes(b"\x1b]7366;remora;1;ping\x07");
        assert_eq!(marker_seen_count(&evs), 1);
        assert!(
            evs.iter()
                .all(|e| !matches!(e, DetectorEvent::Status(_) | DetectorEvent::Preview(_))),
            "ping must not drive status or preview, got {evs:?}"
        );
    }

    #[test]
    fn awaiting_marker_also_latches_marker_seen() {
        let mut d = Detector::new();
        // "awaiting_input" state marker.
        let evs = d.on_bytes(b"\x1b]7366;remora;1;state;YXdhaXRpbmdfaW5wdXQ=\x07");
        assert_eq!(marker_seen_count(&evs), 1);
        assert_eq!(statuses(evs), vec![SessionStatus::Awaiting]);
    }

    #[test]
    fn marker_seen_latch_is_idempotent() {
        let mut d = Detector::new();
        let first = d.on_bytes(b"\x1b]7366;remora;1;ping\x07");
        assert_eq!(marker_seen_count(&first), 1);
        // A later awaiting marker must NOT re-emit MarkerSeen.
        let second = d.on_bytes(b"\x1b]7366;remora;1;state;YXdhaXRpbmdfaW5wdXQ=\x07");
        assert_eq!(marker_seen_count(&second), 0);
    }

    #[test]
    fn byte_firehose_never_latches_marker_seen() {
        let mut d = Detector::new();
        assert_eq!(marker_seen_count(&d.on_bytes(b"lots of output")), 0);
        assert_eq!(marker_seen_count(&d.on_tick()), 0);
        assert_eq!(marker_seen_count(&d.on_bytes(b"more")), 0);
    }
}
