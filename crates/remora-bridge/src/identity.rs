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
use std::path::{Path, PathBuf};

use base64::Engine as _;
use blake2::{Blake2s256, Digest as _};
use rand::TryRng as _;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

use remora_protocol::{DeviceId, PushRegistration};

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
    /// Another live process holds this identity (spec D2, #234).
    #[error("bridge identity {path} is in use by another bridge process (desktop app or another remora-bridge serve)")]
    Locked {
        /// The identity file another process is holding.
        path: PathBuf,
    },
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
    rand::rngs::SysRng
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

/// Atomically writes `contents` to `path` as a `0600` file.
///
/// Writes to a `0600` temp file in the *same* directory (so the final rename
/// stays on one filesystem and is therefore atomic), flushes and `sync_all()`s
/// it, then renames it over the target. This never truncates the target in
/// place: a crash or `ENOSPC` mid-write leaves the previous file intact instead
/// of an empty or half-written one — losing every paired PSK would otherwise be
/// a single failed write away. The temp file is created `0600` via
/// `create_new` (O_CREAT|O_EXCL) so a secret is never briefly world-readable and
/// a stray temp is never silently reused.
fn write_secret_file(path: &Path, contents: &str) -> Result<(), IdentityError> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        // No parent component (a bare file name): write into the cwd.
        _ => Path::new("."),
    };

    // A unique temp name in the same directory. The random suffix avoids
    // colliding with a concurrent writer or a leftover temp.
    let mut rand = [0u8; 8];
    os_random(&mut rand)?;
    let suffix: String = rand.iter().map(|b| format!("{b:02x}")).collect();
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("secret")),
    );
    tmp_name.push(".tmp.");
    tmp_name.push(&suffix);
    let tmp = dir.join(tmp_name);

    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();

    if let Err(e) = result {
        // Best-effort cleanup so a failed write never leaves a temp secret behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(IdentityError::io(path, e));
    }
    Ok(())
}

