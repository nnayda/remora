import { describe, expect, it } from "vitest";
import { previewWhenAwaiting, rowTitle } from "./agent-claimed";

describe("rowTitle", () => {
  it("frames a preview as sandbox-claimed", () => {
    expect(rowTitle({ preview: "Approve running tests?" })).toBe(
      "the session says: Approve running tests?",
    );
  });

  it("falls back to the provided title when there is no preview", () => {
    expect(
      rowTitle({ preview: undefined, fallback: "Stopped — click to respawn" }),
    ).toBe("Stopped — click to respawn");
  });

  it("prefers the preview over the fallback", () => {
    expect(rowTitle({ preview: "Pick a file", fallback: "Stopped" })).toBe(
      "the session says: Pick a file",
    );
  });

  it("returns undefined when neither is present", () => {
    expect(rowTitle({ preview: undefined })).toBeUndefined();
  });

  it("appends the hook affirmation below a preview", () => {
    expect(
      rowTitle({ preview: "Approve running tests?", hookActive: true }),
    ).toBe("the session says: Approve running tests?\nActivity hook active");
  });

  it("shows the affirmation alone when there is no preview or fallback", () => {
    expect(rowTitle({ hookActive: true })).toBe("Activity hook active");
  });

  it("shows the affirmation below a stopped fallback", () => {
    expect(
      rowTitle({ fallback: "Stopped — click to respawn", hookActive: true }),
    ).toBe("Stopped — click to respawn\nActivity hook active");
  });

  it("omits the affirmation when the hook is not confirmed", () => {
    expect(rowTitle({ preview: "Pick a file", hookActive: false })).toBe(
      "the session says: Pick a file",
    );
    expect(rowTitle({ hookActive: false })).toBeUndefined();
  });
});

describe("previewWhenAwaiting", () => {
  it("returns preview when activity is awaiting", () => {
    expect(previewWhenAwaiting("awaiting", "Approve running tests?")).toBe(
      "Approve running tests?",
    );
  });

  it("returns undefined when activity is working", () => {
    expect(previewWhenAwaiting("working", "stale preview")).toBeUndefined();
  });

  it("returns undefined when activity is idle", () => {
    expect(previewWhenAwaiting("idle", "stale preview")).toBeUndefined();
  });

  it("returns undefined when activity is unknown", () => {
    expect(previewWhenAwaiting("unknown", "stale preview")).toBeUndefined();
  });

  it("returns undefined when activity is undefined", () => {
    expect(previewWhenAwaiting(undefined, "stale preview")).toBeUndefined();
  });

  it("returns undefined when preview is undefined even if awaiting", () => {
    expect(previewWhenAwaiting("awaiting", undefined)).toBeUndefined();
  });
});
