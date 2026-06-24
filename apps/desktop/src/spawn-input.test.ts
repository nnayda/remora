import { describe, expect, it } from "vitest";
import { isValidSlug, normalizeSlugInput } from "./spawn-input";

describe("isValidSlug", () => {
  it("accepts lower-case slugs of [a-z0-9-]", () => {
    expect(isValidSlug("api")).toBe(true);
    expect(isValidSlug("fix-bug-2")).toBe(true);
    expect(isValidSlug("a")).toBe(true);
    expect(isValidSlug("a".repeat(64))).toBe(true);
  });

  it("rejects empty, over-length, and out-of-grammar input", () => {
    expect(isValidSlug("")).toBe(false);
    expect(isValidSlug("a".repeat(65))).toBe(false);
    expect(isValidSlug("API")).toBe(false);
    expect(isValidSlug("has space")).toBe(false);
    expect(isValidSlug("under_score")).toBe(false);
    expect(isValidSlug("slash/here")).toBe(false);
  });
});

describe("normalizeSlugInput", () => {
  it("lowercases uppercase letters so autocapitalized input is accepted", () => {
    expect(normalizeSlugInput("MyApp")).toBe("myapp");
    expect(normalizeSlugInput("API")).toBe("api");
    expect(normalizeSlugInput("Fix-Bug-2")).toBe("fix-bug-2");
  });

  it("leaves already-canonical input unchanged", () => {
    expect(normalizeSlugInput("api")).toBe("api");
    expect(normalizeSlugInput("")).toBe("");
  });

  it("only canonicalizes case, leaving other out-of-grammar chars for validation", () => {
    expect(normalizeSlugInput("Has Space")).toBe("has space");
    expect(normalizeSlugInput("Under_Score")).toBe("under_score");
  });
});
