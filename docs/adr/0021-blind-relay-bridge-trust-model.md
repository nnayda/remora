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

This ADR **amends ADR-0002's relay definition**: what 0002 called "the same
session layer hosted behind a WebSocket" is the *bridge* below (never hosted
by us); the *relay* is a separate blind component that is the thing offered
hosted. ADR-0002's platform decision is untouched.

## Decision

We split relay mode into two components along the trust line:

```
Phone ──WS/TLS──► relay (blind forwarder) ──WS/TLS──► bridge ──ssh/kubectl──► sandbox (tmux)
Desktop ──┘          self-hosted OR hosted             ▲
                                                       │ bridge = desktop app itself (default)
  one Noise session runs END-TO-END phone⇄bridge,      │   or headless `remora-bridge`
  through the relay; per-hop WS/TLS is transport       │   container (opt-in, always-on)
  framing, never where encryption terminates           │
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

**Multi-client consequences (below the seam).** The moment a phone can act
on a session concurrently with the desktop, per-session mutual exclusion
becomes a **core/bridge responsibility** — today's guards against
open-vs-teardown orphaning live in the desktop frontend store, which a
phone acting through the bridge bypasses entirely. Relay slice 1 must own
the racy pairs (attach vs teardown, respawn vs close) below the
`SessionSource` seam. Likewise, concurrent attach from two clients hits
tmux's multi-client window-size arbitration (smallest-client clamping /
resize thrash); slice 1 picks and documents a policy. Device-facing session
identity is **(bridge ID, session ID)** — never a bare session ID — because
a desktop bridge and a headless bridge configured for the same hosts
discover the same sessions and are otherwise uncoordinated writers to the
same worktrees; the desktop+headless coexistence/migration story belongs to
the headless-bridge follow-up.

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
the handshake transcript binds both statics + PSK. **The QR itself is a
bearer credential while its window is open** — anyone who captures it
(photo, screen share, screenshot sync) holds everything needed to enroll —
so **enrollment is never silent**: the bridge surfaces every completed
pairing as an explicit, user-visible confirmation ("device enrolled:
&lt;key fingerprint&gt;") that the user acknowledges; a raced enrollment
becomes a visible intruder, not a shrugged-off failed scan. **Relay
credential issuance is bridge-attested**: the relay cannot see inside the
handshake, so the bridge — over its own authenticated relay connection —
tells the relay which handshake-proven device key to credential (no
unknown-key-share; the naive "device self-declares its key to the relay"
reintroduces the substitution attack). Credentials are scoped **per
(device, bridge) pairing**, so one bridge's revocation never over-revokes a
device's other pairings. The no-camera fallback is either the full
high-entropy payload as a copy-pasteable string or a PAKE over a short
code — chosen in the pairing follow-up issue, never a bare short code (too
little entropy for a PSK).

**Roster authority is per-bridge.** Each bridge owns its device roster;
pairing a phone with a second bridge is a second QR scan (stated UX, not a
surprise). Revoking = remove the pinned key from that bridge's roster and
invalidate the device's (device, bridge)-scoped relay credential. **The
bridge roster is the security boundary; relay-side invalidation is
defense-in-depth a malicious relay can ignore.** Because a stolen phone can
drive sessions through the bridge until revoked, revocation is a
first-class UX requirement — and since the roster lives on the bridge, a
user away from their hardware has no revocation path in v1 until they reach
a device that administers that bridge; remote revocation from another
paired device is pairing-follow-up scope, stated here so it is chosen, not
forgotten. Key rotation is deferred; re-pairing is the v1 escape hatch.

**Bridge compromise is its own threat, not just "stolen laptop".** A stolen
*bridge machine* exposes transport creds — identical to direct-mode desktop
today; the remedy (rotate ssh/kube creds) is outside Remora. But a
compromised **bridge key** additionally leaves the thief as every paired
phone's pinned, trusted E2E endpoint, able to keep operating through the
relay. Recovery therefore needs two more first-class flows, parallel to
device revocation: **device-side bridge unpinning** (a phone drops a bridge
from its trust set) and **relay-side bridge deregistration** (the operator
invalidates the bridge's registration). Both ride in the pairing follow-up
issue; ssh-cred rotation alone does not end a bridge-key compromise.

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
alongside `PROTOCOL_VERSION` — concretely, **the envelope's auth/hello
frame carries `PROTOCOL_VERSION`** (today no wire message does, so this is
the negotiation point that makes "gate compatibility on it" real); one
shared constant covers session messages, naming conventions, and envelope,
with the understood cost that a bump on any surface invalidates all modes.
Two envelope invariants are load-bearing:

- **Security-relevant control messages (auth, resume) are authenticated
  inside Noise or force a full re-handshake.** Envelope forgery must be
  availability-only *by construction* — a relay-forgeable resume token
  would be a session-hijack primitive.
- **Resume is loss-free and ordered on the same Noise session, or it
  surfaces to the session layer as a fresh attach.** Noise transport nonces
  require strictly ordered lossless delivery, and `ChannelOutput` carries
  per-attach one-shot semantics (`MarkerSeen` fires once per attach;
  `StatusChange` is ordered after its bytes) that silent lossy resume would
  break. The same nonce property means **relay load-shedding is
  connection-granular by construction**: a relay cannot drop individual
  frames without killing the E2E session, so the relay MVP needs bounded
  per-connection buffers with defined kill behavior, and the resume design
  must tolerate relay-induced re-handshake storms.

**Metadata policy.** The relay legitimately sees: opaque device IDs and
pairing-group association (routing); connection liveness, frame sizes,
timestamps (routing + operations); connection credentials (authenticating
connections, not content); push tokens (the one always-on job only the relay
can do); and, hosted only, account identity at the connection edge. The
protocol never *requires* the relay to see: session content, session
names/previews, host config, sandbox addresses or credentials, agent
identity, repo/branch names — and implementations must not leak these
through routing headers, push metadata, labels, logs, or crash reports.
One honest caveat rides with "frame sizes, timestamps": over an interactive
per-keystroke PTY stream, **timing and size are a known content
side-channel** (SSH-style inter-keystroke timing inference). This is an
accepted risk, named rather than hidden; envelope padding/coalescing is a
relay-slice-1 consideration, not a v1 guarantee.

**Push notifications.** The bridge sends a tiny E2E frame flagged "wake
device X"; the relay learns *that* attention is needed and *when*, never
*why*. Generic push ("a session needs you") is the v1 baseline; E2E-encrypted
payloads (iOS NSE, FCM data messages) are a platform-gated enhancement.
Delivery to official app builds requires the app publisher's APNs/FCM
credentials, which self-hosted relays don't have — so self-hosted relays
deliver push through an **explicitly opt-in**, Remora-operated **push
gateway** receiving only `{push token, generic wake}` (the Home
Assistant/Bitwarden pattern). The gateway requires **per-relay sender
registration and rate limits** (an unauthenticated gateway is an open
push-spam amplifier), and self-hosters opting in should know the residual
cost: push tokens and wake *timing* transit Remora infrastructure — the one
piece of metadata re-centralization in an otherwise self-hosted stack. Any
relay (or the gateway) can also *forge* generic wakes; that buys an
attacker attention timing, never content. UnifiedPush is the Android
zero-vendor alternative.

**Durable session record (#71 hook).** The relay can host **ciphertext
mailboxes**: blobs it cannot read, for the phone's instant session list. The
v1-simple floor is bridge-side per-device fanout — the bridge seals the
record to each paired device's *static public key* (or an HKDF-derived
at-rest key from the static-static DH); Noise *session* keys are never
reused for storage. O(devices) storage; no group-key machinery. Two
requirements #71 inherits rather than discovers: static-key sealing has
**no forward secrecy** (a later device-key compromise decrypts the mailbox
history the relay retained — consider rotating mailbox keys), and a
malicious relay can serve **stale blobs** (session-list rollback — consider
freshness binding). The final format belongs to #71.

**No-relay path.** The client ⇄ bridge layer is transport-independent: the
same Noise-over-WebSocket runs point-to-point over a mesh VPN (e.g.
Tailscale) with rendezvous by address — no relay at all. Mesh-mode pairing
is the same QR with the **bridge's tailnet address in place of the relay
URL and rendezvous token**; the PSK and static pinning are unchanged. In
mesh mode the bridge *does* listen on the tailnet ("outbound-only" is a
relay-mode property; the mesh is the boundary) — meaning it accepts
pre-handshake bytes from anything on the tailnet, a posture the pairing
follow-up examines. Trade-off: no relay ⇒ no always-on Remora-operated push
path (a bridge calling platform push directly would mean distributing the
app's push credentials — rejected).

## Threat model

| Attacker | What they get | What stops the rest |
| --- | --- | --- |
| Malicious relay operator | Metadata (who talks when, frame sizes/timing — a keystroke-timing side-channel, accepted and named above), forged generic wakes, DoS | Noise E2E: no plaintext, no creds; pinned keys + bridge-attested credential issuance: cannot inject peers or frames. Pairing window included: the relay holds only the rendezvous token, never the pairing secret, so it cannot enroll itself as a device |
| QR observer (photo, screen share, screenshot sync during pairing) | Everything needed to enroll as a device, until the single-use window closes | Short-lived single-use tokens bound the window; **mandatory user-visible enrollment confirmation** turns a raced enrollment into a visible intruder instead of a silent success |
| Network eavesdropper | TLS-wrapped ciphertext, connection metadata | TLS to relay + Noise inside; nothing readable at either layer |
| Stolen phone | Full session control through the bridge until revoked — attach, spawn, send keystrokes; no direct sandbox creds | Bridge-side revocation (roster removal + (device,bridge) credential invalidation) — a first-class UX requirement; the roster is the boundary, relay invalidation is defense-in-depth |
| Malicious paired device (compromised desktop on the same bridge) | Whatever that device's pairing grants: session control via the bridge; its own mailbox copies | Per-(device,bridge) scoping bounds it; it cannot alter other devices' pinned keys or read blobs sealed to other statics; roster changes surface as enrollment confirmations |
| Stolen bridge machine | Transport creds (same exposure as direct-mode desktop today) | Rotate ssh/kube creds — outside Remora, unchanged by relay mode |
| Compromised bridge **key** | Remains every paired phone's pinned, trusted E2E endpoint; can keep operating through the relay | Device-side bridge unpinning + relay-side bridge deregistration (first-class flows, pairing follow-up); ssh rotation alone does not end this |
| Hosted-tier account compromise | Can mint bridge registration tokens on that account | Registration admits a bridge to *routing* only — no device trusts it until a user completes PSK pairing with it; blast radius is quota abuse, not sessions |
| Malicious sandbox | Forged tmux/env metadata, garbage PTY output | Existing invariant: discovered state is untrusted display input; spawn/respawn build from local config only ([ADR-0004](0004-local-config-live-session-discovery.md)) |
| Compromised push gateway | Push tokens + wake timing of opted-in self-hosted users; forged wakes | Carries no content ever; per-relay sender registration + rate limits bound spam; explicitly opt-in |

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

Latency: relative to the desktop's direct connection, relay mode inserts
two user-space forwarding intermediaries (relay and bridge) plus Noise
encrypt/decrypt at the endpoints on the phone → relay → bridge → sandbox
path. Same order as mosh/ngrok-style relays, acceptable for an interactive
TUI; measured concretely in the relay-MVP issue before any optimization.

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
- Relay observability is designed so accidental leakage is **catchable**:
  an opt-in **audit mode** (sampled or per-connection-aggregate by default —
  always-on per-frame logging would put write amplification on the hot
  forwarding path) records exactly the fields the relay observed, for
  diffing against the protocol spec. Framed honestly: **cryptography is
  what guarantees blindness; the audit log is a regression guard against
  accidental leakage** (headers, labels, crash reports), not an audit of
  the operator — a malicious relay logs honestly and siphons separately.
  Genuinely verifiable for self-hosters (they control the binary); for the
  hosted tier it is self-attestation until reproducible builds land (a
  relay-MVP requirement). The audit log inherits the timing/size
  side-channel sensitivity, so it ships with short default retention and
  access controls — it must not convert a transient metadata exposure into
  a stored one.

What becomes harder, and what we are committed to:

- Two deployables to build, document, and ship (relay image; later a headless
  bridge image) — the headless bridge is a real operational surface (config
  storage, secrets, updates, health, pairing CLI), not a small follow-up.
  Said plainly: the headless bridge's *entire pairing story* is the
  no-camera fallback, which is deliberately undecided until the pairing
  follow-up — do not scope the headless bridge as small.
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
update and VISION.md open-question resolution (same PR); build issues —
relay slice 1 (#231: envelope + relay MVP + one E2E PTY stream, incl. audit
mode, reproducible-builds requirement, loopback test, cross-device mutual
exclusion below the seam, multi-client resize policy, bounded
per-connection buffers, padding/coalescing consideration), pairing flow
(#232: QR split-secret + PSK handshake, enrollment confirmation, no-camera
fallback, mesh-mode pairing, remote revocation, bridge
unpinning/deregistration), push pipeline (#233: incl. authenticated
rate-limited gateway), headless bridge (#234), SECURITY page (#235), and
PROTOCOL.md (#236) — cross-linked from #71 (which also inherits the
mailbox forward-secrecy and freshness-binding requirements).
