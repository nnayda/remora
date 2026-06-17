import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConfigDto, SessionMetaDto } from "./bindings";
import { type DiscoveryDeps, DiscoveryStore } from "./discovery-store";

const emptyConfig: ConfigDto = { hosts: [], projects: [] };
const session = (sessionId: string): SessionMetaDto => ({
  projectId: "api",
  sessionId,
  state: "live",
  agent: null,
  createdAt: null,
  workspacePath: null,
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
    listSessions: vi.fn(async () => [] as SessionMetaDto[]),
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
        }),
      ),
      listSessions: vi.fn(async () => [session("fix")]),
    });
    await store.start();
    expect(loadConfig).toHaveBeenCalledTimes(1);
    expect(listSessions).toHaveBeenCalledTimes(1);
    expect(store.getSnapshot().config.hosts).toHaveLength(1);
    expect(store.getSnapshot().sessions).toHaveLength(1);
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
    const d = deferred<SessionMetaDto[]>();
    let calls = 0;
    // Initial poll resolves; the second poll (first interval tick) hangs.
    const listSessions = vi.fn(async () => {
      calls++;
      return calls === 1 ? [] : d.promise;
    });
    const { store } = makeStore({ listSessions });
    await store.start(); // call 1 resolves
    await vi.advanceTimersByTimeAsync(1000); // call 2 begins, never resolves yet
    await vi.advanceTimersByTimeAsync(1000); // tick while in-flight → skipped
    await vi.advanceTimersByTimeAsync(1000); // tick while in-flight → skipped
    expect(listSessions).toHaveBeenCalledTimes(2); // guard held
    d.resolve([session("fix")]);
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
      listSessions: vi.fn(async () => [session("fix")]),
    });
    await store.start();
    expect(store.getSnapshot().configError).toBe("bad toml");
    expect(store.getSnapshot().sessions).toHaveLength(1);
    store.stop();
  });

  it("keeps the last good sessions and flags discovery on a list() failure, clearing on recovery", async () => {
    let ok = true;
    const listSessions = vi.fn(async () => {
      if (ok) return [session("fix")];
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
    const d = deferred<SessionMetaDto[]>();
    const listSessions = vi.fn(async () => d.promise);
    const { store } = makeStore({ listSessions });
    const started = store.start();
    store.stop(); // dispose before the in-flight list resolves
    d.resolve([session("late")]);
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
