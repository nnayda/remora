//! External-terminal launch: registry, detection, resolution, spawn.
//!
//! Lives in the desktop shell by design (spec decision 5): core never learns
//! about local GUI apps, the frontend never sees an argv. Everything here is
//! keyed by ABSOLUTE candidate paths because a GUI-launched (Dock/Finder)
//! app inherits launchd's bare PATH — `which ghostty` finds nothing for a
//! Homebrew install even though the user's shell finds it (eng review D4;
//! the same launchd-PATH class for the transport binary is D8, and the
//! embedded kubectl transport's own copy of this bug is #229).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use remora_core::config::TerminalPreference;

/// One registry entry: how to find and invoke a terminal.
struct TerminalSpec {
    id: &'static str,
    name: &'static str,
    /// Binary base name, probed inside [`BIN_DIRS`] and on `PATH`.
    bin: &'static str,
    /// Args between the binary and the attach argv (`-e`, `start --`, or
    /// nothing for positional-command terminals).
    exec_args: &'static [&'static str],
    /// Absolute non-BIN_DIRS fallbacks (macOS app bundles).
    extra_candidates: &'static [&'static str],
}

/// The argv-capable v1 registry (spec decision 3). Terminal.app/iTerm2
/// (AppleScript) and Windows are follow-up issues.
const REGISTRY: &[TerminalSpec] = &[
    TerminalSpec {
        id: "ghostty",
        name: "Ghostty",
        bin: "ghostty",
        exec_args: &["-e"],
        extra_candidates: &["/Applications/Ghostty.app/Contents/MacOS/ghostty"],
    },
    TerminalSpec {
        id: "kitty",
        name: "kitty",
        bin: "kitty",
        exec_args: &[],
        extra_candidates: &["/Applications/kitty.app/Contents/MacOS/kitty"],
    },
    TerminalSpec {
        id: "alacritty",
        name: "Alacritty",
        bin: "alacritty",
        exec_args: &["-e"],
        extra_candidates: &["/Applications/Alacritty.app/Contents/MacOS/alacritty"],
    },
    TerminalSpec {
        id: "wezterm",
        name: "WezTerm",
        bin: "wezterm",
        exec_args: &["start", "--"],
        extra_candidates: &["/Applications/WezTerm.app/Contents/MacOS/wezterm"],
    },
    TerminalSpec {
        id: "foot",
        name: "foot",
        bin: "foot",
        exec_args: &[],
        extra_candidates: &[],
    },
];

/// Directories probed (in order) for any binary this module resolves —
/// terminals AND the transport binary (ssh/kubectl). `PATH` entries are
/// appended last: useful in dev (terminal-launched, full PATH), empty-handed
/// in a packaged GUI launch.
const BIN_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// Filesystem seam so detection/assembly are unit-testable without an OS.
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

/// A terminal found on this machine, with the ABSOLUTE path detection hit —
/// which is exactly what launch executes (never a bare name).
#[derive(Debug, Clone)]
pub struct DetectedTerminal {
    pub id: &'static str,
    pub name: &'static str,
    pub path: PathBuf,
}

/// Environment inputs to candidate enumeration (`$HOME`, `$PATH`),
/// injectable so tests can pin env-dependent ordering deterministically —
/// the same seam philosophy as [`PathProbe`]. The pub API always reads the
/// real process env via [`SearchEnv::from_process`].
struct SearchEnv {
    home: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
}

impl SearchEnv {
    fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            path: std::env::var_os("PATH"),
        }
    }
}

/// Candidate paths for a base name: `BIN_DIRS`, `~/.local/bin`, then every
/// `PATH` entry. Probing is a handful of stat calls — microseconds — so
/// callers run it fresh every time (no cache, no staleness).
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

