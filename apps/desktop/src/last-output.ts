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

import { capWithEllipsis, stripTerminalEscapes } from "./terminal-text";

/**
 * Decode `bytes`, strip terminal escapes, and return the last non-empty line,
 * truncated to `maxLen` (a trailing `…` marks truncation). Returns `""` when no
 * visible text remains. A `\r`-overwritten line yields only its final segment.
 */
export function extractCause(bytes: Uint8Array, maxLen = 200): string {
  const text = stripTerminalEscapes(
    new TextDecoder("utf-8", { fatal: false }).decode(bytes),
    { keepWhitespace: true },
  );
  // Split on newline AND carriage return so a CR-overwritten progress line
  // ("50%\r100%") yields its final segment.
  const lines = text.split(/[\r\n]/);
  let last = "";
  for (const line of lines) {
    const trimmed = line.trim();
    if (trimmed) last = trimmed;
  }
  return capWithEllipsis(
    last,
    Number.isFinite(maxLen) ? Math.max(1, Math.trunc(maxLen)) : 200,
  );
}