/// Tightens `path` to `0600` when its mode grants any group/other access,
/// warning once on stderr and naming the file.
///
/// The exposure already happened the moment the file existed with a loose mode;
/// refusing to load would brick a running bridge for a condition we can simply
/// correct. So we stop the *ongoing* exposure and continue.
fn ensure_secret_mode(path: &Path) -> std::io::Result<()> {
    let mode = std::fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        eprintln!(
            "warning: secret file {} had insecure mode {:o}; tightening to 0600",
            path.display(),
            mode & 0o777
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Held for the lifetime of a bridge host process to guarantee a single
/// process serves a given identity (spec D2, #234): the desktop's in-process
/// bridge and a headless `remora-bridge serve` pointed at the same state dir
/// would otherwise silently share one bridge identity — one relay
/// registration, two uncoordinated claimants.
///
/// Advisory `flock(LOCK_EX | LOCK_NB)` on `<identity>.lock` (a sibling file,
/// not the identity itself, so locking never races the atomic
/// temp-file-rename writes in [`write_secret_file`]). Dropping releases.
pub struct IdentityLock {
    // Held only for its flock; the fd releases the lock on drop.
    _file: std::fs::File,
}

impl IdentityLock {
    /// Acquire the exclusive lock for `identity_path`, failing fast (never
    /// blocking) when another process holds it.
    pub fn acquire(identity_path: &Path) -> Result<IdentityLock, IdentityError> {
        let mut lock_path = identity_path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| IdentityError::io(&lock_path, e))?;
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(IdentityLock { _file: file }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(IdentityError::Locked {
                path: identity_path.to_path_buf(),
            }),
            Err(e) => Err(IdentityError::io(&lock_path, std::io::Error::from(e))),
        }
    }
}

/// A short, human-comparable fingerprint of an X25519 static public key
/// (ADR-0021 D5): `XXXX-XXXX-XXXX`, the uppercase hex of the first 6 bytes of
/// BLAKE2s-256 over the key. 48 bits is ample for a distinguish-my-device
/// ceremony inside a short pairing window.
pub fn fingerprint(pubkey: &[u8]) -> String {
    let mut hasher = Blake2s256::new();
    hasher.update(pubkey);
    let digest = hasher.finalize();
    let hex: String = digest[..6].iter().map(|b| format!("{b:02X}")).collect();
    format!("{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12])
}

// ---------------------------------------------------------------------------
// BridgeIdentity
// ---------------------------------------------------------------------------

/// The bridge's own durable identity: a stable device id plus the X25519 static
/// keypair it presents in every Noise session.
///
/// Persisted as TOML with the id in hex and both key halves in base64. Not
/// `Clone`/`Debug` on purpose — it holds a private key — and the private key
/// bytes are zeroized on drop (#278).
pub struct BridgeIdentity {
    /// This bridge's stable device id (the `bridge_id` clients route to).
    pub device_id: DeviceId,
    /// The bridge's long-term X25519 static keypair.
    pub static_keypair: snow::Keypair,
}

impl Drop for BridgeIdentity {
    fn drop(&mut self) {
        // `snow::Keypair` is a plain struct with no drop hygiene of its own;
        // the private half is ours to scrub. The public half is public.
        self.static_keypair.private.zeroize();
    }
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
            Ok(text) => {
                // An existing identity file holds the bridge's private key; if a
                // prior run (or a bad umask) left it group/other-readable, tighten
                // it now rather than keep leaking it.
                ensure_secret_mode(path).map_err(|e| IdentityError::io(path, e))?;
                Self::from_toml(&text)
            }
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
///
/// Secret-bearing (#278): `psk` and `relay_token` are zeroized when an entry
/// (or any clone) drops, and both are redacted from the manual [`Debug`] impl.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct RosterEntry {
    /// The device's stable device id.
    #[zeroize(skip)]
    pub device_id: DeviceId,
    /// The device's pinned X25519 static public key.
    pub static_pubkey: Vec<u8>,
    /// The `psk2` shared between this device and this bridge.
    pub psk: [u8; 32],
    /// The per-device credential the bridge asserts to the relay; equals the
    /// device's `PairingFile.device_token`. A CREDENTIAL field — not defaulted.
    pub relay_token: String,
    /// Display name from the device's `PairingHello` (ADR-0021 D5).
    pub name: String,
    /// Unix seconds the device was enrolled.
    pub enrolled_at: Option<u64>,
    /// Unix seconds of the device's most recent successful session (updated
    /// on session establish so ghost/re-paired entries are self-evident).
    pub last_connected_at: Option<u64>,
    /// The device's registered push-wake channel (ADR-0023), if any. Set via
    /// `RemoteOp::RegisterPushEndpoint` (Task 4); a freshly enrolled device
    /// starts with `None`.
    #[zeroize(skip)]
    pub push: Option<PushRegistration>,
}

impl std::fmt::Debug for RosterEntry {
    /// Redacts `psk` (the per-pair session PSK) and `relay_token` (the
    /// per-device relay bearer credential); the rest is display metadata and
    /// the device's *public* pinned key (#278).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RosterEntry")
            .field("device_id", &self.device_id)
            .field("static_pubkey", &self.static_pubkey)
            .field("psk", &"[redacted]")
            .field("relay_token", &"[redacted]")
            .field("name", &self.name)
            .field("enrolled_at", &self.enrolled_at)
            .field("last_connected_at", &self.last_connected_at)
            .field("push", &self.push)
            .finish()
    }
}

/// The set of devices paired with this bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    /// The paired-device entries.
    pub entries: Vec<RosterEntry>,
}

