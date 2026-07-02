// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { StrictMode } from "react";
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
  // Pairing-dialog seam (only exercised by the "Pair new device" test).
  openPairingWindow: vi.fn(),
  confirmPairing: vi.fn(),
  rejectPairing: vi.fn(),
  cancelPairing: vi.fn(),
  subscribePairingWindowOpened: vi.fn(() => Promise.resolve(() => {})),
  subscribePairingDeviceArrived: vi.fn(() => Promise.resolve(() => {})),
  subscribePairingResult: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("./bridge", () => b);
vi.mock("./clipboard", () => ({ writeClipboard: vi.fn() }));
vi.mock("qrcode", () => ({
  toDataURL: vi.fn(() => Promise.resolve("data:image/png;base64,ZZ")),
  default: {
    toDataURL: vi.fn(() => Promise.resolve("data:image/png;base64,ZZ")),
  },
}));

vi.mock("./ui/icons", () => ({
  Smartphone: () => null,
  Trash: () => null,
  AlertTriangle: () => null,
  Plus: () => null,
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
  b.openPairingWindow.mockResolvedValue({
    code: "remora-pair:1:XyZ",
    expiresAt: Math.floor(Date.now() / 1000) + 120,
    ttlSecs: 120,
  });
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

  it("a failed revoke surfaces its message in the error banner", async () => {
    b.listDevices.mockResolvedValue([PIXEL]);
    b.revokeDevice.mockRejectedValue({
      kind: "relay",
      message: "roster storage failed",
    });
    // StrictMode matches the app root (main.tsx) and pins the unmount latch:
    // the dev double-mount (setup → cleanup → setup) must leave it re-armed,
    // or this banner is silently swallowed in every dev session.
    render(
      <StrictMode>
        <DevicesPanel />
      </StrictMode>,
    );

    await screen.findByText("Pixel 7");
    fireEvent.click(
      screen.getByRole("button", { name: /Revoke device Pixel 7/i }),
    );
    fireEvent.click(screen.getByRole("button", { name: /^Revoke$/ }));

    // The bridge error's message lands in the alert banner, and the roster
    // stays visible (a failed revoke must not blank the panel).
    expect(await screen.findByText("roster storage failed")).not.toBeNull();
    expect(screen.getByRole("alert").textContent).toContain(
      "roster storage failed",
    );
    expect(screen.queryByText("Pixel 7")).not.toBeNull();
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

describe("DevicesPanel — pair new device", () => {
  it("opens the pairing dialog from the header button", async () => {
    b.listDevices.mockResolvedValue([PIXEL]);
    render(<DevicesPanel />);

    await screen.findByText("Pixel 7");
    // The header exposes a "Pair new device" affordance.
    fireEvent.click(screen.getByRole("button", { name: /pair new device/i }));

    // The ceremony dialog opens and requests a pairing window.
    expect(await screen.findByText(/remora-pair:1:XyZ/)).not.toBeNull();
    expect(b.openPairingWindow).toHaveBeenCalledOnce();
  });

  it("hides the pair button when the relay is not configured", async () => {
    b.getBridgeFingerprint.mockRejectedValue({
      kind: "relayNotConfigured",
      message: "no relay",
    });
    b.listDevices.mockRejectedValue({
      kind: "relayNotConfigured",
      message: "no relay",
    });
    render(<DevicesPanel />);

    await screen.findByText(/No relay bridge is running/i);
    expect(
      screen.queryByRole("button", { name: /pair new device/i }),
    ).toBeNull();
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

    expect(
      await screen.findByText(/No relay bridge is running/i),
    ).not.toBeNull();
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
