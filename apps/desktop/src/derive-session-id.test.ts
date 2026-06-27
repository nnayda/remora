import { describe, expect, it } from "vitest";
import vectors from "../../../crates/remora-protocol/tests/fixtures/derive-session-id-vectors.json";
import { deriveSessionId } from "./derive-session-id";

describe("deriveSessionId (must match Rust naming::derive_session_id byte-for-byte)", () => {
  for (const v of vectors) {
    it(`${v.branch} -> ${v.session_id}`, () => {
      expect(deriveSessionId(v.branch)).toBe(v.session_id);
    });
  }
  it("remora/<slug> round-trips with no hash", () => {
    expect(deriveSessionId("remora/fix-login")).toBe("fix-login");
  });
  it("distinct branches that slugify the same stay distinct", () => {
    expect(deriveSessionId("feat/login")).not.toBe(
      deriveSessionId("feat-login"),
    );
  });
  it("an over-long branch returns null (mirrors Rust None)", () => {
    expect(deriveSessionId(`feature/${"a".repeat(60)}`)).toBeNull();
  });
});
