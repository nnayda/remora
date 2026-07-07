// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PairingDeviceArrived, PairingOutcomeDto } from "./bindings";

// ---------------------------------------------------------------------------
// Mocks — the dialog talks to the backend only through bridge.ts, reaches the
// clipboard only through clipboard.ts, and renders the QR only through the
// `qrcode` package. Mock all three seams (like the other component tests).
// A real QR draw fights jsdom's missing canvas, so the honest assertion is
// that `qrcode` was invoked with the pairing code (plus the copyable string).
// ---------------------------------------------------------------------------

const b = vi.hoisted(() => ({
  openPairingWindow: vi.fn(),
  confirmPairing: vi.fn(),
  rejectPairing: vi.fn(),
  cancelPairing: vi.fn(),
  subscribePairingWindowOpened: vi.fn(),
  subscribePairingDeviceArrived: vi.fn(),
  subscribePairingResult: vi.fn(),
}));
vi.mock("./bridge", () => b);

const clip = vi.hoisted(() => ({ writeClipboard: vi.fn() }));
vi.mock("./clipboard", () => clip);

const qr = vi.hoisted(() => ({ toDataURL: vi.fn() }));
vi.mock("qrcode", () => ({
  toDataURL: qr.toDataURL,
  default: { toDataURL: qr.toDataURL },
}));

vi.mock("./ui/icons", () => ({
  Smartphone: () => null,
  Check: () => null,
  X: () => null,
  AlertTriangle: () => null,
}));

import { PairingDialog } from "./PairingDialog";

// ---------------------------------------------------------------------------
// Fixtures + captured event callbacks
// ---------------------------------------------------------------------------

const CODE = "remora-pair:1:AbCdEfGh";

const ARRIVED: PairingDeviceArrived = {
  deviceId: "c".repeat(64),
  name: "Pixel 7",
  fingerprint: "AAAA-BBBB-CCCC",
};

let onWindowOpened: (p: { code: string; expiresAt: number }) => void;
let onDeviceArrived: (p: PairingDeviceArrived) => void;
let onResult: (p: { outcome: PairingOutcomeDto }) => void;

function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

