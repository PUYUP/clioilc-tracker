//! Incremental line reader for append-only files such as JSONL session logs.
//!
//! Work is proportional to newly appended bytes, never to file size:
//! - each file keeps an open handle positioned at the last byte read, so a
//!   read is one `stat` plus sequential `read`s of the new tail only (an
//!   unchanged file costs just the `stat`);
//! - newlines are found with SIMD `memchr`, and complete lines are handed out
//!   as slices of one reused buffer; bytes are copied only for a line that
//!   straddles two reads;
//! - read positions survive restarts through a [`Checkpoint`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Size of the reused read buffer.
    pub read_buf: usize,
    /// Max bytes read from one file per [`Tailer::read_new`] call, so a large
    /// backlog in one file cannot starve the others.
    pub budget: u64,
    /// Max bytes buffered for a line that spans reads; longer lines are
    /// dropped instead of growing memory without bound.
    pub max_line: usize,
    /// Max simultaneously open file handles (least recently used are closed).
    pub max_open: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            read_buf: 256 * 1024,
            budget: 16 * 1024 * 1024,
            max_line: 64 * 1024 * 1024,
            max_open: 128,
        }
    }
}

/// Outcome of one [`Tailer::read_new`] call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadStats {
    pub bytes: u64,
    pub lines: u64,
    /// Lines longer than [`Limits::max_line`] that were skipped.
    pub dropped: u64,
    /// The read budget ran out with data left; call again.
    pub more: bool,
    /// The file was truncated or replaced, so reading restarted at byte 0.
    pub restarted: bool,
}

/// Identity of the underlying file, to tell a replaced file from a grown one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(meta: &Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Some(Self {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            None
        }
    }
}

struct TailState {
    id: Option<FileId>,
    file: Option<File>,
    /// Next byte to read.
    offset: u64,
    /// Start of the line being assembled: everything before it has been
    /// delivered. This is the position a checkpoint stores.
    line_start: u64,
    /// Bytes of an incomplete trailing line.
    partial: Vec<u8>,
    /// Discarding an oversized line until its newline.
    skipping: bool,
    last_used: u64,
    missing_since: Option<Instant>,
}

impl TailState {
    fn at(id: Option<FileId>, offset: u64) -> Self {
        Self {
            id,
            file: None,
            offset,
            line_start: offset,
            partial: Vec::new(),
            skipping: false,
            last_used: 0,
            missing_since: None,
        }
    }

    fn rewind(&mut self) {
        self.offset = 0;
        self.line_start = 0;
        self.partial = Vec::new();
        self.skipping = false;
    }
}

/// Follows many append-only files, delivering each complete line once.
pub struct Tailer {
    files: HashMap<PathBuf, TailState>,
    resume: HashMap<PathBuf, Saved>,
    limits: Limits,
    buf: Vec<u8>,
    open: usize,
    clock: u64,
    new_since: SystemTime,
    changed: bool,
}

impl Tailer {
    /// `resume` restores positions from a previous run. A file seen for the
    /// first time without a saved position is read from the beginning if it
    /// was born at or after `new_since`, otherwise from its current end.
    pub fn new(resume: Checkpoint, new_since: SystemTime) -> Self {
        Self::with_limits(resume, new_since, Limits::default())
    }

    pub fn with_limits(resume: Checkpoint, new_since: SystemTime, limits: Limits) -> Self {
        Self {
            files: HashMap::new(),
            resume: resume.files,
            buf: vec![0; limits.read_buf.max(1)],
            limits,
            open: 0,
            clock: 0,
            new_since,
            changed: false,
        }
    }

    /// Start following `path` if not already. Returns whether it has unread data.
    pub fn adopt(&mut self, path: &Path, meta: &Metadata) -> bool {
        let len = meta.len();
        if let Some(state) = self.files.get(path) {
            return state.offset < len;
        }
        let id = FileId::of(meta);
        let saved = self
            .resume
            .remove(path)
            .filter(|s| s.id == id && s.offset <= len);
        let state = if let Some(saved) = saved {
            TailState::at(id, saved.offset)
        } else if let Some(moved) = self.take_renamed(id) {
            moved
        } else if meta.created().map_or(true, |born| born >= self.new_since) {
            TailState::at(id, 0)
        } else {
            TailState::at(id, len)
        };
        let backlog = state.offset < len;
        self.files.insert(path.to_path_buf(), state);
        self.changed = true;
        backlog
    }

