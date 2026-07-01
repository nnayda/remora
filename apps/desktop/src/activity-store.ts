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
  noteMarkerSeen(key: string): void;
  clear(key: string): void;
}

/**
 * Passive per-session activity store. Each attached TerminalController forwards
 * the `statusChange` / `previewUpdate` events it receives on its session channel;
 * the store records the latest value per key and notifies React subscribers
 * (via useSyncExternalStore) only on an actual change.
 *
 *   setStatus(key, s)  ─▶ status map (+ expire stale preview) ─▶ snapshot ─▶ UI
 *   setPreview(key, t) ─▶ preview map ─▶ preview snapshot ─▶ UI (own commit; never churns the status snapshot)
 *   clear(key)         ─▶ drop both   (snapshots updated independently; at most one notification)
 */
export class ActivityStore implements ActivitySink {
  private readonly status = new Map<string, ActivityState>();
  private readonly previews = new Map<string, string>();
  private readonly seen = new Set<string>();
  private snapshot: ReadonlyMap<string, ActivityState> = new Map();
  private previewSnapshot: ReadonlyMap<string, string> = new Map();
  private seenSnapshot: ReadonlySet<string> = new Set();
  private readonly listeners = new Set<() => void>();

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): ReadonlyMap<string, ActivityState> => this.snapshot;

  getPreviewSnapshot = (): ReadonlyMap<string, string> => this.previewSnapshot;

  getMarkerSeenSnapshot = (): ReadonlySet<string> => this.seenSnapshot;

  getPreview = (key: string): string | undefined => this.previews.get(key);

  setStatus(key: string, status: ActivityState): void {
    if (this.status.get(key) === status) return; // no churn on unchanged status
    this.status.set(key, status);
    // A preview belongs to the status episode it arrived in. A status change
    // makes any prior preview stale, so drop it — otherwise re-entering
    // `awaiting` without a fresh preview would re-show the previous prompt as
    // agent-claimed text. The detector emits Status before Preview within one
    // marker (ADR-0013), so a marker that does carry a preview re-sets it on the
    // very next call; this only expires orphaned text.
    const hadPreview = this.previews.delete(key);
    this.snapshot = new Map(this.status);
    if (hadPreview) this.previewSnapshot = new Map(this.previews);
    this.notify();
  }

  setPreview(key: string, preview: string): void {
    if (this.previews.get(key) === preview) return; // no churn on unchanged text
    this.previews.set(key, preview);
    this.commitPreviews();
  }

  noteMarkerSeen(key: string): void {
    if (this.seen.has(key)) return; // one-shot per key; no churn
    this.seen.add(key);
    this.seenSnapshot = new Set(this.seen);
    this.notify();
  }

  clear(key: string): void {
    const hadStatus = this.status.delete(key);
    const hadPreview = this.previews.delete(key);
    const hadSeen = this.seen.delete(key);
    if (hadStatus) this.snapshot = new Map(this.status);
    if (hadPreview) this.previewSnapshot = new Map(this.previews);
    if (hadSeen) this.seenSnapshot = new Set(this.seen);
    if (hadStatus || hadPreview || hadSeen) this.notify();
  }

  private notify(): void {
    for (const listener of this.listeners) listener();
  }

  private commitPreviews(): void {
    this.previewSnapshot = new Map(this.previews);
    this.notify();
  }
}
