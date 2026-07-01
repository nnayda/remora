//! Direct-mode transports built on the reusable PTY-process bridge.

mod batch;
pub mod kubectl;
mod pty_process;
mod remote;
pub mod ssh;

pub use kubectl::KubectlSource;
pub use ssh::SshSource;
