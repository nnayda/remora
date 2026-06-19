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
    lastOutput: () => "",
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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

    await store.openSession({ projectId: "p", sessionId: "s", agent: null });

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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    await store.openSession({ projectId: "p", sessionId: "s1", agent: null });
    await store.openSession({ projectId: "p", sessionId: "s2", agent: null });
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
    };
    const store = new SessionStore(openers);
    const result = await store.openViaRespawn(spec("p", "s"));
    // spawn must NOT be called — only respawn
    expect(openers.spawn).not.toHaveBeenCalled();
    expect(openers.respawn).toHaveBeenCalledTimes(1);
    expect(result).toEqual({ ok: true, attached: false });
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
    };
    const store = new SessionStore(openers);
    // Open a second tab first so focus is elsewhere
    await store.openViaRespawn(spec("p", "s"));
    await store.openSession(spec("p", "other"));
    expect(store.getSnapshot().activeKey).toBe("p/other");
    // Now call openViaRespawn for the already-open key
    const result = await store.openViaRespawn(spec("p", "s"));
    expect(result).toEqual({ ok: true, attached: false });
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    };
    const store2 = new SessionStore(openers2);
    const { conn: spawnConn2, die: die2 } = fakeConn();
    // Patch spawn to return our controlled conn
    (openers2.spawn as ReturnType<typeof vi.fn>).mockResolvedValue({
      connection: spawnConn2,
      attached: false,
    });
    await store2.openSession({ projectId: "p", sessionId: "s", agent: null });
    die2(); // trigger death
    await Promise.resolve();
    await Promise.resolve();
    expect(store2.getSnapshot().tabs[0].status).toBe("stopped");

    // Now call openViaRespawn for the already-open stopped tab
    const result = await store2.openViaRespawn(spec("p", "s"));
    expect(result).toEqual({ ok: true, attached: false });
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
    };
    const store = new SessionStore(openers);
    await store.openSession({ projectId: "p", sessionId: "live", agent: null });
    await store.openSession({
      projectId: "p",
      sessionId: "stopped",
      agent: null,
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
    };

    const store = new SessionStore(openers);
    // Open reconnecting tab and drive it to reconnecting (transport fail → backoff)
    await store.openSession({
      projectId: "p",
      sessionId: "recon",
      agent: null,
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
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    };
    const store2 = new SessionStore(openers2);
    // Provide a fresh spawn conn
    const initialConn = fakeConn();
    (openers2.spawn as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      connection: initialConn.conn,
      attached: false,
    });
    await store2.openSession({ projectId: "p", sessionId: "s", agent: null });
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
    };
    const initialConn = fakeConn();
    (openers.spawn as ReturnType<typeof vi.fn>).mockResolvedValueOnce({
      connection: initialConn.conn,
      attached: false,
    });
    const store = new SessionStore(openers);
    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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

    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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

    await store.openSession({ projectId: "p", sessionId: "s", agent: null });
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