/// On-disk shape of a single [`RosterEntry`] (id hex, key + psk base64).
///
/// `relay_token` is a credential: not `#[serde(default)]`, so a roster entry
/// missing it is a corrupt roster, not a partial one. `name`, `enrolled_at`,
/// `last_connected_at`, and `push` are display/optional metadata and are
/// defaulted, so an older roster file (or a hand-edited one) still loads.
#[derive(Serialize, Deserialize)]
struct RosterEntryFile {
    device_id: String,
    static_pubkey: String,
    psk: String,
    relay_token: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    enrolled_at: Option<u64>,
    #[serde(default)]
    last_connected_at: Option<u64>,
    #[serde(default)]
    push: Option<PushRegistration>,
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
        // The roster holds every pair's PSK; tighten a loosened file on load.
        ensure_secret_mode(path).map_err(|e| IdentityError::io(path, e))?;
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
                relay_token: e.relay_token,
                name: e.name,
                enrolled_at: e.enrolled_at,
                last_connected_at: e.last_connected_at,
                push: e.push,
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
                    relay_token: e.relay_token.clone(),
                    name: e.name.clone(),
                    enrolled_at: e.enrolled_at,
                    last_connected_at: e.last_connected_at,
                    push: e.push.clone(),
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

    /// Mutable entry for `id`, if paired (for stamping `last_connected_at`).
    pub fn find_by_device_mut(&mut self, id: &DeviceId) -> Option<&mut RosterEntry> {
        self.entries.iter_mut().find(|e| &e.device_id == id)
    }

    /// Whether `id` is currently paired.
    pub fn contains_device(&self, id: &DeviceId) -> bool {
        self.entries.iter().any(|e| &e.device_id == id)
    }

    /// Removes the entry for `id`; returns whether one was removed.
    pub fn remove_by_device(&mut self, id: &DeviceId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| &e.device_id != id);
        self.entries.len() != before
    }
}

// ---------------------------------------------------------------------------
// PairingFile
// ---------------------------------------------------------------------------

/// The client-side pairing bundle — the QR payload minus the QR (spec D4).
///
/// Everything a freshly provisioned device needs to reach this bridge through
/// the relay and complete the Noise handshake: the relay endpoint + device
/// token, the bridge's id and static public key (to pin), the shared PSK, and
/// the device's own minted id + keypair.
///
/// Secret-bearing (#278): `device_token`, `psk`, and `device_private_key` are
/// zeroized when a file value (or any clone) drops, and all three are redacted
/// from the manual [`Debug`] impl.
#[derive(Clone, PartialEq, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
pub struct PairingFile {
    /// The relay endpoint (`wss://…`) to dial.
    pub relay_url: String,
    /// The device's routing credential, asserted by the bridge.
    pub device_token: String,
    /// The bridge's device id.
    #[zeroize(skip)]
    pub bridge_id: DeviceId,
    /// The bridge's static public key (base64) — the device pins this.
    pub bridge_static_pubkey: String,
    /// The shared `psk2` for this pairing (base64).
    pub psk: String,
    /// The device's minted device id.
    #[zeroize(skip)]
    pub device_id: DeviceId,
    /// The device's static private key (base64).
    pub device_private_key: String,
    /// The device's static public key (base64).
    pub device_public_key: String,
}

impl std::fmt::Debug for PairingFile {
    /// Redacts `device_token` (relay bearer credential), `psk` (session PSK),
    /// and `device_private_key`; endpoints, ids, and public keys stay visible
    /// for diagnostics (#278).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingFile")
            .field("relay_url", &self.relay_url)
            .field("device_token", &"[redacted]")
            .field("bridge_id", &self.bridge_id)
            .field("bridge_static_pubkey", &self.bridge_static_pubkey)
            .field("psk", &"[redacted]")
            .field("device_id", &self.device_id)
            .field("device_private_key", &"[redacted]")
            .field("device_public_key", &self.device_public_key)
            .finish()
    }
}

impl PairingFile {
    /// Loads a pairing file from `path` (JSON).
    pub fn load(path: &Path) -> Result<PairingFile, IdentityError> {
        let text = std::fs::read_to_string(path).map_err(|e| IdentityError::io(path, e))?;
        // The pairing file carries the device private key and PSK; tighten a
        // loosened file on load.
        ensure_secret_mode(path).map_err(|e| IdentityError::io(path, e))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Persists this pairing file to `path` as `0600` JSON (it carries the
    /// device's private key).
    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        let text = serde_json::to_string_pretty(self)?;
        write_secret_file(path, &text)
    }

    /// Deletes this pairing file — the device-side unpinning flow (ADR-0021
    /// D6): a client's entire trust state is this file, so removing it drops
    /// the bridge from the device's trust set.
    pub fn delete(path: &Path) -> Result<(), IdentityError> {
        std::fs::remove_file(path).map_err(|e| IdentityError::io(path, e))
    }
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

    /// A helper that lists any leftover `.tmp.` scratch files in a directory.
    fn temp_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp."))
            .collect()
    }

