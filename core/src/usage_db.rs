//! SQLite storage for parsed usage and activity.
//!
//! Tables are split into three tiers:
//!
//! - **Public** (`messages`, `tool_calls`): metadata that is safe to share.
//!   Identifiers are keyed hashes, labels are whitelisted, and there is no
//!   content, path, working directory, branch or account information.
//! - **Private** (`private_*`): what the agent actually did (prompts, replies,
//!   thinking, tool actions, error output, real project names), for the
//!   owner only. Secrets are redacted and long text truncated before storing.
//!   Synced to the owner's account, end-to-end or server-side encrypted per
//!   the user's choice; never shown to anyone else.
//! - **Local** (`meta`): the hashing salt. Never leaves this machine.
//!
//! Public and private rows share the same hashed ids so the owner's view
//! can join them. Every synced row has a `synced_at` column (NULL = not yet
//! uploaded), reset whenever the row changes.

use crate::contract::{self, Tally};
use crate::redact::redact;
use crate::usage::{Message, Prompt, Record, Status, ToolResult, Usage};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// Longest stored prompt, reply or thinking text, in characters.
pub const MAX_TEXT: usize = 8_000;
/// Longest stored tool action summary.
pub const MAX_SUMMARY: usize = 500;
/// Longest stored tool error output.
pub const MAX_ERROR: usize = 1_000;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

-- local only: never synced ---------------------------------------------
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);

-- public: shareable metadata --------------------------------------------
CREATE TABLE IF NOT EXISTS messages (
    id                 TEXT PRIMARY KEY,  -- hash of the provider message id
    tool               TEXT NOT NULL,     -- reader name, e.g. 'claude'
    project_id         TEXT NOT NULL,     -- hash of the project name
    session_id         TEXT NOT NULL,     -- hash of the session id
    is_subagent        INTEGER NOT NULL,
    ts                 TEXT NOT NULL,     -- RFC 3339, UTC, seconds
    model              TEXT NOT NULL,
    status             TEXT NOT NULL,     -- 'ok' or an error kind
    input_tokens       INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    cache_read_tokens  INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    synced_at          TEXT,
    cache_write_5m_tokens INTEGER,         -- split of cache_write_tokens, if known
    cache_write_1h_tokens INTEGER,
    speed              TEXT               -- 'standard', 'fast', ... if known
);
CREATE INDEX IF NOT EXISTS messages_unsynced ON messages(synced_at) WHERE synced_at IS NULL;
CREATE INDEX IF NOT EXISTS messages_ts ON messages(ts);

CREATE TABLE IF NOT EXISTS tool_calls (
    id         TEXT PRIMARY KEY,  -- hash of the provider tool-call id
    message_id TEXT,              -- messages.id; NULL if only the result was seen
    name       TEXT,              -- built-in tool name, or 'mcp'
    status     TEXT,              -- 'ok', 'error', or NULL while pending
    synced_at  TEXT
);
CREATE INDEX IF NOT EXISTS tool_calls_unsynced ON tool_calls(synced_at) WHERE synced_at IS NULL;
CREATE INDEX IF NOT EXISTS tool_calls_message ON tool_calls(message_id);

-- private: owner only ---------------------------------------------------
-- Timestamps here are RFC 3339 UTC with milliseconds, for ordering.
CREATE TABLE IF NOT EXISTS private_projects (
    project_id TEXT PRIMARY KEY,      -- messages.project_id
    name       TEXT NOT NULL,         -- e.g. -Volumes-SSD1-Private-app
    synced_at  TEXT
);
CREATE TABLE IF NOT EXISTS private_prompts (
    id          TEXT PRIMARY KEY,     -- hash of the log line id
    project_id  TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    is_subagent INTEGER NOT NULL,
    ts          TEXT NOT NULL,
    text        TEXT NOT NULL,
    synced_at   TEXT,
    branch      TEXT                  -- local only: git branch the agent was on
);
CREATE INDEX IF NOT EXISTS private_prompts_ts ON private_prompts(ts);
CREATE TABLE IF NOT EXISTS private_blocks (
    id         TEXT PRIMARY KEY,      -- hash of log line id + block index
    message_id TEXT NOT NULL,         -- messages.id
    ts         TEXT NOT NULL,
    kind       TEXT NOT NULL,         -- 'text' or 'thinking'
    text       TEXT NOT NULL,
    synced_at  TEXT
);
CREATE INDEX IF NOT EXISTS private_blocks_ts ON private_blocks(ts);
CREATE TABLE IF NOT EXISTS private_tool_calls (
    id        TEXT PRIMARY KEY,       -- tool_calls.id
    ts        TEXT,
    full_name TEXT,                   -- real tool name, e.g. mcp__server__tool
    summary   TEXT,                   -- command, file path, URL, ...
    error     TEXT,                   -- error output when the call failed
    synced_at TEXT,
    commits   TEXT,                   -- JSON list of {hash, branch, subject, remote}
    -- local only, to find commits: folders, end time, done flag
    git_dirs  TEXT,                   -- JSON list of repository folders
    printed   TEXT,                   -- JSON commits as the tool output printed them
    prs       TEXT,                   -- JSON pull/merge request URLs the output printed
    branch    TEXT,                   -- git branch the agent was on
    result_ts TEXT,
    commits_checked INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS private_tool_calls_ts ON private_tool_calls(ts);

-- private: work items of the journal (journal.rs), one JSON document each
CREATE TABLE IF NOT EXISTS work_items (
    id         TEXT PRIMARY KEY,      -- keyed hash of the opening prompt
    project_id TEXT NOT NULL,
    ts         TEXT NOT NULL,         -- last activity
    data       TEXT NOT NULL,         -- JSON document, or the deleted marker
    synced_at  TEXT
);

-- local only: what team sync (E2E workspaces) already uploaded ---------
CREATE TABLE IF NOT EXISTS team_sync (
    project_pk  INTEGER NOT NULL,     -- team project id on the server
    kind        TEXT NOT NULL,        -- tool_call, prompt, block
    id          TEXT NOT NULL,
    key_version INTEGER NOT NULL,
    digest      TEXT NOT NULL,        -- sha256 of the uploaded plaintext
    PRIMARY KEY (project_pk, kind, id)
);
";

/// Tables uploaded by sync, by tier.
pub const PUBLIC_TABLES: &[&str] = &["messages", "tool_calls"];
pub const PRIVATE_TABLES: &[&str] = &[
    "private_projects",
    "private_prompts",
    "private_blocks",
    "private_tool_calls",
    "work_items",
];

const UPSERT_MESSAGE: &str = "
INSERT INTO messages (id, tool, project_id, session_id, is_subagent, ts, model, status,
                      input_tokens, cache_write_tokens, cache_read_tokens, output_tokens,
                      cache_write_5m_tokens, cache_write_1h_tokens, speed)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
ON CONFLICT(id) DO UPDATE SET
    input_tokens       = max(input_tokens, excluded.input_tokens),
    cache_write_tokens = max(cache_write_tokens, excluded.cache_write_tokens),
    cache_read_tokens  = max(cache_read_tokens, excluded.cache_read_tokens),
    output_tokens      = max(output_tokens, excluded.output_tokens),
    -- The split must describe the cache_write_tokens kept above.
    cache_write_5m_tokens = CASE
        WHEN excluded.cache_write_5m_tokens IS NOT NULL
         AND excluded.cache_write_tokens >= messages.cache_write_tokens
        THEN excluded.cache_write_5m_tokens ELSE messages.cache_write_5m_tokens END,
    cache_write_1h_tokens = CASE
        WHEN excluded.cache_write_1h_tokens IS NOT NULL
         AND excluded.cache_write_tokens >= messages.cache_write_tokens
        THEN excluded.cache_write_1h_tokens ELSE messages.cache_write_1h_tokens END,
    speed              = coalesce(excluded.speed, messages.speed),
    synced_at          = NULL
WHERE excluded.input_tokens       > messages.input_tokens
   OR excluded.cache_write_tokens > messages.cache_write_tokens
   OR excluded.cache_read_tokens  > messages.cache_read_tokens
   OR excluded.output_tokens      > messages.output_tokens
   -- Backfill details older rows lack (re-run `usage import`).
   OR (messages.cache_write_5m_tokens IS NULL AND excluded.cache_write_5m_tokens IS NOT NULL
       AND excluded.cache_write_tokens >= messages.cache_write_tokens)
   OR (messages.speed IS NULL AND excluded.speed IS NOT NULL)";

const UPSERT_TOOL_CALL: &str = "
INSERT INTO tool_calls (id, message_id, name) VALUES (?1, ?2, ?3)
ON CONFLICT(id) DO UPDATE SET
    message_id = excluded.message_id,
    name       = excluded.name,
    synced_at  = NULL
WHERE tool_calls.message_id IS NULL";

const UPSERT_TOOL_RESULT: &str = "
INSERT INTO tool_calls (id, status) VALUES (?1, ?2)
ON CONFLICT(id) DO UPDATE SET
    status    = excluded.status,
    synced_at = NULL
WHERE tool_calls.status IS NOT excluded.status";

const UPSERT_PRIVATE_TOOL_CALL: &str = "
INSERT INTO private_tool_calls (id, ts, full_name, summary) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(id) DO UPDATE SET
    ts        = excluded.ts,
    full_name = excluded.full_name,
    summary   = excluded.summary,
    synced_at = NULL
WHERE private_tool_calls.full_name IS NULL";

const UPSERT_PRIVATE_TOOL_ERROR: &str = "
INSERT INTO private_tool_calls (id, error) VALUES (?1, ?2)
ON CONFLICT(id) DO UPDATE SET
    error     = excluded.error,
    synced_at = NULL
WHERE private_tool_calls.error IS NOT excluded.error";

const ACTIVITY: &str = "
SELECT p.ts, pp.name, p.session_id, p.is_subagent, 'prompt', NULL, p.text, NULL, NULL, NULL
FROM private_prompts p
LEFT JOIN private_projects pp ON pp.project_id = p.project_id
WHERE p.ts >= ?1 AND p.ts < ?2
UNION ALL
SELECT b.ts, pp.name, m.session_id, m.is_subagent, b.kind, NULL, b.text, NULL, NULL, NULL
FROM private_blocks b
JOIN messages m ON m.id = b.message_id
LEFT JOIN private_projects pp ON pp.project_id = m.project_id
WHERE b.ts >= ?1 AND b.ts < ?2
UNION ALL
SELECT pt.ts, pp.name, m.session_id, m.is_subagent, 'tool', pt.full_name, pt.summary,
       t.status, pt.error, pt.commits
FROM private_tool_calls pt
JOIN tool_calls t ON t.id = pt.id
JOIN messages m ON m.id = t.message_id
LEFT JOIN private_projects pp ON pp.project_id = m.project_id
WHERE pt.ts >= ?1 AND pt.ts < ?2
ORDER BY 1";

/// What to group a [`UsageDb::report`] by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Model,
    Project,
    Day,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    pub key: String,
    pub messages: u64,
    pub errors: u64,
    pub usage: Usage,
    pub tool_calls: u64,
    pub tool_errors: u64,
}

