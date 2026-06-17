import { beforeEach, describe, expect, it, vi } from "vitest";

const b = vi.hoisted(() => ({
  attachSession: vi.fn(),
  spawnSession: vi.fn(),
  respawnSession: vi.fn(),
  writeSession: vi.fn(),
  resizeSession: vi.fn(),
  closeSession: vi.fn(),
}));

vi.mock("./bridge", () => ({
  attachSession: b.attachSession,
  spawnSession: b.spawnSession,
  respawnSession: b.respawnSession,
  writeSession: b.writeSession,
  resizeSession: b.resizeSession,
  closeSession: b.closeSession,
}));

import type { BridgeOutput, OnOutput } from "./bridge";
import {
  attachConnection,
  isSessionExists,
  isSessionNotFound,
  openConnection,
  openSession,
  respawnConnection,
} from "./connection";

// A fake opener that hands us the OnOutput so the test can push events.
function fakeOpener() {
  let sink: OnOutput | null = null;
  const open = (o: OnOutput) => {
    sink = o;
    return Promise.resolve({} as never); // handle unused in these tests
  };
  const emit = (msg: BridgeOutput) => sink?.(msg);
  return { open, emit };
}

describe("connection.onClose", () => {
  it("fires when a closed event is observed", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    const onClose = vi.fn();
    conn.onClose(onClose);
    f.emit({ event: "closed" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("does not fire on our own close()", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    const onClose = vi.fn();
    conn.onClose(onClose);
    await conn.close(); // local teardown — bridge is contracted to stay silent
    expect(onClose).not.toHaveBeenCalled();
  });
});

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

describe("connection.ts — attachConnection / respawnConnection", () => {
  it("attachConnection delegates to attachSession", async () => {
    b.attachSession.mockResolvedValue(10);
    const conn = await attachConnection("p", "s");
    expect(b.attachSession).toHaveBeenCalledWith("p", "s", expect.anything());
    expect(conn.closed).toBe(false);
  });

  it("respawnConnection delegates to respawnSession", async () => {
    b.respawnSession.mockResolvedValue(11);
    const conn = await respawnConnection("p", "s", "claude");
    expect(b.respawnSession).toHaveBeenCalledWith(
      "p",
      "s",
      "claude",
      expect.anything(),
    );
    expect(conn.closed).toBe(false);
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
