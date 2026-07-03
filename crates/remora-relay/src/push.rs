//! Push-wake policy: opt-in `[push]` config, stored registrations, and the
//! pure wake **decision** (ADR-0023, spec Task 6).
//!
//! This module owns the relay's *policy* half of the push pipeline: whether a
//! well-formed `PushTrigger` from a bridge should turn into a delivered wake,
//! and to which endpoint. It performs **no network I/O** — the HTTP POST, the
//! SSRF resolve-check-pin, and the global in-flight semaphore are Task 7's
//! delivery half, which consumes the endpoint [`decide_wake`] returns on
//! success. Keeping the decision pure and clock-injected ([`std::time::Instant`]
//! threaded in) makes the cooldown and per-bridge budget deterministically
//! testable without a real clock.
//!
//! State that must outlive a single frame — the per-device last-wake instant
//! (cooldown) and the per-bridge token bucket (budget) — lives in [`PushState`],
//! owned alongside the router's registry. Crucially it is keyed by `DeviceId` /
//! bridge id and is **not** rebuilt from a bridge's `AssertDevices`: a re-assert
//! replaces the stored registrations but a device's cooldown survives, so a
//! bridge cannot reset a phone's cooldown by re-asserting.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use remora_protocol::{validate_push_endpoint, DeviceId, PushRegistration};
use serde::Deserialize;
use tokio::net::lookup_host;
use tokio::sync::Semaphore;

/// The one and only wake body — a fixed constant, identical for every wake from
/// every bridge (ADR-0023 metadata policy: the relay learns *that* a session
/// needs attention, never *why*, so nothing session-identifying may appear in
/// the POST's body, headers, or URL suffix).
const WAKE_BODY: &str = "A session needs your attention";

/// Per-delivery hard deadline (connect + TLS + response). A slow or
/// black-holing endpoint must not pin an in-flight permit indefinitely.
const WAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Opt-in push-wake configuration, the `[push]` section of the relay's TOML
/// (ADR-0023). Absent section = every field its default = push disabled.
///
/// Each field carries `#[serde(default = …)]` so a *partial* `[push]` table
/// fills the rest, and [`PushConfig::default`] mirrors those same defaults so an
/// *absent* table (the field's own `#[serde(default)]` on [`crate::RelayConfig`])
/// yields an identical value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PushConfig {
    /// Master switch. `false` (the default) means the relay never delivers a
    /// wake: a well-formed `PushTrigger` is still accepted (never a protocol
    /// violation) but its decision is [`DropReason::Disabled`].
    #[serde(default = "default_push_enabled")]
    pub enabled: bool,
    /// Admit cleartext `http://` endpoints. Delivery-time policy (Task 7) still
    /// only honours this for private/loopback targets; stored here so the whole
    /// network policy reads from one struct.
    #[serde(default = "default_allow_http")]
    pub allow_http: bool,
    /// Admit endpoints that resolve to private/loopback addresses (LAN
    /// self-hosters). Enforced at delivery time (Task 7); stored here now.
    #[serde(default = "default_allow_private_endpoints")]
    pub allow_private_endpoints: bool,
    /// Minimum seconds between two delivered wakes for the *same device*, so one
    /// flapping session cannot spam one phone. `0` disables the cooldown.
    #[serde(default = "default_device_cooldown_secs")]
    pub device_cooldown_secs: u64,
    /// Token-bucket budget of delivered wakes per *bridge* per minute, so one
    /// compromised or buggy bridge cannot spam the relay's outbound path. `0`
    /// blocks every wake from that bridge.
    #[serde(default = "default_per_bridge_per_minute")]
    pub per_bridge_per_minute: u32,
    /// Global cap on simultaneously in-flight deliveries (Task 7's semaphore
    /// bound). Stored here now; enforced by the delivery half.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
}

fn default_push_enabled() -> bool {
    false
}
fn default_allow_http() -> bool {
    false
}
fn default_allow_private_endpoints() -> bool {
    false
}
fn default_device_cooldown_secs() -> u64 {
    30
}
fn default_per_bridge_per_minute() -> u32 {
    10
}
fn default_max_in_flight() -> usize {
    32
}

impl Default for PushConfig {
    fn default() -> PushConfig {
        PushConfig {
            enabled: default_push_enabled(),
            allow_http: default_allow_http(),
            allow_private_endpoints: default_allow_private_endpoints(),
            device_cooldown_secs: default_device_cooldown_secs(),
            per_bridge_per_minute: default_per_bridge_per_minute(),
            max_in_flight: default_max_in_flight(),
        }
    }
}

