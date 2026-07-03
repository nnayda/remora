//! Sans-IO relay router: connection registry, hello authentication,
//! adjacency-scoped routing, and per-connection byte-budgeted outbound
//! queues (ADR-0021, spec D5/D9/D16).
//!
//! This is the decision core the WebSocket server (Task 8) drives: it owns no
//! sockets and performs no I/O. The WS layer decodes a frame, hands the router
//! the already-parsed header fields, and acts on the returned [`HelloOutcome`]
//! / [`RouteOutcome`] (registering a connection, forwarding bytes, or closing
//! a peer with a specific WebSocket close code). The router never inspects a
//! `Data` payload — blindness is structural: [`Router::route`] only ever sees
//! the header fields the caller decoded plus the opaque raw frame bytes it
//! forwards verbatim.
//!
//! ## Byte budget and the drain contract
//!
//! Each connection has an outbound queue: an unbounded [`mpsc`] channel paired
//! with a shared [`AtomicUsize`] byte counter and a fixed budget from config.
//! [`Router::route`] reserves `raw.len()` bytes on the *destination's* counter
//! before enqueueing; if that would exceed the budget the frame is dropped and
//! the destination's [`OutboundHandle`] is handed back so the caller closes the
//! slow consumer (WebSocket 4008). This is spec D9's dst-kill policy: a slow
//! reader dies, never the shared sender.
//!
//! The counter is decremented by the WS writer task as it drains, via the
//! [`OutboundFrame`] guard returned by [`OutboundReceiver::recv`]: the
//! reservation is held from enqueue until the frame guard is dropped (after the
//! write completes), so the counter can never drift — every increment in
//! `try_enqueue` is balanced by exactly one decrement in `OutboundFrame::drop`,
//! and a reservation that fails to enqueue is refunded before `try_enqueue`
//! returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use remora_protocol::{DeviceId, HelloRole, RelayControl, RelayHello};
use tokio::sync::{mpsc, Semaphore};

use crate::config::{token_matches, RelayConfig};
use crate::push::{decide_wake, DropReason, PushConfig, PushState, StoredRegistration};

/// Sans-IO connection registry and routing authority. Cheap to clone-share via
/// the returned [`Arc`]; all mutable state lives behind a single [`Mutex`] that
/// is never held across an `.await` (every method here is synchronous).
pub struct Router {
    config: Arc<RelayConfig>,
    state: Mutex<RouterState>,
    /// Global cap on simultaneously in-flight push deliveries (ADR-0023). Shared
    /// across every connection so a burst of wakes from many bridges cannot
    /// collectively exhaust sockets/DNS/memory; handed to each spawned
    /// [`crate::push::deliver_wake`], which drops rather than queues when full.
    push_permits: Arc<Semaphore>,
}

/// Mutable registry, guarded by [`Router::state`].
struct RouterState {
    /// Monotonic connection serial, stamped into each [`ConnPermit`] so a
    /// connection that was replaced (4009) cannot deregister its replacement.
    next_serial: u64,
    /// Live connections keyed by routing id. For a bridge the routing id is
    /// its own device id; for a device it is its fresh per-connection routing
    /// id. Newest registration at a key wins.
    conns: HashMap<DeviceId, Registration>,
    /// Per-bridge soft state (ADR-0021 D4): each bridge's asserted device
    /// credentials and its single active pairing window. Keyed by bridge id;
    /// created/refreshed on bridge hello and dropped when the bridge's
    /// connection closes.
    bridges: HashMap<DeviceId, BridgeState>,
    /// Push-wake budgeting state (ADR-0023): per-device cooldowns and
    /// per-bridge token buckets. Deliberately owned here, **outside**
    /// [`BridgeState`], so a bridge's `AssertDevices` (which replaces its
    /// `asserted`/`push` maps) never resets a device's cooldown — a bridge
    /// cannot clear a phone's rate limit by re-asserting.
    push_state: PushState,
}

/// Per-bridge soft state (ADR-0021 D4): the bridge's asserted device
/// credentials and its single active pairing window. `serial` is the
/// connection serial that owns this state, so an assert arriving on a stale
/// (already-displaced) bridge permit is rejected rather than resurrecting a
/// credential the current bridge connection never asserted.
#[derive(Default)]
struct BridgeState {
    /// The connection serial that owns this soft state.
    serial: u64,
    /// device_id -> asserted token (constant-time compared on device hello).
    asserted: HashMap<DeviceId, String>,
    /// device_id -> asserted push registration (ADR-0023), for the subset of
    /// asserted devices that carry one. Replaced wholesale on each
    /// `AssertDevices`, alongside `asserted`.
    push: HashMap<DeviceId, StoredRegistration>,
    /// The active pairing window's rendezvous token + absolute expiry, if any.
    window: Option<PairingWindow>,
}

/// A bridge's single active pairing window: a rendezvous token that admits the
/// pairing device to routing until `expires_at` (absolute unix seconds).
struct PairingWindow {
    token: String,
    expires_at: u64,
}

/// One live connection's registration.
struct Registration {
    role: HelloRole,
    /// For a device: the bridge it routes through. For a bridge: its own id.
    bridge_id: DeviceId,
    /// The connection's authenticated device id (`hello.device_id`). For a
    /// bridge this equals its `bridge_id`; for a device it is the identity the
    /// bridge asserted, used to map a de-asserted device back to its live
    /// routing id(s) for an `AssertDevices` kick.
    device_id: DeviceId,
    /// The connection serial that owns this registration.
    serial: u64,
    outbound: OutboundHandle,
}

/// Proof that a connection completed a valid [`Router::hello`]: its registered
/// identity plus the serial that owns its registry slot. The WS layer holds
/// this for the connection's lifetime and passes it to [`Router::route`] and
/// [`Router::disconnect`]. Only the router mints these.
#[derive(Debug, Clone)]
pub struct ConnPermit {
    role: HelloRole,
    /// This connection's routing id (== registry key).
    routing_id: DeviceId,
    /// For a device: its declared bridge. For a bridge: its own id.
    bridge_id: DeviceId,
    /// Serial identifying this specific connection instance.
    serial: u64,
}

impl ConnPermit {
    /// This connection's routing id.
    pub fn routing_id(&self) -> DeviceId {
        self.routing_id
    }

    /// Whether this connection authenticated as a bridge or a device.
    pub fn role(&self) -> HelloRole {
        self.role
    }
}

/// Result of a [`Router::hello`] authentication attempt.
pub enum HelloOutcome {
    /// Hello accepted; the connection is registered. Drive routing with the
    /// returned [`ConnPermit`].
    Accepted(ConnPermit),
    /// Hello rejected (unknown identity, bad token, or an illegal routing id).
    /// The caller closes the connection with WebSocket close code 4001.
    Rejected,
}

