use crate::readers;
use anyhow::{Context, Result};
use clap::ValueEnum;
use clioilc_core::usage_db::{GroupBy, ReportRow};
use clioilc_core::{watcher, DatabaseSource, Reader, Store, UsageDb};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Parses log lines with a reader and stores the records.
pub struct Ingest<'a> {
    reader: &'a dyn Reader,
    root: PathBuf,
    db: UsageDb,
    pub records: u64,
}

impl<'a> Ingest<'a> {
    pub fn open(store: &Store, reader: &'a dyn Reader, dir: &Path) -> Result<Self> {
        let root = dir
            .canonicalize()
            .with_context(|| format!("cannot read {}", dir.display()))?;
        Ok(Self {
            reader,
            root,
            db: UsageDb::open(&store.usage_db_path())?,
            records: 0,
        })
    }

    pub fn line(&mut self, path: &Path, line: &[u8]) -> Result<()> {
        let records = self.reader.parse_line(line);
        if records.is_empty() {
            return Ok(());
        }
        let project = path
            .strip_prefix(&self.root)
            .ok()
            .and_then(|rel| self.reader.project_name(rel));
        for record in &records {
            self.db
                .insert(self.reader.name(), project.as_deref(), record)?;
            self.records += 1;
        }
        Ok(())
    }

    /// Store what was read, then look up the git commits of shell commands
    /// that just finished (see `clioilc_core::git`).
    pub fn commit(&mut self) -> Result<()> {
        self.db.commit()?;
        self.db.resolve_commits()?;
        Ok(())
    }
}

/// Where a database reader stopped (local only, kept in usage.db's meta).
fn cursor_key(reader: &dyn Reader) -> String {
    format!("cursor:{}", reader.name())
}

