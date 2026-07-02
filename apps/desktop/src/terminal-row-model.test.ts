import { describe, expect, it } from "vitest";
import { terminalRowModel } from "./terminal-row-model";

const GHOSTTY = { id: "ghostty", name: "Ghostty" };
const KITTY = { id: "kitty", name: "kitty" };

describe("terminalRowModel", () => {
  it("select mode: detected options plus Not set, current selection marked", () => {
    const m = terminalRowModel("kitty", [GHOSTTY, KITTY]);
    expect(m).toEqual({
      mode: "select",
      current: "kitty",
      options: [
        { id: "ghostty", name: "Ghostty" },
        { id: "kitty", name: "kitty" },
      ],
      hint: "2 terminals detected",
    });
  });
  it("unset preference selects nothing and hints at detection state", () => {
    expect(terminalRowModel(null, [])).toEqual({
      mode: "select",
      current: null,
      options: [],
      hint: "none detected — install one or set a custom command in the config file",
    });
  });
  it("configured-but-uninstalled id stays selected so the user sees the stale value", () => {
    const m = terminalRowModel("alacritty", [GHOSTTY]);
    // The model itself doesn't know "alacritty" is uninstalled — it just
    // passes the preference through unchanged; the renderer appends a
    // "(not installed)" option for it (asserted via toEqual, not narrowed
    // property access, so this stays a plain type under strict mode).
    expect(m).toEqual({
      mode: "select",
      current: "alacritty",
      options: [GHOSTTY],
      hint: "1 terminal detected",
    });
  });
  it("custom argv is read-only and never clobbered", () => {
    expect(terminalRowModel(["my-term", "-e"], [GHOSTTY])).toEqual({
      mode: "custom",
      display: "Custom (config file): my-term -e",
    });
  });
});
