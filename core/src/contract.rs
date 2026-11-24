//! The reader contract: what a [`Record`] must look like, whatever the
//! tool's log format (see `docs/readers.md`).
//!
//! [`crate::UsageDb::insert`] checks every record against it. A record that
//! would corrupt the data (an empty id merges unrelated rows into one, a
//! timestamp in 1970 lands outside every report) is **rejected**: skipped
//! and counted. A value that can be stored but loses something (a model
//! name with spaces becomes `unknown`) is **warned** about and stored with
//! its fallback. The import and watch commands print both counts, so a
//! reader that drifts from the contract is noticed instead of silently
//! filling the dashboard with `unknown`.
//!
//! Reader tests call [`assert_conforms`] on what they parse, which fails on
//! warnings too.

use crate::usage::{Record, Status};
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// How a broken rule is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The record is skipped.
    Reject,
    /// The value is stored with a fallback (or left out).
    Warn,
}

/// One broken rule. `rule` names the field and what is wrong with it; the
/// same strings are listed in `docs/readers.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub rule: &'static str,
}

const fn reject(rule: &'static str) -> Problem {
    Problem {
        severity: Severity::Reject,
        rule,
    }
}
const fn warn(rule: &'static str) -> Problem {
    Problem {
        severity: Severity::Warn,
        rule,
    }
}

/// A short identifier: what shareable label columns accept (model, tool
/// name, error kind, speed). The server validates the same pattern.
pub fn is_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.<>".contains(&b))
}

/// A commit hash the server accepts: 7 to 64 lowercase hex characters.
pub fn is_commit_hash(s: &str) -> bool {
    (7..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A pull request URL the server accepts.
pub fn is_pr_url(s: &str) -> bool {
    s.len() <= 500
        && (s.starts_with("https://") || s.starts_with("http://"))
        && !s.contains(char::is_whitespace)
}

/// Longest project name the server stores.
pub const MAX_PROJECT: usize = 1000;

/// Earliest timestamp accepted: no AI coding agent logged before this.
fn earliest() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
}

fn time_ok(ts: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    ts >= earliest() && ts <= now + Duration::days(1)
}

/// Every rule `record` (from reader `tool`, in `project`) breaks.
pub fn check(
    tool: &str,
    project: Option<&str>,
    record: &Record,
    now: DateTime<Utc>,
) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut need = |ok: bool, p: Problem| {
        if !ok {
            out.push(p);
        }
    };
    need(is_label(tool), reject("reader name is not a label"));
    match record {
        Record::Message(m) => {
            need(!m.id.is_empty(), reject("message.id is empty"));
            need(!m.line_id.is_empty(), reject("message.line_id is empty"));
            need(
                !m.session_id.is_empty(),
                reject("message.session_id is empty"),
            );
            need(
                time_ok(m.timestamp, now),
                reject("message.timestamp is before 2020 or in the future"),
            );
            need(
                project.is_some(),
                warn("project is missing (stored as '-')"),
            );
            need(
                is_label(&m.model),
                warn("message.model is not a label (stored as 'unknown')"),
            );
            if let Status::Error(kind) = &m.status {
                need(
                    is_label(kind),
                    warn("message.status error kind is not a label (stored as 'api_error')"),
                );
            }
            if let Some(speed) = &m.speed {
                need(
                    is_label(speed),
                    warn("message.speed is not a label (stored as 'unknown')"),
                );
            }
            let u = &m.usage;
            let split_ok = match (u.cache_write_5m, u.cache_write_1h) {
                (None, None) => true,
                (Some(a), Some(b)) => a.checked_add(b) == Some(u.cache_write),
                _ => false,
            };
            need(
                split_ok,
                warn("message.usage cache split does not add up to cache_write (split dropped)"),
            );
            for b in &m.blocks {
                need(
                    !b.text.trim().is_empty(),
                    warn("message.blocks has an empty text (stored empty)"),
                );
            }
            for c in &m.tool_calls {
                need(!c.id.is_empty(), reject("message.tool_calls[].id is empty"));
                need(
                    is_label(&c.name),
                    warn("message.tool_calls[].name is not a label (stored as 'other')"),
                );
                need(
                    !c.name.contains("__"),
                    warn("message.tool_calls[].name looks like a private name (map it to 'mcp')"),
                );
                need(
                    !c.full_name.is_empty(),
                    warn("message.tool_calls[].full_name is empty"),
                );
            }
        }
        Record::ToolResult(r) => {
            need(
                !r.tool_call_id.is_empty(),
                reject("tool_result.tool_call_id is empty"),
            );
            if let Some(ts) = r.timestamp {
                need(
                    time_ok(ts, now),
                    warn("tool_result.timestamp is before 2020 or in the future (ignored)"),
                );
            }
            need(
                r.is_error || r.error_text.is_none(),
                warn("tool_result.error_text is set on a success (ignored)"),
            );
            for c in &r.commits {
                need(
                    is_commit_hash(&c.hash),
                    warn("tool_result.commits[].hash is not 7-64 lowercase hex (commit dropped)"),
                );
            }
            for pr in &r.prs {
                need(is_pr_url(pr), warn("tool_result.prs[] is not an http(s) URL of at most 500 characters (dropped)"));
            }
        }
        Record::Prompt(p) => {
            need(!p.id.is_empty(), reject("prompt.id is empty"));
            need(
                !p.session_id.is_empty(),
                reject("prompt.session_id is empty"),
            );
            need(
                time_ok(p.timestamp, now),
                reject("prompt.timestamp is before 2020 or in the future"),
            );
            need(
                project.is_some(),
                warn("project is missing (stored as '-')"),
            );
            need(!p.text.trim().is_empty(), warn("prompt.text is empty"));
        }
    }
    if let Some(name) = project {
        need(!name.is_empty(), warn("project is empty"));
        need(
            name.chars().count() <= MAX_PROJECT,
            warn("project is longer than 1000 characters (name cut)"),
        );
    }
    out
}

