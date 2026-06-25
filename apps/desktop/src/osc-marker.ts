import {
  capWithEllipsis,
  decodeBase64Utf8,
  stripTerminalEscapes,
} from "./terminal-text";

/** The activity states a marker can assert. `awaiting` is the red "needs you"
 * state, only ever produced by an explicit marker (never inferred). */
export type MarkerState = "working" | "idle" | "awaiting";

export interface ActivityMarker {
  state: MarkerState;
}

const SUPPORTED_VERSION = "1";
const MARKER_TYPE = "state";
const TOKEN = "remora"; // collision guard (ADR-0010); the real scoping, not the number
const PAYLOAD_CAP = 80;

// Wire tokens → internal states. `awaiting_input` (protocol/spec word) maps to
// the shorter internal `awaiting`.
const STATE_BY_TOKEN: Record<string, MarkerState> = {
  working: "working",
  idle: "idle",
  awaiting_input: "awaiting",
};

/**
 * Parse the data of an OSC 7366 sequence (everything after `7366;`):
 * `remora;<ver>;<type>;<base64-payload>`. Returns null for anything that is not
 * a well-formed, supported state marker — callers consume it silently and never
 * render a false state. The decoded payload is untrusted/forgeable, so it is
 * control-stripped and length-capped before matching (ADR-0010 threat model).
 */
export function parseActivityMarker(data: string): ActivityMarker | null {
  const parts = data.split(";");
  if (parts.length !== 4) return null;
  const [token, ver, type, b64] = parts;
  if (token !== TOKEN) return null;
  if (ver !== SUPPORTED_VERSION) return null;
  if (type !== MARKER_TYPE) return null;
  let decoded: string;
  try {
    decoded = decodeBase64Utf8(b64);
  } catch {
    return null;
  }
  const clean = capWithEllipsis(
    stripTerminalEscapes(decoded, { keepWhitespace: false }),
    PAYLOAD_CAP,
  );
  const state = STATE_BY_TOKEN[clean];
  return state ? { state } : null;
}
