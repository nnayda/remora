import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BridgeOutput, OnOutput, SessionConnection } from "./connection";

// Mock activity-store and useActivity so tests don't pull in the real singleton.
vi.mock("./activity-store");
vi.mock("./useActivity", () => ({
  activityStore: { noteOutput: vi.fn(), noteMarker: vi.fn(), clear: vi.fn() },
}));

// Mock xterm: capture the constructed Terminal/FitAddon so tests can inspect
// writes, drive onData, and set rows/cols. We test our wiring, not the library.
const xt = vi.hoisted(() => {
  const state: { term: FakeTerminal | null; fit: FakeFit | null } = {
    term: null,
    fit: null,
  };
  // Keyed by OSC id (52, 7366, …). Exposed so tests can drive individual handlers.
  const oscHandlers = new Map<number, (data: string) => boolean>();
  const oscDispose = vi.fn();
  class FakeTerminal {
    rows = 24;
    cols = 80;
    written: Array<string | Uint8Array> = [];
    dataCb: ((d: string) => void) | null = null;
    keyCb: ((e: KeyboardEvent) => boolean) | null = null;
    dataDispose = vi.fn();
    open = vi.fn();
    dispose = vi.fn();
    loadAddon = vi.fn();
    focus = vi.fn();
    /** Backward-compat: first handler registered (OSC 52). Tests that were
     * written before the keyed map can still use this directly. */
    get oscCb(): ((data: string) => boolean) | null {
      return oscHandlers.get(52) ?? null;
    }
    oscDispose = oscDispose;
    keyHandler: ((e: KeyboardEvent) => boolean) | null = null;
    selection = "";
    getSelection = vi.fn(() => this.selection);
    // The controller registers ONE custom key handler that covers both the
    // Shift+Enter soft-return and the copy chord. Expose it under both names the
    // two test suites reach for (keyCb, keyHandler) so each can drive it.
    attachCustomKeyEventHandler = vi.fn((cb: (e: KeyboardEvent) => boolean) => {
      this.keyCb = cb;
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
        registerOscHandler: (id, cb) => {
          oscHandlers.set(id, cb);
          return {
            dispose: () => {
              oscHandlers.delete(id);
              oscDispose();
            },
          };
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
  return { state, FakeTerminal, FakeFit, oscHandlers };
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
  xt.oscHandlers.clear();
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

// Minimal KeyboardEvent double: just the fields the key handler inspects.
// `type` defaults to "keydown" since that's the event the handler acts on.
function key(
  k: string,
  opts: {
    shiftKey?: boolean;
    ctrlKey?: boolean;
    altKey?: boolean;
    metaKey?: boolean;
    type?: string;
  },
): KeyboardEvent {
  return {
    key: k,
    type: opts.type ?? "keydown",
    shiftKey: opts.shiftKey ?? false,
    ctrlKey: opts.ctrlKey ?? false,
    altKey: opts.altKey ?? false,
    metaKey: opts.metaKey ?? false,
    preventDefault: vi.fn(),
  } as unknown as KeyboardEvent;
}

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

  it("forwards Shift+Enter as ESC+CR (soft return) and suppresses the default CR", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    const ev = key("Enter", { shiftKey: true });
    const handled = term().keyCb?.(ev);
    expect(conn.write).toHaveBeenCalledWith(new TextEncoder().encode("\x1b\r"));
    expect(handled).toBe(false); // false = xterm must not also emit a plain CR
    // preventDefault stops the browser firing a `keypress` that xterm would
    // otherwise turn into a stray trailing CR (submitting the prompt anyway).
    expect(ev.preventDefault).toHaveBeenCalled();
  });

  it("leaves a plain Enter to xterm's default handling", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    const ev = key("Enter", {});
    const handled = term().keyCb?.(ev);
    expect(handled).toBe(true);
    expect(conn.write).not.toHaveBeenCalled();
    expect(ev.preventDefault).not.toHaveBeenCalled(); // xterm owns plain Enter
  });

  it("does not treat Ctrl/Alt/Meta+Enter as a soft return", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    for (const mods of [
      { ctrlKey: true },
      { altKey: true },
      { metaKey: true },
    ]) {
      expect(term().keyCb?.(key("Enter", { shiftKey: true, ...mods }))).toBe(
        true,
      );
    }
    expect(conn.write).not.toHaveBeenCalled();
  });

  it("ignores non-keydown Shift+Enter events (keyup/keypress)", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    expect(
      term().keyCb?.(key("Enter", { shiftKey: true, type: "keyup" })),
    ).toBe(true);
    expect(conn.write).not.toHaveBeenCalled();
  });

  it("suppresses Shift+Enter after close without writing to a dead session", () => {
    const conn = fakeConn();
    new TerminalController(el, conn as unknown as SessionConnection);
    conn.emit({ event: "closed" });
    conn.write.mockClear();
    expect(term().keyCb?.(key("Enter", { shiftKey: true }))).toBe(false);
    expect(conn.write).not.toHaveBeenCalled();
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

  it("ignores an empty OSC 52 payload (does not clobber the clipboard)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    const handled = term().oscCb?.("c;");
    expect(handled).toBe(true);
    expect(writeClipboard).not.toHaveBeenCalled();
  });

  it("logs and swallows OSC 52 with valid base64 but invalid UTF-8 bytes", () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { writeClipboard } = ctrlWithClipboard();
    // base64("\xff") = "/w==" — 0xFF is not a valid UTF-8 lead byte.
    const handled = term().oscCb?.("c;/w==");
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

  // A copy-chord KeyboardEvent double. `key` is matched by code, not key, so the
  // soft-return branch is skipped; preventDefault is a spy we can assert on.
  function copyChord(opts: {
    metaKey?: boolean;
    ctrlKey?: boolean;
    shiftKey?: boolean;
  }): KeyboardEvent {
    return {
      type: "keydown",
      code: "KeyC",
      altKey: false,
      metaKey: opts.metaKey ?? false,
      ctrlKey: opts.ctrlKey ?? false,
      shiftKey: opts.shiftKey ?? false,
      preventDefault: vi.fn(),
    } as unknown as KeyboardEvent;
  }

  it("copies the selection and consumes Cmd+C when text is selected", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "selected text";
    const ev = copyChord({ metaKey: true });
    const handled = term().keyHandler?.(ev);
    expect(writeClipboard).toHaveBeenCalledWith("selected text");
    expect(handled).toBe(false); // consumed: not forwarded to the PTY
    expect(ev.preventDefault).toHaveBeenCalled(); // also suppress native copy
  });

  it("copies the selection on Ctrl+Shift+C (Linux/Windows chord)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "linux copy";
    const handled = term().keyHandler?.(
      copyChord({ ctrlKey: true, shiftKey: true }),
    );
    expect(writeClipboard).toHaveBeenCalledWith("linux copy");
    expect(handled).toBe(false);
  });

  it("consumes the copy chord but writes nothing when there is no selection", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "";
    const ev = copyChord({ metaKey: true });
    const handled = term().keyHandler?.(ev);
    expect(writeClipboard).not.toHaveBeenCalled();
    expect(handled).toBe(false);
    expect(ev.preventDefault).toHaveBeenCalled();
  });

  it("lets a bare Ctrl-C through (stays SIGINT, never copies)", () => {
    const { writeClipboard } = ctrlWithClipboard();
    term().selection = "ignored";
    const ev = copyChord({ ctrlKey: true });
    const handled = term().keyHandler?.(ev);
    expect(writeClipboard).not.toHaveBeenCalled();
    expect(handled).toBe(true); // passed through to xterm → ^C
    expect(ev.preventDefault).not.toHaveBeenCalled(); // not consumed
  });

  // Reusable harness that creates a fresh fake connection + element pair.
  function makeHarness() {
    const conn = fakeConn();
    const harnesEl = {} as HTMLElement;
    return { el: harnesEl, conn };
  }

  it("notes output and parses the activity marker into the sink", () => {
    const activity = {
      noteOutput: vi.fn(),
      noteMarker: vi.fn(),
      clear: vi.fn(),
    };
    const { el: hEl, conn } = makeHarness();
    new TerminalController(
      hEl,
      conn as unknown as SessionConnection,
      undefined,
      {
        sessionKey: "api/fix",
        activity,
      },
    );
    // a bytes message → noteOutput
    conn.emit({ event: "bytes", bytes: [...new TextEncoder().encode("hi")] });
    expect(activity.noteOutput).toHaveBeenCalledWith("api/fix");
    // an OSC-7366 awaiting marker → noteMarker(awaiting)
    const handler7366 = xt.oscHandlers.get(7366);
    if (!handler7366) throw new Error("OSC 7366 handler not registered");
    handler7366(`remora;1;state;${btoa("awaiting_input")}`);
    expect(activity.noteMarker).toHaveBeenCalledWith("api/fix", "awaiting");
  });

  it("clears the sink on a closed event", () => {
    const activity = {
      noteOutput: vi.fn(),
      noteMarker: vi.fn(),
      clear: vi.fn(),
    };
    const { el: hEl, conn } = makeHarness();
    new TerminalController(
      hEl,
      conn as unknown as SessionConnection,
      undefined,
      {
        sessionKey: "api/fix",
        activity,
      },
    );
    conn.emit({ event: "closed" });
    expect(activity.clear).toHaveBeenCalledWith("api/fix");
  });

  it("does not call noteOutput for bytes replayed from the backlog on subscribe, but does for live bytes after", () => {
    // Simulates attaching to an already-active session: the connection replays
    // buffered bytes synchronously inside subscribe(), then delivers live bytes
    // asynchronously. Replayed bytes must NOT count as live activity (they are
    // catch-up output, not fresh agent work), while live bytes MUST.
    const activity = {
      noteOutput: vi.fn(),
      noteMarker: vi.fn(),
      clear: vi.fn(),
    };
    // A connection that replays one buffered bytes event synchronously on subscribe.
    let sub: OnOutput | null = null;
    const unsubscribe = vi.fn();
    const bufferedConn = {
      closed: false,
      subscribe: vi.fn((cb: OnOutput) => {
        sub = cb;
        // Synchronous replay of buffered output — same contract as real openConnection.
        cb({ event: "bytes", bytes: [104, 105] });
        return unsubscribe;
      }),
      write: vi.fn().mockResolvedValue(undefined),
      resize: vi.fn().mockResolvedValue(undefined),
      close: vi.fn().mockResolvedValue(undefined),
      unsubscribe,
      emit: (m: BridgeOutput) => sub?.(m),
    };
    const hEl = {} as HTMLElement;
    new TerminalController(
      hEl,
      bufferedConn as unknown as SessionConnection,
      undefined,
      { sessionKey: "s/attach", activity },
    );
    // The replayed byte must have been written to the terminal.
    expect(xt.state.term?.written).toContainEqual(new Uint8Array([104, 105]));
    // But noteOutput must NOT have been called for the replayed byte.
    expect(activity.noteOutput).not.toHaveBeenCalled();

    // A live byte arriving after subscribe() must call noteOutput.
    bufferedConn.emit({ event: "bytes", bytes: [119] });
    expect(activity.noteOutput).toHaveBeenCalledWith("s/attach");
    expect(activity.noteOutput).toHaveBeenCalledTimes(1);
  });
});
