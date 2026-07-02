import { describe, expect, it, vi } from "vitest";
import {
  canOpenExternal,
  externalTerminalLabel,
  runCopyAttach,
  runOpenExternal,
} from "./external-terminal";

const GHOSTTY = { id: "ghostty", name: "Ghostty" };
const KITTY = { id: "kitty", name: "kitty" };

describe("externalTerminalLabel", () => {
  it("names the configured registry terminal", () => {
    expect(externalTerminalLabel("ghostty", [GHOSTTY, KITTY])).toBe(
      "Open in Ghostty",
    );
  });
  it("falls back to the generic label for custom argv, unknown id, or nothing", () => {
    expect(externalTerminalLabel(["my-term", "-e"], [GHOSTTY])).toBe(
      "Open in external terminal",
    );
    expect(externalTerminalLabel("st", [GHOSTTY])).toBe(
      "Open in external terminal",
    );
    expect(externalTerminalLabel(null, [GHOSTTY, KITTY])).toBe(
      "Open in external terminal",
    );
  });
  it("names the single detected terminal when nothing is configured", () => {
    // Mirrors the resolver's single-detected auto-pick (spec §3).
    expect(externalTerminalLabel(null, [GHOSTTY])).toBe("Open in Ghostty");
  });
});

describe("canOpenExternal", () => {
  it("gates on live", () => {
    expect(canOpenExternal("live")).toBe(true);
    expect(canOpenExternal("stopped")).toBe(false);
  });
});

describe("runOpenExternal", () => {
  it("passes ids through and stays quiet on success", async () => {
    const open = vi.fn().mockResolvedValue({ status: "ok", data: null });
    const onNotConfigured = vi.fn();
    const onError = vi.fn();
    await runOpenExternal({ open, onNotConfigured, onError }, "api", "s");
    expect(open).toHaveBeenCalledWith("api", "s", null);
    expect(onNotConfigured).not.toHaveBeenCalled();
    expect(onError).not.toHaveBeenCalled();
  });
  it("routes terminalNotConfigured to the settings deep-link", async () => {
    const open = vi.fn().mockResolvedValue({
      status: "error",
      error: { kind: "terminalNotConfigured", message: "pick one" },
    });
    const onNotConfigured = vi.fn();
    const onError = vi.fn();
    await runOpenExternal({ open, onNotConfigured, onError }, "api", "s");
    expect(onNotConfigured).toHaveBeenCalledTimes(1);
    expect(onError).not.toHaveBeenCalled();
  });
  it("routes every other error to the notice with its message", async () => {
    const open = vi.fn().mockResolvedValue({
      status: "error",
      error: { kind: "transport", message: "ghostty exited immediately" },
    });
    const onError = vi.fn();
    await runOpenExternal(
      { open, onNotConfigured: vi.fn(), onError },
      "api",
      "s",
    );
    expect(onError).toHaveBeenCalledWith("ghostty exited immediately");
  });
});

describe("runCopyAttach", () => {
  it("copies via the command and reports errors", async () => {
    const copy = vi.fn().mockResolvedValue({ status: "ok", data: null });
    const onError = vi.fn();
    await runCopyAttach({ copy, onError }, "api", "s");
    expect(copy).toHaveBeenCalledWith("api", "s");
    const failing = vi.fn().mockResolvedValue({
      status: "error",
      error: { kind: "transport", message: "no clipboard" },
    });
    await runCopyAttach({ copy: failing, onError }, "api", "s");
    expect(onError).toHaveBeenCalledWith("no clipboard");
  });
});
