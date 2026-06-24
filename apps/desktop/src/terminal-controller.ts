import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import type { SessionConnection } from "./connection";

const encoder = new TextEncoder();

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
  private closed = false;
  private lastRows = 0;
  private lastCols = 0;
  private resizeRaf = 0;

  /** Mount an xterm into `element` and wire it to `connection`: stream output
   * in, keystrokes out, and fit-on-resize. */
  constructor(
    element: HTMLElement,
    private readonly connection: SessionConnection,
  ) {
    this.term = new Terminal({ cursorBlink: true });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.attachCustomKeyEventHandler((e) => this.handleKeyEvent(e));
    this.term.open(element);

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
   * Intercept Shift+Enter and forward it as ESC+CR (`\x1b\r`) — the soft-return
   * sequence agents expect to insert a newline in their input — instead of
   * letting xterm collapse it to a bare CR that submits the prompt. Returning
   * `false` suppresses xterm's default handling so it doesn't also emit a CR;
   * every other key returns `true` and falls through to xterm unchanged.
   *
   * Stays agent-agnostic: `\x1b\r` is the same byte sequence a native terminal
   * is configured to send for Shift+Enter, so we're faithfully forwarding the
   * keystroke over the PTY, not special-casing any agent. Other modifiers
   * (Ctrl/Alt/Meta) are left alone so their own bindings still reach the agent.
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
      // Suppress the default CR either way; only write while the session lives.
      if (!this.closed) {
        void this.connection
          .write(encoder.encode("\x1b\r"))
          .catch((e) => this.logTransportError("write", e));
      }
      return false;
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
    if (this.resizeRaf) cancelAnimationFrame(this.resizeRaf);
    this.observer.disconnect();
    this.onDataDisposable.dispose();
    this.unsubscribe();
    this.term.dispose();
  }
}
