import type { DirtyReasonDto, WorkspaceModeDto } from "./bindings";
import type { SessionConnection } from "./connection";
import { errorMessage, isSessionNotFound, reconnectFate } from "./connection";

export type TeardownResult = { ok: true } | { ok: false; error?: unknown };
export type RemoveResult =
  | { ok: true }
  | { ok: false; dirty?: DirtyReasonDto; error?: unknown };

function isWorkspaceDirty(
  e: unknown,
): e is { kind: "workspaceDirty"; reason: DirtyReasonDto } {
  return (
    typeof e === "object" &&
    e !== null &&
    (e as { kind?: string }).kind === "workspaceDirty"
  );
}

/** Identity + dedupe key for a tab. */
export function tabKey(projectId: string, sessionId: string): string {
  return `${projectId}/${sessionId}`;
}

export interface SpawnInput {
  projectId: string;
  sessionId: string;
  agent: string | null;
  base: string | null;
  workspace: WorkspaceModeDto;
  branch?: string | null;
  worktreeRoot?: string | null;
}

export type TabStatus = "live" | "reconnecting" | "stopped" | "disconnected";

export interface Tab {
  key: string;
  projectId: string;
  sessionId: string;
  agent: string | null;
  workspace: WorkspaceModeDto;
  connection: SessionConnection;
  /** True if the open attached an existing session instead of spawning. */
  attached: boolean;
  status: TabStatus;
  error: string | null;
}

export interface Snapshot {
  tabs: Tab[];
  activeKey: string | null;
  /** Keys whose open (spawn/attach/respawn) is in flight but not yet committed
   * as a tab. Drives the sidebar row's connecting spinner (#170): a fresh open
   * has no tab to carry a status until it resolves, so this transient set is the
   * only signal the click registered. Mirrors the `pending` open guard. */
  connecting: string[];
}

export interface StoreOpeners {
  spawn(
    projectId: string,
    sessionId: string,
    agent: string | null,
    base: string | null,
    workspace: WorkspaceModeDto,
    branch: string | null,
    worktreeRoot: string | null,
  ): Promise<{ connection: SessionConnection; attached: boolean }>;
  attach(projectId: string, sessionId: string): Promise<SessionConnection>;
  respawn(
    projectId: string,
    sessionId: string,
    agent: string | null,
  ): Promise<SessionConnection>;
  /** Injected timer so tests drive backoff deterministically. */
  schedule(fn: () => void, ms: number): unknown;
  stop(projectId: string, sessionId: string): Promise<void>;
  remove(projectId: string, sessionId: string, force: boolean): Promise<void>;
}

/** Returned by `openSession` when the open was cancelled (closed/disposed
 * mid-connect). The connection is closed by the store; callers ignore it. */
export const OPEN_CANCELLED = Symbol("open-cancelled");

/**
 * `opened` is true only when this call committed a **new** tab (a fresh, live
 * terminal); it is false when the call short-circuited to focus an
 * already-open tab. The UI needs this to decide whether a spawn produced a
 * terminal worth focusing — focusing an existing non-live tab must NOT arm
 * focus, or the armed flag survives to steal focus when the pane later goes
 * live (#133). `attached` distinguishes spawn-vs-attach within a fresh open.
 */
export type OpenResult =
  | { ok: true; attached: boolean; opened: boolean }
  | { ok: false; error: unknown };

const BACKOFF_MS = [1000, 2000, 4000, 8000] as const;

/** Cancellation handle for one reconnect loop. A newer trigger flips the prior
 * token's `cancelled`, so the stale loop bails. Depth is the `attempt` param. */
