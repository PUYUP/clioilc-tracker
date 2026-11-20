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
    synced_at   TEXT
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
    synced_at TEXT
);
CREATE INDEX IF NOT EXISTS private_tool_calls_ts ON private_tool_calls(ts);

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
SELECT p.ts, pp.name, p.session_id, p.is_subagent, 'prompt', NULL, p.text, NULL, NULL
FROM private_prompts p
LEFT JOIN private_projects pp ON pp.project_id = p.project_id
WHERE p.ts >= ?1 AND p.ts < ?2
UNION ALL
SELECT b.ts, pp.name, m.session_id, m.is_subagent, b.kind, NULL, b.text, NULL, NULL
FROM private_blocks b
JOIN messages m ON m.id = b.message_id
LEFT JOIN private_projects pp ON pp.project_id = m.project_id
WHERE b.ts >= ?1 AND b.ts < ?2
UNION ALL
SELECT pt.ts, pp.name, m.session_id, m.is_subagent, 'tool', pt.full_name, pt.summary,
       t.status, pt.error
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
    },
}

pub struct UsageDb {
    conn: Connection,
    salt: Vec<u8>,
    in_tx: bool,
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
        })
    }

    /// Store one record parsed from a log of `tool` (the reader name).
    /// Writes are batched in a transaction until [`UsageDb::commit`].
    pub fn insert(&mut self, tool: &str, project: Option<&str>, record: &Record) -> Result<()> {
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
            m.usage.cache_write_5m.map(to_i64),
            m.usage.cache_write_1h.map(to_i64),
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
        Ok(())
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
        Ok(())
    }

    /// Returns the hashed project id, recording its real name privately.
    fn insert_project(&mut self, project: &str) -> Result<String> {
        let project_id = self.hash("project", project);
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO private_projects (project_id, name) VALUES (?1, ?2)",
            )?
            .execute(params![project_id, project])?;
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
            "SELECT p.id, p.ts, p.full_name, p.summary, p.error
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
                data: json!({
                    "full_name": r.get::<_, Option<String>>(2)?,
                    "summary": r.get::<_, Option<String>>(3)?,
                    "error": r.get::<_, Option<String>>(4)?,
                }),
            })
        })?;
        for row in rows {
            out.push(row?);
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
    /// `project`, `prompt`, `block` or `tool_call`.
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
        "SELECT p.id, p.ts, p.full_name, p.summary, p.error, m.project_id
         FROM private_tool_calls p
         LEFT JOIN tool_calls t ON t.id = p.id
         LEFT JOIN messages m ON m.id = t.message_id",
        &|r| {
            Ok(SyncPrivate {
                kind: "tool_call",
                id: r.get(0)?,
                ts: r.get(1)?,
                project_id: r.get(5)?,
                data: json!({
                    "full_name": r.get::<_, Option<String>>(2)?,
                    "summary": r.get::<_, Option<String>>(3)?,
                    "error": r.get::<_, Option<String>>(4)?,
                }),
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
];

fn migrate(conn: &Connection) -> Result<()> {
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
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.<>".contains(&b));
    if ok { raw } else { fallback }.to_string()
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
                })
                .collect(),
        })
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
                        error: Some("boom".into())
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

        let first = db.sync_claim(2).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first.messages.len(), 2, "public tables are claimed first");
        db.sync_finish(true, at(0)).unwrap();
        assert_eq!(pending(&db), total - 2);

        let rest = db.sync_claim(100).unwrap();
        assert_eq!(rest.len() as u64, total - 2);
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
