import {
  OPEN_CANCELLED,
  type OpenResult,
  type TabStatus,
} from "./session-store";

/**
 * Whether a sidebar open must disarm the `focusOnSelect` intent flag.
 *
 * The flag is armed before `openSession` is called so the focus effect grabs
 * the terminal once it's live. But the effect only consumes the flag when the
 * active tab is `live` — so any open that doesn't leave a live terminal to
 * focus must disarm the flag itself, or it survives to steal focus on the next
 * unrelated `activeKey`/`activeStatus`/`focusRequest` change (e.g. a background
 * reconnect or respawn going live).
 *
 * Keep the arm only when the click lands on something live to focus:
 *  - a fresh spawn (`opened`), which commits a live tab, or
 *  - a dedupe to an already-open **live** tab (explicit re-selection → focus).
 *
 * Disarm otherwise:
 *  - a real failure (a no-op open never flips `activeKey`), but NOT a cancel
 *    (store closed/disposed mid-connect — not a real open attempt); and
 *  - a dedupe to ANY **non-live** existing tab (`disconnected`/`reconnecting`/
 *    `stopped`) — the stale-discovery window where discovery reports `live` but
 *    the local tab isn't. `openSession` focuses that tab without reconnecting,
 *    so the effect's `activeStatus === "live"` guard never consumes the flag
 *    (#136, the sidebar twin of the dialog leak fixed in #133). We disarm even a
 *    `reconnecting` tab that may later recover on its own: letting that deferred
 *    recovery grab focus after the user has looked elsewhere is the very steal
 *    this guards against — the conservative choice is to never carry the arm
 *    across a state change we don't control.
 *
 * @param existingStatus status of the deduped existing tab, or null if the open
 *   committed a fresh tab / no matching tab is open.
 */
export function shouldDisarmAfterSidebarOpen(
  result: OpenResult,
  existingStatus: TabStatus | null,
): boolean {
  if (!result.ok) return result.error !== OPEN_CANCELLED;
  if (result.opened) return false; // fresh live tab — keep armed to focus it
  return existingStatus !== "live"; // deduped: disarm unless the tab is live
}
