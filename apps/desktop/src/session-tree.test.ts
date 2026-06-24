import { describe, expect, it } from "vitest";
import type { ConfigDto, SessionMetaDto, WorkspaceModeDto } from "./bindings";
import { tabKey } from "./session-store";
import { buildTree, UNCONFIGURED_HOST_ID } from "./session-tree";

const host = (
  id: string,
  transport: "ssh" | "kubectl",
  name: string | null = null,
) => ({
  id,
  name,
  transport,
});
const project = (
  id: string,
  hostId: string,
  agent = "claude",
  name: string | null = null,
  workspace: WorkspaceModeDto = "worktree",
) => ({
  id,
  name,
  hostId,
  agent,
  workspace,
});
const session = (
  projectId: string,
  sessionId: string,
  state: "live" | "stopped" = "live",
  agent: string | null = null,
): SessionMetaDto => ({
  projectId,
  sessionId,
  state,
  agent,
  createdAt: null,
  workspacePath: null,
  workspace: null,
});

const cfg = (
  hosts: ConfigDto["hosts"],
  projects: ConfigDto["projects"],
): ConfigDto => ({ hosts, projects, agents: [] });

describe("buildTree", () => {
  it("nests projects under their configured host", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh", "Dev box")], [project("api", "devbox")]),
      [session("api", "fix")],
    );
    expect(tree).toHaveLength(1);
    expect(tree[0]).toMatchObject({
      id: "devbox",
      label: "Dev box",
      unconfigured: false,
    });
    expect(tree[0].projects[0]).toMatchObject({
      id: "api",
      label: "api",
      agent: "claude",
    });
    expect(tree[0].projects[0].sessions[0]).toMatchObject({
      projectId: "api",
      sessionId: "fix",
      state: "live",
      key: tabKey("api", "fix"),
    });
  });

  it("renders a configured host with no projects (T3)", () => {
    const tree = buildTree(cfg([host("empty", "ssh")], []), []);
    expect(tree).toHaveLength(1);
    expect(tree[0].id).toBe("empty");
    expect(tree[0].projects).toHaveLength(0);
  });

  it("renders a configured project with no sessions", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [],
    );
    expect(tree[0].projects[0].sessions).toHaveLength(0);
  });

  it("buckets sessions with no config project under a synthetic Unconfigured host, rendered last", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix"), session("ghost", "x")],
    );
    expect(tree).toHaveLength(2);
    const last = tree[tree.length - 1];
    expect(last.id).toBe(UNCONFIGURED_HOST_ID);
    expect(last.unconfigured).toBe(true);
    expect(last.projects[0].id).toBe("ghost");
    expect(last.projects[0].sessions[0].sessionId).toBe("x");
  });

  it("with empty config, every session is Unconfigured", () => {
    const tree = buildTree(cfg([], []), [
      session("api", "fix"),
      session("api", "feat"),
    ]);
    expect(tree).toHaveLength(1);
    expect(tree[0].id).toBe(UNCONFIGURED_HOST_ID);
    // both sessions of project "api" grouped under one synthetic project
    expect(tree[0].projects).toHaveLength(1);
    expect(tree[0].projects[0].sessions).toHaveLength(2);
  });

  it("carries live and stopped state through to nodes", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix", "live"), session("api", "old", "stopped")],
    );
    const states = tree[0].projects[0].sessions.map((s) => s.state);
    expect(states).toEqual(
      ["fix", "old"].map((id) => (id === "old" ? "stopped" : "live")),
    );
  });

  it("dedupes sessions sharing a (projectId, sessionId) so React keys stay unique", () => {
    // The SessionSource trait does not promise unique (project, session) tuples
    // (source.rs), and stage-12 multi-host aggregation can surface the same one
    // twice. buildTree must not emit duplicate node keys.
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix"), session("api", "fix")],
    );
    const sessions = tree[0].projects[0].sessions;
    expect(sessions).toHaveLength(1);
    expect(sessions[0].key).toBe(tabKey("api", "fix"));
  });

  it("dedupes unconfigured sessions sharing a key too", () => {
    const tree = buildTree(cfg([], []), [
      session("ghost", "x"),
      session("ghost", "x"),
    ]);
    expect(tree[0].projects[0].sessions).toHaveLength(1);
  });

  it("preserves config host order and does not invent an Unconfigured host when all sessions map", () => {
    const tree = buildTree(
      cfg(
        [host("alpha", "kubectl"), host("zeta", "ssh")],
        [project("api", "zeta"), project("web", "alpha")],
      ),
      [session("api", "s1"), session("web", "s2")],
    );
    expect(tree.map((h) => h.id)).toEqual(["alpha", "zeta"]);
    expect(tree.some((h) => h.unconfigured)).toBe(false);
  });

  it("carries workspace mode from a configured worktree project onto its session leaf", () => {
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("api", "devbox", "claude", null, "worktree")],
      ),
      [session("api", "fix")],
    );
    expect(tree[0].projects[0].sessions[0].workspace).toBe("worktree");
  });

  it("carries workspace mode 'shared' from a configured shared project onto its session leaf", () => {
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("api", "devbox", "claude", null, "shared")],
      ),
      [session("api", "fix")],
    );
    expect(tree[0].projects[0].sessions[0].workspace).toBe("shared");
  });

  it("carries null workspace for unconfigured-project sessions", () => {
    const tree = buildTree(cfg([], []), [session("ghost", "x")]);
    expect(tree[0].projects[0].sessions[0].workspace).toBeNull();
  });
});
