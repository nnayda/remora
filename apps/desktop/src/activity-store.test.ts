import { describe, expect, it } from "vitest";
import { ActivityStore, SETTLE_WINDOW_MS } from "./activity-store";

function fixture() {
  let clock = 1000;
  const store = new ActivityStore({ now: () => clock });
  return {
    store,
    set: (t: number) => {
      clock = t;
    },
    advance: (d: number) => {
      clock += d;
    },
  };
}
const K = "api/fix";

describe("ActivityStore", () => {
  it("first output -> working", () => {
    const { store } = fixture();
    store.noteOutput(K);
    expect(store.getSnapshot().get(K)).toBe("working");
  });

  it("working -> idle after the settle window via sweep", () => {
    const { store, advance } = fixture();
    store.noteOutput(K);
    advance(SETTLE_WINDOW_MS - 1);
    store.sweep();
    expect(store.getSnapshot().get(K)).toBe("working"); // not yet
    advance(1);
    store.sweep();
    expect(store.getSnapshot().get(K)).toBe("idle");
  });

  it("an awaiting marker settles immediately to red, sweep leaves it", () => {
    const { store, advance } = fixture();
    store.noteOutput(K); // working
    store.noteMarker(K, "awaiting");
    expect(store.getSnapshot().get(K)).toBe("awaiting");
    advance(SETTLE_WINDOW_MS + 10);
    store.sweep(); // sweep only touches `working`
    expect(store.getSnapshot().get(K)).toBe("awaiting");
  });

  it("output supersedes a stale awaiting marker back to working", () => {
    const { store } = fixture();
    store.noteMarker(K, "awaiting");
    store.noteOutput(K);
    expect(store.getSnapshot().get(K)).toBe("working");
  });

  it("an idle marker settles to blue, never red", () => {
    const { store } = fixture();
    store.noteMarker(K, "idle");
    expect(store.getSnapshot().get(K)).toBe("idle");
  });

  it("clear removes the entry", () => {
    const { store } = fixture();
    store.noteOutput(K);
    store.clear(K);
    expect(store.getSnapshot().has(K)).toBe(false);
  });

  it("tracks two sessions independently", () => {
    const { store, advance } = fixture();
    store.noteOutput("a");
    advance(SETTLE_WINDOW_MS + 1);
    store.noteOutput("b"); // b just spoke
    store.sweep();
    expect(store.getSnapshot().get("a")).toBe("idle");
    expect(store.getSnapshot().get("b")).toBe("working");
  });

  it("notifies subscribers only on state change, not every output", () => {
    const { store } = fixture();
    let n = 0;
    store.subscribe(() => {
      n += 1;
    });
    store.noteOutput(K); // unknown -> working: 1 notify
    store.noteOutput(K); // still working: no notify
    expect(n).toBe(1);
  });

  it("noteMarker does not notify on same-state flood", () => {
    const { store } = fixture();
    let n = 0;
    store.subscribe(() => {
      n += 1;
    });
    store.noteMarker(K, "awaiting"); // unknown -> awaiting: 1 notify
    store.noteMarker(K, "awaiting"); // still awaiting: no notify
    store.noteMarker(K, "awaiting"); // still awaiting: no notify
    expect(n).toBe(1);
  });
});
