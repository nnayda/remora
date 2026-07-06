//! Desktop-side resolver seam: re-exports the shared resolver from
//! `remora_core::resolve` (lifted there for the headless bridge, #234) and
//! maps its typed error into the frontend's `BridgeError` DTO.

pub use remora_core::resolve::{ConfigResolver, ResolveError, SourceResolver};

use super::error::BridgeError;

impl From<ResolveError> for BridgeError {
    fn from(e: ResolveError) -> Self {
        BridgeError::Config {
            message: e.to_string(),
        }
    }
}
