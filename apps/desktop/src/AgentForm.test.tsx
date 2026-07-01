// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AgentForm } from "./AgentForm";
import type { AgentInputDto } from "./bindings";

// ---------------------------------------------------------------------------
// Vitest / JSDOM mocks for icon modules
// ---------------------------------------------------------------------------

vi.mock("./ui/icons", () => ({
  ArrowUp: () => null,
  Plus: () => null,
  Trash: () => null,
}));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type OnSubmitFn = (id: string, input: AgentInputDto) => Promise<void>;
type OnCancelFn = () => void;

let onSubmit: ReturnType<typeof vi.fn<OnSubmitFn>>;
let onCancel: ReturnType<typeof vi.fn<OnCancelFn>>;

function renderForm() {
  render(<AgentForm mode="create" onSubmit={onSubmit} onCancel={onCancel} />);
}

beforeEach(() => {
  onSubmit = vi.fn<OnSubmitFn>();
  onCancel = vi.fn<OnCancelFn>();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("AgentForm — Claude activity-markers template button", () => {
  it("renders the template button in create mode", () => {
    renderForm();
    expect(
      screen.queryByRole("button", {
        name: /claude code \(activity markers\)/i,
      }),
    ).not.toBeNull();
  });

  it("clicking it fills the command with --settings and saves the provision file", async () => {
    onSubmit.mockResolvedValue(undefined);
    renderForm();
    fireEvent.click(
      screen.getByRole("button", { name: /claude code \(activity markers\)/i }),
    );
    fireEvent.change(screen.getByLabelText("Id"), {
      target: { value: "claude" },
    });
    fireEvent.click(screen.getByRole("button", { name: /^save$/i }));
    await vi.waitFor(() => expect(onSubmit).toHaveBeenCalledOnce());
    const [id, input] = onSubmit.mock.calls[0] as [string, AgentInputDto];
    expect(id).toBe("claude");
    expect(input.command).toContain("--settings");
    expect(input.provision?.path).toBe("~/.remora/hooks/claude-notify.sh");
  });
});
