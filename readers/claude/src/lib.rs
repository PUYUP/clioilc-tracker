use anyhow::Result;
use chrono::{DateTime, Utc};
use clioilc_core::usage::{
    Message, Prompt, Record, Status, TextBlock, TextKind, ToolCall, ToolResult, Usage,
};
use clioilc_core::{home_dir, Reader};
use memchr::memmem;
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::path::{Path, PathBuf};

/// Max bytes of tool error output kept while parsing (the store truncates
/// further after redaction).
const ERROR_PREFIX: usize = 4_096;
/// Context Claude Code injects into user prompts; not something the human typed.
const INJECTED_TAGS: &[&str] = &["system-reminder", "ide_opened_file", "ide_selection"];

/// Reader for Claude Code session logs in `~/.claude/projects`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeReader;

impl Reader for ClaudeReader {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn default_watch_dir(&self) -> Result<PathBuf> {
        Ok(home_dir()?.join(".claude").join("projects"))
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json", "jsonl"]
    }

    /// `<project-slug>/<session>.jsonl` -> `<project-slug>`
    fn project_name(&self, relative: &Path) -> Option<String> {
        let mut parts = relative.components();
        let project = parts.next()?;
        // A file directly in the watch root belongs to no project.
        parts.next()?;
        Some(project.as_os_str().to_string_lossy().into_owned())
    }

    fn parse_line(&self, line: &[u8]) -> Vec<Record> {
        // Many lines (request metadata, attachments, file snapshots) are of
        // no interest. Skip them with a SIMD substring search before paying
        // for JSON parsing.
        if memmem::find(line, br#""type":"assistant""#).is_none()
            && memmem::find(line, br#""type":"user""#).is_none()
        {
            return Vec::new();
        }
        match serde_json::from_slice::<Line>(line) {
            Ok(l) => l.into_records(),
            Err(_) => Vec::new(),
        }
    }
}

/// The subset of a log line we read. Unknown fields (file snapshots, tool
/// output objects, account ids, ...) are skipped without being allocated.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    uuid: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_api_error_message: bool,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_compact_summary: bool,
    origin: Option<Origin>,
    #[serde(default, deserialize_with = "string_or_none")]
    error: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message: Option<LineMessage>,
}

#[derive(Deserialize)]
struct Origin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct LineMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LineUsage>,
    #[serde(default)]
    content: Blocks,
}

#[derive(Deserialize)]
struct LineUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    thinking: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<ToolInput>,
    tool_use_id: Option<String>,
    is_error: Option<bool>,
    #[serde(default)]
    content: ResultText,
}

/// The tool input fields that describe an action. Bulky ones (file
/// contents, edit strings, subagent prompts) are not listed and are skipped.
#[derive(Deserialize, Default)]
struct ToolInput {
    description: Option<String>,
    command: Option<String>,
    file_path: Option<String>,
    notebook_path: Option<String>,
    url: Option<String>,
    query: Option<String>,
    pattern: Option<String>,
    path: Option<String>,
    skill: Option<String>,
    task_id: Option<String>,
    title: Option<String>,
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    question: Option<String>,
}