/// One entry of the owner's activity timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    pub ts: DateTime<Utc>,
    /// Real project name, if known.
    pub project: Option<String>,
    /// Hashed session id (groups items of one conversation).
    pub session_id: String,
    pub is_subagent: bool,
    pub kind: ActivityKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityKind {
    Prompt,
    Text,
    Thinking,
    Tool {
        name: String,
        /// `ok`, `error`, or `None` while the result is pending.
        status: Option<String>,
        error: Option<String>,
        /// Git commits the action made.
        commits: Vec<crate::git::Commit>,
    },
}

pub struct UsageDb {
    conn: Connection,
    salt: Vec<u8>,
    in_tx: bool,
    /// Records that broke the reader contract since the last
    /// [`UsageDb::take_contract`] (see [`crate::contract`]).
    contract: Tally,
}

impl UsageDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // `watch` and `sync` may write at the same time from two connections.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(SCHEMA)
            .context("creating usage schema")?;
        migrate(&conn).context("upgrading usage schema")?;
        let salt = load_or_create_salt(&conn)?;
        Ok(Self {
            conn,
            salt,
            in_tx: false,
            contract: Tally::default(),
        })
    }

    /// Store one record parsed from a log of `tool` (the reader name).
    /// Writes are batched in a transaction until [`UsageDb::commit`].
    ///
    /// The record is checked against the reader contract first: one that
    /// would corrupt the data is skipped, and broken rules are counted (see
    /// [`UsageDb::take_contract`]).
    pub fn insert(&mut self, tool: &str, project: Option<&str>, record: &Record) -> Result<()> {
        let problems = contract::check(tool, project, record, Utc::now());
        if self.contract.add(&problems) {
            return Ok(());
        }
        if !self.in_tx {
            self.conn.execute_batch("BEGIN")?;
            self.in_tx = true;
        }
        let project = project.unwrap_or("-");
        match record {
            Record::Message(m) => self.insert_message(tool, project, m),
            Record::ToolResult(r) => self.insert_tool_result(r),
            Record::Prompt(p) => self.insert_prompt(project, p),
        }
    }

    /// Broken reader-contract rules counted since the last call.
    pub fn take_contract(&mut self) -> Tally {
        std::mem::take(&mut self.contract)
    }

    pub fn commit(&mut self) -> Result<()> {
        if std::mem::take(&mut self.in_tx) {
            self.conn.execute_batch("COMMIT")?;
        }
        Ok(())
    }

    fn insert_message(&mut self, tool: &str, project: &str, m: &Message) -> Result<()> {
        let id = self.hash("message", &m.id);
        let project_id = self.insert_project(project)?;
        let session_id = self.hash("session", &m.session_id);
        let status = match &m.status {
            Status::Ok => "ok".to_string(),
            Status::Error(kind) => label(kind, "api_error"),
        };
        // The cache-write split, only when it adds up (the server refuses others).
        let split = match (m.usage.cache_write_5m, m.usage.cache_write_1h) {
            (Some(a), Some(b)) if a.checked_add(b) == Some(m.usage.cache_write) => Some((a, b)),
            _ => None,
        };
        self.conn.prepare_cached(UPSERT_MESSAGE)?.execute(params![
            id,
            label(tool, "unknown"),
            project_id,
            session_id,
            m.is_subagent,
            m.timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
            label(&m.model, "unknown"),
            status,
            to_i64(m.usage.input),
            to_i64(m.usage.cache_write),
            to_i64(m.usage.cache_read),
            to_i64(m.usage.output),
            split.map(|(w5, _)| to_i64(w5)),
            split.map(|(_, w1)| to_i64(w1)),
            m.speed.as_deref().map(|s| label(s, "unknown")),
        ])?;

        let ts = private_ts(m.timestamp);
        for (i, block) in m.blocks.iter().enumerate() {
            let block_id = self.hash("block", &format!("{}#{i}", m.line_id));
            self.conn
                .prepare_cached(
                    "INSERT OR IGNORE INTO private_blocks (id, message_id, ts, kind, text)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    block_id,
                    id,
                    ts,
                    block.kind.as_str(),
                    private_text(&block.text, MAX_TEXT)
                ])?;
        }
        for call in &m.tool_calls {
            let call_id = self.hash("tool_call", &call.id);
            self.conn
                .prepare_cached(UPSERT_TOOL_CALL)?
                .execute(params![call_id, id, label(&call.name, "other")])?;
            self.conn
                .prepare_cached(UPSERT_PRIVATE_TOOL_CALL)?
                .execute(params![
                    call_id,
                    ts,
                    truncate(&call.full_name, 128),
                    private_text(&call.summary, MAX_SUMMARY),
                ])?;
            if let Some(branch) = &m.branch {
                self.conn
                    .prepare_cached(
                        "UPDATE private_tool_calls SET branch = ?2 WHERE id = ?1 AND branch IS NULL",
                    )?
                    .execute(params![call_id, truncate(branch, 250)])?;
            }
            if !call.git_dirs.is_empty() {
                // Local only: no re-sync. Backfilled by a later `usage import`.
                self.conn
                    .prepare_cached(
                        "UPDATE private_tool_calls SET git_dirs = ?2, commits_checked = 0
                         WHERE id = ?1 AND git_dirs IS NULL",
                    )?
                    .execute(params![call_id, serde_json::to_string(&call.git_dirs)?])?;
            }
        }
        Ok(())
    }

    fn insert_tool_result(&mut self, r: &ToolResult) -> Result<()> {
        let id = self.hash("tool_call", &r.tool_call_id);
        let status = if r.is_error { "error" } else { "ok" };
        self.conn
            .prepare_cached(UPSERT_TOOL_RESULT)?
            .execute(params![id, status])?;
        if let Some(error) = r.error_text.as_deref().filter(|_| r.is_error) {
            self.conn
                .prepare_cached(UPSERT_PRIVATE_TOOL_ERROR)?
                .execute(params![id, private_text(error, MAX_ERROR)])?;
        }
        if let Some(ts) = r
            .timestamp
            .filter(|t| *t <= Utc::now() + chrono::Duration::days(1))
        {
            self.conn
                .prepare_cached(
                    "UPDATE private_tool_calls SET result_ts = ?2
                     WHERE id = ?1 AND result_ts IS NULL",
                )?
                .execute(params![id, private_ts(ts)])?;
        }
        let prs: Vec<&String> = r.prs.iter().filter(|u| contract::is_pr_url(u)).collect();
        if !prs.is_empty() {
            self.conn
                .prepare_cached(
                    "UPDATE private_tool_calls SET prs = ?2, synced_at = NULL
                     WHERE id = ?1 AND prs IS NULL",
                )?
                .execute(params![id, serde_json::to_string(&prs)?])?;
        }
        if r.commits.iter().any(|c| contract::is_commit_hash(&c.hash)) {
            // As printed; resolve_commits() completes them from the repository.
            let commits: Vec<_> = r
                .commits
                .iter()
                .filter(|c| contract::is_commit_hash(&c.hash))
                .map(|c| crate::git::Commit {
                    subject: private_text(&c.subject, 300),
                    ..c.clone()
                })
                .collect();
            let json = serde_json::to_string(&commits)?;
            self.conn
                .prepare_cached(
                    "UPDATE private_tool_calls SET printed = ?2 WHERE id = ?1 AND printed IS NULL",
                )?
                .execute(params![id, json])?;
            self.conn
                .prepare_cached(
                    "UPDATE private_tool_calls SET commits = ?2, synced_at = NULL
                     WHERE id = ?1 AND commits IS NULL",
                )?
                .execute(params![id, json])?;
        }
        Ok(())
    }

    /// Complete the commits of finished shell commands that ran git in a
    /// repository on this machine (see [`crate::git`]). Each call is looked
    /// at once. Returns how many tool calls got commits.
    pub fn resolve_commits(&mut self) -> Result<usize> {
        self.commit()?;
        let pending: Vec<PendingCommits> = {
            let mut stmt = self.conn.prepare(
                "SELECT p.id, p.ts, p.result_ts, p.git_dirs, p.commits, t.status, p.printed
                 FROM private_tool_calls p JOIN tool_calls t ON t.id = p.id
                 WHERE p.git_dirs IS NOT NULL AND p.commits_checked = 0
                   AND t.status IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let epoch = |ts: &Option<String>| {
            ts.as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp())
        };
        let mut found = 0;
        for (id, ts, result_ts, dirs, current, status, printed) in pending {
            // What the output printed, not an earlier lookup: that may be redone.
            let printed = parse_commits(printed);
            let commits = match (status.as_deref(), epoch(&ts)) {
                (Some("ok"), Some(from)) => {
                    let dirs: Vec<String> = serde_json::from_str(&dirs).unwrap_or_default();
                    let to = epoch(&result_ts).unwrap_or(from + 600);
                    crate::git::resolve(&printed, &dirs, from, to)
                }
                _ => printed,
            };
            let json = (!commits.is_empty())
                .then(|| serde_json::to_string(&commits))
                .transpose()?;
            if json != current {
                self.conn.execute(
                    "UPDATE private_tool_calls
                     SET commits = ?2, commits_checked = 1, synced_at = NULL WHERE id = ?1",
                    params![id, json],
                )?;
            } else {
                self.conn.execute(
                    "UPDATE private_tool_calls SET commits_checked = 1 WHERE id = ?1",
                    params![id],
                )?;
            }
            if json.is_some() {
                found += 1;
            }
        }
        Ok(found)
    }

    fn insert_prompt(&mut self, project: &str, p: &Prompt) -> Result<()> {
        let id = self.hash("prompt", &p.id);
        let project_id = self.insert_project(project)?;
        let session_id = self.hash("session", &p.session_id);
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO private_prompts
                     (id, project_id, session_id, is_subagent, ts, text)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                id,
                project_id,
                session_id,
                p.is_subagent,
                private_ts(p.timestamp),
                private_text(&p.text, MAX_TEXT)
            ])?;
        if let Some(branch) = &p.branch {
            // Local only; backfilled by a later `usage import`.
            self.conn
                .prepare_cached(
                    "UPDATE private_prompts SET branch = ?2 WHERE id = ?1 AND branch IS NULL",
                )?
                .execute(params![id, truncate(branch, 250)])?;
        }
        Ok(())
    }

    /// Returns the hashed project id, recording its real name privately.
    fn insert_project(&mut self, project: &str) -> Result<String> {
        let project_id = self.hash("project", project);
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO private_projects (project_id, name) VALUES (?1, ?2)",
            )?
            .execute(params![
                project_id,
                truncate_plain(project, contract::MAX_PROJECT)
            ])?;
        Ok(project_id)
    }

    pub fn report(&self, by: GroupBy) -> Result<Vec<ReportRow>> {
        let key = match by {
            GroupBy::Model => "m.model",
            GroupBy::Project => "coalesce(p.name, m.project_id)",
            GroupBy::Day => "substr(m.ts, 1, 10)",
        };
        let sql = format!(
            "WITH tc AS (
                 SELECT message_id, count(*) AS n, sum(status = 'error') AS e
                 FROM tool_calls WHERE message_id IS NOT NULL GROUP BY message_id)
             SELECT {key} AS k, count(*), sum(m.status <> 'ok'),
                    sum(m.input_tokens), sum(m.cache_write_tokens),
                    sum(m.cache_read_tokens), sum(m.output_tokens),
                    coalesce(sum(tc.n), 0), coalesce(sum(tc.e), 0)
             FROM messages m
             LEFT JOIN tc ON tc.message_id = m.id
             LEFT JOIN private_projects p ON p.project_id = m.project_id
             GROUP BY k ORDER BY {order}",
            order = if by == GroupBy::Day {
                "k"
            } else {
                "sum(m.output_tokens) DESC"
            },
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let n = |i| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok(ReportRow {
                key: r.get(0)?,
                messages: n(1)?,
                errors: n(2)?,
                usage: Usage {
                    input: n(3)?,
                    cache_write: n(4)?,
                    cache_read: n(5)?,
                    output: n(6)?,
                    ..Usage::default()
                },
                tool_calls: n(7)?,
                tool_errors: n(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The owner's timeline between `from` (inclusive) and `to` (exclusive).
    pub fn activity(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<ActivityItem>> {
        let mut stmt = self.conn.prepare(ACTIVITY)?;
        let rows = stmt.query_map(params![private_ts(from), private_ts(to)], |r| {
            let ts: String = r.get(0)?;
            let kind: String = r.get(4)?;
            let kind = match kind.as_str() {
                "prompt" => ActivityKind::Prompt,
                "thinking" => ActivityKind::Thinking,
                "tool" => ActivityKind::Tool {
                    name: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    status: r.get(7)?,
                    error: r.get(8)?,
                    commits: parse_commits(r.get(9)?),
                },
                _ => ActivityKind::Text,
            };
            Ok(ActivityItem {
                ts: DateTime::parse_from_rfc3339(&ts)
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or_default(),
                project: r.get(1)?,
                session_id: r.get(2)?,
                is_subagent: r.get(3)?,
                kind,
                text: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Rows waiting to be uploaded, per table.
    pub fn unsynced(&self) -> Result<Vec<(&'static str, u64)>> {
        PUBLIC_TABLES
            .iter()
            .chain(PRIVATE_TABLES)
            .map(|table| {
                let sql = format!("SELECT count(*) FROM {table} WHERE synced_at IS NULL");
                let n = self.conn.query_row(&sql, [], |r| r.get::<_, i64>(0))?;
                Ok((*table, n.max(0) as u64))
            })
            .collect()
    }

    /// Make `account` (server + user) the one this database syncs to. When
    /// it was another account, or none was recorded yet, every row is marked
    /// unsent so the account gets this machine's whole history (uploads are
    /// idempotent). Returns whether that happened.
    pub fn claim_account(&mut self, account: &str) -> Result<bool> {
        self.commit()?;
        let current: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'sync_account'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if current.as_deref() == Some(account.as_bytes()) {
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        for table in PUBLIC_TABLES.iter().chain(PRIVATE_TABLES) {
            tx.execute(&format!("UPDATE {table} SET synced_at = NULL"), [])?;
        }
        tx.execute("DELETE FROM team_sync", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('sync_account', ?1)",
            [account.as_bytes()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// A local-only value (never synced), e.g. a reader's read position.
    pub fn local_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_local_value(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The hashing salt. Shared by all devices of an account once logged in.
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// `messages.project_id` of an agent project name (e.g. Claude's
    /// `-Users-me-code-app`), whether or not it has data yet.
    pub fn project_key(&self, name: &str) -> String {
        self.hash("project", name)
    }

    /// Message count and first message time (RFC 3339) of a project.
    pub fn project_activity(&self, project_id: &str) -> Result<(u64, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), MIN(ts) FROM messages WHERE project_id = ?1",
            [project_id],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
        )?)
    }

    /// Known projects whose name starts with `name-` (subfolders, or
    /// siblings like `app-v2`: the name cannot tell them apart), as
    /// (name, project_id), sorted by name.
    pub fn projects_under(&self, name: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, project_id FROM private_projects
             WHERE substr(name, 1, length(?1) + 1) = ?1 || '-' ORDER BY name",
        )?;
        let rows = stmt
            .query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Private records of `project_id` for team sync, from `since` (RFC 3339)
    /// on: tool actions, plus prompts and replies when `text` is true.
    /// Records without a timestamp count only when there is no `since`.
    pub fn team_candidates(
        &self,
        project_id: &str,
        since: Option<&str>,
        text: bool,
    ) -> Result<Vec<TeamCandidate>> {
        use serde_json::json;
        let since = since.unwrap_or("");
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.ts, p.full_name, p.summary, p.error, p.commits, p.prs
             FROM private_tool_calls p
             JOIN tool_calls t ON t.id = p.id
             JOIN messages m ON m.id = t.message_id
             WHERE m.project_id = ?1 AND (?2 = '' OR p.ts >= ?2)",
        )?;
        let rows = stmt.query_map(params![project_id, since], |r| {
            Ok(TeamCandidate {
                kind: "tool_call",
                id: r.get(0)?,
                ts: r.get(1)?,
                data: tool_call_data(r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?),
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        // Work items: facts always (like tool actions); the title (from a
        // prompt) and error samples only with text sharing.
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, data FROM work_items
             WHERE project_id = ?1 AND (?2 = '' OR ts >= ?2)",
        )?;
        let rows = stmt.query_map(params![project_id, since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, ts, data) = row?;
            let mut data: serde_json::Value = serde_json::from_str(&data)?;
            if !text && data.get("deleted").is_none() {
                data["title"] = data["commits"][0]["subject"]
                    .as_str()
                    .or(data["branch"].as_str())
                    .map_or(serde_json::Value::Null, |t| json!(t));
                if let Some(errors) = data["errors"].as_array_mut() {
                    for e in errors {
                        e["sample"] = serde_json::Value::Null;
                    }
                }
            }
            out.push(TeamCandidate {
                kind: "work_item",
                id,
                ts: Some(ts),
                data,
            });
        }
        if !text {
            return Ok(out);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, session_id, is_subagent, text FROM private_prompts
             WHERE project_id = ?1 AND (?2 = '' OR ts >= ?2)",
        )?;
        let rows = stmt.query_map(params![project_id, since], |r| {
            Ok(TeamCandidate {
                kind: "prompt",
                id: r.get(0)?,
                ts: r.get(1)?,
                // project_id is set by team sync to the team project's id.
                data: json!({
                    "session_id": r.get::<_, String>(2)?,
                    "is_subagent": r.get::<_, bool>(3)?,
                    "text": r.get::<_, String>(4)?,
                }),
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        let mut stmt = self.conn.prepare(
            "SELECT b.id, b.ts, b.message_id, b.kind, b.text
             FROM private_blocks b JOIN messages m ON m.id = b.message_id
             WHERE m.project_id = ?1 AND (?2 = '' OR b.ts >= ?2)",
        )?;
        let rows = stmt.query_map(params![project_id, since], |r| {
            Ok(TeamCandidate {
                kind: "block",
                id: r.get(0)?,
                ts: r.get(1)?,
                data: json!({
                    "message_id": r.get::<_, String>(2)?,
                    "kind": r.get::<_, String>(3)?,
                    "text": r.get::<_, String>(4)?,
                }),
            })
        })?;
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// What team sync already uploaded for a team project: (kind, id) ->
    /// (key version, digest).
    pub fn team_sent(&self, project_pk: u64) -> Result<HashMap<(String, String), (u64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, id, key_version, digest FROM team_sync WHERE project_pk = ?1")?;
        let rows = stmt.query_map([project_pk as i64], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                (r.get::<_, i64>(2)? as u64, r.get::<_, String>(3)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn team_mark(
        &mut self,
        project_pk: u64,
        version: u64,
        sent: &[(String, String, String)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO team_sync (project_pk, kind, id, key_version, digest)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (project_pk, kind, id)
                 DO UPDATE SET key_version = excluded.key_version, digest = excluded.digest",
            )?;
            for (kind, id, digest) in sent {
                stmt.execute(params![project_pk as i64, kind, id, version as i64, digest])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget uploads of these kinds (the server dropped them), so they are
    /// sent again if sharing is turned back on.
    pub fn team_forget(&mut self, project_pk: u64, kinds: &[&str]) -> Result<()> {
        for kind in kinds {
            self.conn.execute(
                "DELETE FROM team_sync WHERE project_pk = ?1 AND kind = ?2",
                params![project_pk as i64, kind],
            )?;
        }
        Ok(())
    }

    /// Name of a known project, by `project_id`.
    pub fn project_name(&self, project_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT name FROM private_projects WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// True when no synced table has any row (a fresh database).
    pub fn is_empty(&self) -> Result<bool> {
        for table in PUBLIC_TABLES.iter().chain(PRIVATE_TABLES) {
            let sql = format!("SELECT EXISTS (SELECT 1 FROM {table})");
            if self.conn.query_row(&sql, [], |r| r.get::<_, bool>(0))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Use the account's salt instead of the one created for this machine.
    /// Only allowed while empty: existing ids were hashed with the old salt
    /// and cannot be re-hashed (the raw values are not stored).
    pub fn replace_salt(&mut self, salt: &[u8]) -> Result<()> {
        anyhow::ensure!(salt.len() >= 16, "salt too short");
        anyhow::ensure!(
            self.is_empty()?,
            "cannot change the salt of a non-empty usage.db"
        );
        self.commit()?;
        self.conn
            .execute("UPDATE meta SET value = ?1 WHERE key = 'salt'", [salt])?;
        self.salt = salt.to_vec();
        Ok(())
    }

    /// Release rows left claimed by an interrupted sync. Call only while
    /// holding the sync lock.
    pub fn sync_release(&mut self) -> Result<()> {
        self.commit()?;
        for table in PUBLIC_TABLES.iter().chain(PRIVATE_TABLES) {
            self.conn.execute(
                &format!("UPDATE {table} SET synced_at = NULL WHERE synced_at = '{SENDING}'"),
                [],
            )?;
        }
        Ok(())
    }

    /// Claim up to `limit` unsynced rows (across all tables) for upload.
    ///
    /// Claimed rows are marked `sending`. A row that changes while in
    /// flight is reset to NULL by its upsert, so [`UsageDb::sync_finish`]
    /// will not mark it synced and the new version goes in the next batch.
    /// New prompts, replies or tool calls not uploaded yet: work items may
    /// have changed.
    pub fn has_unsynced_activity(&mut self) -> Result<bool> {
        self.commit()?;
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM private_prompts WHERE synced_at IS NULL)
                 OR EXISTS(SELECT 1 FROM private_blocks WHERE synced_at IS NULL)
                 OR EXISTS(SELECT 1 FROM private_tool_calls WHERE synced_at IS NULL)",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn sync_claim(&mut self, limit: usize) -> Result<SyncBatch> {
        self.commit()?;
        let tx = self.conn.transaction()?;
        let mut left = limit;
        for table in PUBLIC_TABLES.iter().chain(PRIVATE_TABLES) {
            if left == 0 {
                break;
            }
            left -= tx.execute(
                &format!(
                    "UPDATE {table} SET synced_at = '{SENDING}' WHERE rowid IN \
                     (SELECT rowid FROM {table} WHERE synced_at IS NULL LIMIT ?1)"
                ),
                [left as i64],
            )?;
        }
        let batch = read_claimed(&tx)?;
        tx.commit()?;
        Ok(batch)
    }

    /// After an upload: mark claimed rows synced (`ok`) or release them.
    pub fn sync_finish(&mut self, ok: bool, now: DateTime<Utc>) -> Result<()> {
        self.commit()?;
        let value = ok.then(|| now.to_rfc3339_opts(SecondsFormat::Secs, true));
        let tx = self.conn.transaction()?;
        for table in PUBLIC_TABLES.iter().chain(PRIVATE_TABLES) {
            tx.execute(
                &format!("UPDATE {table} SET synced_at = ?1 WHERE synced_at = '{SENDING}'"),
                [&value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Keyed hash: stable on this machine, unlinkable to the raw value
    /// without the local salt. 128 bits, hex encoded.
    fn hash(&self, kind: &str, raw: &str) -> String {
        let digest = Sha256::new()
            .chain_update(&self.salt)
            .chain_update(kind.as_bytes())
            .chain_update([0])
            .chain_update(raw.as_bytes())
            .finalize();
        digest[..16].iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl Drop for UsageDb {
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

/// `synced_at` value of rows claimed by a sync in progress.
const SENDING: &str = "sending";

/// Rows claimed by [`UsageDb::sync_claim`], shaped like the sync API (v1).
#[derive(Debug, Default, Serialize)]
pub struct SyncBatch {
    pub messages: Vec<SyncMessage>,
    pub tool_calls: Vec<SyncToolCall>,
    #[serde(skip)]
    pub private: Vec<SyncPrivate>,
}

impl SyncBatch {
    pub fn len(&self) -> usize {
        self.messages.len() + self.tool_calls.len() + self.private.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Serialize)]
pub struct SyncMessage {
    pub id: String,
    pub tool: String,
    pub project_id: String,
    pub session_id: String,
    pub is_subagent: bool,
    pub ts: String,
    pub model: String,
    pub status: String,
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_5m_tokens: Option<i64>,
    pub cache_write_1h_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SyncToolCall {
    pub id: String,
    pub message_id: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
}

/// A record team sync may share (docs/e2e-teams.md, "Berbagi data").
#[derive(Debug)]
pub struct TeamCandidate {
    pub kind: &'static str,
    pub id: String,
    pub ts: Option<String>,
    pub data: serde_json::Value,
}

/// A private row: `kind` and `id` stay in clear, `data` gets encrypted
/// (by the server or, end-to-end, by the client).
#[derive(Debug)]
pub struct SyncPrivate {
    /// `project`, `prompt`, `block`, `tool_call` or `work_item`.
    pub kind: &'static str,
    pub id: String,
    pub ts: Option<String>,
    /// The record's project (same hash as `messages.project_id`), sent in
    /// clear so the server can scope records to a project without opening
    /// them. None when the message it belongs to is not known locally.
    pub project_id: Option<String>,
    pub data: serde_json::Value,
}

fn read_claimed(conn: &Connection) -> Result<SyncBatch> {
    use serde_json::json;
    let mut batch = SyncBatch::default();
    let claimed = format!("WHERE synced_at = '{SENDING}'");

    let mut stmt = conn.prepare(&format!(
        "SELECT id, tool, project_id, session_id, is_subagent, ts, model, status, input_tokens,
                cache_write_tokens, cache_read_tokens, output_tokens,
                cache_write_5m_tokens, cache_write_1h_tokens, speed FROM messages {claimed}"
    ))?;
    batch.messages = stmt
        .query_map([], |r| {
            Ok(SyncMessage {
                id: r.get(0)?,
                tool: r.get(1)?,
                project_id: r.get(2)?,
                session_id: r.get(3)?,
                is_subagent: r.get(4)?,
                ts: r.get(5)?,
                model: r.get(6)?,
                status: r.get(7)?,
                input_tokens: r.get(8)?,
                cache_write_tokens: r.get(9)?,
                cache_read_tokens: r.get(10)?,
                output_tokens: r.get(11)?,
                cache_write_5m_tokens: r.get(12)?,
                cache_write_1h_tokens: r.get(13)?,
                speed: r.get(14)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut stmt = conn.prepare(&format!(
        "SELECT id, message_id, name, status FROM tool_calls {claimed}"
    ))?;
    batch.tool_calls = stmt
        .query_map([], |r| {
            Ok(SyncToolCall {
                id: r.get(0)?,
                message_id: r.get(1)?,
                name: r.get(2)?,
                status: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut push = |sql: &str, f: &dyn Fn(&rusqlite::Row) -> rusqlite::Result<SyncPrivate>| {
        // `sql` names the private table `p`; joined tables have synced_at too.
        let mut stmt = conn.prepare(&format!("{sql} WHERE p.synced_at = '{SENDING}'"))?;
        let rows = stmt.query_map([], |r| f(r))?;
        for row in rows {
            batch.private.push(row?);
        }
        anyhow::Ok(())
    };
    push(
        "SELECT p.project_id, p.name FROM private_projects p",
        &|r| {
            Ok(SyncPrivate {
                kind: "project",
                id: r.get(0)?,
                ts: None,
                project_id: Some(r.get(0)?),
                data: json!({ "name": r.get::<_, String>(1)? }),
            })
        },
    )?;
    push(
        "SELECT p.id, p.ts, p.project_id, p.session_id, p.is_subagent, p.text
         FROM private_prompts p",
        &|r| {
            Ok(SyncPrivate {
                kind: "prompt",
                id: r.get(0)?,
                ts: r.get(1)?,
                project_id: Some(r.get(2)?),
                data: json!({
                    "project_id": r.get::<_, String>(2)?,
                    "session_id": r.get::<_, String>(3)?,
                    "is_subagent": r.get::<_, bool>(4)?,
                    "text": r.get::<_, String>(5)?,
                }),
            })
        },
    )?;
    push(
        "SELECT p.id, p.ts, p.message_id, p.kind, p.text, m.project_id
         FROM private_blocks p LEFT JOIN messages m ON m.id = p.message_id",
        &|r| {
            Ok(SyncPrivate {
                kind: "block",
                id: r.get(0)?,
                ts: r.get(1)?,
                project_id: r.get(5)?,
                data: json!({
                    "message_id": r.get::<_, String>(2)?,
                    "kind": r.get::<_, String>(3)?,
                    "text": r.get::<_, String>(4)?,
                }),
            })
        },
    )?;
    push(
        "SELECT p.id, p.ts, p.full_name, p.summary, p.error, m.project_id, p.commits, p.prs
         FROM private_tool_calls p
         LEFT JOIN tool_calls t ON t.id = p.id
         LEFT JOIN messages m ON m.id = t.message_id",
        &|r| {
            Ok(SyncPrivate {
                kind: "tool_call",
                id: r.get(0)?,
                ts: r.get(1)?,
                project_id: r.get(5)?,
                data: tool_call_data(r.get(2)?, r.get(3)?, r.get(4)?, r.get(6)?, r.get(7)?),
            })
        },
    )?;
    push(
        "SELECT p.id, p.ts, p.project_id, p.data FROM work_items p",
        &|r| {
            let data: String = r.get(3)?;
            Ok(SyncPrivate {
                kind: "work_item",
                id: r.get(0)?,
                ts: r.get(1)?,
                project_id: r.get(2)?,
                data: serde_json::from_str(&data).unwrap_or_else(|_| json!({ "deleted": true })),
            })
        },
    )?;
    Ok(batch)
}

/// Columns added after the first release, for databases created before.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("messages", "cache_write_5m_tokens", "INTEGER"),
    ("messages", "cache_write_1h_tokens", "INTEGER"),
    ("messages", "speed", "TEXT"),
    ("private_tool_calls", "commits", "TEXT"),
    ("private_tool_calls", "git_dirs", "TEXT"),
    ("private_tool_calls", "result_ts", "TEXT"),
    (
        "private_tool_calls",
        "commits_checked",
        "INTEGER NOT NULL DEFAULT 0",
    ),
    ("private_tool_calls", "printed", "TEXT"),
    ("private_tool_calls", "prs", "TEXT"),
    ("private_tool_calls", "branch", "TEXT"),
    ("private_prompts", "branch", "TEXT"),
];

/// Version of `git::commit_dirs`. When it changes, the folders of earlier
/// shell commands are worked out again on the next `usage import`.
const GIT_DIRS_VERSION: i64 = 5;

fn migrate(conn: &Connection) -> Result<()> {
    migrate_columns(conn)?;
    // Servers before 2026-11-21 dropped commit sizes (files, insertions,
    // deletions) of server-mode accounts: send the rows carrying them again,
    // once (uploads are idempotent).
    let resent: Option<Vec<u8>> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'commit_stats_resent'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if resent.is_none() {
        conn.execute_batch(
            "UPDATE private_tool_calls SET synced_at = NULL WHERE commits IS NOT NULL;
             UPDATE work_items SET synced_at = NULL;
             INSERT OR REPLACE INTO meta (key, value) VALUES ('commit_stats_resent', x'01');",
        )?;
    }
    let version: i64 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'git_dirs_version'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if version < GIT_DIRS_VERSION {
        conn.execute_batch(
            "UPDATE private_tool_calls SET git_dirs = NULL, commits_checked = 0
             WHERE git_dirs IS NOT NULL",
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('git_dirs_version', ?1)",
            [GIT_DIRS_VERSION],
        )?;
    }
    Ok(())
}

fn migrate_columns(conn: &Connection) -> Result<()> {
    for (table, column, kind) in MIGRATIONS {
        let exists: bool = conn.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1)"),
            [column],
            |r| r.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
        }
    }
    Ok(())
}

/// A finished shell command waiting for its commits to be looked up:
/// (id, start, end, git_dirs JSON, commits JSON, status, printed JSON).
type PendingCommits = (
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The private data of a tool call as uploaded; `commits` only when there are some.
fn tool_call_data(
    full_name: Option<String>,
    summary: Option<String>,
    error: Option<String>,
    commits: Option<String>,
    prs: Option<String>,
) -> serde_json::Value {
    let mut data =
        serde_json::json!({ "full_name": full_name, "summary": summary, "error": error });
    let commits = parse_commits(commits);
    if !commits.is_empty() {
        data["commits"] = serde_json::json!(commits);
    }
    let prs: Vec<String> = prs
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !prs.is_empty() {
        data["prs"] = serde_json::json!(prs);
    }
    data
}

fn parse_commits(json: Option<String>) -> Vec<crate::git::Commit> {
    json.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn private_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Redact secrets first (so truncation cannot split a secret and keep half
/// of it), then cap the length.
fn private_text(text: &str, max_chars: usize) -> String {
    truncate(&redact(text), max_chars)
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}…[truncated]", &text[..cut]),
    }
}

/// A short identifier-like label (model, tool name, error kind); anything
/// else is replaced by `fallback` so free text can never be stored.
fn label(raw: &str, fallback: &str) -> String {
    if contract::is_label(raw) {
        raw
    } else {
        fallback
    }
    .to_string()
}

/// The first `max_chars` characters, without a marker.
fn truncate_plain(text: &str, max_chars: usize) -> &str {
    text.char_indices()
        .nth(max_chars)
        .map_or(text, |(cut, _)| &text[..cut])
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn load_or_create_salt(conn: &Connection) -> Result<Vec<u8>> {
    let existing: Option<Vec<u8>> = conn
        .query_row("SELECT value FROM meta WHERE key = 'salt'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(salt) = existing {
        return Ok(salt);
    }
    let salt = random_bytes()?;
    conn.execute("INSERT INTO meta (key, value) VALUES ('salt', ?1)", [&salt])?;
    Ok(salt)
}

fn random_bytes() -> Result<Vec<u8>> {
    use std::io::Read;
    let mut salt = vec![0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut salt))
        .context("reading /dev/urandom for the hashing salt")?;
    Ok(salt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{TextBlock, TextKind, ToolCall};
    use chrono::TimeZone;

    fn db() -> UsageDb {
        let path = std::env::temp_dir().join(format!(
            "clioilc-usage-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
        UsageDb::open(&path).unwrap()
    }

    fn at(secs: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 1, 0, secs).unwrap()
    }

    #[test]
    fn records_that_break_the_contract_are_skipped_or_cleaned() {
        let mut db = db();
        let count = |db: &UsageDb, table: &str| -> i64 {
            db.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        // An empty message id would merge unrelated responses: skipped.
        let Record::Message(mut broken) = message("", "l1", 5, "hi", &[]) else {
            unreachable!()
        };
        db.insert("claude", Some("p"), &Record::Message(broken.clone()))
            .unwrap();
        assert_eq!(count(&db, "messages"), 0);
        // A split that does not add up: stored without it (the server refuses it).
        broken.id = "m1".into();
        broken.model = "Opus 5.5".into();
        broken.usage.cache_write_5m = Some(3);
        broken.usage.cache_write_1h = Some(3);
        db.insert("claude", Some("p"), &Record::Message(broken))
            .unwrap();
        // Commits and PR links the server would refuse are dropped.
        db.insert(
            "claude",
            None,
            &Record::ToolResult(ToolResult {
                tool_call_id: "t1".into(),
                is_error: false,
                error_text: None,
                timestamp: None,
                commits: vec![crate::git::Commit {
                    hash: "NOT-A-HASH".into(),
                    ..Default::default()
                }],
                prs: vec!["see PR 12".into()],
            }),
        )
        .unwrap();
        db.commit().unwrap();
        let (model, w5): (String, Option<i64>) = db
            .conn
            .query_row(
                "SELECT model, cache_write_5m_tokens FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((model.as_str(), w5), ("unknown", None));
        let (commits, prs): (Option<String>, Option<String>) = db
            .conn
            .query_row("SELECT commits, prs FROM private_tool_calls", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap_or((None, None));
        assert_eq!((commits, prs), (None, None));

        let tally = db.take_contract();
        assert_eq!(tally.rejected.get("message.id is empty"), Some(&1));
        assert_eq!(
            tally
                .warned
                .get("message.model is not a label (stored as 'unknown')"),
            Some(&1)
        );
        assert_eq!(
            tally
                .warned
                .get("message.usage cache split does not add up to cache_write (split dropped)"),
            Some(&1)
        );
        assert_eq!(tally.warned.len(), 4, "{tally:?}");
        assert!(db.take_contract().is_empty(), "taking resets the counts");
    }

    fn message(
        id: &str,
        line: &str,
        output: u64,
        text: &str,
        calls: &[(&str, &str, &str)],
    ) -> Record {
        Record::Message(Message {
            id: id.into(),
            line_id: line.into(),
            session_id: "sess-1".into(),
            is_subagent: false,
            timestamp: at(line.len() as u32),
            model: "claude-opus-5-5".into(),
            status: Status::Ok,
            usage: Usage {
                input: 2,
                cache_write: 10,
                cache_read: 100,
                output,
                ..Usage::default()
            },
            speed: None,
            branch: None,
            blocks: if text.is_empty() {
                vec![]
            } else {
                vec![TextBlock {
                    kind: TextKind::Text,
                    text: text.into(),
                }]
            },
            tool_calls: calls
                .iter()
                .map(|(id, name, summary)| ToolCall {
                    id: (*id).into(),
                    name: (*name).into(),
                    full_name: (*name).into(),
                    summary: (*summary).into(),
                    git_dirs: Vec::new(),
                })
                .collect(),
        })
    }

    #[test]
    fn another_account_gets_the_whole_history() {
        let dir = std::env::temp_dir().join(format!("clioilc-account-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = UsageDb::open(&dir.join("usage.db")).unwrap();
        db.insert(
            "claude",
            Some("proj"),
            &message("msg_a", "la", 5, "hi", &[]),
        )
        .unwrap();
        db.commit().unwrap();
        assert!(db.claim_account("http://s#alice").unwrap());
        let batch = db.sync_claim(100).unwrap();
        assert!(!batch.is_empty());
        db.sync_finish(true, at(1)).unwrap();
        // Same account: nothing to resend.
        assert!(!db.claim_account("http://s#alice").unwrap());
        assert_eq!(db.sync_claim(100).unwrap().len(), 0);
        db.sync_finish(true, at(2)).unwrap();
        // Another account: everything again.
        assert!(db.claim_account("http://s#bob").unwrap());
        assert_eq!(db.sync_claim(100).unwrap().len(), batch.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_commits_a_tool_call_made() {
        let dir = std::env::temp_dir().join(format!("clioilc-commits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = UsageDb::open(&dir.join("usage.db")).unwrap();
        let mut msg = message("msg_c", "lc", 5, "", &[("tu_c", "Bash", "commit it")]);
        if let Record::Message(m) = &mut msg {
            m.tool_calls[0].git_dirs = vec!["/nonexistent/clioilc-repo".into()];
        }
        db.insert("claude", Some("proj"), &msg).unwrap();
        let printed = crate::git::parse_output("[main 3c514b0] Fix login");
        let result = Record::ToolResult(ToolResult {
            tool_call_id: "tu_c".into(),
            is_error: false,
            error_text: None,
            timestamp: Some(at(5)),
            commits: printed.clone(),
            prs: vec!["https://github.com/acme/pay/pull/42".into()],
        });
        db.insert("claude", Some("proj"), &result).unwrap();
        db.commit().unwrap();
        // The repository is gone: the printed commit stays, looked at once.
        assert_eq!(db.resolve_commits().unwrap(), 1);
        assert_eq!(db.resolve_commits().unwrap(), 0);

        let tool = db
            .activity(at(0), at(59))
            .unwrap()
            .into_iter()
            .find_map(|a| match a.kind {
                ActivityKind::Tool { commits, .. } => Some(commits),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool, printed);
        let project = db.project_key("proj");
        let shared = db.team_candidates(&project, None, false).unwrap();
        assert_eq!(shared[0].data["commits"][0]["hash"], "3c514b0");
        assert_eq!(shared[0].data["commits"][0]["branch"], "main");
        assert_eq!(
            shared[0].data["prs"][0],
            "https://github.com/acme/pay/pull/42"
        );
        // The local helpers are never uploaded.
        assert!(shared[0].data.get("git_dirs").is_none());

        // Work items go to the team; without text sharing, no prompt title.
        db.build_work_items(at(30)).unwrap();
        let shared = db.team_candidates(&project, None, false).unwrap();
        let item = shared.iter().find(|c| c.kind == "work_item").unwrap();
        assert_eq!(item.data["title"], "Fix login");
        assert_eq!(item.data["commits"][0]["hash"], "3c514b0");
        let mine = db.work_items().unwrap();
        assert_eq!(mine.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn all_rows(db: &UsageDb, tables: &[&str]) -> String {
        tables
            .iter()
            .map(|t| {
                db.conn
                    .prepare(&format!("SELECT * FROM {t}"))
                    .unwrap()
                    .query_map([], |r| {
                        let n = r.as_ref().column_count();
                        Ok((0..n)
                            .map(|i| match r.get_ref(i).unwrap() {
                                rusqlite::types::ValueRef::Text(s) => {
                                    String::from_utf8_lossy(s).into_owned()
                                }
                                other => format!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join("|"))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn dedupes_split_messages_and_links_tool_results() {
        let mut db = db();
        // one response written as two lines (text block, then tool_use block)
        db.insert(
            "claude",
            Some("proj"),
            &message("msg_1", "l1", 50, "hi", &[]),
        )
        .unwrap();
        db.insert(
            "claude",
            Some("proj"),
            &message("msg_1", "l22", 50, "", &[("tu_1", "Bash", "ls")]),
        )
        .unwrap();
        let result = Record::ToolResult(ToolResult {
            tool_call_id: "tu_1".into(),
            is_error: true,
            error_text: Some("boom".into()),
            timestamp: None,
            commits: Vec::new(),
            prs: Vec::new(),
        });
        db.insert("claude", Some("proj"), &result).unwrap();
        db.commit().unwrap();

        let rows = db.report(GroupBy::Project).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.key, "proj");
        assert_eq!(
            (r.messages, r.errors, r.tool_calls, r.tool_errors),
            (1, 0, 1, 1)
        );
        assert_eq!(r.usage.output, 50);

        let activity = db.activity(at(0), at(59)).unwrap();
        let kinds: Vec<_> = activity
            .iter()
            .map(|a| (&a.kind, a.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (&ActivityKind::Text, "hi"),
                (
                    &ActivityKind::Tool {
                        name: "Bash".into(),
                        status: Some("error".into()),
                        error: Some("boom".into()),
                        commits: Vec::new(),
                    },
                    "ls"
                ),
            ]
        );
        assert_eq!(activity[0].project.as_deref(), Some("proj"));
    }

    #[test]
    fn public_tables_hold_no_raw_identifiers_or_content() {
        let mut db = db();
        let mut m = match message(
            "msg_secret",
            "line_secret",
            1,
            "reply about secret-project",
            &[("tu_secret", "evil name; DROP", "cat /Users/me/secret.txt")],
        ) {
            Record::Message(m) => m,
            _ => unreachable!(),
        };
        m.status = Status::Error("Your card was declined for user@example.com".into());
        db.insert(
            "claude",
            Some("-Users-me-secret-project"),
            &Record::Message(m),
        )
        .unwrap();
        let prompt = Record::Prompt(Prompt {
            id: "p1".into(),
            session_id: "sess-1".into(),
            is_subagent: false,
            timestamp: at(1),
            text: "please fix secret-project".into(),
            branch: None,
        });
        db.insert("claude", Some("-Users-me-secret-project"), &prompt)
            .unwrap();
        db.commit().unwrap();

        let public = all_rows(&db, PUBLIC_TABLES);
        for secret in [
            "msg_secret",
            "line_secret",
            "sess-1",
            "tu_secret",
            "secret",
            "@",
            "DROP",
            "/Users",
        ] {
            assert!(
                !public.contains(secret),
                "{secret} leaked into public tables:\n{public}"
            );
        }
        assert!(public.contains("api_error") && public.contains("other"));

        let private = all_rows(&db, PRIVATE_TABLES);
        for wanted in [
            "reply about secret-project",
            "cat /Users/me/secret.txt",
            "please fix",
            "-Users-me-secret-project",
        ] {
            assert!(
                private.contains(wanted),
                "{wanted} missing from private tables"
            );
        }
    }

    #[test]
    fn private_text_is_redacted_then_truncated() {
        let mut db = db();
        let long = format!("token=abcdefgh123456 {}", "x".repeat(MAX_TEXT + 10));
        db.insert("claude", None, &message("m", "l", 1, &long, &[]))
            .unwrap();
        db.commit().unwrap();
        let text: String = db
            .conn
            .query_row("SELECT text FROM private_blocks", [], |r| r.get(0))
            .unwrap();
        assert!(text.starts_with("token=[REDACTED:assignment] x"));
        assert!(text.ends_with("…[truncated]"));
        assert!(!text.contains("abcdefgh123456"));
    }

    #[test]
    fn usage_updates_reset_sync_state() {
        let mut db = db();
        let unsynced_messages = |db: &UsageDb| db.unsynced().unwrap()[0].1;
        db.insert("claude", None, &message("m", "l", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        db.conn
            .execute("UPDATE messages SET synced_at = 'x'", [])
            .unwrap();

        db.insert("claude", None, &message("m", "l", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        assert_eq!(unsynced_messages(&db), 0, "identical line must not resync");

        db.insert("claude", None, &message("m", "l", 9, "", &[]))
            .unwrap();
        db.commit().unwrap();
        assert_eq!(unsynced_messages(&db), 1);
        assert_eq!(db.report(GroupBy::Model).unwrap()[0].usage.output, 9);
    }

    fn pending(db: &UsageDb) -> u64 {
        db.unsynced().unwrap().iter().map(|(_, n)| n).sum()
    }

    #[test]
    fn sync_claims_in_batches_and_marks_synced() {
        let mut db = db();
        db.insert(
            "claude",
            Some("app"),
            &message("m1", "l1", 5, "hi", &[("t1", "Bash", "ls")]),
        )
        .unwrap();
        db.insert("claude", Some("app"), &message("m2", "l2", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        let total = pending(&db);
        assert!(total >= 5, "messages, tool call, project and private rows");
        assert!(db.has_unsynced_activity().unwrap(), "new activity");

        let first = db.sync_claim(2).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first.messages.len(), 2, "public tables are claimed first");
        db.sync_finish(true, at(0)).unwrap();
        assert_eq!(pending(&db), total - 2);

        let rest = db.sync_claim(100).unwrap();
        assert_eq!(rest.len() as u64, total - 2);
        db.sync_finish(true, at(0)).unwrap();
        assert!(!db.has_unsynced_activity().unwrap(), "all sent");
        let kinds: Vec<_> = rest.private.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&"project") && kinds.contains(&"block"));
        let project = rest.private.iter().find(|p| p.kind == "project").unwrap();
        assert_eq!(project.data["name"], "app");
        assert!(project.ts.is_none());
        // Every private row names its project, even after its message was
        // already sent in an earlier batch.
        let project_id = first.messages[0].project_id.clone();
        for p in &rest.private {
            assert_eq!(
                p.project_id.as_deref(),
                Some(project_id.as_str()),
                "{}",
                p.kind
            );
        }
        db.sync_finish(true, at(0)).unwrap();
        assert_eq!(pending(&db), 0);
        assert!(db.sync_claim(100).unwrap().is_empty());
    }

    #[test]
    fn project_lookup_for_link() {
        let mut db = db();
        for project in ["-Users-me-app", "-Users-me-app-api", "-Users-me-apple"] {
            db.insert(
                "claude",
                Some(project),
                &message(project, project, 5, "", &[]),
            )
            .unwrap();
        }
        db.commit().unwrap();
        let key = db.project_key("-Users-me-app");
        assert_eq!(db.project_activity(&key).unwrap().0, 1);
        assert_eq!(
            db.project_name(&key).unwrap().as_deref(),
            Some("-Users-me-app")
        );
        let under: Vec<_> = db.projects_under("-Users-me-app").unwrap();
        assert_eq!(under.len(), 1, "app-api, not apple");
        assert_eq!(under[0].0, "-Users-me-app-api");
        assert_eq!(under[0].1, db.project_key("-Users-me-app-api"));
        let none = db.project_key("-Users-me-new");
        assert_eq!(db.project_activity(&none).unwrap(), (0, None));
    }

    #[test]
    fn failed_or_interrupted_sync_releases_rows() {
        let mut db = db();
        db.insert("claude", None, &message("m1", "l1", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        let total = pending(&db);

        db.sync_claim(100).unwrap();
        db.sync_finish(false, at(0)).unwrap();
        assert_eq!(pending(&db), total);

        db.sync_claim(100).unwrap(); // then the process dies
        assert_eq!(pending(&db), 0, "claimed rows are not counted as pending");
        db.sync_release().unwrap();
        assert_eq!(pending(&db), total);
    }

    #[test]
    fn row_changed_while_in_flight_is_sent_again() {
        let mut db = db();
        db.insert("claude", None, &message("m1", "l1", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        let batch = db.sync_claim(100).unwrap();
        assert_eq!(batch.messages[0].output_tokens, 5);

        // The watcher sees more output for the same message mid-upload.
        db.insert("claude", None, &message("m1", "l1", 9, "", &[]))
            .unwrap();
        db.commit().unwrap();
        db.sync_finish(true, at(0)).unwrap();

        let again = db.sync_claim(100).unwrap();
        assert_eq!(again.messages.len(), 1);
        assert_eq!(again.messages[0].output_tokens, 9);
    }

    #[test]
    fn salt_can_only_be_replaced_while_empty() {
        let mut db = db();
        let shared = [7u8; 32];
        db.replace_salt(&shared).unwrap();
        assert_eq!(db.salt(), shared);
        assert!(db.replace_salt(&[1u8; 8]).is_err(), "too short");

        db.insert("claude", Some("app"), &message("m1", "l1", 5, "", &[]))
            .unwrap();
        db.commit().unwrap();
        let id = db.sync_claim(1).unwrap().messages[0].id.clone();
        assert_eq!(id, db.hash("message", "m1"));
        assert!(!db.is_empty().unwrap());
        assert!(db.replace_salt(&[8u8; 32]).is_err());
        assert_eq!(db.salt(), shared);
    }

    #[test]
    fn same_salt_gives_same_ids_on_two_devices() {
        let mut a = db();
        let path = std::env::temp_dir().join(format!("clioilc-usage-b-{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
        let mut b = UsageDb::open(&path).unwrap();
        assert_ne!(a.hash("project", "app"), b.hash("project", "app"));
        let salt = a.salt().to_vec();
        b.replace_salt(&salt).unwrap();
        assert_eq!(a.hash("project", "app"), b.hash("project", "app"));
        a.commit().unwrap();
    }

    fn with_split(id: &str, output: u64, split: Option<(u64, u64)>, speed: Option<&str>) -> Record {
        let Record::Message(mut m) = message(id, "l1", output, "", &[]) else {
            unreachable!()
        };
        if let Some((a, b)) = split {
            m.usage.cache_write_5m = Some(a);
            m.usage.cache_write_1h = Some(b);
        }
        m.speed = speed.map(str::to_string);
        Record::Message(m)
    }

    #[test]
    fn old_databases_get_the_new_columns() {
        let path =
            std::env::temp_dir().join(format!("clioilc-usage-old-{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE messages (id TEXT PRIMARY KEY, tool TEXT NOT NULL, project_id TEXT NOT NULL,
                 session_id TEXT NOT NULL, is_subagent INTEGER NOT NULL, ts TEXT NOT NULL, model TEXT NOT NULL,
                 status TEXT NOT NULL, input_tokens INTEGER NOT NULL, cache_write_tokens INTEGER NOT NULL,
                 cache_read_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL, synced_at TEXT);",
            )
            .unwrap();
        }
        let mut db = UsageDb::open(&path).unwrap();
        db.insert(
            "claude",
            None,
            &with_split("m", 5, Some((4, 6)), Some("fast")),
        )
        .unwrap();
        db.commit().unwrap();
        let m = &db.sync_claim(10).unwrap().messages[0];
        assert_eq!(
            (m.cache_write_5m_tokens, m.cache_write_1h_tokens),
            (Some(4), Some(6))
        );
        assert_eq!(m.speed.as_deref(), Some("fast"));
        // Opening again doesn't try to add the columns twice.
        drop(db);
        UsageDb::open(&path).unwrap();
    }

    #[test]
    fn reimport_backfills_split_and_speed_and_resyncs() {
        let mut db = db();
        db.insert("claude", None, &with_split("m", 5, None, None))
            .unwrap();
        db.commit().unwrap();
        db.sync_claim(100).unwrap();
        db.sync_finish(true, at(0)).unwrap();
        assert_eq!(pending(&db), 0);

        // Same message, now read by a reader that knows the split.
        db.insert(
            "claude",
            None,
            &with_split("m", 5, Some((3, 7)), Some("standard")),
        )
        .unwrap();
        db.commit().unwrap();
        let batch = db.sync_claim(100).unwrap();
        assert_eq!(batch.messages.len(), 1, "changed row is sent again");
        let m = &batch.messages[0];
        assert_eq!(
            (m.cache_write_5m_tokens, m.cache_write_1h_tokens),
            (Some(3), Some(7))
        );
        assert_eq!(m.speed.as_deref(), Some("standard"));
        db.sync_finish(true, at(0)).unwrap();

        // Identical line again: nothing to resend.
        db.insert(
            "claude",
            None,
            &with_split("m", 5, Some((3, 7)), Some("standard")),
        )
        .unwrap();
        db.commit().unwrap();
        assert!(db.sync_claim(100).unwrap().is_empty());
    }

    #[test]
    fn sync_json_omits_unknown_speed() {
        let mut db = db();
        db.insert("claude", None, &with_split("m", 5, None, None))
            .unwrap();
        db.commit().unwrap();
        let json = serde_json::to_value(db.sync_claim(10).unwrap()).unwrap();
        let m = &json["messages"][0];
        assert!(m.get("speed").is_none());
        assert!(m["cache_write_5m_tokens"].is_null());
    }
}
