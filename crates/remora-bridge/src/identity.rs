//! File-based bridge identity, device roster, and client pairing files.
//!
//! Three secrets live on disk here, all created `0600`:
//! - the bridge identity ([`BridgeIdentity`]): its device id + static keypair;
//! - the roster ([`Roster`]): one [`RosterEntry`] per paired device, pinning the
//!   device's static public key and holding the shared PSK for that pair;
//! - the client pairing file ([`PairingFile`]): everything a device needs to
//!   reach and authenticate to this bridge (spec D4 — the QR payload sans QR).
//!
//! Keys are X25519 static keys from `snow`'s Noise builder; the wire pattern is
//! `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s`, so the bridge is the responder with
//! a known static key and each pair carries an independent `psk2`.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use base64::Engine as _;
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};

use remora_protocol::DeviceId;

/// Noise pattern the bridge and its clients speak. The bridge is the responder
/// with a known static key; `psk2` binds each session to the paired device's
/// PSK. Used here only to mint X25519 static keypairs.
const NOISE_PATTERN: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Errors from loading, saving, or provisioning identity material.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Reading or writing a file on disk failed.
    #[error("identity i/o error at {path}: {source}")]
    Io {
        /// The file involved.
        path: String,
        /// The underlying OS error.
        source: std::io::Error,
    },
    /// A TOML document on disk could not be parsed.
    #[error("malformed identity file: {0}")]
    Toml(#[from] toml::de::Error),
    /// An identity document could not be serialized to TOML.
    #[error("could not serialize identity file: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    /// A pairing file could not be parsed or serialized as JSON.
    #[error("malformed pairing file: {0}")]
    Json(#[from] serde_json::Error),
    /// A base64 field on disk could not be decoded.
    #[error("malformed base64 in identity file: {0}")]
    Base64(#[from] base64::DecodeError),
    /// A hex device id on disk was not a valid [`DeviceId`].
    #[error("malformed device id in identity file: {0}")]
    DeviceId(String),
    /// `snow` could not generate a keypair.
    #[error("noise keypair generation failed: {0}")]
    KeyGen(String),
    /// The OS CSPRNG could not be read.
    #[error("could not read random bytes: {0}")]
    Random(String),
}

impl IdentityError {
    fn io(path: &Path, source: std::io::Error) -> Self {
        IdentityError::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// Fills `buf` with cryptographically secure random bytes from the OS CSPRNG.
fn os_random(buf: &mut [u8]) -> Result<(), IdentityError> {
    rand::rngs::OsRng
        .try_fill_bytes(buf)
        .map_err(|e| IdentityError::Random(e.to_string()))
}

/// Generates a fresh X25519 static keypair via the Noise builder.
fn generate_keypair() -> Result<snow::Keypair, IdentityError> {
    let params = NOISE_PATTERN
        .parse()
        .map_err(|e: snow::Error| IdentityError::KeyGen(e.to_string()))?;
    snow::Builder::new(params)
        .generate_keypair()
        .map_err(|e| IdentityError::KeyGen(e.to_string()))
}

/// Generates a random 32-byte [`DeviceId`].
fn random_device_id() -> Result<DeviceId, IdentityError> {
    let mut raw = [0u8; 32];
    os_random(&mut raw)?;
    Ok(DeviceId(raw))
}

/// Creates (or truncates) `path` as a `0600` file and writes `contents`.
///
/// The file is created with mode `0600` so a newly written secret is never
/// briefly world-readable. Because `mode()` is ignored when opening a file that
/// already exists, we also re-assert `0600` after truncation (which has already
/// emptied the file, so no secret bytes are exposed before we tighten perms).
fn write_secret_file(path: &Path, contents: &str) -> Result<(), IdentityError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| IdentityError::io(path, e))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| IdentityError::io(path, e))?;
    (&file)
        .write_all(contents.as_bytes())
        .map_err(|e| IdentityError::io(path, e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// BridgeIdentity
// ---------------------------------------------------------------------------

/// The bridge's own durable identity: a stable device id plus the X25519 static
/// keypair it presents in every Noise session.
///
/// Persisted as TOML with the id in hex and both key halves in base64. Not
/// `Clone`/`Debug` on purpose — it holds a private key.
pub struct BridgeIdentity {
    /// This bridge's stable device id (the `bridge_id` clients route to).
    pub device_id: DeviceId,
    /// The bridge's long-term X25519 static keypair.
    pub static_keypair: snow::Keypair,
}

/// On-disk shape of [`BridgeIdentity`].
#[derive(Serialize, Deserialize)]
struct IdentityFile {
    device_id: String,
    static_private_key: String,
    static_public_key: String,
}

impl BridgeIdentity {
    /// Loads the identity at `path`, or creates and persists a fresh one if the
    /// file does not exist. A corrupt existing file is a typed error, never
    /// silently overwritten. Created files are `0600`, and a reload returns the
    /// exact same id and key bytes.
    pub fn load_or_create(path: &Path) -> Result<BridgeIdentity, IdentityError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::create(path),
            Err(e) => Err(IdentityError::io(path, e)),
        }
    }

    fn from_toml(text: &str) -> Result<BridgeIdentity, IdentityError> {
        let doc: IdentityFile = toml::from_str(text)?;
        let device_id = doc
            .device_id
            .parse::<DeviceId>()
            .map_err(|e| IdentityError::DeviceId(e.to_string()))?;
        let private = B64.decode(doc.static_private_key)?;
        let public = B64.decode(doc.static_public_key)?;
        Ok(BridgeIdentity {
            device_id,
            static_keypair: snow::Keypair { private, public },
        })
    }

    fn create(path: &Path) -> Result<BridgeIdentity, IdentityError> {
        let identity = BridgeIdentity {
            device_id: random_device_id()?,
            static_keypair: generate_keypair()?,
        };
        let doc = IdentityFile {
            device_id: identity.device_id.to_string(),
            static_private_key: B64.encode(&identity.static_keypair.private),
            static_public_key: B64.encode(&identity.static_keypair.public),
        };
        let text = toml::to_string(&doc)?;
        write_secret_file(path, &text)?;
        Ok(identity)
    }
}

// ---------------------------------------------------------------------------
// Roster
// ---------------------------------------------------------------------------

/// One paired device: its device id, its pinned static public key, and the PSK
/// shared with this bridge for that pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    /// The device's stable device id.
    pub device_id: DeviceId,
    /// The device's pinned X25519 static public key.
    pub static_pubkey: Vec<u8>,
    /// The `psk2` shared between this device and this bridge.
    pub psk: [u8; 32],
}

/// The set of devices paired with this bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    /// The paired-device entries.
    pub entries: Vec<RosterEntry>,
}

