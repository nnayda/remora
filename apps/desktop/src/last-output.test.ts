import { describe, expect, it } from "vitest";
import { extractCause } from "./last-output";

const enc = (s: string): Uint8Array => new TextEncoder().encode(s);

describe("extractCause", () => {
  it("returns the last non-empty line of plain text", () => {
    expect(extractCause(enc("starting\nclaude: command not found\n"))).toBe(
      "claude: command not found",
    );
  });

  it("strips CSI colour escapes around the line", () => {
    // `\x1b[31m` … `\x1b[0m` is a red SGR wrap the agent emits around an error.
    expect(
      extractCause(enc("\x1b[31mclaude: command not found\x1b[0m\n")),
    ).toBe("claude: command not found");
  });

  it("strips OSC sequences (e.g. window-title sets)", () => {
    // OSC 0 ; title BEL — a title set tmux/agents emit; must not leak into text.
    expect(extractCause(enc("\x1b]0;some title\x07auth failed\n"))).toBe(
      "auth failed",
    );
  });

  it("takes the final segment of a carriage-return-overwritten line", () => {
    // A progress line overwritten in place: only the post-`\r` content is shown.
    expect(extractCause(enc("downloading 50%\rdownloading 100%\n"))).toBe(
      "downloading 100%",
    );
  });

  it("ignores trailing blank lines", () => {
    expect(extractCause(enc("real error here\n\n  \n"))).toBe(
      "real error here",
    );
  });

  it("picks the last non-empty line from multi-line output", () => {
    expect(extractCause(enc("line one\nline two\nline three\n"))).toBe(
      "line three",
    );
  });

  it("returns empty string when output is only escapes / whitespace", () => {
    expect(extractCause(enc("\x1b[2J\x1b[H   \n\n"))).toBe("");
  });

  it("returns empty string for empty input", () => {
    expect(extractCause(new Uint8Array())).toBe("");
  });

  it("truncates a long line to maxLen with an ellipsis", () => {
    const long = "x".repeat(300);
    const out = extractCause(enc(`${long}\n`), 200);
    expect(out.length).toBe(200);
    expect(out.endsWith("…")).toBe(true);
    expect(out.startsWith("x".repeat(199))).toBe(true);
  });
});
