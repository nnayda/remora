//! `remora-bridge` binary (#234): headless bridge daemon + its ctl CLI.
//! `serve` runs the daemon; every other subcommand talks to the running
//! daemon over `ctl.sock` (except `init`, which is offline by design).

mod args;
mod ctl_client;
mod daemon;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use args::{parse, resolve_state_dir, Cli, Command};

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(&raw) {
        Ok(cli) => cli,
        Err(msg) => {
            let informational = raw
                .first()
                .is_some_and(|a| matches!(a.as_str(), "--help" | "-h" | "--version" | "-V"));
            if informational {
                println!("{msg}");
                return ExitCode::SUCCESS;
            }
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("remora-bridge: failed to start async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("remora-bridge: {msg}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    let env_state = std::env::var("REMORA_BRIDGE_STATE_DIR").ok();
    // Captured up front: matching on `cli.command` by value below partially
    // moves `cli`, so a later `&cli` borrow (needed in the non-Serve arms)
    // wouldn't compile — take the one field we still need before the match.
    let state_dir_flag = cli.state_dir;
    match cli.command {
        Command::Serve { config } => {
            let config_path = resolve_config_path(config)?;
            let state_dir = resolve_state_dir(
                state_dir_flag.as_deref(),
                env_state.as_deref(),
                &config_path,
            );
            daemon::run_serve(config_path, state_dir).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Init => {
            let state_dir = state_dir_for_client(state_dir_flag.as_deref(), env_state.as_deref())?;
            daemon::run_init(&state_dir)?;
            Ok(ExitCode::SUCCESS)
        }
        command => {
            let state_dir = state_dir_for_client(state_dir_flag.as_deref(), env_state.as_deref())?;
            ctl_client::run(command, &state_dir).await
        }
    }
}

/// Explicit path, or the XDG default; a headless box with neither env set
/// must be told where config lives.
fn resolve_config_path(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    args::default_config_path(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or_else(|| {
        "cannot locate config: pass <config.toml> or set XDG_CONFIG_HOME/HOME".to_string()
    })
}

/// Client subcommands need only the state dir (for ctl.sock / identity).
fn state_dir_for_client(
    state_dir_flag: Option<&Path>,
    env_state: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(flag) = state_dir_flag {
        return Ok(flag.to_path_buf());
    }
    if let Some(env) = env_state.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(env));
    }
    let config = resolve_config_path(None)?;
    Ok(args::resolve_state_dir(None, None, &config))
}
