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

use remora_protocol::{DeviceId, HelloRole, RelayHello};
use tokio::sync::mpsc;

use crate::config::{token_matches, RelayConfig};

/// Sans-IO connection registry and routing authority. Cheap to clone-share via
/// the returned [`Arc`]; all mutable state lives behind a single [`Mutex`] that
/// is never held across an `.await` (every method here is synchronous).
pub struct Router {
    config: Arc<RelayConfig>,
    state: Mutex<RouterState>,
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
}

/// One live connection's registration.
struct Registration {
    role: HelloRole,
    /// For a device: the bridge it routes through. For a bridge: its own id.
    bridge_id: DeviceId,
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

/// Result of a [`Router::route`] decision.
pub enum RouteOutcome {
    /// The frame was enqueued on the destination's outbound queue.
    Delivered,
    /// The destination is not currently registered. The caller closes the
    /// **sender** with WebSocket close code 4004.
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
        Arc::new(Router {
            config,
            state: Mutex::new(RouterState {
                next_serial: 0,
                conns: HashMap::new(),
            }),
        })
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
        let Some((routing_id, bridge_id)) = self.authenticate(hello) else {
            return (HelloOutcome::Rejected, None);
        };

        let mut state = self.lock();
        let serial = state.next_serial;
        state.next_serial += 1;
        let displaced = state.conns.insert(
            routing_id,
            Registration {
                role: hello.role,
                bridge_id,
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
    /// Returns the `(routing_id, bridge_id)` the connection registers under, or
    /// `None` if the hello must be rejected.
    fn authenticate(&self, hello: &RelayHello) -> Option<(DeviceId, DeviceId)> {
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
                let entry =
                    self.config.devices.iter().find(|d| {
                        d.device_id == hello.device_id && d.bridge_id == hello.bridge_id
                    })?;
                if !token_matches(&hello.token, &entry.token) {
                    return None;
                }
                Some((hello.routing_id, hello.bridge_id))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BridgeEntry, DeviceEntry};

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

    /// Config with one bridge (`BRIDGE`) and one device (`DEVICE`) bound to it,
    /// plus a second bridge/device pair for cross-group tests.
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
            devices: vec![
                DeviceEntry {
                    token: "device-tok".to_string(),
                    device_id: did(DEVICE),
                    bridge_id: did(BRIDGE),
                },
                DeviceEntry {
                    token: "other-device-tok".to_string(),
                    device_id: did(OTHER_DEVICE),
                    bridge_id: did(OTHER_BRIDGE),
                },
            ],
            buffer_bytes: 1_048_576,
            audit: None,
        })
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
    async fn device_hello_wrong_token_rejected() {
        let router = Router::new(config());
        let mut hello = device_hello(0x55);
        hello.token = "wrong".to_string();
        let (handle, _rx) = outbound_channel(1024);
        let (outcome, displaced) = router.hello(&hello, handle);
        assert!(matches!(outcome, HelloOutcome::Rejected));
        assert!(displaced.is_none());
    }

    #[tokio::test]
    async fn device_token_bound_to_other_bridge_rejected() {
        // `DEVICE`'s token is valid only for `BRIDGE`; claiming `OTHER_BRIDGE`
        // finds no matching (device_id, bridge_id) entry.
        let router = Router::new(config());
        let mut hello = device_hello(0x55);
        hello.bridge_id = did(OTHER_BRIDGE);
        let (handle, _rx) = outbound_channel(1024);
        let (outcome, _) = router.hello(&hello, handle);
        assert!(matches!(outcome, HelloOutcome::Rejected));
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
        // Device is registered but its bridge never connected.
        let router = Router::new(config());
        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);
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
        let (h1, _rx1) = outbound_channel(1024);
        let old = accept(router.hello(&device_hello(0x55), h1).0);

        let (h2, _rx2) = outbound_channel(1024);
        let (outcome2, displaced) = router.hello(&device_hello(0x55), h2);
        let _new = accept(outcome2);
        assert!(displaced.is_some());

        // The old connection now closes; its stale serial must be a no-op.
        router.disconnect(&old);

        // The replacement is still registered: route bridge->device succeeds.
        let bridge = accept(router.hello(&bridge_hello(), outbound_channel(1024).0).0);
        let (outcome, _) = router.route(&bridge, did(BRIDGE), did(0x55), vec![1]);
        assert!(matches!(outcome, RouteOutcome::Delivered));
    }

    #[tokio::test]
    async fn envelope_src_must_match_permit_routing_id() {
        let router = Router::new(config());
        let (dh, _drx) = outbound_channel(1024);
        let device = accept(router.hello(&device_hello(0x55), dh).0);
        // Spoofed src (not the device's own routing id).
        let (outcome, kill) = router.route(&device, did(0x77), did(BRIDGE), vec![1]);
        assert!(matches!(outcome, RouteOutcome::NotAllowed));
        assert!(kill.is_none());
    }
}
