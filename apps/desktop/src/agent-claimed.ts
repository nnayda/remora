import type { ActivityState } from "./activity-store";

/** Compose the tooltip text for a session row. A live agent preview is rendered
 * as *sandbox-claimed* — the byte stream is untrusted, so we never present it as
 * authoritative chrome (ADR-0010/0018 threat model). Falls back to a caller-
 * supplied title (e.g. the stopped-state hint) when there is no preview. */
export function rowTitle({
  preview,
  fallback,
}: {
  preview?: string;
  fallback?: string;
}): string | undefined {
  if (preview) return `the session says: ${preview}`;
  return fallback;
}

/** Gate a stored preview on the session being actively awaiting input.
 * After the user answers and the agent resumes (working/idle), the stored
 * preview would show a stale already-answered question — suppress it so the
 * tooltip never shows a question that has already been answered. Returns
 * `preview` only when `activity === "awaiting"`, otherwise undefined. */
export function previewWhenAwaiting(
  activity: ActivityState | undefined,
  preview: string | undefined,
): string | undefined {
  if (activity !== "awaiting") return undefined;
  return preview;
}