impl Line {
    fn into_records(self) -> Vec<Record> {
        let Some(msg) = self.message else {
            return Vec::new();
        };
        let (Some(session_id), Some(timestamp)) = (self.session_id, self.timestamp) else {
            return Vec::new();
        };
        match self.kind.as_str() {
            "assistant" => {
                let (Some(id), Some(line_id)) = (msg.id, self.uuid) else {
                    return Vec::new();
                };
                let usage = msg.usage.map_or_else(Usage::default, |u| Usage {
                    input: u.input_tokens,
                    cache_write: u.cache_creation_input_tokens,
                    cache_read: u.cache_read_input_tokens,
                    output: u.output_tokens,
                });
                let status = if self.is_api_error_message {
                    Status::Error(self.error.unwrap_or_else(|| "api_error".into()))
                } else {
                    Status::Ok
                };
                let mut blocks = Vec::new();
                let mut tool_calls = Vec::new();
                for b in msg.content.0 {
                    match b.kind.as_str() {
                        "text" => blocks.extend(text_block(TextKind::Text, b.text)),
                        "thinking" => blocks.extend(text_block(TextKind::Thinking, b.thinking)),
                        "tool_use" => {
                            if let (Some(id), Some(name)) = (b.id, b.name) {
                                let summary = summarize(&b.input.unwrap_or_default());
                                tool_calls.push(ToolCall {
                                    id,
                                    name: public_tool_name(&name),
                                    full_name: name,
                                    summary,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                vec![Record::Message(Message {
                    id,
                    line_id,
                    session_id,
                    is_subagent: self.is_sidechain,
                    timestamp,
                    model: msg.model.unwrap_or_else(|| "unknown".into()),
                    status,
                    usage,
                    blocks,
                    tool_calls,
                })]
            }
            "user" => {
                let mut records = Vec::new();
                let mut prompt = String::new();
                for b in msg.content.0 {
                    match b.kind.as_str() {
                        "tool_result" => {
                            let Some(tool_call_id) = b.tool_use_id else {
                                continue;
                            };
                            let is_error = b.is_error.unwrap_or(false);
                            records.push(Record::ToolResult(ToolResult {
                                tool_call_id,
                                is_error,
                                error_text: is_error
                                    .then_some(b.content.0)
                                    .filter(|t| !t.is_empty()),
                            }));
                        }
                        "text" => push_paragraph(
                            &mut prompt,
                            &strip_injected(b.text.as_deref().unwrap_or("")),
                        ),
                        "image" => push_paragraph(&mut prompt, "[image]"),
                        _ => {}
                    }
                }
                if let Some(text) = msg.content.1 {
                    push_paragraph(&mut prompt, &strip_injected(&text));
                }
                let typed_by_human = !self.is_meta
                    && !self.is_compact_summary
                    && self
                        .origin
                        .and_then(|o| o.kind)
                        .is_none_or(|kind| kind == "human");
                if typed_by_human && !prompt.is_empty() {
                    if let Some(id) = self.uuid {
                        records.push(Record::Prompt(Prompt {
                            id,
                            session_id,
                            is_subagent: self.is_sidechain,
                            timestamp,
                            text: prompt,
                        }));
                    }
                }
                records
            }
            _ => Vec::new(),
        }
    }
}

fn text_block(kind: TextKind, text: Option<String>) -> Option<TextBlock> {
    text.filter(|t| !t.trim().is_empty())
        .map(|text| TextBlock { kind, text })
}

/// One line describing what a tool call did: e.g. `List files: ls -la`,
/// a file path, or a URL. Bulky inputs (file contents, edits) are never used.
fn summarize(input: &ToolInput) -> String {
    let questions: Vec<&str> = input
        .questions
        .iter()
        .filter_map(|q| q.question.as_deref())
        .collect();
    let questions = questions.join(" | ");
    let candidates = [
        input.description.as_deref(),
        input.command.as_deref(),
        input.file_path.as_deref(),
        input.notebook_path.as_deref(),
        input.url.as_deref(),
        input.query.as_deref(),
        input.pattern.as_deref(),
        input.path.as_deref(),
        input.skill.as_deref(),
        input.task_id.as_deref(),
        input.title.as_deref(),
        Some(questions.as_str()),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(": ")
}

/// MCP tool names embed the server name (`mcp__<server>__<tool>`), which can
/// reveal private integrations; the shareable name is just `mcp`.
fn public_tool_name(name: &str) -> String {
    if name.starts_with("mcp__") {
        "mcp".into()
    } else {
        name.into()
    }
}

/// Remove `<tag>...</tag>` blocks that Claude Code injects into prompts.
fn strip_injected(text: &str) -> String {
    let mut out = text.to_string();
    for tag in INJECTED_TAGS {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        while let Some(start) = out.find(&open) {
            let end = out[start..]
                .find(&close)
                .map_or(out.len(), |i| start + i + close.len());
            out.replace_range(start..end, "");
        }
    }
    out.trim().to_string()
}

fn push_paragraph(out: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(text);
}

/// `message.content`: a string (plain prompt) or a list of blocks.
#[derive(Default)]
struct Blocks(Vec<Block>, Option<String>);

impl<'de> Deserialize<'de> for Blocks {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Blocks;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or a list of content blocks")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Blocks, E> {
                Ok(Blocks(Vec::new(), Some(s.to_string())))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Blocks, E> {
                Ok(Blocks::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Blocks, A::Error> {
                let mut blocks = Vec::new();
                while let Some(b) = seq.next_element::<Option<Block>>()? {
                    blocks.extend(b);
                }
                Ok(Blocks(blocks, None))
            }
        }
        d.deserialize_any(V)
    }
}

/// A tool result's `content` (string or list of text blocks), keeping only
/// a bounded prefix: successful outputs can be hundreds of KB.
#[derive(Default)]
struct ResultText(String);

impl ResultText {
    fn push(&mut self, s: &str) {
        let room = ERROR_PREFIX.saturating_sub(self.0.len());
        if room == 0 {
            return;
        }
        if !self.0.is_empty() {
            self.0.push('\n');
        }
        let mut cut = s.len().min(room);
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.0.push_str(&s[..cut]);
    }
}

impl<'de> Deserialize<'de> for ResultText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = ResultText;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("tool result content")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<ResultText, E> {
                let mut t = ResultText::default();
                t.push(s);
                Ok(t)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<ResultText, E> {
                Ok(ResultText::default())
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<ResultText, A::Error> {
                let mut t = ResultText::default();
                while let Some(part) = seq.next_element::<Option<ResultPart>>()? {
                    if let Some(text) = part.and_then(|p| p.text) {
                        t.push(&text.0);
                    }
                }
                Ok(t)
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
struct ResultPart {
    text: Option<ResultText>,
}

/// `error` is a short kind string on API errors; ignore any other shape.
fn string_or_none<'de, D: Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        S(String),
        Other(IgnoredAny),
    }
    Ok(match Value::deserialize(d)? {
        Value::S(s) => Some(s),
        Value::Other(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_is_first_directory_under_root() {
        let r = ClaudeReader;
        assert_eq!(
            r.project_name(Path::new("-Users-me-projects-app/abc.jsonl"))
                .as_deref(),
            Some("-Users-me-projects-app")
        );
        assert_eq!(
            r.project_name(Path::new("slug/session/sub.json"))
                .as_deref(),
            Some("slug")
        );
        assert_eq!(r.project_name(Path::new("top-level.json")), None);
    }

    fn parse(line: &str) -> Vec<Record> {
        ClaudeReader.parse_line(line.as_bytes())
    }

    fn one_message(line: &str) -> Message {
        match <[Record; 1]>::try_from(parse(line)) {
            Ok([Record::Message(m)]) => m,
            other => panic!("expected one message, got {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_usage_text_and_tool_calls() {
        let line = r#"{"type":"assistant","uuid":"line-1","sessionId":"s1","isSidechain":true,"timestamp":"2026-11-20T01:23:09.864Z","cwd":"/secret","message":{"id":"msg_1","model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"plan it","signature":"sig"},{"type":"thinking","thinking":"","signature":"sig"},{"type":"text","text":"Saya cek dulu."},{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls -la","description":"List files","timeout":5}},{"type":"tool_use","id":"tu_2","name":"Write","input":{"file_path":"/p/a.rs","content":"huge file body"}},{"type":"tool_use","id":"tu_3","name":"mcp__acme_crm__lookup","input":{"title":"Find customer"}}],"usage":{"input_tokens":2,"cache_creation_input_tokens":1041,"cache_read_input_tokens":182940,"output_tokens":452,"service_tier":"standard"}}}"#;
        let m = one_message(line);
        assert_eq!(
            (
                m.id.as_str(),
                m.line_id.as_str(),
                m.session_id.as_str(),
                m.is_subagent
            ),
            ("msg_1", "line-1", "s1", true)
        );
        assert_eq!(m.status, Status::Ok);
        assert_eq!(m.usage.output, 452);
        assert_eq!(
            m.blocks,
            [
                TextBlock {
                    kind: TextKind::Thinking,
                    text: "plan it".into()
                },
                TextBlock {
                    kind: TextKind::Text,
                    text: "Saya cek dulu.".into()
                },
            ]
        );
        let calls: Vec<_> = m
            .tool_calls
            .iter()
            .map(|c| (c.name.as_str(), c.full_name.as_str(), c.summary.as_str()))
            .collect();
        assert_eq!(
            calls,
            [
                ("Bash", "Bash", "List files: ls -la"),
                ("Write", "Write", "/p/a.rs"),
                ("mcp", "mcp__acme_crm__lookup", "Find customer"),
            ]
        );
    }

    #[test]
    fn parses_api_errors() {
        let line = r#"{"type":"assistant","uuid":"l","isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{"id":"msg_e","model":"<synthetic>","content":[{"type":"text","text":"API Error: Rate limit reached"}],"usage":{"input_tokens":0,"output_tokens":0}}}"#;
        let m = one_message(line);
        assert_eq!(m.status, Status::Error("rate_limit".into()));
        assert_eq!(m.model, "<synthetic>");
    }

    #[test]
    fn parses_tool_results_with_bounded_error_text() {
        let big = "y".repeat(10_000);
        let line = format!(
            r#"{{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu_1","is_error":true,"content":"boom"}},{{"type":"tool_result","tool_use_id":"tu_2","content":[{{"type":"text","text":"{big}"}}]}},{{"type":"tool_result","tool_use_id":"tu_3","is_error":true,"content":[{{"type":"text","text":"{big}"}}]}}]}},"toolUseResult":{{"stdout":"lots"}}}}"#
        );
        let results: Vec<_> = parse(&line)
            .into_iter()
            .map(|r| match r {
                Record::ToolResult(t) => {
                    (t.tool_call_id, t.is_error, t.error_text.map(|e| e.len()))
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            results,
            [
                ("tu_1".to_string(), true, Some(4)),
                ("tu_2".to_string(), false, None),
                ("tu_3".to_string(), true, Some(ERROR_PREFIX)),
            ]
        );
    }

    #[test]
    fn parses_human_prompts_without_injected_context() {
        let line = r#"{"type":"user","uuid":"u1","sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>The user opened a.rs</ide_opened_file>"},{"type":"text","text":"tes baca token"}]}}"#;
        let got = parse(line);
        let [Record::Prompt(p)] = got.as_slice() else {
            panic!("expected a prompt, got {got:?}");
        };
        assert_eq!((p.id.as_str(), p.text.as_str()), ("u1", "tes baca token"));

        let plain = r#"{"type":"user","uuid":"u2","sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{"role":"user","content":"hello <system-reminder>noise</system-reminder>"}}"#;
        let got = parse(plain);
        let [Record::Prompt(p)] = got.as_slice() else {
            panic!("expected a prompt, got {got:?}");
        };
        assert_eq!(p.text, "hello");
    }

    #[test]
    fn skips_non_human_and_irrelevant_lines() {
        let meta = r#"{"type":"user","uuid":"u","isMeta":true,"sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{"role":"user","content":"Caveat: injected"}}"#;
        let notif = r#"{"type":"user","uuid":"u","origin":{"kind":"task-notification"},"sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{"role":"user","content":"task done"}}"#;
        let only_context = r#"{"type":"user","uuid":"u","sessionId":"s1","timestamp":"2026-11-20T01:00:00Z","message":{"role":"user","content":"<ide_opened_file>x</ide_opened_file>"}}"#;
        for line in [
            meta,
            notif,
            only_context,
            r#"{"type":"ai-title","aiTitle":"x"}"#,
            r#"{"type":"assistant", broken"#,
        ] {
            assert!(parse(line).is_empty(), "{line}");
        }
    }
}
