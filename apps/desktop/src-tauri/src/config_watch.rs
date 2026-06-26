//! Per-device config file watcher.
//!
//! Watches the config file's *parent directory* (atomic-rename safe), debounces
//! bursty editor writes, and invokes a callback when the config file itself
//! changes. The Tauri-specific emit is injected (see `watch_config`), so the
//! watch wiring is testable without an `AppHandle`.
//!
//! ```text
//! config.toml saved → debouncer (parent dir) → event_concerns_config filter
//!   → on_change() → (production) emit ConfigChanged → frontend refresh
//! ```

use std::path::{Path, PathBuf};

/// True if any path in this debounced batch is the config file itself. We watch
/// the parent dir, so unrelated sibling writes also arrive here and must be
/// filtered out. Full-path equality is exact for our single-file watch.
fn event_concerns_config(paths: &[PathBuf], config_path: &Path) -> bool {
    paths.iter().any(|p| p == config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_matches_the_config_path() {
        let cfg = PathBuf::from("/cfg/remora/config.toml");
        let paths = vec![PathBuf::from("/cfg/remora/config.toml")];
        assert!(event_concerns_config(&paths, &cfg));
    }

    #[test]
    fn predicate_ignores_sibling_files() {
        let cfg = PathBuf::from("/cfg/remora/config.toml");
        let paths = vec![PathBuf::from("/cfg/remora/other.toml")];
        assert!(!event_concerns_config(&paths, &cfg));
    }

    #[test]
    fn predicate_ignores_an_empty_batch() {
        let cfg = PathBuf::from("/cfg/remora/config.toml");
        assert!(!event_concerns_config(&[], &cfg));
    }
}
