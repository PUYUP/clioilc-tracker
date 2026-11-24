//! `clioilc sync`: upload rows of usage.db that are not yet on the server.

use super::login::{unlock, Keyring};
use crate::cloud::auth::{self, Auth};
use crate::cloud::client::{api_error_of, Client};
use crate::cloud::e2e;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clioilc_core::usage_db::SyncBatch;
use clioilc_core::{Store, UsageDb};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{File, TryLockError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Rows per request (the server accepts up to 2,000).
const BATCH: usize = 500;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Totals {
    pub messages: u64,
    pub tool_calls: u64,
    pub private: u64,
}

impl Totals {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for Totals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} message(s), {} tool call(s), {} private row(s)",
            self.messages, self.tool_calls, self.private
        )
    }
}

#[derive(Deserialize)]
struct Accepted {
    accepted: Counts,
}

#[derive(Deserialize)]
struct Counts {
    messages: u64,
    tool_calls: u64,
    private: u64,
}

pub struct Syncer {
    client: Client,
    auth: Auth,
    db: UsageDb,
    data_key: Option<[u8; e2e::KEY_LEN]>,
    data_dir: std::path::PathBuf,
    _lock: File,
    /// E2E team sync (commands/team.rs): when it last ran, and its notes.
    last_team: Option<std::time::Instant>,
    /// Journal work items (clioilc_core::journal): when they were last
    /// rebuilt, and whether activity arrived since.
    last_journal: Option<std::time::Instant>,
    journal_dirty: bool,
    pub team_notes: Vec<String>,
    pub team_uploaded: usize,
    /// Show a progress bar while uploading (`clioilc sync`, not `watch`).
    progress: bool,
}

/// Team sync runs after the personal sync, at most this often unless new
/// private data arrived.
const TEAM_EVERY: Duration = Duration::from_secs(300);
/// Work items are rebuilt soon after new activity (a rebuild reads all
/// events and asks git, about a second), and at least every TEAM_EVERY for
/// what depends on time or on the repository (a push from a terminal).
const JOURNAL_AFTER_ACTIVITY: Duration = Duration::from_secs(30);

/// Another clioilc (usually the background service) holds the sync lock.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "another clioilc is already syncing this data (the background service? see \
             `clioilc service status`); this one keeps recording and takes over if it stops",
        )
    }
}

impl std::error::Error for Busy {}

impl Syncer {
    /// Fails if not logged in, another sync holds the lock, or usage.db was
    /// hashed with a different salt than the account's.
    pub fn open(store: &Store) -> Result<Self> {
        let data_dir = store.data_dir();
        let auth = auth::require(data_dir)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join("sync.lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(anyhow::Error::new(Busy)),
            Err(TryLockError::Error(e)) => return Err(e).context("locking sync.lock"),
        }
        let client = Client::new(&auth.server, Some(&auth.token))?;
        let data_key = match &auth.data_key {
            Some(k) => Some(e2e::to_key(&B64.decode(k)?)?),
            None => None,
        };
        let mut db = UsageDb::open(&store.usage_db_path())?;
        // Holding the lock: claims left by an interrupted run are stale.
        db.sync_release()?;

        let mut syncer = Self {
            client,
            auth,
            db,
            data_key,
            data_dir: data_dir.to_path_buf(),
            _lock: lock,
            last_team: None,
            last_journal: None,
            journal_dirty: false,
            team_notes: Vec::new(),
            team_uploaded: 0,
            progress: false,
        };
        syncer.check_salt()?;
        // History synced to another account (e.g. after logging in as a new
        // one on this machine) must go to this one too.
        let account = format!("{}#{}", syncer.auth.server, syncer.auth.username);
        if syncer.db.claim_account(&account)? {
            eprintln!(
                "first sync of this machine to {}: uploading its whole history",
                syncer.auth.username
            );
        }
        Ok(syncer)
    }

