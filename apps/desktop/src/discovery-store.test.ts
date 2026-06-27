import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConfigDto, SessionListDto, SessionMetaDto } from "./bindings";
import {
  type DiscoveryDeps,
  DiscoveryStore,
  RECONNECT_GRACE_MS,
} from "./discovery-store";

const emptyConfig: ConfigDto = { hosts: [], projects: [], agents: [] };

const session = (projectId: string, sessionId: string): SessionMetaDto => ({
  projectId,
  sessionId,
  state: "live",
  agent: null,
  createdAt: null,
  workspacePath: null,
  workspace: null,
  branch: null,
});

/** Build a host-grouped poll result. */
const listResult = (
  hosts: { hostId: string; available?: boolean; sessions?: SessionMetaDto[] }[],
): SessionListDto => ({
  hosts: hosts.map((h) => ({
    hostId: h.hostId,
    available: h.available ?? true,
    sessions: h.sessions ?? [],
  })),
});

/** A promise whose resolution the test controls, to model in-flight/late results. */
function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function makeStore(overrides: Partial<DiscoveryDeps> = {}) {
  const deps: DiscoveryDeps = {
    loadConfig: vi.fn(async () => emptyConfig),
    listSessions: vi.fn(async () => listResult([])),
    intervalMs: 1000,
    ...overrides,
  };
  // Return the effective mocks (an override replaces the default).
  return {
    store: new DiscoveryStore(deps),
    loadConfig: deps.loadConfig as ReturnType<typeof vi.fn>,
    listSessions: deps.listSessions as ReturnType<typeof vi.fn>,
  };
}

describe("DiscoveryStore", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("loads config + sessions on start", async () => {
    const { store, loadConfig, listSessions } = makeStore({
      loadConfig: vi.fn(
        async (): Promise<ConfigDto> => ({
          hosts: [{ id: "h", name: null, transport: "ssh" }],
          projects: [],
          agents: [],
        }),
      ),
      listSessions: vi.fn(async () =>
        listResult([{ hostId: "h", sessions: [session("api", "fix")] }]),
      ),
    });
    await store.start();
    expect(loadConfig).toHaveBeenCalledTimes(1);
    expect(listSessions).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot().config.hosts).toHaveLength(1);
    expect(store.getSnapshot().sessions).toHaveLength(1);
    store.stop();
  });

  it("clamps a non-positive intervalMs to the 4s default (no tight loop)", async () => {
    const { store, listSessions } = makeStore({ intervalMs: 0 });
    await store.start();
    expect(listSessions).toHaveBeenCalledTimes(1);
    // A 0ms interval would have fired many times here; the clamp holds it to 4s.
    await vi.advanceTimersByTimeAsync(3999);
    expect(listSessions).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(listSessions).toHaveBeenCalledTimes(2);
    store.stop();
  });

  it("re-lists sessions on each interval tick", async () => {
    const { store, listSessions } = makeStore();
    await store.start();
    expect(listSessions).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(listSessions).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1000);
    expect(listSessions).toHaveBeenCalledTimes(3);
    store.stop();
  });

  it("skips an interval tick while a previous list() is still in flight (D3)", async () => {
    const d = deferred<SessionListDto>();
    let calls = 0;
    // Initial poll resolves; the second poll (first interval tick) hangs.
    const listSessions = vi.fn(async () => {
      calls++;
      return calls === 1 ? listResult([]) : d.promise;
    });
    const { store } = makeStore({ listSessions });
    await store.start(); // call 1 resolves
    await vi.advanceTimersByTimeAsync(1000); // call 2 begins, never resolves yet
    await vi.advanceTimersByTimeAsync(1000); // tick while in-flight → skipped
    await vi.advanceTimersByTimeAsync(1000); // tick while in-flight → skipped
    expect(listSessions).toHaveBeenCalledTimes(2); // guard held
    d.resolve(listResult([{ hostId: "h", sessions: [session("api", "fix")] }]));
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(1000); // now free → polls again
    expect(listSessions).toHaveBeenCalledTimes(3);
    store.stop();
  });

  it("pauses polling when inactive and resumes (with immediate refresh) when active (D7)", async () => {
    const { store, listSessions } = makeStore();
    await store.start();
    expect(listSessions).toHaveBeenCalledTimes(1);
    store.setActive(false);
    await vi.advanceTimersByTimeAsync(5000); // hidden → no polls
    expect(listSessions).toHaveBeenCalledTimes(1);
    store.setActive(true); // resume → immediate refresh
    await Promise.resolve();
    expect(listSessions).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1000); // interval running again
    expect(listSessions).toHaveBeenCalledTimes(3);
    store.stop();
  });

  it("surfaces a config error but still loads sessions", async () => {
    const { store } = makeStore({
      loadConfig: vi.fn(async () => {
        throw { kind: "config", message: "bad toml" };
      }),
      listSessions: vi.fn(async () =>
        listResult([{ hostId: "h", sessions: [session("api", "fix")] }]),
      ),
    });
    await store.start();
    expect(store.getSnapshot().configError).toBe("bad toml");
    expect(store.getSnapshot().sessions).toHaveLength(1);
    store.stop();
  });

  it("keeps the last good sessions and flags discovery on a list() failure, clearing on recovery", async () => {
    let ok = true;
    const listSessions = vi.fn(async (): Promise<SessionListDto> => {
      if (ok)
        return listResult([{ hostId: "h", sessions: [session("api", "fix")] }]);
      throw { kind: "transport", message: "down" };
    });
    const { store } = makeStore({ listSessions });
    await store.start();
    expect(store.getSnapshot().sessions).toHaveLength(1);
    ok = false;
    await vi.advanceTimersByTimeAsync(1000);
    expect(store.getSnapshot().discoveryUnavailable).toBe(true);
    expect(store.getSnapshot().sessions).toHaveLength(1); // last good kept
    ok = true;
    await vi.advanceTimersByTimeAsync(1000);
    expect(store.getSnapshot().discoveryUnavailable).toBe(false);
    store.stop();
  });

  it("ignores a list() result that resolves after stop()", async () => {
    const d = deferred<SessionListDto>();
    const listSessions = vi.fn(async () => d.promise);
    const { store } = makeStore({ listSessions });
    const started = store.start();
    store.stop(); // dispose before the in-flight list resolves
    d.resolve(
      listResult([{ hostId: "h", sessions: [session("api", "late")] }]),
    );
    await started;
    await Promise.resolve();
    expect(store.getSnapshot().sessions).toHaveLength(0);
  });

  it("manual refresh re-reads config and sessions", async () => {
    const { store, loadConfig, listSessions } = makeStore();
    await store.start();
    expect(loadConfig).toHaveBeenCalledTimes(1);
    await store.refresh();
    expect(loadConfig).toHaveBeenCalledTimes(2);
    expect(listSessions).toHaveBeenCalledTimes(2);
    store.stop();
  });

  it("refreshAfterOpen re-lists sessions without re-reading config", async () => {
    const { store, loadConfig, listSessions } = makeStore();
    await store.start();
    await store.refreshAfterOpen();
    expect(loadConfig).toHaveBeenCalledTimes(1); // config untouched
    expect(listSessions).toHaveBeenCalledTimes(2);
    store.stop();
  });
});

