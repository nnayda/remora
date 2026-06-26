//! Scrub untrusted marker payloads (ADR-0010 threat model): strip the control
//! bytes a terminal would consume (incl. ESC, so any re-injected escape becomes
//! inert text), then length-cap. Port of `terminal-text.ts` (#55), simplified:
//! a `char::is_control()` filter covers C0 + C1 + DEL + \t\n\r, which is exactly
//! the `keepWhitespace:false` behavior used by the marker path. (#61 may switch
//! to vte-print stripping for cleaner previews; not needed while preview is the
//! dormant pipe.)

use serde::Serialize;

/// Text that has passed [`sanitize`]. Constructing a [`PreviewUpdate`] from
/// arbitrary `String` is possible at the protocol layer, but inside core the
/// only path to one is via this newtype — making the sanitize step structural.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SanitizedText(String);

impl SanitizedText {
    /// The scrubbed, capped inner string.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Borrow the scrubbed string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when nothing survived scrubbing (caller may skip emitting).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Strip control characters (C0/C1/DEL incl. ESC, tab, newline, CR) then cap to
/// `cap` chars, marking truncation with a trailing `…`. `cap == 0` clamps to 1.
pub fn sanitize(text: &str, cap: usize) -> SanitizedText {
    let stripped: String = text.chars().filter(|c| !c.is_control()).collect();
    let limit = cap.max(1);
    let capped = if stripped.chars().count() <= limit {
        stripped
    } else {
        let head: String = stripped.chars().take(limit.saturating_sub(1)).collect();
        format!("{head}…")
    };
    SanitizedText(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_bytes_and_escapes() {
        // ESC (0x1b), C0 controls, DEL, and \n\r\t are all stripped.
        assert_eq!(
            sanitize("wo\x1b[31mrking\n", 80).into_string(),
            "wo[31mrking"
        );
        assert_eq!(sanitize("a\tb\rc\x07d\x7f", 80).into_string(), "abcd");
    }

    #[test]
    fn caps_with_ellipsis() {
        assert_eq!(sanitize("abcdef", 4).into_string(), "abc…");
        assert_eq!(sanitize("abc", 4).into_string(), "abc");
    }

    #[test]
    fn zero_cap_is_clamped_not_panicking() {
        // A non-positive cap clamps to 1 rather than panicking or empty-slicing.
        let out = sanitize("abc", 0).into_string();
        assert!(out.chars().count() >= 1);
    }
}
