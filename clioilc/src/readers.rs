use anyhow::{anyhow, Result};
use clioilc_core::Reader;

/// All registered readers. `clioilc add mod` inserts new entries above the marker.
pub fn all() -> Vec<Box<dyn Reader>> {
    vec![
        Box::new(clioilc_claude::ClaudeReader),
        Box::new(clioilc_gemini::GeminiReader),
        // clioilc-add-mod:insert-here
    ]
}

/// Look up a reader by name, or the first registered one when `name` is `None`.
pub fn find(name: Option<&str>) -> Result<Box<dyn Reader>> {
    let mut readers = all().into_iter();
    match name {
        Some(n) => readers.find(|r| r.name() == n).ok_or_else(|| {
            let known: Vec<_> = all().iter().map(|r| r.name()).collect();
            anyhow!("unknown reader '{n}' (available: {})", known.join(", "))
        }),
        None => readers
            .next()
            .ok_or_else(|| anyhow!("no readers registered")),
    }
}
