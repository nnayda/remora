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
