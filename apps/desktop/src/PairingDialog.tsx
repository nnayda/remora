import { toDataURL } from "qrcode";
import { type KeyboardEvent, type ReactNode, useEffect, useState } from "react";
import type { PairingDeviceArrived, PairingOutcomeDto } from "./bindings";
import {
  cancelPairing,
  confirmPairing,
  openPairingWindow,
  rejectPairing,
  subscribePairingDeviceArrived,
  subscribePairingResult,
  subscribePairingWindowOpened,
} from "./bridge";
import { writeClipboard } from "./clipboard";
import { formErrorMessage } from "./form-error";
import { Button, Dialog } from "./ui";
import { AlertTriangle, Check, Smartphone } from "./ui/icons";
import "./PairingDialog.css";

/** True when a thrown bridge value is the "this device hosts no relay" error
 * (ADR-0021) — the dialog shows a friendly state rather than a QR. */
function isRelayNotConfigured(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "kind" in error &&
    (error as { kind: unknown }).kind === "relayNotConfigured"
  );
}

/**
 * Pairing ceremony state (ADR-0021 D7).
 * - `opening`: the open-window command is in flight.
 * - `open`: a live window — `device` null while waiting for a scan, set once a
 *   device arrives and the human must compare fingerprints. The window is still
 *   counting down in both cases, so closing here cancels it.
 * - `result`: a terminal outcome (paired / rejected / expired). The window is
 *   already closed backend-side, so closing here just dismisses.
 * - `not-configured` / `error`: the open failed.
 */
type Phase =
  | { kind: "opening" }
  | {
      kind: "open";
      code: string;
      expiresAt: number;
      device: PairingDeviceArrived | null;
      busy: boolean;
    }
  | { kind: "result"; outcome: PairingOutcomeDto }
  | { kind: "not-configured" }
  | { kind: "error"; message: string };