/// Result of [`Router::handle_control`] — a bridge's relay-terminated
/// [`RelayControl`] message (ADR-0021 D4).
#[derive(Debug)]
pub enum ControlOutcome {
    /// The control message was applied; the server replies `RelayControlAck`.
    Ack,
    /// The control message was rejected (e.g. it arrived on a stale bridge
    /// connection); the server replies `RelayControlError`.
    Error(String),
    /// A device (not a bridge) sent a Control frame; the server closes it 4002.
    NotBridge,
    /// An `AssertDevices` was applied. The server replies `RelayControlAck`, and:
    /// - kicks each **routing id** in `kicked` via its kill channel (4001):
    ///   these are de-asserted devices that still had live connections;
    /// - logs + audits each entry in `invalid_push`: a `(device_id, reason)`
    ///   for an asserted push endpoint that failed syntax validation. Such an
    ///   endpoint is stored-but-flagged (dropped at delivery time as
    ///   [`DropReason::PolicyInvalid`]); the assert still ACKs, so one bad
    ///   endpoint never kicks the bridge's other, correct devices (ADR-0023).
    Asserted {
        kicked: Vec<DeviceId>,
        invalid_push: Vec<(DeviceId, String)>,
    },
}

/// Result of [`Router::decide_push_wake`] — the relay's reaction to a
/// well-formed `PushTrigger` from a bridge (ADR-0023, spec Task 6).
#[derive(Debug)]
pub enum PushDecision {
    /// The `dst` device is not in the sending bridge's asserted set (or the
    /// permit is stale / not a bridge). This is an accept-rule violation: the
    /// server closes the **sender** with `CloseReason::Protocol`.
    NotAsserted,
    /// A wake should be delivered to this endpoint URL. The server hands it to
    /// the delivery seam (Task 7) and the sender continues. `device_id` and
    /// `stamped` (the instant this wake charged the cooldown) let the delivery
    /// task compare-and-clear the stamp if delivery ultimately fails, so a
    /// missed wake is not suppressed by its own cooldown (#233 F2).
    Deliver {
        endpoint: String,
        device_id: DeviceId,
        stamped: std::time::Instant,
    },
    /// The wake was dropped for this policy reason — never a protocol
    /// violation; the sender continues. Counted/logged for observability.
    Drop(DropReason),
}

/// Result of a [`Router::route`] decision.
pub enum RouteOutcome {
    /// The frame was enqueued on the destination's outbound queue.
    Delivered,
    /// The destination is not currently registered. The caller reacts by the
    /// sender's role (a routing/availability policy, not a payload decision): a
    /// **device** sender is closed with WebSocket close code 4004 (its only
    /// bridge is gone), while a **bridge** sender drops the undeliverable frame
    /// and continues (a departed device is routine under spec D3 — see the WS
    /// layer's `peer_unavailable_step`).
    PeerUnavailable,
    /// Adjacency violation: the sender may not address this destination (or the
    /// envelope `src` did not match the sender's routing id). The caller closes
    /// the **sender** with WebSocket close code 4002.
    NotAllowed,
    /// The destination's outbound queue is over budget. The frame was dropped
    /// and the destination's handle is returned so the caller closes the
    /// **destination** with WebSocket close code 4008 (dst-kill, spec D9).
    Overflow,
}

impl Router {
    /// Builds a router bound to `config`.
    pub fn new(config: Arc<RelayConfig>) -> Arc<Router> {
        let push_permits = Arc::new(Semaphore::new(config.push.max_in_flight));
        Arc::new(Router {
            config,
            state: Mutex::new(RouterState {
                next_serial: 0,
                conns: HashMap::new(),
                bridges: HashMap::new(),
                push_state: PushState::default(),
            }),
            push_permits,
        })
    }

    /// The push-wake network policy this relay was configured with (ADR-0023),
    /// handed to a spawned [`crate::push::deliver_wake`] on a cleared decision.
    pub fn push_config(&self) -> PushConfig {
        self.config.push.clone()
    }

    /// A handle to the shared global in-flight semaphore for push deliveries.
    pub fn push_permits(&self) -> Arc<Semaphore> {
        self.push_permits.clone()
    }

    /// Authenticates a [`RelayHello`] and, on success, registers the
    /// connection's `outbound` handle under its routing key.
    ///
    /// Returns the [`HelloOutcome`] plus, when a previous connection held the
    /// same routing key, that old holder's [`OutboundHandle`] so the caller can
    /// close it (4009, newest-wins). The returned handle is `None` unless a
    /// displacement happened.
    pub fn hello(
        &self,
        hello: &RelayHello,
        outbound: OutboundHandle,
    ) -> (HelloOutcome, Option<OutboundHandle>) {
        self.hello_at(hello, outbound, now_secs())
    }

    /// [`Router::hello`] with an injected wall-clock `now` (unix seconds), used
    /// to evaluate pairing-window expiry deterministically in tests. Public
    /// `hello` calls this with [`now_secs`].
    pub fn hello_at(
        &self,
        hello: &RelayHello,
        outbound: OutboundHandle,
        now: u64,
    ) -> (HelloOutcome, Option<OutboundHandle>) {
        let mut state = self.lock();
        let Some((routing_id, bridge_id)) = self.authenticate_at(&state, hello, now) else {
            return (HelloOutcome::Rejected, None);
        };

        let serial = state.next_serial;
        state.next_serial += 1;
        // A bridge starts each connection with fresh soft state owned by this
        // serial: any credentials/window from a prior (now-displaced) connection
        // are cleared, and a stale-serial assert on the old permit is rejected.
        if hello.role == HelloRole::Bridge {
            state.bridges.insert(
                routing_id,
                BridgeState {
                    serial,
                    ..BridgeState::default()
                },
            );
        }
        let displaced = state.conns.insert(
            routing_id,
            Registration {
                role: hello.role,
                bridge_id,
                device_id: hello.device_id,
                serial,
                outbound,
            },
        );
        let permit = ConnPermit {
            role: hello.role,
            routing_id,
            bridge_id,
            serial,
        };
        (
            HelloOutcome::Accepted(permit),
            displaced.map(|reg| reg.outbound),
        )
    }

