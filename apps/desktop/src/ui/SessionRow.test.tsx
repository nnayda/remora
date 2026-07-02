// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { SessionRow } from "./SessionRow";

afterEach(cleanup);

/** The status slot renders exactly one cue, in precedence order:
 * removing > connecting > status dot / reserved footprint. `removing` must
 * win — a background teardown is the row's dominant fact regardless of any
 * concurrent open (the store's busy-guard makes co-occurrence impossible,
 * but the render contract should not depend on that). */
describe("SessionRow — status slot precedence", () => {
  it("shows the Removing… spinner when removing, even if connecting is also set", () => {
    render(<SessionRow name="fix" removing connecting />);
    expect(screen.queryByRole("status", { name: "Removing…" })).not.toBeNull();
    expect(screen.queryByRole("status", { name: "Connecting…" })).toBeNull();
  });

  it("shows the Connecting… spinner when connecting and not removing", () => {
    render(<SessionRow name="fix" connecting />);
    expect(
      screen.queryByRole("status", { name: "Connecting…" }),
    ).not.toBeNull();
    expect(screen.queryByRole("status", { name: "Removing…" })).toBeNull();
  });

  it("shows no spinner on a plain connected row", () => {
    render(<SessionRow name="fix" />);
    expect(screen.queryByRole("status", { name: "Removing…" })).toBeNull();
    expect(screen.queryByRole("status", { name: "Connecting…" })).toBeNull();
  });
});
