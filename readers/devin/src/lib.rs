//! Reader for Devin CLI (Cognition / Windsurf), which keeps its history in
//! a SQLite database instead of log files.
//!
//! The database belongs to Devin: it is opened read-only and read by
//! `message_nodes.row_id`, the cursor. Each node is one chat message:
//!
//! - `assistant`: a model response with token metrics, text, thinking and
//!   tool calls. The same response is copied to several nodes when a
//!   conversation is forked or re-rendered (same `message_id`, same
//!   metrics), so `message_id` is the dedupe key, as Claude's message id is.
//! - `tool`: a tool call's output; whether it failed comes from
//!   `tool_call_state` (status, and the exit code of shell commands).
//! - `user` with `metadata.is_user_input`: something the human typed.
//!
//! Tool names are mapped to Claude Code's (`exec` -> `Bash`, `edit` ->
//! `Edit`, ...) so that the journal finds edits, commits and pushes the
//! same way for both agents.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use clioilc_core::git;
use clioilc_core::reader::DatabaseSource;
use clioilc_core::usage::{
    Message, Prompt, Record, Status, TextBlock, TextKind, ToolCall, ToolResult, Usage,
};
use clioilc_core::{home_dir, Reader};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Rows per query; the cursor makes the rest come in the next call.
const BATCH: i64 = 2000;
/// Tool output kept to look for commits, PR URLs and the error text.
const OUTPUT_PREFIX: usize = 16 * 1024;

pub struct DevinReader;

impl Reader for DevinReader {
    fn name(&self) -> &'static str {
        "devin"
    }

    fn default_watch_dir(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".local/share/devin/cli"))
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["db"]
    }

    fn database(&self) -> Option<&dyn DatabaseSource> {
        Some(self)
    }
}

impl DatabaseSource for DevinReader {
    fn path(&self) -> Result<PathBuf> {
        Ok(self.default_watch_dir()?.join("sessions.db"))
    }

    fn read_since(
        &self,
        path: &Path,
        cursor: i64,
        out: &mut dyn FnMut(Option<&str>, Record),
    ) -> Result<i64> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        let mut cursor = cursor;
        let mut sessions: HashMap<String, Option<Session>> = HashMap::new();
        // Nodes in a shape this reader does not know (Devin changed its
        // format) are reported, so they are not lost silently.
        let mut unreadable = 0u64;
        let report = |n: u64| {
            if n > 0 {
                eprintln!(
                    "[devin] {n} message(s) in an unknown format were skipped; clioilc may need an update"
                );
            }
        };
        loop {
            let rows = read_rows(&conn, cursor)?;
            let Some(last) = rows.last().map(|r| r.row_id) else {
                report(unreadable);
                return Ok(cursor);
            };
            for row in &rows {
                if !sessions.contains_key(&row.session_id) {
                    let s = session(&conn, &row.session_id)?;
                    sessions.insert(row.session_id.clone(), s);
                }
                let Some(Some(s)) = sessions.get(&row.session_id) else {
                    continue;
                };
                let project = project_name(&s.working_directory);
                match decode(row) {
                    Some(node) => {
                        for record in records(row, s, node) {
                            out(Some(&project), record);
                        }
                    }
                    None => unreadable += 1,
                }
            }
            cursor = last;
            if (rows.len() as i64) < BATCH {
                report(unreadable);
                return Ok(cursor);
            }
        }
    }
}

struct Row {
    row_id: i64,
    session_id: String,
    created_at: i64,
    message: String,
    /// For tool nodes: `tool_call_state.tool_call_update_json`.
    update: Option<String>,
}

struct Session {
    working_directory: String,
    model: String,
}