fn resolve_binary(
    bin: &str,
    extra: &[&'static str],
    probe: &dyn PathProbe,
    env: &SearchEnv,
) -> Option<PathBuf> {
    candidate_paths(bin, env)
        .into_iter()
        .chain(extra.iter().map(PathBuf::from))
        .find(|p| probe.is_file(p))
}

fn detect_terminals_with(probe: &dyn PathProbe, env: &SearchEnv) -> Vec<DetectedTerminal> {
    REGISTRY
        .iter()
        .filter_map(|spec| {
            resolve_binary(spec.bin, spec.extra_candidates, probe, env).map(|path| {
                DetectedTerminal {
                    id: spec.id,
                    name: spec.name,
                    path,
                }
            })
        })
        .collect()
}

/// Every registry terminal present on this machine, in registry order.
pub fn detect_terminals(probe: &dyn PathProbe) -> Vec<DetectedTerminal> {
    detect_terminals_with(probe, &SearchEnv::from_process())
}

/// Resolve the transport binary (attach argv\[0\]: `ssh`/`kubectl`) to an
/// absolute path — same launchd-PATH failure class as the terminals (D8).
pub fn resolve_transport_binary(bin: &str, probe: &dyn PathProbe) -> Option<PathBuf> {
    resolve_binary(bin, &[], probe, &SearchEnv::from_process())
}

/// Why a launch could not even be attempted. `NotConfigured` deep-links
/// Settings in the UI; the others surface as errors naming the culprit.
#[derive(Debug)]
pub enum ResolveError {
    NotConfigured(String),
    UnknownId(String),
    NotDetected(String),
}

/// The terminal-side prefix of the final argv: absolute program + exec args
/// (registry), or the custom argv exactly as configured.
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub argv: Vec<String>,
}

fn registry_plan(id: &str, detected: &[DetectedTerminal]) -> Result<LaunchPlan, ResolveError> {
    let spec = REGISTRY
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ResolveError::UnknownId(format!("unknown terminal id `{id}`")))?;
    let hit = detected
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| ResolveError::NotDetected(format!("terminal `{id}` is not installed")))?;
    let mut argv = vec![hit.path.to_string_lossy().into_owned()];
    argv.extend(spec.exec_args.iter().map(|s| (*s).to_string()));
    Ok(LaunchPlan { argv })
}

/// Precedence (spec §3): explicit id > config key > single detected >
/// NotConfigured. Ambiguity (2+ detected, nothing configured) is
/// NotConfigured too — the UI's cue to deep-link Settings.
pub fn resolve_terminal(
    explicit: Option<&str>,
    pref: Option<&TerminalPreference>,
    detected: &[DetectedTerminal],
) -> Result<LaunchPlan, ResolveError> {
    if let Some(id) = explicit {
        return registry_plan(id, detected);
    }
    match pref {
        Some(TerminalPreference::Registry(id)) => registry_plan(id, detected),
        Some(TerminalPreference::Custom(argv)) => Ok(LaunchPlan { argv: argv.clone() }),
        None => match detected {
            [only] => registry_plan(only.id, detected),
            [] => Err(ResolveError::NotConfigured(
                "no external terminal configured and none detected".into(),
            )),
            _ => Err(ResolveError::NotConfigured(
                "no external terminal configured; several detected — pick one in Settings".into(),
            )),
        },
    }
}

/// Final argv: terminal prefix + attach argv with its first token resolved
/// to an absolute path. A custom-argv prefix is trusted as-written (the
/// user's own config), but the transport binary is still resolved — a
/// flash-closing "command not found" window helps nobody.
pub fn assemble_launch(
    plan: &LaunchPlan,
    attach_argv: &[String],
    probe: &dyn PathProbe,
) -> Result<Vec<String>, ResolveError> {
    let (transport, rest) = attach_argv
        .split_first()
        .ok_or_else(|| ResolveError::NotDetected("empty attach command".into()))?;
    let resolved = resolve_transport_binary(transport, probe).ok_or_else(|| {
        ResolveError::NotDetected(format!(
            "`{transport}` not found in standard locations — required to attach"
        ))
    })?;
    let mut argv = plan.argv.clone();
    argv.push(resolved.to_string_lossy().into_owned());
    argv.extend(rest.iter().cloned());
    Ok(argv)
}

