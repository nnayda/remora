//! `SourceError` ↔ [`WireError`] projection for relay mode (ADR-0021).
//!
//! The bridge answers a client's request with a [`SourceError`] from the local
//! [`SessionSource`](remora_core::SessionSource); the wire cannot carry that
//! type (it lives in `remora-core`, which `remora-protocol` must not depend on),
//! so [`map_source_error`] projects it onto the stable [`WireError`]. The client
//! reverses the projection with [`map_wire_error`].
//!
//! Two rules make the projection trustworthy:
//!
//! - **Structured context survives structurally.** `SessionExists` /
//!   `SessionNotFound` carry validated [`ProjectId`]/[`SessionId`] ids; those
//!   ride the wire as ids, so the client rebuilds the exact same error.
//! - **Message-bearing variants carry the sender's own escaped `Display`.**
//!   `WorkspaceDirty`, `Plan`, and `Transport` have no structural wire analogue
//!   (their payload is a `DirtyReason`, a `PlanError`, or backend-specific
//!   bytes), so they ride as a `message: String` built from the *whole
//!   `SourceError`'s* `Display` — never hand-assembled. This matters most for
//!   `Transport`: its `Display` escapes and bounds remote-influenced bytes, and
//!   feeding that escaped string onto the wire is what keeps a hostile backend
//!   from smuggling terminal-escape sequences to the client (spec review C15).

use remora_core::SourceError;
use remora_protocol::WireError;

/// Projects a local [`SourceError`] onto the wire [`WireError`] (bridge side).
///
/// Structured variants keep their ids; the message-bearing variants carry the
/// *whole error's* `Display` (`e.to_string()`) — never a hand-assembled string —
/// so `SourceError::Transport`'s escaping of remote-influenced bytes is what
/// lands on the wire. The `#[non_exhaustive]` wildcard folds any future variant
/// onto `Transport { message }` with its own `Display`, which stays display-safe
/// by the same rule.
pub fn map_source_error(e: &SourceError) -> WireError {
    match e {
        SourceError::SessionExists {
            project_id,
            session_id,
        } => WireError::SessionExists {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        },
        SourceError::SessionNotFound {
            project_id,
            session_id,
        } => WireError::SessionNotFound {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
        },
        SourceError::WorkspaceDirty { .. } => WireError::WorkspaceDirty {
            message: e.to_string(),
        },
        SourceError::Plan(_) => WireError::Plan {
            message: e.to_string(),
        },
        SourceError::ChannelClosed => WireError::ChannelClosed,
        SourceError::Transport(_) => WireError::Transport {
            message: e.to_string(),
        },
        // `SourceError` is `#[non_exhaustive]`: fold any variant this crate was
        // not compiled against onto a display-safe `Transport` message.
        _ => WireError::Transport {
            message: e.to_string(),
        },
    }
}

