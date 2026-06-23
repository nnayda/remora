import { describe, expect, it } from "vitest";
import type {
  EditableConfigDto,
  EditorConfigDto,
  EditorHostDto,
} from "./bindings";
import {
  addArg,
  agentFormFromDto,
  buildSettingsModel,
  emptyAgentForm,
  emptyHostForm,
  emptyProjectForm,
  hostFormFromDto,
  moveArg,
  projectFormFromDto,
  removeArg,
  setArg,
  toAgentInput,
  toHostInput,
  toProjectInput,
  validateAgentForm,
  validateHostForm,
  validateProjectForm,
} from "./config-editor-model";

function editorConfig(over: Partial<EditorConfigDto> = {}): EditorConfigDto {
  return { hosts: [], projects: [], agents: [], ...over };
}

function editable(over: Partial<EditableConfigDto> = {}): EditableConfigDto {
  return {
    config: editorConfig(),
    issues: [],
    present: { hosts: [], projects: [], agents: [] },
    ...over,
  };
}

const sshHostDto: EditorHostDto = {
  id: "devbox",
  name: "Dev box",
  transport: { kind: "ssh", host: "h", user: "u", port: 2222 },
};

describe("buildSettingsModel", () => {
  it("exposes the entities and is not degraded when config is present", () => {
    const model = buildSettingsModel(
      editable({
        config: editorConfig({ hosts: [sshHostDto] }),
      }),
    );
    expect(model.degraded).toBe(false);
    expect(model.hosts).toEqual([sshHostDto]);
    expect(model.issues).toEqual([]);
  });

  it("is degraded with empty entity lists when config is null", () => {
    const model = buildSettingsModel(
      editable({
        config: null,
        issues: ["host `a`: unknown transport `telnet`"],
        present: { hosts: ["a", "b"], projects: [], agents: [] },
      }),
    );
    expect(model.degraded).toBe(true);
    expect(model.hosts).toEqual([]);
    expect(model.issues).toHaveLength(1);
    expect(model.present.hosts).toEqual(["a", "b"]);
  });
});

describe("host form", () => {
  it("starts empty as an ssh host", () => {
    const form = emptyHostForm();
    expect(form.id).toBe("");
    expect(form.kind).toBe("ssh");
    expect(form.sshHost).toBe("");
  });

  it("prefills ssh fields from a dto", () => {
    const form = hostFormFromDto(sshHostDto);
    expect(form.id).toBe("devbox");
    expect(form.name).toBe("Dev box");
    expect(form.kind).toBe("ssh");
    expect(form.sshHost).toBe("h");
    expect(form.user).toBe("u");
    expect(form.port).toBe("2222");
  });

  it("prefills kubectl fields from a dto", () => {
    const form = hostFormFromDto({
      id: "kube",
      name: null,
      transport: {
        kind: "kubectl",
        pod: "p",
        namespace: "ns",
        context: null,
        container: null,
      },
    });
    expect(form.kind).toBe("kubectl");
    expect(form.pod).toBe("p");
    expect(form.namespace).toBe("ns");
    expect(form.name).toBe("");
  });

  it("requires a valid slug id only when creating", () => {
    const bad = { ...emptyHostForm(), id: "BAD UPPER", sshHost: "h" };
    expect(validateHostForm(bad, "create")).toMatch(/id/i);
    // In edit mode the id is locked, so its shape is not re-checked.
    expect(validateHostForm(bad, "edit")).toBeNull();
  });

  it("requires the ssh host and a sane port", () => {
    const noHost = { ...emptyHostForm(), id: "ok" };
    expect(validateHostForm(noHost, "create")).toMatch(/host/i);
    const badPort = { ...emptyHostForm(), id: "ok", sshHost: "h", port: "x" };
    expect(validateHostForm(badPort, "create")).toMatch(/port/i);
    const bigPort = {
      ...emptyHostForm(),
      id: "ok",
      sshHost: "h",
      port: "99999",
    };
    expect(validateHostForm(bigPort, "create")).toMatch(/port/i);
  });

  it("requires the kubectl pod", () => {
    const noPod = { ...emptyHostForm(), id: "ok", kind: "kubectl" as const };
    expect(validateHostForm(noPod, "create")).toMatch(/pod/i);
  });

  it("accepts a complete ssh form and builds an ssh input", () => {
    const form = {
      ...emptyHostForm(),
      id: "ok",
      name: " Box ",
      sshHost: " h ",
      user: " ",
      port: "22",
    };
    expect(validateHostForm(form, "create")).toBeNull();
    const input = toHostInput(form);
    expect(input.name).toBe("Box");
    expect(input.transport).toEqual({
      kind: "ssh",
      host: "h",
      user: null,
      port: 22,
    });
  });

  it("builds a kubectl input dropping empty optionals", () => {
    const form = {
      ...emptyHostForm(),
      id: "ok",
      kind: "kubectl" as const,
      pod: "p",
      namespace: "ns",
    };
    const input = toHostInput(form);
    expect(input.transport).toEqual({
      kind: "kubectl",
      pod: "p",
      namespace: "ns",
      context: null,
      container: null,
    });
  });
});