/// Broken rules counted over many records, for one summary line per run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    /// Records skipped, by the first rejecting rule.
    pub rejected: BTreeMap<&'static str, u64>,
    /// Values stored with a fallback, by rule.
    pub warned: BTreeMap<&'static str, u64>,
}

impl Tally {
    /// Counts `problems` of one record; true when the record must be skipped.
    pub fn add(&mut self, problems: &[Problem]) -> bool {
        if let Some(p) = problems.iter().find(|p| p.severity == Severity::Reject) {
            *self.rejected.entry(p.rule).or_default() += 1;
            return true;
        }
        for p in problems {
            *self.warned.entry(p.rule).or_default() += 1;
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.rejected.is_empty() && self.warned.is_empty()
    }

    /// Lines for stderr, prefixed with the reader name; none when clean.
    pub fn report(&self, reader: &str) -> Vec<String> {
        let list = |m: &BTreeMap<&str, u64>| {
            let mut s = String::new();
            for (k, (rule, n)) in m.iter().enumerate() {
                let _ = write!(s, "{}{rule} ×{n}", if k > 0 { "; " } else { "" });
            }
            s
        };
        let mut out = Vec::new();
        if !self.rejected.is_empty() {
            let n: u64 = self.rejected.values().sum();
            out.push(format!(
                "[{reader}] skipped {n} record(s) that break the reader contract (docs/readers.md): {}",
                list(&self.rejected)
            ));
        }
        if !self.warned.is_empty() {
            out.push(format!(
                "[{reader}] stored with a fallback: {}",
                list(&self.warned)
            ));
        }
        out
    }
}

/// For reader tests: panics, listing every broken rule (warnings too),
/// unless all `records` follow the contract.
#[track_caller]
pub fn assert_conforms(tool: &str, project: Option<&str>, records: &[Record]) {
    let now = Utc::now();
    let broken: Vec<String> = records
        .iter()
        .enumerate()
        .flat_map(|(i, r)| {
            check(tool, project, r, now)
                .into_iter()
                .map(move |p| format!("record {i}: {} ({:?})", p.rule, p.severity))
        })
        .collect();
    assert!(
        broken.is_empty(),
        "records break the reader contract:\n{}",
        broken.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::{Message, Prompt, TextBlock, TextKind, ToolCall, ToolResult, Usage};

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 3, 0, 0).unwrap()
    }

