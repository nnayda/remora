import { Channel } from "@tauri-apps/api/core";
import {
  type BridgeError,
  type BridgeOutput,
  type ChannelHandle,
  type ConfigDto,
  commands,
  type Result,
  type SessionMetaDto,
  type SessionStateDto,
} from "./bindings";

export type {
  BridgeError,
  BridgeOutput,
  ChannelHandle,
  ConfigDto,
  SessionMetaDto,
  SessionStateDto,
};
export type OnOutput = (msg: BridgeOutput) => void;

function makeChannel(onOutput: OnOutput): Channel<BridgeOutput> {
  const ch = new Channel<BridgeOutput>();
  ch.onmessage = onOutput;
  return ch;
}

function unwrap<T>(r: Result<T, BridgeError>): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

export async function listSessions(): Promise<SessionMetaDto[]> {
  return unwrap(await commands.sessionList());
}

export async function getConfig(): Promise<ConfigDto> {
  return unwrap(await commands.configGet());
}

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

export async function attachSession(
  projectId: string,
  sessionId: string,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionAttach(projectId, sessionId, makeChannel(onOutput)),
  );
}

export async function respawnSession(
  projectId: string,
  sessionId: string,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionRespawn(projectId, sessionId, makeChannel(onOutput)),
  );
}

export async function writeSession(
  handle: ChannelHandle,
  bytes: Uint8Array | number[],
): Promise<void> {
  unwrap(await commands.sessionWrite(handle, Array.from(bytes)));
}

export async function resizeSession(
  handle: ChannelHandle,
  rows: number,
  cols: number,
): Promise<void> {
  unwrap(await commands.sessionResize(handle, rows, cols));
}

export async function closeSession(handle: ChannelHandle): Promise<void> {
  unwrap(await commands.sessionClose(handle));
}
