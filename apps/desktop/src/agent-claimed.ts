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
