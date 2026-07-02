import { describe, expect, it, vi } from "vitest";
import type { SessionConnection } from "./connection";
import type { StoreOpeners, Tab } from "./session-store";
import {
  canRespawn,
  OPEN_CANCELLED,
  removeErrorMessage,
  reorderTabs,
  SessionStore,
  tabKey,
} from "./session-store";

function makeConn(): SessionConnection {
  return {
    subscribe: () => () => {},
    onClose: () => () => {},
    write: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    closed: false,
    lastOutput: () => "",
  };
}

// An opener set that spawns immediately with a fresh connection.
function instantOpeners(attached = false): StoreOpeners {
  return {
    spawn: vi.fn(async (_p, _s, _a, _w) => ({
      connection: makeConn(),
      attached,
    })),
    attach: vi.fn(async () => makeConn()),
    respawn: vi.fn(async () => makeConn()),
    schedule: vi.fn(),
    stop: vi.fn(async () => {}),
    remove: vi.fn(async () => {}),
  };
}

const spec = (p: string, s: string, a: string | null = null) => ({
  projectId: p,
  sessionId: s,
  agent: a,
  base: null,
  workspace: "worktree" as const,
});

describe("SessionStore", () => {
  it("opens a session as a focused tab", async () => {
    const store = new SessionStore(instantOpeners());
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: false, opened: true });
    const snap = store.getSnapshot();
    expect(snap.tabs.map((t) => t.key)).toEqual(["api/fix"]);
    expect(snap.activeKey).toBe("api/fix");
  });

  it("records attached:true from the opener", async () => {
    const store = new SessionStore(instantOpeners(true));
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: true, opened: true });
    expect(store.getSnapshot().tabs[0].attached).toBe(true);
  });

  it("opens with status live and error null", async () => {
    const store = new SessionStore(instantOpeners());
    await store.openSession(spec("api", "fix"));
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.error).toBeNull();
  });

  it("dedupes: opening an existing key focuses it, no second open", async () => {
    const openers = instantOpeners();
    const store = new SessionStore(openers);
    await store.openSession(spec("api", "fix"));
    await store.openSession(spec("web", "ui"));
    store.focusTab("api/fix"); // move focus away from web/ui
    const result = await store.openSession(spec("web", "ui", "other-agent"));
    expect(result).toEqual({ ok: true, attached: false, opened: false });
    expect(openers.spawn).toHaveBeenCalledTimes(2); // not 3
    expect(store.getSnapshot().tabs).toHaveLength(2);
    expect(store.getSnapshot().activeKey).toBe("web/ui");
  });

  it("reports opened:true for a fresh spawn, opened:false for an existing tab (#133)", async () => {
    const store = new SessionStore(instantOpeners());
    // First open spawns a fresh tab.
    const first = await store.openSession(spec("api", "fix"));
    expect(first).toEqual({ ok: true, attached: false, opened: true });
    // Re-opening the same key only focuses it — no fresh terminal, so the
    // dialog/spawn-focus path must not arm focus on a possibly-non-live tab.
    const second = await store.openSession(spec("api", "fix"));
    expect(second).toEqual({ ok: true, attached: false, opened: false });
  });

  it("closeTab detaches the connection, removes the tab, focuses a neighbour", async () => {
    const conns: SessionConnection[] = [];
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => {
        const connection = makeConn();
        conns.push(connection);
        return { connection, attached: false };
      }),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    await store.openSession(spec("a", "1"));
    await store.openSession(spec("b", "2")); // active = b/2
    store.closeTab("b/2");
    expect(conns[1].close).toHaveBeenCalledTimes(1);
    const snap = store.getSnapshot();
    expect(snap.tabs.map((t) => t.key)).toEqual(["a/1"]);
    expect(snap.activeKey).toBe("a/1"); // re-focused the neighbour
  });

  it("closing the last tab returns to the empty state", async () => {
    const store = new SessionStore(instantOpeners());
    await store.openSession(spec("a", "1"));
    store.closeTab("a/1");
    expect(store.getSnapshot().tabs).toHaveLength(0);
    expect(store.getSnapshot().activeKey).toBeNull();
  });

  it("CRITICAL: closes the orphaned connection if the key is closed before connect resolves", async () => {
    let resolve!: (r: {
      connection: SessionConnection;
      attached: boolean;
    }) => void;
    const conn = makeConn();
    const openers: StoreOpeners = {
      spawn: vi.fn(
        () =>
          new Promise<{ connection: SessionConnection; attached: boolean }>(
            (res) => {
              resolve = res;
            },
          ),
      ),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const pending = store.openSession(spec("a", "1"));
    store.closeTab("a/1"); // cancels the in-flight open
    resolve({ connection: conn, attached: false });
    const result = await pending;
    expect(result).toEqual({ ok: false, error: OPEN_CANCELLED });
    expect(conn.close).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("exposes the connecting key synchronously while an open is in flight, then clears it when live (#170)", async () => {
    let resolve!: (r: {
      connection: SessionConnection;
      attached: boolean;
    }) => void;
    const openers: StoreOpeners = {
      spawn: vi.fn(
        () =>
          new Promise<{ connection: SessionConnection; attached: boolean }>(
            (res) => {
              resolve = res;
            },
          ),
      ),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const listener = vi.fn();
    store.subscribe(listener);
    // The open's commit must land synchronously (before the first await
    // suspends), so the sidebar row spins within a frame of the click.
    const pending = store.openSession(spec("api", "fix"));
    expect(listener).toHaveBeenCalled();
    expect(store.getSnapshot().connecting).toEqual(["api/fix"]);
    expect(store.getSnapshot().tabs).toHaveLength(0); // not committed yet
    resolve({ connection: makeConn(), attached: false });
    await pending;
    expect(store.getSnapshot().connecting).toEqual([]);
    expect(store.getSnapshot().tabs.map((t) => t.key)).toEqual(["api/fix"]);
  });

  it("clears the connecting key when the open fails (#170)", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => {
        throw { kind: "transport", message: "net" };
      }),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openSession(spec("api", "fix"));
    expect(result.ok).toBe(false);
    expect(store.getSnapshot().connecting).toEqual([]);
  });

  it("clears the connecting key when the open is cancelled before it resolves (#170)", async () => {
    let resolve!: (r: {
      connection: SessionConnection;
      attached: boolean;
    }) => void;
    const openers: StoreOpeners = {
      spawn: vi.fn(
        () =>
          new Promise<{ connection: SessionConnection; attached: boolean }>(
            (res) => {
              resolve = res;
            },
          ),
      ),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const pending = store.openSession(spec("api", "fix"));
    expect(store.getSnapshot().connecting).toEqual(["api/fix"]);
    store.dispose(); // cancel the in-flight open
    resolve({ connection: makeConn(), attached: false });
    await pending;
    expect(store.getSnapshot().connecting).toEqual([]);
  });

  it("tracks connecting through the respawn-from-stopped open path (#170)", async () => {
    let resolve!: (c: SessionConnection) => void;
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(
        () =>
          new Promise<SessionConnection>((res) => {
            resolve = res;
          }),
      ),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const pending = store.openViaRespawn(spec("api", "fix"));
    expect(store.getSnapshot().connecting).toEqual(["api/fix"]);
    resolve(makeConn());
    await pending;
    expect(store.getSnapshot().connecting).toEqual([]);
    expect(store.getSnapshot().tabs.map((t) => t.key)).toEqual(["api/fix"]);
  });

  it("dispose closes all open connections and cancels pending opens", async () => {
    let resolve!: (r: {
      connection: SessionConnection;
      attached: boolean;
    }) => void;
    const pendingConn = makeConn();
    const openConn = makeConn();
    let call = 0;
    const openers: StoreOpeners = {
      spawn: vi.fn(() => {
        call += 1;
        if (call === 1)
          return Promise.resolve({ connection: openConn, attached: false });
        return new Promise<{
          connection: SessionConnection;
          attached: boolean;
        }>((res) => {
          resolve = res;
        });
      }),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    await store.openSession(spec("a", "1")); // committed
    const pending = store.openSession(spec("b", "2")); // in flight
    store.dispose();
    resolve({ connection: pendingConn, attached: false });
    await pending;
    expect(openConn.close).toHaveBeenCalledTimes(1);
    expect(pendingConn.close).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("propagates an opener rejection as { ok: false }", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => {
        throw { kind: "transport", message: "net" };
      }),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openSession(spec("a", "1"));
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatchObject({ kind: "transport" });
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("notifies subscribers on change and tabKey formats the key", async () => {
    const store = new SessionStore(instantOpeners());
    const listener = vi.fn();
    const unsub = store.subscribe(listener);
    await store.openSession(spec("a", "1"));
    expect(listener).toHaveBeenCalled();
    unsub();
    expect(tabKey("a", "1")).toBe("a/1");
  });

  it("unsubscribed listener is not called on later changes", async () => {
    const store = new SessionStore(instantOpeners());
    const listener = vi.fn();
    const unsub = store.subscribe(listener);
    await store.openSession(spec("a", "1"));
    const callsBeforeUnsub = listener.mock.calls.length;
    unsub();
    await store.openSession(spec("b", "2"));
    expect(listener).toHaveBeenCalledTimes(callsBeforeUnsub);
  });

  it("closeTab of a non-active tab leaves activeKey unchanged", async () => {
    const store = new SessionStore(instantOpeners());
    await store.openSession(spec("a", "1"));
    await store.openSession(spec("b", "2"));
    await store.openSession(spec("c", "3"));
    store.focusTab("a/1");
    store.closeTab("b/2"); // not the active tab
    const snap = store.getSnapshot();
    expect(snap.activeKey).toBe("a/1");
    expect(snap.tabs.map((t) => t.key)).toEqual(["a/1", "c/3"]);
  });

  // A disposed store short-circuits openSession: it returns OPEN_CANCELLED
  // immediately and never invokes the opener, so no spawn/attach side effect
  // happens post-teardown. (dispose-DURING-flight is covered by the test above.)
  it("an open started after dispose is cancelled without invoking the opener", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({
        connection: makeConn(),
        attached: false,
      })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    store.dispose();
    const result = await store.openSession(spec("a", "1"));
    expect(result).toEqual({ ok: false, error: OPEN_CANCELLED });
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });
});

// ─── reorderTabs (pure drag-to-reorder / live-preview helper) ────────────────

describe("reorderTabs", () => {
  // Minimal Tab stand-ins: reorderTabs only touches `key`.
  const tabs = (...keys: string[]) => keys.map((key) => ({ key }) as Tab);
  const keys = (ts: Tab[]) => ts.map((t) => t.key);

  it("moving a tab right drops it after the target", () => {
    expect(keys(reorderTabs(tabs("a", "b", "c", "d"), "a", "c"))).toEqual([
      "b",
      "c",
      "a",
      "d",
    ]);
  });

  it("moving a tab left drops it before the target", () => {
    expect(keys(reorderTabs(tabs("a", "b", "c", "d"), "d", "b"))).toEqual([
      "a",
      "d",
      "b",
      "c",
    ]);
  });

  it("dropping onto an adjacent tab swaps the two", () => {
    expect(keys(reorderTabs(tabs("a", "b", "c"), "a", "b"))).toEqual([
      "b",
      "a",
      "c",
    ]);
  });

  it("moves the head tab to the last slot", () => {
    expect(keys(reorderTabs(tabs("a", "b", "c", "d"), "a", "d"))).toEqual([
      "b",
      "c",
      "d",
      "a",
    ]);
  });

  it("moves the tail tab to the first slot", () => {
    expect(keys(reorderTabs(tabs("a", "b", "c", "d"), "d", "a"))).toEqual([
      "d",
      "a",
      "b",
      "c",
    ]);
  });

  it("returns the same array reference for a self-move (no-op)", () => {
    const input = tabs("a", "b");
    expect(reorderTabs(input, "a", "a")).toBe(input);
  });

  it("returns the same array reference for an unknown key (no-op)", () => {
    const input = tabs("a", "b");
    expect(reorderTabs(input, "ghost", "b")).toBe(input);
    expect(reorderTabs(input, "a", "ghost")).toBe(input);
  });

  it("does not mutate the input array", () => {
    const input = tabs("a", "b", "c");
    reorderTabs(input, "a", "c");
    expect(keys(input)).toEqual(["a", "b", "c"]);
  });
});

// ─── reorderTab (drag-to-reorder) ────────────────────────────────────────────

describe("SessionStore reorderTab", () => {
  // Open `keys` in order and return the store. activeKey ends on the last open.
  async function storeWith(keys: Array<[string, string]>) {
    const store = new SessionStore(instantOpeners());
    for (const [p, s] of keys) await store.openSession(spec(p, s));
    return store;
  }
  const order = (store: SessionStore) =>
    store.getSnapshot().tabs.map((t) => t.key);

  it("moving a tab right drops it after the target", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
      ["c", "3"],
      ["d", "4"],
    ]);
    store.reorderTab("a/1", "c/3");
    expect(order(store)).toEqual(["b/2", "c/3", "a/1", "d/4"]);
  });

  it("moving a tab left drops it before the target", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
      ["c", "3"],
      ["d", "4"],
    ]);
    store.reorderTab("d/4", "b/2");
    expect(order(store)).toEqual(["a/1", "d/4", "b/2", "c/3"]);
  });

  it("dropping a tab onto an adjacent tab swaps the two", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
      ["c", "3"],
    ]);
    store.reorderTab("a/1", "b/2");
    expect(order(store)).toEqual(["b/2", "a/1", "c/3"]);
  });

  it("dropping a tab on itself is a no-op", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
    ]);
    store.reorderTab("a/1", "a/1");
    expect(order(store)).toEqual(["a/1", "b/2"]);
  });

  it("ignores unknown drag/target keys", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
    ]);
    store.reorderTab("ghost", "b/2");
    store.reorderTab("a/1", "ghost");
    expect(order(store)).toEqual(["a/1", "b/2"]);
  });

  it("reordering leaves the active tab unchanged", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
      ["c", "3"],
    ]);
    store.focusTab("a/1");
    store.reorderTab("c/3", "a/1");
    expect(store.getSnapshot().activeKey).toBe("a/1");
    expect(order(store)).toEqual(["c/3", "a/1", "b/2"]);
  });

  it("notifies subscribers when the order changes", async () => {
    const store = await storeWith([
      ["a", "1"],
      ["b", "2"],
    ]);
    const listener = vi.fn();
    store.subscribe(listener);
    store.reorderTab("a/1", "b/2");
    expect(listener).toHaveBeenCalledTimes(1);
  });
});