interface ReconnectToken {
  cancelled: boolean;
}

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
  private reconnectTokens = new Map<string, ReconnectToken>();
  private disposed = false;
  private listeners = new Set<() => void>();
  private snapshot: Snapshot = { tabs: [], activeKey: null, connecting: [] };
  private teardownPending = new Set<string>();
  // Per-key count of in-flight respawns. A respawn is an "open" for
  // mutual-exclusion purposes but, unlike `openTab`, isn't tracked in `pending`
  // (it cancels via the reconnect token, not the pending token — see
  // `respawnTab`). A count (not a Set) keeps the key marked for the whole
  // overlap when two respawns race (newest-wins), so a teardown can't slip in.
  private respawning = new Map<string, number>();

  /** True while any open, respawn, or teardown for `key` is in flight. The
   * single cross-cutting lock: opens use `pending`, respawns `respawning`,
   * teardowns `teardownPending`. Prevents an open racing a teardown into an
   * orphaned session (live tmux with no worktree/branch). */
  private busy(key: string): boolean {
    return (
      this.pending.has(key) ||
      this.respawning.has(key) ||
      this.teardownPending.has(key)
    );
  }

  /** Inject the openers (spawn/attach/respawn + timer) so tests can fake them. */
  constructor(private readonly openers: StoreOpeners) {}

  /** `useSyncExternalStore` subscribe: register a listener, returns unsubscribe. */
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  /** `useSyncExternalStore` getSnapshot: the current immutable snapshot. */
  getSnapshot = (): Snapshot => this.snapshot;

  /** Publish a fresh snapshot (defensively-copied tabs) and notify subscribers. */
  private commit(): void {
    // New snapshot object every commit so useSyncExternalStore re-renders;
    // unchanged between commits so it does not loop. Each tab is a fresh
    // shallow copy so a consumer can't mutate the store's live state — the
    // `connection` ref is intentionally shared (a live handle, not data).
    this.snapshot = {
      tabs: this.tabs.map((t) => ({ ...t })),
      activeKey: this.activeKey,
      // `pending` is exactly the set of in-flight opens (set/cleared only in
      // openTab), so it doubles as the connecting set the UI spins on.
      connecting: [...this.pending.keys()],
    };
    for (const listener of this.listeners) listener();
  }

  /** Set one tab's status + error message and commit. */
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

  /** Arm the connection's death listener to drive this tab into reconnect. */
  private registerDeath(tab: Tab): void {
    tab.connection.onClose(() => this.onDeath(tab.key));
  }

  /** Channel death handler: move a still-recoverable tab into reconnecting and
   * kick off the attach-retry loop (stopped/disconnected tabs are left alone). */
  private onDeath(key: string): void {
    const tab = this.tabs.find((t) => t.key === key);
    if (!tab || tab.status === "stopped" || tab.status === "disconnected")
      return;
    this.setStatus(key, "reconnecting", null);
    void this.startReconnect(key, 0);
  }

  /** Mint a fresh reconnect token for `key`, cancelling any older loop so only
   * the newest one proceeds (see `startReconnect`). */
  private newReconnectToken(key: string): ReconnectToken {
    const prev = this.reconnectTokens.get(key);
    if (prev) prev.cancelled = true; // cancel any older loop
    const token: ReconnectToken = { cancelled: false };
    this.reconnectTokens.set(key, token);
    return token;
  }

  /**
   * Attach-only reconnect with classification + capped backoff.
   *
   * The token is threaded through the whole loop (captured once, passed into the
   * scheduled retry) rather than re-fetched by key each attempt. A concurrent
   * trigger (`reconnectStale`/`reconnectAll`/another `onDeath`) calls
   * `newReconnectToken(key)` during a backoff window, which flips THIS token's
   * `cancelled` and installs a fresh one. The stale scheduled retry then carries
   * the old (now-cancelled) `t` and bails, so only the newest loop proceeds —
   * no double attach / orphaned connection.
   */
  private async startReconnect(
    key: string,
    attempt: number,
    token?: ReconnectToken,
  ): Promise<void> {
    // attempt-0 callers pass no token → fresh; retries pass the captured one.
    const t = token ?? this.newReconnectToken(key);
    if (t.cancelled || this.disposed) return;
    const tab = this.tabs.find((tb) => tb.key === key);
    if (!tab) return;
    let next: SessionConnection;
    try {
      next = await this.openers.attach(tab.projectId, tab.sessionId);
    } catch (e) {
      if (t.cancelled || this.disposed) return;
      const fate = reconnectFate(e);
      if (fate === "stopped") {
        // The session is gone, but the now-dead connection still holds the last
        // bytes it received — surface them as the cause so a fast-exiting agent
        // reads "Session stopped: claude: command not found" instead of a bare
        // message (#28). `tab.connection` is the dead one (not yet swapped).
        return this.setStatus(
          key,
          "stopped",
          tab.connection.lastOutput() || null,
        );
      }
      if (fate === "terminal")
        return this.setStatus(key, "disconnected", errorMessage(e));
      // retry: schedule next attempt with capped backoff, threading the SAME
      // token so a concurrent trigger can cancel this loop.
      const ms = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)];
      this.openers.schedule(
        () => void this.startReconnect(key, attempt + 1, t),
        ms,
      );
      return;
    }
    if (t.cancelled || this.disposed) {
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

  /**
   * Shared open flow for `openSession` and `openViaRespawn` (non-existing path).
   * Callers pass a concrete `open` function that returns `{connection, attached}`.
   * Handles: pending guard → open → post-await cancel/dispose guard → commit tab
   * (status "live", error null) → registerDeath → return {ok:true, attached}.
   */
  private async openTab(
    input: SpawnInput,
    open: (
      p: string,
      s: string,
      a: string | null,
      b: string | null,
      w: WorkspaceModeDto,
      branch: string | null,
      worktreeRoot: string | null,
    ) => Promise<{ connection: SessionConnection; attached: boolean }>,
  ): Promise<OpenResult> {
    const key = tabKey(input.projectId, input.sessionId);

    // A second open of a key whose open is still in flight would overwrite
    // the pending token and could commit a duplicate tab. The dialog prevents
    // it (submit disabled while connecting), but the store guards it too. The
    // wider `busy` check also refuses an open while a respawn or teardown for
    // the same key is in flight (the cross-guard against orphaning).
    if (this.busy(key)) {
      return { ok: false, error: OPEN_CANCELLED };
    }
    const token = { cancelled: false };
    this.pending.set(key, token);
    // Publish the in-flight key now (synchronously, before the await suspends)
    // so the sidebar row spins within a frame of the click (#170).
    this.commit();
    let opened: { connection: SessionConnection; attached: boolean };
    try {
      opened = await open(
        input.projectId,
        input.sessionId,
        input.agent,
        input.base,
        input.workspace,
        input.branch ?? null,
        input.worktreeRoot ?? null,
      );
    } catch (error) {
      this.pending.delete(key);
      this.commit(); // clear the spinner on failure
      return { ok: false, error };
    }
    this.pending.delete(key);

    if (token.cancelled || this.disposed) {
      void opened.connection.close().catch(() => {});
      this.commit(); // clear the spinner on cancel/dispose
      return { ok: false, error: OPEN_CANCELLED };
    }

    const tab: Tab = {
      key,
      projectId: input.projectId,
      sessionId: input.sessionId,
      agent: input.agent,
      workspace: input.workspace,
      connection: opened.connection,
      attached: opened.attached,
      status: "live",
      error: null,
    };
    this.tabs = [...this.tabs, tab];
    this.activeKey = key;
    this.commit();
    this.registerDeath(tab);
    return { ok: true, attached: opened.attached, opened: true };
  }

  /** Open (or focus, if already open) a session via the spawn opener — spawn
   * first, attaching the running one on `sessionExists`. */
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
      // Focused an already-open tab — no NEW tab was committed, so report
      // opened:false (the store doesn't reason about liveness here). The dialog
      // uses this to keep focus on the + button rather than arm focus for a tab
      // it didn't open, which would leak if that tab isn't live (#133).
      return { ok: true, attached: existing.attached, opened: false };
    }

    return this.openTab(input, (p, s, a, b, w, branch, worktreeRoot) =>
      this.openers.spawn(p, s, a, b, w, branch, worktreeRoot),
    );
  };

  /** Close a tab: cancel an in-flight open/reconnect if any, else close the
   * connection, drop the tab, and refocus a neighbour. */
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

  /**
   * Drag-to-reorder: move the `key` tab to the `targetKey` tab's position. The
   * dragged tab lands on the side the drag came from — after the target when
   * moving rightward, before it when moving leftward — so a tab dropped on a
   * neighbour swaps with it. Order is the `tabs` array order, which the
   * app-scoped store holds for the session (survives React remounts). No-op for
   * a self-drop or an unknown key; `activeKey` is untouched (reordering doesn't
   * change which session is focused).
   */
  reorderTab = (key: string, targetKey: string): void => {
    if (key === targetKey) return;
    const from = this.tabs.findIndex((t) => t.key === key);
    const to = this.tabs.findIndex((t) => t.key === targetKey);
    if (from === -1 || to === -1) return;
    const next = [...this.tabs];
    const [moved] = next.splice(from, 1);
    const target = next.findIndex((t) => t.key === targetKey);
    next.splice(from < to ? target + 1 : target, 0, moved);
    this.tabs = next;
    this.commit();
  };

  /** Make `key` the active tab (no-op if already active or unknown). */
  focusTab = (key: string): void => {
    if (this.activeKey === key) return;
    if (!this.tabs.some((t) => t.key === key)) return;
    this.activeKey = key;
    this.commit();
  };

  /** Focus kick: retry every reconnecting tab now (fresh token, attempt 0). */
  reconnectStale = (): void => {
    if (this.disposed) return;
    for (const tab of this.tabs) {
      if (tab.status === "reconnecting") void this.startReconnect(tab.key, 0);
    }
  };

  /** Wake recovery (D2): re-attach every live/reconnecting tab. Serialized
   * (N3) so a burst of tabs doesn't stampede ssh handshakes. Stopped and
   * disconnected tabs are left for the explicit Respawn affordance. */
  reconnectAll = async (): Promise<void> => {
    if (this.disposed) return;
    const keys = this.tabs
      .filter((t) => t.status === "live" || t.status === "reconnecting")
      .map((t) => t.key);
    for (const key of keys) {
      if (this.disposed) return;
      const tab = this.tabs.find((t) => t.key === key);
      if (!tab) continue;
      this.setStatus(key, "reconnecting", null);
      await this.startReconnect(key, 0); // one at a time
    }
  };

  /**
   * Open a brand-new tab for a stopped (or otherwise absent) session by calling
   * the respawn opener directly.  Structurally mirrors `openSession` but:
   *   • calls `openers.respawn` which returns `Promise<SessionConnection>` (not
   *     `{connection, attached}`)
   *   • always commits the tab with `attached: false, status: "live"`
   *
   * If the key already exists and its tab is stopped or disconnected, the tab is
   * focused and `respawnTab` is triggered to re-create the session in-place
   * (rather than silently focusing a dead tab). Live/reconnecting existing tabs
   * are just focused, as before.
   */
  openViaRespawn = async (input: SpawnInput): Promise<OpenResult> => {
    if (this.disposed) {
      return { ok: false, error: OPEN_CANCELLED };
    }
    const key = tabKey(input.projectId, input.sessionId);
    const existing = this.tabs.find((t) => t.key === key);
    if (existing) {
      this.activeKey = key;
      this.commit();
      if (existing.status === "stopped" || existing.status === "disconnected") {
        void this.respawnTab(key);
      }
      return { ok: true, attached: false, opened: false };
    }

    return this.openTab(input, (p, s, a, _b, _w, _branch, _worktreeRoot) =>
      this.openers
        .respawn(p, s, a)
        .then((connection) => ({ connection, attached: false })),
    );
  };

  /**
   * Open a session that discovery reports as LIVE by ATTACHING to the running
   * tmux — never the spawn-first path. Spawn-first runs `git worktree add` before
   * it learns the session already exists, and when the session was created with
   * an explicit branch, that add lands at a *different* (convention) path and
   * succeeds, leaking a duplicate worktree/branch that resurfaces on remove
   * (the orphaned-session bug). Attaching creates nothing.
   *
   * An already-open tab just gets focused. If the live session died between the
   * discovery poll and this click, attach fails `sessionNotFound`; we fall back
   * to respawn (re-create tmux in the surviving worktree) rather than error.
   */
  openViaAttach = async (input: SpawnInput): Promise<OpenResult> => {
    if (this.disposed) {
      return { ok: false, error: OPEN_CANCELLED };
    }
    const key = tabKey(input.projectId, input.sessionId);
    const existing = this.tabs.find((t) => t.key === key);
    if (existing) {
      this.activeKey = key;
      this.commit();
      return { ok: true, attached: existing.attached, opened: false };
    }

    const result = await this.openTab(input, (p, s) =>
      this.openers.attach(p, s).then((connection) => ({
        connection,
        attached: true,
      })),
    );
    // Died between poll and click → re-create in the surviving worktree.
    if (!result.ok && "error" in result && isSessionNotFound(result.error)) {
      return this.openViaRespawn(input);
    }
    return result;
  };

  respawnTab = async (key: string): Promise<void> => {
    const tab = this.tabs.find((t) => t.key === key);
    if (!tab || this.disposed) return;
    // Refuse to respawn while a spawn-open or a teardown for this key is in
    // flight: a respawn racing a remove re-creates tmux in a worktree the
    // remove is deleting, leaving an orphan. A concurrent *respawn* is allowed
    // (newest-wins via the reconnect token below); only opens/teardowns block.
    if (this.pending.has(key) || this.teardownPending.has(key)) return;
    const token = this.newReconnectToken(key); // cancel any reconnect loop
    this.respawning.set(key, (this.respawning.get(key) ?? 0) + 1);
    this.setStatus(key, "reconnecting", null);
    let next: SessionConnection;
    try {
      next = await this.openers.respawn(
        tab.projectId,
        tab.sessionId,
        tab.agent,
      );
    } catch (e) {
      this.endRespawn(key);
      if (token.cancelled || this.disposed) return;
      this.setStatus(key, "disconnected", errorMessage(e));
      return;
    }
    this.endRespawn(key);
    if (token.cancelled || this.disposed) {
      void next.close().catch(() => {});
      return;
    }
    this.swapConnection(key, next, "live");
  };

  /** Decrement the in-flight respawn count for `key`, dropping the entry at 0. */
  private endRespawn(key: string): void {
    const n = (this.respawning.get(key) ?? 1) - 1;
    if (n <= 0) this.respawning.delete(key);
    else this.respawning.set(key, n);
  }

  /** Stop a session (kill tmux, keep the worktree). An open tab flips to
   * "stopped" directly — we do NOT wait for the channel-death path, which would
   * flicker through "reconnecting" and waste an attach (D2). */
  stop = async (
    projectId: string,
    sessionId: string,
  ): Promise<TeardownResult> => {
    if (this.disposed) return { ok: false };
    const key = tabKey(projectId, sessionId);
    // Refuse if any open/respawn/teardown for this key is already in flight —
    // tearing down a session mid-spawn is the orphaning race.
    if (this.busy(key)) return { ok: false };
    this.teardownPending.add(key);
    try {
      await this.openers.stop(projectId, sessionId);
    } catch (error) {
      this.teardownPending.delete(key);
      return { ok: false, error };
    }
    this.teardownPending.delete(key);
    // Cancel any in-flight reconnect loop (the killed channel's onDeath may have
    // started one) so it doesn't race our status set, then mark the tab stopped.
    const token = this.reconnectTokens.get(key);
    if (token) token.cancelled = true;
    if (this.tabs.some((t) => t.key === key))
      this.setStatus(key, "stopped", null);
    return { ok: true };
  };

  /** Remove a session for good. On success the local tab is closed (silent;
   * closeTab also cancels the reconnect token). WorkspaceDirty is surfaced so the
   * UI can confirm a force-remove (D2/D5). */
  remove = async (
    projectId: string,
    sessionId: string,
    force: boolean,
  ): Promise<RemoveResult> => {
    if (this.disposed) return { ok: false };
    const key = tabKey(projectId, sessionId);
    // Refuse if any open/respawn/teardown for this key is already in flight —
    // removing a session mid-spawn is the orphaning race.
    if (this.busy(key)) return { ok: false };
    this.teardownPending.add(key);
    try {
      await this.openers.remove(projectId, sessionId, force);
    } catch (error) {
      this.teardownPending.delete(key);
      if (isWorkspaceDirty(error)) return { ok: false, dirty: error.reason };
      return { ok: false, error };
    }
    this.teardownPending.delete(key);
    this.closeTab(key); // no-op if not open; else silent close + token cancel
    return { ok: true };
  };

  /** App teardown: cancel all pending opens/reconnects and close every
   * connection. Terminal — a disposed store never opens new work. */
  dispose = (): void => {
    this.disposed = true;
    for (const token of this.pending.values()) token.cancelled = true;
    this.pending.clear();
    // Cancel all reconnect tokens
    for (const token of this.reconnectTokens.values()) token.cancelled = true;
    this.reconnectTokens.clear();
    // In-flight respawns observe `disposed` post-await and bail; drop the marks.
    this.respawning.clear();
    for (const tab of this.tabs) void tab.connection.close().catch(() => {});
    this.tabs = [];
    this.activeKey = null;
    this.commit();
  };
}

/** Respawn requires a worktree; a shared session has none (NotWorktreeProject).
 * Pure so the App pane can gate the Respawn affordance without rendering tests. */
export function canRespawn(workspace: WorkspaceModeDto): boolean {
  return workspace !== "shared";
}

/** UI copy for a failed remove. A genuine backend failure is a tauri-specta
 * `BridgeError` — a typed *plain object*, not an `Error` — so an `instanceof
 * Error` check alone drops its message and shows a generic string, hiding the
 * real reason (e.g. "kill tmux: permission denied"). Surface the message when
 * there is one; fall back to friendly copy for a bare `{ok:false}` (the
 * in-flight guard / disposed store, which aren't real failures). */
export function removeErrorMessage(result: RemoveResult): string {
  if ("error" in result && result.error !== undefined) {
    const e = result.error;
    if (e instanceof Error) return e.message;
    return errorMessage(e); // BridgeError → its message; string → itself
  }
  return "Could not remove the session.";
}