/// POSIX single-quote quoting for the Copy-attach-command string. Only
/// DISPLAYED/pasted — never executed by Remora (the launch path passes argv
/// directly, no shell).
pub fn shell_quote_command(argv: &[String]) -> String {
    fn quote(token: &str) -> String {
        // `~` is deliberately plain: this string is display-only, and a
        // pasted `~/...` must stay unquoted for the shell to expand it.
        let plain = !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,~".contains(c));
        if plain {
            token.to_string()
        } else {
            format!("'{}'", token.replace('\'', r"'\''"))
        }
    }
    argv.iter().map(|t| quote(t)).collect::<Vec<_>>().join(" ")
}

/// Spawn the terminal detached: null stdio, own process group (unix) so it
/// outlives Remora. The caller keeps the `Child` briefly for the early-exit
/// check (D9), then drops it.
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
    use std::path::Path;

    /// Probe that "has" exactly the given absolute paths.
    struct FakeProbe(BTreeSet<&'static str>);
    impl PathProbe for FakeProbe {
        fn is_file(&self, path: &Path) -> bool {
            self.0.contains(path.to_str().unwrap_or_default())
        }
    }

    #[test]
    fn detection_probes_candidates_in_order_and_returns_resolved_paths() {
        // Fixed env (no real $HOME/$PATH) so ordering is deterministic.
        let env = SearchEnv {
            home: None,
            path: Some("/fake/path-dir".into()),
        };
        let probe = FakeProbe(BTreeSet::from([
            // ghostty in TWO BIN_DIRS at once — the first listed must win.
            "/opt/homebrew/bin/ghostty",
            "/usr/local/bin/ghostty",
            // kitty only as the bundle fallback (probed after BIN_DIRS+PATH).
            "/Applications/kitty.app/Contents/MacOS/kitty",
            // foot findable ONLY via a $PATH entry.
            "/fake/path-dir/foot",
            // wezterm in a BIN_DIRS dir AND on $PATH — BIN_DIRS wins (PATH last).
            "/usr/bin/wezterm",
            "/fake/path-dir/wezterm",
        ]));
        let detected = detect_terminals_with(&probe, &env);
        let ghostty = detected
            .iter()
            .find(|t| t.id == "ghostty")
            .expect("ghostty");
        assert_eq!(
            ghostty.path,
            Path::new("/opt/homebrew/bin/ghostty"),
            "first BIN_DIRS hit wins over a later one"
        );
        let kitty = detected.iter().find(|t| t.id == "kitty").expect("kitty");
        assert_eq!(
            kitty.path,
            Path::new("/Applications/kitty.app/Contents/MacOS/kitty")
        );
        let foot = detected.iter().find(|t| t.id == "foot").expect("foot");
        assert_eq!(
            foot.path,
            Path::new("/fake/path-dir/foot"),
            "a $PATH-only install is still found"
        );
        let wezterm = detected
            .iter()
            .find(|t| t.id == "wezterm")
            .expect("wezterm");
        assert_eq!(
            wezterm.path,
            Path::new("/usr/bin/wezterm"),
            "BIN_DIRS beats $PATH — PATH entries are probed last"
        );
        assert!(
            !detected.iter().any(|t| t.id == "alacritty"),
            "not installed"
        );
    }

    #[test]
    fn resolver_precedence_explicit_then_config_then_single_detected() {
        let ghostty = DetectedTerminal {
            id: "ghostty",
            name: "Ghostty",
            path: "/opt/homebrew/bin/ghostty".into(),
        };
        let kitty = DetectedTerminal {
            id: "kitty",
            name: "kitty",
            path: "/usr/local/bin/kitty".into(),
        };
        use remora_core::config::TerminalPreference as Pref;

        // 1. explicit beats config
        let plan = resolve_terminal(
            Some("kitty"),
            Some(&Pref::Registry("ghostty".into())),
            &[ghostty.clone(), kitty.clone()],
        )
        .expect("explicit");
        assert_eq!(plan.argv, ["/usr/local/bin/kitty"]);

        // 2. config registry id
        let plan = resolve_terminal(
            None,
            Some(&Pref::Registry("ghostty".into())),
            &[ghostty.clone(), kitty.clone()],
        )
        .expect("config");
        assert_eq!(plan.argv, ["/opt/homebrew/bin/ghostty", "-e"]);

        // 3. config custom argv used as-written (no resolution)
        let plan = resolve_terminal(
            None,
            Some(&Pref::Custom(vec!["my-term".into(), "-e".into()])),
            &[],
        )
        .expect("custom");
        assert_eq!(plan.argv, ["my-term", "-e"]);

        // 4. single detected wins with nothing configured
        let plan = resolve_terminal(None, None, std::slice::from_ref(&ghostty)).expect("single");
        assert_eq!(plan.argv, ["/opt/homebrew/bin/ghostty", "-e"]);

        // 5. zero or ambiguous detected -> NotConfigured
        assert!(matches!(
            resolve_terminal(None, None, &[]),
            Err(ResolveError::NotConfigured(_))
        ));
        assert!(matches!(
            resolve_terminal(None, None, &[ghostty.clone(), kitty.clone()]),
            Err(ResolveError::NotConfigured(_))
        ));

        // 6. unknown id names the id
        let err = resolve_terminal(None, Some(&Pref::Registry("st".into())), &[ghostty])
            .expect_err("unknown");
        match err {
            ResolveError::UnknownId(msg) => assert!(msg.contains("st"), "{msg}"),
            other => panic!("expected UnknownId, got {other:?}"),
        }
    }

    #[test]
    fn assemble_resolves_transport_binary_and_concatenates_exec_styles() {
        let probe = FakeProbe(BTreeSet::from(["/opt/homebrew/bin/kubectl"]));
        // -e style
        let plan = LaunchPlan {
            argv: vec!["/opt/homebrew/bin/ghostty".into(), "-e".into()],
        };
        let attach = vec!["kubectl".to_string(), "exec".into(), "-i".into()];
        let argv = assemble_launch(&plan, &attach, &probe).expect("assemble");
        assert_eq!(
            argv,
            [
                "/opt/homebrew/bin/ghostty",
                "-e",
                "/opt/homebrew/bin/kubectl",
                "exec",
                "-i"
            ]
        );
        // positional style (no exec args) — attach argv appended directly
        let plan = LaunchPlan {
            argv: vec!["/usr/local/bin/kitty".into()],
        };
        let argv = assemble_launch(&plan, &attach, &probe).expect("assemble");
        assert_eq!(argv[0], "/usr/local/bin/kitty");
        assert_eq!(argv[1], "/opt/homebrew/bin/kubectl");
        // unresolvable transport binary is a loud error, not a flash-close
        let bare = FakeProbe(BTreeSet::new());
        assert!(matches!(
            assemble_launch(&plan, &attach, &bare),
            Err(ResolveError::NotDetected(_))
        ));
    }

    #[test]
    fn shell_quote_survives_spaces_quotes_and_tilde() {
        assert_eq!(
            shell_quote_command(&["ssh".into(), "-o".into(), "User=it's me".into()]),
            r#"ssh -o 'User=it'\''s me'"#
        );
        // Plain tokens stay readable (no needless quoting).
        assert_eq!(
            shell_quote_command(&[
                "tmux".into(),
                "attach-session".into(),
                "-t".into(),
                "x".into()
            ]),
            "tmux attach-session -t x"
        );
        // `~` survives unquoted (display-only string; a quoted tilde would
        // block shell expansion when the user pastes the command).
        assert_eq!(
            shell_quote_command(&["ssh".into(), "-F".into(), "~/.ssh/config".into()]),
            "ssh -F ~/.ssh/config"
        );
    }
}
