use crate::reader::Reader;
use crate::store::{EventKind, EventWriter, TrackedEvent};
use crate::tail::{Checkpoint, Tailer};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use notify::event::ModifyKind;
use notify::{EventKind as NotifyKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

/// How long a path must stay quiet before its pending change is recorded.
/// Collapses bursts such as a log file being appended to while streaming.
/// Line delivery (`follow`) is not debounced.
pub const DEBOUNCE: Duration = Duration::from_secs(1);
/// A path written to continuously is still recorded at least this often.
pub const MAX_DELAY: Duration = Duration::from_secs(10);
/// Longest wait between loop iterations (and thus for noticing `stop`).
const TICK: Duration = Duration::from_millis(250);
/// Allowance for the delay between a file being created and its first event
/// reaching us.
const BIRTH_SLACK: Duration = Duration::from_secs(1);
/// How often read positions are persisted while following.
const CHECKPOINT_EVERY: Duration = Duration::from_secs(5);
/// How long a vanished file's read position is kept in case it was renamed.
const MISSING_GRACE: Duration = Duration::from_secs(10);
/// Max queued notifications handled before the next read pass.
const MAX_BATCH: usize = 4096;

/// Receives what the watcher produces.
pub trait Handler {
    /// A complete, non-blank line appended to `path` (line ending removed).
    /// Only called when [`Options::follow`] is set.
    fn on_line(&mut self, _path: &Path, _line: &[u8]) {}
    /// Called after a read pass that delivered at least one line, e.g. to
    /// flush buffered output.
    fn on_batch_end(&mut self) {}
    /// A non-fatal error; watching continues.
    fn on_error(&mut self, err: anyhow::Error);
}

pub struct Options<'a> {
    /// Stream appended lines to [`Handler::on_line`] in realtime, persisting
    /// read positions to this file so a restart resumes where it stopped.
    pub follow: Option<PathBuf>,
    /// Checked at least every 250 ms; set it to stop gracefully.
    pub stop: &'a AtomicBool,
}

