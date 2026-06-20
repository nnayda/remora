import { Channel } from "@tauri-apps/api/core";
import {
  type AgentInputDto,
  type BridgeError,
  type BridgeOutput,
  type ChannelHandle,
  type ConfigDto,
  commands,
  type EditableConfigDto,
  type HostInputDto,
  type ProjectInputDto,
  type Result,
  type SessionMetaDto,
  type SessionStateDto,
} from "./bindings";

export type {
  AgentInputDto,
  BridgeError,
  BridgeOutput,
  ChannelHandle,
  ConfigDto,
  EditableConfigDto,
  HostInputDto,
  ProjectInputDto,
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

/** Read the **un-redacted** editable config for the Settings forms, with its
 * validation state for degraded-mode recovery (ADR-0006). Local-only. */
export async function getEditableConfig(): Promise<EditableConfigDto> {
  return unwrap(await commands.configGetEditable());
}

/** Create a host. Rejects a duplicate id or an invalid edit with the typed
 * `BridgeError` (`configEdit`/`invalidId`), shown inline on the form. */
export async function insertHost(
  id: string,
  input: HostInputDto,
): Promise<void> {
  unwrap(await commands.configInsertHost(id, input));
}

/** Edit a host in place (also the display-name rename path). */
export async function updateHost(
  id: string,
  input: HostInputDto,
): Promise<void> {
  unwrap(await commands.configUpdateHost(id, input));
}

/** Remove a host. Rejected (`configEdit`) if a project still references it. */
export async function removeHost(id: string): Promise<void> {
  unwrap(await commands.configRemoveHost(id));
}

/** Create a project. */
export async function insertProject(
  id: string,
  input: ProjectInputDto,
): Promise<void> {
  unwrap(await commands.configInsertProject(id, input));
}

/** Edit a project in place. */
export async function updateProject(
  id: string,
  input: ProjectInputDto,
): Promise<void> {
  unwrap(await commands.configUpdateProject(id, input));
}

/** Remove a project. */
export async function removeProject(id: string): Promise<void> {
  unwrap(await commands.configRemoveProject(id));
}

/** Create an agent. */
export async function insertAgent(
  id: string,
  input: AgentInputDto,
): Promise<void> {
  unwrap(await commands.configInsertAgent(id, input));
}

/** Edit an agent in place. */
export async function updateAgent(
  id: string,
  input: AgentInputDto,
): Promise<void> {
  unwrap(await commands.configUpdateAgent(id, input));
}

/** Remove an agent. Rejected (`configEdit`) if a project still references it. */
export async function removeAgent(id: string): Promise<void> {
  unwrap(await commands.configRemoveAgent(id));
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
