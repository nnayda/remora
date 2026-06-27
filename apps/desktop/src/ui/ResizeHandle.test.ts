import { describe, expect, it } from "vitest";
import { nextWidthFromKey, nextWidthFromPointer } from "./ResizeHandle";

const RECT = { left: 100, right: 1000 };

describe("nextWidthFromPointer", () => {
  it("edge:right measures from the rail's left edge", () => {
    expect(nextWidthFromPointer(400, RECT, "right", 180, 480)).toBe(300); // 400-100
  });
  it("edge:left measures from the rail's right edge", () => {
    expect(nextWidthFromPointer(700, RECT, "left", 180, 480)).toBe(300); // 1000-700
  });
  it("clamps to min and max", () => {
    expect(nextWidthFromPointer(120, RECT, "right", 180, 480)).toBe(180); // 20 -> min
    expect(nextWidthFromPointer(2000, RECT, "right", 180, 480)).toBe(480); // 1900 -> max
  });
});

describe("nextWidthFromKey", () => {
  it("edge:right — ArrowRight grows, ArrowLeft shrinks", () => {
    expect(
      nextWidthFromKey({ key: "ArrowRight" }, 300, 8, 180, 480, "right"),
    ).toBe(308);
    expect(
      nextWidthFromKey({ key: "ArrowLeft" }, 300, 8, 180, 480, "right"),
    ).toBe(292);
  });
  it("edge:left — directions invert spatially", () => {
    expect(
      nextWidthFromKey({ key: "ArrowRight" }, 300, 8, 180, 480, "left"),
    ).toBe(292);
    expect(
      nextWidthFromKey({ key: "ArrowLeft" }, 300, 8, 180, 480, "left"),
    ).toBe(308);
  });
  it("Shift quadruples the step", () => {
    expect(
      nextWidthFromKey(
        { key: "ArrowRight", shiftKey: true },
        300,
        8,
        180,
        480,
        "right",
      ),
    ).toBe(332);
  });
  it("Home/End jump to bounds, other keys are ignored", () => {
    expect(nextWidthFromKey({ key: "Home" }, 300, 8, 180, 480, "right")).toBe(
      180,
    );
    expect(nextWidthFromKey({ key: "End" }, 300, 8, 180, 480, "right")).toBe(
      480,
    );
    expect(nextWidthFromKey({ key: "a" }, 300, 8, 180, 480, "right")).toBe(300);
  });
  it("clamps at the bounds", () => {
    expect(
      nextWidthFromKey(
        { key: "ArrowLeft", shiftKey: true },
        190,
        8,
        180,
        480,
        "right",
      ),
    ).toBe(180);
  });
  it("edge:left — clamps at the bounds", () => {
    // ArrowRight on edge:left shrinks (delta is negative), so near-min clamps to min
    expect(
      nextWidthFromKey(
        { key: "ArrowRight", shiftKey: true },
        190,
        8,
        180,
        480,
        "left",
      ),
    ).toBe(180);
  });
  it("edge:left — Shift quadruples the step", () => {
    expect(
      nextWidthFromKey(
        { key: "ArrowLeft", shiftKey: true },
        300,
        8,
        180,
        480,
        "left",
      ),
    ).toBe(332);
  });
});