/// Reverses [`map_source_error`] on the client side.
///
/// Structured variants reconstruct their exact ids. The message-bearing wire
/// variants have no structural `SourceError` analogue to rebuild (their id +
/// reason / plan payload never rode the wire), so they land as
/// [`SourceError::Transport`] carrying the already-escaped message verbatim —
/// the human-readable text is preserved, and no re-escaping is needed because
/// the string arrived display-safe.
pub fn map_wire_error(e: WireError) -> SourceError {
    match e {
        WireError::SessionExists {
            project_id,
            session_id,
        } => SourceError::SessionExists {
            project_id,
            session_id,
        },
        WireError::SessionNotFound {
            project_id,
            session_id,
        } => SourceError::SessionNotFound {
            project_id,
            session_id,
        },
        WireError::WorkspaceDirty { message } => SourceError::Transport(message),
        WireError::Plan { message } => SourceError::Transport(message),
        WireError::ChannelClosed => SourceError::ChannelClosed,
        WireError::Transport { message } => SourceError::Transport(message),
        // `WireError` is `#[non_exhaustive]`: a bridge speaking a newer protocol
        // could send a variant this client predates. Surface it as an opaque
        // transport failure rather than dropping it silently.
        _ => SourceError::Transport("unrecognized wire error variant".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remora_core::{DirtyReason, PlanError};
    use remora_protocol::{ProjectId, SessionId};

    fn project() -> ProjectId {
        ProjectId::new("api").expect("valid slug")
    }

    fn session() -> SessionId {
        SessionId::new("fix-login").expect("valid slug")
    }

    /// Every `SourceError` variant maps to the wire and back: structured
    /// variants reconstruct their ids exactly, and the message-bearing
    /// variants carry the sender's own (escaped) `Display` onto the wire and
    /// preserve it verbatim through the inverse.
    #[test]
    fn wire_error_mapping_is_exhaustive_and_inverse() {
        // --- SessionExists: structural, reconstructs its ids exactly. ---
        let e = SourceError::SessionExists {
            project_id: project(),
            session_id: session(),
        };
        let wire = map_source_error(&e);
        assert_eq!(
            wire,
            WireError::SessionExists {
                project_id: project(),
                session_id: session(),
            }
        );
        match map_wire_error(wire) {
            SourceError::SessionExists {
                project_id,
                session_id,
            } => {
                assert_eq!(project_id, project());
                assert_eq!(session_id, session());
            }
            other => panic!("expected SessionExists, got {other:?}"),
        }

        // --- SessionNotFound: structural, reconstructs its ids exactly. ---
        let e = SourceError::SessionNotFound {
            project_id: project(),
            session_id: SessionId::new("gone").expect("slug"),
        };
        let wire = map_source_error(&e);
        assert_eq!(
            wire,
            WireError::SessionNotFound {
                project_id: project(),
                session_id: SessionId::new("gone").expect("slug"),
            }
        );
        match map_wire_error(wire) {
            SourceError::SessionNotFound {
                project_id,
                session_id,
            } => {
                assert_eq!(project_id, project());
                assert_eq!(session_id.as_str(), "gone");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }

        // --- WorkspaceDirty: message = the whole SourceError Display,
        //     including the DirtyReason text; inverse carries it verbatim. ---
        let e = SourceError::WorkspaceDirty {
            project_id: project(),
            session_id: session(),
            reason: DirtyReason::Both,
        };
        let display = e.to_string();
        assert!(
            display.contains("uncommitted changes and commits not on any remote"),
            "DirtyReason text must be in the Display: {display}"
        );
        let wire = map_source_error(&e);
        assert_eq!(
            wire,
            WireError::WorkspaceDirty {
                message: display.clone(),
            }
        );
        match map_wire_error(wire) {
            SourceError::Transport(m) => assert_eq!(m, display),
            other => panic!("expected Transport, got {other:?}"),
        }

        // --- Plan: message = the whole SourceError Display, including the
        //     PlanError text; inverse carries it verbatim. ---
        let e = SourceError::Plan(PlanError::UnknownProject(
            ProjectId::new("ghost").expect("slug"),
        ));
        let display = e.to_string();
        assert!(
            display.contains("ghost"),
            "PlanError id must survive into the Display: {display}"
        );
        let wire = map_source_error(&e);
        assert_eq!(
            wire,
            WireError::Plan {
                message: display.clone(),
            }
        );
        match map_wire_error(wire) {
            SourceError::Transport(m) => assert_eq!(m, display),
            other => panic!("expected Transport, got {other:?}"),
        }

        // --- ChannelClosed: unit, exact round trip. ---
        let e = SourceError::ChannelClosed;
        let wire = map_source_error(&e);
        assert_eq!(wire, WireError::ChannelClosed);
        assert!(matches!(map_wire_error(wire), SourceError::ChannelClosed));

        // --- Transport: message = the ESCAPED Display; the escaping is the
        //     whole point, and it must survive onto the wire unchanged. ---
        let e = SourceError::Transport("\x1b]0;pwn\x07x".to_string());
        let display = e.to_string();
        assert_eq!(
            display, r"transport error: \u{1b}]0;pwn\u{7}x",
            "SourceError::Transport must escape control bytes in Display"
        );
        let wire = map_source_error(&e);
        assert_eq!(
            wire,
            WireError::Transport {
                message: display.clone(),
            },
            "the raw control bytes must NEVER reach the wire — only the escaped form"
        );
        match map_wire_error(wire) {
            SourceError::Transport(m) => assert_eq!(m, display),
            other => panic!("expected Transport, got {other:?}"),
        }
    }
}