    /// Validates a hello's token and routing-id scoping per spec D5/D16.
    ///
    /// A **bridge** is authenticated against the static config token bound to
    /// its device id. A **device** is authenticated against its bridge's live
    /// soft state (ADR-0021 D4): the claimed `bridge_id` must have an active
    /// connection whose asserted set holds a matching `(device_id, token)`, or
    /// an unexpired pairing window whose rendezvous token matches. `now` is
    /// wall-clock seconds, threaded in so window expiry is testable.
    ///
    /// Returns the `(routing_id, bridge_id)` the connection registers under, or
    /// `None` if the hello must be rejected.
    fn authenticate_at(
        &self,
        state: &RouterState,
        hello: &RelayHello,
        now: u64,
    ) -> Option<(DeviceId, DeviceId)> {
        match hello.role {
            HelloRole::Bridge => {
                // A bridge routes under its own device id; token must match the
                // bridge entry bound to that device id.
                if hello.routing_id != hello.device_id || hello.bridge_id != hello.device_id {
                    return None;
                }
                let entry = self
                    .config
                    .bridges
                    .iter()
                    .find(|b| b.device_id == hello.device_id)?;
                if !token_matches(&hello.token, &entry.token) {
                    return None;
                }
                Some((hello.device_id, hello.device_id))
            }
            HelloRole::Device => {
                // A device joins its bridge's group under a fresh routing id
                // that must be non-zero and must not shadow any bridge id.
                if hello.routing_id.is_zero() {
                    return None;
                }
                if self
                    .config
                    .bridges
                    .iter()
                    .any(|b| b.device_id == hello.routing_id)
                {
                    return None;
                }
                // Bridge-asserted soft state (ADR-0021 D4): admit iff the
                // claimed bridge is connected and either asserts this device's
                // credential or has an unexpired pairing window matching the
                // presented token.
                let bridge = state.bridges.get(&hello.bridge_id)?;
                let asserted_ok = bridge
                    .asserted
                    .get(&hello.device_id)
                    .is_some_and(|expected| token_matches(&hello.token, expected));
                let window_ok = bridge
                    .window
                    .as_ref()
                    .is_some_and(|w| w.expires_at > now && token_matches(&hello.token, &w.token));
                if asserted_ok || window_ok {
                    Some((hello.routing_id, hello.bridge_id))
                } else {
                    None
                }
            }
        }
    }

    /// Applies a bridge's relay-terminated [`RelayControl`] message (ADR-0021
    /// D4) against its per-bridge soft state. Bridge-only; a device sender
    /// yields [`ControlOutcome::NotBridge`].
    pub fn handle_control(&self, permit: &ConnPermit, control: RelayControl) -> ControlOutcome {
        self.handle_control_at(permit, control, now_secs())
    }

    /// [`Router::handle_control`] with an injected wall-clock `now` (unix
    /// seconds) used to stamp a pairing window's absolute expiry. Public
    /// `handle_control` calls this with [`now_secs`].
    pub fn handle_control_at(
        &self,
        permit: &ConnPermit,
        control: RelayControl,
        now: u64,
    ) -> ControlOutcome {
        if permit.role != HelloRole::Bridge {
            return ControlOutcome::NotBridge;
        }
        let mut state = self.lock();
        let bridge_id = permit.routing_id;
        // The soft state must exist and be owned by *this* bridge connection.
        // A stale (already-displaced) permit carries an old serial and must not
        // mutate the current connection's credentials.
        match state.bridges.get(&bridge_id) {
            Some(bs) if bs.serial == permit.serial => {}
            _ => return ControlOutcome::Error("stale connection".to_string()),
        }

        match control {
            RelayControl::RegisterPairing {
                token, ttl_secs, ..
            } => {
                if let Some(bs) = state.bridges.get_mut(&bridge_id) {
                    bs.window = Some(PairingWindow {
                        token,
                        expires_at: now.saturating_add(ttl_secs),
                    });
                }
                ControlOutcome::Ack
            }
            RelayControl::CancelPairing { .. } => {
                if let Some(bs) = state.bridges.get_mut(&bridge_id) {
                    bs.window = None;
                }
                ControlOutcome::Ack
            }
            RelayControl::AssertDevices { devices, .. } => {
                // Build the replacement routing-credential and push-registration
                // maps in lockstep, caching each endpoint's syntax verdict
                // (ADR-0023). A flagged endpoint is stored, not rejected, and
                // surfaced in `invalid_push` for the server to log + audit — the
                // assert still ACKs so one bad endpoint never kicks this bridge's
                // other, correctly-configured devices.
                let mut new_asserted: HashMap<DeviceId, String> = HashMap::new();
                let mut new_push: HashMap<DeviceId, StoredRegistration> = HashMap::new();
                let mut invalid_push: Vec<(DeviceId, String)> = Vec::new();
                for d in devices {
                    if let Some(reg) = &d.push {
                        let stored = StoredRegistration::from_registration(reg);
                        if let Some(reason) = stored.invalid_reason() {
                            invalid_push.push((d.device_id, reason.to_string()));
                        }
                        new_push.insert(d.device_id, stored);
                    }
                    new_asserted.insert(d.device_id, d.token);
                }
                // Devices in the old set but not the new one are de-asserted.
                let removed: std::collections::HashSet<DeviceId> = {
                    let bs = match state.bridges.get_mut(&bridge_id) {
                        Some(bs) => bs,
                        // Unreachable: presence + serial checked above under the
                        // same lock, but stay total rather than panic.
                        None => return ControlOutcome::Error("stale connection".to_string()),
                    };
                    let removed = bs
                        .asserted
                        .keys()
                        .filter(|id| !new_asserted.contains_key(id))
                        .copied()
                        .collect();
                    bs.asserted = new_asserted;
                    bs.push = new_push;
                    removed
                };
                // Map each de-asserted device_id back to its live routing id(s)
                // in this bridge's group, drop those registrations so they can
                // no longer route, and hand the routing ids to the server to
                // kick (4001). Note `push_state` (cooldowns/budgets) is *not*
                // touched here — it survives re-asserts by design.
                let kicked: Vec<DeviceId> = state
                    .conns
                    .iter()
                    .filter(|(_, reg)| {
                        reg.role == HelloRole::Device
                            && reg.bridge_id == bridge_id
                            && removed.contains(&reg.device_id)
                    })
                    .map(|(routing_id, _)| *routing_id)
                    .collect();
                for routing_id in &kicked {
                    state.conns.remove(routing_id);
                }
                ControlOutcome::Asserted {
                    kicked,
                    invalid_push,
                }
            }
            // `RelayControl` is `#[non_exhaustive]`: a control this relay build
            // does not understand is rejected rather than silently accepted, so
            // a newer bridge cannot assume an effect the relay never applied.
            _ => ControlOutcome::Error("unsupported control".to_string()),
        }
    }

    /// Decides whether a well-formed `PushTrigger` from `permit`'s bridge,
    /// targeting device `dst`, should deliver a wake (ADR-0023, spec Task 6).
    ///
    /// Enforces the last accept rule the server cannot check alone — `dst` must
    /// be in **this** bridge's asserted set — then runs the pure
    /// [`decide_wake`] policy against the relay's live state (push config,
    /// whether `dst` is currently connected, and the surviving cooldown/budget
    /// [`PushState`]). A non-bridge or stale permit, or an unasserted `dst`,
    /// yields [`PushDecision::NotAsserted`] (an accept-rule violation the server
    /// maps to a protocol close); every other result is a routine deliver/drop.
    pub fn decide_push_wake(&self, permit: &ConnPermit, dst: DeviceId) -> PushDecision {
        self.decide_push_wake_at(permit, dst, std::time::Instant::now())
    }

