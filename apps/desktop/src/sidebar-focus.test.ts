import { describe, expect, it } from "vitest";
import { OPEN_CANCELLED, type OpenResult } from "./session-store";
import {
  shouldDisarmAfterSidebarOpen,
  shouldFocusActiveTabInPlace,
} from "./sidebar-focus";

const ok = (opened: boolean): OpenResult => ({
  ok: true,
  attached: false,
  opened,
});

describe("shouldDisarmAfterSidebarOpen", () => {
  it("keeps the arm for a fresh spawn (a live terminal worth focusing)", () => {
    expect(shouldDisarmAfterSidebarOpen(ok(true), null)).toBe(false);
    expect(shouldDisarmAfterSidebarOpen(ok(true), "disconnected")).toBe(false);
  });

  it("keeps the arm when deduping to ANY existing tab (a controlled path to live follows)", () => {
    // live → the focus effect focuses it; non-live → an opener revive
    // (reconnect/respawn) leads to live, and the focus effect clears the arm if
    // that revive terminally fails (#189 §2b). So every dedupe keeps the arm.
    expect(shouldDisarmAfterSidebarOpen(ok(false), "live")).toBe(false);
    expect(shouldDisarmAfterSidebarOpen(ok(false), "stopped")).toBe(false);
    expect(shouldDisarmAfterSidebarOpen(ok(false), "disconnected")).toBe(false);
    expect(shouldDisarmAfterSidebarOpen(ok(false), "reconnecting")).toBe(false);
  });

  it("disarms when the deduped tab has vanished (defensive; nothing to focus)", () => {
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

describe("shouldFocusActiveTabInPlace", () => {
  it("focuses in place when re-clicking the active, locally-live tab", () => {
    expect(shouldFocusActiveTabInPlace("api/fix", "api/fix", "live")).toBe(
      true,
    );
  });

  it("does NOT focus in place when the active tab is non-live (must revive) — Bug A", () => {
    for (const s of ["stopped", "disconnected", "reconnecting"] as const) {
      expect(shouldFocusActiveTabInPlace("api/fix", "api/fix", s)).toBe(false);
    }
    expect(shouldFocusActiveTabInPlace("api/fix", "api/fix", null)).toBe(false);
  });

  it("does NOT focus in place when the clicked tab is not the active one", () => {
    expect(shouldFocusActiveTabInPlace("api/fix", "web/ui", "live")).toBe(
      false,
    );
    expect(shouldFocusActiveTabInPlace(null, "web/ui", "live")).toBe(false);
  });
});
