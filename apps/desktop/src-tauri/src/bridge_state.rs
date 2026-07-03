//! On-disk layout for this device's bridge state (ADR-0021).
//!
//! The bridge's durable identity and its paired-device roster live alongside
//! `config.toml` in the app config dir (`…/remora/`). This layout is
//! load-bearing: both the dev-only relay loopback ([`crate::remote_host`]) and
//! the real relay bridge ([`crate::relay`]) resolve their identity/roster
//! through the *same* helpers so the two paths cannot drift.

use std::path::{Path, PathBuf};

/// The bridge's stable identity file (device id + static keypair).
const IDENTITY_FILE: &str = "bridge_identity.toml";
/// The paired-device roster file (pinned keys + per-pair PSKs).
const ROSTER_FILE: &str = "bridge_roster.toml";

/// The directory holding bridge state: the parent of `config.toml`
/// (`…/remora/`). Falls back to the current directory if the config path is
/// somehow parent-less, matching the loopback's original behavior.
pub(crate) fn state_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Path to the bridge identity file, derived from the config path.
pub(crate) fn identity_path(config_path: &Path) -> PathBuf {
    state_dir(config_path).join(IDENTITY_FILE)
}

/// Path to the bridge roster file, derived from the config path.
pub(crate) fn roster_path(config_path: &Path) -> PathBuf {
    state_dir(config_path).join(ROSTER_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_files_sit_beside_config() {
        let config = PathBuf::from("/home/dev/.config/remora/config.toml");
        let dir = PathBuf::from("/home/dev/.config/remora");
        assert_eq!(state_dir(&config), dir);
        assert_eq!(identity_path(&config), dir.join("bridge_identity.toml"));
        assert_eq!(roster_path(&config), dir.join("bridge_roster.toml"));
    }

    #[test]
    fn parentless_config_falls_back_to_current_dir() {
        // The root path is the case with a genuinely absent parent; the fallback
        // keeps state beside the current dir rather than panicking.
        let config = PathBuf::from("/");
        assert_eq!(state_dir(&config), PathBuf::from("."));
    }
}