    /// [`Router::decide_push_wake`] with an injected `now`, so the cooldown and
    /// per-bridge budget are deterministic in tests. Public `decide_push_wake`
    /// calls this with [`std::time::Instant::now`].
    pub fn decide_push_wake_at(
        &self,
        permit: &ConnPermit,
        dst: DeviceId,
        now: std::time::Instant,
    ) -> PushDecision {
        // Push triggers are bridge→relay only; a device sender is an accept-rule
        // violation. (The server already gates on role, but stay total.)
        if permit.role != HelloRole::Bridge {
            return PushDecision::NotAsserted;
        }
        let mut state = self.lock();
        let bridge_id = permit.routing_id;

        // Gather everything the decision needs from the registry, then release
        // the immutable borrows before mutating `push_state`.
        let (registration, dst_asserted) = match state.bridges.get(&bridge_id) {
            // The soft state must be owned by *this* bridge connection; a stale
            // permit must not drive a wake against the current connection's set.
            Some(bs) if bs.serial == permit.serial => {
                (bs.push.get(&dst).cloned(), bs.asserted.contains_key(&dst))
            }
            _ => return PushDecision::NotAsserted,
        };
        if !dst_asserted {
            return PushDecision::NotAsserted;
        }
        // The target "currently has a live relay connection" (DstConnected) iff a
        // device registration for `dst` exists in this bridge's group.
        let dst_connected = state.conns.values().any(|reg| {
            reg.role == HelloRole::Device && reg.bridge_id == bridge_id && reg.device_id == dst
        });

        match decide_wake(
            &self.config.push,
            &mut state.push_state,
            bridge_id,
            dst,
            registration.as_ref(),
            dst_connected,
            now,
        ) {
            // `now` is exactly the instant `decide_wake` stamped into the
            // cooldown, so it is the compare key a later revoke uses.
            Ok(endpoint) => PushDecision::Deliver {
                endpoint,
                device_id: dst,
                stamped: now,
            },
            Err(reason) => PushDecision::Drop(reason),
        }
    }

    /// Compare-and-clears the cooldown stamp for `device` if it still equals
    /// `stamped` (#233 F2). Called by a delivery task when the wake ultimately
    /// failed to reach the phone, so the missed wake is not suppressed for the
    /// full cooldown; a newer legitimate wake's stamp differs and is untouched.
    /// Takes the router lock briefly and never holds it across an `.await`.
    pub fn revoke_wake_stamp(&self, device: DeviceId, stamped: std::time::Instant) {
        let mut state = self.lock();
        state.push_state.revoke_wake(device, stamped);
    }

    /// Routes one decoded `Data` envelope from a registered connection.
    ///
    /// Enforces `envelope_src == permit.routing_id`, then adjacency (a device
    /// may address only its declared bridge; a bridge may address only routing
    /// ids currently registered in its own group), then byte-budgeted enqueue
    /// of the verbatim `raw` frame bytes. Returns the [`RouteOutcome`] plus, on
    /// [`RouteOutcome::Overflow`], the destination's handle to close (4008).
    pub fn route(
        &self,
        permit: &ConnPermit,
        envelope_src: DeviceId,
        envelope_dst: DeviceId,
        raw: Vec<u8>,
    ) -> (RouteOutcome, Option<OutboundHandle>) {
        // Anti-spoof: the envelope's src must be the sender's own routing id.
        if envelope_src != permit.routing_id {
            return (RouteOutcome::NotAllowed, None);
        }

        let state = self.lock();
        let dst = match permit.role {
            // A device may address only its declared bridge.
            HelloRole::Device => {
                if envelope_dst != permit.bridge_id {
                    return (RouteOutcome::NotAllowed, None);
                }
                match state.conns.get(&envelope_dst) {
                    Some(reg) => reg,
                    None => return (RouteOutcome::PeerUnavailable, None),
                }
            }
            // A bridge may address only devices currently in its own group.
            HelloRole::Bridge => match state.conns.get(&envelope_dst) {
                None => return (RouteOutcome::PeerUnavailable, None),
                Some(reg) => {
                    let in_group =
                        reg.role == HelloRole::Device && reg.bridge_id == permit.routing_id;
                    if !in_group {
                        return (RouteOutcome::NotAllowed, None);
                    }
                    reg
                }
            },
        };

        match dst.outbound.try_enqueue(raw) {
            Ok(()) => (RouteOutcome::Delivered, None),
            Err(TryEnqueueError::OverBudget) => {
                (RouteOutcome::Overflow, Some(dst.outbound.clone()))
            }
            // Receiver gone: the destination connection is already tearing down.
            // Treat as unavailable; nothing to close that isn't already closing.
            Err(TryEnqueueError::Closed) => (RouteOutcome::PeerUnavailable, None),
        }
    }

    /// Enqueues an already-encoded frame onto `routing_id`'s outbound queue, if
    /// that connection is still registered. Used by the server to hand a bridge
    /// its relay-terminated `RelayControlAck`/`RelayControlError` reply (ADR-0021
    /// D4) on the bridge's own outbound, without inspecting any payload. A frame
    /// that would exceed the destination's byte budget is dropped (the reply is
    /// tiny relative to the budget, and a control reply is not worth killing the
    /// bridge over); a departed connection is a silent no-op.
    pub fn enqueue_to(&self, routing_id: &DeviceId, raw: Vec<u8>) {
        let state = self.lock();
        if let Some(reg) = state.conns.get(routing_id) {
            let _ = reg.outbound.try_enqueue(raw);
        }
    }

