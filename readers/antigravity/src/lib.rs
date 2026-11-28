//! Reader for Google Antigravity / Gemini IDE, which stores conversation
//! history and steps in SQLite databases in `~/.gemini/antigravity/`.
//!
//! Antigravity stores:
//! - An index of conversations in `conversation_summaries.db`.
//! - Trajectories per conversation in `conversations/<conversation_id>.db`.
//!
//! Steps within each conversation database:
//! - Type 14: User prompt (`Record::Prompt`)
//! - Type 15: Model / Assistant response (`Record::Message`)
//! - Type 132: Tool call execution result (`Record::ToolResult`)
//!
//! Tool names are mapped to canonical names (e.g. `run_command` -> `Bash`,
//! `read_file` -> `Read`, `edit_file` -> `Edit`, etc.) as defined in
//! `docs/readers.md`.

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use clioilc_core::git;
use clioilc_core::reader::DatabaseSource;
use clioilc_core::usage::{
    Message, Prompt, Record, Status, TextBlock, TextKind, ToolCall, ToolResult, Usage,
};
use clioilc_core::{home_dir, Reader};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Max tool output prefix kept for error inspection, commits, and PR URLs.
const OUTPUT_PREFIX: usize = 16 * 1024;

/// Reader for Antigravity SQLite databases.
#[derive(Debug, Default, Clone, Copy)]
pub struct AntigravityReader;

impl Reader for AntigravityReader {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn default_watch_dir(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".gemini").join("antigravity"))
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["db"]
    }

    fn database(&self) -> Option<&dyn DatabaseSource> {
        Some(self)
    }
}

