import type { ConfigDto, SessionMetaDto } from "./bindings";

/**
 * App-scoped owner of the sidebar's live discovery state: the per-device config
 * (read once, re-read on manual refresh) and the polled session list. A plain
 * class (no React) so the poll cadence, in-flight guard, pause-while-hidden, and
 * error handling are node-testable with fake timers — the thin `useDiscovery`
 * hook only wraps `subscribe`/`getSnapshot` and wires DOM visibility/focus
 * events to `setActive`/`refresh` (stage-9 `SessionStore` precedent).
 *
 *   start ─▶ load config + poll sessions ─▶ interval(poll) while active
 *   setActive(false) ─▶ pause interval     setActive(true) ─▶ resume + poll now
 *   refresh ─▶ reload config + poll         refreshAfterOpen ─▶ poll only
 *   stop ─▶ dispose: clear interval + ignore any in-flight result
 */

export interface DiscoveryDeps {
  loadConfig: () => Promise<ConfigDto>;
  listSessions: () => Promise<SessionMetaDto[]>;
  /** Poll period while active. Defaults to 4s. */
  intervalMs?: number;
}

export interface DiscoverySnapshot {
  config: ConfigDto;
  sessions: SessionMetaDto[];
  /** Message when the config file exists but could not be read/parsed; else null. */
  configError: string | null;
  /** True when the last session poll failed (last good list is retained). */
  discoveryUnavailable: boolean;
}

const EMPTY_CONFIG: ConfigDto = { hosts: [], projects: [] };

function errorMessage(e: unknown): string {
  if (typeof e === "object" && e !== null && "message" in e) {
    return String((e as { message: unknown }).message);
  }
  return "Could not read config.";
}

export class DiscoveryStore {
  private readonly intervalMs: number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private fetching = false;
  private active = true;
  private disposed = false;
  private startedOnce = false;
  private listeners = new Set<() => void>();
  private snapshot: DiscoverySnapshot = {
    config: EMPTY_CONFIG,
    sessions: [],
    configError: null,
    discoveryUnavailable: false,
  };

  constructor(private readonly deps: DiscoveryDeps) {
    // Guard against a 0/negative override turning the poll into a tight loop
    // that hammers listSessions(); fall back to the 4s default.
    const intervalMs = deps.intervalMs ?? 4000;
    this.intervalMs = intervalMs > 0 ? intervalMs : 4000;
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): DiscoverySnapshot => this.snapshot;

  private commit(patch: Partial<DiscoverySnapshot>): void {
    // New object every commit so useSyncExternalStore re-renders; unchanged
    // between commits so it does not loop.
    this.snapshot = { ...this.snapshot, ...patch };
    for (const listener of this.listeners) listener();
  }

  /**
   * Initial load: config + one session poll, then begin polling while active.
   * Idempotent — a StrictMode/HMR remount of the hook must not re-trigger it.
   */
  start = async (): Promise<void> => {
    if (this.startedOnce || this.disposed) return;
    this.startedOnce = true;
    await this.loadConfig();
    await this.pollSessions();
    if (!this.disposed && this.active) this.startTimer();
  };

  /** Pause (hidden) or resume (visible/focused) polling. Resuming refreshes now. */
  setActive = (active: boolean): void => {
    if (this.disposed || active === this.active) return;
    this.active = active;
    if (active) {
      void this.pollSessions();
      this.startTimer();
    } else {
      this.stopTimer();
    }
  };

  /** Manual refresh: re-read the config file AND the session list. */
  refresh = async (): Promise<void> => {
    await this.loadConfig();
    await this.pollSessions();
  };

  /** After a spawn changed server state: re-list sessions only (config unchanged). */
  refreshAfterOpen = async (): Promise<void> => {
    await this.pollSessions();
  };

  /**
   * Dispose: stop polling and ignore any in-flight result (no late commit).
   * Terminal — a stopped store never restarts (`start()` early-returns once
   * disposed). The app-scoped singleton is intentionally never stopped (see
   * useDiscovery.ts); this exists for tests and a future explicit teardown.
   */
  stop = (): void => {
    this.disposed = true;
    this.stopTimer();
  };

  private startTimer(): void {
    if (this.timer !== null) return;
    this.timer = setInterval(() => {
      void this.pollSessions();
    }, this.intervalMs);
  }

  private stopTimer(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  private async loadConfig(): Promise<void> {
    try {
      const config = await this.deps.loadConfig();
      if (this.disposed) return;
      this.commit({ config, configError: null });
    } catch (e) {
      if (this.disposed) return;
      // A bad config must not blank the sidebar: keep showing discovered
      // sessions (which join under "Unconfigured") and surface a banner.
      this.commit({ configError: errorMessage(e) });
    }
  }

  private async pollSessions(): Promise<void> {
    // In-flight guard: a slow transport's list() must not pile up behind itself
    // (real ssh discovery is 1+N+M round-trips). Skip ticks while one is open.
    if (this.fetching) return;
    this.fetching = true;
    try {
      const sessions = await this.deps.listSessions();
      if (this.disposed) return;
      this.commit({ sessions, discoveryUnavailable: false });
    } catch {
      if (this.disposed) return;
      // Keep the last good list; just flag that discovery is currently down.
      this.commit({ discoveryUnavailable: true });
    } finally {
      this.fetching = false;
    }
  }
}