/// Stores the rows a database reader (e.g. Devin) added since its cursor,
/// or since the start with `from_start` (stored data is deduplicated, so
/// reading again is safe). Returns the number of records.
pub fn pull_database(
    db: &mut UsageDb,
    reader: &dyn Reader,
    source: &dyn DatabaseSource,
    path: &Path,
    from_start: bool,
) -> Result<u64> {
    let key = cursor_key(reader);
    let cursor = if from_start {
        0
    } else {
        db.local_value(&key)?
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let mut records = 0u64;
    let mut failed: Option<anyhow::Error> = None;
    let next = source.read_since(path, cursor, &mut |project, record| {
        if failed.is_none() {
            match db.insert(reader.name(), project, &record) {
                Ok(()) => records += 1,
                Err(e) => failed = Some(e),
            }
        }
    })?;
    if let Some(e) = failed {
        return Err(e.context(format!("storing {} records", reader.name())));
    }
    db.set_local_value(&key, next.to_string().as_bytes())?;
    db.commit()?;
    db.resolve_commits()?;
    Ok(records)
}

/// `usage import` for a database reader: everything, from the start.
fn import_database(
    store: &Store,
    reader: &dyn Reader,
    source: &dyn DatabaseSource,
    path: Option<PathBuf>,
) -> Result<()> {
    let path = match path {
        Some(p) => p,
        None => source.path()?,
    };
    if !path.exists() {
        anyhow::bail!(
            "{} not found (is {} installed?)",
            path.display(),
            reader.name()
        );
    }
    let started = Instant::now();
    let mut db = UsageDb::open(&store.usage_db_path())?;
    let records = pull_database(&mut db, reader, source, &path, true)?;
    eprintln!(
        "imported {} -> {records} record(s) in {:.2}s",
        path.display(),
        started.elapsed().as_secs_f64()
    );
    eprintln!("stored in {}", store.usage_db_path().display());
    let items = db.build_work_items(chrono::Utc::now())?;
    if items > 0 {
        eprintln!("journal: {items} work item(s) updated");
    }
    Ok(())
}

pub fn import(store: &Store, reader: Option<&str>, path: Option<PathBuf>) -> Result<()> {
    let reader = readers::find(reader)?;
    if let Some(source) = reader.database() {
        return import_database(store, reader.as_ref(), source, path);
    }
    let dir = match path {
        Some(p) => p,
        None => reader.default_watch_dir()?,
    };
    let mut ingest = Ingest::open(store, reader.as_ref(), &dir)?;
    let started = Instant::now();

    let mut files = Vec::new();
    watcher::scan(&ingest.root.clone(), reader.as_ref(), &mut |p, _| {
        files.push(p)
    });
    files.sort();

    let (mut bytes, mut lines) = (0u64, 0u64);
    let mut buf = Vec::with_capacity(64 * 1024);
    for file in &files {
        let mut input = BufReader::with_capacity(
            256 * 1024,
            File::open(file).with_context(|| format!("opening {}", file.display()))?,
        );
        loop {
            buf.clear();
            let n = input
                .read_until(b'\n', &mut buf)
                .with_context(|| format!("reading {}", file.display()))?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            // An unterminated last line may still be being written; the
            // watcher picks it up once it is complete.
            let Some(line) = buf.strip_suffix(b"\n") else {
                break;
            };
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if !line.is_empty() {
                lines += 1;
                ingest.line(file, line)?;
            }
        }
        ingest.commit()?;
    }

    eprintln!(
        "imported {} file(s), {} line(s), {:.1} MB -> {} record(s) in {:.2}s",
        files.len(),
        lines,
        bytes as f64 / 1e6,
        ingest.records,
        started.elapsed().as_secs_f64()
    );
    eprintln!("stored in {}", store.usage_db_path().display());
    let items = ingest.db.build_work_items(chrono::Utc::now())?;
    if items > 0 {
        eprintln!("journal: {items} work item(s) updated");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum By {
    Model,
    Project,
    Day,
}

pub fn report(store: &Store, by: By) -> Result<()> {
    let db = UsageDb::open(&store.usage_db_path())?;
    let rows = db.report(match by {
        By::Model => GroupBy::Model,
        By::Project => GroupBy::Project,
        By::Day => GroupBy::Day,
    })?;
    if rows.is_empty() {
        eprintln!("no usage recorded yet: run `clioilc usage import` or `clioilc watch`");
        return Ok(());
    }

    let header = [
        "",
        "msgs",
        "errors",
        "input",
        "cache_write",
        "cache_read",
        "output",
        "tools",
        "tool_err",
    ];
    let mut table: Vec<[String; 9]> = vec![header.map(String::from)];
    let mut total = ReportRow {
        key: "TOTAL".into(),
        messages: 0,
        errors: 0,
        usage: Default::default(),
        tool_calls: 0,
        tool_errors: 0,
    };
    for r in &rows {
        total.messages += r.messages;
        total.errors += r.errors;
        total.usage.input += r.usage.input;
        total.usage.cache_write += r.usage.cache_write;
        total.usage.cache_read += r.usage.cache_read;
        total.usage.output += r.usage.output;
        total.tool_calls += r.tool_calls;
        total.tool_errors += r.tool_errors;
        table.push(cells(r));
    }
    if rows.len() > 1 {
        table.push(cells(&total));
    }

    let widths: Vec<usize> = (0..9)
        .map(|c| {
            table
                .iter()
                .map(|row| row[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in &table {
        let mut out = format!("{:<w$}", row[0], w = widths[0]);
        for c in 1..9 {
            out.push_str(&format!("  {:>w$}", row[c], w = widths[c]));
        }
        println!("{}", out.trim_end());
    }

    let pending: Vec<String> = db
        .unsynced()?
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(table, n)| format!("{table}={n}"))
        .collect();
    if !pending.is_empty() {
        eprintln!("not yet synced: {}", pending.join(" "));
    }
    Ok(())
}

fn cells(r: &ReportRow) -> [String; 9] {
    [
        r.key.clone(),
        thousands(r.messages),
        thousands(r.errors),
        thousands(r.usage.input),
        thousands(r.usage.cache_write),
        thousands(r.usage.cache_read),
        thousands(r.usage.output),
        thousands(r.tool_calls),
        thousands(r.tool_errors),
    ]
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
