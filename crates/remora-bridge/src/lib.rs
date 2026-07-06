//! User-side bridge for Remora relay mode (ADR-0021).
//!
//! The bridge is the trusted endpoint that runs the end-to-end Noise session
//! with clients through the blind relay. This crate provides the *identity
//! layer*: the bridge's own static keypair and device id, the roster of
//! paired devices (their pinned static public keys, per-pair PSKs, and
//! relay-routing credentials), and the client-side [`PairingFile`] that a
//! paired device needs to reach and authenticate to this bridge.
//!
//! Device ids, pinned static keys, and a per-`(device, bridge)` PSK are the
//! pairing semantics. The out-of-band pairing ceremony (QR display,
//! confirm-gated enrollment) is driven by [`run_pairing`]; the `IKpsk2` Noise
//! handshake ([`Handshake`], [`HandshakeKind`]), the bridge server loop
//! ([`serve_bridge`]), and the [`RemoteSource`] transport are all implemented
//! here.

mod bridge;
mod identity;
mod noise;
mod pairing_client;
mod remote_source;
mod wake;
mod wire_error;

pub use bridge::{
    is_ws_url, serve_bridge, wake_channel, BridgeConfig, BridgeError, BridgeEvent, BridgeHealth,
    BridgeServeError, BridgeWakeHandle, PairingCommand, PairingOutcome, WakeReceiver,
};
pub use identity::{
    fingerprint, BridgeIdentity, IdentityError, IdentityLock, PairingFile, Roster, RosterEntry,
};
pub use noise::{
    chunk_bytes, prologue, Handshake, HandshakeKind, NoiseError, Transport, MAX_NOISE_PLAINTEXT,
    NOISE_PATTERN, PTY_CHUNK_BYTES,
};
pub use pairing_client::{run_pairing, PairingError, PairingProgress};
pub use remote_source::RemoteSource;
pub use wire_error::{map_source_error, map_wire_error};
