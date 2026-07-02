import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import type { ActivitySink } from "./activity-store";
import {
  type ClipboardWriter,
  writeClipboard as defaultWriteClipboard,
} from "./clipboard";
import type { SessionConnection } from "./connection";
import { decodeBase64Utf8 } from "./terminal-text";
import { activityStore } from "./useActivity";

const encoder = new TextEncoder();

/** Bytes for the macOS line-editing chords xterm won't produce on its own:
 * it drops meta+arrow outright and treats Cmd+Backspace as a one-char delete,
 * because on macOS meta chords are conventionally the app's to map. Native
 * terminals translate them in the emulator layer (iTerm's "Natural Text
 * Editing", VS Code's terminal keybindings), so we do the same, to the same
 * readline control bytes. Option+Backspace is included even though xterm can
 * emit it, so the chord stays deterministic regardless of the webview's
 * option-key (third-level-shift) handling. Returns undefined for anything
 * that isn't exactly one of these chords. */
function editingChordBytes(event: KeyboardEvent): string | undefined {
  if (event.type !== "keydown" || event.ctrlKey || event.shiftKey)
    return undefined;
  if (event.metaKey && !event.altKey) {
    if (event.key === "Backspace") return "\x15"; // Cmd+Delete → kill line backward (^U)
    if (event.key === "ArrowLeft") return "\x01"; // Cmd+Left → beginning of line (^A)
    if (event.key === "ArrowRight") return "\x05"; // Cmd+Right → end of line (^E)
  }
  if (event.altKey && !event.metaKey && event.key === "Backspace") {
    return "\x1b\x7f"; // Option+Delete → backward-kill-word (ESC DEL)
  }
  return undefined;
}

// Literal hex from notes/design-system/tokens/colors.css (xterm can't read CSS vars).
const XTERM_THEME = {
  background: "#08090C", // --ink-1000 / --term-bg
  foreground: "#D6D9E1", // --ansi-white
  cursor: "#6EA4FF", // --accent-bright / marine-pulse
  cursorAccent: "#08090C",
  selectionBackground: "rgba(30,111,245,0.30)",
  black: "#1A1B22",
  red: "#F0556B",
  green: "#4ECB83",
  yellow: "#E8A33D",
  blue: "#54A0F0",
  magenta: "#C77DD8",
  cyan: "#4FC4C9",
  white: "#D6D9E1",
  brightBlack: "#565A68",
  brightRed: "#F0556B",
  brightGreen: "#4ECB83",
  brightYellow: "#E8A33D",
  brightBlue: "#54A0F0",
  brightMagenta: "#C77DD8",
  brightCyan: "#4FC4C9",
  brightWhite: "#F4F5F8",
};

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
  private readonly activityDisposable: { dispose(): void };
  private readonly sessionKey?: string;
  private readonly activity: ActivitySink;
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
    opts: { sessionKey?: string; activity?: ActivitySink } = {},
  ) {
    this.sessionKey = opts.sessionKey;
    this.activity = opts.activity ?? activityStore;

    this.term = new Terminal({
      cursorBlink: true,
      theme: XTERM_THEME,
      fontFamily:
        "'JetBrains Mono', ui-monospace, 'SF Mono', Menlo, Consolas, monospace",
      fontSize: 13,
      lineHeight: 1.35,
    });
    this.fit = new FitAddon();
    this.term.loadAddon(this.fit);
    this.term.attachCustomKeyEventHandler((e) => this.handleKeyEvent(e));
    this.term.open(element);
    this.oscDisposable = this.term.parser.registerOscHandler(52, (data) =>
      this.handleOsc52(data),
    );
    // Core parses the OSC-7366 marker now (ADR-0013); the frontend only needs
    // to SWALLOW it so the still-present marker bytes never render. Returning
    // true marks it handled.
    this.activityDisposable = this.term.parser.registerOscHandler(
      7366,
      () => true,
    );

    this.unsubscribe = connection.subscribe((msg) => {
      if (msg.event === "bytes") {
        this.term.write(new Uint8Array(msg.bytes));
      } else if (msg.event === "statusChange") {
        if (this.sessionKey)
          this.activity.setStatus(this.sessionKey, msg.status);
      } else if (msg.event === "previewUpdate") {
        if (this.sessionKey)
          this.activity.setPreview(this.sessionKey, msg.preview);
      } else if (msg.event === "markerSeen") {
        if (this.sessionKey) this.activity.noteMarkerSeen(this.sessionKey);
      } else if (msg.event === "closed") {
        this.handleClosed();
      }
      // A future BridgeOutput variant falls through and is ignored.
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
   * Custom key handling layered over xterm. Three interceptions, all returning
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
   * 3. macOS line-editing chords (Cmd+Delete/Left/Right, Option+Delete) →
   *    their conventional readline bytes; see editingChordBytes.
   *
   * A bare Ctrl-C matches none of the branches, so it stays SIGINT. Other
   * modifiers (Ctrl/Alt/Meta+Enter) are left alone so their own bindings reach
   * the agent.
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

    const editingBytes = editingChordBytes(event);
    if (editingBytes !== undefined) {
      // Same reasoning as Shift+Enter: stop the webview acting on the chord
      // (e.g. Cmd+Left is "history back" in WebKit) on top of suppressing xterm.
      event.preventDefault();
      if (!this.closed) {
        void this.connection
          .write(encoder.encode(editingBytes))
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
    if (this.sessionKey) this.activity.clear(this.sessionKey);
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
    this.activityDisposable.dispose();
    if (this.sessionKey) this.activity.clear(this.sessionKey);
    if (this.resizeRaf) cancelAnimationFrame(this.resizeRaf);
    this.observer.disconnect();
    this.onDataDisposable.dispose();
    this.unsubscribe();
    this.term.dispose();
  }
}
