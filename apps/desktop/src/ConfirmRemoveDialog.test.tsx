// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { WorkspaceModeDto } from "./bindings";
import { ConfirmRemoveDialog } from "./ConfirmRemoveDialog";
import type { RemoveResult } from "./session-store";

// ---------------------------------------------------------------------------
// Mocks
// ---------------------------------------------------------------------------

vi.mock("./ui/icons", () => ({
  AlertTriangle: () => null,
  X: () => null,
}));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function renderDialog(workspace: WorkspaceModeDto | null) {
  render(
    <ConfirmRemoveDialog
      projectId="my-project"
      sessionId="my-session"
      workspace={workspace}
      onConfirm={async () => ({ ok: true }) as RemoveResult}
      onClose={() => {}}
    />,
  );
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("ConfirmRemoveDialog — shared workspace", () => {
  it("shows 'Close session' title, no destructive copy, and a 'Close' button", () => {
    renderDialog("shared");

    // Title reads "Close session" (getByText throws if absent)
    expect(screen.queryByText("Close session")).not.toBeNull();

    // The destructive worktree note must NOT appear
    expect(screen.queryByText(/deletes its worktree and branch/)).toBeNull();

    // Footer button label is "Close" (non-destructive)
    // queryAllByText avoids throws if both <span> and its <button> match
    expect(screen.queryAllByText("Close").length).toBeGreaterThan(0);
  });
});

describe("ConfirmRemoveDialog — worktree workspace (unchanged behaviour)", () => {
  it("shows 'Remove session' title, destructive copy, and a 'Remove' button", () => {
    renderDialog("worktree");

    // Title reads "Remove session"
    expect(screen.queryByText("Remove session")).not.toBeNull();

    // The destructive worktree note must be present
    expect(
      screen.queryAllByText(/deletes its worktree and branch/).length,
    ).toBeGreaterThan(0);

    // Footer button label is "Remove"
    expect(screen.queryAllByText("Remove").length).toBeGreaterThan(0);
  });
});

describe("ConfirmRemoveDialog — null workspace (unknown mode)", () => {
  it("renders the destructive 'Remove session' copy, never 'Close session'", () => {
    // A null workspace mode (unconfigured project) must NOT be treated as
    // shared: the gate is `workspace === "shared"`, so null falls through to
    // the conservative destructive rendering.
    renderDialog(null);

    expect(screen.queryByText("Remove session")).not.toBeNull();
    expect(screen.queryByText("Close session")).toBeNull();
  });
});
