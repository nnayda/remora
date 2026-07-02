import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { DeviceInfoDto } from "./bindings";
import {
  getBridgeFingerprint,
  listDevices,
  revokeDevice,
  subscribeRosterChanged,
} from "./bridge";
import { formErrorMessage } from "./form-error";
import { PairingDialog } from "./PairingDialog";
import { Button, Dialog, IconButton } from "./ui";
import { AlertTriangle, Plus, Smartphone, Trash } from "./ui/icons";

/** True when a thrown bridge value is the "this device hosts no relay" error
 * (ADR-0021) — the panel shows a friendly empty state rather than a roster. */
function isRelayNotConfigured(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "kind" in error &&
    (error as { kind: unknown }).kind === "relayNotConfigured"
  );
}

/** Load phase: the initial fetch resolves into a roster, the relay-absent
 * empty state, or a read error. */
type Phase =
  | { kind: "loading" }
  | { kind: "ready"; fingerprint: string; devices: DeviceInfoDto[] }
  | { kind: "not-configured" }
  | { kind: "error"; message: string };

/** Format a unix-second timestamp as a human date, or a dash for null. Dates
 * are ambient context, so they read as muted prose rather than a mono value. */
function formatDate(secs: number | null): string {
  if (secs === null) return "Never";
  return new Date(secs * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

/**
 * Settings → Devices roster (ADR-0021 D7). Lists the paired devices this
 * desktop's bridge trusts, shows this bridge's own comparable fingerprint, and
 * offers a per-row Revoke behind a confirm dialog. Self-contained: fetches on
 * mount and re-reads on every backend `RosterChanged` ping (enroll/revoke).
 *
 * Renders inline as a Settings section, so it owns its own async state rather
 * than lifting it into the dialog shell.
 */
export function DevicesPanel() {
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  // The device pending revoke-confirmation, or null when the dialog is closed.
  const [revokeTarget, setRevokeTarget] = useState<DeviceInfoDto | null>(null);
  // A failed revoke surfaces above the list (the load itself has its own state).
  const [revokeError, setRevokeError] = useState<string | null>(null);
  // Whether the pairing-ceremony dialog is open.
  const [pairing, setPairing] = useState(false);
  // Unmount latch for `confirmRevoke`, which outlives the mount effect's local
  // `live` flag: don't set state after the panel is gone. Re-armed in the
  // effect body — not just the ref initializer — so StrictMode's dev
  // double-mount (setup → cleanup → setup) leaves the surviving mount latched
  // true instead of permanently false.
  const liveRef = useRef(true);
  useEffect(() => {
    liveRef.current = true;
    return () => {
      liveRef.current = false;
    };
  }, []);

  // Fetch fingerprint + roster together; a relayNotConfigured rejection from
  // either is the "no bridge" empty state, anything else is a read error.
  useEffect(() => {
    let live = true;
    async function load() {
      try {
        const [fingerprint, devices] = await Promise.all([
          getBridgeFingerprint(),
          listDevices(),
        ]);
        if (live) setPhase({ kind: "ready", fingerprint, devices });
      } catch (err) {
        if (!live) return;
        if (isRelayNotConfigured(err)) setPhase({ kind: "not-configured" });
        else setPhase({ kind: "error", message: formErrorMessage(err) });
      }
    }
    void load();

    // Re-read the roster on every enroll/revoke ping. Keep the existing
    // fingerprint — only the device list changes — and stay silent on a
    // transient list failure (the next ping, or a reopen, recovers).
    let unlisten: (() => void) | null = null;
    subscribeRosterChanged(() => {
      listDevices()
        .then((devices) => {
          if (!live) return;
          setPhase((prev) =>
            prev.kind === "ready" ? { ...prev, devices } : prev,
          );
        })
        .catch(() => {});
    })
      .then((fn) => {
        if (live) unlisten = fn;
        else fn();
      })
      .catch(() => {});

    return () => {
      live = false;
      unlisten?.();
    };
  }, []);

  /** Fire the revoke, close the confirm, and refresh the roster. The backend
   * also emits `RosterChanged`, but re-reading here means the list converges
   * even if that ping is missed. */
  async function confirmRevoke(device: DeviceInfoDto) {
    setRevokeError(null);
    setRevokeTarget(null);
    try {
      await revokeDevice(device.deviceId);
      const devices = await listDevices();
      if (!liveRef.current) return;
      setPhase((prev) => (prev.kind === "ready" ? { ...prev, devices } : prev));
    } catch (err) {
      if (liveRef.current) setRevokeError(formErrorMessage(err));
    }
  }

  return (
    <section className="settings-section">
      <div className="settings-section__head">
        <span className="settings-section__title">
          <Smartphone size={14} />
          Devices
        </span>
        {phase.kind === "ready" && (
          <Button
            size="sm"
            variant="secondary"
            icon={<Plus size={14} />}
            onClick={() => setPairing(true)}
          >
            Pair new device
          </Button>
        )}
      </div>

      {phase.kind === "loading" ? (
        <p className="settings-section__hint">Loading…</p>
      ) : phase.kind === "not-configured" ? (
        <p className="settings-section__hint">
          No relay bridge is running. Configure a <code>[relay]</code> section
          in config.toml to pair devices with this desktop.
        </p>
      ) : phase.kind === "error" ? (
        <p className="settings-error" role="alert">
          {phase.message}
        </p>
      ) : (
        <>
          <p className="devices-fingerprint">
            This desktop:{" "}
            <span className="devices-fingerprint__value">
              {phase.fingerprint}
            </span>
          </p>
          {revokeError && (
            <p className="settings-error" role="alert">
              {revokeError}
            </p>
          )}
          {phase.devices.length === 0 ? (
            <p className="settings-empty">No devices paired yet.</p>
          ) : (
            <ul className="settings-list">
              {phase.devices.map((device) => (
                <DeviceRow
                  key={device.deviceId}
                  device={device}
                  onRevoke={() => {
                    setRevokeError(null);
                    setRevokeTarget(device);
                  }}
                />
              ))}
            </ul>
          )}
        </>
      )}

      {revokeTarget && (
        <ConfirmRevokeDialog
          device={revokeTarget}
          onConfirm={() => void confirmRevoke(revokeTarget)}
          onClose={() => setRevokeTarget(null)}
        />
      )}

      {pairing && <PairingDialog onClose={() => setPairing(false)} />}
    </section>
  );
}

function DeviceRow({
  device,
  onRevoke,
}: {
  device: DeviceInfoDto;
  onRevoke: () => void;
}) {
  return (
    <li className="settings-row">
      <div className="devices-row__main">
        <span className="settings-row__name">{device.name}</span>
        <span className="settings-row__id">{device.fingerprint}</span>
        <span className="devices-row__meta">
          Enrolled {formatDate(device.enrolledAt)} · Last connected{" "}
          {formatDate(device.lastConnectedAt)}
        </span>
      </div>
      <div className="settings-row__actions">
        <IconButton
          size="sm"
          label={`Revoke device ${device.name}`}
          onClick={onRevoke}
        >
          <Trash size={14} />
        </IconButton>
      </div>
    </li>
  );
}

/** Single-stage confirm for un-pairing a device (the #241 confirm pattern,
 * without the two-stage force flow — a revoke has no dirty-workspace branch).
 * The design `<Dialog>` is presentational, so this owns Esc + focus itself and
 * stops key events from bubbling to an enclosing dialog's focus trap. */
function ConfirmRevokeDialog({
  device,
  onConfirm,
  onClose,
}: {
  device: DeviceInfoDto;
  onConfirm: () => void;
  onClose: () => void;
}) {
  const confirmRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    confirmRef.current?.focus();
    return () => previouslyFocused?.focus?.();
  }, []);

  function onKeyDown(e: KeyboardEvent) {
    // Nested inside the Settings dialog: keep Esc/Tab from reaching that
    // dialog's own trap by stopping propagation here.
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
  }

  const footer = (
    <>
      <Button variant="ghost" onClick={onClose}>
        Cancel
      </Button>
      <Button ref={confirmRef} variant="danger" onClick={onConfirm}>
        Revoke
      </Button>
    </>
  );

  return (
    <Dialog
      open
      title="Revoke device?"
      description="This un-pairs the device and ends any live session it holds."
      icon={<AlertTriangle size={18} />}
      onClose={onClose}
      onKeyDown={onKeyDown}
      footer={footer}
    >
      <p>
        Revoke{" "}
        <strong style={{ fontFamily: "var(--font-mono)" }}>
          {device.name}
        </strong>{" "}
        (
        <span style={{ fontFamily: "var(--font-mono)" }}>
          {device.fingerprint}
        </span>
        )? It will need to pair again to reconnect.
      </p>
    </Dialog>
  );
}
