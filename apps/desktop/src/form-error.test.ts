import { describe, expect, it } from "vitest";
import { formErrorMessage } from "./form-error";

describe("formErrorMessage", () => {
  it("returns the message of a BridgeError-shaped object", () => {
    expect(formErrorMessage({ kind: "configEdit", message: "id exists" })).toBe(
      "id exists",
    );
  });

  it("coerces a non-string message", () => {
    expect(formErrorMessage({ message: 42 })).toBe("42");
  });

  it("falls back for null, bare strings, and message-less objects", () => {
    expect(formErrorMessage(null)).toBe("Something went wrong.");
    expect(formErrorMessage("boom")).toBe("Something went wrong.");
    expect(formErrorMessage({ kind: "channelClosed" })).toBe(
      "Something went wrong.",
    );
  });
});