    fn message() -> Message {
        Message {
            id: "msg_1".into(),
            line_id: "l1".into(),
            session_id: "s1".into(),
            is_subagent: false,
            timestamp: ts(),
            model: "claude-opus-5-5".into(),
            status: Status::Ok,
            usage: Usage {
                input: 1,
                cache_write: 10,
                cache_read: 0,
                output: 2,
                cache_write_5m: Some(4),
                cache_write_1h: Some(6),
            },
            speed: Some("standard".into()),
            blocks: vec![TextBlock {
                kind: TextKind::Text,
                text: "hi".into(),
            }],
            tool_calls: vec![ToolCall {
                id: "tu_1".into(),
                name: "Bash".into(),
                full_name: "Bash".into(),
                summary: "ls".into(),
                git_dirs: vec![],
            }],
            branch: None,
        }
    }

    fn rules(r: &Record, project: Option<&str>) -> Vec<&'static str> {
        check("claude", project, r, ts())
            .into_iter()
            .map(|p| p.rule)
            .collect()
    }

    #[test]
    fn a_good_message_passes() {
        assert_conforms("claude", Some("-r-app"), &[Record::Message(message())]);
    }

    #[test]
    fn ids_and_times_reject() {
        let mut m = message();
        m.id.clear();
        m.timestamp = Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
        let problems = check("claude", Some("p"), &Record::Message(m), ts());
        assert_eq!(
            problems
                .iter()
                .map(|p| (p.severity, p.rule))
                .collect::<Vec<_>>(),
            [
                (Severity::Reject, "message.id is empty"),
                (
                    Severity::Reject,
                    "message.timestamp is before 2020 or in the future"
                ),
            ]
        );
        let p = Prompt {
            id: "p1".into(),
            session_id: String::new(),
            is_subagent: false,
            timestamp: ts() + Duration::days(3),
            text: "x".into(),
            branch: None,
        };
        assert_eq!(
            rules(&Record::Prompt(p), Some("p")),
            [
                "prompt.session_id is empty",
                "prompt.timestamp is before 2020 or in the future"
            ]
        );
    }

    #[test]
    fn degraded_values_warn() {
        let mut m = message();
        m.model = "Claude Opus 5.5".into();
        m.status = Status::Error("rate limited!".into());
        m.usage.cache_write_1h = None;
        m.tool_calls[0].name = "mcp__crm__lookup".into();
        assert_eq!(
            rules(&Record::Message(m), None),
            [
                "project is missing (stored as '-')",
                "message.model is not a label (stored as 'unknown')",
                "message.status error kind is not a label (stored as 'api_error')",
                "message.usage cache split does not add up to cache_write (split dropped)",
                "message.tool_calls[].name looks like a private name (map it to 'mcp')",
            ]
        );
    }

    #[test]
    fn tool_results_check_commits_and_prs() {
        let r = ToolResult {
            tool_call_id: "tu_1".into(),
            is_error: false,
            error_text: Some("noise".into()),
            timestamp: Some(ts()),
            commits: vec![crate::git::Commit {
                hash: "XYZ".into(),
                ..Default::default()
            }],
            prs: vec!["github.com/a/b/pull/1".into()],
        };
        assert_eq!(rules(&Record::ToolResult(r), None).len(), 3);
    }

    #[test]
    fn tally_skips_on_reject_and_reports() {
        let mut t = Tally::default();
        assert!(t.add(&[warn("w"), reject("r")]));
        assert!(!t.add(&[warn("w")]));
        assert!(!t.add(&[]));
        assert_eq!(t.rejected.get("r"), Some(&1));
        assert_eq!(
            t.warned.get("w"),
            Some(&1),
            "a rejected record's warnings are not counted"
        );
        assert_eq!(
            t.report("x"),
            [
                "[x] skipped 1 record(s) that break the reader contract (docs/readers.md): r ×1",
                "[x] stored with a fallback: w ×1"
            ]
        );
        assert!(Tally::default().report("x").is_empty());
    }

    #[test]
    fn labels_hashes_and_urls() {
        assert!(is_label("swe-2-flash-medium") && is_label("<synthetic>"));
        assert!(!is_label("") && !is_label("a b") && !is_label(&"x".repeat(65)));
        assert!(is_commit_hash("3c514b0") && !is_commit_hash("3C514B0") && !is_commit_hash("abc"));
        assert!(is_pr_url("https://github.com/a/b/pull/1") && !is_pr_url("github.com/a/b/pull/1"));
    }
}
