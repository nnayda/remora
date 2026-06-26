import type { ConfigDto, SessionListDto, SessionMetaDto } from "./bindings";
import { tabKey } from "./session-store";

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

/** How long a host's last-good rows are retained while it is continuously
 * unavailable, before they are pruned. Partial-failure only — all-hosts-down is
 * the discoveryUnavailable banner's job. */
export const RECONNECT_GRACE_MS = 15_000;

export interface DiscoveryDeps {
  loadConfig: () => Promise<ConfigDto>;
  listSessions: () => Promise<SessionListDto>;
  /** Poll period while active. Defaults to 4s. */
  intervalMs?: number;
  /** Wall clock for the grace window. Defaults to Date.now (injectable for tests). */
  now?: () => number;
}

export interface DiscoverySnapshot {
  config: ConfigDto;
  sessions: SessionMetaDto[];
  /** Message when the config file exists but could not be read/parsed; else null. */
  configError: string | null;
  /** True when the last session poll failed (last good list is retained). */
  discoveryUnavailable: boolean;
  /** tabKey(projectId, sessionId) of every session on a host currently
   * unavailable (retained, shown dimmed/reconnecting). */
  reconnectingKeys: string[];
}

const EMPTY_CONFIG: ConfigDto = { hosts: [], projects: [], agents: [] };

/** Best-effort human message from an unknown thrown config-load error. */
function errorMessage(e: unknown): string {
  if (typeof e === "object" && e !== null && "message" in e) {
    return String((e as { message: unknown }).message);
  }
  return "Could not read config.";
}

export class DiscoveryStore {
  private readonly intervalMs: number;
  private readonly now: () => number;
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
    reconnectingKeys: [],
  };

  // host id → its last-good rows + when it started failing (null ⇒ reachable)
  // + when it was last seen available (used to seed the grace window on first
  // unavailable poll, so the window measures from the last good contact, not
  // from the moment the down-poll arrived).
  private byHost = new Map<
    string,
    {
      sessions: SessionMetaDto[];
      unavailableSince: number | null;
      lastSeenAt: number;
    }
  >();

  /** Resolve the poll interval, clamping a 0/negative override to the default. */
  constructor(private readonly deps: DiscoveryDeps) {
    // Guard against a 0/negative override turning the poll into a tight loop
    // that hammers listSessions(); fall back to the 4s default.
    const intervalMs = deps.intervalMs ?? 4000;
    this.intervalMs = intervalMs > 0 ? intervalMs : 4000;
    this.now = deps.now ?? Date.now;
  }

  /** `useSyncExternalStore` subscribe: register a listener, returns unsubscribe. */
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  /** `useSyncExternalStore` getSnapshot: the current immutable snapshot. */
  getSnapshot = (): DiscoverySnapshot => this.snapshot;

  /** Apply a snapshot patch (fresh object) and notify subscribers. */
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
      // Resume from hidden: restart the grace window for any still-failing host
      // so a long hidden gap doesn't prune-then-reappear (the #159 flicker).
      const t = this.now();
      for (const entry of this.byHost.values()) {
        if (entry.unavailableSince !== null) entry.unavailableSince = t;
      }
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

  /** Arm the poll interval (idempotent: a second call is a no-op). */
  private startTimer(): void {
    if (this.timer !== null) return;
    this.timer = setInterval(() => {
      void this.pollSessions();
    }, this.intervalMs);
  }

  /** Clear the poll interval if armed. */
  private stopTimer(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Re-read the config file; a failure surfaces a banner without blanking the
   * last good config (discovered sessions keep rendering). */
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

  /** Poll the session list once, guarding against overlapping in-flight polls
   * and retaining the last good list when discovery is momentarily down. */
  private async pollSessions(): Promise<void> {
    // In-flight guard: a slow transport's list() must not pile up behind itself
    // (real ssh discovery is 1+N+M round-trips). Skip ticks while one is open.
    if (this.fetching) return;
    this.fetching = true;
    try {
      const result = await this.deps.listSessions();
      if (this.disposed) return;
      this.mergeHosts(result);
      this.commit({
        sessions: this.flattenSessions(),
        reconnectingKeys: this.computeReconnectingKeys(),
        discoveryUnavailable: false,
      });
    } catch {
      if (this.disposed) return;
      // All hosts down (or transport error): keep last good, flag the banner.
      // Per-host timers are intentionally left running.
      this.commit({ discoveryUnavailable: true });
    } finally {
      this.fetching = false;
    }
  }

  /** Fold a host-grouped poll into the retention map. */
  private mergeHosts(result: SessionListDto): void {
    const seen = new Set<string>();
    for (const host of result.hosts) {
      seen.add(host.hostId);
      if (host.available) {
        // Authoritative — including an empty list (reachable-but-empty clears).
        this.byHost.set(host.hostId, {
          sessions: host.sessions,
          unavailableSince: null,
          lastSeenAt: this.now(),
        });
        continue;
      }
      const prior = this.byHost.get(host.hostId);
      // Nothing worth retaining ⇒ don't arm a timer or mark reconnecting.
      if (!prior || prior.sessions.length === 0) {
        this.byHost.delete(host.hostId);
        continue;
      }
      // Use when we first noted this host going down; if this is the first
      // unavailable poll, anchor from the last successful contact so the window
      // measures total downtime (including the time the app was polling).
      const since = prior.unavailableSince ?? prior.lastSeenAt;
      if (this.now() - since > RECONNECT_GRACE_MS) {
        this.byHost.delete(host.hostId); // prune after grace
      } else {
        this.byHost.set(host.hostId, {
          sessions: prior.sessions,
          unavailableSince: since,
          lastSeenAt: prior.lastSeenAt,
        });
      }
    }
    // Reconcile: a host absent from this poll (removed from config) must not
    // linger as ghost rows on the app-scoped singleton.
    for (const hostId of [...this.byHost.keys()]) {
      if (!seen.has(hostId)) this.byHost.delete(hostId);
    }
  }

  /** All retained sessions, sorted by (projectId, sessionId) for a stable tree. */
  private flattenSessions(): SessionMetaDto[] {
    const all: SessionMetaDto[] = [];
    for (const { sessions } of this.byHost.values()) all.push(...sessions);
    all.sort((a, b) =>
      a.projectId === b.projectId
        ? a.sessionId.localeCompare(b.sessionId)
        : a.projectId.localeCompare(b.projectId),
    );
    return all;
  }

  /** tabKeys of sessions on a currently-unavailable (retained) host. */
  private computeReconnectingKeys(): string[] {
    const keys: string[] = [];
    for (const { sessions, unavailableSince } of this.byHost.values()) {
      if (unavailableSince === null) continue;
      for (const s of sessions) keys.push(tabKey(s.projectId, s.sessionId));
    }
    return keys;
  }
}
