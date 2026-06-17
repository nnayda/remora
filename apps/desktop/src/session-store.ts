import type { SessionConnection } from "./connection";
import { errorMessage, reconnectFate } from "./connection";

/** Identity + dedupe key for a tab. */
export function tabKey(projectId: string, sessionId: string): string {
  return `${projectId}/${sessionId}`;
}

export interface SpawnInput {
  projectId: string;
  sessionId: string;
  agent: string | null;
}

export type TabStatus = "live" | "reconnecting" | "stopped" | "disconnected";

export interface Tab {
  key: string;
  projectId: string;
  sessionId: string;
  agent: string | null;
  connection: SessionConnection;
  /** True if the open attached an existing session instead of spawning. */
  attached: boolean;
  status: TabStatus;
  error: string | null;
}

export interface Snapshot {
  tabs: Tab[];
  activeKey: string | null;
}

export interface StoreOpeners {
  spawn(
    projectId: string,
    sessionId: string,
    agent: string | null,
  ): Promise<{ connection: SessionConnection; attached: boolean }>;
  attach(projectId: string, sessionId: string): Promise<SessionConnection>;
  respawn(
    projectId: string,
    sessionId: string,
    agent: string | null,
  ): Promise<SessionConnection>;
  /** Injected timer so tests drive backoff deterministically. */
  schedule(fn: () => void, ms: number): unknown;
}

/** The opener the store depends on; the real impl is `connection.openSession`. */
export type OpenSession = (
  projectId: string,
  sessionId: string,
  agent: string | null,
) => Promise<{ connection: SessionConnection; attached: boolean }>;

/** Returned by `openSession` when the open was cancelled (closed/disposed
 * mid-connect). The connection is closed by the store; callers ignore it. */
export const OPEN_CANCELLED = Symbol("open-cancelled");

export type OpenResult =
  | { ok: true; attached: boolean }
  | { ok: false; error: unknown };

const BACKOFF_MS = [1000, 2000, 4000, 8000] as const;

/**
 * App-scoped owner of the open tabs and their connections. Plain class (no
 * React) so it is node-testable and survives React remounts. Exposes a
 * `subscribe`/`getSnapshot` pair for `useSyncExternalStore`.
 *
 *   openSession ─▶ [in-flight] ─resolve─▶ commit tab (unless cancelled/disposed)
 *   closeTab    ─▶ cancel-if-pending  OR  close connection + drop tab + refocus
 *   dispose     ─▶ cancel all pending + close all connections (app teardown)
 *   onDeath     ─▶ reconnecting → attach-only retry with backoff + error classify
 */
export class SessionStore {
  private tabs: Tab[] = [];
  private activeKey: string | null = null;
  private pending = new Map<string, { cancelled: boolean }>();
  // One token per tab key; bumped to cancel an in-flight reconnect.
  private reconnectTokens = new Map<
    string,
    { cancelled: boolean; attempt: number }
  >();
  private disposed = false;
  private listeners = new Set<() => void>();
  private snapshot: Snapshot = { tabs: [], activeKey: null };

