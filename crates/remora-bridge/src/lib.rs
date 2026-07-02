//! User-side bridge for Remora relay mode (ADR-0021).
//!
//! The bridge is the trusted endpoint that runs the end-to-end Noise session
//! with clients through the blind relay. This crate currently provides the
//! *identity layer* only: the bridge's own static keypair and device id, the
//! roster of paired devices (their pinned static public keys + per-pair PSKs),
//! and the client-side [`PairingFile`] that a freshly provisioned device needs
//! to reach and authenticate to this bridge.
//!
//! [`provision_device`] is the slice-1 pairing story: it mints a device
//! identity + PSK, pins it into the roster, and returns the matching pairing
//! file. The *workflow* around it (QR display, out-of-band transfer) is
//! replaced by #232, but the *semantics* — device ids, pinned static keys, and
//! a per-`(device, bridge)` PSK — are final.
//!
//! The Noise handshake, the bridge server loop, and the `RemoteSource`
//! transport land in later tasks.

mod identity;

pub use identity::{
    provision_device, BridgeIdentity, IdentityError, PairingFile, Roster, RosterEntry,
};