/// On-disk shape of a single [`RosterEntry`] (id hex, key + psk base64).
#[derive(Serialize, Deserialize)]
struct RosterEntryFile {
    device_id: String,
    static_pubkey: String,
    psk: String,
}

/// On-disk shape of a [`Roster`].
#[derive(Serialize, Deserialize, Default)]
struct RosterFile {
    #[serde(default, rename = "entry")]
    entries: Vec<RosterEntryFile>,
}

impl Roster {
    /// Loads the roster at `path`. A missing file is an empty roster (an
    /// un-paired bridge is a normal, expected state, not an error).
    pub fn load(path: &Path) -> Result<Roster, IdentityError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Roster::default()),
            Err(e) => return Err(IdentityError::io(path, e)),
        };
        let doc: RosterFile = toml::from_str(&text)?;
        let mut entries = Vec::with_capacity(doc.entries.len());
        for e in doc.entries {
            let device_id = e
                .device_id
                .parse::<DeviceId>()
                .map_err(|err| IdentityError::DeviceId(err.to_string()))?;
            let static_pubkey = B64.decode(e.static_pubkey)?;
            let psk_bytes = B64.decode(e.psk)?;
            let psk: [u8; 32] = psk_bytes
                .try_into()
                .map_err(|_| IdentityError::DeviceId("psk is not 32 bytes".to_string()))?;
            entries.push(RosterEntry {
                device_id,
                static_pubkey,
                psk,
            });
        }
        Ok(Roster { entries })
    }

    /// Persists the roster to `path` as a `0600` TOML file.
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let doc = RosterFile {
            entries: self
                .entries
                .iter()
                .map(|e| RosterEntryFile {
                    device_id: e.device_id.to_string(),
                    static_pubkey: B64.encode(&e.static_pubkey),
                    psk: B64.encode(e.psk),
                })
                .collect(),
        };
        let text = toml::to_string(&doc)?;
        write_secret_file(path, &text)
    }

    /// Finds the entry for `id`, if this device is paired.
    pub fn find_by_device(&self, id: &DeviceId) -> Option<&RosterEntry> {
        self.entries.iter().find(|e| &e.device_id == id)
    }
}