    /// Deregisters a connection on close — but only if the current registration
    /// at its routing key is still *this* connection's (serial match). A
    /// connection that was displaced by a newer one (4009) carries a stale
    /// serial, so its late disconnect is a no-op and cannot evict the
    /// replacement.
    pub fn disconnect(&self, permit: &ConnPermit) {
        let mut state = self.lock();
        if let Some(reg) = state.conns.get(&permit.routing_id) {
            if reg.serial == permit.serial {
                state.conns.remove(&permit.routing_id);
            }
        }
        // A closing bridge takes its soft state (asserted credentials + pairing
        // window) with it, so its devices are no longer authorized to reconnect
        // until a fresh bridge connection re-asserts them. Serial-guarded so a
        // displaced bridge's late disconnect cannot evict its replacement's
        // soft state.
        if permit.role == HelloRole::Bridge {
            if let Some(bs) = state.bridges.get(&permit.routing_id) {
                if bs.serial == permit.serial {
                    state.bridges.remove(&permit.routing_id);
                }
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RouterState> {
        // The mutex only guards in-memory registry mutation and is never held
        // across an await, so poisoning can only follow a panic while mutating
        // it; recover the guard rather than propagate the poison.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Cloneable producer half of a connection's byte-budgeted outbound queue.
///
/// Every clone shares the same channel, byte counter, and budget. [`Router`]
/// stores one per registered connection and enqueues onto the *destination's*
/// handle in [`Router::route`].
#[derive(Clone)]
pub struct OutboundHandle {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    bytes: Arc<AtomicUsize>,
    budget: usize,
}

/// Why [`OutboundHandle::try_enqueue`] refused a frame.
enum TryEnqueueError {
    /// Enqueueing would exceed the connection's byte budget.
    OverBudget,
    /// The receiver half was dropped (connection gone).
    Closed,
}

impl OutboundHandle {
    /// Attempts to enqueue `raw`, reserving `raw.len()` bytes against the
    /// budget. Fails without enqueueing (and without leaving any reservation
    /// behind) when the queue is over budget or the receiver is gone.
    fn try_enqueue(&self, raw: Vec<u8>) -> Result<(), TryEnqueueError> {
        let len = raw.len();
        // Reserve first, then verify against the budget. Only the router (under
        // its mutex) enqueues, so reservations are serialized; the drain only
        // ever *decrements*, so a concurrent drain can only make room. A failed
        // reservation is refunded below, keeping the counter drift-free.
        let prev = self.bytes.fetch_add(len, Ordering::AcqRel);
        if prev + len > self.budget {
            self.bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(TryEnqueueError::OverBudget);
        }
        if self.tx.send(raw).is_err() {
            self.bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(TryEnqueueError::Closed);
        }
        Ok(())
    }

    /// Bytes currently reserved on this connection's outbound queue.
    pub fn queued_bytes(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }
}

/// Consumer half of a connection's outbound queue, owned by the WS writer task.
///
/// Each [`OutboundReceiver::recv`] yields an [`OutboundFrame`] guard that holds
/// its byte reservation until dropped, so the budget reflects bytes the relay
/// is still buffering. Dropping the receiver drops the channel, after which any
/// further [`OutboundHandle::try_enqueue`] fails as [`TryEnqueueError::Closed`].
pub struct OutboundReceiver {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    bytes: Arc<AtomicUsize>,
}

impl OutboundReceiver {
    /// Awaits the next queued frame, or `None` once every [`OutboundHandle`] is
    /// dropped and the queue is drained. The returned guard decrements the byte
    /// counter when dropped (after the WS write completes).
    pub async fn recv(&mut self) -> Option<OutboundFrame> {
        let raw = self.rx.recv().await?;
        Some(OutboundFrame {
            len: raw.len(),
            raw,
            bytes: self.bytes.clone(),
        })
    }
}

/// A dequeued outbound frame. Holds its byte reservation until dropped; the WS
/// writer reads [`OutboundFrame::bytes`] (or takes the [`Vec`] via
/// [`OutboundFrame::into_vec`]), writes it, then drops the guard to release the
/// reservation.
pub struct OutboundFrame {
    raw: Vec<u8>,
    len: usize,
    bytes: Arc<AtomicUsize>,
}

impl OutboundFrame {
    /// The frame's raw bytes to write to the socket.
    pub fn bytes(&self) -> &[u8] {
        &self.raw
    }

    /// The frame's byte length (also the reservation released on drop).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the frame is empty.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

impl Drop for OutboundFrame {
    fn drop(&mut self) {
        self.bytes.fetch_sub(self.len, Ordering::AcqRel);
    }
}

/// Creates a linked ([`OutboundHandle`], [`OutboundReceiver`]) pair sharing a
/// fresh byte counter and the given `budget`.
pub fn outbound_channel(budget: usize) -> (OutboundHandle, OutboundReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let bytes = Arc::new(AtomicUsize::new(0));
    (
        OutboundHandle {
            tx,
            bytes: bytes.clone(),
            budget,
        },
        OutboundReceiver { rx, bytes },
    )
}

/// Current wall-clock time in whole seconds since the unix epoch, used to stamp
/// and expire pairing windows. A clock before the epoch (unrepresentable in
/// practice) folds to `0` rather than panicking.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BridgeEntry;
    use remora_protocol::{AssertedDevice, PushRegistration, RelayControl};

    /// Registers `devices` as `bridge`'s full asserted set at `now`.
    fn assert_devices(
        router: &Router,
        bridge: &ConnPermit,
        now: u64,
        devices: Vec<AssertedDevice>,
    ) -> ControlOutcome {
        router.handle_control_at(bridge, RelayControl::AssertDevices { id: 1, devices }, now)
    }

    // NOTE: the brief's `frames_before_hello_have_no_permit` case is not a test
    // here: `Router::route` takes a `&ConnPermit`, and a `ConnPermit` can only
    // be minted by a successful `hello`. "Frames before hello" is therefore
    // unrepresentable in this API — the WS layer (Task 8) is where a
    // pre-authentication Data frame can arrive, so that guard is tested there.

    const BRIDGE: u8 = 0x11;
    const DEVICE: u8 = 0x22;
    const OTHER_BRIDGE: u8 = 0x33;
    const OTHER_DEVICE: u8 = 0x44;

    fn did(fill: u8) -> DeviceId {
        DeviceId([fill; 32])
    }

    /// Config with one bridge (`BRIDGE`), plus a second bridge (`OTHER_BRIDGE`)
    /// for cross-group tests. Devices are no longer config-driven (ADR-0021
    /// D4) — `device_hello`/`DEVICE`/`OTHER_DEVICE` below are admitted only
    /// after their bridge asserts their credential at runtime (see
    /// `authenticate_at`'s `HelloRole::Device` arm), so device-hello tests must
    /// register the bridge and `assert_devices` first.
    fn config() -> Arc<RelayConfig> {
        Arc::new(RelayConfig {
            listen: "127.0.0.1:0".to_string(),
            bridges: vec![
                BridgeEntry {
                    token: "bridge-tok".to_string(),
                    device_id: did(BRIDGE),
                },
                BridgeEntry {
                    token: "other-bridge-tok".to_string(),
                    device_id: did(OTHER_BRIDGE),
                },
            ],
            buffer_bytes: 1_048_576,
            handshake_timeout_secs: 10,
            max_connections: 1024,
            audit: None,
            push: crate::push::PushConfig::default(),
        })
    }

    /// [`config`] with push delivery enabled (and a generous per-bridge budget),
    /// for the wake-decision tests.
    fn config_push_enabled() -> Arc<RelayConfig> {
        let mut cfg = (*config()).clone();
        cfg.push = crate::push::PushConfig {
            enabled: true,
            per_bridge_per_minute: 60,
            device_cooldown_secs: 30,
            ..crate::push::PushConfig::default()
        };
        Arc::new(cfg)
    }

    fn bridge_hello() -> RelayHello {
        RelayHello {
            role: HelloRole::Bridge,
            token: "bridge-tok".to_string(),
            device_id: did(BRIDGE),
            routing_id: did(BRIDGE),
            bridge_id: did(BRIDGE),
        }
    }

    /// Device hello for `DEVICE` in `BRIDGE`'s group, routed as `routing`.
    fn device_hello(routing: u8) -> RelayHello {
        RelayHello {
            role: HelloRole::Device,
            token: "device-tok".to_string(),
            device_id: did(DEVICE),
            routing_id: did(routing),
            bridge_id: did(BRIDGE),
        }
    }

    fn accept(outcome: HelloOutcome) -> ConnPermit {
        match outcome {
            HelloOutcome::Accepted(permit) => permit,
            HelloOutcome::Rejected => panic!("expected Accepted, got Rejected"),
        }
    }

    #[tokio::test]
    async fn bridge_hello_with_scoped_token_registers() {
        let router = Router::new(config());
        let (handle, _rx) = outbound_channel(1024);
        let (outcome, displaced) = router.hello(&bridge_hello(), handle);
        assert!(displaced.is_none());
        let permit = accept(outcome);
        assert_eq!(permit.routing_id(), did(BRIDGE));
        assert_eq!(permit.role(), HelloRole::Bridge);
    }

    #[tokio::test]
    async fn device_hello_rejected_until_asserted() {
        let router = Router::new(config());
        // No bridge, no assertion yet: a device hello is rejected.
        let (dh, _drx) = outbound_channel(1024);
        let (outcome, _) = router.hello_at(&device_hello(0x55), dh, 0);
        assert!(matches!(outcome, HelloOutcome::Rejected));
    }

    #[tokio::test]
    async fn asserted_device_hello_accepted() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let (dh, _drx) = outbound_channel(1024);
        let (outcome, _) = router.hello_at(&device_hello(0x55), dh, 0);
        assert!(matches!(outcome, HelloOutcome::Accepted(_)));
    }

    #[tokio::test]
    async fn control_from_device_is_not_bridge() {
        let router = Router::new(config());
        // Register the device via an assertion first so it can connect...
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let device = accept(
            router
                .hello_at(&device_hello(0x55), outbound_channel(1024).0, 0)
                .0,
        );
        let outcome = router.handle_control_at(&device, RelayControl::CancelPairing { id: 1 }, 0);
        assert!(matches!(outcome, ControlOutcome::NotBridge));
    }

    #[tokio::test]
    async fn register_pairing_token_authorizes_a_device_then_replaces() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        // Open a window with rendezvous token "rvz", ttl 120s.
        let out = router.handle_control_at(
            &bridge,
            RelayControl::RegisterPairing {
                id: 1,
                token: "rvz".to_string(),
                ttl_secs: 120,
            },
            1000,
        );
        assert!(matches!(out, ControlOutcome::Ack));
        // A device presenting the rendezvous token for this bridge is admitted.
        let mut h = device_hello(0x60);
        h.token = "rvz".to_string();
        assert!(matches!(
            router.hello_at(&h, outbound_channel(1024).0, 1000).0,
            HelloOutcome::Accepted(_)
        ));
        // A second RegisterPairing replaces the first; the old token no longer works.
        router.handle_control_at(
            &bridge,
            RelayControl::RegisterPairing {
                id: 2,
                token: "rvz2".to_string(),
                ttl_secs: 120,
            },
            1001,
        );
        let mut h_old = device_hello(0x61);
        h_old.token = "rvz".to_string();
        assert!(matches!(
            router.hello_at(&h_old, outbound_channel(1024).0, 1001).0,
            HelloOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn pairing_token_expires() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        router.handle_control_at(
            &bridge,
            RelayControl::RegisterPairing {
                id: 1,
                token: "rvz".to_string(),
                ttl_secs: 120,
            },
            1000,
        );
        let mut h = device_hello(0x62);
        h.token = "rvz".to_string();
        // now = 1000 + 121 > deadline: rejected.
        assert!(matches!(
            router.hello_at(&h, outbound_channel(1024).0, 1121).0,
            HelloOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn deassert_returns_kick_handles() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        // Device connects.
        let (dh, _drx) = outbound_channel(1024);
        let _device = accept(router.hello_at(&device_hello(0x55), dh, 0).0);
        // Re-assert WITHOUT the device: the router returns the routing id to kick.
        let out = assert_devices(&router, &bridge, 0, vec![]);
        match out {
            ControlOutcome::Asserted { kicked, .. } => assert_eq!(kicked, vec![did(0x55)]),
            other => panic!("expected Asserted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stale_serial_assert_is_dropped() {
        // A bridge reconnects (new serial); an assert on the OLD permit must not
        // resurrect a credential.
        let router = Router::new(config());
        let old = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        let _new = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0); // displaces old
        let out = router.handle_control_at(
            &old,
            RelayControl::AssertDevices {
                id: 1,
                devices: vec![AssertedDevice {
                    device_id: did(DEVICE),
                    token: "device-tok".to_string(),
                    push: None,
                }],
            },
            0,
        );
        assert!(
            matches!(out, ControlOutcome::Error(_)),
            "stale-serial assert rejected"
        );
        // The device is NOT authorized.
        let (dh, _drx) = outbound_channel(1024);
        assert!(matches!(
            router.hello_at(&device_hello(0x55), dh, 0).0,
            HelloOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn bridge_disconnect_clears_soft_state() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        router.disconnect(&bridge);
        // With the bridge gone, its device is no longer authorized.
        let (dh, _drx) = outbound_channel(1024);
        assert!(matches!(
            router.hello_at(&device_hello(0x55), dh, 0).0,
            HelloOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn device_routing_id_zero_rejected() {
        let router = Router::new(config());
        let mut hello = device_hello(0x55);
        hello.routing_id = DeviceId::ZERO;
        let (handle, _rx) = outbound_channel(1024);
        let (outcome, _) = router.hello(&hello, handle);
        assert!(matches!(outcome, HelloOutcome::Rejected));
    }

    #[tokio::test]
    async fn device_routing_id_colliding_with_bridge_rejected() {
        // A device may not route under a routing id that shadows a bridge id.
        let router = Router::new(config());
        let hello = device_hello(BRIDGE);
        let (handle, _rx) = outbound_channel(1024);
        let (outcome, _) = router.hello(&hello, handle);
        assert!(matches!(outcome, HelloOutcome::Rejected));
    }

    #[tokio::test]
    async fn duplicate_routing_id_newest_wins_returns_old_handle() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let (h1, _rx1) = outbound_channel(1024);
        let (outcome1, displaced1) = router.hello(&device_hello(0x55), h1);
        assert!(displaced1.is_none());
        let _permit1 = accept(outcome1);

        let (h2, _rx2) = outbound_channel(1024);
        let (outcome2, displaced2) = router.hello(&device_hello(0x55), h2);
        let _permit2 = accept(outcome2);
        assert!(
            displaced2.is_some(),
            "old holder handed back for 4009 close"
        );
    }

    #[tokio::test]
    async fn device_to_its_bridge_routes() {
        let router = Router::new(config());
        let (bh, mut brx) = outbound_channel(1024);
        let bridge = accept(router.hello(&bridge_hello(), bh).0);
        assert_eq!(bridge.routing_id(), did(BRIDGE));
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );

        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);

        let (outcome, kill) = router.route(&device, did(0x55), did(BRIDGE), vec![1, 2, 3]);
        assert!(matches!(outcome, RouteOutcome::Delivered));
        assert!(kill.is_none());
        let frame = brx.recv().await.expect("frame delivered to bridge");
        assert_eq!(frame.bytes(), &[1, 2, 3]);
    }

    #[tokio::test]
    async fn device_to_other_device_not_allowed() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);
        // Address a peer that is not the device's declared bridge.
        let (outcome, kill) = router.route(&device, did(0x55), did(OTHER_DEVICE), vec![9]);
        assert!(matches!(outcome, RouteOutcome::NotAllowed));
        assert!(kill.is_none());
    }

    #[tokio::test]
    async fn bridge_to_foreign_group_device_not_allowed() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);

        // OTHER_BRIDGE connects and asserts OTHER_DEVICE into its own group.
        let other_bridge_hello = RelayHello {
            role: HelloRole::Bridge,
            token: "other-bridge-tok".to_string(),
            device_id: did(OTHER_BRIDGE),
            routing_id: did(OTHER_BRIDGE),
            bridge_id: did(OTHER_BRIDGE),
        };
        let other_bridge = accept(
            router
                .hello(&other_bridge_hello, outbound_channel(1024).0)
                .0,
        );
        assert_devices(
            &router,
            &other_bridge,
            0,
            vec![AssertedDevice {
                device_id: did(OTHER_DEVICE),
                token: "other-device-tok".to_string(),
                push: None,
            }],
        );

        // A device registered in OTHER_BRIDGE's group.
        let foreign_hello = RelayHello {
            role: HelloRole::Device,
            token: "other-device-tok".to_string(),
            device_id: did(OTHER_DEVICE),
            routing_id: did(0x66),
            bridge_id: did(OTHER_BRIDGE),
        };
        let (fh, _frx) = outbound_channel(1024);
        let _foreign = accept(router.hello(&foreign_hello, fh).0);

        let (outcome, kill) = router.route(&bridge, did(BRIDGE), did(0x66), vec![7]);
        assert!(matches!(outcome, RouteOutcome::NotAllowed));
        assert!(kill.is_none());
    }

    #[tokio::test]
    async fn route_to_offline_peer_is_peer_unavailable() {
        // The device is registered (its bridge asserted it, then dropped): the
        // device's registration outlives the bridge, so routing to the now-gone
        // bridge is PeerUnavailable.
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);
        router.disconnect(&bridge);
        let (outcome, kill) = router.route(&device, did(0x55), did(BRIDGE), vec![1]);
        assert!(matches!(outcome, RouteOutcome::PeerUnavailable));
        assert!(kill.is_none());
    }

    #[tokio::test]
    async fn budget_overflow_returns_dst_handle() {
        let router = Router::new(config());
        // Bridge has a tiny 100-byte outbound budget; the device is the sender.
        let (bh, _brx) = outbound_channel(100);
        let bridge = accept(router.hello(&bridge_hello(), bh).0);
        assert_eq!(bridge.routing_id(), did(BRIDGE));
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );

        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);

        let first = router.route(&device, did(0x55), did(BRIDGE), vec![0u8; 60]);
        assert!(matches!(first.0, RouteOutcome::Delivered));
        assert!(first.1.is_none());

        let second = router.route(&device, did(0x55), did(BRIDGE), vec![0u8; 60]);
        assert!(matches!(second.0, RouteOutcome::Overflow));
        let killed = second.1.expect("dst handle handed back for 4008 close");
        assert_eq!(
            killed.queued_bytes(),
            60,
            "only the first frame stayed queued"
        );
    }

    #[tokio::test]
    async fn disconnect_deregisters() {
        let router = Router::new(config());
        let (bh, _brx) = outbound_channel(1024);
        let bridge = accept(router.hello(&bridge_hello(), bh).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );

        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);

        // Bridge is reachable before disconnect.
        assert!(matches!(
            router.route(&device, did(0x55), did(BRIDGE), vec![1]).0,
            RouteOutcome::Delivered
        ));

        router.disconnect(&bridge);

        let (outcome, _) = router.route(&device, did(0x55), did(BRIDGE), vec![1]);
        assert!(matches!(outcome, RouteOutcome::PeerUnavailable));
    }

    #[tokio::test]
    async fn replaced_connection_disconnect_does_not_evict_replacement() {
        // Epoch guard: an old (4009-displaced) permit's late disconnect must
        // not deregister the connection that replaced it.
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );

        let (h1, _rx1) = outbound_channel(1024);
        let old = accept(router.hello(&device_hello(0x55), h1).0);

        let (h2, _rx2) = outbound_channel(1024);
        let (outcome2, displaced) = router.hello(&device_hello(0x55), h2);
        let _new = accept(outcome2);
        assert!(displaced.is_some());

        // The old connection now closes; its stale serial must be a no-op.
        router.disconnect(&old);

        // The replacement is still registered: route bridge->device succeeds.
        let (outcome, _) = router.route(&bridge, did(BRIDGE), did(0x55), vec![1]);
        assert!(matches!(outcome, RouteOutcome::Delivered));
    }

    #[tokio::test]
    async fn envelope_src_must_match_permit_routing_id() {
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);
        // Spoofed src (not the device's own routing id).
        let (outcome, kill) = router.route(&device, did(0x77), did(BRIDGE), vec![1]);
        assert!(matches!(outcome, RouteOutcome::NotAllowed));
        assert!(kill.is_none());
    }

