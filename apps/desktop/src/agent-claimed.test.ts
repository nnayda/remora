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
