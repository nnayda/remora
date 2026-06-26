// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConfigDto } from "./bindings";
import { deriveSessionId } from "./derive-session-id";
import { NewSessionDialog } from "./NewSessionDialog";
import type { OpenResult, SpawnInput } from "./session-store";
import { OPEN_CANCELLED } from "./session-store";

// ---------------------------------------------------------------------------
// Vitest / JSDOM mocks for Tauri-specific and icon modules
// ---------------------------------------------------------------------------

vi.mock("@tauri-apps/api/core", () => ({
  Channel: class {
    onmessage: ((m: unknown) => void) | null = null;
  },
}));

vi.mock("./ui/icons", () => ({
  Terminal: () => null,
  X: () => null,
  ChevronDown: () => null,
}));

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function makeConfig(over: Partial<ConfigDto> = {}): ConfigDto {
  return {
    hosts: [{ id: "h", name: "My Host", transport: "ssh" }],
    projects: [
      {
        id: "proj",
        name: "My Project",
        hostId: "h",
        agent: "claude",
        workspace: "worktree" as const,
      },
    ],
    agents: [{ id: "claude" }],
    ...over,
  };
}

function fillBranch(value: string) {
  const input = screen.getByRole("textbox", { name: /branch name/i });
  fireEvent.change(input, { target: { value } });
}

function fillWorktreeRoot(value: string) {
  const input = screen.getByRole("textbox", { name: /worktree root/i });
  fireEvent.change(input, { target: { value } });
}

function clickOpen() {
  const btn = screen.getByRole("button", { name: /^open$/i });
  fireEvent.click(btn);
}

function openButton() {
  return screen.getByRole("button", { name: /^open$/i });
}

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

// Use the exact prop types so renders below are type-safe without casts.
type OpenSessionFn = (input: SpawnInput) => Promise<OpenResult>;
type OnOpenedFn = (result: { attached: boolean; opened: boolean }) => void;
type OnCloseFn = () => void;

let openSession: ReturnType<typeof vi.fn<OpenSessionFn>>;
let onOpened: ReturnType<typeof vi.fn<OnOpenedFn>>;
let onClose: ReturnType<typeof vi.fn<OnCloseFn>>;

function renderDialog(configOver: Partial<ConfigDto> = {}) {
  const cfg = makeConfig(configOver);
  render(
    <NewSessionDialog
      config={cfg}
      openSession={openSession}
      onOpened={onOpened}
      onClose={onClose}
    />,
  );
}

beforeEach(() => {
  openSession = vi.fn<OpenSessionFn>();
  onOpened = vi.fn<OnOpenedFn>();
  onClose = vi.fn<OnCloseFn>();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("NewSessionDialog — Branch name field", () => {
  it("renders a 'Branch name' text input", () => {
    renderDialog();
    expect(screen.getByRole("textbox", { name: /branch name/i })).toBeDefined();
  });

  it("does NOT render a 'Session' text input (old sessionId field is gone)", () => {
    renderDialog();
    const inputs = screen
      .queryAllByRole("textbox")
      .filter((el) => el.getAttribute("aria-label") === "Session");
    expect(inputs).toHaveLength(0);
  });

  it("Open button is disabled when branch is empty", () => {
    renderDialog();
    expect(openButton().getAttribute("disabled")).not.toBeNull();
  });

  it("Open button is enabled when branch is non-empty and valid", () => {
    renderDialog();
    fillBranch("feat/login");
    expect(openButton().getAttribute("disabled")).toBeNull();
  });

  it("Open button is disabled when branch produces a null sessionId (too long)", () => {
    renderDialog();
    // A branch that slugifies to a slug longer than 64 chars overflows; the
    // derived id is null and submit must stay disabled.
    const longBranch = `feat/${"x".repeat(60)}-and-more-to-overflow`;
    // Precondition: confirm the test data actually yields a null derived id,
    // so the assertion below is never silently skipped.
    expect(deriveSessionId(longBranch)).toBeNull();
    fillBranch(longBranch);
    expect(openButton().getAttribute("disabled")).not.toBeNull();
  });

  it("submit sends sessionId = deriveSessionId(branch) and branch = 'feat/login'", async () => {
    openSession.mockResolvedValue({
      ok: true,
      attached: false,
      opened: true,
    } as OpenResult);
    renderDialog();
    fillBranch("feat/login");
    clickOpen();
    await vi.waitFor(() => expect(openSession).toHaveBeenCalledOnce());
    const [payload] = openSession.mock.calls[0] as [SpawnInput];
    expect(payload.branch).toBe("feat/login");
    expect(payload.sessionId).toBe(deriveSessionId("feat/login"));
  });

  it("worktreeRoot value is passed through when provided", async () => {
    openSession.mockResolvedValue({
      ok: true,
      attached: false,
      opened: true,
    } as OpenResult);
    renderDialog();
    fillBranch("feat/login");
    fillWorktreeRoot("/home/user/projects/my-repo");
    clickOpen();
    await vi.waitFor(() => expect(openSession).toHaveBeenCalledOnce());
    const [payload] = openSession.mock.calls[0] as [SpawnInput];
    expect(payload.worktreeRoot).toBe("/home/user/projects/my-repo");
  });

  it("worktreeRoot is null when field is left blank", async () => {
    openSession.mockResolvedValue({
      ok: true,
      attached: false,
      opened: true,
    } as OpenResult);
    renderDialog();
    fillBranch("feat/login");
    clickOpen();
    await vi.waitFor(() => expect(openSession).toHaveBeenCalledOnce());
    const [payload] = openSession.mock.calls[0] as [SpawnInput];
    expect(payload.worktreeRoot).toBeNull();
  });

  it("does not submit when branch is empty (guard holds)", () => {
    renderDialog();
    // No branch filled — attempt a direct form submission
    const form = document.getElementById("new-session-form");
    if (form) fireEvent.submit(form);
    expect(openSession).not.toHaveBeenCalled();
  });

  it("shows an inline hint when a non-empty branch yields null sessionId", () => {
    renderDialog();
    // A branch that exceeds the max length after slugification (64+ chars of
    // slug plus the 9-char "-XXXXXXXX" hash suffix), so the derived id is null.
    const longBranch = `feat/${"a".repeat(60)}-overflow`;
    // Precondition: confirm the test data actually yields a null derived id.
    expect(deriveSessionId(longBranch)).toBeNull();
    fillBranch(longBranch);
    const hint = screen.queryByText(/too long|not a valid/i);
    expect(hint).not.toBeNull();
  });
});

describe("NewSessionDialog — error path", () => {
  it("displays a spawn rejection message", async () => {
    openSession.mockResolvedValue({
      ok: false,
      error: new Error("invalid branch: my!bad"),
    } as OpenResult);
    renderDialog();
    fillBranch("feat/login");
    clickOpen();
    await vi.waitFor(() =>
      expect(screen.queryByText(/invalid branch/i)).not.toBeNull(),
    );
  });

  it("does not show OPEN_CANCELLED as an error", async () => {
    openSession.mockResolvedValue({
      ok: false,
      error: OPEN_CANCELLED,
    } as OpenResult);
    renderDialog();
    fillBranch("feat/login");
    clickOpen();
    // Give the async path time to run
    await vi.waitFor(() => expect(openSession).toHaveBeenCalledOnce());
    expect(screen.queryByText(/cancelled/i)).toBeNull();
  });
});
