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
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::new_debouncer;

/// True if any path in this debounced batch is the config file itself. We watch
/// the parent dir, so unrelated sibling writes also arrive here and must be
/// filtered out. Full-path equality is exact for our single-file watch.
fn event_concerns_config(paths: &[PathBuf], config_path: &Path) -> bool {
    paths.iter().any(|p| p == config_path)
}

/// Watch `config_path`'s parent dir and invoke `on_change` (debounced by
/// `debounce`) whenever the config file itself changes. Creates the parent dir
/// if absent (benign — the file stays absent until written) so the watch can
/// attach on a fresh device. A dedicated OS thread owns the debouncer for the
/// app's lifetime; on app exit the process tears the thread down.
pub fn watch_config(
    config_path: &Path,
    debounce: Duration,
    on_change: impl Fn() + Send + 'static,
) -> notify::Result<()> {
    let config_path = config_path.to_path_buf();
    let parent = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Benign: ensures the watch target exists; the config file itself is not
    // created (ADR-0004: a missing file is a valid empty config).
    let _ = std::fs::create_dir_all(&parent);

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(debounce, None, tx)?;
    debouncer.watch(&parent, RecursiveMode::NonRecursive)?;

    std::thread::spawn(move || {
        // Keep the debouncer alive for as long as the thread (app) lives.
        let _debouncer = debouncer;
        for result in rx {
            let Ok(events) = result else { continue };
            if events
                .iter()
                .any(|e| event_concerns_config(&e.paths, &config_path))
            {
                on_change();
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Unique temp dir per test so concurrent `cargo test` can't collide
    /// (matches the `remora-config-test-{pid}` convention used elsewhere).
    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("remora-watch-{tag}-{}", std::process::id()))
    }

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

    #[test]
    fn predicate_matches_config_among_siblings() {
        let cfg = PathBuf::from("/cfg/remora/config.toml");
        let paths = vec![
            PathBuf::from("/cfg/remora/other.toml"),
            PathBuf::from("/cfg/remora/config.toml"),
        ];
        assert!(event_concerns_config(&paths, &cfg));
    }

    #[test]
    fn fires_on_config_write() {
        let dir = temp_dir("write");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cfg = dir.join("config.toml");

        let (tx, rx) = mpsc::channel();
        watch_config(&cfg, Duration::from_millis(20), move || {
            let _ = tx.send(());
        })
        .expect("watcher starts");

        // Let inotify (or the platform equivalent) attach before we write.
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&cfg, "hosts = {}\n").expect("write config");

        let got = rx.recv_timeout(Duration::from_secs(5));
        std::fs::remove_dir_all(&dir).ok();
        assert!(got.is_ok(), "expected a debounced change event");
    }

    #[test]
    fn creates_a_missing_parent_dir() {
        let base = temp_dir("mkdir");
        std::fs::remove_dir_all(&base).ok();
        let cfg = base.join("remora").join("config.toml");

        let res = watch_config(&cfg, Duration::from_millis(50), || {});
        let parent_exists = cfg.parent().map(Path::is_dir).unwrap_or(false);
        std::fs::remove_dir_all(&base).ok();

        assert!(
            res.is_ok(),
            "watcher should start even if parent dir was absent"
        );
        assert!(parent_exists, "watch_config should create the parent dir");
    }
}
