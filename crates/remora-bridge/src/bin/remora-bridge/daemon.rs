//! Daemon implementation lands in plan Task 8; these stubs keep main.rs compiling.
use std::path::{Path, PathBuf};

#[allow(
    clippy::unused_async,
    reason = "stub pending plan Task 8 (daemon); signature must match the async caller"
)]
pub async fn run_serve(_config_path: PathBuf, _state_dir: PathBuf) -> Result<(), String> {
    Err("not implemented yet (plan Task 8)".to_string())
}

pub fn run_init(_state_dir: &Path) -> Result<(), String> {
    Err("not implemented yet (plan Task 8)".to_string())
}
