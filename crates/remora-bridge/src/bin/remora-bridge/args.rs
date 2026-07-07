//! Manual argument parsing (spec D11): the grammar is six subcommands and
//! three flags — small and fixed. Explicit `match`-dispatch, a hand-written
//! usage text, and hard rejection of anything unrecognized. No arg-parsing
//! dependency enters the workspace; revisit if the grammar grows.

use std::path::{Path, PathBuf};

/// Matches the desktop pairing dialog's default TTL
/// (`apps/desktop/src-tauri/src/bridge/pairing.rs::DEFAULT_PAIRING_TTL_SECS`).
pub const DEFAULT_PAIR_TTL_SECS: u64 = 120;

const USAGE: &str = "\
remora-bridge — headless Remora bridge (ADR-0021, #234)

Usage:
  remora-bridge serve [<config.toml>] [--state-dir <dir>]
  remora-bridge init [--state-dir <dir>]
  remora-bridge pair [--ttl <secs>] [--state-dir <dir>]
  remora-bridge devices [--state-dir <dir>]
  remora-bridge revoke <device-id> [--state-dir <dir>]
  remora-bridge status [--require-relay] [--state-dir <dir>]
  remora-bridge fingerprint [--state-dir <dir>]
  remora-bridge --help | --version

State dir resolution: --state-dir, else $REMORA_BRIDGE_STATE_DIR, else the
config file's parent directory. Config default: $XDG_CONFIG_HOME (or
~/.config) + remora/config.toml.";

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Serve { config: Option<PathBuf> },
    Init,
    Pair { ttl_secs: u64 },
    Devices,
    Revoke { device_id: String },
    Status { require_relay: bool },
    Fingerprint,
}

#[derive(Debug)]
pub struct Cli {
    pub command: Command,
    pub state_dir: Option<PathBuf>,
}

