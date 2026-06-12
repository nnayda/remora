# 0002. Build one Tauri codebase for all platforms, with an optional relay

- **Status:** Accepted
- **Date:** 2026-06-11
- **Issue/PR:** — <!-- predates the repo; product context in VISION.md -->

## Context

Remora must ship on macOS (priority) and Windows (required), with iOS and
Android wanted — as an open-source project, where install friction and "do I
have to host a server?" directly affect adoption (see
[VISION.md](../VISION.md)).

The session itself lives on the sandbox ([ADR-0001](0001-tmux-session-persistence.md)),
so every client is a thin window onto a remote tmux session. But the
phone-from-anywhere scenario and push notifications need an always-on
server-side component, which conflicts with "a new user must not be required
to host anything."

## Decision

We will build **one Tauri 2 codebase** compiled to macOS, Windows, iOS, and
Android, and make the relay **optional**.

- **Direct mode** is the zero-infra default: the app drives `ssh` /
  `kubectl exec` in-process. Download, point at your sandbox, go.
- **Relay mode** is an opt-in, self-hosted upgrade: the same session layer
  hosted behind a WebSocket, adding phone-from-anywhere and push
  notifications.

Both modes sit behind the `SessionSource` trait in `remora-core`, speaking
the same wire shape (`remora-protocol`). That seam is what keeps the relay
"the session layer, hosted" rather than a second product.

## Alternatives considered

- **Electron desktop + React Native mobile, direct SSH, no server:** the most
  proven embedded-terminal path, but two UI codebases to maintain, and the
  phone story barely works — direct SSH from a phone is painful and the
  errand scenario needs a server for notifications anyway.
- **Native SwiftUI app:** maximum Mac polish, zero Windows users, and the
  whole UI rebuilt for every other platform.
- **Phased: Tauri desktop now, design relay + mobile later:** lowest initial
  risk, rejected as the architecture — mobile and relay would be bolted onto
  a seam designed after the fact. Its sequencing survives in the roadmap:
  desktop direct mode still ships first.
- **Backend-first: publish an open session protocol + thin reference
  clients:** its best idea — the transport seam as a documented protocol —
  is folded in as `remora-protocol`, rather than maintaining a spec as the
  day-one product.

## Consequences

What becomes easier:

- One UI codebase across four platforms; Mac polish is styling, not a fork.
- Relay mode cannot drift from direct mode: both implement `SessionSource`.
- Third-party clients can target `remora-protocol` without us building them.

What becomes harder, and what we are committed to:

- Tauri mobile is the newer bet. A PTY rendered in a mobile webview
  (xterm.js) must be validated early; the phone client may need an adapted
  thin view instead of a full TUI (tracked in [VISION.md](../VISION.md) open
  questions).
- We must build, document, and ship the relay (container image, Helm chart)
  even though it is optional, plus document the no-relay mesh-VPN path.
- Every change to the session layer has to keep direct and relay mode at
  parity — the seam is a contract, not a suggestion.
