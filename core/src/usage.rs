//! Facts parsed from AI tool logs.
//!
//! Readers fill these with raw values exactly as found in the log.
//! [`crate::usage_db::UsageDb`] decides what goes where: identifiers are
//! hashed and labels whitelisted for the shareable tables, and content is
//! redacted and truncated for the private ones. A reader therefore cannot
//! leak private data into shareable tables by accident.

use chrono::{DateTime, Utc};

/// One fact from one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Record {
    /// A model response. The same response may appear on several lines
    /// (Claude writes one line per content block); `Message::id` dedupes them.
    Message(Message),
    /// The outcome of an earlier tool call.
    ToolResult(ToolResult),
    /// Something the human typed.
    Prompt(Prompt),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Provider message id, shared by all lines of one response.
    pub id: String,
    /// Id of this log line; distinguishes the blocks of one response.
    pub line_id: String,
    /// Session the response belongs to; subagents report their parent's.
    pub session_id: String,
    pub is_subagent: bool,
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub status: Status,
    pub usage: Usage,
    /// Private: visible text and thinking on this line.
    pub blocks: Vec<TextBlock>,
    /// Tool calls requested on this line.
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Machine-readable error kind, e.g. `rate_limit`.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Thinking,
}

impl TextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TextKind::Text => "text",
            TextKind::Thinking => "thinking",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextBlock {
    pub kind: TextKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Provider tool-call id, links the result to the call.
    pub id: String,
    /// Shareable tool name. Readers map names that may reveal private setup
    /// (e.g. custom MCP servers) to a generic label.
    pub name: String,
    /// Private: the real tool name.
    pub full_name: String,
    /// Private: a one-line description of the action (command, file, URL).
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub is_error: bool,
    /// Private: the error output, if the call failed.
    pub error_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// Id of the log line, used for dedupe.
    pub id: String,
    pub session_id: String,
    pub is_subagent: bool,
    pub timestamp: DateTime<Utc>,
    /// Private: the prompt with tool-injected context removed.
    pub text: String,
}
