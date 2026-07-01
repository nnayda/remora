import type { ActivityState } from "./activity-store";

/** Compose the tooltip text for a session row. A live agent preview renders as
 * *sandbox-claimed* (never authoritative chrome; ADR-0010/0018 threat model).
 * When the session's activity hook is confirmed (`hookActive`, #198), a positive
 * "Activity hook active" line is appended — the honest, false-positive-free
 * surfacing (absence is ambiguous, presence is not; see ADR-0019). Falls back to
 * a caller-supplied title (e.g. the stopped-state hint) when there is no preview. */
export function rowTitle({
  preview,
  fallback,
  hookActive,
}: {
  preview?: string;
  fallback?: string;
  hookActive?: boolean;
}): string | undefined {
  const base = preview ? `the session says: ${preview}` : fallback;
  if (!hookActive) return base;
  const affirmation = "Activity hook active";
  return base ? `${base}\n${affirmation}` : affirmation;
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