    #[test]
    fn write_secret_file_is_atomic_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.toml");

        // A successful create renames its temp over the target, leaving none.
        BridgeIdentity::load_or_create(&path).expect("create");
        assert_eq!(file_mode(&path), 0o600, "written secret must be 0600");
        assert!(
            temp_files(dir.path()).is_empty(),
            "a successful write must leave no temp file: {:?}",
            temp_files(dir.path())
        );

        // A rewrite (roster save into the same dir) is likewise clean.
        let roster_path = dir.path().join("roster.toml");
        Roster::default().save(&roster_path).expect("save roster");
        Roster::default()
            .save(&roster_path)
            .expect("overwrite roster");
        assert!(
            temp_files(dir.path()).is_empty(),
            "an overwrite must leave no temp file: {:?}",
            temp_files(dir.path())
        );
    }

    #[test]
    fn load_tightens_a_permissive_identity_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("identity.toml");

        // Create a valid identity, then loosen its mode as if a bad umask or an
        // older build had written it 0644.
        BridgeIdentity::load_or_create(&path).expect("create");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen mode");
        assert_eq!(file_mode(&path), 0o644);

        // Loading it back must tighten the mode to 0600 (and still parse).
        BridgeIdentity::load_or_create(&path).expect("reload");
        assert_eq!(
            file_mode(&path),
            0o600,
            "loading a permissive identity file must tighten it to 0600"
        );
    }

    #[test]
    fn load_tightens_a_permissive_pairing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pairing_path = dir.path().join("pairing.json");

        let pairing = PairingFile {
            relay_url: "wss://r/ws".to_string(),
            device_token: "tok".to_string(),
            bridge_id: DeviceId([3; 32]),
            bridge_static_pubkey: B64.encode([0u8; 32]),
            psk: B64.encode([0u8; 32]),
            device_id: DeviceId([4; 32]),
            device_private_key: B64.encode([0u8; 32]),
            device_public_key: B64.encode([0u8; 32]),
        };
        pairing.save(&pairing_path).expect("save pairing");

        std::fs::set_permissions(&pairing_path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen mode");
        PairingFile::load(&pairing_path).expect("reload pairing");
        assert_eq!(
            file_mode(&pairing_path),
            0o600,
            "loading a permissive pairing file must tighten it to 0600"
        );
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
                    name: "phone".to_string(),
                    enrolled_at: Some(1_765_000_000),
                    last_connected_at: Some(1_765_100_000),
                    relay_token: "relay-tok-1".to_string(),
                    push: Some(PushRegistration::UnifiedPush {
                        endpoint: "https://ntfy.sh/topic".to_string(),
                    }),
                },
                RosterEntry {
                    device_id: DeviceId([0x22; 32]),
                    static_pubkey: vec![0xcc; 32],
                    psk: [0xdd; 32],
                    name: "laptop".to_string(),
                    enrolled_at: None,
                    last_connected_at: None,
                    relay_token: "relay-tok-2".to_string(),
                    push: None,
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
        assert_eq!(found.push, None);
        let with_push = loaded
            .find_by_device(&DeviceId([0x11; 32]))
            .expect("find first device");
        assert_eq!(
            with_push.push,
            Some(PushRegistration::UnifiedPush {
                endpoint: "https://ntfy.sh/topic".to_string(),
            })
        );
        assert!(loaded.find_by_device(&DeviceId([0x99; 32])).is_none());
    }

    #[test]
    fn roster_entry_without_push_field_loads_as_none() {
        // Old on-disk roster files predate `push`; `#[serde(default)]` on
        // `RosterEntryFile::push` means a hand-written entry missing the
        // field still loads, with the device treated as unregistered for
        // push (not a corrupt-roster error).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");
        let text = format!(
            "[[entry]]\ndevice_id = \"{}\"\nstatic_pubkey = \"{}\"\npsk = \"{}\"\nrelay_token = \"tok-old\"\nname = \"oldphone\"\n",
            DeviceId([0x33; 32]),
            B64.encode([0xaa; 32]),
            B64.encode([0xbb; 32]),
        );
        std::fs::write(&path, text).expect("write old-format roster");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("set mode");

        let loaded = Roster::load(&path).expect("load old-format roster");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].push, None);
        assert_eq!(loaded.entries[0].name, "oldphone");
    }

    #[test]
    fn fingerprint_is_stable_grouped_hex() {
        let fp = fingerprint(&[0xab; 32]);
        // 6 bytes -> 12 hex chars in 3 groups of 4, uppercase.
        assert_eq!(fp.len(), 14, "XXXX-XXXX-XXXX");
        assert_eq!(fp.matches('-').count(), 2);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase() || c == '-'));
        assert_eq!(fp, fingerprint(&[0xab; 32]), "deterministic");
        assert_ne!(fp, fingerprint(&[0xac; 32]), "distinguishes keys");
    }

    #[test]
    fn roster_entry_persists_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roster.toml");
        let roster = Roster {
            entries: vec![RosterEntry {
                device_id: DeviceId([0x11; 32]),
                static_pubkey: vec![0xaa; 32],
                psk: [0xbb; 32],
                name: "iPhone".to_string(),
                enrolled_at: Some(1_765_500_000),
                last_connected_at: None,
                relay_token: "tok-abc".to_string(),
                push: None,
            }],
        };
        roster.save(&path).expect("save");
        let loaded = Roster::load(&path).expect("load");
        assert_eq!(loaded, roster);
        assert_eq!(loaded.entries[0].name, "iPhone");
    }

    #[test]
    fn roster_remove_by_device() {
        let mut roster = Roster {
            entries: vec![RosterEntry {
                device_id: DeviceId([0x11; 32]),
                static_pubkey: vec![0xaa; 32],
                psk: [0xbb; 32],
                name: "a".to_string(),
                enrolled_at: None,
                last_connected_at: None,
                relay_token: "t".to_string(),
                push: None,
            }],
        };
        assert!(roster.contains_device(&DeviceId([0x11; 32])));
        assert!(roster.remove_by_device(&DeviceId([0x11; 32])));
        assert!(!roster.contains_device(&DeviceId([0x11; 32])));
        assert!(
            !roster.remove_by_device(&DeviceId([0x11; 32])),
            "second remove is a no-op"
        );
    }

    #[test]
    fn identity_lock_excludes_second_claimant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bridge_identity.toml");
        let first = IdentityLock::acquire(&path).expect("first lock");
        // flock is per open-file-description: a second open in the SAME
        // process conflicts too, so this asserts the real exclusion.
        let second = IdentityLock::acquire(&path);
        assert!(matches!(second, Err(IdentityError::Locked { .. })));
        drop(first);
        IdentityLock::acquire(&path).expect("lock re-acquirable after release");
    }

    #[test]
    fn roster_entry_debug_redacts_psk_and_relay_token() {
        let entry = RosterEntry {
            device_id: DeviceId([0x11; 32]),
            static_pubkey: vec![0xaa; 32],
            // 0xED = 237: a decimal rendering distinct from other fields.
            psk: [0xED; 32],
            relay_token: "relay-SECRET-token".to_string(),
            name: "phone".to_string(),
            enrolled_at: Some(1_765_000_000),
            last_connected_at: None,
            push: None,
        };
        let dbg = format!("{entry:?}");
        assert!(!dbg.contains("237"), "psk bytes leaked: {dbg}");
        assert!(!dbg.contains("relay-SECRET-token"), "token leaked: {dbg}");
        assert!(dbg.contains("[redacted]"), "no redaction marker: {dbg}");
        assert!(dbg.contains("phone"), "name should stay visible: {dbg}");

        // A whole roster delegates to the entry's redacting impl.
        let roster = Roster {
            entries: vec![entry],
        };
        let dbg = format!("{roster:?}");
        assert!(!dbg.contains("relay-SECRET-token"), "leaked: {dbg}");
    }

    #[test]
    fn pairing_file_debug_redacts_secrets() {
        let pf = PairingFile {
            relay_url: "wss://r/ws".to_string(),
            device_token: "tok-SECRET".to_string(),
            bridge_id: DeviceId([3; 32]),
            bridge_static_pubkey: B64.encode([0x22; 32]),
            psk: B64.encode([0xED; 32]),
            device_id: DeviceId([4; 32]),
            device_private_key: B64.encode([0xEE; 32]),
            device_public_key: B64.encode([0x44; 32]),
        };
        let dbg = format!("{pf:?}");
        assert!(!dbg.contains("tok-SECRET"), "device token leaked: {dbg}");
        assert!(
            !dbg.contains(&B64.encode([0xED; 32])),
            "psk b64 leaked: {dbg}"
        );
        assert!(
            !dbg.contains(&B64.encode([0xEE; 32])),
            "private key b64 leaked: {dbg}"
        );
        assert!(dbg.contains("[redacted]"), "no redaction marker: {dbg}");
        assert!(dbg.contains("wss://r/ws"), "relay_url should stay: {dbg}");
        assert!(
            dbg.contains(&B64.encode([0x44; 32])),
            "public key should stay visible: {dbg}"
        );
    }

    #[test]
    fn zeroize_clears_secret_fields_and_keeps_skipped_ids() {
        // Calling `zeroize()` directly exercises the exact same derive the
        // drop path uses, without reading freed memory: the secret fields must
        // clear, the `#[zeroize(skip)]` ids must survive.
        let mut entry = RosterEntry {
            device_id: DeviceId([0x11; 32]),
            static_pubkey: vec![0xaa; 32],
            psk: [0xbb; 32],
            relay_token: "relay-tok".to_string(),
            name: "phone".to_string(),
            enrolled_at: Some(1),
            last_connected_at: None,
            push: None,
        };
        entry.zeroize();
        assert_eq!(entry.psk, [0u8; 32], "psk must be wiped");
        assert!(entry.relay_token.is_empty(), "relay_token must be wiped");
        assert_eq!(
            entry.device_id,
            DeviceId([0x11; 32]),
            "skipped device_id must survive"
        );

        let mut pf = PairingFile {
            relay_url: "wss://r/ws".to_string(),
            device_token: "tok".to_string(),
            bridge_id: DeviceId([3; 32]),
            bridge_static_pubkey: B64.encode([0u8; 32]),
            psk: B64.encode([0x5a; 32]),
            device_id: DeviceId([4; 32]),
            device_private_key: B64.encode([0x5b; 32]),
            device_public_key: B64.encode([0u8; 32]),
        };
        pf.zeroize();
        assert!(pf.psk.is_empty(), "psk must be wiped");
        assert!(pf.device_token.is_empty(), "device_token must be wiped");
        assert!(
            pf.device_private_key.is_empty(),
            "device_private_key must be wiped"
        );
        assert_eq!(pf.bridge_id, DeviceId([3; 32]), "skipped id must survive");
    }

    #[test]
    fn pairing_file_delete_removes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pairing.json");
        let pf = PairingFile {
            relay_url: "wss://r/ws".to_string(),
            device_token: "dt".to_string(),
            bridge_id: DeviceId([1; 32]),
            bridge_static_pubkey: B64.encode([0u8; 32]),
            psk: B64.encode([0u8; 32]),
            device_id: DeviceId([2; 32]),
            device_private_key: B64.encode([0u8; 32]),
            device_public_key: B64.encode([0u8; 32]),
        };
        pf.save(&path).expect("save");
        assert!(path.exists());
        PairingFile::delete(&path).expect("delete");
        assert!(!path.exists());
    }
}
