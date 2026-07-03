//! Messages flowing over one attached session channel.
//!
//! A channel is a byte stream plus resize, nothing smarter (spine spike):
//! clients send keystrokes and window geometry, the session sends raw PTY
//! output. Bytes are opaque — screen state belongs to the client's terminal
//! emulator, and nothing transport- or core-side parses ANSI. There is no
//! "detached" message: channel death is only observable locally, and each
//! transport owns its own disconnect semantics.
//!
//! Byte payloads are unbounded at the type level: transports own framing and
//! must cap message size (a peer could otherwise force unbounded
//! allocations). The JSON encoding of bytes as a number array is deliberate
//! for now — a binary relay codec is a later, type-compatible swap.

use serde::{Deserialize, Serialize};

/// Error returned when a terminal size has zero rows or columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTerminalSizeError {
    rows: u16,
    cols: u16,
}

impl std::fmt::Display for InvalidTerminalSizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid terminal size {}x{}: rows and cols must be nonzero",
            self.rows, self.cols
        )
    }
}

impl std::error::Error for InvalidTerminalSizeError {}

/// Terminal geometry in character cells. Rows and cols are always nonzero:
/// a 0x0 winsize reaching the remote TTY is a classic source of
/// divide-by-zero and rendering bugs, so it is rejected at every
/// construction and deserialization path.
///
/// Note: Remora-spawned sessions set `window-size latest` (tmux >= 3.1), so
/// the window follows the latest client to write — the geometry the agent sees
/// tracks the most recently active client rather than being clamped to the
/// smallest one. On tmux < 3.1 the option is absent and tmux falls back to
/// sizing the window to the smallest attached client. Either way tmux reserves
/// a status line, so the geometry the agent sees may still differ from the
/// requested size (e.g. request 30 rows, get 29). The protocol carries the
/// requested size; compensation, if any, is a client concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireTerminalSize", into = "WireTerminalSize")]
pub struct TerminalSize {
    rows: u16,
    cols: u16,
}

impl TerminalSize {
    /// Validates and wraps a geometry; zero rows or cols are rejected.
    pub fn new(rows: u16, cols: u16) -> Result<Self, InvalidTerminalSizeError> {
        if rows == 0 || cols == 0 {
            Err(InvalidTerminalSizeError { rows, cols })
        } else {
            Ok(Self { rows, cols })
        }
    }

    /// Height in character cells; never zero.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Width in character cells; never zero.
    pub fn cols(&self) -> u16 {
        self.cols
    }
}

/// The unvalidated wire shape of [`TerminalSize`].
#[derive(Serialize, Deserialize)]
struct WireTerminalSize {
    rows: u16,
    cols: u16,
}

impl TryFrom<WireTerminalSize> for TerminalSize {
    type Error = InvalidTerminalSizeError;

    fn try_from(wire: WireTerminalSize) -> Result<Self, Self::Error> {
        Self::new(wire.rows, wire.cols)
    }
}

impl From<TerminalSize> for WireTerminalSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
        }
    }
}

/// Client → session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelInput {
    /// Raw input bytes (keystrokes, pastes) for the session's PTY.
    Bytes(Vec<u8>),
    /// Propagate a client-side terminal resize to the remote TTY.
    Resize(TerminalSize),
}

