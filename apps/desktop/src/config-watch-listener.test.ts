import { describe, expect, it, vi } from "vitest";

const { listen } = vi.hoisted(() => ({
  listen: vi.fn(async (_handler: () => void) => () => {}),
}));
vi.mock("./bindings", () => ({
  events: { configChanged: { listen } },
}));

import { subscribeConfigChanged } from "./config-watch-listener";

describe("subscribeConfigChanged", () => {
  it("invokes onChange whenever the event fires", async () => {
    const onChange = vi.fn();
    await subscribeConfigChanged(onChange);

    // The handler registered with the event bus should call onChange.
    expect(listen).toHaveBeenCalledTimes(1);
    const handler = listen.mock.calls[0][0];
    handler();
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("returns the unlisten function from the event bus", async () => {
    const unlisten = vi.fn();
    listen.mockResolvedValueOnce(unlisten);
    const off = await subscribeConfigChanged(() => {});
    expect(off).toBe(unlisten);
  });
});
