//! Blind envelope-frame relay (ADR-0021).
//!
//! The relay routes opaque [`remora_protocol::Envelope`] frames between
//! authenticated WebSocket connections; it never terminates the Noise
//! session those frames carry and never inspects their payload. Blindness
//! here is not just a runtime property — it is enforced by this crate's
//! dependency graph: `remora-relay` must never depend on `snow`,
//! `remora-core`, or any other crypto/session-content crate. If a change to
//! this crate would need one, the design is wrong, not the dependency list.
//!
//! This module provides the relay's config surface ([`config`]) and the
//! sans-IO [`router`] core (registry, hello auth, adjacency routing, byte
//! budgets); the WebSocket server lands in a later slice of #231.

mod config;
mod router;

pub use config::{AuditConfig, BridgeEntry, DeviceEntry, RelayConfig, RelayConfigError};
pub use router::{
    outbound_channel, ConnPermit, HelloOutcome, OutboundFrame, OutboundHandle, OutboundReceiver,
    RouteOutcome, Router,
};
