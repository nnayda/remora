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

  constructor(
    element: HTMLElement,
    private readonly connection: SessionConnection,
  ) {
    this.term = new Terminal({ cursorBlink: true });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.open(element);

    this.unsubscribe = connection.subscribe((msg) => {
      if (msg.event === "bytes") this.term.write(new Uint8Array(msg.bytes));
      else if (msg.event === "closed") this.handleClosed();
      // A future BridgeOutput variant falls through here and is ignored, rather
      // than being mislabeled as a closed session.
    });

    this.onDataDisposable = this.term.onData((data) => {
      if (this.closed) return;
      void this.connection.write(encoder.encode(data)).catch(() => {});
    });

    this.observer = new ResizeObserver(() => this.scheduleFit());
    this.observer.observe(element);
    this.syncSize(); // initial fit
  }

  private handleClosed(): void {
    if (this.closed) return;
    this.closed = true;
    // Dim grey notice so a dead session reads clearly, not as a frozen bug.
    this.term.write("\r\n\x1b[90m[session closed]\x1b[0m\r\n");
  }

  private scheduleFit(): void {
    if (this.resizeRaf) return; // coalesce a burst of observe callbacks into one fit
    this.resizeRaf = requestAnimationFrame(() => {
      this.resizeRaf = 0;
      this.syncSize();
    });
  }

  private syncSize(): void {
    if (this.closed) return;
    this.fit.fit();
    const { rows, cols } = this.term;
    if (rows === 0 || cols === 0) return; // unlaid-out/hidden element: skip (bridge rejects 0)
    if (rows === this.lastRows && cols === this.lastCols) return; // nothing changed
    this.lastRows = rows;
    this.lastCols = cols;
    void this.connection.resize(rows, cols).catch(() => {});
  }

  dispose(): void {
    if (this.resizeRaf) cancelAnimationFrame(this.resizeRaf);
    this.observer.disconnect();
    this.onDataDisposable.dispose();
    this.unsubscribe();
    this.term.dispose();
  }
}
