import { describe, expect, it, vi } from "vitest";
import type { SessionConnection } from "./connection";
import type { StoreOpeners } from "./session-store";
import { OPEN_CANCELLED, SessionStore, tabKey } from "./session-store";

function makeConn(): SessionConnection {
  return {
    subscribe: () => () => {},
    onClose: () => () => {},
    write: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    close: vi.fn(async () => {}),
    closed: false,
  };
}

// An opener set that spawns immediately with a fresh connection.
function instantOpeners(attached = false): StoreOpeners {
  return {
    spawn: vi.fn(async () => ({ connection: makeConn(), attached })),
    attach: vi.fn(async () => makeConn()),
    respawn: vi.fn(async () => makeConn()),
    schedule: vi.fn(),
  };
}

const spec = (p: string, s: string, a: string | null = null) => ({
  projectId: p,
  sessionId: s,
  agent: a,
});

describe("SessionStore", () => {
  it("opens a session as a focused tab", async () => {
    const store = new SessionStore(instantOpeners());
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: false });
    const snap = store.getSnapshot();
    expect(snap.tabs.map((t) => t.key)).toEqual(["api/fix"]);
    expect(snap.activeKey).toBe("api/fix");
  });

  it("records attached:true from the opener", async () => {
    const store = new SessionStore(instantOpeners(true));
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: true });
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
    expect(result).toEqual({ ok: true, attached: false });
    expect(openers.spawn).toHaveBeenCalledTimes(2); // not 3
    expect(store.getSnapshot().tabs).toHaveLength(2);
    expect(store.getSnapshot().activeKey).toBe("web/ui");
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
    };
    const store = new SessionStore(openers);
    store.dispose();
    const result = await store.openSession(spec("a", "1"));
    expect(result).toEqual({ ok: false, error: OPEN_CANCELLED });
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });
});

// ─── Reconnect state machine tests ───────────────────────────────────────────

// Minimal fake connection that lets the test drive death + capture close().
function fakeConn() {
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

function makeStore(overrides: Partial<StoreOpeners> = {}) {
  const spawned = fakeConn();
  const clock = fakeClock();
  const openers: StoreOpeners = {
    spawn: () => Promise.resolve({ connection: spawned.conn, attached: false }),
    attach: () => Promise.resolve(fakeConn().conn),
    respawn: () => Promise.resolve(fakeConn().conn),
    schedule: clock.schedule,
    ...overrides,
  };
  return { store: new SessionStore(openers), clock, spawned };
}

describe("SessionStore reconnect machine", () => {
  it("opens a tab as live", async () => {
    const { store } = makeStore();
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
    expect(store.getSnapshot().tabs[0].status).toBe("live");
  });

  it("death → reconnecting → live after a successful re-attach", async () => {
    const fresh = fakeConn();
    const { store, spawned } = makeStore({
      attach: () => Promise.resolve(fresh.conn),
    });
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
    spawned.die();
    await Promise.resolve();
    await Promise.resolve();
    expect(store.getSnapshot().tabs[0].status).toBe("stopped");
  });

  it("death → disconnected with cause on a config error (no retry loop)", async () => {
    const { store, spawned, clock } = makeStore({
      attach: () =>
        Promise.reject({ kind: "config", message: "unknown host hermes" }),
    });
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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

    await store.openSession({ projectId: "p", sessionId: "s", agent: null });

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

    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
});
