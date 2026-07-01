import {
  OPEN_CANCELLED,
  type OpenResult,
  type TabStatus,
} from "./session-store";

/**
 * Whether a sidebar open must disarm the `focusOnSelect` intent flag.
 *
 * The flag is armed before the opener runs so the focus effect grabs the
 * terminal once the active tab is `live`. Both sidebar openers now lead every
 * existing tab to a live terminal — a `live` dedupe is focused directly, and a
 * non-live dedupe is revived in place (`openViaAttach` → `reconnectTab`,
 * `openViaRespawn` → `respawnTab`), a controlled path to `live`. So keep the arm
 * for any successful open, and rely on the focus effect to clear it if a revive
 * terminally fails (App.tsx §2b). Disarm only when there is nothing a live
 * terminal can arrive for:
 *  - a real failure (a no-op open never flips `activeKey`), but NOT a cancel
 *    (store closed/disposed mid-connect — not a real open attempt); or
 *  - a vanished tab (`preStatus === null` on a dedupe). `preStatus` is sampled
 *    before the call, so a dedupe (`opened === false`) always had a tab: this
 *    case is unreachable via `openFromSidebar` and kept only as a defensive belt.
 *
 * @param preStatus the matching tab's status sampled BEFORE the opener ran
 *   (openers flip status synchronously), or null if no matching tab was open.
 */
export function shouldDisarmAfterSidebarOpen(
  result: OpenResult,
  preStatus: TabStatus | null,
): boolean {
  if (!result.ok) return result.error !== OPEN_CANCELLED;
  if (result.opened) return false; // fresh live tab — keep armed to focus it
  return preStatus === null; // deduped: keep the arm; disarm only if vanished
}

/**
 * Whether a sidebar re-click of the *active* tab should focus its terminal in
 * place instead of routing to an opener.
 *
 * Re-clicking the active, locally-`live` tab changes no store state — an opener
 * would dedupe to the current `activeKey`, so the activeKey-gated focus effect
 * never fires to consume `focusOnSelect`; focusing directly (and disarming) is
 * the only way to land focus and avoid leaking the armed flag onto the next
 * unrelated change. A non-live active tab must NOT short-circuit: it has to fall
 * through to a revive path (#189 Bug A) — the old code focused a placeholder
 * (no terminal handle) and returned without reconnecting.
 */
export function shouldFocusActiveTabInPlace(
  activeKey: string | null,
  key: string,
  preStatus: TabStatus | null,
): boolean {
  return key === activeKey && preStatus === "live";
}
