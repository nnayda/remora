/** Rendered activity for a session, sourced from core's status events (ADR-0013).
 * `idle` = output paused (blue, NOT "done"); `awaiting` = needs-you (red),
 * asserted by core via the marker, never inferred; `unknown` = attached, no
 * signal yet. */
export type ActivityState = "working" | "idle" | "awaiting" | "unknown";

/** The write surface a TerminalController uses to record core-emitted events.
 * Detection lives in core now (ADR-0013) — this store only records. */
export interface ActivitySink {
  setStatus(key: string, status: ActivityState): void;
  setPreview(key: string, preview: string): void;
  clear(key: string): void;
}

/**
 * Passive per-session activity store. Each attached TerminalController forwards
 * the `statusChange` / `previewUpdate` events it receives on its session channel;
 * the store records the latest value per key and notifies React subscribers
 * (via useSyncExternalStore) only on an actual change.
 *
 *   setStatus(key, s) ─▶ status map ─┐
 *   setPreview(key, t) ─▶ preview map ├▶ snapshot (status only) ─▶ UI
 *   clear(key)        ─▶ drop both   ┘   (commit only on status change)
 */
export class ActivityStore implements ActivitySink {
  private readonly status = new Map<string, ActivityState>();
  private readonly previews = new Map<string, string>();
  private snapshot: ReadonlyMap<string, ActivityState> = new Map();
  private readonly listeners = new Set<() => void>();

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): ReadonlyMap<string, ActivityState> => this.snapshot;

  getPreview = (key: string): string | undefined => this.previews.get(key);

  setStatus(key: string, status: ActivityState): void {
    if (this.status.get(key) === status) return; // no churn on unchanged status
    this.status.set(key, status);
    this.commit();
  }

  setPreview(key: string, preview: string): void {
    // Preview is recorded for future consumers (#60/#73/#61); it is not part of
    // the status snapshot, so recording it does not notify status subscribers.
    this.previews.set(key, preview);
  }

  clear(key: string): void {
    const had = this.status.delete(key);
    this.previews.delete(key);
    if (had) this.commit();
  }

  private commit(): void {
    this.snapshot = new Map(this.status);
    for (const listener of this.listeners) listener();
  }
}