/// Why a well-formed `PushTrigger` did **not** produce a delivered wake.
///
/// Every variant is a routine delivery-policy outcome, never a protocol
/// violation — the sending bridge continues after any of them (spec Task 6).
/// Recorded/logged for observability so an operator can see *why* a wake was
/// suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Push delivery is disabled (`[push] enabled = false`).
    Disabled,
    /// The target device currently has a live relay connection, so it does not
    /// need a wake.
    DstConnected,
    /// The device has no registered push endpoint.
    NoEndpoint,
    /// A delivered wake for this device is still within its cooldown window.
    Cooldown,
    /// The sending bridge has exhausted its per-minute wake budget.
    BridgeBudget,
    /// The device's registered endpoint failed syntax validation at assert
    /// time (stored-but-flagged) or is an unsupported registration variant.
    PolicyInvalid,
}

impl DropReason {
    /// Stable snake_case name for logs and any future audit surface.
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Disabled => "disabled",
            DropReason::DstConnected => "dst_connected",
            DropReason::NoEndpoint => "no_endpoint",
            DropReason::Cooldown => "cooldown",
            DropReason::BridgeBudget => "bridge_budget",
            DropReason::PolicyInvalid => "policy_invalid",
        }
    }
}

/// A device's push registration as the relay stores it, with the assert-time
/// syntax-validation verdict cached alongside.
///
/// ADR-0023: a policy-invalid endpoint (or an unsupported future
/// [`PushRegistration`] variant) is **stored, not rejected** — failing a
/// full-set `AssertDevices` over one bad endpoint would kick every other
/// correctly-configured device off routing. The flag is carried here so
/// delivery drops it cleanly at wake time ([`DropReason::PolicyInvalid`]) while
/// the operator already saw the warning at assert time.
#[derive(Debug, Clone)]
pub struct StoredRegistration {
    endpoint: String,
    /// `Some(reason)` if the endpoint failed validation (or the variant is
    /// unsupported); `None` if it is a syntactically valid endpoint.
    invalid: Option<String>,
}

impl StoredRegistration {
    /// Builds a stored registration from an asserted [`PushRegistration`],
    /// caching whether its endpoint passes [`validate_push_endpoint`].
    pub fn from_registration(reg: &PushRegistration) -> StoredRegistration {
        match reg {
            PushRegistration::UnifiedPush { endpoint } => StoredRegistration {
                invalid: validate_push_endpoint(endpoint)
                    .err()
                    .map(|e| e.to_string()),
                endpoint: endpoint.clone(),
            },
            // `PushRegistration` is `#[non_exhaustive]`: an unknown future
            // variant carries no endpoint this build can POST to, so flag it
            // policy-invalid rather than guess.
            _ => StoredRegistration {
                endpoint: String::new(),
                invalid: Some("unsupported push registration variant".to_string()),
            },
        }
    }

    /// The validation-failure reason, if this endpoint was flagged at assert
    /// time; `None` for a syntactically valid endpoint.
    pub fn invalid_reason(&self) -> Option<&str> {
        self.invalid.as_deref()
    }

    fn is_invalid(&self) -> bool {
        self.invalid.is_some()
    }
}

/// Per-bridge / per-device budgeting state that must outlive a single frame.
///
/// Owned by the router alongside its registry, keyed independently of any
/// bridge's asserted set so a re-assert never resets it.
#[derive(Default)]
pub struct PushState {
    /// Last *delivered* wake per device (cooldown key).
    last_wake: HashMap<DeviceId, Instant>,
    /// Token bucket per bridge (budget key).
    buckets: HashMap<DeviceId, TokenBucket>,
}