impl DatabaseSource for AntigravityReader {
    fn path(&self) -> Result<PathBuf> {
        Ok(self.default_watch_dir()?.join("conversation_summaries.db"))
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

        let mut stmt = conn.prepare_cached(
            "SELECT conversation_id, workspace_uris,
                    CAST(strftime('%s', last_modified_time) AS INTEGER) AS mod_ts
             FROM conversation_summaries
             WHERE CAST(strftime('%s', last_modified_time) AS INTEGER) > ?1
             ORDER BY mod_ts ASC",
        )?;

        let rows = stmt.query_map([cursor], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;

        let convs_dir = path
            .parent()
            .map(|p| p.join("conversations"))
            .unwrap_or_else(|| PathBuf::from("conversations"));

        let mut new_cursor = cursor;
        for item in rows {
            let (conv_id, workspace_uris, mod_ts) = item?;
            let project = parse_project_name(&workspace_uris);
            let conv_path = convs_dir.join(format!("{conv_id}.db"));
            if conv_path.is_file() {
                if let Err(e) = read_conversation(&conv_path, &conv_id, project.as_deref(), out) {
                    eprintln!(
                        "[antigravity] error reading conversation {}: {e:#}",
                        conv_path.display()
                    );
                }
            }
            if mod_ts > new_cursor {
                new_cursor = mod_ts;
            }
        }

        Ok(new_cursor)
    }
}

/// Convert workspace URIs JSON (e.g. `["file:///path/to/project"]`) to canonical project name.
pub fn parse_project_name(uris_json: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(uris_json).ok()?;
    let uri = parsed.as_array()?.first()?.as_str()?;
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let cleaned = path.trim_end_matches('/');
    if cleaned.is_empty() {
        return None;
    }
    Some(
        cleaned
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect(),
    )
}

/// Reads one conversation SQLite database file.
fn read_conversation(
    path: &Path,
    conv_id: &str,
    project: Option<&str>,
    out: &mut dyn FnMut(Option<&str>, Record),
) -> Result<()> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    // Pre-read model slugs from gen_metadata if present.
    let mut models_by_idx: HashMap<i64, String> = HashMap::new();
    if let Ok(mut g_stmt) = conn.prepare_cached("SELECT idx, data FROM gen_metadata") {
        if let Ok(rows) = g_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))) {
            for item in rows.flatten() {
                if let Some(slug) = extract_model_slug(&item.1) {
                    models_by_idx.insert(item.0, slug);
                }
            }
        }
    }

    let mut stmt = conn.prepare_cached(
        "SELECT idx, step_type, status, metadata, step_payload
         FROM steps
         ORDER BY idx ASC",
    )?;

    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i32>(1)?,
            r.get::<_, i32>(2)?,
            r.get::<_, Vec<u8>>(3)?,
            r.get::<_, Vec<u8>>(4)?,
        ))
    })?;

    for item in rows {
        let (idx, step_type, status, metadata_bytes, payload_bytes) = item?;
        let meta = ProtoMessage::parse(&metadata_bytes);
        let payload = ProtoMessage::parse(&payload_bytes);
        let ts = meta
            .field(1)
            .and_then(|v| parse_timestamp(v))
            .unwrap_or_else(Utc::now);

        match step_type {
            14 => {
                // User Prompt
                if let Some(f19_bytes) = payload.field(19) {
                    let f19 = ProtoMessage::parse(f19_bytes);
                    let text = f19.string(2).unwrap_or_default().trim().to_string();
                    let id = f19
                        .string(19)
                        .map(str::to_string)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("{conv_id}#p{idx}"));
                    if !text.is_empty() {
                        out(
                            project,
                            Record::Prompt(Prompt {
                                id,
                                session_id: conv_id.to_string(),
                                is_subagent: false,
                                timestamp: ts,
                                text,
                                branch: None,
                            }),
                        );
                    }
                }
            }
            15 | 23 => {
                // Model / Assistant Response
                let is_subagent = step_type == 23;
                if let Some(f20_bytes) = payload.field(20) {
                    let f20 = ProtoMessage::parse(f20_bytes);
                    let resp_id = f20
                        .string(6)
                        .or_else(|| f20.string(15))
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{conv_id}#m{idx}"));

                    let mut blocks = Vec::new();
                    if let Some(thinking) = f20.string(3) {
                        let t = thinking.trim();
                        if !t.is_empty() {
                            blocks.push(TextBlock {
                                kind: TextKind::Thinking,
                                text: t.to_string(),
                            });
                        }
                    }

                    // Tool calls in response
                    let mut tool_calls = Vec::new();
                    for call_bytes in f20.fields(7) {
                        let call_msg = ProtoMessage::parse(call_bytes);
                        let cid = call_msg.string(1).unwrap_or_default();
                        let raw_name = call_msg.string(2).unwrap_or_default();
                        let args_str = call_msg.string(3).unwrap_or("{}");
                        if !cid.is_empty() {
                            let (c_name, summary, git_dirs) = parse_tool_call(raw_name, args_str);
                            tool_calls.push(ToolCall {
                                id: cid.to_string(),
                                name: c_name,
                                full_name: raw_name.to_string(),
                                summary,
                                git_dirs,
                            });
                        }
                    }

                    // Metrics from metadata field 9
                    let (inp, outp) = meta
                        .field(9)
                        .map(|m9_bytes| {
                            let m9 = ProtoMessage::parse(m9_bytes);
                            (m9.varint(2).unwrap_or(0), m9.varint(3).unwrap_or(0))
                        })
                        .unwrap_or((0, 0));

                    let model = models_by_idx
                        .get(&idx)
                        .cloned()
                        .unwrap_or_else(|| "gemini-3.8-flash-medium".into());

                    out(
                        project,
                        Record::Message(Message {
                            id: resp_id.clone(),
                            line_id: format!("{conv_id}#{idx}"),
                            session_id: conv_id.to_string(),
                            is_subagent,
                            timestamp: ts,
                            model,
                            status: Status::Ok,
                            usage: Usage {
                                input: inp,
                                cache_write: 0,
                                cache_read: 0,
                                output: outp,
                                cache_write_5m: None,
                                cache_write_1h: None,
                            },
                            speed: None,
                            blocks,
                            tool_calls,
                            branch: None,
                        }),
                    );
                }
            }
            132 => {
                // Tool Execution Result
                if let Some(m4_bytes) = meta.field(4) {
                    let m4 = ProtoMessage::parse(m4_bytes);
                    let cid = m4.string(1).unwrap_or_default();
                    if !cid.is_empty() {
                        let output_text = payload
                            .field(140)
                            .and_then(|f140_bytes| {
                                let f140 = ProtoMessage::parse(f140_bytes);
                                f140.field(2).and_then(|f2_bytes| {
                                    let f2 = ProtoMessage::parse(f2_bytes);
                                    f2.string(1)
                                })
                            })
                            .unwrap_or_default();

                        let output_prefix: String =
                            output_text.chars().take(OUTPUT_PREFIX).collect();
                        let is_err = status != 3
                            || output_text.contains("Command exited with code")
                                && !output_text.contains("Command exited with code 0");

                        let (commits, prs) = if is_err {
                            (Vec::new(), Vec::new())
                        } else {
                            (
                                git::parse_output(&output_prefix),
                                git::parse_prs(&output_prefix),
                            )
                        };

                        out(
                            project,
                            Record::ToolResult(ToolResult {
                                tool_call_id: cid.to_string(),
                                is_error: is_err,
                                error_text: if is_err && !output_prefix.is_empty() {
                                    Some(output_prefix)
                                } else {
                                    None
                                },
                                timestamp: Some(ts),
                                commits,
                                prs,
                            }),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Map Antigravity tool name and arguments to canonical name, summary, and git dirs.
pub fn parse_tool_call(name: &str, args_json: &str) -> (String, String, Vec<String>) {
    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty());

    let (c_name, summary, git_dirs) = match name {
        "run_command" => {
            let cmd = s("CommandLine")
                .or_else(|| s("command"))
                .unwrap_or_default();
            let sum = s("toolSummary")
                .map(|ts| format!("{ts}: {cmd}"))
                .unwrap_or_else(|| cmd.to_string());
            let dirs = git::commit_dirs(&cmd, None);
            ("Bash", sum, dirs)
        }
        "read_file" => ("Read", s("Path").or_else(|| s("path")).unwrap_or_default().to_string(), Vec::new()),
        "edit_file" => ("Edit", s("Path").or_else(|| s("path")).unwrap_or_default().to_string(), Vec::new()),
        "write_file" => ("Write", s("Path").or_else(|| s("path")).unwrap_or_default().to_string(), Vec::new()),
        "search_web" => ("WebSearch", s("query").unwrap_or_default().to_string(), Vec::new()),
        "invoke_subagent" => (
            "Agent",
            s("Role")
                .or_else(|| s("Prompt"))
                .unwrap_or_default()
                .to_string(),
            Vec::new(),
        ),
        "ask_question" => (
            "AskUserQuestion",
            args["questions"]
                .as_array()
                .map(|qs| {
                    qs.iter()
                        .filter_map(|q| q["question"].as_str())
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .unwrap_or_default(),
            Vec::new(),
        ),
        other => {
            if other.starts_with("mcp__") {
                ("mcp", s("query").or_else(|| s("url")).unwrap_or_default().to_string(), Vec::new())
            } else if clioilc_core::contract::is_label(other) {
                (other, String::new(), Vec::new())
            } else {
                ("other", String::new(), Vec::new())
            }
        }
    };

    (c_name.to_string(), summary, git_dirs)
}

/// Extract model slug from gen_metadata protobuf bytes.
fn extract_model_slug(bytes: &[u8]) -> Option<String> {
    let top = ProtoMessage::parse(bytes);
    let f1_bytes = top.field(1)?;
    let f1 = ProtoMessage::parse(f1_bytes);
    let slug = f1.string(19)?;
    if clioilc_core::contract::is_label(slug) {
        Some(slug.to_string())
    } else {
        None
    }
}

/// Parse google.protobuf.Timestamp (field 1: int64 seconds, field 2: int32 nanos).
fn parse_timestamp(bytes: &[u8]) -> Option<DateTime<Utc>> {
    let msg = ProtoMessage::parse(bytes);
    let secs = msg.varint(1)? as i64;
    let nanos = msg.varint(2).unwrap_or(0) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

// ============================================================================
// Lightweight Pure-Rust Protobuf Parser (Zero Dependencies)
// ============================================================================

struct ProtoMessage<'a> {
    fields: Vec<(u32, u8, &'a [u8])>,
}

impl<'a> ProtoMessage<'a> {
    fn parse(mut data: &'a [u8]) -> Self {
        let mut fields = Vec::new();
        while !data.is_empty() {
            let Some((tag, rest)) = decode_varint(data) else {
                break;
            };
            let wire_type = (tag & 7) as u8;
            let field_num = (tag >> 3) as u32;
            data = rest;
            match wire_type {
                0 => {
                    let Some((_, rest)) = decode_varint(data) else {
                        break;
                    };
                    let len = data.len() - rest.len();
                    fields.push((field_num, wire_type, &data[..len]));
                    data = rest;
                }
                1 => {
                    if data.len() < 8 {
                        break;
                    }
                    fields.push((field_num, wire_type, &data[..8]));
                    data = &data[8..];
                }
                2 => {
                    let Some((len, rest)) = decode_varint(data) else {
                        break;
                    };
                    let len = len as usize;
                    if rest.len() < len {
                        break;
                    }
                    fields.push((field_num, wire_type, &rest[..len]));
                    data = &rest[len..];
                }
                5 => {
                    if data.len() < 4 {
                        break;
                    }
                    fields.push((field_num, wire_type, &data[..4]));
                    data = &data[4..];
                }
                _ => break,
            }
        }
        Self { fields }
    }

    fn field(&self, num: u32) -> Option<&'a [u8]> {
        self.fields
            .iter()
            .find(|(n, _, _)| *n == num)
            .map(|(_, _, d)| *d)
    }

    fn fields(&self, num: u32) -> Vec<&'a [u8]> {
        self.fields
            .iter()
            .filter(|(n, _, _)| *n == num)
            .map(|(_, _, d)| *d)
            .collect()
    }

    fn string(&self, num: u32) -> Option<&'a str> {
        self.field(num).and_then(|b| std::str::from_utf8(b).ok())
    }

    fn varint(&self, num: u32) -> Option<u64> {
        self.field(num)
            .and_then(|b| decode_varint(b).map(|(v, _)| v))
    }
}

fn decode_varint(data: &[u8]) -> Option<(u64, &[u8])> {
    let mut val = 0u64;
    let mut shift = 0;
    for (i, &b) in data.iter().enumerate() {
        val |= ((b & 0x7f) as u64) << shift;
        if (b & 0x80) == 0 {
            return Some((val, &data[i + 1..]));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

// ============================================================================
// Unit Tests (Strict Contract Validation)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_uris_to_canonical_project_name() {
        let uris = r#"["file:///Volumes/SSD1/Private/clioilc"]"#;
        assert_eq!(
            parse_project_name(uris).as_deref(),
            Some("-Volumes-SSD1-Private-clioilc")
        );
        let empty = r#"[]"#;
        assert_eq!(parse_project_name(empty), None);
    }

    #[test]
    fn maps_antigravity_tools_to_canonical_names() {
        let (name, sum, dirs) = parse_tool_call(
            "run_command",
            r#"{"CommandLine":"cd /repo && git commit -m init","toolSummary":"Commit files"}"#,
        );
        assert_eq!(name, "Bash");
        assert_eq!(sum, "Commit files: cd /repo && git commit -m init");
        assert_eq!(dirs, vec!["/repo"]);

        let (name, sum, _) = parse_tool_call("read_file", r#"{"Path":"/src/main.rs"}"#);
        assert_eq!(name, "Read");
        assert_eq!(sum, "/src/main.rs");

        let (name, _, _) = parse_tool_call("mcp__github__get_repo", r#"{}"#);
        assert_eq!(name, "mcp");
    }

    #[test]
    fn conforms_to_reader_contract() {
        let mut records = Vec::new();

        // 1. Prompt record
        records.push(Record::Prompt(Prompt {
            id: "p1".into(),
            session_id: "s1".into(),
            is_subagent: false,
            timestamp: Utc::now(),
            text: "analisis log database".into(),
            branch: None,
        }));

        // 2. Message record
        records.push(Record::Message(Message {
            id: "m1".into(),
            line_id: "s1#1".into(),
            session_id: "s1".into(),
            is_subagent: false,
            timestamp: Utc::now(),
            model: "gemini-3.8-flash-medium".into(),
            status: Status::Ok,
            usage: Usage {
                input: 100,
                cache_write: 0,
                cache_read: 0,
                output: 50,
                cache_write_5m: None,
                cache_write_1h: None,
            },
            speed: None,
            blocks: vec![TextBlock {
                kind: TextKind::Thinking,
                text: "periksa database".into(),
            }],
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "Bash".into(),
                full_name: "run_command".into(),
                summary: "ls -la".into(),
                git_dirs: vec![],
            }],
            branch: None,
        }));

        // 3. ToolResult record
        records.push(Record::ToolResult(ToolResult {
            tool_call_id: "call_1".into(),
            is_error: false,
            error_text: None,
            timestamp: Some(Utc::now()),
            commits: vec![],
            prs: vec![],
        }));

        // Validate contract
        clioilc_core::contract::assert_conforms("antigravity", Some("-Volumes-SSD1-test"), &records);
    }
}
