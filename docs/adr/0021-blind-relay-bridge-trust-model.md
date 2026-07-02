# 0021. Split relay mode into a blind relay and a user-side bridge, paired end-to-end

- **Status:** Accepted
- **Date:** 2026-07-02
- **Issue/PR:** [#68](https://github.com/nnayda/remora/issues/68)

## Context

The phone-from-anywhere hero and push notifications ([VISION.md](../VISION.md))
need a relay: something the desktop and phone both reach, so a phone behind
cellular NAT can talk to a session on a sandbox behind a corporate firewall.
[ADR-0002](0002-tauri-single-codebase-optional-relay.md) made the relay an
opt-in upgrade but left the trust model open, and ARCHITECTURE.md's old sketch
(`App ──WS──► relay ──ssh──► sandbox`) had the relay driving ssh/kubectl
itself — meaning the relay would hold sandbox credentials and see plaintext.
That is incompatible with a relay anyone should trust, self-hosted or hosted.

Two business modes must work with one design: users self-host the relay, or
pay for a hosted relay subscription. The hosted offering is only credible if
the operator *cannot* read customer sessions or hold their credentials.

The market moved while this was open: by mid-2026, E2E-encrypted
laptop-bridge relays are table stakes in the "control your coding agent from
your phone" category (Happy, Paseo, Sesori all ship the shape; Omnara
publicly cannot, because its service reads content). None of them keep the
agent off user hardware — their bridge is the machine *running the agent*.
Remora's differentiator is the sandbox-first inversion: the agent and code
stay in a disposable remote sandbox; the user-side component holds only
transport credentials.

## Decision

We split relay mode into two components along the trust line:

```
Phone ───E2E───► relay (blind forwarder) ───E2E───► bridge ──ssh/kubectl──► sandbox (tmux)
Desktop ──┘         self-hosted OR hosted            ▲
                                                     │ bridge = desktop app itself (default)
                                                     │   or headless `remora-bridge`
                                                     │   container (opt-in, always-on)
```

**The relay is a blind forwarder.** It accepts *outbound* WebSocket
connections from devices and bridges (in relay mode nothing dials in to user
machines) and routes opaque ciphertext frames between paired peers. It holds
routing state, push tokens, and connection credentials — never E2E keys,
sandbox credentials, or plaintext session content. Blindness is
content-blindness, not traffic privacy: the relay remains a metadata
authority (which opaque devices talk, when, how much). This is the component
that gets hosted — by the user (container image + Helm chart) or by us as the
paid subscription. Hosted service operations (accounts, quotas, billing,
abuse) are out of this ADR's scope; the protocol constrains them only in that
they never touch content or credentials.

**The bridge is the hosted `SessionSource`.** It holds host config +
transport creds and drives ssh/kubectl exactly like direct mode today — it is
the "future relay binary will host remora-core behind a WebSocket" that
ARCHITECTURE.md promised, renamed to what it actually is. It runs **only on
user hardware**: inside the desktop app by default (phone reach while the
desktop is up), or as a headless `remora-bridge` container next to the
sandboxes for laptop-asleep access. It is never part of the hosted
subscription — hosting bridges would mean holding customer sandbox creds.
When the desktop hosts the bridge, the desktop's own UI keeps using the
in-process `SessionSource`; the bridge serves remote devices only.

**Pairing (QR, split-secret).** Each device generates a static X25519 keypair
on-device; private keys never leave it; the relay is never given any E2E key.
The bridge generates two secrets and displays a QR encoding `{relay URL,
rendezvous token, pairing secret, bridge static public key}`:

- the **rendezvous token** is registered with the relay and is the only
  secret the relay ever sees — presenting it admits the new device to
  routing, nothing more;
- the **pairing secret** never reaches the relay (the QR travels
  desktop-screen → phone-camera): it is mixed into the Noise handshake as a
  PSK, so the phone proves knowledge of it to the bridge *inside* the
  encrypted transcript.

The phone authenticates the bridge by the QR-carried static key; the bridge
authenticates the phone by the PSK proof; both pin each other's statics. A
malicious relay holds only the rendezvous token — it can route itself to the
bridge but fails the PSK handshake, so it cannot enroll itself as a device,
even during the pairing window. The QR binds relay URL + bridge key + secret;
the handshake transcript binds both statics + PSK; the relay credential is
issued to the handshake-proven device key (no unknown-key-share). The
no-camera fallback is either the full high-entropy payload as a
copy-pasteable string or a PAKE over a short code — chosen in the pairing
follow-up issue, never a bare short code (too little entropy for a PSK).

**Roster authority is per-bridge.** Each bridge owns its device roster;
pairing a phone with a second bridge is a second QR scan (stated UX, not a
surprise). Revoking = remove the pinned key from that bridge's roster and
invalidate the device's relay credential; because a stolen phone can drive
sessions through the bridge until revoked, revocation is a first-class UX
requirement. Key rotation is deferred; re-pairing is the v1 escape hatch. A
stolen *bridge* machine exposes transport creds — identical to direct-mode
desktop today; the remedy (rotate ssh/kube creds) is outside Remora.

