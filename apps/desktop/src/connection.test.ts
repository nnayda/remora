import { beforeEach, describe, expect, it, vi } from "vitest";

const b = vi.hoisted(() => ({
  attachSession: vi.fn(),
  spawnSession: vi.fn(),
  writeSession: vi.fn(),
  resizeSession: vi.fn(),
  closeSession: vi.fn(),
}));

vi.mock("./bridge", () => ({
  attachSession: b.attachSession,
  spawnSession: b.spawnSession,
  writeSession: b.writeSession,
  resizeSession: b.resizeSession,
  closeSession: b.closeSession,
}));

import type { BridgeOutput, OnOutput } from "./bridge";
import { openConnection } from "./connection";

beforeEach(() => {
  for (const f of Object.values(b)) f.mockReset();
});

// Open a connection whose internal OnOutput we capture so the test can drive
// output, simulating the bridge pushing PTY bytes.
async function open(): Promise<{
  conn: Awaited<ReturnType<typeof openConnection>>;
  emit: OnOutput;
}> {
  let emit!: OnOutput;
  b.spawnSession.mockImplementation(async (_p, _s, _a, onOutput: OnOutput) => {
    emit = onOutput;
    return 7;
  });
  const conn = await openConnection((o) => b.spawnSession("p", "s", null, o));
  return { conn, emit };
}

describe("connection.ts — openConnection", () => {
  it("buffers output before subscribe, then replays it in order", async () => {
    const { conn, emit } = await open();
    emit({ event: "bytes", bytes: [1] });
    emit({ event: "bytes", bytes: [2] });
    const seen: BridgeOutput[] = [];
    conn.subscribe((m) => seen.push(m));
    expect(seen).toEqual([
      { event: "bytes", bytes: [1] },
      { event: "bytes", bytes: [2] },
    ]);
  });

  it("forwards live output to the subscriber and flips closed", async () => {
    const { conn, emit } = await open();
    const seen: BridgeOutput[] = [];
    conn.subscribe((m) => seen.push(m));
    emit({ event: "bytes", bytes: [9] });
    expect(seen).toEqual([{ event: "bytes", bytes: [9] }]);
    expect(conn.closed).toBe(false);
    emit({ event: "closed" });
    expect(conn.closed).toBe(true);
    expect(seen).toContainEqual({ event: "closed" });
  });

  it("delegates write/resize/close to the bridge with the stored handle", async () => {
    b.spawnSession.mockResolvedValue(7);
    b.writeSession.mockResolvedValue(undefined);
    b.resizeSession.mockResolvedValue(undefined);
    b.closeSession.mockResolvedValue(undefined);
    const conn = await openConnection((o) => b.spawnSession("p", "s", null, o));
    await conn.write(new Uint8Array([104]));
    await conn.resize(24, 80);
    await conn.close();
    expect(b.writeSession).toHaveBeenCalledWith(7, new Uint8Array([104]));
    expect(b.resizeSession).toHaveBeenCalledWith(7, 24, 80);
    expect(b.closeSession).toHaveBeenCalledWith(7);
  });

  it("unsubscribe stops delivery; a later subscribe replays buffered output", async () => {
    const { conn, emit } = await open();
    const first: BridgeOutput[] = [];
    const unsub = conn.subscribe((m) => first.push(m));
    emit({ event: "bytes", bytes: [1] });
    unsub();
    emit({ event: "bytes", bytes: [2] }); // no subscriber -> buffered
    const second: BridgeOutput[] = [];
    conn.subscribe((m) => second.push(m));
    expect(first).toEqual([{ event: "bytes", bytes: [1] }]);
    expect(second).toEqual([{ event: "bytes", bytes: [2] }]);
  });
});