/// Watch `watch_dir` recursively: record debounced changes to tracked files
/// and, when following, deliver their appended lines as they are written.
///
/// Runs until `opts.stop` is set or the underlying watcher stops. On a
/// graceful stop, pending changes are recorded and read positions saved.
pub fn run(
    reader: &dyn Reader,
    watch_dir: &Path,
    writer: &mut EventWriter,
    opts: Options<'_>,
    handler: &mut dyn Handler,
) -> Result<()> {
    // Platform watchers report canonical paths (e.g. /private/var on macOS);
    // canonicalize so paths can be made relative to the root.
    let root = watch_dir
        .canonicalize()
        .with_context(|| format!("cannot watch {}", watch_dir.display()))?;

    // Scan before arming the watch: bytes appended in between are picked up
    // from the scanned offsets on the next event instead of being skipped.
    let mut follow = opts
        .follow
        .map(|checkpoint| Follow::start(&root, reader, checkpoint, handler));

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    let mut pending = Debouncer::new(DEBOUNCE, MAX_DELAY);
    let mut session = Session {
        reader,
        root,
        writer,
        known: HashSet::new(),
    };

    while !opts.stop.load(Ordering::Relaxed) {
        // While a backlog remains, keep reading instead of waiting.
        let backlog = follow.as_ref().is_some_and(Follow::has_backlog);
        let timeout = if backlog { Duration::ZERO } else { TICK };
        match rx.recv_timeout(timeout) {
            Ok(first) => {
                // Take the whole queued burst so one read pass covers it.
                let batch = std::iter::once(first).chain(rx.try_iter().take(MAX_BATCH));
                for res in batch {
                    match res {
                        Ok(event) => {
                            let Some(kind) = map_kind(&event.kind) else {
                                continue;
                            };
                            let now = Instant::now();
                            for path in event.paths.into_iter().filter(|p| reader.is_tracked(p)) {
                                if let Some(f) = follow.as_mut() {
                                    f.dirty.insert(path.clone());
                                }
                                pending.push(path, kind, now);
                            }
                        }
                        Err(err) => handler.on_error(err.into()),
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(f) = follow.as_mut() {
            f.pass(handler);
            f.save(handler, false);
        }
        session.record(pending.drain_ready(Instant::now()), handler);
    }

    if let Some(f) = follow.as_mut() {
        f.pass(handler);
        f.save(handler, true);
    }
    session.record(pending.drain_all(), handler);
    Ok(())
}

/// Realtime line delivery for tracked files.
struct Follow {
    tailer: Tailer,
    /// Files with possibly unread data.
    dirty: HashSet<PathBuf>,
    checkpoint: PathBuf,
    last_save: Instant,
}

impl Follow {
    fn start(
        root: &Path,
        reader: &dyn Reader,
        checkpoint: PathBuf,
        handler: &mut dyn Handler,
    ) -> Self {
        let resume = Checkpoint::load(&checkpoint).unwrap_or_else(|err| {
            handler.on_error(err.context("ignoring saved read positions"));
            Checkpoint::default()
        });
        let new_since = SystemTime::now()
            .checked_sub(BIRTH_SLACK)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut tailer = Tailer::new(resume, new_since);
        let mut dirty = HashSet::new();
        // Existing files start at their saved position, or at their end: a
        // restart never re-reads hundreds of MB of history.
        scan(root, reader, &mut |path, meta| {
            if tailer.adopt(&path, &meta) {
                dirty.insert(path);
            }
        });
        Self {
            tailer,
            dirty,
            checkpoint,
            last_save: Instant::now(),
        }
    }

    fn has_backlog(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Read each dirty file once (bounded per file); files with data left
    /// stay dirty for the next pass.
    fn pass(&mut self, handler: &mut dyn Handler) {
        let mut delivered = false;
        for path in std::mem::take(&mut self.dirty) {
            match self
                .tailer
                .read_new(&path, |line| handler.on_line(&path, line))
            {
                Ok(stats) => {
                    delivered |= stats.lines > 0;
                    if stats.dropped > 0 {
                        handler.on_error(anyhow!(
                            "skipped {} oversized line(s) in {}",
                            stats.dropped,
                            path.display()
                        ));
                    }
                    if stats.more {
                        self.dirty.insert(path);
                    }
                }
                Err(err) => handler.on_error(
                    anyhow::Error::from(err).context(format!("reading {}", path.display())),
                ),
            }
        }
        self.tailer.prune_missing(MISSING_GRACE);
        if delivered {
            handler.on_batch_end();
        }
    }

    fn save(&mut self, handler: &mut dyn Handler, force: bool) {
        if !force && self.last_save.elapsed() < CHECKPOINT_EVERY {
            return;
        }
        self.last_save = Instant::now();
        if !self.tailer.take_changed() {
            return;
        }
        if let Err(err) = self.tailer.checkpoint().save(&self.checkpoint) {
            handler.on_error(err);
        }
    }
}

/// Visit tracked files under `root`. Symlinks are not followed.
pub fn scan(root: &Path, reader: &dyn Reader, visit: &mut dyn FnMut(PathBuf, Metadata)) {
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => dirs.push(path),
                Ok(t) if t.is_file() && reader.is_tracked(&path) => {
                    if let Ok(meta) = entry.metadata() {
                        visit(path, meta);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Turns debounced changes into recorded events.
struct Session<'a> {
    reader: &'a dyn Reader,
    root: PathBuf,
    writer: &'a mut EventWriter,
    /// Paths recorded as existing during this session.
    known: HashSet<PathBuf>,
}

impl Session<'_> {
    fn record(&mut self, changes: Vec<Change>, handler: &mut dyn Handler) {
        for change in changes {
            let event = to_tracked(self.reader, &self.root, &mut self.known, change);
            if let Err(err) = self.writer.record(&event) {
                handler.on_error(err.context(format!("recording {}", event.path.display())));
            }
        }
    }
}

fn map_kind(kind: &NotifyKind) -> Option<EventKind> {
    match kind {
        NotifyKind::Create(_) => Some(EventKind::Created),
        NotifyKind::Modify(ModifyKind::Name(_)) => Some(EventKind::Renamed),
        // Permission/timestamp-only changes are noise for content tracking.
        NotifyKind::Modify(ModifyKind::Metadata(_)) => None,
        NotifyKind::Modify(_) => Some(EventKind::Modified),
        NotifyKind::Remove(_) => Some(EventKind::Removed),
        _ => None,
    }
}

fn to_tracked(
    reader: &dyn Reader,
    root: &Path,
    known: &mut HashSet<PathBuf>,
    change: Change,
) -> TrackedEvent {
    let meta = std::fs::metadata(&change.path).ok();
    let on_disk = match &meta {
        None => OnDisk::Missing,
        Some(_) if known.contains(&change.path) => OnDisk::Known,
        Some(m) => OnDisk::Unknown {
            born_in_window: born_after(m, change.first_seen - BIRTH_SLACK),
        },
    };
    if meta.is_some() {
        known.insert(change.path.clone());
    } else {
        known.remove(&change.path);
    }

    let project = change
        .path
        .strip_prefix(root)
        .ok()
        .and_then(|rel| reader.project_name(rel));
    TrackedEvent {
        timestamp: change.timestamp,
        kind: change.seen.resolve(on_disk),
        path: change.path,
        reader: reader.name().to_string(),
        project,
        size: meta.map(|m| m.len()),
    }
}

/// Whether the file was born at or after `threshold`. Assumes yes when the
/// platform does not report birth times.
fn born_after(meta: &Metadata, threshold: SystemTime) -> bool {
    meta.created().map_or(true, |born| born >= threshold)
}

/// State of a path on disk when its debounce window closes.
#[derive(Debug, Clone, Copy)]
enum OnDisk {
    Missing,
    /// Exists and was already recorded as existing this session.
    Known,
    /// Exists and has not been recorded yet this session.
    Unknown {
        born_in_window: bool,
    },
}

/// Which kinds of change were observed for a path during one debounce window.
///
/// Platform backends may report several kinds for a single operation, and
/// macOS FSEvents can replay stale create/rename flags on later writes, so
/// neither the arrival order nor the flags alone are trustworthy. The final
/// kind is decided by [`Seen::resolve`] together with the on-disk state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Seen {
    created: bool,
    modified: bool,
    removed: bool,
    renamed: bool,
}

impl Seen {
    fn insert(&mut self, kind: EventKind) {
        match kind {
            EventKind::Created => self.created = true,
            EventKind::Modified => self.modified = true,
            EventKind::Removed => self.removed = true,
            EventKind::Renamed => self.renamed = true,
        }
    }

    fn resolve(self, on_disk: OnDisk) -> EventKind {
        match on_disk {
            OnDisk::Missing if self.renamed => EventKind::Renamed,
            OnDisk::Missing => EventKind::Removed,
            OnDisk::Known => EventKind::Modified,
            OnDisk::Unknown { born_in_window } if self.created && born_in_window => {
                EventKind::Created
            }
            OnDisk::Unknown { .. } if self.renamed => EventKind::Renamed,
            OnDisk::Unknown { .. } => EventKind::Modified,
        }
    }
}

#[derive(Debug)]
struct Change {
    path: PathBuf,
    seen: Seen,
    first_seen: SystemTime,
    timestamp: DateTime<Utc>,
}

struct Pending {
    seen: Seen,
    first_seen: SystemTime,
    timestamp: DateTime<Utc>,
    opened: Instant,
    last_seen: Instant,
}

/// Coalesces repeated changes to the same path until it has been quiet for
/// `quiet`, or its window has been open for `max_delay`.
struct Debouncer {
    quiet: Duration,
    max_delay: Duration,
    pending: HashMap<PathBuf, Pending>,
}

impl Debouncer {
    fn new(quiet: Duration, max_delay: Duration) -> Self {
        Self {
            quiet,
            max_delay,
            pending: HashMap::new(),
        }
    }

    fn push(&mut self, path: PathBuf, kind: EventKind, now: Instant) {
        let p = self.pending.entry(path).or_insert_with(|| Pending {
            seen: Seen::default(),
            first_seen: SystemTime::now(),
            timestamp: Utc::now(),
            opened: now,
            last_seen: now,
        });
        p.seen.insert(kind);
        p.timestamp = Utc::now();
        p.last_seen = now;
    }

    fn drain_ready(&mut self, now: Instant) -> Vec<Change> {
        let (quiet, max_delay) = (self.quiet, self.max_delay);
        Self::collect(self.pending.extract_if(|_, p| {
            now.duration_since(p.last_seen) >= quiet || now.duration_since(p.opened) >= max_delay
        }))
    }

    fn drain_all(&mut self) -> Vec<Change> {
        Self::collect(self.pending.drain())
    }

    fn collect(entries: impl Iterator<Item = (PathBuf, Pending)>) -> Vec<Change> {
        let mut changes: Vec<_> = entries
            .map(|(path, p)| Change {
                path,
                seen: p.seen,
                first_seen: p.first_seen,
                timestamp: p.timestamp,
            })
            .collect();
        changes.sort_by_key(|c| c.timestamp);
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EventKind::*;

    fn seen(kinds: &[EventKind]) -> Seen {
        let mut s = Seen::default();
        kinds.iter().for_each(|k| s.insert(*k));
        s
    }

    const NEW: OnDisk = OnDisk::Unknown {
        born_in_window: true,
    };
    const OLD: OnDisk = OnDisk::Unknown {
        born_in_window: false,
    };

    #[test]
    fn coalesces_bursts_until_quiet() {
        let mut d = Debouncer::new(Duration::from_secs(1), Duration::from_secs(60));
        let t0 = Instant::now();
        let p = PathBuf::from("/a.jsonl");
        d.push(p.clone(), Created, t0);
        d.push(p.clone(), Modified, t0 + Duration::from_millis(500));

        assert!(d.drain_ready(t0 + Duration::from_millis(1200)).is_empty());

        let ready = d.drain_ready(t0 + Duration::from_millis(1500));
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].path, p);
        assert_eq!(ready[0].seen, seen(&[Created, Modified]));
        assert!(d.drain_all().is_empty());
    }

    #[test]
    fn continuous_writes_are_recorded_every_max_delay() {
        let mut d = Debouncer::new(Duration::from_secs(1), Duration::from_secs(10));
        let t0 = Instant::now();
        let p = PathBuf::from("/a.jsonl");
        for ms in (0..=9_500).step_by(500) {
            d.push(p.clone(), Modified, t0 + Duration::from_millis(ms));
            assert!(d.drain_ready(t0 + Duration::from_millis(ms)).is_empty());
        }
        d.push(p.clone(), Modified, t0 + Duration::from_millis(10_000));
        assert_eq!(d.drain_ready(t0 + Duration::from_millis(10_000)).len(), 1);
    }

    #[test]
    fn resolves_kind_from_flags_and_disk_state() {
        // new file written in a burst
        assert_eq!(seen(&[Created, Modified]).resolve(NEW), Created);
        // deletion reported alongside stale create/modify flags
        assert_eq!(
            seen(&[Created, Removed, Modified]).resolve(OnDisk::Missing),
            Removed
        );
        // both sides of a rename
        assert_eq!(seen(&[Renamed, Modified]).resolve(OnDisk::Missing), Renamed);
        assert_eq!(seen(&[Created, Renamed]).resolve(OLD), Renamed);
        // stale flags on a later append to a pre-existing or already-seen file
        assert_eq!(seen(&[Created, Modified]).resolve(OLD), Modified);
        assert_eq!(
            seen(&[Created, Renamed, Modified]).resolve(OnDisk::Known),
            Modified
        );
    }

    #[test]
    fn maps_notify_kinds() {
        use notify::event::{CreateKind, DataChange, MetadataKind, RenameMode};
        assert_eq!(
            map_kind(&NotifyKind::Create(CreateKind::File)),
            Some(Created)
        );
        assert_eq!(
            map_kind(&NotifyKind::Modify(ModifyKind::Name(RenameMode::Any))),
            Some(Renamed)
        );
        assert_eq!(
            map_kind(&NotifyKind::Modify(ModifyKind::Data(DataChange::Any))),
            Some(Modified)
        );
        assert_eq!(
            map_kind(&NotifyKind::Modify(ModifyKind::Metadata(MetadataKind::Any))),
            None
        );
    }
}
