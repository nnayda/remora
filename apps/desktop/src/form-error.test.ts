import { describe, expect, it } from "vitest";
import { formErrorMessage } from "./form-error";

describe("formErrorMessage", () => {
  it("returns the message of a BridgeError-shaped object", () => {
    expect(formErrorMessage({ kind: "configEdit", message: "id exists" })).toBe(
      "id exists",
    );
  });

  it("falls back when message is non-string or empty", () => {
    expect(formErrorMessage({ message: 42 })).toBe("Something went wrong.");
    expect(formErrorMessage({ message: "" })).toBe("Something went wrong.");
    expect(formErrorMessage({ message: "   " })).toBe("Something went wrong.");
    expect(formErrorMessage({ message: null })).toBe("Something went wrong.");
  });

  it("falls back for null, bare strings, and message-less objects", () => {
    expect(formErrorMessage(null)).toBe("Something went wrong.");
    expect(formErrorMessage("boom")).toBe("Something went wrong.");
    expect(formErrorMessage({ kind: "channelClosed" })).toBe(
      "Something went wrong.",
    );
  });
});
