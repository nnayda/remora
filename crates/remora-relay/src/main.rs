//! `remora-relay` binary: a blind envelope-frame relay (ADR-0021).
//!
//! Usage: `remora-relay <config.toml>`. Parses the config, opens the (opt-in)
//! audit log, binds the WebSocket server, and runs until killed. Any startup
//! error is written to stderr and the process exits non-zero.

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

    let (addr, handle) = serve(config, audit).await?;
    eprintln!("remora-relay listening on {addr}");

    // Run until the accept loop ends (it never does under normal operation).
    handle.await?;
    Ok(())
}