/// A lazily-refilled token bucket: `tokens` is topped up on each access by the
/// time elapsed since `last_refill`, capped at the configured capacity.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl PushState {
    /// Attempts to consume one wake token from `bridge_id`'s bucket, refilling
    /// it for the time elapsed since its last access. Returns whether a token
    /// was available (and consumed).
    fn take_bridge_token(
        &mut self,
        bridge_id: DeviceId,
        config: &PushConfig,
        now: Instant,
    ) -> bool {
        let capacity = f64::from(config.per_bridge_per_minute);
        if capacity <= 0.0 {
            return false;
        }
        let refill_per_sec = capacity / 60.0;
        let bucket = self.buckets.entry(bridge_id).or_insert(TokenBucket {
            tokens: capacity,
            last_refill: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Decides whether a well-formed `PushTrigger` should deliver a wake, and to
/// which endpoint — the pure policy core (ADR-0023, spec Task 6).
///
/// Gates run in priority order: disabled → target already connected → no
/// endpoint → policy-invalid endpoint → device cooldown → bridge budget. On
/// success the device's cooldown is stamped and one bridge token is consumed
/// (both mutate `state`); on any drop `state` is left as if no wake fired
/// (a refused bridge token is not consumed). `now` is injected so cooldown and
/// budget are deterministic in tests.
pub fn decide_wake(
    config: &PushConfig,
    state: &mut PushState,
    bridge_id: DeviceId,
    device_id: DeviceId,
    registration: Option<&StoredRegistration>,
    dst_connected: bool,
    now: Instant,
) -> Result<String, DropReason> {
    if !config.enabled {
        return Err(DropReason::Disabled);
    }
    if dst_connected {
        return Err(DropReason::DstConnected);
    }
    let Some(reg) = registration else {
        return Err(DropReason::NoEndpoint);
    };
    if reg.is_invalid() {
        return Err(DropReason::PolicyInvalid);
    }
    let cooldown = Duration::from_secs(config.device_cooldown_secs);
    // Evict cooldown entries older than the window before consulting the map: a
    // device past its cooldown can never block again, so its entry is dead
    // weight. Pruning on every access keeps `last_wake` bounded on a long-lived
    // relay instead of accreting one entry per device ever woken (Task 6 review).
    state
        .last_wake
        .retain(|_, last| now.saturating_duration_since(*last) < cooldown);
    if let Some(last) = state.last_wake.get(&device_id) {
        if now.saturating_duration_since(*last) < cooldown {
            return Err(DropReason::Cooldown);
        }
    }
    // Budget last: only charge the bridge a token for a wake that clears every
    // earlier gate, so a cooldown/no-endpoint drop never drains the budget.
    if !state.take_bridge_token(bridge_id, config, now) {
        return Err(DropReason::BridgeBudget);
    }
    state.last_wake.insert(device_id, now);
    Ok(reg.endpoint.clone())
}

// ── Delivery half (ADR-0023 SSRF policy + bounded HTTP POST, spec Task 7) ──────

/// Where a resolved address sits relative to the relay's network-target policy.
///
/// [`AddrClass::Blocked`] addresses are refused *unconditionally* — no config
/// flag re-admits them; they include the cloud-metadata callers
/// (`169.254.0.0/16`, `fe80::/10`) an SSRF attacker most wants to reach.
/// [`AddrClass::PrivateOrLoopback`] is refused by default but re-admitted by
/// `allow_private_endpoints` (LAN self-hosters), and is the *only* class
/// cleartext `http://` may ever target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddrClass {
    /// Never a valid target regardless of config: unspecified, multicast,
    /// broadcast, and link-local (incl. IPv4/IPv6 cloud-metadata ranges).
    Blocked,
    /// Loopback, RFC 1918 private, or IPv6 ULA (`fc00::/7`) — admitted only
    /// with `allow_private_endpoints`.
    PrivateOrLoopback,
    /// A routable public address.
    Public,
}

/// Collapses an IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`,
/// deprecated but still parseable), or NAT64 well-known-prefix (`64:ff9b::a.b.c.d`,
/// RFC 6052 `64:ff9b::/96`) IPv6 address to its embedded IPv4, so e.g.
/// `64:ff9b::169.254.169.254` is classified as the link-local metadata address
/// it really reaches on a NAT64 host rather than slipping through the v6 checks
/// as an opaque "public" v6 literal — a classic SSRF bypass. Only the `/96`
/// well-known NAT64 prefix is covered; the RFC 8215 `64:ff9b:1::/48`
/// operator-assigned range is out of scope (no fixed embedding offset to parse
/// generically).
fn normalize_mapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else if let Some(v4) = embedded_v4(v6) {
                IpAddr::V4(v4)
            } else {
                IpAddr::V6(v6)
            }
        }
        v4 => v4,
    }
}

/// Extracts the embedded IPv4 from an IPv4-compatible (`::a.b.c.d`) or NAT64
/// well-known-prefix (`64:ff9b::a.b.c.d`) IPv6 address; `None` for anything
/// else (including `::` and `::1`, which are not compatible-mapped forms and
/// are already classified correctly as v6 unspecified/loopback).
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let low32 =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
        // NAT64 `64:ff9b::/96` (RFC 6052).
        Some(low32(seg[6], seg[7]))
    } else if seg[0..6] == [0, 0, 0, 0, 0, 0]
        && v6 != Ipv6Addr::UNSPECIFIED
        && v6 != Ipv6Addr::LOCALHOST
    {
        // IPv4-compatible `::a.b.c.d` (deprecated form), excluding `::` and
        // `::1` which share the all-zero-top-96-bits shape but are not this.
        Some(low32(seg[6], seg[7]))
    } else {
        None
    }
}

/// `fe80::/10` — IPv6 link-local (`is_unicast_link_local` is unstable std, so
/// the top-10-bit prefix is checked by hand).
fn is_v6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// `fc00::/7` — IPv6 unique-local (ULA); `is_unique_local` is unstable std, so
/// the top-7-bit prefix is checked by hand.
fn is_v6_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// Classifies a (mapping-normalised) address against the target policy.
fn classify(ip: IpAddr) -> AddrClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() || v4.is_link_local() {
                AddrClass::Blocked
            } else if v4.is_loopback() || v4.is_private() {
                AddrClass::PrivateOrLoopback
            } else {
                AddrClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() || is_v6_link_local(v6) {
                AddrClass::Blocked
            } else if v6.is_loopback() || is_v6_unique_local(v6) {
                AddrClass::PrivateOrLoopback
            } else {
                AddrClass::Public
            }
        }
    }
}

