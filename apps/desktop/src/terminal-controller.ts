import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import {
  type ClipboardWriter,
  writeClipboard as defaultWriteClipboard,
} from "./clipboard";
import type { SessionConnection } from "./connection";

const encoder = new TextEncoder();
// fatal: a remote-supplied payload with invalid UTF-8 throws here rather than
// silently landing U+FFFD replacement characters on the clipboard.
const decoder = new TextDecoder("utf-8", { fatal: true });

/** Decode a base64 string whose bytes are UTF-8 (OSC 52 payloads are UTF-8).
 * Throws on malformed input: `atob` on invalid base64, the decoder on invalid
 * UTF-8 — both are caught by the OSC 52 handler. */
function decodeBase64Utf8(b64: string): string {
  const binary = atob(b64);
  const bytes = Uint8Array.from(binary, (ch) => ch.charCodeAt(0));
  return decoder.decode(bytes);
}

/**
 * Owns one xterm.js Terminal bound to a SessionConnection. Wiring only — the
 * emulator owns screen state; we never parse bytes.
 *
 *   connection ──{bytes}──▶ term.write(Uint8Array)
 *   term.onData ──utf8 ───▶ connection.write
 *   ResizeObserver ─▶ fit ─▶ connection.resize   (debounced; 0/unchanged guarded)
 *   connection ──{closed}─▶ "[session closed]" notice, input stops
 */
export class TerminalController {
  private readonly term: Terminal;
  private readonly fit: FitAddon;
  private readonly observer: ResizeObserver;
  private readonly unsubscribe: () => void;
  private readonly onDataDisposable: { dispose(): void };
  private readonly oscDisposable: { dispose(): void };
  private closed = false;
  private lastRows = 0;
  private lastCols = 0;
  private resizeRaf = 0;

  /** Mount an xterm into `element` and wire it to `connection`: stream output
   * in, keystrokes out, and fit-on-resize. */
  constructor(
    element: HTMLElement,
    private readonly connection: SessionConnection,
    private readonly writeClipboard: ClipboardWriter = defaultWriteClipboard,
  ) {
    this.term = new Terminal({ cursorBlink: true });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.attachCustomKeyEventHandler((e) => this.handleKeyEvent(e));
    this.term.open(element);
    this.oscDisposable = this.term.parser.registerOscHandler(52, (data) =>
      this.handleOsc52(data),
    );

    this.unsubscribe = connection.subscribe((msg) => {
      if (msg.event === "bytes") this.term.write(new Uint8Array(msg.bytes));
      else if (msg.event === "closed") this.handleClosed();
      // A future BridgeOutput variant falls through here and is ignored, rather
      // than being mislabeled as a closed session.
    });

    this.onDataDisposable = this.term.onData((data) => {
      if (this.closed) return;
      void this.connection
        .write(encoder.encode(data))
        .catch((e) => this.logTransportError("write", e));
    });

    this.observer = new ResizeObserver(() => this.scheduleFit());
    this.observer.observe(element);
    this.syncSize(); // initial fit
  }

  /**
   * Custom key handling layered over xterm. Two interceptions, both returning
   * `false` to suppress xterm's default for that key; every other key returns
   * `true` and falls through unchanged.
   *
   * 1. Shift+Enter → ESC+CR (`\x1b\r`), the soft-return sequence agents expect
   *    to insert a newline, instead of the bare CR xterm would emit (which
   *    submits the prompt). Agent-agnostic: it's the same byte sequence a native
   *    terminal sends for Shift+Enter, so we're faithfully forwarding the key.
   * 2. The copy chord — Cmd+C (macOS) or Ctrl+Shift+C (Linux/Windows) — copies
   *    the current selection to the host clipboard and swallows the key so it is
   *    not sent to the PTY.
   *
   * A bare Ctrl-C matches neither branch, so it stays SIGINT. Other modifiers
   * (Ctrl/Alt/Meta+Enter) are left alone so their own bindings reach the agent.
   */
  private handleKeyEvent(event: KeyboardEvent): boolean {
    if (
      event.type === "keydown" &&
      event.key === "Enter" &&
      event.shiftKey &&
      !event.ctrlKey &&
      !event.altKey &&
      !event.metaKey
    ) {
      // Returning false suppresses xterm's keydown handling, but xterm never
      // calls preventDefault on our behalf, so the browser would still fire a
      // `keypress` for Enter and xterm would emit a stray CR there. preventDefault
      // kills that follow-up keypress — the same thing xterm's own Enter path does.
      event.preventDefault();
      // Suppress the default CR either way; only write while the session lives.
      if (!this.closed) {
        void this.connection
          .write(encoder.encode("\x1b\r"))
          .catch((e) => this.logTransportError("write", e));
      }
      return false;
    }

    const isCopyChord =
      event.type === "keydown" &&
      event.code === "KeyC" &&
      !event.altKey &&
      ((event.metaKey && !event.ctrlKey && !event.shiftKey) ||
        (event.ctrlKey && event.shiftKey && !event.metaKey));
    if (isCopyChord) {
      // Suppress the browser's native copy too, not just xterm's handling:
      // returning false only stops xterm, so without this the webview's own
      // Cmd+C copy could also fire and race our clipboard write.
      event.preventDefault();
      const selection = this.term.getSelection();
      if (selection) {
        void this.writeClipboard(selection).catch((err) =>
          this.logClipboardError(err),
        );
      }
      return false; // consume the chord; never forward to the PTY
    }

    return true;
  }

