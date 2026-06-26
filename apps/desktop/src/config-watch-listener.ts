import { events } from "./bindings";

/** Subscribe to backend config-file-change pings. The Rust shell watches the
 * per-device config.toml and emits `ConfigChanged` on each (debounced) edit;
 * `onChange` re-reads config via the discovery store's refresh. Returns the
 * unlisten function so the caller can clean up on unmount. */
export function subscribeConfigChanged(
  onChange: () => void,
): Promise<() => void> {
  return events.configChanged.listen(() => onChange());
}