**Bridge→relay registration.** A bridge bootstraps onto a relay with an
operator-issued registration token, then authenticates by its Noise static
key. Self-hosted: the token lives in relay config, **closed registration by
default**, open registration an explicit opt-in flag. Hosted: the token is
minted at subscription signup — the paid tier's enforcement point. One
mechanism, both business modes, no account system forced on self-hosters.

**Crypto.** Noise protocol framework (`snow` crate, X25519 +
ChaCha20-Poly1305), pairwise between the user's devices (phone ⇄ bridge,
desktop ⇄ bridge). First pairing is an IKpsk-family handshake (the initiator
knows the responder's static from the QR; pairing secret as PSK); subsequent
sessions are IK between pinned statics. Exact pattern name, payloads, and
transcript bindings are fixed in the pairing follow-up issue; this ADR
commits to the family and the PSK requirement. The Noise *pattern* is the
proven "pairwise devices, hostile middle" shape (WireGuard/WhatsApp lineage;
Messenger runs Noise Pipes); the `snow` *crate* is actively maintained but
has **no formal audit** — pin ≥ 0.9.5 (GHSA-97f8-h76h-f297 fixed there); an
accepted risk shared with much of the Rust ecosystem. The relay terminates
TLS but never a Noise session.

**Wire shape.** The E2E channel carries the existing `remora-protocol`
session messages as payload; the relay routes the protocol and is never a
party to it. The new surface is a thin outer envelope: a routing header the
relay parses (source/destination device IDs; frame type: data / pairing /
push-trigger) wrapping opaque Noise ciphertext. Remote-mode control messages
(auth, liveness, reconnect/resume, backpressure) live at the envelope layer —
the *session* protocol rides unchanged, but remote mode is not zero new
messages. Envelope types will live in `remora-protocol` (dependency-light;
`snow` binds in core/bridge/clients, not the protocol crate) and version
alongside `PROTOCOL_VERSION`.

**Metadata policy.** The relay legitimately sees: opaque device IDs and
pairing-group association (routing); connection liveness, frame sizes,
timestamps (routing + operations); connection credentials (authenticating
connections, not content); push tokens (the one always-on job only the relay
can do); and, hosted only, account identity at the connection edge. The
protocol never *requires* the relay to see: session content, session
names/previews, host config, sandbox addresses or credentials, agent
identity, repo/branch names — and implementations must not leak these
through routing headers, push metadata, labels, logs, or crash reports.

**Push notifications.** The bridge sends a tiny E2E frame flagged "wake
device X"; the relay learns *that* attention is needed and *when*, never
*why*. Generic push ("a session needs you") is the v1 baseline; E2E-encrypted
payloads (iOS NSE, FCM data messages) are a platform-gated enhancement.
Delivery to official app builds requires the app publisher's APNs/FCM
credentials, which self-hosted relays don't have — so self-hosted relays
deliver push through an optional Remora-operated **push gateway** receiving
only `{push token, generic wake}` (the Home Assistant/Bitwarden pattern);
UnifiedPush is the Android zero-vendor alternative.

**Durable session record (#71 hook).** The relay can host **ciphertext
mailboxes**: blobs it cannot read, for the phone's instant session list. The
v1-simple floor is bridge-side per-device fanout — the bridge seals the
record to each paired device's *static public key* (or an HKDF-derived
at-rest key from the static-static DH); Noise *session* keys are never
reused for storage. O(devices) storage; no group-key machinery. The final
format belongs to #71.

**No-relay path.** The client ⇄ bridge layer is transport-independent: the
same Noise-over-WebSocket runs point-to-point over a mesh VPN (e.g.
Tailscale) with rendezvous by address — no relay at all. In mesh mode the
bridge *does* listen on the tailnet ("outbound-only" is a relay-mode
property; the mesh is the boundary). Trade-off: no relay ⇒ no always-on
Remora-operated push path (a bridge calling platform push directly would
mean distributing the app's push credentials — rejected).

## Threat model

| Attacker | What they get | What stops the rest |
| --- | --- | --- |
| Malicious relay operator | Metadata (who talks when, sizes, push timing), DoS | Noise E2E: no content, no creds; pinned keys: cannot inject peers or frames. Pairing window included: the relay holds only the rendezvous token, never the pairing secret, so it cannot enroll itself as a device |
| Network eavesdropper | TLS-wrapped ciphertext, connection metadata | TLS to relay + Noise inside; nothing readable at either layer |
| Stolen phone | Full session control through the bridge until revoked — attach, spawn, send keystrokes; no direct sandbox creds | Bridge-side revocation (roster removal + relay-credential invalidation) — a first-class UX requirement |
| Stolen bridge machine | Transport creds (same exposure as direct-mode desktop today) | Rotate ssh/kube creds — outside Remora, unchanged by relay mode |
| Malicious sandbox | Forged tmux/env metadata, garbage PTY output | Existing invariant: discovered state is untrusted display input; spawn/respawn build from local config only ([ADR-0004](0004-local-config-live-session-discovery.md)) |

Precisely, on a compromised relay: AEAD kills frame forgery; Noise session
nonces kill in-session replay/reordering; what a hostile relay *can* still do
is drop/delay frames, flood, and spam pairing attempts — availability
attacks, handled as relay-level rate/abuse concerns, never trust breaks.

## Degraded modes and latency

| Failure | Behavior |
| --- | --- |
| Relay down | Direct mode unaffected (never touches the relay). Phone loses reach; bridge reconnects outbound with backoff — safe, no listener to re-expose |
| Bridge down | Sessions stay alive in tmux ([ADR-0001](0001-tmux-session-persistence.md)). Phone sees them unreachable until the bridge returns; reconnect is re-attach |
| Pairing token expired / scan failed | Tokens are short-lived and single-use; re-issue a fresh QR. No state to clean up |

Latency: relay mode adds one user-space forwarding hop (phone → relay →
bridge → sandbox). Same order as mosh/ngrok-style relays, acceptable for an
interactive TUI; measured concretely in the relay-MVP issue before any
optimization.

## Alternatives considered

- **Relay drives ssh/kubectl** (the old ARCHITECTURE.md sketch): the relay
  holds creds and plaintext — untrustable hosted, unattractive self-hosted.
- **Sandbox-side bridge** (provisioned at spawn like
  [ADR-0020](0020-launch-time-hook-injection.md)'s hook scripts): needs no
  transport creds and survives laptop sleep, but puts a Remora daemon on the
  sandbox — violates "nothing custom on the sandbox" — and adds per-arch
  binary delivery. May be revisited; not v1.
- **Desktop-only bridge, no headless option:** simplest, but caps the hero at
  "phone works while the laptop is awake" with no upgrade path.
- **OIDC pairing:** drags an identity provider into a no-server-required
  open-source product and doesn't itself yield E2E keys.
- **Long-lived shared token:** leaks, no per-device identity, all-or-nothing
  revocation.
- **Single-token pairing (no PSK split):** rejected after review — the relay
  sees the authenticator and can race the phone to enroll itself.
- **MLS / double-ratchet:** messaging-grade group semantics for N devices of
  one user attaching to live PTYs — overkill.
- **Raw libsodium `crypto_box`:** hand-assembles the session/rekey/replay
  logic Noise provides.
- **Do nothing:** the mobile/relay axis gets owned by the laptop-bridge
  competitors while Remora stays desktop-only.

## Consequences

What becomes easier:

- The paid hosted relay has a defensible story: the operator cannot read
  sessions or hold sandbox keys, by construction.
- Self-hosting is one small container (plus Helm chart); the same binary and
  trust model as hosted. Scaling is permitted by design — routing/credential/
  push state is small, per-group, and externalizable; cross-instance frame
  delivery still needs ordinary connection-affinity/pub-sub plumbing (the E2E
  design just adds no burden beyond that standard machinery).
- #71's durable record has a home (ciphertext mailbox) without the relay
  reading it.
- Relay observability is designed so content-blindness is **auditable**: the
  relay emits a structured log of exactly the fields it observed per frame
  (routing header only) for auditors to diff against the protocol spec.
  Scoped honestly: genuinely verifiable for self-hosters (they control the
  binary); for the hosted tier it is self-attestation until reproducible
  builds land (a relay-MVP requirement).

What becomes harder, and what we are committed to:

- Two deployables to build, document, and ship (relay image; later a headless
  bridge image) — the headless bridge is a real operational surface (config
  storage, secrets, updates, health, pairing CLI), not a small follow-up.
- With only a desktop bridge, phone-from-anywhere requires the desktop awake;
  the headless bridge is the documented path past that.
- Normal desktop use never exercises the remote/E2E path — relay slice 1
  carries an explicit loopback test (desktop attaches via its own bridge) so
  the remote path stays dogfooded.
- We operate a (tiny, contentless) push gateway so self-hosted relays can
  wake the official mobile apps.
- Every session-layer change must keep direct, relay, and mesh modes at
  parity — the envelope is a contract.

Follow-ups this decision creates: ARCHITECTURE.md diagram + invariants
update and VISION.md open-question resolution (same PR); build issues for
relay slice 1 (envelope + relay MVP + one E2E PTY stream, incl. audit log,
reproducible-builds requirement, loopback test), pairing flow, push pipeline
(incl. gateway), headless bridge, SECURITY page, and PROTOCOL.md — filed
with this PR and cross-linked from #71.