// ---------------------------------------------------------------------------
// PairingFile
// ---------------------------------------------------------------------------

/// The client-side pairing bundle — the QR payload minus the QR (spec D4).
///
/// Everything a freshly provisioned device needs to reach this bridge through
/// the relay and complete the Noise handshake: the relay endpoint + rendezvous
/// token, the bridge's id and static public key (to pin), the shared PSK, and
/// the device's own minted id + keypair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingFile {
    /// The relay endpoint (`wss://…`) to dial.
    pub relay_url: String,
    /// The rendezvous token that pairs this device's connection to the bridge.
    pub rendezvous_token: String,
    /// The bridge's device id.
    pub bridge_id: DeviceId,
    /// The bridge's static public key (base64) — the device pins this.
    pub bridge_static_pubkey: String,
    /// The shared `psk2` for this pairing (base64).
    pub psk: String,
    /// The device's minted device id.
    pub device_id: DeviceId,
    /// The device's static private key (base64).
    pub device_private_key: String,
    /// The device's static public key (base64).
    pub device_public_key: String,
}

impl PairingFile {
    /// Loads a pairing file from `path` (JSON).
    pub fn load(path: &Path) -> Result<PairingFile, IdentityError> {
        let text = std::fs::read_to_string(path).map_err(|e| IdentityError::io(path, e))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Persists this pairing file to `path` as `0600` JSON (it carries the
    /// device's private key).
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let text = serde_json::to_string_pretty(self)?;
        write_secret_file(path, &text)
    }
}

// ---------------------------------------------------------------------------
// Provisioning
// ---------------------------------------------------------------------------

/// Provisions a new device against this bridge: mints an X25519 keypair, a
/// random device id, and a random 32-byte PSK; appends the matching
/// [`RosterEntry`] (pinning the device's static public key) to `roster`; and
/// returns the [`PairingFile`] the device needs.
///
/// This is the slice-1 pairing story. The out-of-band workflow (QR, transfer)
/// is replaced by #232, but the material it produces — a device id, a pinned
/// device static key, and a per-`(device, bridge)` PSK — is final.
///
/// The caller is responsible for persisting the mutated `roster`.
pub fn provision_device(
    identity: &BridgeIdentity,
    roster: &mut Roster,
    relay_url: &str,
    rendezvous_token: &str,
) -> Result<PairingFile, IdentityError> {
    let device_keypair = generate_keypair()?;
    let device_id = random_device_id()?;
    let mut psk = [0u8; 32];
    os_random(&mut psk)?;

    roster.entries.push(RosterEntry {
        device_id,
        static_pubkey: device_keypair.public.clone(),
        psk,
    });

    Ok(PairingFile {
        relay_url: relay_url.to_string(),
        rendezvous_token: rendezvous_token.to_string(),
        bridge_id: identity.device_id,
        bridge_static_pubkey: B64.encode(&identity.static_keypair.public),
        psk: B64.encode(psk),
        device_id,
        device_private_key: B64.encode(&device_keypair.private),
        device_public_key: B64.encode(&device_keypair.public),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("stat file")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn load_or_create_creates_0600_and_reloads_identically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.toml");

        let first = BridgeIdentity::load_or_create(&path).expect("create");
        assert_eq!(file_mode(&path), 0o600, "identity file must be 0600");

        let second = BridgeIdentity::load_or_create(&path).expect("reload");
        assert_eq!(first.device_id, second.device_id);
        assert_eq!(
            first.static_keypair.private, second.static_keypair.private,
            "private key bytes must round-trip exactly"
        );
        assert_eq!(
            first.static_keypair.public, second.static_keypair.public,
            "public key bytes must round-trip exactly"
        );
        assert_eq!(first.static_keypair.public.len(), 32);
        assert_eq!(first.static_keypair.private.len(), 32);
    }

    #[test]
    fn corrupt_identity_file_is_a_typed_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write garbage");

        // `BridgeIdentity` is intentionally not `Debug` (it holds a private
        // key), so unwrap the Result by hand rather than via `expect_err`.
        let err = match BridgeIdentity::load_or_create(&path) {
            Ok(_) => panic!("corrupt file must not load"),
            Err(e) => e,
        };
        assert!(matches!(err, IdentityError::Toml(_)), "got: {err:?}");

        // A corrupt file must not be clobbered.
        let after = std::fs::read_to_string(&path).expect("still there");
        assert_eq!(after, "this is not valid toml = = =");
    }

    #[test]
    fn missing_roster_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");
        let roster = Roster::load(&path).expect("missing = empty");
        assert!(roster.entries.is_empty());
    }

