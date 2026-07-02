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
//! This module currently provides only the relay's config surface
//! ([`config`]); the router and WebSocket server land in later slices of
//! #231.

mod config;

pub use config::{AuditConfig, BridgeEntry, DeviceEntry, RelayConfig, RelayConfigError};
