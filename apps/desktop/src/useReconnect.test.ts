import { describe, expect, it } from "vitest";
import { decideReconnect } from "./useReconnect";

describe("decideReconnect", () => {
  it("long gap → all, short gap → stale", () => {
    expect(decideReconnect(20_000)).toBe("all");
    expect(decideReconnect(3_000)).toBe("stale");
    expect(decideReconnect(0)).toBe("stale");
  });
});
