//! Transport-agnostic launch primitives shared by the external-terminal and
//! VS Code launchers: absolute-path binary resolution (against launchd's bare
//! PATH — a Dock/Finder-launched app inherits no shell PATH) and detached
//! spawn. Editor/terminal specifics live in their own modules.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Directories probed (in order) for any binary resolved here. `PATH` entries
/// are appended last: useful in dev (terminal-launched, full PATH), empty in a
/// packaged GUI launch.
const BIN_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// Filesystem seam so resolution is unit-testable without an OS.
pub trait PathProbe {
    fn is_file(&self, path: &Path) -> bool;
}

/// Production probe: plain filesystem checks.
pub struct RealProbe;

impl PathProbe for RealProbe {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// Environment inputs to candidate enumeration (`$HOME`, `$PATH`), injectable
/// so tests can pin env-dependent ordering deterministically.
pub(crate) struct SearchEnv {
    pub home: Option<OsString>,
    pub path: Option<OsString>,
}

impl SearchEnv {
    pub(crate) fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            path: std::env::var_os("PATH"),
        }
    }
}

/// Candidate paths for a base name: `BIN_DIRS`, `~/.local/bin`, then every
/// `PATH` entry. A handful of stat calls — callers run it fresh every time.
fn candidate_paths(bin: &str, env: &SearchEnv) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = BIN_DIRS.iter().map(|d| Path::new(d).join(bin)).collect();
    if let Some(home) = &env.home {
        paths.push(Path::new(home).join(".local/bin").join(bin));
    }
    if let Some(path_var) = &env.path {
        paths.extend(std::env::split_paths(path_var).map(|d| d.join(bin)));
    }
    paths
}

/// Env-injectable resolution: first existing candidate in `BIN_DIRS` + PATH,
/// then the absolute `extra` fallbacks (e.g. macOS app bundles).
pub(crate) fn resolve_binary_with(
    bin: &str,
    extra: &[&str],
    probe: &dyn PathProbe,
    env: &SearchEnv,
) -> Option<PathBuf> {
    candidate_paths(bin, env)
        .into_iter()
        .chain(extra.iter().map(PathBuf::from))
        .find(|p| probe.is_file(p))
}

/// Resolve `bin` to an absolute path against the real process env, falling back
/// to the absolute `extra` candidates. `None` if nothing exists.
pub fn resolve_binary(bin: &str, extra: &[&str], probe: &dyn PathProbe) -> Option<PathBuf> {
    resolve_binary_with(bin, extra, probe, &SearchEnv::from_process())
}

/// Spawn a program detached: null stdio, own process group (unix) so it
/// outlives Remora. The caller keeps the `Child` briefly for the early-exit
/// check, then reaps it.
pub fn spawn_detached(argv: &[String]) -> std::io::Result<Child> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct FakeProbe(BTreeSet<&'static str>);
    impl PathProbe for FakeProbe {
        fn is_file(&self, path: &Path) -> bool {
            self.0.contains(path.to_str().unwrap_or_default())
        }
    }

    #[test]
    fn resolve_binary_prefers_bin_dirs_then_extra_fallback() {
        let env = SearchEnv {
            home: None,
            path: Some("/fake/path-dir".into()),
        };
        // Present only as an extra (absolute) candidate.
        let probe = FakeProbe(BTreeSet::from(["/Applications/Foo.app/bin/foo"]));
        let hit = resolve_binary_with("foo", &["/Applications/Foo.app/bin/foo"], &probe, &env);
        assert_eq!(
            hit.as_deref(),
            Some(Path::new("/Applications/Foo.app/bin/foo"))
        );
        // A BIN_DIRS hit wins over the extra fallback.
        let probe2 = FakeProbe(BTreeSet::from([
            "/usr/local/bin/foo",
            "/Applications/Foo.app/bin/foo",
        ]));
        let hit2 = resolve_binary_with("foo", &["/Applications/Foo.app/bin/foo"], &probe2, &env);
        assert_eq!(hit2.as_deref(), Some(Path::new("/usr/local/bin/foo")));
        // Nothing anywhere -> None.
        assert_eq!(
            resolve_binary_with("foo", &[], &FakeProbe(BTreeSet::new()), &env),
            None
        );
    }
}
