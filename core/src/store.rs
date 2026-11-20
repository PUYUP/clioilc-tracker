use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};

const EVENTS_FILE: &str = "events.jsonl";
const CHECKPOINT_FILE: &str = "offsets.json";
const USAGE_DB_FILE: &str = "usage.db";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedEvent {
    pub timestamp: DateTime<Utc>,
    pub kind: EventKind,
    pub path: PathBuf,
    pub reader: String,
    pub project: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Created,
    Modified,
    Removed,
    Renamed,
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            EventKind::Created => "Created",
            EventKind::Modified => "Modified",
            EventKind::Removed => "Removed",
            EventKind::Renamed => "Renamed",
        };
        // `pad` honours width/alignment flags such as `{:<8}`.
        f.pad(s)
    }
}

/// Events loaded from the store, plus how many unreadable lines were skipped.
#[derive(Debug, Default)]
pub struct EventLog {
    pub events: Vec<TrackedEvent>,
    pub skipped: usize,
}

/// Append-only JSON Lines event log inside a data directory.
pub struct Store {
    file: PathBuf,
}

impl Store {
    /// Does not touch the filesystem; the directory is created on first write.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join(EVENTS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    /// The data directory (holds the event log, usage.db, auth.json, ...).
    pub fn data_dir(&self) -> &Path {
        self.file.parent().unwrap_or(Path::new("."))
    }

    /// Where `watch --follow` persists read positions, next to the event log.
    pub fn checkpoint_path(&self) -> PathBuf {
        self.file.with_file_name(CHECKPOINT_FILE)
    }

    /// SQLite database of parsed usage, next to the event log.
    pub fn usage_db_path(&self) -> PathBuf {
        self.file.with_file_name(USAGE_DB_FILE)
    }

    pub fn load(&self) -> Result<EventLog> {
        let file = match File::open(&self.file) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(EventLog::default()),
            Err(e) => return Err(e).with_context(|| format!("opening {}", self.file.display())),
        };
        let mut log = EventLog::default();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(event) => log.events.push(event),
                Err(_) => log.skipped += 1,
            }
        }
        Ok(log)
    }

    /// Open the log for appending, creating the data directory if needed.
    pub fn writer(&self) -> Result<EventWriter> {
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .with_context(|| format!("opening {}", self.file.display()))?;
        Ok(EventWriter { file })
    }
}

/// Holds the event log open for the lifetime of a watch session.
pub struct EventWriter {
    file: File,
}

impl EventWriter {
    pub fn record(&mut self, event: &TrackedEvent) -> Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        // One write per line so concurrent readers never see a partial line.
        self.file.write_all(&line)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clioilc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn event(path: &str) -> TrackedEvent {
        TrackedEvent {
            timestamp: Utc::now(),
            kind: EventKind::Created,
            path: PathBuf::from(path),
            reader: "claude".into(),
            project: Some("p".into()),
            size: Some(3),
        }
    }

    #[test]
    fn missing_log_loads_empty() {
        let store = Store::new(&temp_dir("missing"));
        let log = store.load().unwrap();
        assert!(log.events.is_empty());
        assert_eq!(log.skipped, 0);
    }

    #[test]
    fn round_trips_and_counts_corrupt_lines() {
        let dir = temp_dir("roundtrip");
        let store = Store::new(&dir);
        let mut writer = store.writer().unwrap();
        writer.record(&event("/a.json")).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(store.path())
            .unwrap()
            .write_all(b"not json\n\n")
            .unwrap();
        writer.record(&event("/b.json")).unwrap();

        let log = store.load().unwrap();
        let paths: Vec<_> = log
            .events
            .iter()
            .map(|e| e.path.to_str().unwrap())
            .collect();
        assert_eq!(paths, ["/a.json", "/b.json"]);
        assert_eq!(log.skipped, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn display_respects_padding() {
        assert_eq!(format!("[{:<8}]", EventKind::Created), "[Created ]");
    }
}
