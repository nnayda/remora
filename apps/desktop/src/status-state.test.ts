import { describe, expect, it } from "vitest";
import { sessionIndicatorState, tabIndicatorState } from "./status-state";

describe("sessionIndicatorState", () => {
  it("maps stopped lifecycle to idle regardless of activity", () => {
    expect(sessionIndicatorState("stopped")).toBe("idle");
    expect(sessionIndicatorState("stopped", "working")).toBe("idle");
    expect(sessionIndicatorState("stopped", "awaiting")).toBe("idle");
  });

  it("maps live + working to working", () => {
    expect(sessionIndicatorState("live", "working")).toBe("working");
  });

  it("maps live + awaiting to needs", () => {
    expect(sessionIndicatorState("live", "awaiting")).toBe("needs");
  });

  it("maps live + idle/unknown/undefined to idle", () => {
    expect(sessionIndicatorState("live", "idle")).toBe("idle");
    expect(sessionIndicatorState("live", "unknown")).toBe("idle");
    expect(sessionIndicatorState("live")).toBe("idle");
  });
});

describe("tabIndicatorState", () => {
  it("maps disconnected to error", () => {
    expect(tabIndicatorState("disconnected")).toBe("error");
    expect(tabIndicatorState("disconnected", "working")).toBe("error");
  });

  it("maps reconnecting to working", () => {
    expect(tabIndicatorState("reconnecting")).toBe("working");
    expect(tabIndicatorState("reconnecting", "idle")).toBe("working");
  });

  it("maps stopped to idle", () => {
    expect(tabIndicatorState("stopped")).toBe("idle");
    expect(tabIndicatorState("stopped", "working")).toBe("idle");
  });

  it("delegates live to sessionIndicatorState", () => {
    expect(tabIndicatorState("live", "working")).toBe("working");
    expect(tabIndicatorState("live", "awaiting")).toBe("needs");
    expect(tabIndicatorState("live", "idle")).toBe("idle");
    expect(tabIndicatorState("live", "unknown")).toBe("idle");
    expect(tabIndicatorState("live")).toBe("idle");
  });
});
