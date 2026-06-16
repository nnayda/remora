//! Frontend-facing output: the streamed message, the sink seam, the handle.

/// Streamed from a session's PTY to the frontend. Internally tagged + camelCase
/// so the generated TS is a clean discriminated union. A local bridge<->frontend
/// DTO (NOT a wire-protocol type), so it is free to grow.
#[derive(Clone, Debug, serde::Serialize, specta::Type)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum BridgeOutput {
    Bytes { bytes: Vec<u8> },
    Closed,
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
}
