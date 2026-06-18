import {
  attachSession,
  type BridgeError,
  type BridgeOutput,
  type ChannelHandle,
  closeSession,
  type OnOutput,
  resizeSession,
  respawnSession,
  spawnSession,
  writeSession,
} from "./bridge";

export type { BridgeOutput, OnOutput };

/** One open bridge channel: a buffered output stream plus input controls. */
export interface SessionConnection {
  /** Replay buffered output to `onMessage`, then stream live. Returns an unsubscribe. */
  subscribe(onMessage: OnOutput): () => void;
  /** Register a death listener; fires once when a `closed` event is observed
   * (transport death), never on our own `close()`. Returns an unsubscribe. */
  onClose(listener: () => void): () => void;
  write(bytes: Uint8Array): Promise<void>;
  resize(rows: number, cols: number): Promise<void>;
  close(): Promise<void>;
  /** True once a `{ event: "closed" }` message has been observed. */
  readonly closed: boolean;
}

/** How a connection opens its channel: a bridge call wired to an OnOutput. */
type Opener = (onOutput: OnOutput) => Promise<ChannelHandle>;

/**
 * Open one channel and wrap it. Output produced before `subscribe` is buffered
 * and replayed on the first subscribe, so nothing is lost in the window between
 * the channel opening and the terminal mounting (e.g. an attach banner).
 *
 * The caller is expected to `subscribe` within a render tick (the terminal
 * mounts immediately): the buffer is a short-lived handoff, not a durable
 * store, so it is intentionally uncapped — a terminal stream cannot drop bytes
 * mid-escape-sequence, so a ring buffer would corrupt the screen. Durable
 * backpressure for detach/reattach is a transport concern (stage 11).
 */
export async function openConnection(open: Opener): Promise<SessionConnection> {
  let subscriber: OnOutput | null = null;
  const buffer: BridgeOutput[] = [];
  let closed = false;
  const closeListeners = new Set<() => void>();

  const onOutput: OnOutput = (msg) => {
    if (msg.event === "closed" && !closed) {
      // First close TRANSITION: fire death listeners exactly once, then clear
      // so a second `closed` event (or a lingering listener) can't re-trigger
      // the death path.
      closed = true;
      const listeners = [...closeListeners];
      closeListeners.clear();
      for (const l of listeners) l();
    } else if (msg.event === "closed") {
      closed = true; // already closed; do not re-fire
    }
    if (subscriber) subscriber(msg);
    else buffer.push(msg);
  };

  const handle = await open(onOutput);

  return {
    get closed() {
      return closed;
    },
    subscribe(onMessage) {
      subscriber = onMessage;
      // splice(0) drains and clears in one step; no message can arrive mid-loop
      // (single-threaded, no await), so order is preserved.
      for (const msg of buffer.splice(0)) onMessage(msg);
      return () => {
        if (subscriber === onMessage) subscriber = null;
      };
    },
    onClose(listener) {
      // Fire-once contract: if death already happened (listeners cleared),
      // a late registration must still see it — else `registerDeath` could
      // miss the signal and leave a tab `live` over a dead channel.
      if (closed) {
        listener();
        return () => {};
      }
      closeListeners.add(listener);
      return () => closeListeners.delete(listener);
    },
    write(bytes) {
      return writeSession(handle, bytes);
    },
    resize(rows, cols) {
      return resizeSession(handle, rows, cols);
    },
    close() {
      return closeSession(handle);
    },
  };
}

/** Narrow an unknown thrown value to a typed `BridgeError` (it carries `kind`). */
function isBridgeError(e: unknown): e is BridgeError {
  return typeof e === "object" && e !== null && "kind" in e;
}

/** True when `e` is the bridge's "the session is gone" error. */
export function isSessionNotFound(e: unknown): boolean {
  return isBridgeError(e) && e.kind === "sessionNotFound";
}

/** True when `e` is the bridge's "a live session already holds this name" error. */
export function isSessionExists(e: unknown): boolean {
  return isBridgeError(e) && e.kind === "sessionExists";
}

export type ReconnectFate = "retry" | "stopped" | "terminal";

/** Classify an attach failure for the reconnect machine (D5):
 *  - sessionNotFound → the tmux session is gone → `stopped` (respawnable)
 *  - config/invalidId → permanent (bad config/auth) → `terminal` (show cause)
 *  - everything else (transport/network) → `retry` with backoff */
export function reconnectFate(e: unknown): ReconnectFate {
  if (isSessionNotFound(e)) return "stopped";
  if (isBridgeError(e) && (e.kind === "config" || e.kind === "invalidId")) {
    return "terminal";
  }
  return "retry";
}

/** Human-readable cause for a terminal disconnect. */
export function errorMessage(e: unknown): string {
  if (isBridgeError(e) && "message" in e && typeof e.message === "string") {
    return e.message;
  }
  if (typeof e === "string") return e;
  return "unknown error";
}

/** Attach-only opener for reconnect / sidebar-live clicks. Unlike the removed
 * `connectSession`, it never spawns on not-found — the caller decides whether a
 * vanished session becomes `stopped` (respawnable). */
export async function attachConnection(
  projectId: string,
  sessionId: string,
): Promise<SessionConnection> {
  return openConnection((o) => attachSession(projectId, sessionId, o));
}

/** Respawn a stopped session, carrying the discovered agent (D6). */
export async function respawnConnection(
  projectId: string,
  sessionId: string,
  agent: string | null,
): Promise<SessionConnection> {
  return openConnection((o) => respawnSession(projectId, sessionId, agent, o));
}

/**
 * Open a *new* session: spawn first, and on `sessionExists` attach the running
 * one instead. Returns `attached: true` when it attached an existing session so
 * the UI can say so rather than silently opening old state. (Contrast the
 * removed `connectSession`, which was attach-first for reconnect callers.)
 */
export async function openSession(
  projectId: string,
  sessionId: string,
  agent: string | null,
): Promise<{ connection: SessionConnection; attached: boolean }> {
  try {
    const connection = await openConnection((o) =>
      spawnSession(projectId, sessionId, agent, o),
    );
    return { connection, attached: false };
  } catch (e) {
    if (!isSessionExists(e)) throw e;
    const connection = await openConnection((o) =>
      attachSession(projectId, sessionId, o),
    );
    return { connection, attached: true };
  }
}
