import type { MarkerState } from "./osc-marker";

/** Rendered activity for a session. `idle` = output paused (blue, NOT "done");
 * `awaiting` = needs-you (red), marker-only; `unknown` = attached, no signal. */
export type ActivityState = "working" | "idle" | "awaiting" | "unknown";

/** Quiet-for-this-long ⇒ settle `working` → `idle`. Conservative: an agent can
 * pause >1s mid-thought, so a short window would mislabel thinking as idle. */
export const SETTLE_WINDOW_MS = 1500;
/** How often the single shared sweep checks for newly-settled sessions. */
export const SWEEP_INTERVAL_MS = 250;

/** The write surface a TerminalController uses; the store implements it. */
export interface ActivitySink {
  noteOutput(key: string): void;
  noteMarker(key: string, marker: MarkerState): void;
  clear(key: string): void;
}

interface Entry {
  lastOutputAt: number;
  state: ActivityState;
}

export interface ActivityDeps {
  /** Injectable clock for deterministic tests. */
  now?: () => number;
}

/**
 * Client-side per-session activity, driven by each attached TerminalController.
 *
 *   noteOutput ─▶ working (stamp lastOutputAt)        ┐
 *   noteMarker(working) ─▶ working                    ├▶ snapshot Map ─▶ UI
 *   noteMarker(idle|awaiting) ─▶ settle NOW to state  │   (commit only on
 *   sweep (one ~250ms tick) ─▶ working→idle past W    ┘    state change)
 *
 * Marker precedence: an idle/awaiting marker means "I am now in this state", so
 * it settles immediately (pre-dating lastOutputAt past the window) — no working
 * flicker from the bytes that carried it. Later real output supersedes it.
 */
export class ActivityStore implements ActivitySink {
  private readonly now: () => number;
  private readonly entries = new Map<string, Entry>();
  private snapshot: ReadonlyMap<string, ActivityState> = new Map();
  private readonly listeners = new Set<() => void>();
  private interval: ReturnType<typeof setInterval> | null = null;

  constructor(deps: ActivityDeps = {}) {
    this.now = deps.now ?? (() => Date.now());
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): ReadonlyMap<string, ActivityState> => this.snapshot;

  noteOutput(key: string): void {
    const e = this.entries.get(key) ?? {
      lastOutputAt: 0,
      state: "unknown" as ActivityState,
    };
    e.lastOutputAt = this.now();
    const transitioned = e.state !== "working";
    e.state = "working";
    this.entries.set(key, e);
    if (transitioned) this.commit(); // skip per-chunk churn while already working
  }

  noteMarker(key: string, marker: MarkerState): void {
    const e = this.entries.get(key) ?? {
      lastOutputAt: 0,
      state: "unknown" as ActivityState,
    };
    const prevState = e.state;
    if (marker === "working") {
      e.lastOutputAt = this.now();
      e.state = "working";
    } else {
      // Settle immediately to the marked state; pre-date so the sweep (which
      // only advances `working`) leaves it. `idle` marker → blue, `awaiting` → red.
      e.lastOutputAt = this.now() - SETTLE_WINDOW_MS - 1;
      e.state = marker === "awaiting" ? "awaiting" : "idle";
    }
    this.entries.set(key, e);
    const transitioned = e.state !== prevState;
    if (transitioned) this.commit(); // skip same-state marker floods
  }

  clear(key: string): void {
    if (this.entries.delete(key)) this.commit();
  }

  /** Advance any `working` session quiet past the window to `idle` (blue). */
  sweep(): void {
    const t = this.now();
    let changed = false;
    for (const e of this.entries.values()) {
      if (e.state === "working" && t - e.lastOutputAt >= SETTLE_WINDOW_MS) {
        e.state = "idle";
        changed = true;
      }
    }
    if (changed) this.commit();
  }

  /** Start the single shared sweep interval (idempotent). */
  start(): void {
    if (this.interval !== null) return;
    this.interval = setInterval(() => this.sweep(), SWEEP_INTERVAL_MS);
  }

  /** Stop the sweep interval. */
  stop(): void {
    if (this.interval !== null) {
      clearInterval(this.interval);
      this.interval = null;
    }
  }

  private commit(): void {
    const next = new Map<string, ActivityState>();
    for (const [k, e] of this.entries) next.set(k, e.state);
    this.snapshot = next;
    for (const listener of this.listeners) listener();
  }
}
