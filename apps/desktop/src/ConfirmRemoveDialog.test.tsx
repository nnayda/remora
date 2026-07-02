// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { DirtyReasonDto, WorkspaceModeDto } from "./bindings";
import { ConfirmRemoveDialog } from "./ConfirmRemoveDialog";

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

function renderDialog(
  workspace: WorkspaceModeDto | null,
  opts: {
    forceReason?: DirtyReasonDto | null;
    onConfirm?: (force: boolean) => void;
    onClose?: () => void;
  } = {},
) {
  render(
    <ConfirmRemoveDialog
      projectId="my-project"
      sessionId="my-session"
      workspace={workspace}
      forceReason={opts.forceReason ?? null}
      onConfirm={opts.onConfirm ?? (() => {})}
      onClose={opts.onClose ?? (() => {})}
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

describe("ConfirmRemoveDialog — fire-and-forget confirm", () => {
  it("clicking Remove fires onConfirm(false) once and does not call onClose itself", () => {
    const onConfirm = vi.fn();
    const onClose = vi.fn();
    renderDialog("worktree", { onConfirm, onClose });

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));

    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onConfirm).toHaveBeenCalledWith(false);
    // App owns closing the dialog (it nulls removeTarget in its confirm
    // handler); the dialog must not double-close.
    expect(onClose).not.toHaveBeenCalled();
  });

  it("Cancel calls onClose without firing onConfirm", () => {
    const onConfirm = vi.fn();
    const onClose = vi.fn();
    renderDialog("worktree", { onConfirm, onClose });

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onConfirm).not.toHaveBeenCalled();
  });
});

describe("ConfirmRemoveDialog — force stage from forceReason", () => {
  it("opens directly at the force stage with mapped reason copy", () => {
    renderDialog("worktree", { forceReason: "uncommitted" });

    expect(screen.queryByText("Remove anyway?")).not.toBeNull();
    expect(
      screen.queryByText(/This session has uncommitted changes\./),
    ).not.toBeNull();
    expect(screen.queryAllByText("Remove anyway").length).toBeGreaterThan(0);
  });

  it("confirming the force stage fires onConfirm(true)", () => {
    const onConfirm = vi.fn();
    renderDialog("worktree", { forceReason: "notOnRemote", onConfirm });

    fireEvent.click(screen.getByRole("button", { name: "Remove anyway" }));

    expect(onConfirm).toHaveBeenCalledTimes(1);
    expect(onConfirm).toHaveBeenCalledWith(true);
  });
});