    /// Read whatever was appended to `path` since the last call, passing each
    /// complete, non-blank line (without its line ending) to `on_line`.
    pub fn read_new(
        &mut self,
        path: &Path,
        mut on_line: impl FnMut(&[u8]),
    ) -> io::Result<ReadStats> {
        let mut stats = ReadStats::default();
        let meta = match fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            Ok(_) => return Ok(stats),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.mark_missing(path);
                return Ok(stats);
            }
            Err(e) => return Err(e),
        };
        self.adopt(path, &meta);
        if self.open >= self.limits.max_open && self.files[path].file.is_none() {
            self.evict_lru();
        }
        self.clock += 1;

        let Self {
            files,
            buf,
            limits,
            open,
            clock,
            changed,
            ..
        } = self;
        let st = files.get_mut(path).expect("adopted above");
        st.last_used = *clock;
        st.missing_since = None;

        let id = FileId::of(&meta);
        let len = meta.len();
        if st.id != id {
            // Replaced (e.g. written elsewhere and renamed over this path).
            if st.file.take().is_some() {
                *open -= 1;
            }
            st.id = id;
            st.rewind();
            stats.restarted = true;
        } else if len < st.offset {
            st.rewind();
            if let Some(f) = st.file.as_mut() {
                f.seek(SeekFrom::Start(0))?;
            }
            stats.restarted = true;
        }
        if stats.restarted {
            *changed = true;
        }
        if len <= st.offset {
            return Ok(stats);
        }

        if st.file.is_none() {
            let mut f = File::open(path)?;
            if FileId::of(&f.metadata()?) != id {
                // Replaced between stat and open; the next event retries.
                return Ok(stats);
            }
            f.seek(SeekFrom::Start(st.offset))?;
            st.file = Some(f);
            *open += 1;
        }

        // Detach the handle so `consume` can borrow the state mutably.
        let mut file = st.file.take().expect("opened above");
        let mut remaining = limits.budget;
        let result = loop {
            if remaining == 0 {
                break Ok(());
            }
            let want = buf
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let n = match file.read(&mut buf[..want]) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            };
            remaining -= n as u64;
            stats.bytes += n as u64;
            consume(st, &buf[..n], limits, &mut stats, &mut on_line);
        };
        st.file = Some(file);
        result?;
        stats.more = remaining == 0 && st.offset < len;
        if stats.bytes > 0 {
            *changed = true;
        }
        Ok(stats)
    }

    /// Forget files that have been missing for at least `grace`. Missing files
    /// are kept for a while so a rename can carry their position over.
    pub fn prune_missing(&mut self, grace: Duration) {
        let before = self.files.len();
        self.files
            .retain(|_, s| s.missing_since.is_none_or(|t| t.elapsed() < grace));
        self.changed |= self.files.len() != before;
    }

    /// Whether positions changed since the last call.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Positions of all present files, at their last complete line.
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            files: self
                .files
                .iter()
                .filter(|(_, s)| s.missing_since.is_none())
                .map(|(p, s)| {
                    let saved = Saved {
                        id: s.id,
                        offset: s.line_start,
                    };
                    (p.clone(), saved)
                })
                .collect(),
        }
    }

    fn mark_missing(&mut self, path: &Path) {
        if let Some(st) = self.files.get_mut(path) {
            // Close right away: an open handle keeps a deleted file's disk
            // space allocated.
            if st.file.take().is_some() {
                self.open -= 1;
            }
            st.missing_since.get_or_insert_with(Instant::now);
            self.changed = true;
        }
    }

    /// Detach the state of a file that now lives under another path.
    fn take_renamed(&mut self, id: Option<FileId>) -> Option<TailState> {
        let id = id?;
        let old = self.files.iter().find_map(|(p, s)| {
            let moved = s.id == Some(id)
                && (s.missing_since.is_some()
                    || fs::metadata(p).ok().and_then(|m| FileId::of(&m)) != Some(id));
            moved.then(|| p.clone())
        })?;
        let mut state = self.files.remove(&old)?;
        state.missing_since = None;
        Some(state)
    }

    fn evict_lru(&mut self) {
        let lru = self
            .files
            .values_mut()
            .filter(|s| s.file.is_some())
            .min_by_key(|s| s.last_used);
        if let Some(st) = lru {
            st.file = None;
            self.open -= 1;
        }
    }
}

/// Split `chunk` (the bytes at `st.offset`) into lines.
fn consume(
    st: &mut TailState,
    chunk: &[u8],
    limits: &Limits,
    stats: &mut ReadStats,
    on_line: &mut impl FnMut(&[u8]),
) {
    let mut emit = |line: &[u8], stats: &mut ReadStats| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.is_empty() {
            on_line(line);
            stats.lines += 1;
        }
    };

    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', chunk) {
        let segment = &chunk[start..nl];
        if st.skipping {
            st.skipping = false;
            stats.dropped += 1;
        } else if st.partial.is_empty() {
            emit(segment, stats);
        } else if st.partial.len() + segment.len() > limits.max_line {
            stats.dropped += 1;
            st.partial = Vec::new();
        } else {
            st.partial.extend_from_slice(segment);
            emit(&st.partial, stats);
            if st.partial.capacity() > 4 * limits.read_buf {
                st.partial = Vec::new();
            } else {
                st.partial.clear();
            }
        }
        start = nl + 1;
        st.line_start = st.offset + start as u64;
    }

    let rest = &chunk[start..];
    if !st.skipping && !rest.is_empty() {
        if st.partial.len() + rest.len() > limits.max_line {
            st.partial = Vec::new();
            st.skipping = true;
        } else {
            st.partial.extend_from_slice(rest);
        }
    }
    st.offset += chunk.len() as u64;
}