/** Format a remaining-seconds count as `m:ss` for the countdown. */
function formatRemaining(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

/**
 * The pairing ceremony dialog. On open it requests a pairing window from the
 * hosted bridge, renders the resulting `remora-pair:…` string as an offline QR
 * (plus a copyable fallback) and a live countdown, then reacts to backend
 * events: a `PairingDeviceArrived` swaps to a fingerprint-compare + Confirm /
 * Reject prompt, and a `PairingResult` terminalizes to paired / rejected /
 * expired. Closing a still-open window cancels it.
 *
 * The pairing `code` embeds the pairing PSK (ADR-0021 D1) — it is shown for the
 * QR/copy by design but is never logged.
 */
export function PairingDialog({ onClose }: { onClose: () => void }) {
  const [phase, setPhase] = useState<Phase>({ kind: "opening" });
  // Data-URL of the rendered QR for the current code, or null until it renders.
  const [qrUrl, setQrUrl] = useState<string | null>(null);
  // Wall-clock second, ticked while a window is open, to drive the countdown.
  const [nowSec, setNowSec] = useState(() => Math.floor(Date.now() / 1000));
  // Transient "Copied" affirmation on the copy button.
  const [copied, setCopied] = useState(false);
  // A confirm/reject rejection surfaces inline without leaving the arrived view.
  const [actionError, setActionError] = useState<string | null>(null);

  // Open the window and wire the three pairing events. Subscribe *before*
  // opening so an arrival/result that races the open is never missed.
  useEffect(() => {
    let live = true;
    const unlisteners: Array<() => void> = [];

    async function start() {
      try {
        // allSettled (not all): a rejected subscription must not orphan the
        // ones that *did* succeed — each fulfilled listener is still torn
        // down below, then the first rejection is rethrown into the shared
        // catch so a subscribe failure lands in the dialog's error phase
        // instead of leaving it stuck on "Opening a pairing window…".
        const results = await Promise.allSettled([
          subscribePairingWindowOpened(({ code, expiresAt }) => {
            // The authoritative window (re-)opened: refresh code + deadline.
            setPhase((prev) =>
              prev.kind === "opening" || prev.kind === "open"
                ? { kind: "open", code, expiresAt, device: null, busy: false }
                : prev,
            );
          }),
          subscribePairingDeviceArrived((device) => {
            setActionError(null);
            setPhase((prev) =>
              prev.kind === "open" ? { ...prev, device, busy: false } : prev,
            );
          }),
          subscribePairingResult(({ outcome }) => {
            setPhase({ kind: "result", outcome });
          }),
        ]);
        if (!live) {
          for (const r of results) if (r.status === "fulfilled") r.value();
          return;
        }
        for (const r of results) {
          if (r.status === "fulfilled") unlisteners.push(r.value);
        }
        const failed = results.find((r) => r.status === "rejected");
        if (failed) throw (failed as PromiseRejectedResult).reason;

        const dto = await openPairingWindow(null);
        if (!live) return;
        // The window-opened event carries the same values and may already have
        // landed; only seed the open phase if we're still waiting.
        setPhase((prev) =>
          prev.kind === "opening"
            ? {
                kind: "open",
                code: dto.code,
                expiresAt: dto.expiresAt,
                device: null,
                busy: false,
              }
            : prev,
        );
      } catch (err) {
        if (!live) return;
        if (isRelayNotConfigured(err)) setPhase({ kind: "not-configured" });
        else setPhase({ kind: "error", message: formErrorMessage(err) });
      }
    }

    void start();
    return () => {
      live = false;
      for (const un of unlisteners) un();
    };
  }, []);

  // Render the QR from the current code, locally (no network). A failed draw
  // leaves the copyable string as the fallback.
  const code = phase.kind === "open" ? phase.code : null;
  useEffect(() => {
    if (code === null) {
      setQrUrl(null);
      return;
    }
    let live = true;
    toDataURL(code, { margin: 1, width: 220 })
      .then((url) => {
        if (live) setQrUrl(url);
      })
      .catch(() => {
        if (live) setQrUrl(null);
      });
    return () => {
      live = false;
    };
  }, [code]);

  // Tick the countdown once per second while a window is open.
  const open = phase.kind === "open";
  useEffect(() => {
    if (!open) return;
    const id = setInterval(
      () => setNowSec(Math.floor(Date.now() / 1000)),
      1000,
    );
    return () => clearInterval(id);
  }, [open]);

  const remaining =
    phase.kind === "open" ? Math.max(0, phase.expiresAt - nowSec) : 0;

  // A locally-elapsed countdown terminalizes to expired even if the backend's
  // own expiry event is slow (they converge on the same state).
  useEffect(() => {
    if (phase.kind === "open" && remaining <= 0) {
      setPhase({ kind: "result", outcome: { kind: "expired" } });
    }
  }, [phase, remaining]);

  // Clear the "Copied" affirmation shortly after it shows.
  useEffect(() => {
    if (!copied) return;
    const id = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(id);
  }, [copied]);

  /** Close: cancel a still-open window (mid-ceremony), then dismiss. */
  function handleClose() {
    if (phase.kind === "open") void cancelPairing();
    onClose();
  }

  function copyCode(value: string) {
    void writeClipboard(value);
    setCopied(true);
  }

  async function decide(action: "confirm" | "reject", deviceId: string) {
    setActionError(null);
    setPhase((prev) => (prev.kind === "open" ? { ...prev, busy: true } : prev));
    try {
      if (action === "confirm") await confirmPairing(deviceId);
      else await rejectPairing(deviceId);
      // The terminal state arrives via the PairingResult event.
    } catch (err) {
      setActionError(formErrorMessage(err));
      setPhase((prev) =>
        prev.kind === "open" ? { ...prev, busy: false } : prev,
      );
    }
  }

  const { title, description, icon, body, footer } = renderPhase();

  return (
    <Dialog
      open
      title={title}
      description={description}
      icon={icon}
      onClose={handleClose}
      onKeyDown={trapKeys(handleClose)}
      footer={footer}
    >
      {body}
    </Dialog>
  );

  function renderPhase(): {
    title: string;
    description?: string;
    icon: ReactNode;
    body: ReactNode;
    footer: ReactNode;
  } {
    const smartphone = <Smartphone size={18} />;

    if (phase.kind === "opening") {
      return {
        title: "Pair a new device",
        icon: smartphone,
        body: <p className="pairing-hint">Opening a pairing window…</p>,
        footer: (
          <Button variant="ghost" onClick={handleClose}>
            Close
          </Button>
        ),
      };
    }

    if (phase.kind === "not-configured") {
      return {
        title: "Pairing unavailable",
        icon: smartphone,
        body: (
          <p className="pairing-hint">
            Relay not configured. Add a <code>[relay]</code> section to pair
            devices with this desktop.
          </p>
        ),
        footer: (
          <Button variant="ghost" onClick={handleClose}>
            Close
          </Button>
        ),
      };
    }

    if (phase.kind === "error") {
      return {
        title: "Pairing failed",
        icon: <AlertTriangle size={18} />,
        body: (
          <p className="settings-error" role="alert">
            {phase.message}
          </p>
        ),
        footer: (
          <Button variant="ghost" onClick={handleClose}>
            Close
          </Button>
        ),
      };
    }

    if (phase.kind === "result") {
      return renderResult(phase.outcome);
    }

    // phase.kind === "open"
    if (phase.device) {
      return renderArrived(phase.device, phase.busy);
    }
    return renderWaiting(phase.code);
  }

  function renderWaiting(pairingCode: string) {
    return {
      title: "Pair a new device",
      description:
        "Scan this code from the Remora app on your phone, or copy it.",
      icon: <Smartphone size={18} />,
      body: (
        <div className="pairing-scan">
          <div className="pairing-qr" aria-hidden={qrUrl === null}>
            {qrUrl !== null && (
              <img src={qrUrl} alt="Pairing QR code" width={220} height={220} />
            )}
          </div>
          <code className="pairing-code">{pairingCode}</code>
          <div className="pairing-countdown" aria-live="polite">
            Expires in{" "}
            <span className="pairing-countdown__value">
              {formatRemaining(remaining)}
            </span>
          </div>
        </div>
      ),
      footer: (
        <>
          <Button variant="ghost" onClick={handleClose}>
            Cancel
          </Button>
          <Button
            variant="secondary"
            onClick={() => copyCode(pairingCode)}
            icon={copied ? <Check size={14} /> : undefined}
          >
            {copied ? "Copied" : "Copy pairing code"}
          </Button>
        </>
      ),
    };
  }

  function renderArrived(device: PairingDeviceArrived, busy: boolean) {
    return {
      title: "Confirm this device",
      description:
        "Compare the fingerprint below against the one on your device.",
      icon: <Smartphone size={18} />,
      body: (
        <div className="pairing-confirm">
          <p className="pairing-device-name">{device.name}</p>
          <p className="pairing-fingerprint">{device.fingerprint}</p>
          <div className="pairing-countdown" aria-live="polite">
            Expires in{" "}
            <span className="pairing-countdown__value">
              {formatRemaining(remaining)}
            </span>
          </div>
          {actionError && (
            <p className="settings-error" role="alert">
              {actionError}
            </p>
          )}
        </div>
      ),
      footer: (
        <>
          <Button
            variant="ghost"
            disabled={busy}
            onClick={() => void decide("reject", device.deviceId)}
          >
            Reject
          </Button>
          <Button
            variant="primary"
            loading={busy}
            onClick={() => void decide("confirm", device.deviceId)}
          >
            Confirm
          </Button>
        </>
      ),
    };
  }

  function renderResult(outcome: PairingOutcomeDto) {
    const done = (
      <Button variant="primary" onClick={onClose}>
        Done
      </Button>
    );

    if (outcome.kind === "paired") {
      return {
        title: "Device paired",
        icon: <Check size={18} />,
        body: (
          <p className="pairing-result pairing-result--ok">
            Paired <strong>{outcome.name}</strong> ✓
          </p>
        ),
        footer: done,
      };
    }
    if (outcome.kind === "rejected") {
      return {
        title: "Device rejected",
        icon: <AlertTriangle size={18} />,
        body: (
          <p className="pairing-result">
            Rejected — nothing was granted to that device.
          </p>
        ),
        footer: done,
      };
    }
    return {
      title: "Pairing window expired",
      icon: <AlertTriangle size={18} />,
      body: (
        <p className="pairing-result">
          Expired — the window closed before a device paired. Open a new one to
          try again.
        </p>
      ),
      footer: done,
    };
  }
}

/** Esc + a small Tab focus trap, so the nested dialog doesn't leak keys to the
 * enclosing Settings dialog's trap (mirrors ConfirmRevokeDialog). */
function trapKeys(onClose: () => void) {
  return (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      onClose();
      return;
    }
    if (e.key !== "Tab") return;
    e.stopPropagation();
    const focusable = e.currentTarget.querySelectorAll<HTMLElement>(
      'button:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    );
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  };
}
