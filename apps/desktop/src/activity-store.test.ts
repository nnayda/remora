import { describe, expect, it, vi } from "vitest";
import { ActivityStore } from "./activity-store";

describe("ActivityStore (passive recorder)", () => {
  it("records the latest status per key and snapshots it", () => {
    const s = new ActivityStore();
    s.setStatus("p/a", "working");
    s.setStatus("p/b", "awaiting");
    s.setStatus("p/a", "idle");
    expect(s.getSnapshot().get("p/a")).toBe("idle");
    expect(s.getSnapshot().get("p/b")).toBe("awaiting");
  });

  it("notifies subscribers only on an actual change", () => {
    const s = new ActivityStore();
    const cb = vi.fn();
    s.subscribe(cb);
    s.setStatus("p/a", "working");
    expect(cb).toHaveBeenCalledTimes(1);
    s.setStatus("p/a", "working"); // same value → no notification
    expect(cb).toHaveBeenCalledTimes(1);
    s.setStatus("p/a", "idle");
    expect(cb).toHaveBeenCalledTimes(2);
  });

  it("stores preview text separately and clears both on clear", () => {
    const s = new ActivityStore();
    s.setStatus("p/a", "awaiting");
    s.setPreview("p/a", "run tests?");
    expect(s.getPreview("p/a")).toBe("run tests?");
    s.clear("p/a");
    expect(s.getSnapshot().has("p/a")).toBe(false);
    expect(s.getPreview("p/a")).toBeUndefined();
  });

  it("getSnapshot returns a stable reference until a change", () => {
    const s = new ActivityStore();
    s.setStatus("p/a", "working");
    const snap = s.getSnapshot();
    expect(s.getSnapshot()).toBe(snap); // useSyncExternalStore identity stability
    s.setStatus("p/a", "idle");
    expect(s.getSnapshot()).not.toBe(snap);
  });
});
