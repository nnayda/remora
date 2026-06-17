import { describe, expect, it } from "vitest";
import { decideReconnect } from "./useReconnect";

describe("decideReconnect", () => {
  it("long gap → all, short gap → stale", () => {
    expect(decideReconnect(20_000)).toBe("all");
    expect(decideReconnect(3_000)).toBe("stale");
    expect(decideReconnect(0)).toBe("stale");
  });

  it("pins the > 15 000 ms boundary exactly", () => {
    expect(decideReconnect(15_000)).toBe("stale");
    expect(decideReconnect(15_001)).toBe("all");
  });
});
