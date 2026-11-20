use anyhow::Result;
use clioilc_core::{EventFilter, Store};
use std::collections::BTreeMap;

pub fn run(store: &Store, filter: &EventFilter) -> Result<()> {
    let events = super::load_events(store, filter)?;

    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for e in &events {
        let key = (e.reader.as_str(), e.project.as_deref().unwrap_or("-"));
        *counts.entry(key).or_default() += 1;
    }
    for ((reader, project), count) in counts {
        println!("{count:>6}  {reader:<10} {project}");
    }
    Ok(())
}
