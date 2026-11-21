pub mod filter;
pub mod git;
pub mod reader;
pub mod redact;
pub mod store;
pub mod tail;
pub mod usage;
pub mod usage_db;
pub mod watcher;

pub use filter::EventFilter;
pub use reader::Reader;
pub use store::{EventKind, EventLog, EventWriter, Store, TrackedEvent};
pub use usage::Record;
pub use usage_db::UsageDb;

use anyhow::{bail, Result};
use std::path::PathBuf;

/// The current user's home directory.
///
/// Fails instead of silently falling back to a path relative to the current
/// working directory when no home directory is configured.
pub fn home_dir() -> Result<PathBuf> {
    for var in ["HOME", "USERPROFILE"] {
        if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
    }
    bail!("cannot determine home directory: HOME is not set")
}
