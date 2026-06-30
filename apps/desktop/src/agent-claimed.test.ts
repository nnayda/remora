import { describe, expect, it } from "vitest";
import { rowTitle } from "./agent-claimed";

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
