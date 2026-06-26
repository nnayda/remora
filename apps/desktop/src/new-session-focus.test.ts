import { describe, expect, it } from "vitest";
import { shouldFocusNameField } from "./new-session-focus";

describe("shouldFocusNameField", () => {
  it("focuses the name field when opened pre-scoped to a project (per-project +)", () => {
    expect(shouldFocusNameField("acme")).toBe(true);
  });

  it("leads with the project picker for the global + (no project implied)", () => {
    expect(shouldFocusNameField("")).toBe(false);
    expect(shouldFocusNameField(undefined)).toBe(false);
  });
});
