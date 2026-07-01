import { describe, expect, it } from "vitest";
import { OPEN_CANCELLED, type OpenResult } from "./session-store";
import {
  shouldDisarmAfterSidebarOpen,
  shouldDisarmAfterSidebarRespawn,
} from "./sidebar-focus";

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

describe("shouldDisarmAfterSidebarRespawn", () => {
  it("keeps the arm for a fresh spawn (a live terminal worth focusing)", () => {
    // preStatus is irrelevant when openViaRespawn committed a new tab.
    expect(shouldDisarmAfterSidebarRespawn(ok(true), null)).toBe(false);
    expect(shouldDisarmAfterSidebarRespawn(ok(true), "stopped")).toBe(false);
  });

  it("keeps the arm when deduping to an already-LIVE tab (effect focuses it)", () => {
    // The active-live re-click is short-circuited before the open, so a live
    // status here is a background tab: clicking flips activeKey to it and the
    // focus effect lands focus.
    expect(shouldDisarmAfterSidebarRespawn(ok(false), "live")).toBe(false);
  });

  it("keeps the arm for a stopped/disconnected tab openViaRespawn respawns", () => {
    // Unlike the live-attach path, openViaRespawn respawns these — a controlled
    // path to live — so keep the arm to focus the terminal once it comes up.
    expect(shouldDisarmAfterSidebarRespawn(ok(false), "stopped")).toBe(false);
    expect(shouldDisarmAfterSidebarRespawn(ok(false), "disconnected")).toBe(
      false,
    );
  });

  it("disarms when deduping to a reconnecting tab (uncontrolled recovery #136/#178)", () => {
    // openViaRespawn does NOT respawn a reconnecting tab; its only path to live
    // is a self-recovery we don't control, so disarm to avoid a focus steal
    // when it later goes live.
    expect(shouldDisarmAfterSidebarRespawn(ok(false), "reconnecting")).toBe(
      true,
    );
  });

  it("disarms when the deduped tab has vanished (nothing to focus)", () => {
    expect(shouldDisarmAfterSidebarRespawn(ok(false), null)).toBe(true);
  });

  it("disarms on a real failure (a no-op open would leave the flag armed)", () => {
    expect(
      shouldDisarmAfterSidebarRespawn(
        { ok: false, error: new Error("boom") },
        "stopped",
      ),
    ).toBe(true);
  });

  it("does NOT disarm on a cancel (store closed/disposed, not a real attempt)", () => {
    expect(
      shouldDisarmAfterSidebarRespawn(
        { ok: false, error: OPEN_CANCELLED },
        "stopped",
      ),
    ).toBe(false);
  });
});