    /// Never upload ids hashed with a salt the account does not use.
    fn check_salt(&mut self) -> Result<()> {
        let ring: Keyring = self.client.get("/api/v1/users/keyring/").map_err(explain)?;
        if ring.enc_mode != self.auth.enc_mode {
            bail!("the account's encryption mode changed; run `clioilc login` again");
        }
        let salt = if ring.enc_mode == "e2e" {
            let key = self
                .data_key
                .as_ref()
                .context("missing E2E key; run `clioilc login`")?;
            e2e::open(key, &B64.decode(&ring.salt)?, e2e::AAD_SALT)
                .context("E2E key does not match the account; run `clioilc login` again")?
        } else {
            unlock(&ring)?.2
        };
        if salt != self.db.salt() {
            bail!(
                "usage.db was hashed with a different salt than your account; \
                 run `clioilc login` again to rebuild it"
            );
        }
        Ok(())
    }

    /// `clioilc login` replaced the credentials this syncer was opened with
    /// (another account, or a new token for the same one).
    pub fn login_changed(&self) -> bool {
        auth::load(&self.data_dir)
            .ok()
            .flatten()
            .is_none_or(|a| a.token != self.auth.token || a.server != self.auth.server)
    }

    /// Upload everything pending, batch by batch, then (E2E accounts) the
    /// team copies of linked folders.
    pub fn run(&mut self) -> Result<Totals> {
        self.db.resolve_commits()?;
        self.journal_dirty |= self.db.has_unsynced_activity()?;
        let due = match self.last_journal {
            None => true,
            Some(t) => {
                t.elapsed() >= TEAM_EVERY
                    || (self.journal_dirty && t.elapsed() >= JOURNAL_AFTER_ACTIVITY)
            }
        };
        if due {
            self.last_journal = Some(std::time::Instant::now());
            self.journal_dirty = false;
            self.db.build_work_items(chrono::Utc::now())?;
        }
        let totals = self.run_personal()?;
        self.team_uploaded = 0;
        let due = self.last_team.is_none_or(|t| t.elapsed() >= TEAM_EVERY) || totals.private > 0;
        if let (Some(dek), true) = (self.data_key, due) {
            self.last_team = Some(std::time::Instant::now());
            match super::team::sync(&self.client, &mut self.db, &dek, &self.data_dir) {
                Ok(report) => {
                    self.team_uploaded = report.uploaded;
                    self.team_notes = report.notes;
                }
                // Never let team sync break the personal sync.
                Err(err) => self.team_notes = vec![format!("team sync: {err:#}")],
            }
        }
        Ok(totals)
    }

    /// `clioilc team`: verify teammates, then team sync now.
    pub fn team(&mut self) -> Result<()> {
        let dek = *super::team::require_dek(self.data_key.as_ref())?;
        super::team::run(&self.client, &mut self.db, &dek, &self.data_dir)
    }

    fn run_personal(&mut self) -> Result<Totals> {
        let mut totals = Totals::default();
        let mut bar = if self.progress {
            Progress::new(self.db.unsynced_total()?)
        } else {
            None
        };
        loop {
            let batch = self.db.sync_claim(BATCH)?;
            if batch.is_empty() {
                if let Some(bar) = bar.as_mut() {
                    bar.finish();
                }
                return Ok(totals);
            }
            let rows = batch.len() as u64;
            let sent = self.send(&batch);
            self.db.sync_finish(sent.is_ok(), chrono::Utc::now())?;
            let counts = match sent {
                Ok(c) => c,
                Err(e) => {
                    if let Some(bar) = bar.as_mut() {
                        bar.finish();
                    }
                    return Err(e);
                }
            };
            if let Some(bar) = bar.as_mut() {
                bar.add(rows);
            }
            totals.messages += counts.messages;
            totals.tool_calls += counts.tool_calls;
            totals.private += counts.private;
        }
    }

    fn send(&self, batch: &SyncBatch) -> Result<Counts> {
        let private = batch
            .private
            .iter()
            .map(|p| self.private_row(p))
            .collect::<Result<Vec<_>>>()?;
        let body = json!({
            "messages": batch.messages,
            "tool_calls": batch.tool_calls,
            "private": private,
        });
        let res: Accepted = self
            .client
            .post("/api/v1/usage/sync/", &body)
            .map_err(explain)?;
        Ok(res.accepted)
    }

