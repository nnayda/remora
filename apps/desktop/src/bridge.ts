import { Channel } from "@tauri-apps/api/core";
import {
  type AgentInputDto,
  type BridgeError,
  type BridgeOutput,
  type ChannelHandle,
  type ConfigDto,
  commands,
  type DeviceInfoDto,
  type DirtyReasonDto,
  type EditableConfigDto,
  events,
  type HostInputDto,
  type PairingCodeDto,
  type PairingDeviceArrived,
  type PairingOutcomeDto,
  type PairingResult,
  type PairingWindowOpened,
  type ProjectInputDto,
  type Result,
  type SessionListDto,
  type SessionMetaDto,
  type SessionStateDto,
  type WorkspaceModeDto,
} from "./bindings";

export type {
  AgentInputDto,
  BridgeError,
  BridgeOutput,
  ChannelHandle,
  ConfigDto,
  DeviceInfoDto,
  DirtyReasonDto,
  EditableConfigDto,
  HostInputDto,
  PairingCodeDto,
  PairingDeviceArrived,
  PairingOutcomeDto,
  PairingResult,
  PairingWindowOpened,
  ProjectInputDto,
  SessionListDto,
  SessionMetaDto,
  SessionStateDto,
  WorkspaceModeDto,
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

/** Discover sessions across every configured host, grouped per host with each
 * host's reachability for this poll (sidebar polling). */
export async function listSessions(): Promise<SessionListDto> {
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
 * `agent` of `null` uses the project's default agent; `base` of `null` resolves
 * the project default / detected base (#54). */
export async function spawnSession(
  projectId: string,
  sessionId: string,
  agent: string | null,
  base: string | null,
  workspace: WorkspaceModeDto,
  branch: string | null,
  worktreeRoot: string | null,
  onOutput: OnOutput,
): Promise<ChannelHandle> {
  return unwrap(
    await commands.sessionSpawn(
      projectId,
      sessionId,
      agent,
      base,
      workspace,
      branch,
      worktreeRoot,
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

/** Open (or replace) this device's pairing window; returns the QR payload and
 * the countdown deadline. `ttlSecs` of `null` uses the bridge's default
 * (120s). Throws `BridgeError {kind:"relayNotConfigured"}` when this device
 * hosts no relay bridge. */
export async function openPairingWindow(
  ttlSecs: number | null,
): Promise<PairingCodeDto> {
  return unwrap(await commands.pairingOpenWindow(ttlSecs));
}

/** Confirm the arrived device's fingerprint (enrol it). */
export async function confirmPairing(deviceId: string): Promise<void> {
  unwrap(await commands.pairingConfirm(deviceId));
}

/** Reject the arrived device (grant nothing durable). */
export async function rejectPairing(deviceId: string): Promise<void> {
  unwrap(await commands.pairingReject(deviceId));
}

/** Close the current pairing window without pairing anyone. */
export async function cancelPairing(): Promise<void> {
  unwrap(await commands.pairingCancel());
}

/** List this bridge's paired devices (live roster). */
export async function listDevices(): Promise<DeviceInfoDto[]> {
  return unwrap(await commands.listDevices());
}

/** Un-pair a device (drop from roster, kick any live session). */
export async function revokeDevice(deviceId: string): Promise<void> {
  unwrap(await commands.revokeDevice(deviceId));
}

/** This bridge's own identity fingerprint (ADR-0021 D5), for the pairing UI. */
export async function getBridgeFingerprint(): Promise<string> {
  return unwrap(await commands.bridgeFingerprint());
}

/** Subscribe to a freshly (re)opened pairing window; fires with the QR
 * payload and expiry each time `openPairingWindow` succeeds. Returns the
 * unlisten function so the caller can clean up on unmount. */
export function subscribePairingWindowOpened(
  onOpened: (opened: PairingWindowOpened) => void,
): Promise<() => void> {
  return events.pairingWindowOpened.listen((event) => onOpened(event.payload));
}

/** Subscribe to a device reaching this device's open pairing window and
 * awaiting confirm/reject; the UI shows `fingerprint` for the human to
 * compare against the device's screen. Returns the unlisten function so the
 * caller can clean up on unmount. */
export function subscribePairingDeviceArrived(
  onArrived: (arrived: PairingDeviceArrived) => void,
): Promise<() => void> {
  return events.pairingDeviceArrived.listen((event) =>
    onArrived(event.payload),
  );
}

/** Subscribe to a pairing attempt reaching a terminal outcome (paired,
 * rejected, or expired — including after `cancelPairing`). Returns the
 * unlisten function so the caller can clean up on unmount. */
export function subscribePairingResult(
  onResult: (result: PairingResult) => void,
): Promise<() => void> {
  return events.pairingResult.listen((event) => onResult(event.payload));
}

/** Subscribe to backend roster-change pings. The bridge emits `RosterChanged`
 * on every enroll/revoke; `onChange` re-reads devices via `listDevices`.
 * Returns the unlisten function so the caller can clean up on unmount. */
export function subscribeRosterChanged(
  onChange: () => void,
): Promise<() => void> {
  return events.rosterChanged.listen(() => onChange());
}