fn read_rows(conn: &Connection, cursor: i64) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.row_id, n.session_id, n.created_at, n.chat_message,
                t.tool_call_update_json
         FROM message_nodes n
         LEFT JOIN tool_call_state t
           ON t.session_id = n.session_id
          AND t.tool_call_id = json_extract(n.chat_message, '$.tool_call_id')
          AND json_valid(n.chat_message)
         WHERE n.row_id > ?1
         ORDER BY n.row_id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map([cursor, BATCH], |r| {
        Ok(Row {
            row_id: r.get(0)?,
            session_id: r.get(1)?,
            created_at: r.get(2)?,
            message: r.get(3)?,
            update: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn session(conn: &Connection, id: &str) -> Result<Option<Session>> {
    Ok(conn
        .query_row(
            "SELECT working_directory, model FROM sessions WHERE id = ?1",
            [id],
            |r| {
                Ok(Session {
                    working_directory: r.get(0)?,
                    model: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// The project name Claude Code gives a folder (`/Users/me/app` ->
/// `-Users-me-app`), so work on the same folder with either agent is one
/// project.
pub fn project_name(dir: &str) -> String {
    dir.trim_end_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Node {
    message_id: Option<String>,
    role: String,
    content: Value,
    /// `{"thinking": "...", "signature": "...", "is_stale": true}`.
    thinking: Value,
    tool_calls: Vec<NodeToolCall>,
    tool_call_id: Option<String>,
    metadata: NodeMeta,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeMeta {
    is_user_input: Option<Value>,
    created_at: Option<String>,
    generation_model: Option<String>,
    metrics: Option<Metrics>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Metrics {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeToolCall {
    id: String,
    name: String,
    /// A JSON object encoded as a string.
    arguments: Value,
}

fn decode(row: &Row) -> Option<Node> {
    serde_json::from_str(&row.message).ok()
}

#[cfg(test)]
fn parse_node(row: &Row, session: &Session) -> Vec<Record> {
    let out = decode(row).map_or_else(Vec::new, |node| records(row, session, node));
    // What the reader yields must follow the reader contract (docs/readers.md).
    clioilc_core::contract::assert_conforms("devin", Some("-r-app"), &out);
    out
}

fn records(row: &Row, session: &Session, node: Node) -> Vec<Record> {
    let timestamp = node
        .metadata
        .created_at
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .or_else(|| Utc.timestamp_opt(row.created_at, 0).single())
        .unwrap_or_else(Utc::now);
    // Copies of one message share its id; nodes without one stand alone.
    let id = node
        .message_id
        .clone()
        .unwrap_or_else(|| format!("{}#{}", row.session_id, row.row_id));
    let text = match &node.content {
        Value::String(s) => s.clone(),
        _ => String::new(),
    };

    match node.role.as_str() {
        "assistant" => {
            let m = node.metadata.metrics.unwrap_or_default();
            let usage = Usage {
                input: m.input_tokens.unwrap_or(0),
                cache_write: m.cache_creation_tokens.unwrap_or(0),
                cache_read: m.cache_read_tokens.unwrap_or(0),
                output: m.output_tokens.unwrap_or(0),
                cache_write_5m: None,
                cache_write_1h: None,
            };
            let mut blocks = Vec::new();
            let thinking = match &node.thinking {
                Value::String(t) => Some(t.clone()),
                v => v["thinking"].as_str().map(str::to_string),
            };
            if let Some(t) = thinking.filter(|t| !t.trim().is_empty()) {
                blocks.push(TextBlock {
                    kind: TextKind::Thinking,
                    text: t,
                });
            }
            if !text.trim().is_empty() {
                blocks.push(TextBlock {
                    kind: TextKind::Text,
                    text,
                });
            }
            let tool_calls = node
                .tool_calls
                .iter()
                .filter(|c| !c.id.is_empty())
                .map(|c| tool_call(c, &session.working_directory))
                .collect();
            vec![Record::Message(Message {
                line_id: id.clone(),
                id,
                session_id: row.session_id.clone(),
                is_subagent: false,
                timestamp,
                model: node
                    .metadata
                    .generation_model
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| session.model.clone()),
                status: Status::Ok,
                usage,
                speed: None,
                blocks,
                tool_calls,
                branch: None,
            })]
        }
        "tool" => {
            let Some(tool_call_id) = node.tool_call_id else {
                return Vec::new();
            };
            let update: Value = row
                .update
                .as_deref()
                .and_then(|u| serde_json::from_str(u).ok())
                .unwrap_or_default();
            let failed = update["status"] == "failed";
            let exit = update["_meta"]["terminal_exit"]["exit_code"].as_i64();
            let is_error = failed || exit.is_some_and(|c| c != 0);
            let output: String = text.chars().take(OUTPUT_PREFIX).collect();
            let (commits, prs) = if is_error {
                (Vec::new(), Vec::new())
            } else {
                (git::parse_output(&output), git::parse_prs(&output))
            };
            vec![Record::ToolResult(ToolResult {
                tool_call_id,
                is_error,
                error_text: is_error.then_some(output).filter(|t| !t.is_empty()),
                timestamp: Some(timestamp),
                commits,
                prs,
            })]
        }
        "user" if is_true(node.metadata.is_user_input.as_ref()) => {
            if text.trim().is_empty() {
                return Vec::new();
            }
            vec![Record::Prompt(Prompt {
                id,
                session_id: row.session_id.clone(),
                is_subagent: false,
                timestamp,
                text: text.trim().to_string(),
                branch: None,
            })]
        }
        _ => Vec::new(),
    }
}

fn is_true(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true))) || v.and_then(Value::as_i64) == Some(1)
}

/// Devin tool -> Claude Code tool name (what the journal understands).
fn claude_name(name: &str) -> &str {
    match name {
        "exec" => "Bash",
        "edit" => "Edit",
        "write" => "Write",
        "read" => "Read",
        "grep" => "Grep",
        "find_file_by_name" => "Glob",
        "webfetch" => "WebFetch",
        "web_search" => "WebSearch",
        "todo_write" => "TodoWrite",
        "ask_user_question" => "AskUserQuestion",
        "skill" => "Skill",
        other => other,
    }
}

fn tool_call(c: &NodeToolCall, cwd: &str) -> ToolCall {
    let args: Value = match &c.arguments {
        Value::String(s) => serde_json::from_str(s).unwrap_or_default(),
        other => other.clone(),
    };
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let name = claude_name(&c.name);
    let summary = match c.name.as_str() {
        "exec" => s("command").map(str::to_string),
        "edit" | "write" | "read" => s("file_path").map(str::to_string),
        "grep" | "find_file_by_name" => s("pattern").map(|p| match s("path") {
            Some(path) => format!("{p}: {path}"),
            None => p.to_string(),
        }),
        "webfetch" | "browser_preview" => s("url").map(str::to_string),
        "web_search" => s("query").map(str::to_string),
        "code_search" => s("search_term").map(str::to_string),
        "skill" => s("skill").map(str::to_string),
        "ask_user_question" => args["questions"].as_array().map(|qs| {
            qs.iter()
                .filter_map(|q| q["question"].as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        }),
        _ => None,
    }
    .unwrap_or_default();
    let git_dirs = if c.name == "exec" {
        git::commit_dirs(&summary, Some(s("workdir").unwrap_or(cwd)))
    } else {
        Vec::new()
    };
    ToolCall {
        id: c.id.clone(),
        name: name.to_string(),
        full_name: name.to_string(),
        summary,
        git_dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        Session {
            working_directory: "/Users/me/src/pay".into(),
            model: "swe-2-flash-medium".into(),
        }
    }

    fn row(message: Value, update: Option<Value>) -> Row {
        Row {
            row_id: 7,
            session_id: "s1".into(),
            created_at: 1_791_040_534,
            message: message.to_string(),
            update: update.map(|u| u.to_string()),
        }
    }

    #[test]
    fn project_names_match_claude() {
        assert_eq!(
            project_name("/Volumes/SSD1/Private/clioilc/tracker"),
            "-Volumes-SSD1-Private-clioilc-tracker"
        );
        assert_eq!(project_name("/Users/me/my.app/"), "-Users-me-my-app");
    }

    #[test]
    fn assistant_node_is_a_message_with_usage_text_and_tools() {
        let r = row(
            json!({
                "message_id": "m1", "role": "assistant", "content": "Done.", "thinking": {"thinking": "plan", "signature": "sig", "is_stale": true},
                "tool_calls": [
                    {"id": "c1", "name": "exec", "arguments": "{\"command\":\"git commit -qm x\",\"workdir\":\"/Users/me/src/pay\"}"},
                    {"id": "c2", "name": "edit", "arguments": "{\"file_path\":\"/Users/me/src/pay/a.rs\",\"old_string\":\"secret old\",\"new_string\":\"secret new\"}"},
                    {"id": "c3", "name": "kill_shell", "arguments": "{\"shell_id\":\"x\"}"}
                ],
                "metadata": {"created_at": "2026-10-03T14:29:57.319297Z", "generation_model": "swe-2-flash-medium",
                             "metrics": {"input_tokens": 338, "output_tokens": 187, "cache_read_tokens": 12768, "cache_creation_tokens": null}}
            }),
            None,
        );
        let [Record::Message(m)] = <[Record; 1]>::try_from(parse_node(&r, &session())).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (m.id.as_str(), m.model.as_str()),
            ("m1", "swe-2-flash-medium")
        );
        assert_eq!(
            (
                m.usage.input,
                m.usage.cache_read,
                m.usage.cache_write,
                m.usage.output
            ),
            (338, 12768, 0, 187)
        );
        assert_eq!(m.timestamp.to_rfc3339(), "2026-10-03T14:29:57.319297+00:00");
        assert_eq!(
            m.blocks.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(),
            ["thinking", "text"]
        );
        let tools: Vec<_> = m
            .tool_calls
            .iter()
            .map(|t| (t.name.as_str(), t.summary.as_str()))
            .collect();
        assert_eq!(
            tools,
            [
                ("Bash", "git commit -qm x"),
                ("Edit", "/Users/me/src/pay/a.rs"),
                ("kill_shell", "")
            ]
        );
        // Edit contents never become the summary.
        assert!(!format!("{:?}", m.tool_calls).contains("secret"));
        assert_eq!(m.tool_calls[0].git_dirs, ["/Users/me/src/pay"]);
    }

    #[test]
    fn tool_node_failure_comes_from_status_or_exit_code() {
        let out = |text: &str| json!({"role": "tool", "tool_call_id": "c1", "content": text, "metadata": {}});
        let ok = parse_node(
            &row(
                out("[main 3c514b0] Add retries\n 1 file changed"),
                Some(json!({"status": "completed", "_meta": {"terminal_exit": {"exit_code": 0}}})),
            ),
            &session(),
        );
        let [Record::ToolResult(t)] = <[Record; 1]>::try_from(ok).unwrap() else {
            panic!()
        };
        assert!(!t.is_error);
        assert_eq!(t.commits.len(), 1);

        let exit1 = parse_node(
            &row(
                out("bash: pip: command not found"),
                Some(
                    json!({"status": "completed", "_meta": {"terminal_exit": {"exit_code": 127}}}),
                ),
            ),
            &session(),
        );
        let [Record::ToolResult(t)] = <[Record; 1]>::try_from(exit1).unwrap() else {
            panic!()
        };
        assert!(t.is_error);
        assert_eq!(
            t.error_text.as_deref(),
            Some("bash: pip: command not found")
        );

        let failed = parse_node(
            &row(
                out("Tool 'edit' validation failed"),
                Some(json!({"status": "failed"})),
            ),
            &session(),
        );
        let [Record::ToolResult(t)] = <[Record; 1]>::try_from(failed).unwrap() else {
            panic!()
        };
        assert!(t.is_error);
    }

    #[test]
    fn only_typed_input_is_a_prompt() {
        let typed = json!({"message_id": "u1", "role": "user", "content": " fix the login bug ", "metadata": {"is_user_input": true}});
        let [Record::Prompt(p)] =
            <[Record; 1]>::try_from(parse_node(&row(typed, None), &session())).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (p.id.as_str(), p.text.as_str()),
            ("u1", "fix the login bug")
        );
        let injected = json!({"role": "user", "content": "<system reminder>", "metadata": {}});
        assert!(parse_node(&row(injected, None), &session()).is_empty());
        assert!(parse_node(
            &row(json!({"role": "system", "content": "x"}), None),
            &session()
        )
        .is_empty());
        assert!(parse_node(&row(json!("not an object"), None), &session()).is_empty());
        // Unknown shapes are counted as unreadable, not read as nothing.
        assert!(decode(&row(json!("not an object"), None)).is_none());
        assert!(decode(&row(json!({"role": "assistant", "tool_calls": 5}), None)).is_none());
        assert!(decode(&row(json!({"role": "system", "content": "x"}), None)).is_some());
    }

    #[test]
    fn reads_a_database_by_cursor() {
        let dir = std::env::temp_dir().join(format!("clioilc-devin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, model TEXT);
             CREATE TABLE message_nodes (row_id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, node_id INTEGER, chat_message TEXT, created_at INTEGER);
             CREATE TABLE tool_call_state (session_id TEXT, tool_call_id TEXT, tool_call_update_json TEXT);
             INSERT INTO sessions VALUES ('s1', '/Users/me/src/pay', 'swe-2-flash-medium');",
        )
        .unwrap();
        let add = |m: Value| {
            conn.execute(
                "INSERT INTO message_nodes (session_id, node_id, chat_message, created_at) VALUES ('s1', 0, ?1, 1791040534)",
                [m.to_string()],
            )
            .unwrap();
        };
        add(
            json!({"message_id": "u1", "role": "user", "content": "hi", "metadata": {"is_user_input": true}}),
        );
        add(
            json!({"message_id": "m1", "role": "assistant", "content": "hello", "metadata": {"metrics": {"input_tokens": 5, "output_tokens": 2}}}),
        );
        add(
            json!({"message_id": "m1", "role": "assistant", "content": "hello", "metadata": {"metrics": {"input_tokens": 5, "output_tokens": 2}}}),
        );

        let mut got = Vec::new();
        let cursor = DevinReader
            .read_since(&path, 0, &mut |p, r| got.push((p.map(str::to_string), r)))
            .unwrap();
        assert_eq!(cursor, 3);
        assert_eq!(got.len(), 3); // the copy of m1 is read too; UsageDb dedupes it by id
        assert_eq!(got[0].0.as_deref(), Some("-Users-me-src-pay"));

        add(json!({"role": "system", "content": "x"}));
        let mut more = 0;
        assert_eq!(
            DevinReader
                .read_since(&path, cursor, &mut |_, _| more += 1)
                .unwrap(),
            4
        );
        assert_eq!(more, 0);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
