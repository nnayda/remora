import { describe, expect, it } from "vitest";
import {
  capWithEllipsis,
  decodeBase64Utf8,
  stripTerminalEscapes,
} from "./terminal-text";

describe("stripTerminalEscapes", () => {
  it("strips CSI/OSC/charset escapes", () => {
    expect(stripTerminalEscapes("\x1b[31mred\x1b[0m")).toBe("red");
    expect(stripTerminalEscapes("\x1b]0;title\x07x")).toBe("x");
    expect(stripTerminalEscapes("\x1b(Bok")).toBe("ok");
  });
  it("keeps \\t\\n\\r when keepWhitespace (default true)", () => {
    expect(stripTerminalEscapes("a\nb\tc")).toBe("a\nb\tc");
  });
  it("strips all control chars incl. \\t\\n\\r when keepWhitespace=false", () => {
    expect(stripTerminalEscapes("a\nb\tc", { keepWhitespace: false })).toBe(
      "abc",
    );
  });
});

describe("capWithEllipsis", () => {
  it("returns text unchanged within the cap", () => {
    expect(capWithEllipsis("hello", 10)).toBe("hello");
  });
  it("truncates with an ellipsis past the cap", () => {
    expect(capWithEllipsis("abcdef", 4)).toBe("abc…");
  });
});

describe("decodeBase64Utf8", () => {
  it("decodes UTF-8 base64", () => {
    expect(decodeBase64Utf8(btoa("idle"))).toBe("idle");
  });
  it("throws on malformed base64", () => {
    expect(() => decodeBase64Utf8("!!!!")).toThrow();
  });
});
