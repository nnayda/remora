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
use std::time::{Duration, Instant};

use remora_protocol::{validate_push_endpoint, DeviceId, PushRegistration};
use serde::Deserialize;

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
}
