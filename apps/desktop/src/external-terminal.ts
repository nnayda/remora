import type {
  BridgeError,
  DetectedTerminalDto,
  Result,
  TerminalPreferenceDto,
} from "./bindings";

/** ConfigDto.terminal as bindings project it: registry id, custom argv, or unset. */
export type TerminalPreference = TerminalPreferenceDto | null;

const GENERIC_LABEL = "Open in external terminal";

/** Menu label for the primary open action (spec §4): the configured registry
 * terminal's display name; the single detected terminal when nothing is
 * configured (mirrors the shell resolver's auto-pick); the generic label for
 * custom argv, an unknown/uninstalled id, or genuine ambiguity. */
export function externalTerminalLabel(
  pref: TerminalPreference,
  detected: DetectedTerminalDto[],
): string {
  const named = (id: string) => {
    const hit = detected.find((t) => t.id === id);
    return hit ? `Open in ${hit.name}` : GENERIC_LABEL;
  };
  if (typeof pref === "string") return named(pref);
  if (Array.isArray(pref)) return GENERIC_LABEL;
  if (detected.length === 1) return named(detected[0].id);
  return GENERIC_LABEL;
}

/** Both external actions gate on a live session; a dead session would only
 * show tmux's "no such session" in the spawned terminal. */
export function canOpenExternal(state: "live" | "stopped"): boolean {
  return state === "live";
}

type OpenCommand = (
  projectId: string,
  sessionId: string,
  terminalId: string | null,
) => Promise<Result<null, BridgeError>>;
type CopyCommand = (
  projectId: string,
  sessionId: string,
) => Promise<Result<null, BridgeError>>;

const messageOf = (e: BridgeError): string =>
  "message" in e && typeof e.message === "string" ? e.message : "unknown error";

/** Launch flow: success is silent (the terminal window IS the feedback);
 * terminalNotConfigured deep-links Settings; anything else lands in the
 * app-level notice (the onStop precedent in App.tsx). */
export async function runOpenExternal(
  deps: {
    open: OpenCommand;
    onNotConfigured: () => void;
    onError: (message: string) => void;
  },
  projectId: string,
  sessionId: string,
): Promise<void> {
  const result = await deps.open(projectId, sessionId, null);
  if (result.status === "ok") return;
  if (result.error.kind === "terminalNotConfigured") deps.onNotConfigured();
  else deps.onError(messageOf(result.error));
}

/** Copy flow: the string never reaches the frontend — Rust writes the
 * clipboard (redaction boundary); we only surface failures. */
export async function runCopyAttach(
  deps: { copy: CopyCommand; onError: (message: string) => void },
  projectId: string,
  sessionId: string,
): Promise<void> {
  const result = await deps.copy(projectId, sessionId);
  if (result.status === "error") deps.onError(messageOf(result.error));
}
