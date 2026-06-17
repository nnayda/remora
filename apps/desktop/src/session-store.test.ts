import { describe, expect, it, vi } from "vitest";
import type { SessionConnection } from "./connection";
import type { OpenSession } from "./session-store";
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

// An opener that resolves immediately with a fresh connection.
function instantOpener(attached = false): OpenSession {
  return vi.fn(async () => ({ connection: makeConn(), attached }));
}

const spec = (p: string, s: string, a: string | null = null) => ({
  projectId: p,
  sessionId: s,
  agent: a,
});

describe("SessionStore", () => {
  it("opens a session as a focused tab", async () => {
    const store = new SessionStore(instantOpener());
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: false });
    const snap = store.getSnapshot();
    expect(snap.tabs.map((t) => t.key)).toEqual(["api/fix"]);
    expect(snap.activeKey).toBe("api/fix");
  });

  it("records attached:true from the opener", async () => {
    const store = new SessionStore(instantOpener(true));
    const result = await store.openSession(spec("api", "fix"));
    expect(result).toEqual({ ok: true, attached: true });
    expect(store.getSnapshot().tabs[0].attached).toBe(true);
  });

  it("dedupes: opening an existing key focuses it, no second open", async () => {
    const opener = instantOpener();
    const store = new SessionStore(opener);
    await store.openSession(spec("api", "fix"));
    await store.openSession(spec("web", "ui"));
    store.focusTab("api/fix"); // move focus away from web/ui... actually focus web
    const result = await store.openSession(spec("web", "ui", "other-agent"));
    expect(result).toEqual({ ok: true, attached: false });
    expect(opener).toHaveBeenCalledTimes(2); // not 3
    expect(store.getSnapshot().tabs).toHaveLength(2);
    expect(store.getSnapshot().activeKey).toBe("web/ui");
  });

  it("closeTab detaches the connection, removes the tab, focuses a neighbour", async () => {
    const conns: SessionConnection[] = [];
    const opener: OpenSession = vi.fn(async () => {
      const connection = makeConn();
      conns.push(connection);
      return { connection, attached: false };
    });
    const store = new SessionStore(opener);
    await store.openSession(spec("a", "1"));
    await store.openSession(spec("b", "2")); // active = b/2
    store.closeTab("b/2");
    expect(conns[1].close).toHaveBeenCalledTimes(1);
    const snap = store.getSnapshot();
    expect(snap.tabs.map((t) => t.key)).toEqual(["a/1"]);
    expect(snap.activeKey).toBe("a/1"); // re-focused the neighbour
  });

  it("closing the last tab returns to the empty state", async () => {
    const store = new SessionStore(instantOpener());
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
    const opener: OpenSession = vi.fn(
      () =>
        new Promise<{ connection: SessionConnection; attached: boolean }>(
          (res) => {
            resolve = res;
          },
        ),
    );
    const store = new SessionStore(opener);
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
    const opener: OpenSession = vi.fn(() => {
      call += 1;
      if (call === 1)
        return Promise.resolve({ connection: openConn, attached: false });
      return new Promise<{ connection: SessionConnection; attached: boolean }>(
        (res) => {
          resolve = res;
        },
      );
    });
    const store = new SessionStore(opener);
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
    const opener: OpenSession = vi.fn(async () => {
      throw { kind: "transport", message: "net" };
    });
    const store = new SessionStore(opener);
    const result = await store.openSession(spec("a", "1"));
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatchObject({ kind: "transport" });
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });

  it("notifies subscribers on change and tabKey formats the key", async () => {
    const store = new SessionStore(instantOpener());
    const listener = vi.fn();
    const unsub = store.subscribe(listener);
    await store.openSession(spec("a", "1"));
    expect(listener).toHaveBeenCalled();
    unsub();
    expect(tabKey("a", "1")).toBe("a/1");
  });

  it("unsubscribed listener is not called on later changes", async () => {
    const store = new SessionStore(instantOpener());
    const listener = vi.fn();
    const unsub = store.subscribe(listener);
    await store.openSession(spec("a", "1"));
    const callsBeforeUnsub = listener.mock.calls.length;
    unsub();
    await store.openSession(spec("b", "2"));
    expect(listener).toHaveBeenCalledTimes(callsBeforeUnsub);
  });

  it("closeTab of a non-active tab leaves activeKey unchanged", async () => {
    const store = new SessionStore(instantOpener());
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
    const opener: OpenSession = vi.fn(async () => ({
      connection: makeConn(),
      attached: false,
    }));
    const store = new SessionStore(opener);
    store.dispose();
    const result = await store.openSession(spec("a", "1"));
    expect(result).toEqual({ ok: false, error: OPEN_CANCELLED });
    expect(opener).not.toHaveBeenCalled();
    expect(store.getSnapshot().tabs).toHaveLength(0);
  });
});
