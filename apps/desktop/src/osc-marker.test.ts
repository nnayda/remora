import { describe, expect, it } from "vitest";
import { parseActivityMarker } from "./osc-marker";

const marker = (state: string, ver = "1", type = "state", token = "remora") =>
  `${token};${ver};${type};${btoa(state)}`;

describe("parseActivityMarker", () => {
  it("parses a valid state marker", () => {
    expect(parseActivityMarker(marker("working"))).toEqual({
      state: "working",
    });
    expect(parseActivityMarker(marker("idle"))).toEqual({ state: "idle" });
    expect(parseActivityMarker(marker("awaiting_input"))).toEqual({
      state: "awaiting",
    });
  });
  it("rejects a missing/wrong collision token", () => {
    expect(
      parseActivityMarker(marker("idle", "1", "state", "other")),
    ).toBeNull();
  });
  it("ignores an unsupported version", () => {
    expect(parseActivityMarker(marker("idle", "2"))).toBeNull();
  });
  it("ignores an unsupported type", () => {
    expect(parseActivityMarker(marker("idle", "1", "notify"))).toBeNull();
  });
  it("ignores an unknown state token", () => {
    expect(parseActivityMarker(marker("thinking"))).toBeNull();
  });
  it("ignores malformed base64", () => {
    expect(parseActivityMarker("remora;1;state;!!!!")).toBeNull();
  });
  it("ignores wrong field count", () => {
    expect(parseActivityMarker("remora;1;state")).toBeNull();
    expect(parseActivityMarker("remora;1;state;aWRsZQ==;extra")).toBeNull();
  });
  it("sanitizes a control-laden payload to no-match (never a false state)", () => {
    // base64 of "idle\x1b[31m" -> after strip it is "idle" but the smuggled
    // escape must be gone; a payload that is ONLY controls -> "" -> null.
    expect(parseActivityMarker(`remora;1;state;${btoa("\x1b[2J")}`)).toBeNull();
  });
});