/// Saved read positions, keyed by path.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    files: HashMap<PathBuf, Saved>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Saved {
    id: Option<FileId>,
    offset: u64,
}

impl Checkpoint {
    /// Load from `path`; a missing file yields an empty checkpoint.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically (temp file + rename) so a crash never leaves it torn.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("clioilc-tail-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn append(path: &Path, data: &str) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(data.as_bytes())
            .unwrap();
    }

    /// Reads everything new, ignoring the budget.
    fn lines(t: &mut Tailer, path: &Path) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            let stats = t
                .read_new(path, |l| out.push(String::from_utf8(l.to_vec()).unwrap()))
                .unwrap();
            if !stats.more {
                return out;
            }
        }
    }

    fn from_start() -> Tailer {
        Tailer::new(Checkpoint::default(), SystemTime::UNIX_EPOCH)
    }

    fn small(max_line: usize) -> Tailer {
        let limits = Limits {
            read_buf: 4,
            budget: 8,
            max_line,
            max_open: 1,
        };
        Tailer::with_limits(Checkpoint::default(), SystemTime::UNIX_EPOCH, limits)
    }

    #[test]
    fn delivers_only_appended_complete_lines() {
        let d = Dir::new("append");
        let f = d.file("a.jsonl");
        append(&f, "one\ntw");
        let mut t = from_start();
        assert_eq!(lines(&mut t, &f), ["one"]);
        assert!(lines(&mut t, &f).is_empty());
        append(&f, "o\r\n\nthree\n");
        assert_eq!(lines(&mut t, &f), ["two", "three"]);
    }

    #[test]
    fn preexisting_files_start_at_end() {
        let d = Dir::new("end");
        let f = d.file("a.jsonl");
        append(&f, "old\n");
        let future = SystemTime::now() + Duration::from_secs(3600);
        let mut t = Tailer::new(Checkpoint::default(), future);
        assert!(lines(&mut t, &f).is_empty());
        append(&f, "new\n");
        assert_eq!(lines(&mut t, &f), ["new"]);
    }

    #[test]
    fn restarts_after_truncation_and_replacement() {
        let d = Dir::new("restart");
        let f = d.file("a.jsonl");
        append(&f, "aaaa\nbbbb\n");
        let mut t = from_start();
        assert_eq!(lines(&mut t, &f), ["aaaa", "bbbb"]);

        fs::write(&f, "c\n").unwrap();
        assert_eq!(lines(&mut t, &f), ["c"]);

        let tmp = d.file("tmp");
        fs::write(&tmp, "d\n").unwrap();
        fs::rename(&tmp, &f).unwrap();
        let mut got = Vec::new();
        let stats = t.read_new(&f, |l| got.push(l.to_vec())).unwrap();
        assert!(stats.restarted);
        assert_eq!(got, [b"d".to_vec()]);
    }

    #[test]
    fn rename_keeps_position() {
        let d = Dir::new("rename");
        let (a, b) = (d.file("a.jsonl"), d.file("b.jsonl"));
        append(&a, "one\n");
        let mut t = from_start();
        assert_eq!(lines(&mut t, &a), ["one"]);
        fs::rename(&a, &b).unwrap();
        append(&b, "two\n");
        assert!(lines(&mut t, &a).is_empty());
        assert_eq!(lines(&mut t, &b), ["two"]);
    }

    #[test]
    fn budget_bounds_each_call() {
        let d = Dir::new("budget");
        let f = d.file("a.jsonl");
        append(&f, "aaa\nbbb\nccc\n");
        let mut t = small(64);
        let mut got = 0;
        let stats = t.read_new(&f, |_| got += 1).unwrap();
        assert_eq!((stats.bytes, stats.more, got), (8, true, 2));
        assert_eq!(lines(&mut t, &f), ["ccc"]);
    }

    #[test]
    fn drops_oversized_lines_spanning_reads() {
        let d = Dir::new("oversized");
        let f = d.file("a.jsonl");
        append(&f, "0123456789\nok\n");
        let mut t = small(6);
        let mut got = Vec::new();
        let mut dropped = 0;
        loop {
            let s = t.read_new(&f, |l| got.push(l.to_vec())).unwrap();
            dropped += s.dropped;
            if !s.more {
                break;
            }
        }
        assert_eq!((got, dropped), (vec![b"ok".to_vec()], 1));
    }

    #[test]
    fn checkpoint_resumes_at_last_complete_line() {
        let d = Dir::new("checkpoint");
        let f = d.file("a.jsonl");
        let cp = d.file("offsets.json");
        append(&f, "one\ntw");
        let mut t = from_start();
        assert_eq!(lines(&mut t, &f), ["one"]);
        assert!(t.take_changed());
        t.checkpoint().save(&cp).unwrap();

        append(&f, "o\nthree\n");
        let mut resumed = Tailer::new(Checkpoint::load(&cp).unwrap(), SystemTime::now());
        let meta = fs::metadata(&f).unwrap();
        assert!(resumed.adopt(&f, &meta));
        assert_eq!(lines(&mut resumed, &f), ["two", "three"]);
    }
}
