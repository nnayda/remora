//! Scrub untrusted marker payloads (ADR-0010 threat model): strip the control
//! bytes a terminal would consume (incl. ESC, so any re-injected escape becomes
//! inert text), then length-cap. Port of `terminal-text.ts` (#55), simplified:
//! a `char::is_control()` filter covers C0 + C1 + DEL + \t\n\r, which is exactly
//! the `keepWhitespace:false` behavior used by the marker path. (#61 may switch
//! to vte-print stripping for cleaner previews; not needed while preview is the
//! dormant pipe.) Combining marks survive `is_control()` and let a payload stack
//! Zalgo garble on a base glyph, so we additionally bound them per grapheme
//! cluster (#197).

use serde::Serialize;
use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

/// Max combining marks kept per grapheme cluster. A Zalgo payload stacks dozens
/// on one base to garble the preview; bounding the count neutralizes the attack
/// without dropping marks wholesale (which would mangle legitimate accents). The
/// cap is per *cluster*, not a per-char run, so interleaving spacing marks or
/// other grapheme extenders can't reset it and reassemble an unbounded stack
/// (#197). Four leaves headroom for scripts that legitimately stack 3–4 marks on
/// one base (Biblical Hebrew, Quranic Arabic) while still crushing Zalgo.
const MAX_COMBINING_RUN: usize = 4;

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
    let stripped = strip(text);
    let limit = cap.max(1);
    let capped = if stripped.chars().count() <= limit {
        stripped
    } else {
        let head: String = stripped.chars().take(limit.saturating_sub(1)).collect();
        format!("{head}…")
    };
    SanitizedText(capped)
}

/// Drop [`is_stripped`] characters, then cap stacking combining marks to
/// [`MAX_COMBINING_RUN`] per grapheme cluster.
///
/// Stripping runs first so a stripped char between marks can't split one base's
/// stack into two clusters (the marks still pile on the same rendered base).
/// Bounding then works on grapheme clusters — the unit that actually renders as
/// one glyph — so interleaving a spacing mark or other extender can't reset a
/// per-char counter and reassemble an unbounded stack. Marks in a cluster with
/// no base (leading orphans) attach to nothing trustworthy and are dropped.
fn strip(text: &str) -> String {
    let cleaned: String = text.chars().filter(|c| !is_stripped(*c)).collect();
    let mut out = String::new();
    for cluster in cleaned.graphemes(true) {
        let mut kept_marks = 0usize;
        let mut has_base = false;
        for c in cluster.chars() {
            if is_stacking_mark(c) {
                if has_base && kept_marks < MAX_COMBINING_RUN {
                    kept_marks += 1;
                    out.push(c);
                }
                // else: over budget, or orphan with no base — drop.
            } else {
                // A spacing mark (Mc) extends the cluster without being a base,
                // so it passes through but doesn't unlock more stacking marks.
                if !is_mark(c) {
                    has_base = true;
                }
                out.push(c);
            }
        }
    }
    out
}

/// True for combining marks that stack invisibly over a base glyph: nonspacing
/// marks (Mn, e.g. U+0300–U+036F) and enclosing marks (Me). These are the Zalgo
/// vector — bounded per cluster. Spacing marks (Mc) advance width instead of
/// stacking, so they aren't bounded (only kept from resetting the budget).
fn is_stacking_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark
    )
}

/// True for any combining mark (Mn/Mc/Me) — none of which begins a new base
/// grapheme, so none should re-arm the per-cluster stacking budget.
fn is_mark(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
    )
}

