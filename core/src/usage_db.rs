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
use sha2::{Digest, Sha256};
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
    synced_at          TEXT
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
                      input_tokens, cache_write_tokens, cache_read_tokens, output_tokens)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
ON CONFLICT(id) DO UPDATE SET
    input_tokens       = max(input_tokens, excluded.input_tokens),
    cache_write_tokens = max(cache_write_tokens, excluded.cache_write_tokens),
    cache_read_tokens  = max(cache_read_tokens, excluded.cache_read_tokens),
    output_tokens      = max(output_tokens, excluded.output_tokens),
    synced_at          = NULL
WHERE excluded.input_tokens       > messages.input_tokens
   OR excluded.cache_write_tokens > messages.cache_write_tokens
   OR excluded.cache_read_tokens  > messages.cache_read_tokens
   OR excluded.output_tokens      > messages.output_tokens";

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
        conn.execute_batch(SCHEMA)
            .context("creating usage schema")?;
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
            },
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
}
