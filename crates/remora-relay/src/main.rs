//! `remora-relay` binary: a blind envelope-frame relay (ADR-0021).
//!
//! Usage: `remora-relay <config.toml>`. Parses the config, opens the (opt-in)
//! audit log, binds the WebSocket server, and runs until killed. Any startup
//! error is written to stderr and the process exits non-zero.
//!
//! On Unix, `SIGHUP` re-reads the config file and hot-swaps the `[[bridges]]`
//! table without dropping live connections (#276), so an operator can rotate
//! bridge registration tokens in place. Every other field — most notably
//! `listen` — requires a restart; a changed value is detected and logged, never
//! half-applied. A reload that fails to read or parse keeps the running config.

use std::sync::Arc;

use remora_relay::{serve, AuditSink, RelayConfig};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("remora-relay: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: remora-relay <config.toml>")?;

    let contents =
        std::fs::read_to_string(&path).map_err(|e| format!("reading config `{path}`: {e}"))?;
    let config = Arc::new(RelayConfig::from_toml_str(&contents)?);
    let audit = AuditSink::new(&config)?;

    let (addr, router, handle) = serve(config.clone(), audit).await?;
    eprintln!("remora-relay listening on {addr}");

    #[cfg(unix)]
    spawn_sighup_reload(path, (*config).clone(), router);
    #[cfg(not(unix))]
    drop(router);

    // Run until the accept loop ends (it never does under normal operation).
    handle.await?;
    Ok(())
}

/// Spawns the SIGHUP handler (#276): each signal re-reads `path` and applies
/// the reload against the running config. A handler-registration failure only
/// disables reloads — it never takes the relay down.
#[cfg(unix)]
fn spawn_sighup_reload(path: String, running: RelayConfig, router: Arc<remora_relay::Router>) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hup = match signal(SignalKind::hangup()) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "remora-relay: cannot install SIGHUP handler ({err}); config reload disabled"
            );
            return;
        }
    };
    tokio::spawn(async move {
        let mut running = running;
        // `recv` returns `None` only if the signal stream closes (shutdown).
        while hup.recv().await.is_some() {
            running = reload_config(&path, running, &router);
        }
    });
}

/// One SIGHUP reload pass: re-read + re-parse `path`, hot-swap the bridges
/// table on success, and warn about changed fields that need a restart.
/// Returns the config to treat as running from now on — on any failure that is
/// the old one, untouched (never crash, never half-apply).
#[cfg(unix)]
fn reload_config(path: &str, running: RelayConfig, router: &remora_relay::Router) -> RelayConfig {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!(
                "remora-relay: SIGHUP reload: reading config `{path}`: {err}; keeping the running config"
            );
            return running;
        }
    };
    let outcome = match running.reload_from_str(&contents) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!(
                "remora-relay: SIGHUP reload: `{path}` rejected ({err}); keeping the running config"
            );
            return running;
        }
    };
    for field in &outcome.restart_required {
        if *field == "listen" {
            eprintln!(
                "remora-relay: SIGHUP reload: `listen` changed in `{path}` — a listen address change requires a restart; keeping the existing listener"
            );
        } else {
            eprintln!(
                "remora-relay: SIGHUP reload: `{field}` changed in `{path}` but is not hot-reloadable — restart to apply it"
            );
        }
    }
    if outcome.bridges_changed {
        router.reload_bridges(outcome.effective.bridges.clone());
        eprintln!(
            "remora-relay: SIGHUP reload: bridges table applied ({} entr{}); live connections are untouched",
            outcome.effective.bridges.len(),
            if outcome.effective.bridges.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
    } else {
        eprintln!("remora-relay: SIGHUP reload: bridges table unchanged");
    }
    outcome.effective
}
