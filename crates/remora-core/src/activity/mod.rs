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
}

/// Clock-free per-session activity state machine. The settle timing lives in the
/// caller (the bridge thread calls `on_tick` once per silent settle window).
#[derive(Debug)]
pub struct Detector {
    state: SessionStatus,
    scanner: MarkerScanner,
}

impl Detector {
    pub fn new() -> Self {
        Self {
            state: SessionStatus::Unknown,
            scanner: MarkerScanner::new(),
        }
    }

    /// Bytes arrived: activity ⇒ `Working`, unless a terminal marker
    /// (`idle`/`awaiting`) completed in this chunk, which wins. Emits a status
    /// only on transition, plus any preview messages in order.
    pub fn on_bytes(&mut self, chunk: &[u8]) -> Vec<DetectorEvent> {
        let hits = self.scanner.feed(chunk);
        let mut out = Vec::new();

        // The last status marker in the chunk (if any) decides the chunk's
        // resulting state; otherwise the mere arrival of bytes means Working.
        let marker_status = hits.last().map(|h| h.status);
        let new_state = marker_status.unwrap_or(SessionStatus::Working);
        if self.state != new_state {
            self.state = new_state;
            out.push(DetectorEvent::Status(new_state));
        }
        for h in hits {
            if let Some(preview) = h.preview {
                out.push(DetectorEvent::Preview(preview));
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
            })
            .collect()
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
                _ => None,
            })
            .collect();
        assert_eq!(previews, vec!["Run tests?".to_string()]);
    }
}
