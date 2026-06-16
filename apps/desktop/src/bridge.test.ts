import { beforeEach, describe, expect, it, vi } from "vitest";

const c = vi.hoisted(() => ({
  sessionList: vi.fn(),
  sessionSpawn: vi.fn(),
  sessionAttach: vi.fn(),
  sessionRespawn: vi.fn(),
  sessionWrite: vi.fn(),
  sessionResize: vi.fn(),
  sessionClose: vi.fn(),
}));

vi.mock("./bindings", () => ({ commands: c }));
vi.mock("@tauri-apps/api/core", () => ({
  Channel: class {
    onmessage: ((m: unknown) => void) | null = null;
  },
}));

import * as bridge from "./bridge";

beforeEach(() => {
  for (const f of Object.values(c)) f.mockReset();
});

describe("bridge.ts", () => {
  it("writeSession normalizes Uint8Array to number[]", async () => {
    c.sessionWrite.mockResolvedValue({ status: "ok", data: null });
    await bridge.writeSession(1 as never, new Uint8Array([1, 2, 3]));
    expect(c.sessionWrite).toHaveBeenCalledWith(1, [1, 2, 3]);
  });

  it("spawnSession wires onOutput and forwards bytes + closed events", async () => {
    c.sessionSpawn.mockImplementation(
      async (
        _p: unknown,
        _s: unknown,
        _a: unknown,
        ch: { onmessage: ((m: unknown) => void) | null },
      ) => {
        ch.onmessage?.({ event: "bytes", bytes: [104] });
        ch.onmessage?.({ event: "closed" });
        return { status: "ok", data: 1 };
      },
    );
    const seen: unknown[] = [];
    const h = await bridge.spawnSession("api", "x", null, (m) => seen.push(m));
    expect(h).toBe(1);
    expect(seen).toEqual([
      { event: "bytes", bytes: [104] },
      { event: "closed" },
    ]);
  });

  it("attachSession passes ids + wired channel and returns the handle", async () => {
    c.sessionAttach.mockImplementation(
      async (
        _p: unknown,
        _s: unknown,
        ch: { onmessage: ((m: unknown) => void) | null },
      ) => {
        ch.onmessage?.({ event: "bytes", bytes: [120] });
        return { status: "ok", data: 7 };
      },
    );
    const seen: unknown[] = [];
    const h = await bridge.attachSession("api", "x", (m) => seen.push(m));
    expect(h).toBe(7);
    expect(c.sessionAttach).toHaveBeenCalledWith("api", "x", expect.anything());
    expect(seen).toEqual([{ event: "bytes", bytes: [120] }]);
  });

  it("respawnSession passes ids and returns the handle", async () => {
    c.sessionRespawn.mockResolvedValue({ status: "ok", data: 3 });
    const h = await bridge.respawnSession("api", "x", () => {});
    expect(h).toBe(3);
    expect(c.sessionRespawn).toHaveBeenCalledWith(
      "api",
      "x",
      expect.anything(),
    );
  });

  it("resizeSession forwards rows and cols", async () => {
    c.sessionResize.mockResolvedValue({ status: "ok", data: null });
    await bridge.resizeSession(5 as never, 30, 100);
    expect(c.sessionResize).toHaveBeenCalledWith(5, 30, 100);
  });

  it("listSessions returns data on ok", async () => {
    c.sessionList.mockResolvedValue({ status: "ok", data: [] });
    expect(await bridge.listSessions()).toEqual([]);
  });

  it("unwrap throws the BridgeError on error", async () => {
    c.sessionClose.mockResolvedValue({
      status: "error",
      error: { kind: "unknownHandle" },
    });
    await expect(bridge.closeSession(1 as never)).rejects.toEqual({
      kind: "unknownHandle",
    });
  });
});
