pub mod activity;
pub mod add;
pub mod attest;
pub mod history;
pub mod journal;
pub mod link;
pub mod login;
pub mod service;
pub mod standup;
pub mod summary;
pub mod sync;
pub mod team;
pub mod usage;
pub mod watch;

use anyhow::Result;
use clioilc_core::{EventFilter, Store, TrackedEvent};
use std::io;

/// Load events matching `filter`, warning about any unreadable lines.
fn load_events(store: &Store, filter: &EventFilter) -> Result<Vec<TrackedEvent>> {
    let log = store.load()?;
    if log.skipped > 0 {
        eprintln!(
            "warning: skipped {} corrupt line(s) in {}",
            log.skipped,
            store.path().display()
        );
    }
    Ok(log
        .events
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect())
}

/// A closed stdout (e.g. output piped into `head`) is not an error.
fn ignore_closed_stdout(res: io::Result<()>) -> Result<()> {
    match res {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}