/// True for characters dropped by [`sanitize`]: C0/C1/DEL control chars (incl.
/// ESC, tab, newline, CR) plus the Unicode *format* characters that pass
/// `is_control()` but can spoof rendered text — bidi overrides/embeddings/
/// isolates, zero-width joiners/spaces, and the BOM/annotation marks.
fn is_stripped(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'   // zero-width space/joiner, LRM/RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings + LRO/RLO overrides
            | '\u{2028}' | '\u{2029}' // line/para separators — is_control() misses these
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}'              // BOM / zero-width no-break space
            | '\u{FFF9}'..='\u{FFFB}' // interlinear annotation marks
        )
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

    #[test]
    fn strips_unicode_format_chars() {
        // Bidi override / zero-width / BOM pass char::is_control() but can spoof
        // rendered text — they must be stripped too.
        assert_eq!(sanitize("ab\u{202e}cd", 80).into_string(), "abcd"); // RLO
        assert_eq!(sanitize("a\u{200b}b\u{200d}c", 80).into_string(), "abc"); // ZWSP/ZWJ
        assert_eq!(sanitize("\u{feff}done", 80).into_string(), "done"); // BOM
        assert_eq!(sanitize("a\u{2066}b\u{2069}c", 80).into_string(), "abc"); // isolates
    }

    #[test]
    fn strips_unicode_line_para_separators() {
        // U+2028 and U+2029 are not caught by is_control() but can inject hard
        // line breaks into tooltip text, faking a trusted second line.
        assert_eq!(sanitize("a\u{2028}b\u{2029}c", 80).into_string(), "abc");
    }

    /// Count of nonspacing combining marks (U+0300) in a sanitized string — the
    /// Zalgo-stacking vector the bound is meant to cap.
    fn zalgo_marks(s: &str) -> usize {
        s.chars().filter(|&c| c == '\u{0300}').count()
    }

    #[test]
    fn bounds_zalgo_combining_mark_stacks() {
        // A Zalgo payload stacks many combining marks on one base to garble the
        // tooltip. Bound the marks per grapheme cluster rather than dropping
        // them wholesale (which would mangle legitimate accents). "a" + 8 marks
        // + "b" is two clusters; each cluster's marks cap at MAX_COMBINING_RUN.
        let zalgo = format!("a{m}b", m = "\u{0300}".repeat(8));
        assert_eq!(
            sanitize(&zalgo, 80).into_string(),
            format!("a{m}b", m = "\u{0300}".repeat(MAX_COMBINING_RUN))
        );
    }

    #[test]
    fn preserves_legitimate_decomposed_accents() {
        // "é" as e + U+0301 (NFD) is one combining mark — under the cap, so it
        // must survive untouched.
        assert_eq!(sanitize("e\u{0301}", 80).into_string(), "e\u{0301}");
        // A base with two marks (e.g. Vietnamese ế decomposed) is still legit.
        assert_eq!(
            sanitize("e\u{0302}\u{0301}", 80).into_string(),
            "e\u{0302}\u{0301}"
        );
    }

    #[test]
    fn preserves_stacked_marks_up_to_the_cap() {
        // Scripts like Biblical Hebrew / Quranic Arabic legitimately stack 3–4
        // nonspacing marks on one base. A run at the cap survives intact.
        let at_cap = "\u{0300}".repeat(MAX_COMBINING_RUN);
        assert_eq!(
            sanitize(&format!("a{at_cap}"), 80).into_string(),
            format!("a{at_cap}")
        );
    }

    #[test]
    fn combining_bound_is_per_grapheme_cluster() {
        // Each base grapheme gets its own allowance; the cap doesn't leak across
        // bases, and each base's stack is bounded independently.
        let s = format!("a{m}b{m}", m = "\u{0300}".repeat(8));
        let cap = "\u{0300}".repeat(MAX_COMBINING_RUN);
        assert_eq!(sanitize(&s, 80).into_string(), format!("a{cap}b{cap}"));
    }

    #[test]
    fn bounds_combining_enclosing_marks() {
        // Enclosing marks (Me, e.g. U+20DD combining enclosing circle) stack
        // over a base the same way and are bounded too.
        let s = format!("a{m}", m = "\u{20DD}".repeat(8));
        assert_eq!(
            sanitize(&s, 80).into_string(),
            format!("a{m}", m = "\u{20DD}".repeat(MAX_COMBINING_RUN))
        );
    }

    #[test]
    fn spacing_marks_cannot_bypass_the_combining_bound() {
        // A spacing combining mark (Mc, e.g. U+0903 Devanagari visarga) extends
        // the same grapheme cluster — it must NOT reset the per-cluster mark
        // budget. Interleaving it every two Zalgo marks used to reassemble an
        // unbounded stack; the bound now holds cluster-wide (#197 finding A).
        let payload = format!("a{}", "\u{0300}\u{0300}\u{0903}".repeat(50));
        let out = sanitize(&payload, 4096).into_string();
        assert!(
            zalgo_marks(&out) <= MAX_COMBINING_RUN,
            "combining marks not bounded: {}",
            zalgo_marks(&out)
        );
    }

    #[test]
    fn extend_symbols_cannot_bypass_the_combining_bound() {
        // An emoji modifier (U+1F3FB, category Sk) is a grapheme extender, not a
        // new base. Like Mc it must not reset the mark budget.
        let payload = format!("a{}", "\u{0300}\u{0300}\u{1F3FB}".repeat(50));
        let out = sanitize(&payload, 4096).into_string();
        assert!(
            zalgo_marks(&out) <= MAX_COMBINING_RUN,
            "combining marks not bounded: {}",
            zalgo_marks(&out)
        );
    }

    #[test]
    fn drops_orphan_leading_combining_marks() {
        // Marks with no base char in their cluster attach to nothing (or to
        // adjacent trusted UI chrome) — drop them entirely (#197 finding C).
        assert_eq!(sanitize("\u{0300}\u{0300}\u{0300}x", 80).into_string(), "x");
    }

    #[test]
    fn stripped_format_char_does_not_reset_combining_bound() {
        // A stripped format char between marks is removed entirely, so the marks
        // still pile on the same base — the bound must carry across the gap.
        let s = format!("a{m}\u{200b}{m}", m = "\u{0300}".repeat(4));
        let out = sanitize(&s, 80).into_string();
        assert!(zalgo_marks(&out) <= MAX_COMBINING_RUN);
    }
}
