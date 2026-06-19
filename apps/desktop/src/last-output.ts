/**
 * Extract a short, human-readable "cause" line from raw PTY output bytes — the
 * last meaningful thing the terminal showed before a session died (#28). Used to
 * turn a bare "Session stopped." into "Session stopped: claude: command not
 * found" by reading the tail the connection already received.
 *
 * Pure and transport-agnostic: it only knows about terminal byte streams, never
 * about ssh/tmux/agents. The bytes are whatever the emulator would have rendered,
 * so we strip the escape sequences the emulator would have consumed and return
 * the last non-empty visible line.
 */

// Matching terminal escapes inherently needs the control bytes they start with
// (ESC = \x1b, BEL = \x07); that is the whole job of these patterns.

// CSI: ESC [ , params/intermediates, final byte @-~. Covers SGR colours, cursor
// moves, clears (`\x1b[2J`), etc.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching terminal escapes requires the ESC byte.
const CSI = /\x1b\[[0-?]*[ -/]*[@-~]/g;
// OSC: ESC ] … terminated by BEL (\x07) or ST (ESC \). Window-title sets, etc.
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching terminal escapes requires the ESC/BEL bytes.
const OSC = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;
// Any other escape: ESC followed by one byte (charset selects like ESC ( B, etc).
// biome-ignore lint/suspicious/noControlCharactersInRegex: matching terminal escapes requires the ESC byte.
const ESC = /\x1b[@-Z\\-_]?/g;
// Remaining C0 control chars except \t (\x09), \n (\x0a), \r (\x0d).
// biome-ignore lint/suspicious/noControlCharactersInRegex: stripping terminal control bytes is the point.
const CTRL = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g;

/**
 * Decode `bytes`, strip terminal escapes, and return the last non-empty line,
 * truncated to `maxLen` (a trailing `…` marks truncation). Returns `""` when no
 * visible text remains. A `\r`-overwritten line yields only its final segment.
 */
export function extractCause(bytes: Uint8Array, maxLen = 200): string {
  const text = new TextDecoder("utf-8", { fatal: false })
    .decode(bytes)
    .replace(OSC, "")
    .replace(CSI, "")
    .replace(ESC, "")
    .replace(CTRL, "");

  // Split on both newline and carriage return: a CR-overwritten progress line
  // ("50%\r100%") becomes separate segments, so the last segment is what was
  // actually left on screen.
  const lines = text.split(/[\r\n]/);
  let last = "";
  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed) last = trimmed;
  }

  if (last.length <= maxLen) return last;
  return `${last.slice(0, maxLen - 1)}…`;
}