/// Session → client.
///
/// Carries raw PTY output and activity events. Adding a variant is a breaking
/// protocol change: externally tagged serde enums reject unknown variants, so
/// older clients fail closed rather than skipping unknown messages. Growth
/// therefore requires a [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelOutput {
    /// Raw PTY output. Feed to a terminal emulator; never parse.
    Bytes(Vec<u8>),
    /// A change in detected agent activity (ADR-0013). Rides the same stream as
    /// `Bytes`, ordered after the bytes that triggered it.
    StatusChange(crate::SessionStatus),
    /// A short, already-sanitized one-line preview of the latest agent output
    /// (ADR-0013). The sender (core) control-strips + length-caps the untrusted
    /// payload before constructing this; consumers render it as text.
    PreviewUpdate(String),
    /// One-shot: the OSC-7366 scanner parsed its first well-formed marker on this
    /// channel this attach (#198). Proves the agent's activity hook is wired,
    /// independent of activity state. Carries no data — presence is the signal.
    MarkerSeen,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_bytes_wire_format() {
        let msg = ChannelInput::Bytes(b"hi".to_vec());
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"bytes":[104,105]}"#);
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn input_resize_wire_format() {
        let size = TerminalSize::new(30, 100).expect("nonzero size");
        let msg = ChannelInput::Resize(size);
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"resize":{"rows":30,"cols":100}}"#);
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn zero_terminal_size_is_rejected() {
        assert!(TerminalSize::new(0, 100).is_err());
        assert!(TerminalSize::new(30, 0).is_err());
        assert!(TerminalSize::new(0, 0).is_err());

        for bad in [
            r#"{"resize":{"rows":0,"cols":100}}"#,
            r#"{"resize":{"rows":30,"cols":0}}"#,
        ] {
            assert!(
                serde_json::from_str::<ChannelInput>(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn terminal_size_boundaries() {
        let max = TerminalSize::new(u16::MAX, u16::MAX).expect("nonzero size");
        let json = serde_json::to_string(&ChannelInput::Resize(max)).expect("serialize");
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ChannelInput::Resize(max));

        // Out-of-range wire values must fail cleanly, not wrap.
        for bad in [
            r#"{"resize":{"rows":70000,"cols":100}}"#,
            r#"{"resize":{"rows":-1,"cols":100}}"#,
        ] {
            assert!(
                serde_json::from_str::<ChannelInput>(bad).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn output_bytes_wire_format() {
        let msg = ChannelOutput::Bytes(vec![0x1b, b'[', b'2', b'J']);
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"bytes":[27,91,50,74]}"#);
        let back: ChannelOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
    }

    #[test]
    fn unknown_struct_fields_are_ignored_for_forward_compat() {
        let json = r#"{"resize":{"rows":30,"cols":100,"future_field":true}}"#;
        let msg: ChannelInput = serde_json::from_str(json).expect("deserialize");
        let size = TerminalSize::new(30, 100).expect("nonzero size");
        assert_eq!(msg, ChannelInput::Resize(size));
    }

    #[test]
    fn bytes_are_opaque_not_utf8() {
        // Raw PTY streams are not valid UTF-8 (split multibyte sequences,
        // ANSI control bytes) — the protocol must never assume text.
        let invalid_utf8 = vec![0xff, 0xfe, 0x80];
        let msg = ChannelInput::Bytes(invalid_utf8.clone());
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ChannelInput::Bytes(invalid_utf8));
    }

    #[test]
    fn status_change_wire_format() {
        let msg = ChannelOutput::StatusChange(crate::SessionStatus::Awaiting);
        let json = serde_json::to_string(&msg).expect("ser");
        assert_eq!(json, r#"{"status_change":"awaiting"}"#);
        let back: ChannelOutput = serde_json::from_str(&json).expect("de");
        assert_eq!(msg, back);
    }

    #[test]
    fn preview_update_wire_format() {
        let msg = ChannelOutput::PreviewUpdate("run tests? (y/n)".to_string());
        let json = serde_json::to_string(&msg).expect("ser");
        assert_eq!(json, r#"{"preview_update":"run tests? (y/n)"}"#);
        let back: ChannelOutput = serde_json::from_str(&json).expect("de");
        assert_eq!(msg, back);
    }

    #[test]
    fn marker_seen_wire_format() {
        let msg = ChannelOutput::MarkerSeen;
        let json = serde_json::to_string(&msg).expect("ser");
        assert_eq!(json, r#""marker_seen""#);
        let back: ChannelOutput = serde_json::from_str(&json).expect("de");
        assert_eq!(msg, back);
    }

    #[test]
    fn protocol_version_is_four() {
        assert_eq!(crate::PROTOCOL_VERSION, 4);
    }
}