    // ---- Push-wake decision (ADR-0023, spec Task 6) ----

    /// An asserted device credential carrying a `UnifiedPush` endpoint.
    fn asserted_with_push(device: u8, endpoint: &str) -> AssertedDevice {
        AssertedDevice {
            device_id: did(device),
            token: "device-tok".to_string(),
            push: Some(PushRegistration::UnifiedPush {
                endpoint: endpoint.to_string(),
            }),
        }
    }

    #[tokio::test]
    async fn push_trigger_unasserted_dst_is_not_asserted() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/x")],
        );
        // A dst that is not in the bridge's asserted set is an accept-rule
        // violation, regardless of push config.
        let out = router.decide_push_wake_at(&bridge, did(0x99), std::time::Instant::now());
        assert!(matches!(out, PushDecision::NotAsserted));
    }

    #[tokio::test]
    async fn push_trigger_from_non_bridge_is_not_asserted() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/x")],
        );
        let device = accept(
            router
                .hello_at(&device_hello(0x55), outbound_channel(1024).0, 0)
                .0,
        );
        // A device permit can never drive a wake decision.
        let out = router.decide_push_wake_at(&device, did(DEVICE), std::time::Instant::now());
        assert!(matches!(out, PushDecision::NotAsserted));
    }

    #[tokio::test]
    async fn push_trigger_disabled_config_drops_disabled() {
        // Default config has push disabled: a well-formed trigger is accepted
        // (never NotAsserted here) but dropped Disabled.
        let router = Router::new(config());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/x")],
        );
        let out = router.decide_push_wake_at(&bridge, did(DEVICE), std::time::Instant::now());
        assert!(matches!(out, PushDecision::Drop(DropReason::Disabled)));
    }

    #[tokio::test]
    async fn push_trigger_no_endpoint_drops() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        // Assert the device WITHOUT a push registration.
        assert_devices(
            &router,
            &bridge,
            0,
            vec![AssertedDevice {
                device_id: did(DEVICE),
                token: "device-tok".to_string(),
                push: None,
            }],
        );
        let out = router.decide_push_wake_at(&bridge, did(DEVICE), std::time::Instant::now());
        assert!(matches!(out, PushDecision::Drop(DropReason::NoEndpoint)));
    }

    #[tokio::test]
    async fn push_trigger_dst_connected_drops() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/x")],
        );
        // The target device is currently connected: no wake needed.
        let _device = accept(
            router
                .hello_at(&device_hello(0x55), outbound_channel(1024).0, 0)
                .0,
        );
        let out = router.decide_push_wake_at(&bridge, did(DEVICE), std::time::Instant::now());
        assert!(matches!(out, PushDecision::Drop(DropReason::DstConnected)));
    }

    #[tokio::test]
    async fn push_trigger_policy_invalid_endpoint_drops_and_is_surfaced() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        // A syntactically invalid endpoint: assert still ACKs, endpoint flagged.
        let out = assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "file:///etc/passwd")],
        );
        match out {
            ControlOutcome::Asserted {
                kicked,
                invalid_push,
            } => {
                assert!(kicked.is_empty());
                assert_eq!(invalid_push.len(), 1, "the bad endpoint is surfaced");
                assert_eq!(invalid_push[0].0, did(DEVICE));
                assert!(!invalid_push[0].1.is_empty(), "reason present");
            }
            other => panic!("expected Asserted, got {other:?}"),
        }
        // Delivery-time verdict is PolicyInvalid.
        let out = router.decide_push_wake_at(&bridge, did(DEVICE), std::time::Instant::now());
        assert!(matches!(out, PushDecision::Drop(DropReason::PolicyInvalid)));
    }

    #[tokio::test]
    async fn push_trigger_delivers_endpoint() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/topic")],
        );
        // dst asserted, valid endpoint, not connected, push enabled → deliver.
        let out = router.decide_push_wake_at(&bridge, did(DEVICE), std::time::Instant::now());
        match out {
            PushDecision::Deliver { endpoint, .. } => assert_eq!(endpoint, "https://ntfy.sh/topic"),
            other => panic!("expected Deliver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cooldown_survives_reassert() {
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/topic")],
        );
        let t0 = std::time::Instant::now();
        // First wake delivers and stamps the device cooldown.
        assert!(matches!(
            router.decide_push_wake_at(&bridge, did(DEVICE), t0),
            PushDecision::Deliver { .. }
        ));
        // Re-assert the SAME device: this replaces the bridge's stored
        // registration but MUST NOT reset the surviving cooldown.
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/topic")],
        );
        // A wake at the same instant is still inside the cooldown window.
        assert!(matches!(
            router.decide_push_wake_at(&bridge, did(DEVICE), t0),
            PushDecision::Drop(DropReason::Cooldown)
        ));
    }

    #[tokio::test]
    async fn revoke_wake_stamp_reopens_cooldown_for_a_failed_delivery() {
        // A delivery that ultimately failed revokes its cooldown stamp, so a
        // subsequent decision for the same device — even at the same instant —
        // is admitted again rather than suppressed by a wake that never landed.
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/topic")],
        );
        let t0 = std::time::Instant::now();
        let stamped = match router.decide_push_wake_at(&bridge, did(DEVICE), t0) {
            PushDecision::Deliver { stamped, .. } => stamped,
            other => panic!("expected Deliver, got {other:?}"),
        };
        // Without a revoke, the same instant would be Cooldown; after the revoke
        // the stamp is gone and the wake is admitted again.
        router.revoke_wake_stamp(did(DEVICE), stamped);
        assert!(matches!(
            router.decide_push_wake_at(&bridge, did(DEVICE), t0),
            PushDecision::Deliver { .. }
        ));
    }

    #[tokio::test]
    async fn revoke_wake_stamp_never_clobbers_a_newer_stamp() {
        // A late revoke from an old, failed delivery must not clear a newer
        // legitimate wake's stamp (compare-and-clear on the exact instant).
        let router = Router::new(config_push_enabled());
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        assert_devices(
            &router,
            &bridge,
            0,
            vec![asserted_with_push(DEVICE, "https://ntfy.sh/topic")],
        );
        let t0 = std::time::Instant::now();
        let stamped_old = match router.decide_push_wake_at(&bridge, did(DEVICE), t0) {
            PushDecision::Deliver { stamped, .. } => stamped,
            other => panic!("expected Deliver, got {other:?}"),
        };
        // Past the cooldown, a fresh wake stamps a *newer* instant.
        let t2 = t0 + std::time::Duration::from_secs(31);
        assert!(matches!(
            router.decide_push_wake_at(&bridge, did(DEVICE), t2),
            PushDecision::Deliver { .. }
        ));
        // A late revoke of the OLD stamp is a no-op: the newer stamp survives,
        // so a wake at t2 is still suppressed by cooldown.
        router.revoke_wake_stamp(did(DEVICE), stamped_old);
        assert!(matches!(
            router.decide_push_wake_at(&bridge, did(DEVICE), t2),
            PushDecision::Drop(DropReason::Cooldown)
        ));
    }
}
