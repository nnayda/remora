import { Channel } from "@tauri-apps/api/core";
import {
  type BridgeError,
  type BridgeOutput,
  type ChannelHandle,
  type ConfigDto,
  commands,
  type DirtyReasonDto,
  type Result,
  type SessionMetaDto,
  type SessionStateDto,
} from "./bindings";

export type {
  BridgeError,
  BridgeOutput,
  ChannelHandle,
  ConfigDto,
  DirtyReasonDto,
  SessionMetaDto,
  SessionStateDto,
};
export type OnOutput = (msg: BridgeOutput) => void;

/** Wrap an `OnOutput` callback in a Tauri `Channel` for the bridge to stream into. */
function makeChannel(onOutput: OnOutput): Channel<BridgeOutput> {
  const ch = new Channel<BridgeOutput>();
  ch.onmessage = onOutput;
  return ch;
}

/** Collapse a tauri-specta `Result` to its value, throwing the typed
 * `BridgeError` on the error arm so callers can `try`/`catch` it. */
function unwrap<T>(r: Result<T, BridgeError>): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** Discover the sessions known to every configured host (sidebar polling). */
export async function listSessions(): Promise<SessionMetaDto[]> {
  return unwrap(await commands.sessionList());
}

/** Read the resolved per-device config (hosts, projects, agents). */
export async function getConfig(): Promise<ConfigDto> {
  return unwrap(await commands.configGet());
}

/** Spawn a new session and open its channel; `onOutput` receives the stream.
 * `agent` of `null` uses the project's default agent. */
export async function spawnSession(
  projectId: string,
  sessionId: string,
  agent: string | null,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionSpawn(
      projectId,
      sessionId,
      agent,
      makeChannel(onOutput),
    ),
  );
}

/** Attach to an existing live session and open its channel. */
export async function attachSession(
  projectId: string,
  sessionId: string,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionAttach(projectId, sessionId, makeChannel(onOutput)),
  );
}

/** Re-create the tmux session for a stopped worktree and open its channel,
 * carrying the discovered `agent` (else the project default). */
export async function respawnSession(
  projectId: string,
  sessionId: string,
  agent: string | null,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionRespawn(
      projectId,
      sessionId,
      agent,
      makeChannel(onOutput),
    ),
  );
}

/** Send raw input bytes (keystrokes, pastes) to a session's PTY. */
export async function writeSession(
  handle: ChannelHandle,
  bytes: Uint8Array | number[],
): Promise<void> {
  unwrap(await commands.sessionWrite(handle, Array.from(bytes)));
}

/** Propagate a terminal resize to a session's remote TTY. */
export async function resizeSession(
  handle: ChannelHandle,
  rows: number,
  cols: number,
): Promise<void> {
  unwrap(await commands.sessionResize(handle, rows, cols));
}

/** Close our end of a session channel (reaps the backing transport child). */
export async function closeSession(handle: ChannelHandle): Promise<void> {
  unwrap(await commands.sessionClose(handle));
}

/** Kill a session's tmux (worktree survives, respawnable). */
export async function stopSession(
  projectId: string,
  sessionId: string,
): Promise<void> {
  unwrap(await commands.sessionStop(projectId, sessionId));
}

/** Tear a session down for good. Throws BridgeError {kind:"workspaceDirty"} when
 * the worktree has uncommitted/not-on-remote work and `force` is false. */
export async function removeSession(
  projectId: string,
  sessionId: string,
  force: boolean,
): Promise<void> {
  unwrap(await commands.sessionRemove(projectId, sessionId, force));
}
