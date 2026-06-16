import {
  type BridgeOutput,
  type ChannelHandle,
  closeSession,
  type OnOutput,
  resizeSession,
  writeSession,
} from "./bridge";

/** One open bridge channel: a buffered output stream plus input controls. */
export interface SessionConnection {
  /** Replay buffered output to `onMessage`, then stream live. Returns an unsubscribe. */
  subscribe(onMessage: OnOutput): () => void;
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
 */
export async function openConnection(open: Opener): Promise<SessionConnection> {
  let subscriber: OnOutput | null = null;
  const buffer: BridgeOutput[] = [];
  let closed = false;

  const onOutput: OnOutput = (msg) => {
    if (msg.event === "closed") closed = true;
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
