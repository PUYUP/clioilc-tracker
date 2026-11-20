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
    pub team_notes: Vec<String>,
    pub team_uploaded: usize,
}

/// Team sync runs after the personal sync, at most this often unless new
/// private data arrived.
const TEAM_EVERY: Duration = Duration::from_secs(300);

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
            Err(TryLockError::WouldBlock) => bail!("another `clioilc sync` is running"),
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
            team_notes: Vec::new(),
            team_uploaded: 0,
        };
        syncer.check_salt()?;
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

    /// Upload everything pending, batch by batch, then (E2E accounts) the
    /// team copies of linked folders.
    pub fn run(&mut self) -> Result<Totals> {
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
        loop {
            let batch = self.db.sync_claim(BATCH)?;
            if batch.is_empty() {
                return Ok(totals);
            }
            let sent = self.send(&batch);
            self.db.sync_finish(sent.is_ok(), chrono::Utc::now())?;
            let counts = sent?;
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

pub fn run(store: &Store) -> Result<()> {
    let mut syncer = Syncer::open(store)?;
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
                        eprintln!("sync error: {msg}");
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
