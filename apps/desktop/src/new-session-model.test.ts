import { describe, expect, it } from "vitest";
import type { ConfigDto } from "./bindings";
import { buildNewSessionModel, resolveSelection } from "./new-session-model";

function config(over: Partial<ConfigDto> = {}): ConfigDto {
  return { hosts: [], projects: [], agents: [], ...over };
}

describe("buildNewSessionModel", () => {
  it("maps each project to id, label, host label, and default agent", () => {
    const model = buildNewSessionModel(
      config({
        hosts: [{ id: "hermes", name: "Hermes box", transport: "ssh" }],
        projects: [
          {
            id: "api",
            name: "API",
            hostId: "hermes",
            agent: "claude",
            workspace: "worktree" as const,
          },
        ],
        agents: [{ id: "claude" }],
      }),
    );

    expect(model.projects).toEqual([
      {
        id: "api",
        label: "API",
        hostLabel: "Hermes box",
        defaultAgent: "claude",
        defaultWorkspace: "worktree",
      },
    ]);
  });

  it("carries each project's default workspace mode", () => {
    const model = buildNewSessionModel(
      config({
        hosts: [{ id: "h", name: null, transport: "ssh" }],
        projects: [
          {
            id: "api",
            name: null,
            hostId: "h",
            agent: "claude",
            workspace: "worktree" as const,
          },
          {
            id: "scratch",
            name: null,
            hostId: "h",
            agent: "claude",
            workspace: "shared" as const,
          },
        ],
        agents: [{ id: "claude" }],
      }),
    );
    expect(model.projects.find((p) => p.id === "api")?.defaultWorkspace).toBe(
      "worktree",
    );
  });

  it("falls back to the project id and host id when names are absent", () => {
    const model = buildNewSessionModel(
      config({
        hosts: [{ id: "hermes", name: null, transport: "ssh" }],
        projects: [
          {
            id: "api",
            name: null,
            hostId: "hermes",
            agent: "claude",
            workspace: "worktree" as const,
          },
        ],
        agents: [{ id: "claude" }],
      }),
    );

    expect(model.projects[0].label).toBe("api");
    expect(model.projects[0].hostLabel).toBe("hermes");
  });

  it("falls back to the host id when the host is not in config", () => {
    const model = buildNewSessionModel(
      config({
        projects: [
          {
            id: "api",
            name: null,
            hostId: "ghost",
            agent: "claude",
            workspace: "worktree" as const,
          },
        ],
        agents: [{ id: "claude" }],
      }),
    );

    expect(model.projects[0].hostLabel).toBe("ghost");
  });

  it("lists configured agent ids in config order", () => {
    const model = buildNewSessionModel(
      config({ agents: [{ id: "claude" }, { id: "codex" }] }),
    );

    expect(model.agents).toEqual(["claude", "codex"]);
  });

  it("returns empty lists for an empty config", () => {
    const model = buildNewSessionModel(config());

    expect(model.projects).toEqual([]);
    expect(model.agents).toEqual([]);
  });
});

describe("resolveSelection", () => {
  const model = buildNewSessionModel(
    config({
      hosts: [{ id: "h", name: null, transport: "ssh" }],
      projects: [
        {
          id: "api",
          name: null,
          hostId: "h",
          agent: "claude",
          workspace: "worktree" as const,
        },
        {
          id: "web",
          name: null,
          hostId: "h",
          agent: "codex",
          workspace: "worktree" as const,
        },
        {
          id: "scratch",
          name: null,
          hostId: "h",
          agent: "claude",
          workspace: "shared" as const,
        },
      ],
      agents: [{ id: "claude" }, { id: "codex" }],
    }),
  );

  it("resolves a valid project to itself and its default agent", () => {
    expect(resolveSelection(model, "web")).toEqual({
      projectId: "web",
      agent: "codex",
      workspace: "worktree",
    });
  });

  it("clamps an unknown project to the first project and its default agent", () => {
    expect(resolveSelection(model, "ghost")).toEqual({
      projectId: "api",
      agent: "claude",
      workspace: "worktree",
    });
  });

  it("returns empty strings when there are no projects", () => {
    expect(
      resolveSelection(buildNewSessionModel(config()), "anything"),
    ).toEqual({ projectId: "", agent: "", workspace: "worktree" });
  });

  it("resolveSelection returns the project's workspace", () => {
    expect(resolveSelection(model, "scratch").workspace).toBe("shared");
  });
});
