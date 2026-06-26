/**
 * Shared terminal-text scrubbing: strip the escape/control bytes the emulator
 * would have consumed, cap length, and base64-decode UTF-8 payloads. Extracted
 * from `last-output.ts` (#28) so that `last-output` and `terminal-controller`'s
 * OSC-52 clipboard path share one trusted, security-relevant scrubber.
 */

// CSI: ESC [ , params/intermediates, final byte @-~.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching escapes needs the ESC byte.
const CSI = /\x1b\[[0-?]*[ -/]*[@-~]/g;
// OSC: ESC ] … terminated by BEL (\x07) or ST (ESC \). The character class
// [^\x07] matches any byte except BEL, but allows an embedded ESC only when
// it is NOT followed by \ (i.e. not an ST opener), so a sequence like
// ESC]0;title ESC[31m boom\x07 is consumed in full rather than stopping at
// the inner ESC and leaving the tail unstripped.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching escapes needs the ESC/BEL bytes.
const OSC = /\x1b\](?:[^\x07]|\x1b(?!\\))*?(?:\x07|\x1b\\)/g;
// Any other escape: ESC, optional intermediates, optional final byte.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching escapes needs the ESC byte.
const ESC = /\x1b[ -/]*[0-~]?/g;
// C0 controls EXCEPT \t (\x09), \n (\x0a), \r (\x0d).
// biome-ignore lint/suspicious/noControlCharactersInRegex: stripping control bytes is the point.
const CTRL_KEEP_WS = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g;
// ALL C0 controls + DEL, including \t\n\r (a status token has none).
// biome-ignore lint/suspicious/noControlCharactersInRegex: stripping control bytes is the point.
const CTRL_ALL = /[\x00-\x1f\x7f]/g;

/** Strip the terminal escapes/controls an emulator would consume. With
 * `keepWhitespace` (default true) `\t\n\r` survive (line/column structure is
 * preserved for callers that split on them); false strips every control. */
export function stripTerminalEscapes(
  text: string,
  opts: { keepWhitespace?: boolean } = {},
): string {
  const keepWhitespace = opts.keepWhitespace ?? true;
  return text
    .replace(OSC, "")
    .replace(CSI, "")
    .replace(ESC, "")
    .replace(keepWhitespace ? CTRL_KEEP_WS : CTRL_ALL, "");
}

/** Truncate to `maxLen`, marking truncation with a trailing `…`. A non-finite
 * or non-positive `maxLen` is clamped to a sane minimum. */
export function capWithEllipsis(text: string, maxLen: number): string {
  const limit = Number.isFinite(maxLen) ? Math.max(1, Math.trunc(maxLen)) : 200;
  if (text.length <= limit) return text;
  return `${text.slice(0, limit - 1)}…`;
}

// fatal: an invalid-UTF-8 payload throws here rather than landing U+FFFD.
const fatalDecoder = new TextDecoder("utf-8", { fatal: true });

/** Decode a base64 string whose bytes are UTF-8. Throws on malformed base64
 * (`atob`) or invalid UTF-8 (the decoder) — callers handle the throw. */
export function decodeBase64Utf8(b64: string): string {
  const binary = atob(b64);
  const bytes = Uint8Array.from(binary, (ch) => ch.charCodeAt(0));
  return fatalDecoder.decode(bytes);
}