    #[test]
    fn roster_save_load_round_trips_0600() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");

        let roster = Roster {
            entries: vec![
                RosterEntry {
                    device_id: DeviceId([0x11; 32]),
                    static_pubkey: vec![0xaa; 32],
                    psk: [0xbb; 32],
                },
                RosterEntry {
                    device_id: DeviceId([0x22; 32]),
                    static_pubkey: vec![0xcc; 32],
                    psk: [0xdd; 32],
                },
            ],
        };
        roster.save(&path).expect("save");
        assert_eq!(file_mode(&path), 0o600, "roster file must be 0600");

        let loaded = Roster::load(&path).expect("load");
        assert_eq!(loaded, roster);

        let found = loaded
            .find_by_device(&DeviceId([0x22; 32]))
            .expect("find second device");
        assert_eq!(found.psk, [0xdd; 32]);
        assert!(loaded.find_by_device(&DeviceId([0x99; 32])).is_none());
    }

    #[test]
    fn provision_device_round_trips_through_pairing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id_path = dir.path().join("identity.toml");
        let roster_path = dir.path().join("roster.toml");
        let pairing_path = dir.path().join("pairing.json");

        let identity = BridgeIdentity::load_or_create(&id_path).expect("identity");
        let mut roster = Roster::default();

        let pairing = provision_device(
            &identity,
            &mut roster,
            "wss://relay.example/ws",
            "rendezvous-abc123",
        )
        .expect("provision");

        // Persist both sides and reload from disk.
        roster.save(&roster_path).expect("save roster");
        pairing.save(&pairing_path).expect("save pairing");
        assert_eq!(file_mode(&pairing_path), 0o600, "pairing file must be 0600");

        let roster = Roster::load(&roster_path).expect("reload roster");
        let pairing = PairingFile::load(&pairing_path).expect("reload pairing");

        // The pairing file's bridge pubkey matches the bridge identity.
        assert_eq!(
            pairing.bridge_id, identity.device_id,
            "bridge id must match identity"
        );
        assert_eq!(
            B64.decode(&pairing.bridge_static_pubkey)
                .expect("valid base64"),
            identity.static_keypair.public,
            "bridge pubkey must match identity"
        );

        // The device is pinned in the roster with a matching pubkey + psk.
        let entry = roster
            .find_by_device(&pairing.device_id)
            .expect("device must be in roster");
        assert_eq!(
            entry.static_pubkey,
            B64.decode(&pairing.device_public_key)
                .expect("valid base64"),
            "roster pins the device's pairing-file public key"
        );
        assert_eq!(
            entry.psk.to_vec(),
            B64.decode(&pairing.psk).expect("valid base64"),
            "roster psk matches pairing-file psk"
        );

        // The device's own keypair is well-formed X25519 (32-byte halves).
        assert_eq!(
            B64.decode(&pairing.device_private_key)
                .expect("valid base64")
                .len(),
            32
        );
        assert_eq!(
            B64.decode(&pairing.device_public_key)
                .expect("valid base64")
                .len(),
            32
        );
    }

    #[test]
    fn provisioning_twice_yields_distinct_material() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id_path = dir.path().join("identity.toml");
        let identity = BridgeIdentity::load_or_create(&id_path).expect("identity");
        let mut roster = Roster::default();

        let a = provision_device(&identity, &mut roster, "wss://r/ws", "tok").expect("a");
        let b = provision_device(&identity, &mut roster, "wss://r/ws", "tok").expect("b");

        assert_ne!(a.device_id, b.device_id, "device ids must differ");
        assert_ne!(a.psk, b.psk, "psks must differ (CSPRNG, not fixed)");
        assert_ne!(a.device_public_key, b.device_public_key);
        assert_eq!(roster.entries.len(), 2);
    }
}