describe("project form", () => {
  const hostIds = ["devbox"];
  const agentIds = ["claude"];

  it("preselects the first host and agent when empty", () => {
    const form = emptyProjectForm(hostIds, agentIds);
    expect(form.hostId).toBe("devbox");
    expect(form.agent).toBe("claude");
    expect(form.workspace).toBe("worktree");
  });

  it("prefills from a dto", () => {
    const form = projectFormFromDto({
      id: "api",
      name: "API",
      hostId: "devbox",
      path: "/srv/api",
      workspace: "shared",
      agent: "claude",
    });
    expect(form.id).toBe("api");
    expect(form.path).toBe("/srv/api");
    expect(form.workspace).toBe("shared");
  });

  it("rejects a non-member host or agent and an empty path", () => {
    const stale = {
      ...emptyProjectForm(hostIds, agentIds),
      id: "ok",
      hostId: "gone",
      path: "/x",
    };
    expect(validateProjectForm(stale, "create", hostIds, agentIds)).toMatch(
      /host/i,
    );
    const noPath = { ...emptyProjectForm(hostIds, agentIds), id: "ok" };
    expect(validateProjectForm(noPath, "create", hostIds, agentIds)).toMatch(
      /path/i,
    );
  });

  it("accepts a complete form and builds the input", () => {
    const form = {
      ...emptyProjectForm(hostIds, agentIds),
      id: "api",
      name: " API ",
      path: " /srv/api ",
    };
    expect(validateProjectForm(form, "create", hostIds, agentIds)).toBeNull();
    expect(toProjectInput(form)).toEqual({
      name: "API",
      hostId: "devbox",
      path: "/srv/api",
      workspace: "worktree",
      agent: "claude",
    });
  });
});

describe("agent form", () => {
  it("starts with a single empty argv row", () => {
    expect(emptyAgentForm().command).toEqual([""]);
  });

  it("prefills argv from a dto", () => {
    const form = agentFormFromDto({ id: "claude", command: ["claude", "-r"] });
    expect(form.id).toBe("claude");
    expect(form.command).toEqual(["claude", "-r"]);
  });

  it("edits argv rows immutably (add/set/remove/move)", () => {
    let cmd = ["a", "b"];
    cmd = addArg(cmd);
    expect(cmd).toEqual(["a", "b", ""]);
    cmd = setArg(cmd, 2, "c");
    expect(cmd).toEqual(["a", "b", "c"]);
    cmd = moveArg(cmd, 2, -1);
    expect(cmd).toEqual(["a", "c", "b"]);
    cmd = removeArg(cmd, 0);
    expect(cmd).toEqual(["c", "b"]);
    // Out-of-range moves are no-ops, not throws.
    expect(moveArg(["x"], 0, -1)).toEqual(["x"]);
  });

  it("rejects an all-blank command", () => {
    const blank = { id: "ok", command: ["", "  "] };
    expect(validateAgentForm(blank, "create")).toMatch(/command/i);
  });

  it("builds an input trimming and dropping blank rows", () => {
    const form = { id: "ok", command: [" claude ", "", "-r"] };
    expect(validateAgentForm(form, "create")).toBeNull();
    expect(toAgentInput(form)).toEqual({ command: ["claude", "-r"] });
  });

  it("rejects an argv row starting with a Unicode dash", () => {
    // Autocorrect/paste turns `--flag` into `—flag` (em-dash); the agent CLI
    // only knows ASCII `-`, so it'd be swallowed as a prompt. Catch it early.
    for (const dash of ["—", "–", "‒", "‐", "―"]) {
      const form = { id: "claude", command: ["claude", `${dash}dangerously`] };
      expect(validateAgentForm(form, "create")).toMatch(/dash/i);
    }
    // ASCII flags stay valid; a non-leading dash is left alone.
    expect(
      validateAgentForm(
        { id: "claude", command: ["claude", "--dangerously", "-r"] },
        "create",
      ),
    ).toBeNull();
    expect(
      validateAgentForm({ id: "claude", command: ["claude", "a—b"] }, "create"),
    ).toBeNull();
  });
});
