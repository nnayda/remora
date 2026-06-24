import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import {
  type ClipboardWriter,
  writeClipboard as defaultWriteClipboard,
} from "./clipboard";
import type { SessionConnection } from "./connection";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Decode a base64 string whose bytes are UTF-8 (OSC 52 payloads are UTF-8). */
function decodeBase64Utf8(b64: string): string {
  const binary = atob(b64); // throws DOMException on malformed input
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