    fn private_row(&self, p: &clioilc_core::usage_db::SyncPrivate) -> Result<Value> {
        Ok(match &self.data_key {
            None => json!({
                "kind": p.kind, "id": p.id, "ts": p.ts, "project_id": p.project_id, "data": p.data,
            }),
            Some(key) => {
                let plain = serde_json::to_vec(&p.data)?;
                let blob = e2e::seal(key, &plain, &e2e::record_aad(p.kind, &p.id))?;
                json!({
                    "kind": p.kind,
                    "id": p.id,
                    "ts": p.ts,
                    "project_id": p.project_id,
                    "ciphertext": B64.encode(blob),
                })
            }
        })
    }
}

/// Turn the API errors a user can act on into instructions.
fn explain(err: anyhow::Error) -> anyhow::Error {
    let Some(api) = api_error_of(&err) else {
        return err;
    };
    match (api.status, api.error.as_deref()) {
        (401, _) => err.context("this device was revoked or logged out; run `clioilc login`"),
        (_, Some("keyring_missing")) => err.context("account not set up; run `clioilc login`"),
        (400, _) => {
            let detail = api.body.to_string();
            err.context(format!("server rejected the batch: {detail:.2000}"))
        }
        _ => err,
    }
}

/// Upload progress on stderr: a bar redrawn in place on a terminal, a line
/// every 10% otherwise (logs, pipes).
struct Progress {
    total: u64,
    done: u64,
    started: std::time::Instant,
    tty: bool,
    last_tenth: u64,
}

impl Progress {
    /// None when there is nothing (or very little) to show.
    fn new(total: u64) -> Option<Self> {
        use std::io::IsTerminal;
        (total > BATCH as u64).then(|| Self {
            total,
            done: 0,
            started: std::time::Instant::now(),
            tty: std::io::stderr().is_terminal(),
            last_tenth: 0,
        })
    }

    fn add(&mut self, rows: u64) {
        self.set(self.done + rows);
    }

    fn set(&mut self, done: u64) {
        self.done = done;
        // A batch can hold rows queued after the count (a running watcher).
        self.total = self.total.max(self.done);
        if self.tty {
            eprint!("\r{}", self.line());
        } else {
            let tenth = self.done * 10 / self.total.max(1);
            if tenth > self.last_tenth {
                self.last_tenth = tenth;
                eprintln!("{}", self.line());
            }
        }
    }

    fn line(&self) -> String {
        const WIDTH: usize = 30;
        let ratio = self.done as f64 / self.total.max(1) as f64;
        let filled = ((ratio * WIDTH as f64).round() as usize).min(WIDTH);
        let secs = self.started.elapsed().as_secs_f64();
        let eta = if self.done > 0 && ratio < 1.0 {
            format!(", ~{:.0}s left", secs / ratio - secs)
        } else {
            String::new()
        };
        format!(
            "[{}{}] {:>3.0}%  {}/{} rows  {:.0}s{eta}   ",
            "#".repeat(filled),
            "-".repeat(WIDTH - filled),
            ratio * 100.0,
            self.done,
            self.total,
            secs
        )
    }

    fn finish(&mut self) {
        if self.tty && self.done > 0 {
            eprintln!();
        }
    }
}

/// `clioilc sync`: upload what is pending, with a progress bar. With
/// `resend`, first mark the whole local history as not uploaded, so all of
/// it goes again (safe: the server stores rows by id).
pub fn run(store: &Store, resend: bool) -> Result<()> {
    let mut syncer = match Syncer::open(store) {
        Ok(s) => s,
        Err(e) if e.is::<Busy>() => return follow_service(store, resend),
        Err(e) => return Err(e),
    };
    syncer.progress = true;
    if resend {
        let n = syncer.db.mark_all_unsynced()?;
        eprintln!("resending the whole history: {n} row(s) queued again");
    }
    let totals = syncer.run()?;
    if totals.is_empty() {
        eprintln!("up to date ({})", syncer.auth.server);
    } else {
        eprintln!("synced {totals} to {}", syncer.auth.server);
    }
    if syncer.team_uploaded > 0 {
        eprintln!("team: {} record(s) shared", syncer.team_uploaded);
    }
    for note in &syncer.team_notes {
        eprintln!("team: {note}");
    }
    Ok(())
}

