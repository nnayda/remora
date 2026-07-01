import { describe, expect, it } from "vitest";
import {
  branchInitial,
  projectIcon,
  RAIL_ICONS,
  railEntries,
} from "./rail-glyph";
import type { ProjectNode, SessionNode } from "./session-tree";

// --- builders -------------------------------------------------------------
const sessionNode = (
  projectId: string,
  sessionId: string,
  opts: Partial<SessionNode> = {},
): SessionNode => ({
  projectId,
  sessionId,
  state: "live",
  agent: null,
  key: `${projectId} ${sessionId}`,
  workspace: null,
  reconnecting: false,
  branch: null,
  ...opts,
});

const projectNode = (
  id: string,
  sessions: SessionNode[],
  hostLabel = "hostA",
): ProjectNode => ({
  id,
  label: id,
  agent: null,
  hostLabel,
  transport: null,
  unconfigured: false,
  sessions,
});

// --- projectIcon ----------------------------------------------------------
describe("projectIcon", () => {
  it("is deterministic and indexes RAIL_ICONS", () => {
    expect(projectIcon(0)).toBe(RAIL_ICONS[0]);
    expect(projectIcon(3)).toBe(RAIL_ICONS[3]);
  });
  it("wraps past the 8th project", () => {
    expect(projectIcon(8)).toBe(RAIL_ICONS[0]);
    expect(projectIcon(9)).toBe(RAIL_ICONS[1]);
  });
});

// --- branchInitial --------------------------------------------------------
describe("branchInitial", () => {
  it("uppercases the first character of the branch", () => {
    expect(branchInitial(sessionNode("p", "s", { branch: "main" }))).toBe("M");
  });
  it("falls back to sessionId when branch is null", () => {
    expect(branchInitial(sessionNode("p", "abc"))).toBe("A");
  });
  it("returns a stable fallback for empty/whitespace", () => {
    expect(branchInitial(sessionNode("p", "   ", { branch: "  " }))).toBe("·");
  });
  it("handles a multi-byte first character without splitting it", () => {
    expect(branchInitial(sessionNode("p", "s", { branch: "émile" }))).toBe("É");
    expect(branchInitial(sessionNode("p", "s", { branch: "🚀x" }))).toBe("🚀");
  });
});

// --- railEntries ----------------------------------------------------------
describe("railEntries", () => {
  const empty = new Set<string>();
  const noActivity = new Map();

  it("returns [] for an empty tree", () => {
    expect(railEntries([], null, empty, noActivity, empty)).toEqual([]);
  });

  it("flattens one entry per session in tree order", () => {
    const tree = [
      projectNode("p1", [sessionNode("p1", "a"), sessionNode("p1", "b")]),
      projectNode("p2", [sessionNode("p2", "c")]),
    ];
    const out = railEntries(tree, null, empty, noActivity, empty);
    expect(out.map((e) => e.session.sessionId)).toEqual(["a", "b", "c"]);
  });

  it("omits zero-session projects and does not consume an icon index", () => {
    const tree = [
      projectNode("p1", [sessionNode("p1", "a")]),
      projectNode("empty", []),
      projectNode("p2", [sessionNode("p2", "b")]),
    ];
    const out = railEntries(tree, null, empty, noActivity, empty);
    expect(out).toHaveLength(2);
    // p1 -> index 0, p2 -> index 1 (empty did not bump the index)
    expect(out[0].Icon).toBe(RAIL_ICONS[0]);
    expect(out[1].Icon).toBe(RAIL_ICONS[1]);
  });

  it("shares one icon within a project and differs across projects", () => {
    const tree = [
      projectNode("p1", [sessionNode("p1", "a"), sessionNode("p1", "b")]),
      projectNode("p2", [sessionNode("p2", "c")]),
    ];
    const out = railEntries(tree, null, empty, noActivity, empty);
    expect(out[0].Icon).toBe(out[1].Icon); // same project
    expect(out[0].Icon).not.toBe(out[2].Icon); // different project
  });

  it("flags exactly the active key and sets firstOfProject per project", () => {
    const tree = [
      projectNode("p1", [sessionNode("p1", "a"), sessionNode("p1", "b")]),
      projectNode("p2", [sessionNode("p2", "c")]),
    ];
    const activeKey = "p1 b";
    const out = railEntries(tree, activeKey, empty, noActivity, empty);
    expect(out.map((e) => e.active)).toEqual([false, true, false]);
    expect(out.map((e) => e.firstOfProject)).toEqual([true, false, true]);
  });

  it("propagates connected, connecting, reconnecting and labels", () => {
    const tree = [
      projectNode(
        "p1",
        [sessionNode("p1", "a", { reconnecting: true, branch: "feat/x" })],
        "prod",
      ),
    ];
    const out = railEntries(
      tree,
      null,
      new Set(["p1 a"]),
      noActivity,
      new Set(["p1 a"]),
    );
    expect(out[0].connected).toBe(true);
    expect(out[0].connecting).toBe(true);
    expect(out[0].reconnecting).toBe(true);
    expect(out[0].branchLabel).toBe("feat/x");
    expect(out[0].hostLabel).toBe("prod");
    expect(out[0].initial).toBe("F");
  });
});
