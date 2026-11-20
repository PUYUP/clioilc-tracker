use anyhow::Result;
use clioilc_core::{EventFilter, Store, TrackedEvent};
use std::io::{self, Write};

pub fn run(store: &Store, filter: &EventFilter, limit: Option<usize>) -> Result<()> {
    let events = super::load_events(store, filter)?;
    let start = limit.map_or(0, |n| events.len().saturating_sub(n));
    let shown = &events[start..];
    super::ignore_closed_stdout(print(&mut io::stdout().lock(), shown))?;
    eprintln!("{} event(s) shown", shown.len());
    Ok(())
}

fn print(out: &mut impl Write, events: &[TrackedEvent]) -> io::Result<()> {
    for e in events {
        writeln!(
            out,
            "{}  {:<8}  {:<8}  {:<60}  {} bytes  project={}",
            e.timestamp.format("%Y-%m-%d %H:%M:%S"),
            e.kind,
            e.reader,
            e.path.display(),
            e.size.map_or_else(|| "-".to_string(), |s| s.to_string()),
            e.project.as_deref().unwrap_or("-"),
        )?;
    }
    Ok(())
}
