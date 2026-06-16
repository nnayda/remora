import type { SessionConnection } from "./connection";

/** Identity + dedupe key for a tab. */
export function tabKey(projectId: string, sessionId: string): string {
  return `${projectId}/${sessionId}`;
}

export interface SpawnInput {
  projectId: string;
  sessionId: string;
  agent: string | null;
}

export interface Tab {
  key: string;
  projectId: string;
  sessionId: string;
  agent: string | null;
  connection: SessionConnection;
  /** True if the open attached an existing session instead of spawning. */
  attached: boolean;
}

export interface Snapshot {
  tabs: Tab[];
  activeKey: string | null;
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

/**
 * App-scoped owner of the open tabs and their connections. Plain class (no
 * React) so it is node-testable and survives React remounts. Exposes a
 * `subscribe`/`getSnapshot` pair for `useSyncExternalStore`.
 *
 *   openSession ─▶ [in-flight] ─resolve─▶ commit tab (unless cancelled/disposed)
 *   closeTab    ─▶ cancel-if-pending  OR  close connection + drop tab + refocus
 *   dispose     ─▶ cancel all pending + close all connections (app teardown)
 */
export class SessionStore {
  private tabs: Tab[] = [];
  private activeKey: string | null = null;
  private pending = new Map<string, { cancelled: boolean }>();
  private disposed = false;
  private listeners = new Set<() => void>();
  private snapshot: Snapshot = { tabs: [], activeKey: null };

  constructor(private readonly open: OpenSession) {}

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): Snapshot => this.snapshot;

  private commit(): void {
    // New snapshot object every commit so useSyncExternalStore re-renders;
    // unchanged between commits so it does not loop.
    this.snapshot = { tabs: this.tabs, activeKey: this.activeKey };
    for (const listener of this.listeners) listener();
  }

  openSession = async (input: SpawnInput): Promise<OpenResult> => {
    const key = tabKey(input.projectId, input.sessionId);
    const existing = this.tabs.find((t) => t.key === key);
    if (existing) {
      this.activeKey = key;
      this.commit();
      return { ok: true, attached: existing.attached };
    }

    const token = { cancelled: false };
    this.pending.set(key, token);
    let opened: { connection: SessionConnection; attached: boolean };
    try {
      opened = await this.open(input.projectId, input.sessionId, input.agent);
    } catch (error) {
      this.pending.delete(key);
      return { ok: false, error };
    }
    this.pending.delete(key);

    if (token.cancelled || this.disposed) {
      void opened.connection.close().catch(() => {});
      return { ok: false, error: OPEN_CANCELLED };
    }

    this.tabs = [
      ...this.tabs,
      {
        key,
        projectId: input.projectId,
        sessionId: input.sessionId,
        agent: input.agent,
        connection: opened.connection,
        attached: opened.attached,
      },
    ];
    this.activeKey = key;
    this.commit();
    return { ok: true, attached: opened.attached };
  };

  closeTab = (key: string): void => {
    const pendingToken = this.pending.get(key);
    if (pendingToken) {
      // In-flight: mark cancelled; the resolve handler closes the orphan.
      pendingToken.cancelled = true;
      return;
    }
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

  dispose = (): void => {
    this.disposed = true;
    for (const token of this.pending.values()) token.cancelled = true;
    this.pending.clear();
    for (const tab of this.tabs) void tab.connection.close().catch(() => {});
    this.tabs = [];
    this.activeKey = null;
    this.commit();
  };
}
