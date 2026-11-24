use super::usage::Ingest;
use crate::readers;
use anyhow::{Context, Result};
use clioilc_core::watcher::{self, Handler, Options};
use clioilc_core::Store;
use std::io::{self, BufWriter, StdoutLock, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

pub fn run(
    store: &Store,
    reader: Option<&str>,
    path: Option<PathBuf>,
    follow: bool,
    sync_every: Option<Duration>,
) -> Result<()> {
    let reader = readers::find(reader)?;
    let dir = match path {
        Some(p) => p,
        None => reader.default_watch_dir()?,
    };
    let mut writer = store.writer()?;
    let ingest = Ingest::open(store, reader.as_ref(), &dir)?;

    // First Ctrl+C stops gracefully (pending events recorded, read positions
    // saved); a second one exits immediately.
    ctrlc::set_handler(|| {
        if STOP.swap(true, Ordering::SeqCst) {
            std::process::exit(130);
        }
    })
    .context("installing Ctrl+C handler")?;

    eprintln!("[{}] watching {} (recursive)", reader.name(), dir.display());
    eprintln!(
        "events -> {}, usage -> {}; Ctrl+C to stop",
        store.path().display(),
        store.usage_db_path().display()
    );

    if let Some(every) = sync_every {
        // Fail fast on a missing login instead of erroring every interval.
        crate::cloud::auth::require(store.data_dir())?;
        eprintln!("syncing every {}s", every.as_secs());
        super::sync::spawn_loop(Store::new(store.data_dir()), every, &STOP);
    }

    if super::standup::load(store.data_dir()).is_ok_and(|c| c.at.is_some()) {
        eprintln!("standup: scheduled (see `clioilc standup schedule`)");
    }
    // Checked every minute, so a schedule set later is picked up too.
    super::standup::spawn_schedule(Store::new(store.data_dir()), &STOP);

    // Appended lines are always read (to collect usage); --follow only
    // decides whether they are also printed.
    let opts = Options {
        follow: Some(store.checkpoint_path()),
        stop: &STOP,
    };
    let mut output = Output {
        lines: follow.then(|| BufWriter::with_capacity(256 * 1024, io::stdout().lock())),
        ingest,
    };
    watcher::run(reader.as_ref(), &dir, &mut writer, opts, &mut output)
}

struct Output<'a> {
    lines: Option<BufWriter<StdoutLock<'static>>>,
    ingest: Ingest<'a>,
}

impl Output<'_> {
    /// Stdout is gone (e.g. piped into `head`): stop instead of reading for nobody.
    fn close(&mut self) {
        self.lines = None;
        STOP.store(true, Ordering::SeqCst);
    }
}

impl Handler for Output<'_> {
    fn on_line(&mut self, path: &Path, line: &[u8]) {
        if let Err(err) = self.ingest.line(path, line) {
            self.on_error(err.context("storing usage"));
        }
        let Some(out) = self.lines.as_mut() else {
            return;
        };
        let written = out
            .write_all(path.as_os_str().as_encoded_bytes())
            .and_then(|()| out.write_all(b"\t"))
            .and_then(|()| out.write_all(line))
            .and_then(|()| out.write_all(b"\n"));
        if written.is_err() {
            self.close();
        }
    }

    fn on_batch_end(&mut self) {
        if let Err(err) = self.ingest.commit() {
            self.on_error(err.context("storing usage"));
        }
        if self.lines.as_mut().is_some_and(|out| out.flush().is_err()) {
            self.close();
        }
    }

    fn on_error(&mut self, err: anyhow::Error) {
        eprintln!("watch error: {err:#}");
    }
}