/// Whether a single resolved address is a permissible POST target under `cfg`
/// for the given scheme.
fn addr_permitted(ip: IpAddr, scheme_is_http: bool, cfg: &PushConfig) -> bool {
    let class = classify(ip);
    let base_ok = match class {
        AddrClass::Blocked => false,
        AddrClass::PrivateOrLoopback => cfg.allow_private_endpoints,
        AddrClass::Public => true,
    };
    if !base_ok {
        return false;
    }
    if scheme_is_http {
        // Cleartext is off by default, and even when enabled it may only reach
        // private/loopback targets — never the public internet.
        if !cfg.allow_http || class == AddrClass::Public {
            return false;
        }
    }
    true
}

/// Filters resolved addresses to those the policy permits, normalising
/// IPv4-mapped IPv6 first. Pure over its inputs so the whole SSRF matrix is
/// testable without any network I/O. An empty surviving set is
/// [`DropReason::PolicyInvalid`] — every candidate was refused.
pub fn filter_addrs(
    addrs: &[IpAddr],
    scheme_is_http: bool,
    cfg: &PushConfig,
) -> Result<Vec<IpAddr>, DropReason> {
    let allowed: Vec<IpAddr> = addrs
        .iter()
        .copied()
        .map(normalize_mapped)
        .filter(|ip| addr_permitted(*ip, scheme_is_http, cfg))
        .collect();
    if allowed.is_empty() {
        Err(DropReason::PolicyInvalid)
    } else {
        Ok(allowed)
    }
}

/// A parsed, policy-relevant view of an endpoint URL.
struct Target {
    /// The URL host as written (bracketed for an IPv6 literal), used both for
    /// DNS resolution and as the `.resolve()` pin key.
    host: String,
    port: u16,
    scheme_is_http: bool,
}

/// Parses an endpoint into scheme/host/port, rejecting anything but
/// `http`/`https` with a resolvable host and known port. Returns
/// [`DropReason::PolicyInvalid`] on any malformed or unsupported URL.
fn parse_target(endpoint: &str) -> Result<Target, DropReason> {
    let url = reqwest::Url::parse(endpoint).map_err(|_| DropReason::PolicyInvalid)?;
    let scheme_is_http = match url.scheme() {
        "http" => true,
        "https" => false,
        _ => return Err(DropReason::PolicyInvalid),
    };
    let host = url.host_str().ok_or(DropReason::PolicyInvalid)?.to_string();
    let port = url
        .port_or_known_default()
        .ok_or(DropReason::PolicyInvalid)?;
    Ok(Target {
        host,
        port,
        scheme_is_http,
    })
}

/// Parses a URL host as an IP *literal* (so DNS is skipped), handling the
/// bracketed `[::1]` form of an IPv6 literal. `None` for a real domain name.
fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return Some(IpAddr::V4(v4));
    }
    let inner = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    inner.parse::<Ipv6Addr>().ok().map(IpAddr::V6)
}

/// Resolves a target's host to candidate addresses: a literal IP skips DNS
/// (but is still policy-filtered by the caller); a domain goes through
/// [`lookup_host`]. An unresolvable host is [`DropReason::PolicyInvalid`].
async fn resolve_target(target: &Target) -> Result<Vec<IpAddr>, DropReason> {
    if let Some(ip) = parse_ip_literal(&target.host) {
        return Ok(vec![ip]);
    }
    let addrs: Vec<IpAddr> = lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|_| DropReason::PolicyInvalid)?
        .map(|sa| sa.ip())
        .collect();
    if addrs.is_empty() {
        return Err(DropReason::PolicyInvalid);
    }
    Ok(addrs)
}

/// Builds a one-shot reqwest client that is pinned to `checked` for `host`
/// (DNS-rebinding defense: the request connects to the address that *passed*
/// the policy check, never a fresh resolution) with redirects disabled and a
/// hard timeout, then POSTs the fixed wake body. Returns the response status;
/// the body is ignored. Split out so the pin path is unit-testable without DNS.
async fn post_wake_pinned(
    endpoint: &str,
    host: &str,
    checked: SocketAddr,
) -> reqwest::Result<reqwest::StatusCode> {
    let client = reqwest::Client::builder()
        // A redirect is an unchecked second destination (ADR-0023): refuse it.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(WAKE_TIMEOUT)
        // Pin the resolved+checked address for this host. reqwest ignores the
        // port here and uses the URL's, which is what we want.
        .resolve(host, checked)
        .build()?;
    let resp = client.post(endpoint).body(WAKE_BODY).send().await?;
    Ok(resp.status())
}

