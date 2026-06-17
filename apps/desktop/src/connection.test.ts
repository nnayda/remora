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
import {
  connectSession,
  isSessionExists,
  isSessionNotFound,
  openConnection,
  openSession,
} from "./connection";

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

  it("reports closed=true when the closed event arrives before subscribe", async () => {
    const { conn, emit } = await open();
    emit({ event: "closed" });
    expect(conn.closed).toBe(true);
    const seen: BridgeOutput[] = [];
    conn.subscribe((m) => seen.push(m));
    expect(seen).toContainEqual({ event: "closed" });
  });
});

describe("connection.ts — connectSession ladder", () => {
  it("attaches when the session already exists", async () => {
    b.attachSession.mockResolvedValue(1);
    await connectSession("p", "s", null);
    expect(b.attachSession).toHaveBeenCalledTimes(1);
    expect(b.spawnSession).not.toHaveBeenCalled();
  });

  it("spawns when attach reports the session is not found", async () => {
    b.attachSession.mockRejectedValueOnce({
      kind: "sessionNotFound",
      message: "x",
    });
    b.spawnSession.mockResolvedValue(2);
    await connectSession("p", "s", "claude");
    expect(b.attachSession).toHaveBeenCalledTimes(1);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.spawnSession).toHaveBeenCalledWith(
      "p",
      "s",
      "claude",
      expect.anything(),
    );
  });

  it("falls back to attach when a racing spawn reports the session exists", async () => {
    b.attachSession
      .mockRejectedValueOnce({ kind: "sessionNotFound" }) // first attach: not there yet
      .mockResolvedValueOnce(3); // fallback attach succeeds
    b.spawnSession.mockRejectedValueOnce({ kind: "sessionExists" });
    await connectSession("p", "s", null);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.attachSession).toHaveBeenCalledTimes(2);
  });

  it("rethrows unexpected bridge errors", async () => {
    b.attachSession.mockRejectedValueOnce({
      kind: "transport",
      message: "boom",
    });
    await expect(connectSession("p", "s", null)).rejects.toMatchObject({
      kind: "transport",
    });
  });

  it("rethrows an unexpected error from the spawn leg", async () => {
    b.attachSession.mockRejectedValueOnce({ kind: "sessionNotFound" });
    b.spawnSession.mockRejectedValueOnce({ kind: "transport", message: "net" });
    await expect(connectSession("p", "s", null)).rejects.toMatchObject({
      kind: "transport",
    });
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.attachSession).toHaveBeenCalledTimes(1);
  });

  it("error guards match the BridgeError kinds", () => {
    expect(isSessionNotFound({ kind: "sessionNotFound" })).toBe(true);
    expect(isSessionExists({ kind: "sessionExists" })).toBe(true);
    expect(isSessionNotFound({ kind: "sessionExists" })).toBe(false);
    expect(isSessionExists(new Error("x"))).toBe(false);
  });
});

describe("connection.ts — openSession (spawn-first)", () => {
  it("spawns a fresh session and reports attached:false", async () => {
    b.spawnSession.mockResolvedValue(5);
    const { connection, attached } = await openSession("p", "s", "claude");
    expect(attached).toBe(false);
    expect(connection.closed).toBe(false);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.spawnSession).toHaveBeenCalledWith(
      "p",
      "s",
      "claude",
      expect.anything(),
    );
    expect(b.attachSession).not.toHaveBeenCalled();
  });

  it("attaches and reports attached:true when the session already exists", async () => {
    b.spawnSession.mockRejectedValueOnce({ kind: "sessionExists" });
    b.attachSession.mockResolvedValue(6);
    const { attached } = await openSession("p", "s", null);
    expect(attached).toBe(true);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.attachSession).toHaveBeenCalledTimes(1);
  });

  it("propagates unexpected spawn errors", async () => {
    b.spawnSession.mockRejectedValueOnce({ kind: "transport", message: "net" });
    await expect(openSession("p", "s", null)).rejects.toMatchObject({
      kind: "transport",
    });
    expect(b.attachSession).not.toHaveBeenCalled();
  });

  it("propagates an attach error when the fallback attach fails after sessionExists", async () => {
    b.spawnSession.mockRejectedValueOnce({ kind: "sessionExists" });
    b.attachSession.mockRejectedValueOnce({
      kind: "transport",
      message: "net",
    });
    await expect(openSession("p", "s", null)).rejects.toMatchObject({
      kind: "transport",
    });
    expect(b.attachSession).toHaveBeenCalledTimes(1);
  });
});
