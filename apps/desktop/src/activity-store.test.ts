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

  it("setPreview notifies subscribers (via the preview snapshot, not the status snapshot)", () => {
    const s = new ActivityStore();
    const cb = vi.fn();
    s.subscribe(cb);
    s.setPreview("p/a", "run tests?");
    expect(cb).toHaveBeenCalledTimes(1);
  });

  it("exposes previews via a reactive snapshot and notifies on setPreview", () => {
    const store = new ActivityStore();
    const listener = vi.fn();
    store.subscribe(listener);

    store.setPreview("k", "Approve running tests?");

    expect(store.getPreviewSnapshot().get("k")).toBe("Approve running tests?");
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("setPreview does not churn the status snapshot identity", () => {
    const store = new ActivityStore();
    store.setStatus("k", "awaiting");
    const before = store.getSnapshot();

    store.setPreview("k", "anything");

    // Status consumers must not see a change (referential equality preserved).
    expect(store.getSnapshot()).toBe(before);
  });

  it("clear drops the preview from the snapshot and notifies", () => {
    const store = new ActivityStore();
    store.setPreview("k", "x");
    const listener = vi.fn();
    store.subscribe(listener);

    store.clear("k");

    expect(store.getPreviewSnapshot().has("k")).toBe(false);
    expect(listener).toHaveBeenCalled();
  });

  it("getSnapshot returns a stable reference until a change", () => {
    const s = new ActivityStore();
    s.setStatus("p/a", "working");
    const snap = s.getSnapshot();
    expect(s.getSnapshot()).toBe(snap); // useSyncExternalStore identity stability
    s.setStatus("p/a", "idle");
    expect(s.getSnapshot()).not.toBe(snap);
  });

  it("setPreview does not notify on unchanged text", () => {
    const store = new ActivityStore();
    const listener = vi.fn();
    store.subscribe(listener);
    store.setPreview("k", "x");
    store.setPreview("k", "x");
    expect(listener).toHaveBeenCalledTimes(1);
  });

  it("expires a stale preview when the status changes", () => {
    // A preview belongs to its status episode: when the agent leaves (or
    // re-enters) a state, a prior preview must not linger and re-show as the
    // current prompt. A fresh marker that carries a preview re-sets it on the
    // following setPreview call.
    const store = new ActivityStore();
    store.setStatus("k", "awaiting");
    store.setPreview("k", "Approve running tests?");
    expect(store.getPreviewSnapshot().get("k")).toBe("Approve running tests?");

    store.setStatus("k", "working"); // agent resumed — prior prompt is stale
    expect(store.getPreviewSnapshot().has("k")).toBe(false);
    expect(store.getPreview("k")).toBeUndefined();
  });
});
