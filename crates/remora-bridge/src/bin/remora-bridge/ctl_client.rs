//! Ctl-socket client stub; the real implementation lands in plan Task 9.

use std::process::ExitCode;

#[allow(
    clippy::unused_async,
    reason = "stub pending plan Task 9 (ctl client); signature must match the async caller"
)]
pub async fn run(
    _command: crate::args::Command,
    _state_dir: &std::path::Path,
) -> Result<ExitCode, String> {
    Err("not implemented yet (plan Task 9)".to_string())
}