/// Delivers one wake to a device-supplied endpoint under the full ADR-0023
/// network policy: hold a global in-flight permit (drop, never queue, if none
/// is immediately free), resolve the host ourselves, filter every resolved
/// address against the SSRF policy, then POST to the *checked* address with the
/// connection pinned to it. Never logs the endpoint URL — only outcomes by
/// category (metadata policy).
pub async fn deliver_wake(endpoint: &str, cfg: &PushConfig, permits: Arc<Semaphore>) {
    // Bounded concurrency: take a permit immediately or drop. Queueing here
    // would let a burst of wakes accrete unbounded tasks/sockets/DNS lookups.
    // The permit is held for the whole delivery (resolve + connect + POST).
    let _permit = match permits.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            eprintln!("remora-relay: push wake dropped (in_flight_full)");
            return;
        }
    };

    let target = match parse_target(endpoint) {
        Ok(target) => target,
        Err(reason) => {
            eprintln!("remora-relay: push wake dropped ({})", reason.as_str());
            return;
        }
    };
    let addrs = match resolve_target(&target).await {
        Ok(addrs) => addrs,
        Err(reason) => {
            eprintln!("remora-relay: push wake dropped ({})", reason.as_str());
            return;
        }
    };
    let checked_ip = match filter_addrs(&addrs, target.scheme_is_http, cfg) {
        // First surviving address; pinning it closes the rebinding window.
        Ok(mut allowed) => allowed.remove(0),
        Err(reason) => {
            eprintln!("remora-relay: push wake dropped ({})", reason.as_str());
            return;
        }
    };

    let checked = SocketAddr::new(checked_ip, target.port);
    match post_wake_pinned(endpoint, &target.host, checked).await {
        Ok(status) => {
            eprintln!(
                "remora-relay: push wake delivered (status {})",
                status.as_u16()
            );
        }
        // Transport failure only — the endpoint URL is never logged.
        Err(_) => {
            eprintln!("remora-relay: push wake delivery failed (transport)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did(fill: u8) -> DeviceId {
        DeviceId([fill; 32])
    }

    fn valid_reg() -> StoredRegistration {
        StoredRegistration::from_registration(&PushRegistration::UnifiedPush {
            endpoint: "https://ntfy.sh/topic".to_string(),
        })
    }

    fn invalid_reg() -> StoredRegistration {
        StoredRegistration::from_registration(&PushRegistration::UnifiedPush {
            endpoint: "file:///etc/passwd".to_string(),
        })
    }

    /// Config with push on and generous budget, so a test targets exactly the
    /// gate it means to.
    fn enabled() -> PushConfig {
        PushConfig {
            enabled: true,
            per_bridge_per_minute: 60,
            device_cooldown_secs: 30,
            ..PushConfig::default()
        }
    }

    #[test]
    fn absent_section_defaults_are_disabled() {
        let cfg = PushConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.allow_http);
        assert!(!cfg.allow_private_endpoints);
        assert_eq!(cfg.device_cooldown_secs, 30);
        assert_eq!(cfg.per_bridge_per_minute, 10);
        assert_eq!(cfg.max_in_flight, 32);
    }

    #[test]
    fn success_returns_endpoint() {
        let mut state = PushState::default();
        let reg = valid_reg();
        let out = decide_wake(
            &enabled(),
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            false,
            Instant::now(),
        );
        assert_eq!(out, Ok("https://ntfy.sh/topic".to_string()));
    }

    #[test]
    fn disabled_when_push_off() {
        let mut state = PushState::default();
        let reg = valid_reg();
        let out = decide_wake(
            &PushConfig::default(),
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            false,
            Instant::now(),
        );
        assert_eq!(out, Err(DropReason::Disabled));
    }

    #[test]
    fn dst_connected_needs_no_wake() {
        let mut state = PushState::default();
        let reg = valid_reg();
        let out = decide_wake(
            &enabled(),
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            true, // connected
            Instant::now(),
        );
        assert_eq!(out, Err(DropReason::DstConnected));
    }

    #[test]
    fn no_endpoint_registered() {
        let mut state = PushState::default();
        let out = decide_wake(
            &enabled(),
            &mut state,
            did(0x11),
            did(0x22),
            None,
            false,
            Instant::now(),
        );
        assert_eq!(out, Err(DropReason::NoEndpoint));
    }

    #[test]
    fn policy_invalid_endpoint_drops() {
        let mut state = PushState::default();
        let reg = invalid_reg();
        assert!(reg.invalid_reason().is_some());
        let out = decide_wake(
            &enabled(),
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            false,
            Instant::now(),
        );
        assert_eq!(out, Err(DropReason::PolicyInvalid));
    }

    #[test]
    fn cooldown_blocks_second_wake_then_clears() {
        let mut state = PushState::default();
        let reg = valid_reg();
        let cfg = enabled();
        let t0 = Instant::now();
        // First wake succeeds and stamps the cooldown.
        assert!(decide_wake(
            &cfg,
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            false,
            t0
        )
        .is_ok());
        // Within the 30s window: Cooldown.
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(
            decide_wake(
                &cfg,
                &mut state,
                did(0x11),
                did(0x22),
                Some(&reg),
                false,
                t1
            ),
            Err(DropReason::Cooldown)
        );
        // Past the window: allowed again.
        let t2 = t0 + Duration::from_secs(31);
        assert!(decide_wake(
            &cfg,
            &mut state,
            did(0x11),
            did(0x22),
            Some(&reg),
            false,
            t2
        )
        .is_ok());
    }

    #[test]
    fn bridge_budget_exhausts_then_refills() {
        // Budget of 2 wakes/min from one bridge; distinct devices dodge the
        // per-device cooldown so this isolates the per-bridge bucket.
        let cfg = PushConfig {
            enabled: true,
            per_bridge_per_minute: 2,
            device_cooldown_secs: 30,
            ..PushConfig::default()
        };
        let mut state = PushState::default();
        let reg = valid_reg();
        let bridge = did(0x11);
        let t0 = Instant::now();
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x01), Some(&reg), false, t0).is_ok());
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x02), Some(&reg), false, t0).is_ok());
        // Third at the same instant: bucket empty.
        assert_eq!(
            decide_wake(&cfg, &mut state, bridge, did(0x03), Some(&reg), false, t0),
            Err(DropReason::BridgeBudget)
        );
        // 30s later the 2/min bucket has refilled ~1 token.
        let t1 = t0 + Duration::from_secs(30);
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x04), Some(&reg), false, t1).is_ok());
        assert_eq!(
            decide_wake(&cfg, &mut state, bridge, did(0x05), Some(&reg), false, t1),
            Err(DropReason::BridgeBudget)
        );
    }

    #[test]
    fn cooldown_drop_does_not_charge_the_bridge_budget() {
        // A per-device cooldown drop must not consume a bridge token: a device
        // flapping inside its cooldown cannot drain the whole bridge's budget.
        let cfg = PushConfig {
            enabled: true,
            per_bridge_per_minute: 1,
            device_cooldown_secs: 30,
            ..PushConfig::default()
        };
        let mut state = PushState::default();
        let reg = valid_reg();
        let bridge = did(0x11);
        let t0 = Instant::now();
        // Device A takes the single token.
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x01), Some(&reg), false, t0).is_ok());
        // Device A again inside cooldown: Cooldown, and it must NOT have spent a
        // token (there were none anyway) — verified via a *different* device B
        // below still seeing an empty (not negative) bucket.
        assert_eq!(
            decide_wake(&cfg, &mut state, bridge, did(0x01), Some(&reg), false, t0),
            Err(DropReason::Cooldown)
        );
        // Refill one token over a minute; device B should get exactly one wake.
        let t1 = t0 + Duration::from_secs(60);
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x02), Some(&reg), false, t1).is_ok());
    }

    #[test]
    fn stale_last_wake_entries_are_evicted() {
        // A long-lived relay must not accrete one `last_wake` entry per device
        // ever woken: entries past their cooldown are pruned on the next access.
        let cfg = enabled(); // 30s cooldown, generous budget.
        let mut state = PushState::default();
        let reg = valid_reg();
        let bridge = did(0xAA);
        let t0 = Instant::now();
        for i in 0..5u8 {
            assert!(decide_wake(&cfg, &mut state, bridge, did(i), Some(&reg), false, t0).is_ok());
        }
        assert_eq!(state.last_wake.len(), 5);
        // Long past every entry's cooldown window: one new wake evicts all the
        // now-stale entries, leaving only the fresh one.
        let t1 = t0 + Duration::from_secs(120);
        assert!(decide_wake(&cfg, &mut state, bridge, did(0x99), Some(&reg), false, t1).is_ok());
        assert_eq!(state.last_wake.len(), 1);
    }

    // ── Delivery-half tests (SSRF policy + bounded HTTP POST, Task 7) ─────────

    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().expect("valid v4 literal"))
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().expect("valid v6 literal"))
    }

    /// Every knob-combination of `filter_addrs` over the ADR-0023 matrix.
    #[test]
    fn filter_addrs_policy_matrix() {
        let metadata = v4("169.254.169.254");
        let ll_v6 = v6("fe80::1");
        let public = v4("8.8.8.8");
        let loopback = v4("127.0.0.1");
        let private_a = v4("10.0.0.5");
        let private_b = v4("192.168.1.10");
        let loopback_v6 = v6("::1");
        let ula_v6 = v6("fd00::1");
        let multicast_v6 = v6("ff02::1");

        for &allow_private in &[false, true] {
            for &scheme_is_http in &[false, true] {
                let cfg = PushConfig {
                    enabled: true,
                    allow_http: true, // http gated by target class, not this alone
                    allow_private_endpoints: allow_private,
                    ..PushConfig::default()
                };
                let ok = |ip: IpAddr| filter_addrs(&[ip], scheme_is_http, &cfg).is_ok();

                // Metadata / link-local: NEVER allowed, any knob, any scheme.
                assert!(!ok(metadata), "metadata must never pass");
                assert!(!ok(ll_v6), "v6 link-local must never pass");
                assert!(!ok(multicast_v6), "multicast must never pass");

                // Public: https always allowed; http-to-public never allowed.
                assert_eq!(ok(public), !scheme_is_http, "public https ok, http no");

                // Private / loopback: only with the knob, and (for http) only
                // because http-to-private is the one cleartext case allowed.
                for &p in &[loopback, private_a, private_b, loopback_v6, ula_v6] {
                    assert_eq!(ok(p), allow_private, "private only with the knob");
                }
            }
        }
    }

    /// http to a public target is refused even with `allow_http = true`.
    #[test]
    fn http_to_public_denied_even_with_allow_http() {
        let cfg = PushConfig {
            enabled: true,
            allow_http: true,
            allow_private_endpoints: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[v4("8.8.8.8")], true, &cfg),
            Err(DropReason::PolicyInvalid)
        );
    }

    /// An IPv4-mapped IPv6 address is classified as its embedded v4 (bypass
    /// defense): `::ffff:10.0.0.1` must be treated as private `10.0.0.1`.
    #[test]
    fn v4_mapped_v6_classified_as_embedded_v4() {
        let mapped = v6("::ffff:10.0.0.1");
        // Default (no private): refused.
        let deny = PushConfig {
            enabled: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[mapped], false, &deny),
            Err(DropReason::PolicyInvalid)
        );
        // With the private knob: admitted as the private v4 it really reaches.
        let allow = PushConfig {
            enabled: true,
            allow_private_endpoints: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[mapped], false, &allow),
            Ok(vec![v4("10.0.0.1")])
        );
    }

    /// An IPv4-compatible IPv6 address (`::a.b.c.d`, deprecated but still
    /// parseable) is classified as its embedded v4 — otherwise
    /// `::169.254.169.254` would slip through as an opaque "public" v6 literal
    /// and reach the cloud-metadata address on a host that routes it.
    #[test]
    fn v4_compatible_v6_classified_as_embedded_v4() {
        let compatible = v6("::169.254.169.254");
        let cfg = PushConfig {
            enabled: true,
            allow_private_endpoints: true,
            ..PushConfig::default()
        };
        // Blocked (link-local/metadata) regardless of the private-endpoints knob.
        assert_eq!(
            filter_addrs(&[compatible], false, &cfg),
            Err(DropReason::PolicyInvalid)
        );
    }

    /// A NAT64 well-known-prefix address (`64:ff9b::a.b.c.d`, RFC 6052
    /// `64:ff9b::/96`) is classified as its embedded v4: a private target is
    /// denied by default and admitted only with `allow_private_endpoints`, a
    /// public target is reachable over https.
    #[test]
    fn nat64_v6_classified_as_embedded_v4() {
        let private_nat64 = v6("64:ff9b::10.0.0.5");
        let public_nat64 = v6("64:ff9b::8.8.8.8");

        let deny = PushConfig {
            enabled: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[private_nat64], false, &deny),
            Err(DropReason::PolicyInvalid),
            "NAT64-embedded private v4 denied by default"
        );

        let allow = PushConfig {
            enabled: true,
            allow_private_endpoints: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[private_nat64], false, &allow),
            Ok(vec![v4("10.0.0.5")]),
            "NAT64-embedded private v4 admitted with the knob"
        );

        assert_eq!(
            filter_addrs(&[public_nat64], false, &deny),
            Ok(vec![v4("8.8.8.8")]),
            "NAT64-embedded public v4 reachable over https"
        );
    }

    /// Regression guard: the pre-existing v4-mapped (`::ffff:a.b.c.d`) handling
    /// must still catch a metadata target after the v4-compatible/NAT64
    /// extension lands alongside it.
    #[test]
    fn v4_mapped_metadata_still_blocked() {
        let mapped = v6("::ffff:169.254.169.254");
        let cfg = PushConfig {
            enabled: true,
            allow_private_endpoints: true,
            ..PushConfig::default()
        };
        assert_eq!(
            filter_addrs(&[mapped], false, &cfg),
            Err(DropReason::PolicyInvalid)
        );
    }

    /// Reads one HTTP/1.1 request off `sock` (method + body), then replies with
    /// the given raw response. Minimal, test-only parse — no framework dep.
    async fn serve_one(sock: &mut TcpStream, response: &[u8]) -> (String, String) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 512];
        loop {
            let n = sock.read(&mut tmp).await.expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let mut body = buf[pos + 4..].to_vec();
                while body.len() < content_length {
                    let n = sock.read(&mut tmp).await.expect("read request");
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                let method = headers
                    .lines()
                    .next()
                    .and_then(|l| l.split(' ').next())
                    .unwrap_or("")
                    .to_string();
                let _ = sock.write_all(response).await;
                return (method, String::from_utf8_lossy(&body).to_string());
            }
        }
        (String::new(), String::new())
    }

    fn allow_all_cfg() -> PushConfig {
        PushConfig {
            enabled: true,
            allow_http: true,
            allow_private_endpoints: true,
            max_in_flight: 4,
            ..PushConfig::default()
        }
    }

    /// POSTs the fixed body, and a `301` response is never followed.
    #[tokio::test]
    async fn deliver_wake_posts_fixed_body() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let count = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let count_srv = count.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.expect("accept");
                count_srv.fetch_add(1, SeqCst);
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (method, body) = serve_one(
                        &mut sock,
                        b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:9/\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
                    let _ = tx.send((method, body));
                });
            }
        });

        let endpoint = format!("http://127.0.0.1:{}/", addr.port());
        deliver_wake(&endpoint, &allow_all_cfg(), Arc::new(Semaphore::new(4))).await;

        let (method, body) = rx.recv().await.expect("request received");
        assert_eq!(method, "POST");
        assert_eq!(body, WAKE_BODY);
        // A redirect must not spawn a second request.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(count.load(SeqCst), 1, "301 must not be followed");
    }

    /// The global semaphore caps simultaneously-connected deliveries: 8 wakes,
    /// 4 permits, a delaying listener — at most 4 sockets are ever live at once.
    #[tokio::test]
    async fn semaphore_bounds_inflight() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let live = Arc::new(AtomicUsize::new(0));
        let max_live = Arc::new(AtomicUsize::new(0));
        let (live_s, max_s) = (live.clone(), max_live.clone());
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.expect("accept");
                let (live_s, max_s) = (live_s.clone(), max_s.clone());
                tokio::spawn(async move {
                    let n = live_s.fetch_add(1, SeqCst) + 1;
                    max_s.fetch_max(n, SeqCst);
                    // Hold the connection open (delay the response) so overlap is
                    // observable.
                    let mut tmp = [0u8; 512];
                    let _ = sock.read(&mut tmp).await;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    live_s.fetch_sub(1, SeqCst);
                });
            }
        });

        let permits = Arc::new(Semaphore::new(4));
        let endpoint = format!("http://127.0.0.1:{}/", addr.port());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (p, e, cfg) = (permits.clone(), endpoint.clone(), allow_all_cfg());
            handles.push(tokio::spawn(async move { deliver_wake(&e, &cfg, p).await }));
        }
        for h in handles {
            let _ = h.await;
        }
        assert!(
            max_live.load(SeqCst) <= 4,
            "at most 4 concurrent sockets, saw {}",
            max_live.load(SeqCst)
        );
    }

    /// The `.resolve()` pin makes delivery reach the checked address for a host
    /// that has no real DNS record — proving the request connects to the address
    /// that passed the check, not a re-resolution.
    #[tokio::test]
    async fn rebinding_pin_reaches_checked_addr() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("local addr");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let (method, body) =
                serve_one(&mut sock, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            let _ = tx.send((method, body));
        });

        // A host with no DNS record; only the pin can route it to the listener.
        let endpoint = format!("http://localtest.example:{}/", addr.port());
        let status = post_wake_pinned(&endpoint, "localtest.example", addr)
            .await
            .expect("pinned delivery succeeds");
        assert!(status.is_success());
        let (method, body) = rx.recv().await.expect("request received");
        assert_eq!(method, "POST");
        assert_eq!(body, WAKE_BODY);
    }

    /// A literal-IP endpoint is parsed and skips DNS; a bad scheme is refused.
    #[test]
    fn parse_target_scheme_and_literal() {
        let t = parse_target("https://1.2.3.4:8443/topic").expect("parse literal target");
        assert_eq!(t.host, "1.2.3.4");
        assert_eq!(t.port, 8443);
        assert!(!t.scheme_is_http);
        assert_eq!(parse_ip_literal("1.2.3.4"), Some(v4("1.2.3.4")));
        assert_eq!(parse_ip_literal("[::1]"), Some(v6("::1")));
        assert_eq!(parse_ip_literal("ntfy.sh"), None);
        assert_eq!(
            parse_target("ftp://1.2.3.4/").err(),
            Some(DropReason::PolicyInvalid)
        );
    }
}
