# 0023. Deliver v1 wake pushes over UnifiedPush, direct from the relay

- **Status:** Accepted
- **Date:** 2026-07-03
- **Issue/PR:** [#233](https://github.com/nnayda/remora/issues/233); refines
  the "Push notifications" decision in
  [ADR-0021](0021-blind-relay-bridge-trust-model.md)

## Context

ADR-0021 promises push: when an agent blocks for input and the paired device
is away, something must wake it — "a session needs you" — without the relay
learning *why*. The envelope already reserves `FrameType::PushTrigger = 3`;
today the relay closes any connection that sends it. Nothing registers push
endpoints, nothing triggers a wake, nothing delivers one.

Issue #233 as filed covers the full pipeline, including a Remora-operated
push gateway for official mobile apps (the self-hosted path ADR-0021
sketched: `{push token, generic wake}` relayed through Remora-run APNs/FCM
credentials). Remora is pre-alpha with **no mobile app** — the gateway
cannot be exercised against real APNs/FCM, because no app publisher
credentials exist yet. UnifiedPush delivery, by contrast, is fully testable
today: a plain HTTP POST to a UnifiedPush distributor endpoint (e.g. an ntfy
topic) lands a real notification on a real phone, with no Remora mobile app
at all. Building the gateway now would be unverifiable scaffolding; shipping
UnifiedPush now exercises the whole trigger → fan-out → relay-policy →
delivery pipeline end to end.

This ADR narrows #233 to that testable slice and fixes the decisions the
implementation needs before any code lands: the wire shape of a push
registration, who asserts it and to whom, what triggers a wake, and the
network-safety policy for a relay that POSTs to a URL supplied by someone
else's device.

## Decision

**v1 delivery is relay → direct HTTP POST to a device-supplied UnifiedPush
endpoint.** No Remora-operated gateway ships in this PR. The gateway ADR-0021
describes remains real future work — tracked as a fresh follow-up issue,
scoped precisely once official mobile apps and their APNs/FCM credentials
exist — not attempted half-built here.

**Registration is bridge-asserted, matching ADR-0021's soft-state pattern
for credentials.** A device tells its own bridge about its push endpoint
end-to-end, inside Noise: a new `RemoteOp::RegisterPushEndpoint {
registration: Option<PushRegistration> }`, answered by
`RemoteResult::PushEndpointSet` (`None` clears the registration). The bridge
persists the registration in its per-device roster entry and includes it in
every `AssertDevices` call to the relay, alongside the existing routing
credentials. The relay never receives a registration except via its owning
bridge's assert, and never persists push state independently — a relay
restart is free: the bridge's next `AssertDevices` re-supplies everything.
This is the same shape ADR-0021 chose for relay credential issuance
("bridge-attested", never device-declared) applied to push state, not a new
pattern.

**The registration is a structured, forward-compatible enum, not a bare
URL.** `PushRegistration` is `#[non_exhaustive]` with one v1 variant:
`UnifiedPush { endpoint: String }`. A bare string would be the wrong durable
abstraction — the moment an APNs/FCM path or the Remora gateway itself needs
a token shape, device attributes, or platform hints instead of a URL, a
string forces a breaking reshape of stored roster state and wire messages.
An enum lets `Apns { device_token, .. }`, `Fcm { .. }`, or `Gateway { .. }`
arrive as new variants later. `AssertedDevice` gains `push:
Option<PushRegistration>` (serde-default, so old asserts without the field
still decode; absent means no push for that device).

**The trigger is a desktop-shell tee of `StatusChange`, not a bridge-side
session watcher.** The desktop already receives every session's status
transitions to drive its own UI; forwarding them to the bridge's wake API
(`note_session_status`) is a few lines on an existing event, non-blocking so
a full outbound wake queue never stalls the terminal I/O path. A bridge-side
watcher — one that itself attaches to every roster session to notice
`Awaiting` independent of any open desktop tab — is the more complete
design, and is deliberately deferred to the headless-bridge scope (#234):
each such watch is a real ssh/tmux (or kubectl exec) attach pipe, expensive
per session, and today's only bridge host is the desktop process itself, so
the tee already delivers the VISION relay-milestone scenario (a
laptop-started session with its tab open, phone buzzes when it blocks).
**Accepted consequence, named plainly: v1 wakes fire only for sessions with
an open desktop tab.** A session the desktop never opened, or a session a
now-disconnected device was driving with no other tab watching it, sends no
wake in v1. This is the gap #234's bridge-side watcher exists to close.

**The relay's network-target policy (SSRF) is fixed now, not deferred**,
because "POST to a URL supplied by an untrusted device" is exactly the
shape of a server-side request forgery vector and retrofitting a safety
policy onto a shipped delivery path is worse than deciding it before the
first line of delivery code:

- Before every POST, the relay resolves the endpoint host itself and checks
  every resolved address against policy — **denying loopback, link-local
  (including `169.254.0.0/16` and its cloud-metadata callers, and IPv6
  `fe80::/10`), private ranges (RFC 1918, IPv6 ULA `fc00::/7`, and deprecated
  IPv6 site-local `fec0::/10`), CGNAT shared space
  (`100.64.0.0/10`), the whole `0.0.0.0/8` (which routes to the local host on
  Linux, so `0.0.0.1` reaches the same place loopback does), and unspecified
  addresses by default.** A second tier is blocked *unconditionally*, since no
  legitimate push target ever lives there: the RFC 5737 documentation ranges
  (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`), RFC 2544 benchmarking
  (`198.18.0.0/15`), reserved space (`240.0.0.0/4`), the tunnelled-v4 IPv6
  prefixes 6to4 (`2002::/16`) and Teredo (`2001:0::/32`) — blocked whole rather
  than decoding their embedded (for Teredo, XOR-obfuscated) v4, which buys an
  SSRF surface a real target never needs — and the non-routable IPv6 identifier
  prefixes: documentation (`2001:db8::/32`) and ORCHID/ORCHIDv2
  (`2001:10::/28`, `2001:20::/28`). `allow_private_endpoints = true`
  re-admits loopback as well as private/ULA/CGNAT/site-local and `0.0.0.0/8`
  targets — for
  LAN self-hosters (an ntfy instance on the same network) who deliberately want
  one of those. Link-local/cloud-metadata and the unconditional tier stay
  blocked regardless: no config flag re-admits them. One honest asymmetry worth naming: IPv4
  cloud-metadata (`169.254.169.254`) is link-local and so is **always**
  blocked, but the IPv6 equivalent some clouds expose (e.g. AWS's
  `fd00:ec2::254`) is a ULA address, not link-local — so with
  `allow_private_endpoints = true` it is classified as an admitted private
  target, not blocked metadata. A relay operator who opts in to LAN
  self-hosting on a cloud-hosted relay should know that trade-off.
- The relay **pins the checked address** on the HTTP client (reqwest
  `resolve()`) rather than letting the client re-resolve at connect time —
  closing the DNS-rebinding gap where a hostname resolves to a safe address
  during the check and an unsafe one microseconds later. The delivery client
  also **disables proxies** (`no_proxy()`): reqwest otherwise honours
  `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` from the environment, and a proxy would
  re-resolve the endpoint host at its own end — bypassing both the address
  filter and this pin — so delivery must always connect straight to the checked
  address.
- Redirects are disabled outright; a redirect is an unchecked second
  destination.
- `allow_http = true` admits cleartext `http://` **only when the resolved
  target is private or loopback** — cleartext to the public internet is
  never allowed regardless of this flag.
- Delivery is bounded three ways: a **global in-flight semaphore** (so a
  burst of simultaneous wakes cannot exhaust sockets/DNS/memory), a
  **per-device cooldown** (so one flapping session cannot spam one phone),
  and a **per-bridge token-bucket budget** (so one compromised or buggy
  bridge cannot spam the relay's whole outbound path).

**Metadata honesty carries ADR-0021's framing forward, not around it.** The
relay learns *that* a session needs attention and *when* — never *why*: no
session name, branch, host, agent identity, or bridge identity appears in
the POST's URL suffix, headers, or body. The body is a fixed constant
string (`"A session needs your attention"`), identical for every wake from
every bridge. But, exactly as ADR-0021 already says about the metadata
policy in general, **generic content is not metadata-free**: an operator
watching wake traffic still learns that some Remora session, somewhere,
needed attention, right now — timing is content. A relay (or, in the
gateway's future, the gateway) can also *forge* a generic wake to any
registered endpoint; per ADR-0021's own framing, forging buys an attacker
attention timing, never session content. Nothing in this ADR changes that
calculus; it only builds the first concrete path that exercises it.

## Alternatives considered

- **Build the Remora-operated push gateway now**, as #233 originally
  scoped it. Rejected for this PR: it cannot be tested against real APNs/FCM
  without app-publisher credentials that don't exist yet, so it would ship
  as unverified scaffolding. Real work, better done as a precisely-scoped
  follow-up once the credentials exist.
- **Device self-registers its push endpoint directly with the relay**
  (device → relay, bypassing the bridge). Rejected: it reintroduces exactly
  the pattern ADR-0021 rejected for credential issuance — the relay would
  trust a client-declared identifier with no bridge attestation behind it,
  and it would need its own persistence story (the bridge-asserted design
  gets restart recovery for free).
- **A bare endpoint URL as the registration type.** Rejected: the first
  APNs/FCM/gateway variant would force a breaking reshape of roster state
  and wire messages. A `#[non_exhaustive]` enum with a single v1 variant
  costs one match arm today and buys forward compatibility.
- **Bridge-side session watcher as the v1 trigger**, attaching to every
  roster session independent of any open desktop tab. More complete —
  wakes for sessions no desktop tab has open — but each watch is a real
  ssh/tmux or kubectl exec attach pipe; today's only bridge host is the
  desktop process itself, so the incremental cost buys nothing this
  iteration doesn't already get from the tee. Properly a #234
  (headless-bridge) concern, where the bridge runs independently of any
  desktop tab in the first place.
- **Defer the SSRF/network-target policy to a follow-up, ship delivery
  first.** Rejected: an enabled relay POSTing device-supplied URLs with no
  network policy is an SSRF vector from the first release, not a hardening
  gap to close later. The policy is cheap to decide now (resolve-check-pin
  + a static deny-list) and expensive to retrofit onto a path already in
  use.
- **Reject a policy-invalid registration at `AssertDevices` time** (e.g. an
  `http://` endpoint when `allow_http = false`). Rejected: `AssertDevices`
  is a full-replacement, all-or-nothing assert of a bridge's whole device
  set; failing it over one bad endpoint would kick every other,
  correctly-configured device off routing. The relay instead accepts the
  assert and logs the invalid entry (audit-logged too), then drops at
  delivery time with a clear reason — the operator sees the misconfiguration
  at assert time, not at the first missed wake, without punishing every
  other device.
- **Do nothing** (leave `PushTrigger` unimplemented). Leaves ADR-0021's
  wake promise entirely unfulfilled and the relay-milestone VISION scenario
  undemonstrated.

## Consequences

Easier:

- The pipeline is verifiable today with real hardware: pair a phone (or a
  second desktop) over `REMORA_REMOTE_LOOPBACK`, point `push_wake_url` at an
  ntfy topic, and a real phone buzzes when a session blocks — no Remora
  mobile app required. This exercises trigger → fan-out → relay policy →
  delivery; it does not exercise app wake/resume, which needs the mobile
  app itself and stays with VISION step 7.
- `PushRegistration` absorbs future push channels (APNs, FCM, the gateway)
  as new enum variants without another roster/wire reshape.
- The relay's network-target policy is decided once, in one place, before
  any delivery code exists — later gateway or additional-channel work
  inherits it rather than re-litigating it.

Harder, and what we are committed to:

- **The relay becomes a low-rate outbound-POST reflector.** With push enabled,
  an authenticated bridge can cause the relay to POST a fixed constant body to
  a public HTTPS URL its device supplied — a bounded server-side-request-forgery
  surface, not a theoretical one. It is bounded on every axis: a fixed constant
  body (no attacker-chosen bytes), the resolve-check-pin + deny-list network
  policy above, the per-device cooldown, the per-bridge token-bucket budget, and
  the global in-flight concurrency cap. Named, and accepted, as a bounded
  reflector — not eliminated.
- **v1 wakes only fire for sessions with an open desktop tab** — the tee's
  accepted gap. A session a disconnected device was driving, or one no
  desktop ever opened, gets no wake until #234's bridge-side watcher lands.
- **Rate limiting is per-bridge only in v1**; a global cross-bridge cap is
  gateway-follow-up scope, needed once a hosted relay serves many
  bridges/customers.
- **A connected-but-suspended mobile device is indistinguishable from an
  awake one** by the relay's "currently connected" check, so it is wrongly
  treated as not needing a wake. Real once a mobile app exists; named here
  so it isn't rediscovered as a surprise.
- **The Remora-operated push gateway remains unbuilt.** #233 closes with
  this PR's generic-wake slice; the gateway gets its own follow-up issue,
  filed and scoped precisely (not a vague continuation) so the tracker
  records the split rather than leaving #233 as a half-done zombie.
- New `reqwest` dependency in `remora-relay` changes the relay's dependency
  tree and image; the reproducible-build workflow (#251) and `deny.toml`
  (webpki-roots precedent, #253) need to stay green against it.
- Follow-ups this decision creates: the Remora-operated push gateway
  (fresh issue, blocked on mobile app credentials), E2E-encrypted push
  payloads (the `PushTrigger` payload field stays reserved and empty until
  then), the bridge-side session watcher (#234), and a Settings UI field
  for `push_wake_url` (v1 is config-file-only).
