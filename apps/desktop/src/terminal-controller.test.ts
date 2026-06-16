import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BridgeOutput, OnOutput, SessionConnection } from "./connection";

// Mock xterm: capture the constructed Terminal/FitAddon so tests can inspect
// writes, drive onData, and set rows/cols. We test our wiring, not the library.
const xt = vi.hoisted(() => {
  const state: { term: FakeTerminal | null; fit: FakeFit | null } = {
    term: null,
    fit: null,
  };
  class FakeTerminal {
    rows = 24;
    cols = 80;
    written: Array<string | Uint8Array> = [];
    dataCb: ((d: string) => void) | null = null;
    dataDispose = vi.fn();
    open = vi.fn();
    dispose = vi.fn();
    loadAddon = vi.fn();
    constructor() {
      state.term = this;
    }
    write(d: string | Uint8Array) {
      this.written.push(d);
    }
    onData(cb: (d: string) => void) {
      this.dataCb = cb;
      return { dispose: this.dataDispose };
    }
  }
  class FakeFit {
    fit = vi.fn();
    constructor() {
      state.fit = this;
    }
  }
  return { state, FakeTerminal, FakeFit };
});

vi.mock("@xterm/xterm", () => ({ Terminal: xt.FakeTerminal }));
vi.mock("@xterm/addon-fit", () => ({ FitAddon: xt.FakeFit }));

import { TerminalController } from "./terminal-controller";

let roCb: (() => void) | null = null;
let rafCb: (() => void) | null = null;

beforeEach(() => {
  xt.state.term = null;
  xt.state.fit = null;
  roCb = null;
  rafCb = null;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(cb: () => void) {
        roCb = cb;
      }
      observe() {}
      disconnect() {}
    },
  );
  // Capture the rAF callback so the test flushes the debounce explicitly.
  vi.stubGlobal("requestAnimationFrame", (cb: () => void) => {
    rafCb = cb;
    return 1;
  });
  vi.stubGlobal("cancelAnimationFrame", () => {});
});

afterEach(() => vi.unstubAllGlobals());

// A SessionConnection double whose output we can drive via emit().
function fakeConn() {
  let sub: OnOutput | null = null;
  const unsubscribe = vi.fn();
  const conn = {
    closed: false,
    subscribe: vi.fn((cb: OnOutput) => {
      sub = cb;
      return unsubscribe;
    }),
    write: vi.fn().mockResolvedValue(undefined),
    resize: vi.fn().mockResolvedValue(undefined),
    close: vi.fn().mockResolvedValue(undefined),
    unsubscribe,
    emit: (m: BridgeOutput) => sub?.(m),
  };
  return conn;
}

const el = {} as HTMLElement;

describe("TerminalController", () => {
  it("writes incoming bytes to the terminal as a Uint8Array (not a string)", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "bytes", bytes: [104, 105] });
    expect(xt.state.term?.written).toContainEqual(new Uint8Array([104, 105]));
  });

  it("forwards keystrokes as UTF-8 bytes", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    xt.state.term?.dataCb?.("hi");
    expect(conn.write).toHaveBeenCalledWith(new TextEncoder().encode("hi"));
  });

  it("sends an initial resize once on construction", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    expect(xt.state.fit?.fit).toHaveBeenCalled();
    expect(conn.resize).toHaveBeenCalledTimes(1);
    expect(conn.resize).toHaveBeenCalledWith(24, 80);
  });

  it("sends resize when geometry changes after a debounced observe", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.resize.mockClear();
    xt.state.term!.rows = 30;
    xt.state.term!.cols = 100;
    roCb?.();
    rafCb?.(); // flush debounce
    expect(conn.resize).toHaveBeenCalledWith(30, 100);
  });

  it("skips resize when fit yields a zero dimension", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.resize.mockClear();
    xt.state.term!.rows = 0;
    roCb?.();
    rafCb?.();
    expect(xt.state.fit?.fit).toHaveBeenCalled();
    expect(conn.resize).not.toHaveBeenCalled();
  });

  it("does not re-send an unchanged geometry", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.resize.mockClear(); // already sent 24x80 on construct; geometry unchanged
    roCb?.();
    rafCb?.();
    expect(conn.resize).not.toHaveBeenCalled();
  });

  it("on closed: shows a notice and stops forwarding input", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    const text = xt.state
      .term!.written.filter((w): w is string => typeof w === "string")
      .join("");
    expect(text).toContain("[session closed]");
    conn.write.mockClear();
    xt.state.term?.dataCb?.("x");
    expect(conn.write).not.toHaveBeenCalled();
  });

  it("dispose tears down observer, subscription, onData, and terminal", () => {
    const conn = fakeConn();
    const c = new TerminalController(el, conn as unknown as SessionConnection);
    c.dispose();
    expect(conn.unsubscribe).toHaveBeenCalled();
    expect(xt.state.term?.dataDispose).toHaveBeenCalled();
    expect(xt.state.term?.dispose).toHaveBeenCalled();
  });
});