beforeEach(() => {
  for (const fn of Object.values(b)) fn.mockReset();
  clip.writeClipboard.mockReset();
  qr.toDataURL.mockReset();

  b.subscribePairingWindowOpened.mockImplementation(
    (cb: typeof onWindowOpened) => {
      onWindowOpened = cb;
      return Promise.resolve(() => {});
    },
  );
  b.subscribePairingDeviceArrived.mockImplementation(
    (cb: typeof onDeviceArrived) => {
      onDeviceArrived = cb;
      return Promise.resolve(() => {});
    },
  );
  b.subscribePairingResult.mockImplementation((cb: typeof onResult) => {
    onResult = cb;
    return Promise.resolve(() => {});
  });
  b.openPairingWindow.mockResolvedValue({
    code: CODE,
    expiresAt: nowSecs() + 120,
    ttlSecs: 120,
  });
  b.confirmPairing.mockResolvedValue(undefined);
  b.rejectPairing.mockResolvedValue(undefined);
  b.cancelPairing.mockResolvedValue(undefined);
  qr.toDataURL.mockResolvedValue("data:image/png;base64,ZZ");
  clip.writeClipboard.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

/** Render the dialog and flush the async subscribe + open-window sequence. */
async function renderOpen(onClose: () => void = () => {}) {
  render(<PairingDialog onClose={onClose} />);
  // Flush the mount effect's awaited subscribe + openPairingWindow chain.
  await screen.findByText(CODE);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("PairingDialog — window open", () => {
  it("opens a window, renders the code + a QR from it, and copies the code", async () => {
    await renderOpen();

    // The pairing window was opened.
    expect(b.openPairingWindow).toHaveBeenCalledOnce();
    // The copyable pairing string is shown verbatim.
    expect(screen.getByText(CODE)).not.toBeNull();
    // The QR is generated locally from that exact string (no network fetch).
    // The draw lives in a passive effect scheduled *after* the commit that
    // findByText observed, so poll instead of asserting synchronously — under
    // CI load the assertion could fire before the effect ran (#302).
    await waitFor(() =>
      expect(qr.toDataURL).toHaveBeenCalledWith(CODE, expect.anything()),
    );

    // Copy button hands the exact code to the clipboard seam.
    fireEvent.click(screen.getByRole("button", { name: /copy pairing code/i }));
    await waitFor(() => expect(clip.writeClipboard).toHaveBeenCalledWith(CODE));
  });
});

describe("PairingDialog — device arrived", () => {
  it("shows the arrived device's name and fingerprint with Confirm/Reject", async () => {
    await renderOpen();

    act(() => onDeviceArrived(ARRIVED));

    expect(screen.getByText("Pixel 7")).not.toBeNull();
    expect(screen.getByText("AAAA-BBBB-CCCC")).not.toBeNull();
    expect(screen.getByRole("button", { name: /^confirm$/i })).not.toBeNull();
    expect(screen.getByRole("button", { name: /^reject$/i })).not.toBeNull();
  });

  it("Confirm calls confirmPairing and shows paired ✓ on the result", async () => {
    await renderOpen();
    act(() => onDeviceArrived(ARRIVED));

    fireEvent.click(screen.getByRole("button", { name: /^confirm$/i }));
    expect(b.confirmPairing).toHaveBeenCalledWith(ARRIVED.deviceId);
    expect(b.rejectPairing).not.toHaveBeenCalled();

    // The backend confirms via the result event.
    act(() =>
      onResult({
        outcome: {
          kind: "paired",
          deviceId: ARRIVED.deviceId,
          name: "Pixel 7",
        },
      }),
    );
    expect(screen.getAllByText(/paired/i).length).toBeGreaterThan(0);
    expect(screen.getByRole("button", { name: /^done$/i })).not.toBeNull();
  });

  it("Reject calls rejectPairing and shows the rejected state", async () => {
    await renderOpen();
    act(() => onDeviceArrived(ARRIVED));

    fireEvent.click(screen.getByRole("button", { name: /^reject$/i }));
    expect(b.rejectPairing).toHaveBeenCalledWith(ARRIVED.deviceId);
    expect(b.confirmPairing).not.toHaveBeenCalled();

    act(() =>
      onResult({ outcome: { kind: "rejected", deviceId: ARRIVED.deviceId } }),
    );
    expect(screen.getAllByText(/rejected/i).length).toBeGreaterThan(0);
  });
});

describe("PairingDialog — terminal states", () => {
  it("counts down and shows the expired state when the window lapses", async () => {
    vi.useFakeTimers();
    b.openPairingWindow.mockResolvedValue({
      code: CODE,
      expiresAt: nowSecs() + 2,
      ttlSecs: 2,
    });

    render(<PairingDialog onClose={() => {}} />);
    // Flush the async subscribe + open chain under fake timers.
    await act(async () => {});
    expect(screen.getByText(CODE)).not.toBeNull();

    // Advance past expiry — the countdown drives the terminal expired state.
    await act(async () => {
      vi.advanceTimersByTime(3000);
    });
    expect(screen.getAllByText(/expired/i).length).toBeGreaterThan(0);
  });

  it("renders whatever the backend delivers — an expired result terminalizes", async () => {
    await renderOpen();
    act(() => onResult({ outcome: { kind: "expired" } }));
    expect(screen.getAllByText(/expired/i).length).toBeGreaterThan(0);
  });
});

describe("PairingDialog — relay not configured", () => {
  it("shows a friendly state and no QR when this device hosts no relay", async () => {
    b.openPairingWindow.mockRejectedValue({
      kind: "relayNotConfigured",
      message: "no relay",
    });
    render(<PairingDialog onClose={() => {}} />);

    expect(
      await screen.findByText(/no relay bridge is running/i),
    ).not.toBeNull();
    expect(screen.queryByText(CODE)).toBeNull();
    expect(qr.toDataURL).not.toHaveBeenCalled();
  });
});

describe("PairingDialog — subscribe failure", () => {
  it("shows the error state (not stuck on 'Opening…') when a subscription rejects", async () => {
    b.subscribePairingDeviceArrived.mockRejectedValue(
      new Error("listen failed"),
    );
    render(<PairingDialog onClose={() => {}} />);

    expect(await screen.findByText(/listen failed/i)).not.toBeNull();
    // Never got as far as opening the window or rendering a code.
    expect(b.openPairingWindow).not.toHaveBeenCalled();
    expect(screen.queryByText(/opening a pairing window/i)).toBeNull();
  });
});

describe("PairingDialog — close mid-window", () => {
  it("cancels the pairing window and calls onClose when closed while open", async () => {
    const onClose = vi.fn();
    await renderOpen(onClose);

    // The header × (aria-label "Close") dismisses a live window.
    fireEvent.click(screen.getByRole("button", { name: "Close" }));

    expect(b.cancelPairing).toHaveBeenCalledOnce();
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("does not cancel when closing after a terminal result", async () => {
    const onClose = vi.fn();
    await renderOpen(onClose);
    act(() => onResult({ outcome: { kind: "expired" } }));

    fireEvent.click(screen.getByRole("button", { name: /^done$/i }));

    expect(b.cancelPairing).not.toHaveBeenCalled();
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("cancels the just-opened window when it resolves after unmount", async () => {
    // Hold the open unresolved so the dialog stays in the "opening" phase —
    // the window where Close never sees an "open" phase to cancel.
    let resolveOpen!: (dto: {
      code: string;
      expiresAt: number;
      ttlSecs: number;
    }) => void;
    b.openPairingWindow.mockReturnValue(
      new Promise((resolve) => {
        resolveOpen = resolve;
      }),
    );

    const { unmount } = render(<PairingDialog onClose={() => {}} />);
    // Advance the mount effect to the awaited openPairingWindow call.
    await act(async () => {});
    expect(b.openPairingWindow).toHaveBeenCalledOnce();
    expect(b.cancelPairing).not.toHaveBeenCalled();

    // Close during "opening" unmounts the dialog (cleanup sets live = false)
    // before the window finishes opening.
    unmount();

    // The bridge now reports the window as open — after the dialog is gone.
    await act(async () => {
      resolveOpen({ code: CODE, expiresAt: nowSecs() + 120, ttlSecs: 120 });
    });

    // The post-await guard cancels it rather than leaking it until its TTL.
    expect(b.cancelPairing).toHaveBeenCalledOnce();
  });
});
