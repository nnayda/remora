import { describe, expect, it } from "vitest";
import type { ActivityState } from "./activity-store";
import { hostIndicatorState } from "./host-activity";
import type { SessionNode } from "./session-tree";

function session(key: string, state: "live" | "stopped" = "live"): SessionNode {
  return {
    projectId: "p",
    sessionId: key,
    state,
    agent: null,
    key,
    workspace: "worktree",
  };
}

describe("hostIndicatorState", () => {
  const activity = new Map<string, ActivityState>([
    ["a", "working"],
    ["b", "awaiting"],
    ["c", "idle"],
  ]);

  it("returns idle for a host with no open sessions", () => {
    expect(hostIndicatorState([session("a")], new Set(), activity)).toBe(
      "idle",
    );
  });
  it("returns needs when any open session is awaiting (wins over working)", () => {
    const sessions = [session("a"), session("b")];
    expect(hostIndicatorState(sessions, new Set(["a", "b"]), activity)).toBe(
      "needs",
    );
  });
  it("returns working when an open session is working and none need", () => {
    expect(hostIndicatorState([session("a")], new Set(["a"]), activity)).toBe(
      "working",
    );
  });
  it("ignores activity for sessions that are not open", () => {
    expect(hostIndicatorState([session("b")], new Set(), activity)).toBe(
      "idle",
    );
  });
  it("treats stopped open sessions as idle", () => {
    expect(
      hostIndicatorState([session("a", "stopped")], new Set(["a"]), activity),
    ).toBe("idle");
  });
  it("returns idle for an empty session list", () => {
    expect(hostIndicatorState([], new Set(), activity)).toBe("idle");
  });
  it("returns idle for an open live session with no activity entry", () => {
    // key "z" is open but absent from the activity map → no signal → idle
    expect(hostIndicatorState([session("z")], new Set(["z"]), activity)).toBe(
      "idle",
    );
  });
});