/// Background loop for `watch --sync`: sync every `every` until `stop`.
pub fn spawn_loop(store: Store, every: Duration, stop: &'static AtomicBool) {
    std::thread::spawn(move || {
        let mut syncer: Option<Syncer> = None;
        let mut last_error: Option<String> = None;
        let mut last_notes: Vec<String> = Vec::new();
        while !stop.load(Ordering::SeqCst) {
            // A new `clioilc login`: switch to it before the old token is
            // refused (login retires it).
            if syncer.as_ref().is_some_and(Syncer::login_changed) {
                syncer = None;
                last_error = None;
                eprintln!("login changed: syncing with the new login");
            }
            let result = match syncer.as_mut() {
                Some(s) => s.run(),
                None => Syncer::open(&store).and_then(|s| syncer.insert(s).run()),
            };
            if let Some(s) = syncer.as_ref() {
                if s.team_uploaded > 0 {
                    eprintln!("team: {} record(s) shared", s.team_uploaded);
                }
                // Repeat team notes (e.g. "waiting for a key") only when they change.
                if s.team_notes != last_notes {
                    for note in &s.team_notes {
                        eprintln!("team: {note}");
                    }
                    last_notes = s.team_notes.clone();
                }
            }
            match result {
                Ok(t) if !t.is_empty() => {
                    eprintln!("sync: {t}");
                    last_error = None;
                }
                Ok(_) => last_error = None,
                Err(err) => {
                    // Reopen next time (re-checks login and salt), and only
                    // repeat an error when it changes.
                    syncer = None;
                    let msg = format!("{err:#}");
                    if last_error.as_deref() != Some(&msg) {
                        // Another process holding the lock is not a failure.
                        let label = if msg.starts_with("another clioilc is already syncing") {
                            "note"
                        } else {
                            "sync error"
                        };
                        eprintln!("{label}: {msg}");
                    }
                    last_error = Some(msg);
                }
            }
            let mut slept = Duration::ZERO;
            while slept < every && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                slept += Duration::from_millis(250);
            }
        }
    });
}

/// The background service holds the sync lock: queue the history again if
/// asked, then show the service's upload progress until nothing is pending.
fn follow_service(store: &Store, resend: bool) -> Result<()> {
    const POLL: Duration = Duration::from_secs(1);
    const STALLED: Duration = Duration::from_secs(180);
    let mut db = UsageDb::open(&store.usage_db_path())?;
    if resend {
        let n = db.mark_all_unsynced()?;
        eprintln!("resending the whole history: {n} row(s) queued again");
    }
    let total = db.unsynced_total()?;
    if total == 0 {
        eprintln!("up to date (the background service keeps it so)");
        return Ok(());
    }
    eprintln!(
        "the background service is uploading {total} row(s); following its progress \
         (Ctrl-C stops watching, not the upload)"
    );
    let mut bar = Progress::new(total);
    let (mut last, mut moved) = (total, std::time::Instant::now());
    loop {
        std::thread::sleep(POLL);
        let pending = db.unsynced_total()?;
        if let Some(bar) = bar.as_mut() {
            bar.set(total.saturating_sub(pending));
        }
        if pending == 0 {
            if let Some(bar) = bar.as_mut() {
                bar.finish();
            }
            eprintln!("done: the background service uploaded {total} row(s)");
            return Ok(());
        }
        if pending < last {
            (last, moved) = (pending, std::time::Instant::now());
        } else if moved.elapsed() >= STALLED {
            if let Some(bar) = bar.as_mut() {
                bar.finish();
            }
            bail!(
                "no upload progress for {} minutes; {pending} row(s) still pending \
                 (see `clioilc service status`)",
                STALLED.as_secs() / 60
            );
        }
    }
}
