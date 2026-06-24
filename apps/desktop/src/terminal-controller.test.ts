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
    focus = vi.fn();
    oscCb: ((data: string) => boolean) | null = null;
    oscDispose = vi.fn();
    keyHandler: ((e: KeyboardEvent) => boolean) | null = null;
    selection = "";
    getSelection = vi.fn(() => this.selection);
    attachCustomKeyEventHandler = vi.fn((cb: (e: KeyboardEvent) => boolean) => {
      this.keyHandler = cb;
    });
    parser: {
      registerOscHandler: (
        id: number,
        cb: (d: string) => boolean,
      ) => { dispose: () => void };
    };
    constructor() {
      state.term = this;
      this.parser = {
        registerOscHandler: (_id, cb) => {
          this.oscCb = cb;
          return { dispose: this.oscDispose };
        },
      };
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
let disconnectSpy = vi.fn();

beforeEach(() => {
  xt.state.term = null;
  xt.state.fit = null;
  roCb = null;
  rafCb = null;
  disconnectSpy = vi.fn();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(cb: () => void) {
        roCb = cb;
      }
      observe() {}
      disconnect() {
        disconnectSpy();
      }
    },
  );
  // Capture the rAF callback so the test flushes the debounce explicitly.
  vi.stubGlobal("requestAnimationFrame", (cb: () => void) => {
    rafCb = cb;
    return 1;
  });
  // Simulate real cancellation: a cancelled frame's callback never fires.
  vi.stubGlobal("cancelAnimationFrame", () => {
    rafCb = null;
  });
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

function term(): NonNullable<typeof xt.state.term> {
  const t = xt.state.term;
  if (!t) throw new Error("terminal not constructed");
  return t;
}

describe("TerminalController", () => {
  function ctrlWithClipboard() {
    const conn = fakeConn();
    const writeClipboard = vi.fn().mockResolvedValue(undefined);
    const c = new TerminalController(
      el,
      conn as unknown as SessionConnection,
      writeClipboard,
    );
    return { c, conn, writeClipboard };
  }

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
    term().rows = 30;
    term().cols = 100;
    roCb?.();
    rafCb?.(); // flush debounce
    expect(conn.resize).toHaveBeenCalledWith(30, 100);
  });

  it("skips resize when fit yields a zero dimension", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.resize.mockClear();
    term().rows = 0;
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

  it("does not call resize after the session is closed", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    conn.resize.mockClear();
    term().rows = 30;
    term().cols = 100;
    roCb?.();
    rafCb?.(); // flush debounce
    expect(conn.resize).not.toHaveBeenCalled();
  });

  it("on closed: shows a notice and stops forwarding input", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    const text = term()
      .written.filter((w): w is string => typeof w === "string")
      .join("");
    expect(text).toContain("[session closed]");
    conn.write.mockClear();
    xt.state.term?.dataCb?.("x");
    expect(conn.write).not.toHaveBeenCalled();
  });

  it("focus() moves keyboard focus into the emulator", () => {
    const conn = fakeConn();
    const c = new TerminalController(el, conn as unknown as SessionConnection);
    c.focus();
    expect(xt.state.term?.focus).toHaveBeenCalled();
  });

  it("focus() is a no-op after the session is closed", () => {
    const conn = fakeConn();
    const c = new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    c.focus();
    expect(xt.state.term?.focus).not.toHaveBeenCalled();
  });

  it("dispose tears down observer, subscription, onData, and terminal", () => {
    const conn = fakeConn();
    const c = new TerminalController(el, conn as unknown as SessionConnection);
    c.dispose();
    expect(disconnectSpy).toHaveBeenCalled();
    expect(conn.unsubscribe).toHaveBeenCalled();
    expect(xt.state.term?.dataDispose).toHaveBeenCalled();
    expect(xt.state.term?.oscDispose).toHaveBeenCalled();
    expect(xt.state.term?.dispose).toHaveBeenCalled();
  });

  it("writes the closed notice only once when closed arrives repeatedly", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    conn.emit({ event: "closed" });
    const notices = term().written.filter(
      (w): w is string =>
        typeof w === "string" && w.includes("[session closed]"),
    );
    expect(notices).toHaveLength(1);
  });

  it("cancels a pending resize on dispose so resize is not sent after teardown", () => {
    const conn = fakeConn();
    const c = new TerminalController(el, conn as unknown as SessionConnection);
    conn.resize.mockClear();
    term().rows = 30;
    term().cols = 100;
    roCb?.(); // queues a rAF via scheduleFit
    c.dispose(); // cancelAnimationFrame clears the queued callback
    rafCb?.(); // no-op after cancellation
    expect(conn.resize).not.toHaveBeenCalled();
  });

  it("logs a write failure instead of swallowing it, and stays open", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const conn = fakeConn();
    conn.write.mockRejectedValueOnce(new Error("dead channel"));
    new TerminalController(el, conn as unknown as SessionConnection);
    term().dataCb?.("x");
    await Promise.resolve(); // let the rejected write's catch run
    await Promise.resolve();
    expect(errSpy).toHaveBeenCalled();
    // A single rejection must NOT force-close: a later keystroke still writes.
    conn.write.mockResolvedValueOnce(undefined);
    term().dataCb?.("y");
    expect(conn.write).toHaveBeenCalledTimes(2);
    errSpy.mockRestore();
  });

  it("writes the decoded OSC 52 payload to the clipboard", () => {
    const { writeClipboard } = ctrlWithClipboard();
    // base64("hello") = "aGVsbG8="
    term().oscCb?.("c;aGVsbG8=");
    expect(writeClipboard).toHaveBeenCalledWith("hello");
  });

  it("decodes UTF-8 OSC 52 payloads correctly", () => {
    const { writeClipboard } = ctrlWithClipboard();
    // base64(utf8("café")) = "Y2Fmw6k="
    term().oscCb?.("c;Y2Fmw6k=");
    expect(writeClipboard).toHaveBeenCalledWith("café");
  });

  it("ignores OSC 52 read requests (never echoes the clipboard back)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    const handled = term().oscCb?.("c;?");
    expect(handled).toBe(true);
    expect(writeClipboard).not.toHaveBeenCalled();
  });

  it("logs and swallows a malformed OSC 52 payload", () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { writeClipboard } = ctrlWithClipboard();
    const handled = term().oscCb?.("c;@@not-base64@@");
    expect(handled).toBe(true);
    expect(writeClipboard).not.toHaveBeenCalled();
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("logs a clipboard write rejection without throwing", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const conn = fakeConn();
    const writeClipboard = vi.fn().mockRejectedValueOnce(new Error("denied"));
    new TerminalController(
      el,
      conn as unknown as SessionConnection,
      writeClipboard,
    );
    term().oscCb?.("c;aGVsbG8=");
    await Promise.resolve();
    await Promise.resolve();
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("copies the selection and consumes Cmd+C when text is selected", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "selected text";
    const handled = term().keyHandler?.({
      type: "keydown",
      code: "KeyC",
      metaKey: true,
    } as KeyboardEvent);
    expect(writeClipboard).toHaveBeenCalledWith("selected text");
    expect(handled).toBe(false); // consumed: not forwarded to the PTY
  });

  it("copies the selection on Ctrl+Shift+C (Linux/Windows chord)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "linux copy";
    const handled = term().keyHandler?.({
      type: "keydown",
      code: "KeyC",
      ctrlKey: true,
      shiftKey: true,
    } as KeyboardEvent);
    expect(writeClipboard).toHaveBeenCalledWith("linux copy");
    expect(handled).toBe(false);
  });

  it("consumes the copy chord but writes nothing when there is no selection", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "";
    const handled = term().keyHandler?.({
      type: "keydown",
      code: "KeyC",
      metaKey: true,
    } as KeyboardEvent);
    expect(writeClipboard).not.toHaveBeenCalled();
    expect(handled).toBe(false);
  });

  it("lets a bare Ctrl-C through (stays SIGINT, never copies)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "ignored";
    const handled = term().keyHandler?.({
      type: "keydown",
      code: "KeyC",
      ctrlKey: true,
    } as KeyboardEvent);
    expect(writeClipboard).not.toHaveBeenCalled();
    expect(handled).toBe(true); // passed through to xterm → ^C
  });
});