// ─── canRespawn + workspace threading ────────────────────────────────────────

describe("canRespawn", () => {
  it("canRespawn is false for shared, true for worktree", () => {
    expect(canRespawn("shared")).toBe(false);
    expect(canRespawn("worktree")).toBe(true);
  });
});

describe("removeErrorMessage", () => {
  it("surfaces a BridgeError's message (a plain object, not an Error)", () => {
    // The bridge throws typed plain objects; the dialog used to fall through to
    // a generic string because they fail `instanceof Error`, masking the cause.
    const err = { kind: "transport", message: "kill tmux: permission denied" };
    expect(removeErrorMessage({ ok: false, error: err })).toBe(
      "kill tmux: permission denied",
    );
  });

  it("uses a real Error instance's message", () => {
    expect(removeErrorMessage({ ok: false, error: new Error("boom") })).toBe(
      "boom",
    );
  });

  it("passes a string error through", () => {
    expect(removeErrorMessage({ ok: false, error: "nope" })).toBe("nope");
  });

  it("falls back to the friendly copy for a bare {ok:false}", () => {
    expect(removeErrorMessage({ ok: false })).toBe(
      "Could not remove the session.",
    );
  });
});

describe("SessionStore workspace threading", () => {
  it("openSession forwards branch and worktreeRoot to the spawn opener", async () => {
    let capturedBranch: string | null | undefined;
    let capturedWorktreeRoot: string | null | undefined;
    const store = new SessionStore({
      spawn: vi.fn(
        async (
          _p: string,
          _s: string,
          _a: string | null,
          _b: string | null,
          _w: string,
          branch: string | null,
          worktreeRoot: string | null,
        ) => {
          capturedBranch = branch;
          capturedWorktreeRoot = worktreeRoot;
          return { connection: fakeConn().conn, attached: false };
        },
      ),
      attach: vi.fn(async () => fakeConn().conn),
      respawn: vi.fn(async () => fakeConn().conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    } as any);
    await store.openSession({
      projectId: "api",
      sessionId: "s1",
      agent: null,
      base: null,
      workspace: "worktree",
      branch: "feat/my-branch",
      worktreeRoot: "/path/to/worktree",
    });
    expect(capturedBranch).toBe("feat/my-branch");
    expect(capturedWorktreeRoot).toBe("/path/to/worktree");
  });

  it("spawn opener receives the chosen workspace and stores it on the tab", async () => {
    const calls: string[] = [];
    const store = new SessionStore({
      spawn: (
        _p: string,
        _s: string,
        _a: string | null,
        _b: string | null,
        w: string,
      ) => {
        calls.push(w);
        return Promise.resolve({
          connection: fakeConn().conn,
          attached: false,
        });
      },
      attach: vi.fn(async () => fakeConn().conn),
      respawn: vi.fn(async () => fakeConn().conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    } as any);
    await store.openSession({
      projectId: "api",
      sessionId: "s1",
      agent: null,
      base: null,
      workspace: "shared",
    });
    expect(calls).toEqual(["shared"]);
    expect(store.getSnapshot().tabs[0].workspace).toBe("shared");
  });
});

// ─── Reconnect state machine tests ───────────────────────────────────────────

// Minimal fake connection that lets the test drive death + capture close().
// `lastOutput` is the cause line this connection would surface (#28).
function fakeConn(lastOutput = "") {
  let closeListener: (() => void) | null = null;
  let closedFlag = false;
  const conn: SessionConnection = {
    subscribe: () => () => {},
    onClose: (l) => {
      closeListener = l;
      return () => {
        closeListener = null;
      };
    },
    write: () => Promise.resolve(),
    resize: () => Promise.resolve(),
    close: () => {
      closedFlag = true;
      return Promise.resolve();
    },
    get closed() {
      return closedFlag;
    },
    lastOutput: () => lastOutput,
  };
  return { conn, die: () => closeListener?.(), wasClosed: () => closedFlag };
}

// A controllable clock: schedule() records callbacks the test fires manually.
function fakeClock() {
  const pending: Array<{ ms: number; fn: () => void }> = [];
  return {
    schedule: (fn: () => void, ms: number) => {
      pending.push({ ms, fn });
      return pending.length;
    },
    flush: () => {
      const fns = pending.splice(0).map((p) => p.fn);
      for (const f of fns) f();
    },
    count: () => pending.length,
  };
}

function makeStore(
  overrides: Partial<StoreOpeners> = {},
  spawnedLastOutput = "",
) {
  const spawned = fakeConn(spawnedLastOutput);
  const clock = fakeClock();
  const openers: StoreOpeners = {
    spawn: () => Promise.resolve({ connection: spawned.conn, attached: false }),
    attach: () => Promise.resolve(fakeConn().conn),
    respawn: () => Promise.resolve(fakeConn().conn),
    schedule: clock.schedule,
    stop: () => Promise.resolve(),
    remove: () => Promise.resolve(),
    ...overrides,
  };
  return { store: new SessionStore(openers), clock, spawned };
}

describe("SessionStore reconnect machine", () => {
  it("opens a tab as live", async () => {
    const { store } = makeStore();
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    expect(store.getSnapshot().tabs[0].status).toBe("live");
  });

  it("death → reconnecting → live after a successful re-attach", async () => {
    const fresh = fakeConn();
    const { store, spawned } = makeStore({
      attach: () => Promise.resolve(fresh.conn),
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    // reconnect is async; the store sets reconnecting synchronously then swaps.
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    await Promise.resolve();
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(fresh.conn);
    expect(spawned.wasClosed()).toBe(true); // old dead connection is closed
  });

  it("death → stopped when re-attach hits sessionNotFound", async () => {
    const { store, spawned } = makeStore({
      attach: () =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("stopped");
    // No usable last output → no cause (overlay reads a bare "Session stopped.").
    expect(tab.error).toBeNull();
  });

  it("death → stopped carries the dead connection's last output as the cause", async () => {
    const { store, spawned } = makeStore(
      {
        attach: () =>
          Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      },
      "claude: command not found",
    );
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("stopped");
    expect(tab.error).toBe("claude: command not found");
  });

  it("death → disconnected with cause on a config error (no retry loop)", async () => {
    const { store, spawned, clock } = makeStore({
      attach: () =>
        Promise.reject({ kind: "config", message: "unknown host hermes" }),
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("disconnected");
    expect(tab.error).toContain("unknown host");
    expect(clock.count()).toBe(0); // terminal: nothing rescheduled
  });

  it("transport error retries on a backoff schedule", async () => {
    let attempts = 0;
    const fresh = fakeConn();
    const { store, spawned, clock } = makeStore({
      attach: () => {
        attempts += 1;
        return attempts < 2
          ? Promise.reject({ kind: "transport", message: "down" })
          : Promise.resolve(fresh.conn);
      },
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    expect(clock.count()).toBe(1); // one retry scheduled
    clock.flush();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("live");
  });

  // RACE 1: a second respawnTab fires while the first respawn is still pending.
  // The second call bumps the token, so the first respawn is cancelled. When the
  // first (cancelled) respawn resolves, its connection must NOT be installed —
  // the tab must still hold the spawned connection until the second wins.
  // This exercises the respawnTab post-await guard on the CANCELLATION side.
  it("RACE: second respawnTab cancels first → cancelled respawn never installed as live connection", async () => {
    let respawn1Resolve!: (c: SessionConnection) => void;
    const respawnConn1 = fakeConn();
    const respawnConn2 = fakeConn();
    let call = 0;

    const { store, spawned } = makeStore({
      respawn: () => {
        call += 1;
        if (call === 1)
          return new Promise<SessionConnection>((res) => {
            respawn1Resolve = res;
          });
        // Second respawn resolves immediately
        return Promise.resolve(respawnConn2.conn);
      },
    });

    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });

    // Trigger death → store enters reconnecting
    spawned.die();
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");

    // First respawnTab — hangs
    const p1 = store.respawnTab("p/s");
    // Second respawnTab — resolves immediately, cancels first token
    await store.respawnTab("p/s");

    // Tab should now be live with conn2
    expect(store.getSnapshot().tabs[0].status).toBe("live");
    expect(store.getSnapshot().tabs[0].connection).toBe(respawnConn2.conn);

    // Now resolve the stale (cancelled) first respawn
    respawn1Resolve(respawnConn1.conn);
    await Promise.resolve();
    await p1;

    // KEY ASSERTION: the cancelled respawn must NOT replace the live connection
    expect(store.getSnapshot().tabs[0].connection).not.toBe(respawnConn1.conn);
    // cancelled respawn's connection must have been closed
    expect(respawnConn1.wasClosed()).toBe(true);
    // The winner conn2 is still live
    expect(store.getSnapshot().tabs[0].connection).toBe(respawnConn2.conn);
    expect(store.getSnapshot().tabs[0].status).toBe("live");
    await p1;
  });

  // RACE 2: two concurrent respawnTab calls → only one swap survives.
  // The second respawnTab bumps the token, cancelling the first. When the first
  // (cancelled) respawn resolves, it must NOT install conn1 as the live
  // connection — the tab must still hold the original spawned connection.
  // Only after the second resolves should the tab hold conn2.
  it("RACE: two concurrent respawnTab calls → cancelled respawn never installed, winner is conn2", async () => {
    let respawn1Resolve!: (c: SessionConnection) => void;
    let respawn2Resolve!: (c: SessionConnection) => void;
    const conn1 = fakeConn();
    const conn2 = fakeConn();
    let call = 0;

    const { store, spawned } = makeStore({
      respawn: () => {
        call += 1;
        if (call === 1)
          return new Promise<SessionConnection>((res) => {
            respawn1Resolve = res;
          });
        return new Promise<SessionConnection>((res) => {
          respawn2Resolve = res;
        });
      },
    });

    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();

    // First respawnTab call — hangs
    const p1 = store.respawnTab("p/s");
    // Second respawnTab call — also hangs; cancels the first token
    const p2 = store.respawnTab("p/s");

    // Resolve the first (now-cancelled) respawn.
    // KEY ASSERTION: a cancelled respawn must NEVER become the live connection.
    // Before the fix, the unguarded swapConnection would install conn1 here.
    respawn1Resolve(conn1.conn);
    await Promise.resolve();
    await p1;

    // conn1 must NOT be installed — the cancelled respawn was discarded
    expect(store.getSnapshot().tabs[0].connection).not.toBe(conn1.conn);
    // conn1 must have been closed (the cancelled respawn cleaned it up)
    expect(conn1.wasClosed()).toBe(true);

    // Now resolve the second (live) respawn
    respawn2Resolve(conn2.conn);
    await Promise.resolve();
    await p2;

    // Second respawn wins: conn2 is live
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(conn2.conn);
  });

  // A minimal SpawnInput for p/s used across the #189 revival tests.
  const ps = {
    projectId: "p",
    sessionId: "s",
    agent: null,
    base: null,
    workspace: "worktree" as const,
  };

  it("reconnectTab re-attaches a disconnected tab back to live (#189)", async () => {
    let attachFails = true;
    const fresh = fakeConn();
    const { store, spawned } = makeStore({
      attach: () =>
        attachFails
          ? Promise.reject({ kind: "config", message: "unknown host" })
          : Promise.resolve(fresh.conn),
    });
    await store.openSession(ps);
    spawned.die(); // death → reconnect → config = terminal → disconnected
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("disconnected");

    attachFails = false;
    store.reconnectTab("p/s");
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    await Promise.resolve();
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(fresh.conn);
  });

  it("reconnectTab on a truly-gone session lands back at stopped, never respawns (#189)", async () => {
    const respawn = vi.fn(() => Promise.resolve(fakeConn().conn));
    const { store, spawned } = makeStore({
      attach: () =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      respawn,
    });
    await store.openSession(ps);
    spawned.die(); // → stopped (sessionNotFound)
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");

    store.reconnectTab("p/s");
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");
    expect(respawn).not.toHaveBeenCalled(); // attach-only, no duplicate session
  });

  it("reconnectTab defers to an in-flight respawn (busy guard) (#189)", async () => {
    let release: () => void = () => {};
    const respawnConn = fakeConn();
    const respawn = vi.fn(
      () =>
        new Promise<SessionConnection>((res) => {
          release = () => res(respawnConn.conn);
        }),
    );
    const attach = vi.fn(() => Promise.resolve(fakeConn().conn));
    const { store } = makeStore({ respawn, attach });
    await store.openSession(ps);

    const p = store.respawnTab("p/s"); // respawning.has("p/s") = true, awaits
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");

    store.reconnectTab("p/s"); // busy → no-op
    expect(attach).not.toHaveBeenCalled(); // did not start an attach loop

    release();
    await p;
    await Promise.resolve();
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(respawnConn.conn);
  });

  it("openViaAttach reconnects a non-live ACTIVE dedupe in place (#189 Bug A)", async () => {
    let attachFails = true;
    const fresh = fakeConn();
    const attach = vi.fn(() =>
      attachFails
        ? Promise.reject({ kind: "config", message: "bad" })
        : Promise.resolve(fresh.conn),
    );
    const { store, spawned } = makeStore({ attach });
    await store.openSession(ps);
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("disconnected");
    const attachCallsBefore = attach.mock.calls.length;

    attachFails = false;
    const r = await store.openViaAttach(ps); // dedupe to the disconnected active tab
    expect(r).toEqual({ ok: true, attached: false, opened: false });
    expect(attach.mock.calls.length).toBe(attachCallsBefore + 1); // a fresh attach fired
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("live"); // revived in place
  });

  it("openViaAttach flips activeKey to a non-live BACKGROUND tab AND reconnects it (#189)", async () => {
    let bFails = true;
    const freshB = fakeConn();
    const attach = vi.fn(() =>
      bFails
        ? Promise.reject({ kind: "config", message: "bad" })
        : Promise.resolve(freshB.conn),
    );
    const bConn = fakeConn();
    let spawnCall = 0;
    const spawn = vi.fn(() => {
      spawnCall += 1;
      return Promise.resolve({
        connection: spawnCall === 2 ? bConn.conn : fakeConn().conn,
        attached: false,
      });
    });
    const { store } = makeStore({ attach, spawn });
    await store.openSession(ps); // tab A (p/s), active
    await store.openSession({ ...ps, sessionId: "b" }); // tab B (p/b), active
    bConn.die(); // B dies → reconnect → config → disconnected
    await Promise.resolve();
    await Promise.resolve();
    store.focusTab("p/s"); // A active again; B disconnected in background
    expect(store.getSnapshot().activeKey).toBe("p/s");

    bFails = false;
    const attachBefore = attach.mock.calls.length;
    await store.openViaAttach({ ...ps, sessionId: "b" }); // click background B
    expect(store.getSnapshot().activeKey).toBe("p/b"); // flipped active
    expect(attach.mock.calls.length).toBe(attachBefore + 1); // a reconnect fired
    await Promise.resolve();
    await Promise.resolve();
    const tabB = store.getSnapshot().tabs.find((t) => t.key === "p/b");
    expect(tabB?.status).toBe("live"); // and revived to live
  });

  it("openViaAttach only focuses a LIVE dedupe (no reconnect) (#189)", async () => {
    const attach = vi.fn(() => Promise.resolve(fakeConn().conn));
    const { store } = makeStore({ attach });
    await store.openSession(ps); // A live
    await store.openSession({ ...ps, sessionId: "b" }); // B live, active
    store.focusTab("p/s");
    const before = attach.mock.calls.length;
    await store.openViaAttach({ ...ps, sessionId: "b" }); // dedupe to live B
    expect(store.getSnapshot().activeKey).toBe("p/b");
    expect(attach.mock.calls.length).toBe(before); // no reconnect fired
    expect(store.getSnapshot().tabs.find((t) => t.key === "p/b")?.status).toBe(
      "live",
    );
  });

  it("REGRESSION #189 Bug B: openViaRespawn respawns a reconnecting tab", async () => {
    const respawnConn = fakeConn();
    const respawn = vi.fn(() => Promise.resolve(respawnConn.conn));
    const { store, spawned } = makeStore({
      attach: () => Promise.reject({ kind: "transport", message: "down" }),
      respawn,
    });
    await store.openSession(ps);
    spawned.die(); // → reconnecting (transport = retry fate, backoff scheduled)
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");

    const r = await store.openViaRespawn(ps); // discovery says stopped
    expect(r).toEqual({ ok: true, attached: false, opened: false });
    await Promise.resolve();
    await Promise.resolve();
    expect(respawn).toHaveBeenCalledTimes(1); // reconnecting tab WAS respawned
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(respawnConn.conn);
  });

  it("openViaRespawn never respawns a LIVE dedupe (focus only) (#189)", async () => {
    const respawn = vi.fn(() => Promise.resolve(fakeConn().conn));
    const { store } = makeStore({ respawn });
    await store.openSession(ps); // A live
    await store.openSession({ ...ps, sessionId: "b" }); // B live, active
    store.focusTab("p/s");
    await store.openViaRespawn({ ...ps, sessionId: "b" }); // dedupe to live B
    expect(respawn).not.toHaveBeenCalled(); // must not kill a live session
    expect(store.getSnapshot().activeKey).toBe("p/b");
  });
});

describe("SessionStore reconnect token-refetch race", () => {
  // The scheduled retry must carry the SAME token it was created with, not
  // re-fetch the current token by key. Otherwise a concurrent trigger
  // (reconnectStale) that installs a fresh token during the backoff window
  // leaves the stale scheduled retry alive: it re-fetches the NEW token, sees
  // it un-cancelled, and runs a SECOND attach/swap loop — double attach/orphan.
  it("CRITICAL: a stale scheduled retry bails on its captured token after a concurrent reconnectStale wins", async () => {
    // attach behaviour, driven by a phase counter:
    //   call 1 (loop A, attempt 0): FAIL transport → loop A schedules a retry
    //   call 2 (loop B from reconnectStale): SUCCEED → tab goes live
    //   call 3+ (the STALE loop-A retry, if it wrongly runs): must NOT happen
    let attachCalls = 0;
    const fresh = fakeConn();
    const { store, spawned, clock } = makeStore({
      attach: () => {
        attachCalls += 1;
        if (attachCalls === 1) {
          return Promise.reject({ kind: "transport", message: "down" });
        }
        return Promise.resolve(fresh.conn);
      },
    });

    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });

    // Death → loop A (token A): attach call 1 fails → a retry is scheduled.
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    expect(attachCalls).toBe(1);
    expect(clock.count()).toBe(1); // loop A's retry is parked on the clock

    // Concurrent trigger: reconnectStale starts a FRESH loop B (token B), which
    // cancels token A. Loop B's attach (call 2) succeeds → tab is live.
    store.reconnectStale();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("live");
    expect(store.getSnapshot().tabs[0].connection).toBe(fresh.conn);
    expect(attachCalls).toBe(2);

    // Now fire the STALE loop-A retry parked on the clock. With the token
    // threaded, it carries the cancelled token A and bails: NO extra attach,
    // NO swap. (With the bug, it re-fetches token B, runs attach a 3rd time,
    // and swaps in an orphan.)
    clock.flush();
    await Promise.resolve();
    await Promise.resolve();

    expect(attachCalls).toBe(2); // stale retry was a no-op
    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(fresh.conn); // no orphan swapped in
  });
});

describe("SessionStore reconnectAll/Stale", () => {
  it("reconnectAll re-attaches a live tab serially and skips stopped tabs", async () => {
    const fresh = fakeConn();
    let attachCalls = 0;
    const { store, spawned } = makeStore({
      attach: () => {
        attachCalls += 1;
        return Promise.resolve(fresh.conn);
      },
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    await store.reconnectAll();
    expect(attachCalls).toBe(1);
    expect(store.getSnapshot().tabs[0].connection).toBe(fresh.conn);
    expect(spawned.wasClosed()).toBe(true);
  });

  it("reconnectStale kicks a reconnecting tab without touching a live one", async () => {
    // a tab in transport-retry backoff is reconnecting; reconnectStale retries now
    let attempts = 0;
    const fresh = fakeConn();
    const { store, spawned } = makeStore({
      attach: () => {
        attempts += 1;
        return attempts < 2
          ? Promise.reject({ kind: "transport", message: "down" })
          : Promise.resolve(fresh.conn);
      },
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    store.reconnectStale(); // retry immediately instead of waiting for the timer
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("live");
  });

  // N3 anti-stampede: reconnectAll MUST process tabs one at a time (serial
  // for...of/await), not launch all attaches concurrently (Promise.all).
  // The discriminating assertion is the mid-flight check: with serial
  // for...of/await, only ONE attach is in-flight at a time; with
  // Promise.all, both start immediately and the assertion fails.
  it("CRITICAL N3: reconnectAll serializes attaches — second attach waits for first to complete", async () => {
    const deferred: Array<{ resolve: (c: SessionConnection) => void }> = [];
    let started = 0;

    const freshConns = [fakeConn(), fakeConn()];

    const { store } = makeStore({
      attach: () => {
        const idx = started;
        started += 1;
        return new Promise<SessionConnection>((res) => {
          deferred.push({ resolve: () => res(freshConns[idx].conn) });
        });
      },
    });

    // Open two live tabs with different keys.
    await store.openSession({
      projectId: "p",
      sessionId: "s1",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s2",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    expect(store.getSnapshot().tabs).toHaveLength(2);

    // Start reconnectAll but do NOT await yet.
    const reconnectPromise = store.reconnectAll();

    // Yield to the microtask queue so the first attach call can start.
    await Promise.resolve();
    await Promise.resolve();

    // KEY ASSERTION: only ONE attach must be in-flight.
    // Serial for...of/await: started === 1 here.
    // Parallel Promise.all: started === 2 — would fail this assertion.
    expect(started).toBe(1);

    // Resolve the first attach and drain microtasks so the loop advances.
    deferred[0].resolve(freshConns[0].conn);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    // Now the second attach must have started.
    expect(started).toBe(2);

    // Resolve the second attach and let reconnectAll finish.
    deferred[1].resolve(freshConns[1].conn);
    await reconnectPromise;

    // Both tabs should be live.
    const snap = store.getSnapshot();
    expect(snap.tabs.find((t) => t.key === "p/s1")?.status).toBe("live");
    expect(snap.tabs.find((t) => t.key === "p/s2")?.status).toBe("live");
  });
});

// ─── openViaRespawn tests ─────────────────────────────────────────────────────

describe("SessionStore openViaRespawn", () => {
  it("openViaRespawn opens a NEW live tab via the respawn opener", async () => {
    const respawnConn = fakeConn();
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => respawnConn.conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openViaRespawn(spec("p", "s"));
    // spawn must NOT be called — only respawn
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(openers.respawn).toHaveBeenCalledTimes(1);
    expect(result).toEqual({ ok: true, attached: false, opened: true });
    const snap = store.getSnapshot();
    expect(snap.tabs).toHaveLength(1);
    expect(snap.tabs[0].status).toBe("live");
    expect(snap.tabs[0].connection).toBe(respawnConn.conn);
    expect(snap.activeKey).toBe("p/s");
  });

  it("openViaRespawn focuses an already-open tab without re-opening", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => fakeConn().conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    // Open a second tab first so focus is elsewhere
    await store.openViaRespawn(spec("p", "s"));
    await store.openSession(spec("p", "other"));
    expect(store.getSnapshot().activeKey).toBe("p/other");
    // Now call openViaRespawn for the already-open key
    const result = await store.openViaRespawn(spec("p", "s"));
    expect(result).toEqual({ ok: true, attached: false, opened: false });
    // No second respawn opener call
    expect(openers.respawn).toHaveBeenCalledTimes(1);
    // Focus moved back to the existing tab
    expect(store.getSnapshot().activeKey).toBe("p/s");
    expect(store.getSnapshot().tabs).toHaveLength(2);
  });

  it("openViaRespawn surfaces failure as {ok:false}", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => {
        throw { kind: "transport", message: "refused" };
      }),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openViaRespawn(spec("p", "s"));
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatchObject({ kind: "transport" });
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("openViaRespawn registers death on the new tab", async () => {
    const respawnConn = fakeConn();
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => fakeConn().conn),
      respawn: vi.fn(async () => respawnConn.conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    await store.openViaRespawn(spec("p", "s"));
    expect(store.getSnapshot().tabs[0].status).toBe("live");
    // Fire the connection's onClose to trigger onDeath
    respawnConn.die();
    // The store transitions synchronously to reconnecting
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
  });

  // Fix B (F9): sidebar-clicking a STOPPED tab should respawn it, not just focus.
  it("F9: openViaRespawn on a stopped tab focuses + triggers respawnTab", async () => {
    const { store, spawned } = makeStore({
      attach: () =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
    });
    // Open a tab and drive it to stopped via death → sessionNotFound
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");

    // Now swap in a respawn opener that we can observe
    const freshConn = fakeConn();
    let respawnCalled = 0;
    // Patch openers by replacing the store's openers reference is not directly
    // possible, so we use a new store that starts from stopped state by calling
    // respawnTab directly after driving stopped via makeStore, then verify
    // openViaRespawn calls respawnTab by intercepting the respawn opener.
    // Instead: build a store whose respawn we can track, drive stopped, then call openViaRespawn.
    const openers2: StoreOpeners = {
      spawn: vi.fn(async () => ({
        connection: fakeConn().conn,
        attached: false,
      })),
      attach: vi.fn(() =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      ),
      respawn: vi.fn(async () => {
        respawnCalled += 1;
        return freshConn.conn;
      }),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store2 = new SessionStore(openers2);
    const { conn: spawnConn2, die: die2 } = fakeConn();
    // Patch spawn to return our controlled conn
    (openers2.spawn as ReturnType<typeof vi.fn>).mockResolvedValue({
      connection: spawnConn2,
      attached: false,
    });
    await store2.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    die2(); // trigger death
    await Promise.resolve();
    await Promise.resolve();
    expect(store2.getSnapshot().tabs[0].status).toBe("stopped");

    // Now call openViaRespawn for the already-open stopped tab
    const result = await store2.openViaRespawn(spec("p", "s"));
    expect(result).toEqual({ ok: true, attached: false, opened: false });
    // Focus should be on p/s
    expect(store2.getSnapshot().activeKey).toBe("p/s");
    // The respawn opener must have been invoked (via respawnTab)
    await Promise.resolve();
    await Promise.resolve();
    expect(respawnCalled).toBe(1);
    // Tab should be live after respawnTab completes
    expect(store2.getSnapshot().tabs[0].status).toBe("live");
    expect(store2.getSnapshot().tabs[0].connection).toBe(freshConn.conn);
  });
});

// ─── openViaAttach tests ──────────────────────────────────────────────────────

describe("SessionStore openViaAttach", () => {
  it("opens a NEW live tab via the attach opener — never spawns (no worktree add)", async () => {
    // Opening a discovered LIVE session must ATTACH to the running tmux, not
    // spawn-first. Spawn-first back-compat-creates a duplicate worktree/branch
    // that leaks when tmux-new-session collides — the orphaned-session bug.
    const attachConn = fakeConn();
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => attachConn.conn),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openViaAttach(spec("p", "s"));
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(openers.respawn).not.toHaveBeenCalled();
    expect(openers.attach).toHaveBeenCalledTimes(1);
    expect(result).toEqual({ ok: true, attached: true, opened: true });
    const snap = store.getSnapshot();
    expect(snap.tabs).toHaveLength(1);
    expect(snap.tabs[0].status).toBe("live");
    expect(snap.tabs[0].connection).toBe(attachConn.conn);
    expect(snap.activeKey).toBe("p/s");
  });

  it("focuses an already-open tab without re-attaching", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(async () => makeConn()),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    await store.openViaAttach(spec("p", "s"));
    await store.openSession(spec("p", "other"));
    expect(store.getSnapshot().activeKey).toBe("p/other");
    const result = await store.openViaAttach(spec("p", "s"));
    expect(result).toEqual({ ok: true, attached: true, opened: false });
    expect(openers.attach).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot().activeKey).toBe("p/s");
  });

  it("falls back to respawn when attach reports sessionNotFound (died since poll)", async () => {
    // A live session can die between the discovery poll and the click; attach
    // then fails sessionNotFound. Rather than error, re-create it in the
    // surviving worktree via the respawn opener (never spawn — no worktree add).
    const respawnConn = fakeConn();
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(() =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      ),
      respawn: vi.fn(async () => respawnConn.conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openViaAttach(spec("p", "s"));
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(openers.respawn).toHaveBeenCalledTimes(1);
    expect(result).toEqual({ ok: true, attached: false, opened: true });
    const snap = store.getSnapshot();
    expect(snap.tabs).toHaveLength(1);
    expect(snap.tabs[0].status).toBe("live");
    expect(snap.tabs[0].connection).toBe(respawnConn.conn);
  });

  it("surfaces a non-not-found attach failure as {ok:false} without respawning", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({ connection: makeConn(), attached: false })),
      attach: vi.fn(() =>
        Promise.reject({ kind: "transport", message: "refused" }),
      ),
      respawn: vi.fn(async () => makeConn()),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store = new SessionStore(openers);
    const result = await store.openViaAttach(spec("p", "s"));
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatchObject({ kind: "transport" });
    expect(openers.respawn).not.toHaveBeenCalled();
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });
});

// ─── Fix D: additional coverage gaps ─────────────────────────────────────────

describe("SessionStore Fix D coverage", () => {
  // D1: reconnectAll skips stopped tabs
  it("D1: reconnectAll re-attaches a live tab and skips a stopped tab", async () => {
    const freshConn = fakeConn();
    let attachCalls = 0;
    const clock = fakeClock();

    const spawnedLive = fakeConn();
    const spawnedStopped = fakeConn();
    let spawnCall = 0;

    const openers: StoreOpeners = {
      spawn: () => {
        spawnCall += 1;
        const c = spawnCall === 1 ? spawnedLive : spawnedStopped;
        return Promise.resolve({ connection: c.conn, attached: false });
      },
      attach: (_p, sid) => {
        attachCalls += 1;
        if (sid === "stopped") {
          return Promise.reject({ kind: "sessionNotFound", message: "gone" });
        }
        return Promise.resolve(freshConn.conn);
      },
      respawn: () => Promise.resolve(fakeConn().conn),
      schedule: clock.schedule,
      stop: () => Promise.resolve(),
      remove: () => Promise.resolve(),
    };
    const store = new SessionStore(openers);
    await store.openSession({
      projectId: "p",
      sessionId: "live",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    await store.openSession({
      projectId: "p",
      sessionId: "stopped",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });

    // Drive the second tab to stopped via death → sessionNotFound
    spawnedStopped.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/stopped")?.status,
    ).toBe("stopped");

    // Reset attach call count so we only count reconnectAll's calls
    attachCalls = 0;

    await store.reconnectAll();

    // Only the live tab should have been re-attached
    expect(attachCalls).toBe(1);
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/live")?.connection,
    ).toBe(freshConn.conn);
    // The stopped tab must remain stopped and its attach was NOT called
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/stopped")?.status,
    ).toBe("stopped");
  });

  // D2: reconnectStale doesn't touch live tabs
  it("D2: reconnectStale retries a reconnecting tab and leaves a live tab untouched", async () => {
    const freshForReconnecting = fakeConn();
    const clock = fakeClock();
    // Track which session ids were attached
    const attachedIds: string[] = [];

    const spawnedRecon = fakeConn();
    const spawnedLive = fakeConn();
    let spawnCall = 0;

    // Fail on the first attach for "recon" (from the death handler), succeed on the second
    let reconAttachCalls = 0;

    const openers: StoreOpeners = {
      spawn: () => {
        spawnCall += 1;
        const c = spawnCall === 1 ? spawnedRecon : spawnedLive;
        return Promise.resolve({ connection: c.conn, attached: false });
      },
      attach: (_p, sid) => {
        attachedIds.push(sid);
        if (sid === "recon") {
          reconAttachCalls += 1;
          if (reconAttachCalls === 1)
            return Promise.reject({ kind: "transport", message: "down" });
          return Promise.resolve(freshForReconnecting.conn);
        }
        // Should never be called for the live tab
        return Promise.resolve(fakeConn().conn);
      },
      respawn: () => Promise.resolve(fakeConn().conn),
      schedule: clock.schedule,
      stop: () => Promise.resolve(),
      remove: () => Promise.resolve(),
    };

    const store = new SessionStore(openers);
    // Open reconnecting tab and drive it to reconnecting (transport fail → backoff)
    await store.openSession({
      projectId: "p",
      sessionId: "recon",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawnedRecon.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/recon")?.status,
    ).toBe("reconnecting");
    expect(clock.count()).toBe(1); // parked on backoff

    // Open a separate live tab
    await store.openSession({
      projectId: "p",
      sessionId: "live",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/live")?.status,
    ).toBe("live");

    const liveConnBefore = store
      .getSnapshot()
      .tabs.find((t) => t.key === "p/live")?.connection;

    // Clear the attach log before calling reconnectStale
    attachedIds.length = 0;

    // reconnectStale — should retry p/recon but NOT touch p/live
    store.reconnectStale();
    await Promise.resolve();
    await Promise.resolve();

    // p/recon should now be live
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/recon")?.status,
    ).toBe("live");
    // p/live connection unchanged
    expect(
      store.getSnapshot().tabs.find((t) => t.key === "p/live")?.connection,
    ).toBe(liveConnBefore);
    // attach was only called for "recon", not "live"
    expect(attachedIds).toEqual(["recon"]);
  });

  // D3: respawnTab happy path from stopped
  it("D3: respawnTab drives a stopped tab to live and re-arms death", async () => {
    const { store, spawned } = makeStore({
      attach: () =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
    });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");

    // Replace the respawn opener by creating a new store is not possible; instead
    // use makeStore with respawn override from the start.
    const respawnConn = fakeConn();
    const openers2: StoreOpeners = {
      spawn: vi.fn(async () => ({
        connection: spawned.conn,
        attached: false,
      })),
      attach: vi.fn(() =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      ),
      respawn: vi.fn(async () => respawnConn.conn),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const store2 = new SessionStore(openers2);
    // Provide a fresh spawn conn
    const initialConn = fakeConn();
    (openers2.spawn as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      connection: initialConn.conn,
      attached: false,
    });
    await store2.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    initialConn.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store2.getSnapshot().tabs[0].status).toBe("stopped");

    await store2.respawnTab("p/s");

    const tab = store2.getSnapshot().tabs[0];
    expect(tab.status).toBe("live");
    expect(tab.connection).toBe(respawnConn.conn);
    expect(openers2.respawn).toHaveBeenCalledTimes(1);

    // Death re-armed: firing the new conn's close → reconnecting
    respawnConn.die();
    expect(store2.getSnapshot().tabs[0].status).toBe("reconnecting");
  });

  // D4: respawnTab failure (uncancelled) → disconnected with error
  it("D4: respawnTab failure sets status to disconnected with cause", async () => {
    const openers: StoreOpeners = {
      spawn: vi.fn(async () => ({
        connection: fakeConn().conn,
        attached: false,
      })),
      attach: vi.fn(() =>
        Promise.reject({ kind: "sessionNotFound", message: "gone" }),
      ),
      respawn: vi.fn(async () => {
        throw { kind: "transport", message: "refused" };
      }),
      schedule: vi.fn(),
      stop: vi.fn(async () => {}),
      remove: vi.fn(async () => {}),
    };
    const initialConn = fakeConn();
    (openers.spawn as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      connection: initialConn.conn,
      attached: false,
    });
    const store = new SessionStore(openers);
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    initialConn.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");

    await store.respawnTab("p/s");

    const tab = store.getSnapshot().tabs[0];
    expect(tab.status).toBe("disconnected");
    expect(tab.error).toContain("refused");
  });

  // D5: backoff cap — delay for attempt >= 3 stays at BACKOFF_MS[3] = 8000
  it("D5: backoff delays are capped at BACKOFF_MS[3] = 8000 ms", async () => {
    const clock = fakeClock();
    const delays: number[] = [];

    const { store, spawned } = makeStore({
      attach: () => Promise.reject({ kind: "transport", message: "down" }),
      schedule: (fn, ms) => {
        delays.push(ms);
        clock.schedule(fn, ms);
      },
    });

    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();

    // Run 5 attempts (attempts 0–4): each fails with transport → schedules retry
    for (let i = 0; i < 5; i++) {
      await Promise.resolve();
      await Promise.resolve();
      clock.flush();
    }
    await Promise.resolve();
    await Promise.resolve();

    // We should have at least 4 scheduled delays recorded
    expect(delays.length).toBeGreaterThanOrEqual(4);
    // The delay for attempt >= 3 must be capped at 8000
    for (let i = 3; i < delays.length; i++) {
      expect(delays[i]).toBe(8000);
    }
  });

  // Task 10: stop/remove teardown actions

  it("stop sets the open tab to stopped and cancels reconnect", async () => {
    const { store } = makeStore({ stop: vi.fn().mockResolvedValue(undefined) });
    await store.openSession({
      projectId: "api",
      sessionId: "x",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    const r = await store.stop("api", "x");
    expect(r).toEqual({ ok: true });
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");
  });

  it("remove closes the tab on success", async () => {
    const { store } = makeStore({
      remove: vi.fn().mockResolvedValue(undefined),
    });
    await store.openSession({
      projectId: "api",
      sessionId: "x",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    const r = await store.remove("api", "x", false);
    expect(r).toEqual({ ok: true });
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("remove surfaces WorkspaceDirty for the force escalation", async () => {
    const dirty = {
      kind: "workspaceDirty",
      message: "x",
      reason: "uncommitted",
    };
    const { store } = makeStore({ remove: vi.fn().mockRejectedValue(dirty) });
    const r = await store.remove("api", "x", false);
    expect(r).toEqual({ ok: false, dirty: "uncommitted" });
  });

  it("remove is guarded against a double-fire", async () => {
    let resolve!: () => void;
    const remove = vi.fn().mockReturnValue(
      new Promise<void>((r) => {
        resolve = r;
      }),
    );
    const { store } = makeStore({ remove });
    const first = store.remove("api", "x", true);
    const second = await store.remove("api", "x", true); // in-flight → rejected
    expect(second).toEqual({ ok: false });
    resolve();
    await first;
    expect(remove).toHaveBeenCalledTimes(1);
  });

  // Background removal (#0 non-blocking delete): the snapshot publishes which
  // keys have a remove in flight so the sidebar can spin their rows.

  it("remove publishes the key in snapshot.removing while in flight and clears it on success", async () => {
    let resolveRemove!: () => void;
    const remove = vi.fn(
      () =>
        new Promise<void>((r) => {
          resolveRemove = r;
        }),
    );
    const { store } = makeStore({ remove });
    const p = store.remove("api", "x", false);
    expect(store.getSnapshot().removing).toEqual(["api/x"]);
    resolveRemove();
    await p;
    expect(store.getSnapshot().removing).toEqual([]);
  });

  it("remove clears snapshot.removing when the backend fails", async () => {
    let rejectRemove!: (e: unknown) => void;
    const remove = vi.fn(
      () =>
        new Promise<void>((_r, rej) => {
          rejectRemove = rej;
        }),
    );
    const { store } = makeStore({ remove });
    const p = store.remove("api", "x", false);
    expect(store.getSnapshot().removing).toEqual(["api/x"]);
    rejectRemove(new Error("boom"));
    const r = await p;
    expect(r.ok).toBe(false);
    expect(store.getSnapshot().removing).toEqual([]);
  });

  it("remove clears snapshot.removing on a WorkspaceDirty rejection", async () => {
    const dirty = {
      kind: "workspaceDirty",
      message: "x",
      reason: "uncommitted",
    };
    const { store } = makeStore({ remove: vi.fn().mockRejectedValue(dirty) });
    const r = await store.remove("api", "x", false);
    expect(r).toEqual({ ok: false, dirty: "uncommitted" });
    expect(store.getSnapshot().removing).toEqual([]);
  });

  it("stop does NOT appear in snapshot.removing (it is remove-only)", async () => {
    let resolveStop!: () => void;
    const stop = vi.fn(
      () =>
        new Promise<void>((r) => {
          resolveStop = r;
        }),
    );
    const { store } = makeStore({ stop });
    const p = store.stop("api", "x");
    expect(store.getSnapshot().removing).toEqual([]);
    resolveStop();
    await p;
  });

  // CROSS-RACE: open (respawn) vs teardown (remove). The bug: `pending`
  // guards opens against opens and `teardownPending` guards teardowns against
  // teardowns, but nothing serialized an open against a teardown. A remove that
  // killed tmux + deleted the worktree could interleave with a respawn that
  // re-created tmux in that worktree, leaving a live tmux with no worktree or
  // branch — an orphan that shows no Stop affordance and is hard to remove.
  it("RACE: respawnTab is refused while a remove is in-flight (no orphan respawn)", async () => {
    let resolveRemove!: () => void;
    const respawn = vi.fn(() => Promise.resolve(fakeConn().conn));
    const remove = vi.fn(
      () =>
        new Promise<void>((r) => {
          resolveRemove = r;
        }),
    );
    const { store } = makeStore({ respawn, remove });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    // Start a remove whose backend hangs → teardown is in flight for this key.
    const removeP = store.remove("p", "s", true);
    // While the remove runs, a stray respawn fires (the user clicks the row).
    await store.respawnTab("p/s");
    // It must NOT spawn a new session underneath the in-flight teardown.
    expect(respawn).not.toHaveBeenCalled();
    resolveRemove();
    await removeP;
  });

  it("RACE: remove is refused while a respawn is in-flight (won't tear down a session being created)", async () => {
    let resolveRespawn!: (c: SessionConnection) => void;
    const respawn = vi.fn(
      () =>
        new Promise<SessionConnection>((r) => {
          resolveRespawn = r;
        }),
    );
    const remove = vi.fn(() => Promise.resolve());
    const { store, spawned } = makeStore({ respawn, remove });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die(); // → reconnecting
    await Promise.resolve();
    // Drive a respawn whose backend hangs → open is in flight for this key.
    const respawnP = store.respawnTab("p/s");
    // A remove fires concurrently — it must back off, not kill the new session.
    const r = await store.remove("p", "s", true);
    expect(r).toEqual({ ok: false });
    expect(remove).not.toHaveBeenCalled();
    resolveRespawn(fakeConn().conn);
    await respawnP;
  });

  it("RACE: stop is refused while a respawn is in-flight", async () => {
    let resolveRespawn!: (c: SessionConnection) => void;
    const respawn = vi.fn(
      () =>
        new Promise<SessionConnection>((r) => {
          resolveRespawn = r;
        }),
    );
    const stop = vi.fn(() => Promise.resolve());
    const { store, spawned } = makeStore({ respawn, stop });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    const respawnP = store.respawnTab("p/s");
    const r = await store.stop("p", "s");
    expect(r).toEqual({ ok: false });
    expect(stop).not.toHaveBeenCalled();
    resolveRespawn(fakeConn().conn);
    await respawnP;
  });

  it("RACE: respawnTab is refused while a stop is in-flight", async () => {
    let resolveStop!: () => void;
    const respawn = vi.fn(() => Promise.resolve(fakeConn().conn));
    const stop = vi.fn(
      () =>
        new Promise<void>((r) => {
          resolveStop = r;
        }),
    );
    const { store } = makeStore({ respawn, stop });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    const stopP = store.stop("p", "s");
    await store.respawnTab("p/s");
    expect(respawn).not.toHaveBeenCalled();
    resolveStop();
    await stopP;
  });

  // The spawn-open path (openSession → pending) is guarded by the same busy()
  // lock as respawn; these two cover the open(spawn)-vs-teardown directions.
  it("RACE: remove is refused while an openSession (spawn) is in-flight", async () => {
    let resolveSpawn!: (v: {
      connection: SessionConnection;
      attached: boolean;
    }) => void;
    const spawn = vi.fn(
      () =>
        new Promise<{ connection: SessionConnection; attached: boolean }>(
          (r) => {
            resolveSpawn = r;
          },
        ),
    );
    const remove = vi.fn(() => Promise.resolve());
    const { store } = makeStore({ spawn, remove });
    // First open spawns a tab; second concurrent open holds `pending`.
    const openP = store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    const r = await store.remove("p", "s", true);
    expect(r).toEqual({ ok: false });
    expect(remove).not.toHaveBeenCalled();
    resolveSpawn({ connection: fakeConn().conn, attached: false });
    await openP;
  });

  it("RACE: openSession is refused while a teardown is in-flight", async () => {
    let resolveRemove!: () => void;
    const spawn = vi.fn(async () => ({
      connection: fakeConn().conn,
      attached: false,
    }));
    const remove = vi.fn(
      () =>
        new Promise<void>((r) => {
          resolveRemove = r;
        }),
    );
    const { store } = makeStore({ spawn, remove });
    // Tear down a key with no open tab (backend hangs → teardownPending held).
    // With no existing tab, openSession can't dedupe, so it reaches openTab's
    // busy() check — the spawn-open path's cross-guard against orphaning.
    const removeP = store.remove("p", "s", true);
    const r = await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    expect(r).toEqual({ ok: false, error: OPEN_CANCELLED });
    expect(spawn).not.toHaveBeenCalled();
    resolveRemove();
    await removeP;
  });

  it("RACE: two overlapping respawns keep the key busy until BOTH settle", async () => {
    let r1!: (c: SessionConnection) => void;
    let r2!: (c: SessionConnection) => void;
    let call = 0;
    const respawn = vi.fn(
      () =>
        new Promise<SessionConnection>((res) => {
          call += 1;
          if (call === 1) r1 = res;
          else r2 = res;
        }),
    );
    const remove = vi.fn(() => Promise.resolve());
    const { store, spawned } = makeStore({ respawn, remove });
    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    const p1 = store.respawnTab("p/s"); // respawning count = 1
    const p2 = store.respawnTab("p/s"); // count = 2 (newest-wins cancels p1)
    // Settle the first: count drops to 1, key still busy.
    r1(fakeConn().conn);
    await p1;
    expect(await store.remove("p", "s", true)).toEqual({ ok: false });
    expect(remove).not.toHaveBeenCalled();
    // Settle the second: count drains to 0, key free → remove now proceeds.
    r2(fakeConn().conn);
    await p2;
    expect(await store.remove("p", "s", true)).toEqual({ ok: true });
    expect(remove).toHaveBeenCalledTimes(1);
  });

  // D6: closeTab during backoff — cancelled retry is a no-op
  it("D6: closeTab during backoff cancels the scheduled retry", async () => {
    let attachCalls = 0;
    const clock = fakeClock();

    const { store, spawned } = makeStore({
      attach: () => {
        attachCalls += 1;
        return Promise.reject({ kind: "transport", message: "down" });
      },
      schedule: clock.schedule,
    });

    await store.openSession({
      projectId: "p",
      sessionId: "s",
      agent: null,
      base: null,
      workspace: "worktree" as const,
    });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    // Tab is reconnecting, one retry parked on the clock
    expect(store.getSnapshot().tabs[0].status).toBe("reconnecting");
    expect(clock.count()).toBe(1);

    // Close the tab while the retry is pending
    store.closeTab("p/s");
    expect(store.getSnapshot().tabs).toHaveLength(0);

    // Flush the clock — the cancelled retry must be a no-op
    const attachCallsBeforeFlush = attachCalls;
    clock.flush();
    await Promise.resolve();
    await Promise.resolve();

    // No additional attach attempt
    expect(attachCalls).toBe(attachCallsBeforeFlush);
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });
});
