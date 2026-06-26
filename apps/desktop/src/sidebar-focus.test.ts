import { describe, expect, it } from "vitest";
import { OPEN_CANCELLED, type OpenResult } from "./session-store";
import { shouldDisarmAfterSidebarOpen } from "./sidebar-focus";

const ok = (opened: boolean): OpenResult => ({
  ok: true,
  attached: false,
  opened,
});

describe("shouldDisarmAfterSidebarOpen", () => {
  it("keeps the arm for a fresh spawn (a live terminal worth focusing)", () => {
    // existingStatus is irrelevant when a new tab was committed.
    expect(shouldDisarmAfterSidebarOpen(ok(true), null)).toBe(false);
    expect(shouldDisarmAfterSidebarOpen(ok(true), "disconnected")).toBe(false);
  });

  it("keeps the arm when deduping to an already-open LIVE tab (explicit focus)", () => {
    // Clicking an open live session in the sidebar should focus its terminal.
    expect(shouldDisarmAfterSidebarOpen(ok(false), "live")).toBe(false);
  });

  it("disarms when deduping to a non-live tab (stale-discovery focus leak #136)", () => {
    // Discovery says live, but the local tab is disconnected: openSession
    // focuses it without reconnecting, so the focus effect never consumes the
    // flag — disarm so a later background reconnect can't steal focus.
    expect(shouldDisarmAfterSidebarOpen(ok(false), "disconnected")).toBe(true);
    expect(shouldDisarmAfterSidebarOpen(ok(false), "reconnecting")).toBe(true);
    expect(shouldDisarmAfterSidebarOpen(ok(false), "stopped")).toBe(true);
  });

  it("disarms when deduping but the tab has vanished", () => {
    expect(shouldDisarmAfterSidebarOpen(ok(false), null)).toBe(true);
  });

  it("disarms on a real failure (a no-op open would leave the flag armed)", () => {
    expect(
      shouldDisarmAfterSidebarOpen(
        { ok: false, error: new Error("boom") },
        null,
      ),
    ).toBe(true);
  });

  it("does NOT disarm on a cancel (store closed/disposed, not a real attempt)", () => {
    expect(
      shouldDisarmAfterSidebarOpen({ ok: false, error: OPEN_CANCELLED }, null),
    ).toBe(false);
  });
});
