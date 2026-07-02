// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeviceInfoDto } from "./bindings";

// ---------------------------------------------------------------------------
// Mocks — the panel talks to the backend only through the bridge.ts wrappers,
// so we mock at that seam (like the other component tests mock their deps).
// ---------------------------------------------------------------------------

const b = vi.hoisted(() => ({
  listDevices: vi.fn(),
  revokeDevice: vi.fn(),
  getBridgeFingerprint: vi.fn(),
  subscribeRosterChanged: vi.fn(),
}));

vi.mock("./bridge", () => b);

vi.mock("./ui/icons", () => ({
  Smartphone: () => null,
  Trash: () => null,
  AlertTriangle: () => null,
  X: () => null,
}));

import { DevicesPanel } from "./DevicesPanel";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const PIXEL: DeviceInfoDto = {
  deviceId: "a".repeat(64),
  name: "Pixel 7",
  fingerprint: "AAAA-BBBB-CCCC",
  enrolledAt: 1_700_000_000,
  lastConnectedAt: 1_700_100_000,
};

const IPAD: DeviceInfoDto = {
  deviceId: "b".repeat(64),
  name: "iPad",
  fingerprint: "DDDD-EEEE-FFFF",
  enrolledAt: 1_700_000_000,
  lastConnectedAt: null,
};

beforeEach(() => {
  b.listDevices.mockReset();
  b.revokeDevice.mockReset();
  b.getBridgeFingerprint.mockReset();
  b.subscribeRosterChanged.mockReset();
  // Default: a valid, subscribable roster.
  b.getBridgeFingerprint.mockResolvedValue("1111-2222-3333");
  b.subscribeRosterChanged.mockResolvedValue(() => {});
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("DevicesPanel — roster rendering", () => {
  it("shows this desktop's fingerprint and a row per device with name + fingerprint + last-connected", async () => {
    b.listDevices.mockResolvedValue([PIXEL, IPAD]);
    render(<DevicesPanel />);

    // Bridge fingerprint header.
    expect(await screen.findByText(/1111-2222-3333/)).not.toBeNull();

    // Each device's name and fingerprint render.
    expect(screen.queryByText("Pixel 7")).not.toBeNull();
    expect(screen.queryByText(/AAAA-BBBB-CCCC/)).not.toBeNull();
    expect(screen.queryByText("iPad")).not.toBeNull();
    expect(screen.queryByText(/DDDD-EEEE-FFFF/)).not.toBeNull();

    // Last-connected surfaces: a real timestamp for Pixel, "Never" for iPad.
    expect(screen.queryAllByText(/Last connected/i).length).toBe(2);
    expect(screen.queryByText(/Never/i)).not.toBeNull();
  });
});

describe("DevicesPanel — per-row revoke", () => {
  it("clicking Revoke then confirming calls revokeDevice with that device id", async () => {
    b.listDevices.mockResolvedValue([PIXEL]);
    b.revokeDevice.mockResolvedValue(undefined);
    render(<DevicesPanel />);

    await screen.findByText("Pixel 7");

    // Row action opens the confirm dialog; nothing is revoked yet.
    fireEvent.click(
      screen.getByRole("button", { name: /Revoke device Pixel 7/i }),
    );
    expect(b.revokeDevice).not.toHaveBeenCalled();

    // The confirm dialog's primary action fires the revoke.
    fireEvent.click(screen.getByRole("button", { name: /^Revoke$/ }));

    await vi.waitFor(() => expect(b.revokeDevice).toHaveBeenCalledOnce());
    expect(b.revokeDevice).toHaveBeenCalledWith(PIXEL.deviceId);
  });

  it("cancelling the confirm dialog does not revoke", async () => {
    b.listDevices.mockResolvedValue([PIXEL]);
    render(<DevicesPanel />);

    await screen.findByText("Pixel 7");
    fireEvent.click(
      screen.getByRole("button", { name: /Revoke device Pixel 7/i }),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Cancel$/ }));

    expect(b.revokeDevice).not.toHaveBeenCalled();
  });
});

describe("DevicesPanel — relay not configured", () => {
  it("shows a friendly empty state and never renders the roster", async () => {
    b.getBridgeFingerprint.mockRejectedValue({
      kind: "relayNotConfigured",
      message: "no relay",
    });
    b.listDevices.mockRejectedValue({
      kind: "relayNotConfigured",
      message: "no relay",
    });
    render(<DevicesPanel />);

    expect(await screen.findByText(/Relay not configured/i)).not.toBeNull();
    expect(screen.queryByText(/1111-2222-3333/)).toBeNull();
  });
});

describe("DevicesPanel — empty roster", () => {
  it("renders the fingerprint header and a no-devices hint", async () => {
    b.listDevices.mockResolvedValue([]);
    render(<DevicesPanel />);

    expect(await screen.findByText(/1111-2222-3333/)).not.toBeNull();
    expect(screen.queryByText(/No devices paired/i)).not.toBeNull();
  });
});
