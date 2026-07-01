//! Frontend-facing output: the streamed message, the sink seam, the handle.

/// Frontend-facing mirror of `remora_protocol::SessionStatus` (which is
/// specta-agnostic). A local DTO keeps the protocol crate dependency-light while
/// giving the generated TS a clean string union. snake_case matches the
/// frontend `ActivityState` tokens ("working" | "idle" | "awaiting" | "unknown").
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusDto {
    Working,
    Idle,
    Awaiting,
    Unknown,
}

impl From<remora_protocol::SessionStatus> for SessionStatusDto {
    fn from(s: remora_protocol::SessionStatus) -> Self {
        use remora_protocol::SessionStatus as S;
        match s {
            S::Working => Self::Working,
            S::Idle => Self::Idle,
            S::Awaiting => Self::Awaiting,
            S::Unknown => Self::Unknown,
            // SessionStatus is #[non_exhaustive]; an unknown future value is
            // surfaced as Unknown rather than failing the stream.
            _ => Self::Unknown,
        }
    }
}

/// Streamed from a session's PTY to the frontend. Internally tagged + camelCase
/// so the generated TS is a clean discriminated union. A local bridge<->frontend
/// DTO (NOT a wire-protocol type), so it is free to grow.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum BridgeOutput {
    Bytes {
        bytes: Vec<u8>,
    },
    Closed,
    StatusChange {
        status: SessionStatusDto,
    },
    PreviewUpdate {
        preview: String,
    },
    /// The activity-hook pipeline is confirmed wired for this session (#198):
    /// core parsed its first OSC-7366 marker this attach. Presence is the signal.
    MarkerSeen,
}

/// The frontend stopped listening (its Channel receiver is gone).
pub struct SinkClosed;

/// Where a forward task writes output. Production wraps `tauri::ipc::Channel`;
/// tests use an mpsc-backed sink, so the forward loop is provable without Tauri.
pub trait OutputSink: Send + Sync + 'static {
    fn send(&self, msg: BridgeOutput) -> Result<(), SinkClosed>;
}

/// Production sink over a Tauri IPC channel.
pub struct ChannelSink(pub tauri::ipc::Channel<BridgeOutput>);

impl OutputSink for ChannelSink {
    fn send(&self, msg: BridgeOutput) -> Result<(), SinkClosed> {
        self.0.send(msg).map_err(|_| SinkClosed)
    }
}

/// Opaque handle the frontend uses to address one open channel (write/resize/close).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct ChannelHandle(pub u64);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bridge_output_wire_format() {
        let b = BridgeOutput::Bytes {
            bytes: vec![104, 105],
        };
        assert_eq!(
            serde_json::to_string(&b).expect("serialize"),
            r#"{"event":"bytes","bytes":[104,105]}"#
        );
        assert_eq!(
            serde_json::to_string(&BridgeOutput::Closed).expect("serialize"),
            r#"{"event":"closed"}"#
        );
    }
    #[test]
    fn channel_handle_is_a_number() {
        assert_eq!(
            serde_json::to_string(&ChannelHandle(7)).expect("serialize"),
            "7"
        );
    }
    #[test]
    fn status_change_wire_format() {
        let b = BridgeOutput::StatusChange {
            status: SessionStatusDto::Awaiting,
        };
        assert_eq!(
            serde_json::to_string(&b).expect("serialize"),
            r#"{"event":"statusChange","status":"awaiting"}"#
        );
    }

    #[test]
    fn preview_update_wire_format() {
        let b = BridgeOutput::PreviewUpdate {
            preview: "run tests?".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&b).expect("serialize"),
            r#"{"event":"previewUpdate","preview":"run tests?"}"#
        );
    }

    #[test]
    fn marker_seen_wire_format() {
        let b = BridgeOutput::MarkerSeen;
        let json = serde_json::to_string(&b).expect("serialize");
        assert_eq!(json, r#"{"event":"markerSeen"}"#);
    }

    #[test]
    fn session_status_dto_maps_from_protocol() {
        use remora_protocol::SessionStatus;
        assert_eq!(
            SessionStatusDto::from(SessionStatus::Working),
            SessionStatusDto::Working
        );
        assert_eq!(
            SessionStatusDto::from(SessionStatus::Awaiting),
            SessionStatusDto::Awaiting
        );
        assert_eq!(
            SessionStatusDto::from(SessionStatus::Unknown),
            SessionStatusDto::Unknown
        );
    }
}
