import { describe, expect, it } from "vitest";
import {
  clampWidth,
  parseRailState,
  shouldRenderCollapsed,
} from "./use-rail-width";

const OPTS = {
  key: "remora.rail.sidebar",
  defaultWidth: 240,
  min: 180,
  max: 480,
};

describe("clampWidth", () => {
  it("rounds and passes through in-range values", () => {
    expect(clampWidth(240.6, 180, 480)).toBe(241);
  });
  it("clamps below min and above max", () => {
    expect(clampWidth(50, 180, 480)).toBe(180);
    expect(clampWidth(9000, 180, 480)).toBe(480);
  });
  it("returns min for non-finite input", () => {
    expect(clampWidth(Number.NaN, 180, 480)).toBe(180);
    expect(clampWidth(Number.POSITIVE_INFINITY, 180, 480)).toBe(180);
  });
});

describe("parseRailState", () => {
  it("returns defaults for null / unparseable / non-object", () => {
    expect(parseRailState(null, OPTS)).toEqual({
      width: 240,
      collapsed: false,
    });
    expect(parseRailState("{not json", OPTS)).toEqual({
      width: 240,
      collapsed: false,
    });
    expect(parseRailState("42", OPTS)).toEqual({
      width: 240,
      collapsed: false,
    });
  });
  it("clamps an out-of-range numeric width but keeps collapsed", () => {
    expect(parseRailState('{"width":5000,"collapsed":true}', OPTS)).toEqual({
      width: 480,
      collapsed: true,
    });
  });
  it("falls back to default width when width is missing or non-numeric", () => {
    expect(parseRailState('{"collapsed":true}', OPTS)).toEqual({
      width: 240,
      collapsed: true,
    });
    expect(parseRailState('{"width":"wide"}', OPTS)).toEqual({
      width: 240,
      collapsed: false,
    });
  });
  it("round-trips a valid persisted value", () => {
    expect(parseRailState('{"width":300,"collapsed":false}', OPTS)).toEqual({
      width: 300,
      collapsed: false,
    });
  });
});

describe("shouldRenderCollapsed", () => {
  it("is true only when collapsed and not mobile", () => {
    expect(shouldRenderCollapsed(true, false)).toBe(true);
    expect(shouldRenderCollapsed(true, true)).toBe(false);
    expect(shouldRenderCollapsed(false, false)).toBe(false);
    expect(shouldRenderCollapsed(false, true)).toBe(false);
  });
});
