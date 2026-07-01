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

/**
 * Whether a sidebar **respawn-branch** open (the clicked node was discovered
 * `stopped`) must disarm `focusOnSelect`, given the matching tab's local status
 * *before* the open ran (`null` = no tab was open yet).
 *
 * This is the respawn-path twin of {@link shouldDisarmAfterSidebarOpen}, and it
 * differs in one load-bearing way: `openViaRespawn` actively respawns a
 * `stopped`/`disconnected` existing tab — a controlled path to `live` that the
 * focus effect will consume — so the arm is **kept** for those, whereas the
 * live-attach path (which never respawns) disarms them. Keep the arm only when
 * a live terminal will predictably arrive to consume it:
 *  - a fresh spawn (`opened`) commits a live tab;
 *  - a dedupe to an already-`live` tab (clicking it flips `activeKey` to a live
 *    tab, so the effect focuses it — the active-live re-click is short-circuited
 *    before the open, so a `live` status here is a background tab); or
 *  - a `stopped`/`disconnected` tab that `openViaRespawn` will respawn.
 *
 * Disarm otherwise:
 *  - a real failure (a no-op open never flips `activeKey`), but NOT a cancel;
 *  - a dedupe to a `reconnecting` tab — `openViaRespawn` does NOT respawn it, so
 *    its only path to `live` is a self-recovery we don't control (#136 policy:
 *    never carry the arm across a state change we don't own); and
 *  - a vanished tab (`null`) — nothing to focus.
 *
 * Decide from the status captured *before* the call: `respawnTab` flips
 * `stopped`/`disconnected` to `reconnecting` synchronously, so a post-call read
 * would misclassify a tab that is legitimately respawning.
 *
 * @param preStatus the matching tab's status sampled before `openViaRespawn`,
 *   or null if no matching tab was open.
 */
export function shouldDisarmAfterSidebarRespawn(
  result: OpenResult,
  preStatus: TabStatus | null,
): boolean {
  if (!result.ok) return result.error !== OPEN_CANCELLED;
  if (result.opened) return false; // fresh live tab — keep armed to focus it
  // Deduped to an existing tab: keep only where a controlled path to live
  // follows (live now, or stopped/disconnected → respawn). Reconnecting and
  // vanished have no path we control, so disarm.
  return preStatus === "reconnecting" || preStatus === null;
}