/// Parses argv (without argv[0]). `Err` carries the message to print; the
/// caller exits 0 for --help/--version, 2 otherwise (it can tell them apart
/// by checking the original args).
pub fn parse(args: &[String]) -> Result<Cli, String> {
    let mut it = args.iter();
    let sub = it.next().ok_or_else(|| USAGE.to_string())?;
    match sub.as_str() {
        "--help" | "-h" => return Err(USAGE.to_string()),
        "--version" | "-V" => return Err(format!("remora-bridge {}", env!("CARGO_PKG_VERSION"))),
        _ => {}
    }

    // A value-taking flag must never swallow a following flag as its value.
    let flag_value =
        |it: &mut std::slice::Iter<'_, String>, flag: &str| -> Result<String, String> {
            match it.next() {
                Some(v) if !v.starts_with('-') => Ok(v.clone()),
                _ => Err(format!("{flag} requires a value\n\n{USAGE}")),
            }
        };

    let mut state_dir: Option<PathBuf> = None;
    let mut ttl: Option<u64> = None;
    let mut require_relay = false;
    let mut positionals: Vec<String> = Vec::new();

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(flag_value(&mut it, "--state-dir")?));
            }
            "--ttl" => {
                let v = flag_value(&mut it, "--ttl")?;
                ttl = Some(
                    v.parse()
                        .map_err(|_| format!("--ttl expects seconds, got `{v}`\n\n{USAGE}"))?,
                );
            }
            "--require-relay" => require_relay = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`\n\n{USAGE}"))
            }
            other => positionals.push(other.to_string()),
        }
    }

    let expect_positionals = |n: usize| -> Result<(), String> {
        if positionals.len() != n {
            return Err(format!("unexpected arguments {positionals:?}\n\n{USAGE}"));
        }
        Ok(())
    };

    // Symmetric per-subcommand flag validation: `--ttl` belongs to `pair`
    // alone, `--require-relay` to `status` alone (`--state-dir` is global).
    // Tracking `ttl` as set-vs-unset (not compared against the default)
    // means `serve --ttl 120` is rejected like any other misplaced flag.
    let reject_misplaced_flags =
        |ttl_allowed: bool, require_relay_allowed: bool| -> Result<(), String> {
            if ttl.is_some() && !ttl_allowed {
                return Err(format!("--ttl is not valid for `{sub}`\n\n{USAGE}"));
            }
            if require_relay && !require_relay_allowed {
                return Err(format!(
                    "--require-relay is not valid for `{sub}`\n\n{USAGE}"
                ));
            }
            Ok(())
        };

    let command = match sub.as_str() {
        "serve" => {
            reject_misplaced_flags(false, false)?;
            if positionals.len() > 1 {
                return Err(format!("serve takes at most one config path\n\n{USAGE}"));
            }
            Command::Serve {
                config: positionals.pop().map(PathBuf::from),
            }
        }
        "init" => {
            reject_misplaced_flags(false, false)?;
            expect_positionals(0)?;
            Command::Init
        }
        "pair" => {
            reject_misplaced_flags(true, false)?;
            expect_positionals(0)?;
            Command::Pair {
                ttl_secs: ttl.unwrap_or(DEFAULT_PAIR_TTL_SECS),
            }
        }
        "devices" => {
            reject_misplaced_flags(false, false)?;
            expect_positionals(0)?;
            Command::Devices
        }
        "revoke" => {
            reject_misplaced_flags(false, false)?;
            if positionals.len() != 1 {
                return Err(format!("revoke takes exactly one <device-id>\n\n{USAGE}"));
            }
            let device_id = positionals.pop().unwrap_or_default();
            if device_id.is_empty() {
                return Err(format!("revoke <device-id> must not be empty\n\n{USAGE}"));
            }
            Command::Revoke { device_id }
        }
        "status" => {
            reject_misplaced_flags(false, true)?;
            expect_positionals(0)?;
            Command::Status { require_relay }
        }
        "fingerprint" => {
            reject_misplaced_flags(false, false)?;
            expect_positionals(0)?;
            Command::Fingerprint
        }
        other => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    Ok(Cli { command, state_dir })
}

/// Spec D7: --state-dir flag > $REMORA_BRIDGE_STATE_DIR > config parent.
pub fn resolve_state_dir(
    cli_flag: Option<&Path>,
    env: Option<&str>,
    config_path: &Path,
) -> PathBuf {
    if let Some(flag) = cli_flag {
        return flag.to_path_buf();
    }
    if let Some(env) = env.filter(|v| !v.is_empty()) {
        return PathBuf::from(env);
    }
    config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Linux-first default (documented): $XDG_CONFIG_HOME, else $HOME/.config,
/// joined with core's `CONFIG_FILE_RELPATH`. `None` when neither env is set.
pub fn default_config_path(env_xdg: Option<&str>, env_home: Option<&str>) -> Option<PathBuf> {
    let base = match env_xdg.filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => PathBuf::from(env_home.filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(remora_core::config::config_file_path(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn parse_ok(args: &[&str]) -> Cli {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parse")
    }

    #[test]
    fn serve_parses_optional_config_and_state_dir() {
        let cli = parse_ok(&[
            "serve",
            "/etc/remora/config.toml",
            "--state-dir",
            "/var/lib/rb",
        ]);
        assert!(
            matches!(cli.command, Command::Serve { config: Some(ref p) } if p == Path::new("/etc/remora/config.toml"))
        );
        assert_eq!(cli.state_dir.as_deref(), Some(Path::new("/var/lib/rb")));
    }

    #[test]
    fn pair_default_ttl_and_override() {
        assert!(matches!(
            parse_ok(&["pair"]).command,
            Command::Pair { ttl_secs: 120 }
        ));
        assert!(matches!(
            parse_ok(&["pair", "--ttl", "60"]).command,
            Command::Pair { ttl_secs: 60 }
        ));
    }

    #[test]
    fn status_require_relay_flag() {
        assert!(matches!(
            parse_ok(&["status"]).command,
            Command::Status {
                require_relay: false
            }
        ));
        assert!(matches!(
            parse_ok(&["status", "--require-relay"]).command,
            Command::Status {
                require_relay: true
            }
        ));
    }

    // Important 1: a value-taking flag must never consume a following flag
    // as its value; a missing or flag-shaped value is an error.
    #[test]
    fn value_flag_never_consumes_a_following_flag() {
        assert!(matches!(
            parse(&["pair".into(), "--state-dir".into(), "--require-relay".into()]),
            Err(msg) if msg.contains("--state-dir requires a value") && msg.contains("Usage")
        ));
        assert!(matches!(
            parse(&["pair".into(), "--ttl".into()]),
            Err(msg) if msg.contains("--ttl requires a value") && msg.contains("Usage")
        ));
        assert!(matches!(
            parse(&["pair".into(), "--ttl".into(), "--state-dir".into(), "/x".into()]),
            Err(msg) if msg.contains("--ttl requires a value")
        ));
        assert!(matches!(
            parse(&["serve".into(), "--state-dir".into()]),
            Err(msg) if msg.contains("--state-dir requires a value")
        ));
    }

    // Important 2: per-subcommand flag validation — `--ttl` only on `pair`,
    // `--require-relay` only on `status`; `--state-dir` is valid everywhere.
    #[test]
    fn flags_are_rejected_on_wrong_subcommands() {
        for args in [
            vec!["pair", "--require-relay"],
            vec!["status", "--ttl", "5"],
            vec!["init", "--ttl", "5"],
            vec!["serve", "--ttl", "120"], // equal to the default is still misplaced
            vec!["devices", "--require-relay"],
            vec!["fingerprint", "--ttl", "5"],
            vec!["revoke", "some-id", "--require-relay"],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let result = parse(&owned);
            assert!(result.is_err(), "expected error for {args:?}");
            let msg = result.expect_err("checked above");
            assert!(
                msg.contains("Usage"),
                "error for {args:?} lacks usage: {msg}"
            );
        }
        // --state-dir stays valid on every subcommand.
        assert!(parse(&["init".into(), "--state-dir".into(), "/x".into()]).is_ok());
        assert!(parse(&["fingerprint".into(), "--state-dir".into(), "/x".into()]).is_ok());
    }

    #[test]
    fn revoke_rejects_empty_device_id() {
        assert!(matches!(
            parse(&["revoke".into(), "".into()]),
            Err(msg) if msg.contains("device-id") && msg.contains("Usage")
        ));
    }

    // G1: unknown flags and subcommands are rejected, not ignored.
    #[test]
    fn unknown_flag_is_an_error() {
        assert!(parse(&["pair".into(), "--yes".into()]).is_err());
        assert!(parse(&["frobnicate".into()]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn help_and_version_are_recognized() {
        assert!(matches!(parse(&["--help".into()]), Err(msg) if msg.contains("Usage")));
        assert!(
            matches!(parse(&["--version".into()]), Err(msg) if msg.contains(env!("CARGO_PKG_VERSION")))
        );
    }

    // G15: state-dir precedence flag > env > config parent.
    #[test]
    fn state_dir_precedence() {
        let cfg = Path::new("/etc/remora/config.toml");
        assert_eq!(
            resolve_state_dir(Some(Path::new("/flag")), Some("/env"), cfg),
            PathBuf::from("/flag")
        );
        assert_eq!(
            resolve_state_dir(None, Some("/env"), cfg),
            PathBuf::from("/env")
        );
        assert_eq!(
            resolve_state_dir(None, None, cfg),
            PathBuf::from("/etc/remora")
        );
    }

    #[test]
    fn default_config_path_prefers_xdg() {
        assert_eq!(
            default_config_path(Some("/xdg"), Some("/home/u")),
            Some(PathBuf::from("/xdg/remora/config.toml"))
        );
        assert_eq!(
            default_config_path(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/remora/config.toml"))
        );
        assert_eq!(default_config_path(None, None), None);
    }
}