  /** Move keyboard focus into the emulator so the user can type immediately.
   * No-op once closed: a dead session takes no input, so there's nothing to
   * type into. */
  focus(): void {
    if (this.closed) return;
    this.term.focus();
  }

  /** Mark the terminal dead and print a notice; input stops after this. */
  private handleClosed(): void {
    if (this.closed) return;
    this.closed = true;
    // Dim grey notice so a dead session reads clearly, not as a frozen bug.
    this.term.write("\r\n\x1b[90m[session closed]\x1b[0m\r\n");
  }

  /** Surface (don't swallow) a fire-and-forget write/resize rejection. Log only:
   * the bridge's `closed` event is the authoritative death signal, so a single
   * transient rejection must not force-close an otherwise live session. */
  private logTransportError(op: "write" | "resize", error: unknown): void {
    console.error(`terminal ${op} failed`, error);
  }

  /** Handle an inbound OSC 52 clipboard sequence (`Pc;Pd`). A set request writes
   * the decoded UTF-8 text to the host clipboard; a read request (`Pd === "?"`)
   * is ignored so a remote can never exfiltrate the local clipboard. Always
   * reports handled so xterm does not fall back to default processing. */
  private handleOsc52(data: string): boolean {
    const sep = data.indexOf(";");
    const payload = sep === -1 ? data : data.slice(sep + 1);
    if (payload === "?") return true; // read request: never echo the clipboard back
    if (payload === "") return true; // empty set: ignore, don't clobber the clipboard
    let text: string;
    try {
      text = decodeBase64Utf8(payload);
    } catch (err) {
      console.error("terminal OSC 52 decode failed", err);
      return true;
    }
    void this.writeClipboard(text).catch((err) => this.logClipboardError(err));
    return true;
  }

  /** Surface (don't swallow) a clipboard write rejection. Log only: a denied or
   * failed clipboard write must not tear down an otherwise-live session. */
  private logClipboardError(error: unknown): void {
    console.error("terminal clipboard write failed", error);
  }

  /** Coalesce a burst of ResizeObserver callbacks into one fit on the next frame. */
  private scheduleFit(): void {
    if (this.resizeRaf) return; // coalesce a burst of observe callbacks into one fit
    this.resizeRaf = requestAnimationFrame(() => {
      this.resizeRaf = 0;
      this.syncSize();
    });
  }

  /** Fit the emulator to its element and push the new geometry to the remote
   * TTY, skipping zero/unchanged sizes (the bridge rejects a 0x0 winsize). */
  private syncSize(): void {
    if (this.closed) return;
    this.fit.fit();
    const { rows, cols } = this.term;
    if (rows === 0 || cols === 0) return; // unlaid-out/hidden element: skip (bridge rejects 0)
    if (rows === this.lastRows && cols === this.lastCols) return; // nothing changed
    this.lastRows = rows;
    this.lastCols = cols;
    void this.connection
      .resize(rows, cols)
      .catch((e) => this.logTransportError("resize", e));
  }

  /** Tear down all listeners, the ResizeObserver, and the xterm instance. */
  dispose(): void {
    this.oscDisposable.dispose();
    if (this.resizeRaf) cancelAnimationFrame(this.resizeRaf);
    this.observer.disconnect();
    this.onDataDisposable.dispose();
    this.unsubscribe();
    this.term.dispose();
  }
}
