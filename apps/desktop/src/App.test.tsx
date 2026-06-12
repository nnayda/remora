import { describe, expect, it } from "vitest";
import { APP_NAME } from "./App";

describe("App", () => {
  it("exposes the app name", () => {
    expect(APP_NAME).toBe("Remora");
  });
});
