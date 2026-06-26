import { describe, expect, it } from "vitest";
import { filterTree } from "./filter-tree";
import type { ProjectNode, SessionNode } from "./session-tree";

const sess = (sessionId: string): SessionNode => ({
  projectId: "p",
  sessionId,
  state: "live",
  agent: null,
  key: `p/${sessionId}`,
  workspace: null,
});

const proj = (
  id: string,
  label: string,
  hostLabel: string,
  sessionIds: string[],
): ProjectNode => ({
  id,
  label,
  agent: "claude",
  hostLabel,
  transport: "ssh",
  unconfigured: false,
  sessions: sessionIds.map(sess),
});

const tree: ProjectNode[] = [
  proj("api", "api", "hermes", ["fix", "feat"]),
  proj("web", "web", "atlas", ["main"]),
];

describe("filterTree", () => {
  it("empty query returns the tree unchanged (by reference)", () => {
    expect(filterTree(tree, "")).toBe(tree);
    expect(filterTree(tree, "   ")).toBe(tree);
  });

  it("matches on project label (case-insensitive) and keeps all its sessions", () => {
    const out = filterTree(tree, "ApI");
    expect(out).toHaveLength(1);
    expect(out[0].id).toBe("api");
    expect(out[0].sessions).toHaveLength(2);
  });

  it("matches on host label (case-insensitive) and keeps the whole project", () => {
    const out = filterTree(tree, "ATLAS");
    expect(out).toHaveLength(1);
    expect(out[0].id).toBe("web");
    expect(out[0].sessions).toHaveLength(1);
  });

  it("session-only match (case-insensitive) filters sessions and drops non-matching projects", () => {
    const out = filterTree(tree, "FeaT");
    expect(out).toHaveLength(1);
    expect(out[0].id).toBe("api");
    expect(out[0].sessions.map((s) => s.sessionId)).toEqual(["feat"]);
  });

  it("preserves project referential identity when its sessions are unchanged", () => {
    // 'hermes' matches project api's host → api kept with original sessions array.
    const out = filterTree(tree, "hermes");
    expect(out[0]).toBe(tree[0]); // same object reference → React can skip re-render
  });

  it("returns a fresh object only for projects whose sessions were filtered", () => {
    const out = filterTree(tree, "fix");
    expect(out[0]).not.toBe(tree[0]); // sessions changed → new object
    expect(out[0].sessions).toHaveLength(1);
  });

  it("no matches → empty array", () => {
    expect(filterTree(tree, "zzz")).toHaveLength(0);
  });
});