describe("DiscoveryStore per-host retention", () => {
  let clock = 0;
  const now = () => clock;
  beforeEach(() => {
    clock = 0;
    vi.useFakeTimers();
  });
  afterEach(() => vi.useRealTimers());

  const a1 = session("a", "1");
  const b1 = session("b", "2");

  it("retains a transiently-down host's rows and flags them reconnecting", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(
        listResult([
          { hostId: "A", sessions: [a1] },
          { hostId: "B", sessions: [b1] },
        ]),
      )
      .mockResolvedValueOnce(
        listResult([
          { hostId: "A", sessions: [a1] },
          { hostId: "B", available: false },
        ]),
      );
    const { store } = makeStore({ listSessions: list, now });
    await store.start();
    await store.refreshAfterOpen(); // second poll: B down
    const snap = store.getSnapshot();
    expect(snap.sessions).toEqual(expect.arrayContaining([a1, b1])); // B retained
    expect(snap.reconnectingKeys).toEqual(["b/2"]);
    expect(snap.discoveryUnavailable).toBe(false);
  });

  it("prunes a host only after the grace window since first detection (anchor preserved across down polls)", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }])) // up
      .mockResolvedValue(listResult([{ hostId: "B", available: false }])); // down thereafter
    const { store } = makeStore({ listSessions: list, now });
    await store.start(); // clock 0: B up
    clock += 5000;
    await store.refreshAfterOpen(); // first down poll: detection anchored at 5000
    expect(store.getSnapshot().sessions).toEqual([b1]); // retained (within grace)
    expect(store.getSnapshot().reconnectingKeys).toEqual(["b/2"]);
    clock += 5000; // clock=10000: 5s since detection — still within grace
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([b1]); // anchor not reset by the 2nd down poll
    clock += RECONNECT_GRACE_MS; // clock=25000: 20s since the 5000 detection (> grace)
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([]); // pruned
    expect(store.getSnapshot().reconnectingKeys).toEqual([]);
  });

  it("retains a host that has been down for exactly the grace window (boundary)", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }]))
      .mockResolvedValue(listResult([{ hostId: "B", available: false }]));
    const { store } = makeStore({ listSessions: list, now });
    await store.start(); // up at clock 0
    await store.refreshAfterOpen(); // down: detection anchored at 0
    clock += RECONNECT_GRACE_MS; // exactly the window — prune is strictly `>`
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([b1]); // still retained at the boundary
    expect(store.getSnapshot().reconnectingKeys).toEqual(["b/2"]);
  });

  it("clears the reconnecting flag when a down host recovers", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }])) // up
      .mockResolvedValueOnce(listResult([{ hostId: "B", available: false }])) // down
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }])); // recovered
    const { store } = makeStore({ listSessions: list, now });
    await store.start();
    await store.refreshAfterOpen(); // down → reconnecting
    expect(store.getSnapshot().reconnectingKeys).toEqual(["b/2"]);
    await store.refreshAfterOpen(); // recovered → authoritative, flag cleared
    expect(store.getSnapshot().reconnectingKeys).toEqual([]);
    expect(store.getSnapshot().sessions).toEqual([b1]);
  });

  it("[CRITICAL] clears rows when a reachable host returns zero sessions", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "A", sessions: [a1] }]))
      .mockResolvedValueOnce(listResult([{ hostId: "A", sessions: [] }]));
    const { store } = makeStore({ listSessions: list, now });
    await store.start();
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([]); // not over-retained
    expect(store.getSnapshot().reconnectingKeys).toEqual([]);
  });

  it("[Finding 1] drops rows for a host removed from config", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(
        listResult([
          { hostId: "A", sessions: [a1] },
          { hostId: "B", sessions: [b1] },
        ]),
      )
      .mockResolvedValueOnce(listResult([{ hostId: "A", sessions: [a1] }])); // B gone
    const { store } = makeStore({ listSessions: list, now });
    await store.start();
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([a1]); // B not flattened forever
  });

  it("[Finding 3] does not re-arm reconnecting for an already-pruned dead host", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValue(listResult([{ hostId: "B", available: false }]));
    const { store } = makeStore({ listSessions: list, now });
    await store.start(); // B never had rows
    await store.refreshAfterOpen();
    expect(store.getSnapshot().reconnectingKeys).toEqual([]);
  });

  it("all hosts down throws → discoveryUnavailable, rows retained", async () => {
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "A", sessions: [a1] }]))
      .mockRejectedValueOnce(new Error("all hosts unreachable"));
    const { store } = makeStore({ listSessions: list, now });
    await store.start();
    await store.refreshAfterOpen();
    expect(store.getSnapshot().sessions).toEqual([a1]);
    expect(store.getSnapshot().discoveryUnavailable).toBe(true);
  });

  it("[Finding 9] reachable-before-hide host gets a fresh grace window on resume", async () => {
    // B was reachable when the window was hidden, so its window never started.
    // After a gap longer than the grace period, B is now down. Detection-time
    // anchoring means the first post-resume poll anchors `since` at resume time,
    // so B is retained (no prune-then-reappear flicker).
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }])) // start: B reachable
      .mockResolvedValueOnce(listResult([{ hostId: "B", available: false }])); // resume poll: B down
    const { store } = makeStore({ listSessions: list, now });
    await store.start(); // clock=0: B up, unavailableSince=null
    store.setActive(false);
    clock += RECONNECT_GRACE_MS + 1000; // long hidden gap
    store.setActive(true); // fires resume poll; B's window starts now
    await Promise.resolve(); // let pollSessions reach await listSessions()
    await Promise.resolve(); // let mergeHosts + commit run
    const snap = store.getSnapshot();
    expect(snap.sessions).toEqual([b1]); // B retained (fresh grace window)
    expect(snap.reconnectingKeys).toEqual(["b/2"]); // flagged reconnecting
    store.stop();
  });

  it("[Finding 9] already-failing-before-hide host also keeps its grace window on resume", async () => {
    // B was already down (unavailableSince set) when the window was hidden.
    // The resume guard resets unavailableSince on resume so a long hidden gap
    // restarts the grace window rather than pruning B on the first resume poll.
    const list = vi
      .fn<() => Promise<SessionListDto>>()
      .mockResolvedValueOnce(listResult([{ hostId: "B", sessions: [b1] }])) // start: B reachable
      .mockResolvedValueOnce(listResult([{ hostId: "B", available: false }])) // before hide: B down
      .mockResolvedValueOnce(listResult([{ hostId: "B", available: false }])); // resume poll: B still down
    const { store } = makeStore({ listSessions: list, now });
    await store.start(); // clock=0
    clock += 1000;
    await store.refreshAfterOpen(); // B goes down; unavailableSince anchored at 1000
    store.setActive(false);
    clock += RECONNECT_GRACE_MS + 1000; // total gap well past grace
    store.setActive(true); // resets unavailableSince=clock, fires resume poll
    await Promise.resolve();
    await Promise.resolve();
    const snap = store.getSnapshot();
    expect(snap.sessions).toEqual([b1]); // B retained
    expect(snap.reconnectingKeys).toEqual(["b/2"]);
    store.stop();
  });
});