  constructor(private readonly openers: StoreOpeners) {}

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): Snapshot => this.snapshot;

  private commit(): void {
    // New snapshot object every commit so useSyncExternalStore re-renders;
    // unchanged between commits so it does not loop. Each tab is a fresh
    // shallow copy so a consumer can't mutate the store's live state — the
    // `connection` ref is intentionally shared (a live handle, not data).
    this.snapshot = {
      tabs: this.tabs.map((t) => ({ ...t })),
      activeKey: this.activeKey,
    };
    for (const listener of this.listeners) listener();
  }

  private setStatus(
    key: string,
    status: TabStatus,
    error: string | null,
  ): void {
    this.tabs = this.tabs.map((t) =>
      t.key === key ? { ...t, status, error } : t,
    );
    this.commit();
  }

  private registerDeath(tab: Tab): void {
    tab.connection.onClose(() => this.onDeath(tab.key));
  }

  private onDeath(key: string): void {
    const tab = this.tabs.find((t) => t.key === key);
    if (!tab || tab.status === "stopped" || tab.status === "disconnected")
      return;
    this.setStatus(key, "reconnecting", null);
    void this.startReconnect(key, 0);
  }

  private newReconnectToken(key: string): {
    cancelled: boolean;
    attempt: number;
  } {
    const prev = this.reconnectTokens.get(key);
    if (prev) prev.cancelled = true; // cancel any older loop
    const token = { cancelled: false, attempt: 0 };
    this.reconnectTokens.set(key, token);
    return token;
  }

  /** Attach-only reconnect with classification + capped backoff. */
  private async startReconnect(key: string, attempt: number): Promise<void> {
    const token =
      attempt === 0
        ? this.newReconnectToken(key)
        : this.reconnectTokens.get(key);
    if (!token || token.cancelled || this.disposed) return;
    const tab = this.tabs.find((t) => t.key === key);
    if (!tab) return;
    let next: SessionConnection;
    try {
      next = await this.openers.attach(tab.projectId, tab.sessionId);
    } catch (e) {
      if (token.cancelled || this.disposed) return;
      const fate = reconnectFate(e);
      if (fate === "stopped") return this.setStatus(key, "stopped", null);
      if (fate === "terminal")
        return this.setStatus(key, "disconnected", errorMessage(e));
      // retry: schedule next attempt with capped backoff
      const ms = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
      this.openers.schedule(
        () => void this.startReconnect(key, attempt + 1),
        ms,
      );
      return;
    }
    if (token.cancelled || this.disposed) {
      void next.close().catch(() => {});
      return;
    }
    this.swapConnection(key, next, "live");
  }

  /** Replace a tab's connection (close the old), set status, re-arm death. */
  private swapConnection(
    key: string,
    next: SessionConnection,
    status: TabStatus,
  ): void {
    const idx = this.tabs.findIndex((t) => t.key === key);
    if (idx === -1) {
      void next.close().catch(() => {});
      return;
    }
    const old = this.tabs[idx].connection;
    void old.close().catch(() => {}); // N2: closing reaps the old ssh child
    const swapped: Tab = {
      ...this.tabs[idx],
      connection: next,
      status,
      error: null,
    };
    this.tabs = this.tabs.map((t) => (t.key === key ? swapped : t));
    this.registerDeath(swapped);
    this.commit();
  }

  openSession = async (input: SpawnInput): Promise<OpenResult> => {
    // After dispose() the store is dead — never start new work. The opener
    // spawns/attaches a real session, a side effect we must not perform
    // post-teardown (the post-await guard below only cancels locally).
    if (this.disposed) {
      return { ok: false, error: OPEN_CANCELLED };
    }
    const key = tabKey(input.projectId, input.sessionId);
    const existing = this.tabs.find((t) => t.key === key);
    if (existing) {
      this.activeKey = key;
      this.commit();
      return { ok: true, attached: existing.attached };
    }

    // A second open of a key whose open is still in flight would overwrite
    // the pending token and could commit a duplicate tab. The dialog prevents
    // it (submit disabled while connecting), but the store guards it too.
    if (this.pending.has(key)) {
      return { ok: false, error: OPEN_CANCELLED };
    }
    const token = { cancelled: false };
    this.pending.set(key, token);
    let opened: { connection: SessionConnection; attached: boolean };
    try {
      opened = await this.openers.spawn(
        input.projectId,
        input.sessionId,
        input.agent,
      );
    } catch (error) {
      this.pending.delete(key);
      return { ok: false, error };
    }
    this.pending.delete(key);

    if (token.cancelled || this.disposed) {
      void opened.connection.close().catch(() => {});
      return { ok: false, error: OPEN_CANCELLED };
    }

    const tab: Tab = {
      key,
      projectId: input.projectId,
      sessionId: input.sessionId,
      agent: input.agent,
      connection: opened.connection,
      attached: opened.attached,
      status: "live",
      error: null,
    };
    this.tabs = [...this.tabs, tab];
    this.activeKey = key;
    this.commit();
    this.registerDeath(tab);
    return { ok: true, attached: opened.attached };
  };

  closeTab = (key: string): void => {
    const pendingToken = this.pending.get(key);
    if (pendingToken) {
      // In-flight: mark cancelled; the resolve handler closes the orphan.
      pendingToken.cancelled = true;
      return;
    }
    // Cancel any in-flight reconnect and reap the token (tab is being removed).
    const reconnectToken = this.reconnectTokens.get(key);
    if (reconnectToken) reconnectToken.cancelled = true;
    this.reconnectTokens.delete(key);
    const idx = this.tabs.findIndex((t) => t.key === key);
    if (idx === -1) return;
    void this.tabs[idx].connection.close().catch(() => {});
    const next = this.tabs.filter((t) => t.key !== key);
    if (this.activeKey === key) {
      const neighbour = next[idx] ?? next[idx - 1] ?? null;
      this.activeKey = neighbour ? neighbour.key : null;
    }
    this.tabs = next;
    this.commit();
  };

  focusTab = (key: string): void => {
    if (this.activeKey === key) return;
    if (!this.tabs.some((t) => t.key === key)) return;
    this.activeKey = key;
    this.commit();
  };

  respawnTab = async (key: string): Promise<void> => {
    const tab = this.tabs.find((t) => t.key === key);
    if (!tab || this.disposed) return;
    const token = this.newReconnectToken(key); // cancel any reconnect loop
    this.setStatus(key, "reconnecting", null);
    let next: SessionConnection;
    try {
      next = await this.openers.respawn(
        tab.projectId,
        tab.sessionId,
        tab.agent,
      );
    } catch (e) {
      if (token.cancelled || this.disposed) return;
      this.setStatus(key, "disconnected", errorMessage(e));
      return;
    }
    if (token.cancelled || this.disposed) {
      void next.close().catch(() => {});
      return;
    }
    this.swapConnection(key, next, "live");
  };

  dispose = (): void => {
    this.disposed = true;
    for (const token of this.pending.values()) token.cancelled = true;
    this.pending.clear();
    // Cancel all reconnect tokens
    for (const token of this.reconnectTokens.values()) token.cancelled = true;
    this.reconnectTokens.clear();
    for (const tab of this.tabs) void tab.connection.close().catch(() => {});
    this.tabs = [];
    this.activeKey = null;
    this.commit();
  };
}
