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
  reconnectFate,
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

  it("fires the onClose listener exactly ONCE across two closed events", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    const onClose = vi.fn();
    conn.onClose(onClose);
    f.emit({ event: "closed" });
    f.emit({ event: "closed" }); // a second close must NOT re-trigger death
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(conn.closed).toBe(true);
  });

  it("does not fire on our own close()", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    const onClose = vi.fn();
    conn.onClose(onClose);
    await conn.close(); // local teardown — bridge is contracted to stay silent
    expect(onClose).not.toHaveBeenCalled();
  });

  it("fires immediately when registered AFTER the connection already closed", async () => {
    // A late registerDeath (e.g. swapping in a connection that died in the
    // race window) must still see the death, or the tab stays live over a
    // dead channel.
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    f.emit({ event: "closed" });
    const onClose = vi.fn();
    conn.onClose(onClose);
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("connection.lastOutput", () => {
  it("returns the cause line from the bytes the channel received", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    f.emit({
      event: "bytes",
      bytes: [...new TextEncoder().encode("booting\n")],
    });
    f.emit({
      event: "bytes",
      bytes: [...new TextEncoder().encode("claude: command not found\n")],
    });
    f.emit({ event: "closed" });
    expect(conn.lastOutput()).toBe("claude: command not found");
  });

  it("is empty when no output was ever received", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    f.emit({ event: "closed" });
    expect(conn.lastOutput()).toBe("");
  });

  it("keeps only the tail when output exceeds the cap", async () => {
    const f = fakeOpener();
    const conn = await openConnection(f.open);
    // A burst far larger than the 8 KB cap: the early bytes must be dropped, the
    // final line preserved.
    const filler = `${"a".repeat(20000)}\n`;
    f.emit({ event: "bytes", bytes: [...new TextEncoder().encode(filler)] });
    f.emit({
      event: "bytes",
      bytes: [...new TextEncoder().encode("final line\n")],
    });
    expect(conn.lastOutput()).toBe("final line");
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

describe("reconnectFate", () => {
  it("maps error kinds to fates", () => {
    expect(reconnectFate({ kind: "sessionNotFound", message: "x" })).toBe(
      "stopped",
    );
    expect(reconnectFate({ kind: "config", message: "bad host" })).toBe(
      "terminal",
    );
    expect(reconnectFate({ kind: "invalidId", message: "x" })).toBe("terminal");
    expect(reconnectFate({ kind: "transport", message: "down" })).toBe("retry");
    expect(reconnectFate(new Error("boom"))).toBe("retry");
  });
});

describe("connection.ts — openSession (spawn-first)", () => {
  it("spawns a fresh session and reports attached:false", async () => {
    b.spawnSession.mockResolvedValue(5);
    const { connection, attached } = await openSession(
      "p",
      "s",
      "claude",
      null,
      "worktree",
    );
    expect(attached).toBe(false);
    expect(connection.closed).toBe(false);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.spawnSession).toHaveBeenCalledWith(
      "p",
      "s",
      "claude",
      null,
      "worktree",
      expect.anything(),
    );
    expect(b.attachSession).not.toHaveBeenCalled();
  });

  it("spawns with a specific base branch", async () => {
    b.spawnSession.mockResolvedValue(5);
    await openSession("p", "s", null, "origin/dev", "worktree");
    expect(b.spawnSession).toHaveBeenCalledWith(
      "p",
      "s",
      null,
      "origin/dev",
      "worktree",
      expect.anything(),
    );
  });

  it("attaches and reports attached:true when the session already exists", async () => {
    b.spawnSession.mockRejectedValueOnce({ kind: "sessionExists" });
    b.attachSession.mockResolvedValue(6);
    const { attached } = await openSession("p", "s", null, null, "worktree");
    expect(attached).toBe(true);
    expect(b.spawnSession).toHaveBeenCalledTimes(1);
    expect(b.attachSession).toHaveBeenCalledTimes(1);
  });

  it("propagates unexpected spawn errors", async () => {
    b.spawnSession.mockRejectedValueOnce({ kind: "transport", message: "net" });
    await expect(
      openSession("p", "s", null, null, "worktree"),
    ).rejects.toMatchObject({
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
    await expect(
      openSession("p", "s", null, null, "worktree"),
    ).rejects.toMatchObject({
      kind: "transport",
    });
    expect(b.attachSession).toHaveBeenCalledTimes(1);
  });
});
