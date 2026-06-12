//! Messages flowing over one attached session channel.
//!
//! A channel is a byte stream plus resize, nothing smarter (spine spike):
//! clients send keystrokes and window geometry, the session sends raw PTY
//! output. Bytes are opaque — screen state belongs to the client's terminal
//! emulator, and nothing transport- or core-side parses ANSI. There is no
//! "detached" message: channel death is only observable locally, and each
//! transport owns its own disconnect semantics.

use serde::{Deserialize, Serialize};

/// Terminal geometry in character cells.
///
/// Note: tmux sizes its window to the smallest attached client and reserves
/// a status line, so the geometry the agent sees may differ from the
/// requested size (e.g. request 30 rows, get 29). The protocol carries the
/// requested size; compensation, if any, is a client concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

/// Client → session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelInput {
    /// Raw input bytes (keystrokes, pastes) for the session's PTY.
    Bytes(Vec<u8>),
    /// Propagate a client-side terminal resize to the remote TTY.
    Resize(TerminalSize),
}

/// Session → client.
///
/// A single variant today; an enum so the wire shape can grow (e.g. a
/// relay-side close reason) without changing framing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelOutput {
    /// Raw PTY output. Feed to a terminal emulator; never parse.
    Bytes(Vec<u8>),
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
        let msg = ChannelInput::Resize(TerminalSize {
            rows: 30,
            cols: 100,
        });
        let json = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(json, r#"{"resize":{"rows":30,"cols":100}}"#);
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, back);
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
    fn bytes_are_opaque_not_utf8() {
        // Raw PTY streams are not valid UTF-8 (split multibyte sequences,
        // ANSI control bytes) — the protocol must never assume text.
        let invalid_utf8 = vec![0xff, 0xfe, 0x80];
        let msg = ChannelInput::Bytes(invalid_utf8.clone());
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: ChannelInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ChannelInput::Bytes(invalid_utf8));
    }
}
