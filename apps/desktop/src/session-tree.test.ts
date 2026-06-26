import { describe, expect, it } from "vitest";
import type { ConfigDto, SessionMetaDto, WorkspaceModeDto } from "./bindings";
import { tabKey } from "./session-store";
import { buildTree } from "./session-tree";

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
  it("stamps host label + transport onto a configured project", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh", "Dev box")], [project("api", "devbox")]),
      [session("api", "fix")],
    );
    expect(tree).toHaveLength(1);
    expect(tree[0]).toMatchObject({
      id: "api",
      label: "api",
      agent: "claude",
      hostLabel: "Dev box",
      transport: "ssh",
      unconfigured: false,
    });
    expect(tree[0].sessions[0]).toMatchObject({
      projectId: "api",
      sessionId: "fix",
      state: "live",
      key: tabKey("api", "fix"),
    });
  });

  it("falls back to host id when the host has no display name", () => {
    const tree = buildTree(
      cfg([host("devbox", "kubectl")], [project("api", "devbox")]),
      [],
    );
    expect(tree[0]).toMatchObject({
      hostLabel: "devbox",
      transport: "kubectl",
    });
  });

  it("a configured host with no projects produces no row (was T3)", () => {
    const tree = buildTree(cfg([host("empty", "ssh")], []), []);
    expect(tree).toHaveLength(0);
  });

  it("renders a configured project with no sessions", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [],
    );
    expect(tree).toHaveLength(1);
    expect(tree[0].sessions).toHaveLength(0);
  });

  it("groups projects by host adjacently regardless of config interleaving", () => {
    // config interleaves hosts: api@h1, web@h2, db@h1 — output must cluster by host.
    const tree = buildTree(
      cfg(
        [host("h1", "ssh"), host("h2", "kubectl")],
        [project("api", "h1"), project("web", "h2"), project("db", "h1")],
      ),
      [],
    );
    expect(tree.map((p) => p.id)).toEqual(["api", "db", "web"]);
    expect(tree.map((p) => p.hostLabel)).toEqual(["h1", "h1", "h2"]);
  });

  it("preserves config order within a host and host order across hosts", () => {
    const tree = buildTree(
      cfg(
        [host("alpha", "kubectl"), host("zeta", "ssh")],
        [project("z2", "zeta"), project("z1", "zeta"), project("a1", "alpha")],
      ),
      [],
    );
    // alpha's projects first (host order), zeta's in config order (z2 before z1)
    expect(tree.map((p) => p.id)).toEqual(["a1", "z2", "z1"]);
  });

  it("collision: same project label on two hosts yields two distinct, host-tagged rows", () => {
    const tree = buildTree(
      cfg(
        [host("hermes", "ssh"), host("atlas", "kubectl")],
        [
          project("remora-hermes", "hermes", "claude", "remora"),
          project("remora-atlas", "atlas", "claude", "remora"),
        ],
      ),
      [],
    );
    expect(tree).toHaveLength(2);
    expect(tree.map((p) => p.label)).toEqual(["remora", "remora"]);
    expect(tree.map((p) => p.hostLabel)).toEqual(["hermes", "atlas"]);
    // distinct ids → distinct React keys
    expect(new Set(tree.map((p) => p.id)).size).toBe(2);
  });

  it("dangling-host project: raw hostId label, unconfigured, before synthetic", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("ghost-proj", "missing-host")]),
      [session("ghost-proj", "s1"), session("orphan", "s2")],
    );
    // configured-but-dangling 'ghost-proj' comes before synthetic 'orphan'
    const ids = tree.map((p) => p.id);
    expect(ids).toEqual(["ghost-proj", "orphan"]);
    expect(tree[0]).toMatchObject({
      hostLabel: "missing-host",
      transport: null,
      unconfigured: true,
    });
    expect(tree[1]).toMatchObject({
      hostLabel: "Unconfigured",
      unconfigured: true,
      agent: null,
    });
    // a dangling project is still configured, so its session flows through the
    // configured branch (and onto the dangling node), not the synthetic one.
    expect(tree[0].sessions.map((s) => s.sessionId)).toEqual(["s1"]);
    expect(tree[1].sessions.map((s) => s.sessionId)).toEqual(["s2"]);
  });

  it("buckets sessions with no config project under a synthetic project, last", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix"), session("ghost", "x")],
    );
    expect(tree).toHaveLength(2);
    const last = tree[tree.length - 1];
    expect(last).toMatchObject({
      id: "ghost",
      unconfigured: true,
      hostLabel: "Unconfigured",
    });
    expect(last.sessions[0].sessionId).toBe("x");
  });

  it("with empty config, every session is a synthetic unconfigured project", () => {
    const tree = buildTree(cfg([], []), [
      session("api", "fix"),
      session("api", "feat"),
    ]);
    expect(tree).toHaveLength(1);
    expect(tree[0]).toMatchObject({ id: "api", unconfigured: true });
    // both sessions of project "api" grouped under one synthetic project
    expect(tree[0].sessions).toHaveLength(2);
  });

  it("carries live and stopped state through to nodes", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix", "live"), session("api", "old", "stopped")],
    );
    const byId = Object.fromEntries(
      tree[0].sessions.map((s) => [s.sessionId, s.state]),
    );
    expect(byId).toEqual({ fix: "live", old: "stopped" });
  });

  it("dedupes sessions sharing a (projectId, sessionId) so React keys stay unique", () => {
    // The SessionSource trait does not promise unique (project, session) tuples
    // (source.rs), and stage-12 multi-host aggregation can surface the same one
    // twice. buildTree must not emit duplicate node keys.
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix"), session("api", "fix")],
    );
    expect(tree[0].sessions).toHaveLength(1);
    expect(tree[0].sessions[0].key).toBe(tabKey("api", "fix"));
  });

  it("dedupes unconfigured sessions sharing a key too", () => {
    const tree = buildTree(cfg([], []), [
      session("ghost", "x"),
      session("ghost", "x"),
    ]);
    expect(tree[0].sessions).toHaveLength(1);
  });

  it("carries workspace mode from a configured worktree project onto its session leaf", () => {
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("api", "devbox", "claude", null, "worktree")],
      ),
      [session("api", "fix")],
    );
    expect(tree[0].sessions[0].workspace).toBe("worktree");
  });

  it("carries workspace mode 'shared' from a configured shared project onto its session leaf", () => {
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("api", "devbox", "claude", null, "shared")],
      ),
      [session("api", "fix")],
    );
    expect(tree[0].sessions[0].workspace).toBe("shared");
  });

  it("carries null workspace for unconfigured-project sessions", () => {
    const tree = buildTree(cfg([], []), [session("ghost", "x")]);
    expect(tree[0].sessions[0].workspace).toBeNull();
  });

  it("stamps node.workspace from discovered meta, overriding the project default", () => {
    // project "scratch" configured shared; discovered session reports worktree
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("scratch", "devbox", "claude", null, "shared")],
      ),
      [
        {
          projectId: "scratch",
          sessionId: "s1",
          state: "stopped",
          agent: null,
          createdAt: null,
          workspacePath: null,
          workspace: "worktree",
        },
      ],
    );
    const node = tree
      .flatMap((p) => p.sessions)
      .find((s) => s.sessionId === "s1");
    expect(node?.workspace).toBe("worktree");
  });

  it("marks a session reconnecting when its key is in the reconnecting set", () => {
    const tree = buildTree(
      cfg([host("devbox", "ssh")], [project("api", "devbox")]),
      [session("api", "fix"), session("api", "other")],
      new Set([tabKey("api", "fix")]),
    );
    const rows = tree[0].sessions;
    expect(rows.find((s) => s.sessionId === "fix")?.reconnecting).toBe(true);
    expect(rows.find((s) => s.sessionId === "other")?.reconnecting).toBe(false);
  });

  it("falls back to the project default when discovered workspace is null", () => {
    const tree = buildTree(
      cfg(
        [host("devbox", "ssh")],
        [project("api", "devbox", "claude", null, "worktree")],
      ),
      [
        {
          projectId: "api",
          sessionId: "s1",
          state: "live",
          agent: null,
          createdAt: null,
          workspacePath: null,
          workspace: null,
        },
      ],
    );
    const node = tree
      .flatMap((p) => p.sessions)
      .find((s) => s.sessionId === "s1");
    expect(node?.workspace).toBe("worktree"); // api is worktree-default
  });
});
