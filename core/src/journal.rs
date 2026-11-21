//! Engineering work journal (backend docs/journal.md): work items built from
//! the facts in `usage.db`, anchored to git.
//!
//! A work item is one line of work in one project: it starts with a human
//! prompt and may span sessions and days. Whether the next prompt continues it
//! depends on the state of the work, never on the clock alone:
//!
//! - uncommitted edits: the next prompt on the same branch continues it,
//!   however long the pause (holidays, illness);
//! - committed, nothing edited since: the next prompt starts a new item
//!   (pushing later still marks the earlier item pushed);
//! - no edits at all (questions, discussion): a new item after 2 hours idle.
//!
//! Only facts are stored. Statuses that depend on the time of viewing
//! ("pending since Wednesday") are worked out by the dashboard.

use crate::git::Commit;
use crate::usage_db::UsageDb;
use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::params;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// An item without edits ends after this long without activity.
const IDLE_CLOSE_SECS: i64 = 2 * 3600;
/// Gaps longer than this inside an item are recorded as pauses.
const PAUSE_SECS: i64 = 24 * 3600;
/// Gaps longer than this do not count as active time (same as the Digest).
const ACTIVE_GAP_SECS: i64 = 15 * 60;
/// Open items are checked against the repository only after this long idle,
/// so work in progress is never second-guessed.
const REPO_CHECK_AFTER_SECS: i64 = 3600;

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
const MAX_FILES: usize = 50;
const MAX_ERRORS: usize = 20;
const MAX_COMMITS: usize = 100;
const MAX_SPANS: usize = 200;
const MAX_TITLE: usize = 120;
/// Opening prompts shorter than this are replies ("ya lanjut"), not titles.
const SHORT_TITLE: usize = 20;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WorkItem {
    #[serde(skip)]
    pub id: String,
    pub project_id: String,
    /// First line of the opening prompt.
    pub title: Option<String>,
    pub branch: Option<String>,
    /// `open` or `closed`.
    pub state: &'static str,
    /// `commit`, `push`, `pr`, `idle`, `outside` or `discarded` when closed.
    pub closed_by: Option<&'static str>,
    pub edited: bool,
    /// Edits made after the last commit (work not committed yet).
    pub pending_edits: bool,
    pub committed: bool,
    pub pushed: bool,
    pub started: String,
    pub last_activity: String,
    pub pauses: Vec<Pause>,
    pub sessions: usize,
    pub prompts: usize,
    pub active_ms: i64,
    /// Where the item's messages are: lets the server price them
    /// (session ids are the same hashes the server already has).
    pub spans: Vec<Span>,
    pub tokens: Tokens,
    pub commits: Vec<Commit>,
    pub prs: Vec<String>,
    pub files: Vec<FileEdits>,
    pub errors: Vec<ErrorGroup>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Pause {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Span {
    pub session_id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Tokens {
    pub input: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    pub output: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileEdits {
    pub path: String,
    pub edits: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorGroup {
    pub fingerprint: String,
    pub tool: String,
    pub count: u32,
    /// The first error of the group, as stored (already redacted, capped).
    pub sample: String,
}

/// One fact, in time order within a project.
enum Event {
    Prompt {
        id: String,
        session: String,
        text: String,
        branch: Option<String>,
    },
    Tool {
        session: String,
        name: String,
        summary: String,
        status: Option<String>,
        error: Option<String>,
        commits: Vec<Commit>,
        prs: Vec<String>,
        branch: Option<String>,
    },
    Message {
        session: String,
        tokens: Tokens,
    },
}

struct Building {
    item: WorkItem,
    last: i64,
    last_commit: Option<i64>,
    sessions: Vec<String>,
    files: BTreeMap<String, u32>,
    errors: Vec<ErrorGroup>,
    spans: Vec<Span>,
}

impl Building {
    fn new(id: String, project: &str, ts: i64, branch: Option<String>) -> Self {
        Building {
            item: WorkItem {
                id,
                project_id: project.to_string(),
                title: None,
                branch,
                state: "open",
                closed_by: None,
                edited: false,
                pending_edits: false,
                committed: false,
                pushed: false,
                started: iso(ts),
                last_activity: iso(ts),
                pauses: Vec::new(),
                sessions: 0,
                prompts: 0,
                active_ms: 0,
                spans: Vec::new(),
                tokens: Tokens::default(),
                commits: Vec::new(),
                prs: Vec::new(),
                files: Vec::new(),
                errors: Vec::new(),
            },
            last: ts,
            last_commit: None,
            sessions: Vec::new(),
            files: BTreeMap::new(),
            errors: Vec::new(),
            spans: Vec::new(),
        }
    }

    fn is_open(&self) -> bool {
        self.item.state == "open"
    }

    fn close(&mut self, by: &'static str) {
        self.item.state = "closed";
        self.item.closed_by = Some(by);
    }

    /// Record activity at `ts` in `session`: active time, pauses, spans.
    fn touch(&mut self, ts: i64, session: &str) {
        let gap = ts - self.last;
        if gap > 0 && gap <= ACTIVE_GAP_SECS {
            self.item.active_ms += gap * 1000;
        }
        if gap > PAUSE_SECS {
            self.item.pauses.push(Pause {
                from: iso(self.last),
                to: iso(ts),
            });
        }
        self.last = self.last.max(ts);
        self.item.last_activity = iso(self.last);
        if !self.sessions.iter().any(|s| s == session) {
            self.sessions.push(session.to_string());
        }
        match self.spans.last_mut() {
            Some(s) if s.session_id == session => s.to = iso(ts),
            _ => self.spans.push(Span {
                session_id: session.to_string(),
                from: iso(ts),
                to: iso(ts),
            }),
        }
    }

    fn finish(mut self) -> WorkItem {
        self.item.sessions = self.sessions.len();
        // "ya lanjut", "push it": a reply, not a name for the work. The
        // first commit says better what it was.
        let short = self
            .item
            .title
            .as_deref()
            .is_none_or(|t| t.chars().count() < SHORT_TITLE);
        if short {
            if let Some(c) = self.item.commits.first() {
                self.item.title = Some(c.subject.clone());
            }
        }
        let mut files: Vec<FileEdits> = self
            .files
            .into_iter()
            .map(|(path, edits)| FileEdits { path, edits })
            .collect();
        files.sort_by(|a, b| b.edits.cmp(&a.edits).then(a.path.cmp(&b.path)));
        files.truncate(MAX_FILES);
        self.item.files = files;
        self.errors.sort_by_key(|e| std::cmp::Reverse(e.count));
        self.errors.truncate(MAX_ERRORS);
        self.item.errors = self.errors;
        // Merge spans of the same session when there are too many.
        if self.spans.len() > MAX_SPANS {
            let mut merged: Vec<Span> = Vec::new();
            for s in self.spans {
                match merged.iter_mut().find(|m| m.session_id == s.session_id) {
                    Some(m) => {
                        m.from = m.from.clone().min(s.from);
                        m.to = m.to.clone().max(s.to);
                    }
                    None => merged.push(s),
                }
            }
            self.spans = merged;
        }
        self.item.spans = self.spans;
        self.item
    }
}

/// Build the work items of every project from the events in time order.
/// `item_id` makes a stable id from the opening prompt's id.
fn build(
    projects: BTreeMap<String, Vec<(i64, Event)>>,
    item_id: &dyn Fn(&str) -> String,
) -> Vec<WorkItem> {
    let mut out = Vec::new();
    for (project, events) in projects {
        let mut items: Vec<Building> = Vec::new();
        // Open item per branch (None = branch unknown), and the item the
        // latest prompt went to (tool calls and replies belong to it).
        let mut open: HashMap<Option<String>, usize> = HashMap::new();
        let mut current: Option<usize> = None;
        for (ts, event) in events {
            match event {
                Event::Prompt {
                    id,
                    session,
                    text,
                    branch,
                } => {
                    let continue_with = open.get(&branch).copied().filter(|&i| {
                        let b = &mut items[i];
                        if !b.is_open() {
                            return false;
                        }
                        if b.item.pending_edits {
                            return true;
                        }
                        if b.item.committed {
                            b.close("commit");
                            return false;
                        }
                        if ts - b.last > IDLE_CLOSE_SECS {
                            b.close("idle");
                            return false;
                        }
                        true
                    });
                    let i = match continue_with {
                        Some(i) => i,
                        None => {
                            items.push(Building::new(item_id(&id), &project, ts, branch.clone()));
                            open.insert(branch, items.len() - 1);
                            items.len() - 1
                        }
                    };
                    let b = &mut items[i];
                    b.touch(ts, &session);
                    b.item.prompts += 1;
                    if b.item.title.is_none() {
                        b.item.title = title(&text);
                    }
                    current = Some(i);
                }
                Event::Tool {
                    session,
                    name,
                    summary,
                    status,
                    error,
                    commits,
                    prs,
                    branch,
                } => {
                    let i = match current {
                        Some(i) => i,
                        None => {
                            // Actions before any prompt (a log that starts
                            // mid-session): an untitled item.
                            let id = item_id(&format!("{project}:{ts}"));
                            items.push(Building::new(id, &project, ts, branch.clone()));
                            open.insert(branch.clone(), items.len() - 1);
                            current = Some(items.len() - 1);
                            items.len() - 1
                        }
                    };
                    let b = &mut items[i];
                    b.touch(ts, &session);
                    if b.item.branch.is_none() {
                        b.item.branch = branch.clone();
                    }
                    match status.as_deref() {
                        Some("error") => {
                            if let Some(text) = error.as_deref().and_then(meaningful_error) {
                                let fp = fingerprint(text);
                                match b.errors.iter_mut().find(|e| e.fingerprint == fp) {
                                    Some(e) => e.count += 1,
                                    None => b.errors.push(ErrorGroup {
                                        fingerprint: fp,
                                        tool: name.clone(),
                                        count: 1,
                                        sample: text.chars().take(200).collect(),
                                        // (text is the error's own line, see meaningful_error)
                                    }),
                                }
                            }
                        }
                        Some("ok") => {
                            if EDIT_TOOLS.contains(&name.as_str()) && !summary.is_empty() {
                                *b.files.entry(summary.clone()).or_default() += 1;
                                b.item.edited = true;
                                b.item.pending_edits = true;
                            }
                            for c in commits {
                                if b.item.commits.len() < MAX_COMMITS
                                    && !b.item.commits.iter().any(|x| x.hash == c.hash)
                                {
                                    b.item.commits.push(c);
                                }
                                b.item.committed = true;
                                b.item.pending_edits = false;
                                b.last_commit = Some(ts);
                            }
                            for pr in prs {
                                if !b.item.prs.contains(&pr) {
                                    b.item.prs.push(pr);
                                }
                                if !b.item.pending_edits {
                                    b.close("pr");
                                }
                            }
                            if name == "Bash" && is_push(&summary) {
                                let branch = b.item.branch.clone();
                                if b.item.committed {
                                    b.item.pushed = true;
                                    if !b.item.pending_edits && b.is_open() {
                                        b.close("push");
                                    }
                                }
                                // A push sends every earlier commit of the branch.
                                for other in items.iter_mut() {
                                    if other.item.committed && other.item.branch == branch {
                                        other.item.pushed = true;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Event::Message { session, tokens } => {
                    let Some(i) = current else { continue };
                    let b = &mut items[i];
                    b.touch(ts, &session);
                    let t = &mut b.item.tokens;
                    t.input += tokens.input;
                    t.cache_write += tokens.cache_write;
                    t.cache_read += tokens.cache_read;
                    t.output += tokens.output;
                }
            }
        }
        out.extend(items.into_iter().map(Building::finish));
    }
    out
}

/// What a work item's facts mean at `now`. The dashboard applies the same
/// rules (frontend journal.js), so a status never goes stale.
pub fn status(item: &serde_json::Value, now: DateTime<Utc>) -> &'static str {
    let b = |k: &str| item[k].as_bool().unwrap_or(false);
    let idle = item["last_activity"]
        .as_str()
        .and_then(epoch)
        .map_or(0, |t| now.timestamp() - t);
    match item["closed_by"].as_str() {
        Some("discarded") => return "discarded",
        Some("outside") | Some("pr") => return "done",
        _ => {}
    }
    if b("pushed") || !item["prs"].as_array().is_none_or(|p| p.is_empty()) {
        "done"
    } else if b("committed") && !b("pending_edits") {
        "committed"
    } else if b("edited") {
        // Code left uncommitted.
        if idle < PENDING_AFTER_SECS {
            "active"
        } else {
            "pending"
        }
    } else if idle < ABANDONED_AFTER_SECS {
        // A discussion: nothing to pick up, so it only fades out.
        "active"
    } else {
        "abandoned"
    }
}

/// Edited, not committed, and idle this long: pending.
pub const PENDING_AFTER_SECS: i64 = 24 * 3600;
/// Neither edited nor committed, and idle this long: abandoned.
pub const ABANDONED_AFTER_SECS: i64 = 3 * 24 * 3600;

fn is_push(summary: &str) -> bool {
    summary.split(['&', ';', '|', '\n']).any(|s| {
        let w: Vec<&str> = s.split_whitespace().collect();
        w.windows(2)
            .any(|p| (p[0] == "git" || p[0].ends_with("/git")) && p[1] == "push")
            || w.windows(4)
                .any(|p| p[0] == "git" && p[1] == "-C" && p[3] == "push")
    })
}

/// First meaningful line of a prompt, capped: tags such as
/// `<pasted_content …>` are skipped.
fn title(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !(l.starts_with('<') && l.ends_with('>')))?;
    Some(match line.char_indices().nth(MAX_TITLE) {
        None => line.to_string(),
        Some((cut, _)) => format!("{}…", &line[..cut]),
    })
}

/// Messages from the agent's harness, not from the project: permission
/// denials, "file changed since read", interruptions. Not engineering
/// problems, so not counted.
const HARNESS_ERRORS: &[&str] = &[
    "Permission for this",
    "Permission to use",
    "<tool_use_error>",
    "The user doesn't want to proceed",
    "[Request interrupted",
    "Claude requested permissions",
];

/// The part of a tool error that says what went wrong: `Exit code 1` lines
/// (every failing shell command starts with one) are skipped; harness
/// messages give None.
fn meaningful_error(text: &str) -> Option<&str> {
    let mut rest = text.trim_start();
    loop {
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        let l = line.trim();
        let exit = l
            .strip_prefix("Exit code ")
            .is_some_and(|n| n.trim().chars().all(|c| c.is_ascii_digit()));
        if l.is_empty() || exit {
            if after.is_empty() {
                return None;
            }
            rest = after.trim_start();
            continue;
        }
        break;
    }
    if HARNESS_ERRORS.iter().any(|p| rest.starts_with(p)) {
        return None;
    }
    error_line(rest)
}

/// The line that names the error: the first one that looks like one, or,
/// for a Python traceback, the last (`ModuleNotFoundError: …`). None when
/// no line does: a command that just exited non-zero (`grep` finding
/// nothing) is not a problem worth grouping.
fn error_line(text: &str) -> Option<&str> {
    use std::sync::OnceLock;
    static ERR: OnceLock<regex::Regex> = OnceLock::new();
    let re = ERR.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(error|errors|failed|failure|fatal|panicked|denied|not found|no such|cannot|can't|timeout|timed out|refused|invalid|undefined)\b|[A-Za-z]+(Error|Exception)\b\s*:",
        )
        .expect("valid regex")
    });
    let mut lines = text.lines().map(str::trim).filter(|l| re.is_match(l));
    if text.starts_with("Traceback") {
        lines.next_back()
    } else {
        lines.next()
    }
}

/// Errors that differ only in numbers, paths, hashes and times share a
/// fingerprint: `test_auth.py:42 failed in 3.1s` = `test_auth.py:57 failed in 2.8s`.
pub fn fingerprint(error: &str) -> String {
    let first = error
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut norm = String::new();
    for word in first.split_whitespace() {
        let w: String = if word.contains('/') {
            "<path>".into()
        } else if word.len() >= 7 && word.chars().all(|c| c.is_ascii_hexdigit()) {
            "<hex>".into()
        } else {
            word.chars()
                .map(|c| {
                    if c.is_ascii_digit() {
                        '0'
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect::<String>()
                .split('0')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("0")
        };
        if !norm.is_empty() {
            norm.push(' ');
        }
        norm.push_str(&w);
        if norm.len() > 200 {
            break;
        }
    }
    let digest = Sha256::digest(norm.as_bytes());
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn iso(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn epoch(ts: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts).ok().map(|t| t.timestamp())
}

impl UsageDb {
    /// Rebuild the work items from what is stored, check open ones against
    /// their repositories, and mark changed ones for upload. Items that no
    /// longer exist (the rules regrouped them) are uploaded once as
    /// `{"deleted": true}` so the server copy goes too. Returns how many
    /// items changed.
    pub fn build_work_items(&mut self, now: DateTime<Utc>) -> Result<usize> {
        self.commit()?;
        let projects = self.journal_events()?;
        let salt = self.salt().to_vec();
        let item_id = |prompt: &str| {
            let d = Sha256::new()
                .chain_update(&salt)
                .chain_update(b"\0work_item\0")
                .chain_update(prompt.as_bytes())
                .finalize();
            d[..16]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let mut items = build(projects, &item_id);
        for item in items.iter_mut() {
            check_repository(item, now.timestamp());
        }

        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let mut changed = 0;
        let mut seen = std::collections::HashSet::new();
        for item in &items {
            seen.insert(item.id.clone());
            let data = serde_json::to_string(item)?;
            changed += tx.execute(
                "INSERT INTO work_items (id, project_id, ts, data) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                     project_id = excluded.project_id, ts = excluded.ts,
                     data = excluded.data, synced_at = NULL
                 WHERE work_items.data IS NOT excluded.data",
                params![item.id, item.project_id, item.last_activity, data],
            )?;
        }
        let gone: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT id FROM work_items WHERE data != '{\"deleted\":true}'")?;
            let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
            ids.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|id| !seen.contains(id))
                .collect()
        };
        for id in gone {
            changed += tx.execute(
                "UPDATE work_items SET data = '{\"deleted\":true}', synced_at = NULL WHERE id = ?1",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// The stored work items (not deleted), newest activity first.
    pub fn work_items(&self) -> Result<Vec<(String, Option<String>, serde_json::Value)>> {
        let mut stmt = self.conn().prepare(
            "SELECT w.data, pp.name, w.id FROM work_items w
             LEFT JOIN private_projects pp ON pp.project_id = w.project_id
             WHERE w.data != '{\"deleted\":true}' ORDER BY w.ts DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(0)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, data) = row?;
            out.push((id, name, serde_json::from_str(&data)?));
        }
        Ok(out)
    }

    fn journal_events(&self) -> Result<BTreeMap<String, Vec<(i64, Event)>>> {
        let conn = self.conn();
        let mut projects: BTreeMap<String, Vec<(i64, Event, u8)>> = BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, session_id, ts, text, branch FROM private_prompts
             WHERE is_subagent = 0",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (id, project, session, ts, text, branch) = row?;
            let Some(ts) = epoch(&ts) else { continue };
            projects.entry(project).or_default().push((
                ts,
                Event::Prompt {
                    id,
                    session,
                    text,
                    branch,
                },
                0,
            ));
        }
        let mut stmt = conn.prepare(
            "SELECT m.project_id, m.session_id, p.ts, p.full_name, p.summary, t.status,
                    p.error, p.commits, p.prs, p.branch
             FROM private_tool_calls p
             JOIN tool_calls t ON t.id = p.id
             JOIN messages m ON m.id = t.message_id
             WHERE p.ts IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?;
        for row in rows {
            let (project, session, ts, name, summary, status, error, commits, prs, branch) = row?;
            let Some(ts) = epoch(&ts) else { continue };
            projects.entry(project).or_default().push((
                ts,
                Event::Tool {
                    session,
                    name: name.unwrap_or_default(),
                    summary: summary.unwrap_or_default(),
                    status,
                    error,
                    commits: commits
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                    prs: prs
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                    branch,
                },
                1,
            ));
        }
        let mut stmt = conn.prepare(
            "SELECT project_id, session_id, ts, input_tokens, cache_write_tokens,
                    cache_read_tokens, output_tokens FROM messages",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                Tokens {
                    input: r.get(3)?,
                    cache_write: r.get(4)?,
                    cache_read: r.get(5)?,
                    output: r.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (project, session, ts, tokens) = row?;
            let Some(ts) = epoch(&ts) else { continue };
            projects
                .entry(project)
                .or_default()
                .push((ts, Event::Message { session, tokens }, 2));
        }
        // Time order; at the same second, the prompt first, then its actions.
        Ok(projects
            .into_iter()
            .map(|(p, mut events)| {
                events.sort_by_key(|(ts, _, rank)| (*ts, *rank));
                (p, events.into_iter().map(|(ts, e, _)| (ts, e)).collect())
            })
            .collect())
    }
}

/// For an open item idle for a while: did the work end outside the agent?
/// New commits on its branch -> done outside; edited files clean and no new
/// commits -> discarded; commits already on a remote -> pushed.
fn check_repository(item: &mut WorkItem, now: i64) {
    let Some(last) = epoch(&item.last_activity) else {
        return;
    };
    if now - last < REPO_CHECK_AFTER_SECS {
        return;
    }
    let Some(repo) = item.files.iter().find_map(|f| repo_of(&f.path)) else {
        return;
    };
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if item.committed && !item.pushed {
        if let Some(last_commit) = item.commits.last() {
            let remote = git(&["branch", "-r", "--contains", &last_commit.hash]);
            if remote.is_some_and(|r| !r.is_empty()) {
                item.pushed = true;
            }
        }
    }
    if item.state != "open" || !item.pending_edits {
        return;
    }
    // Commits on any branch touching the edited files since: the work was
    // committed outside the agent, or carried to another branch.
    let since = format!("--since=@{last}");
    let known: Vec<&str> = item.commits.iter().map(|c| c.hash.as_str()).collect();
    let mut log_args = vec!["log", "--all", &since, "--format=%H", "--"];
    let paths: Vec<String> = item.files.iter().map(|f| f.path.clone()).collect();
    log_args.extend(paths.iter().map(String::as_str));
    if let Some(log) = git(&log_args) {
        if log.lines().any(|h| !h.is_empty() && !known.contains(&h)) {
            item.state = "closed";
            item.closed_by = Some("outside");
            item.pending_edits = false;
            item.committed = true;
            return;
        }
    }
    let mut args = vec!["status", "--porcelain", "--"];
    args.extend(paths.iter().map(String::as_str));
    if let Some(status) = git(&args) {
        if status.is_empty() {
            item.state = "closed";
            item.closed_by = Some("discarded");
            item.pending_edits = false;
        }
    }
}

/// The repository a file belongs to, if it still exists on this machine.
fn repo_of(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let mut dir = p.parent()?;
    while !dir.exists() {
        dir = dir.parent()?;
    }
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3600;
    const DAY: i64 = 24 * H;

    fn prompt(id: &str, branch: Option<&str>) -> Event {
        Event::Prompt {
            id: id.into(),
            session: "s1".into(),
            text: format!("{id} please\nmore"),
            branch: branch.map(Into::into),
        }
    }

    fn tool(name: &str, summary: &str, commits: &[&str]) -> Event {
        Event::Tool {
            session: "s1".into(),
            name: name.into(),
            summary: summary.into(),
            status: Some("ok".into()),
            error: None,
            commits: commits
                .iter()
                .map(|h| Commit {
                    hash: (*h).into(),
                    branch: None,
                    subject: format!("subject {h}"),
                    remote: None,
                    files: None,
                    insertions: None,
                    deletions: None,
                    changes: Vec::new(),
                    root: None,
                })
                .collect(),
            prs: Vec::new(),
            branch: None,
        }
    }

    fn items(events: Vec<(i64, Event)>) -> Vec<WorkItem> {
        let mut p = BTreeMap::new();
        p.insert("proj".to_string(), events);
        build(p, &|id| format!("w-{id}"))
    }

    #[test]
    fn uncommitted_work_continues_after_any_pause() {
        // Day 1: edits, no commit. Ill for five days. Day 7: carry on.
        let got = items(vec![
            (0, prompt("p1", Some("main"))),
            (60, tool("Edit", "/r/src/pay.rs", &[])),
            (6 * DAY, prompt("p2", Some("main"))),
            (
                6 * DAY + 60,
                tool("Bash", "git commit -qm x && git push", &["aaaaaaa"]),
            ),
        ]);
        assert_eq!(got.len(), 1);
        let w = &got[0];
        // A short opening prompt: the first commit names the item.
        assert_eq!(
            (w.title.as_deref(), w.prompts),
            (Some("subject aaaaaaa"), 2)
        );
        assert_eq!(
            (w.state, w.closed_by, w.pushed),
            ("closed", Some("push"), true)
        );
        assert_eq!(w.pauses.len(), 1);
        // The pause is not active time.
        assert_eq!(w.active_ms, 2 * 60 * 1000);
    }

    #[test]
    fn a_clean_commit_ends_the_item_and_a_later_push_marks_it() {
        let got = items(vec![
            (0, prompt("p1", Some("main"))),
            (60, tool("Edit", "/r/a.rs", &[])),
            (120, tool("Bash", "git commit -qm a", &["aaaaaaa"])),
            (600, prompt("p2", Some("main"))),
            (660, tool("Edit", "/r/b.rs", &[])),
            (
                720,
                tool("Bash", "git commit -qm b; git push", &["bbbbbbb"]),
            ),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].closed_by, got[0].pushed), (Some("commit"), true));
        assert_eq!((got[1].closed_by, got[1].pushed), (Some("push"), true));
        assert_eq!(got[1].files[0].path, "/r/b.rs");
    }

    #[test]
    fn discussions_end_after_two_idle_hours() {
        let got = items(vec![
            (0, prompt("q1", None)),
            (30 * 60, prompt("q2", None)),
            (4 * H, prompt("q3", None)),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].prompts, got[0].closed_by), (2, Some("idle")));
        assert_eq!((got[1].state, got[1].edited), ("open", false));
    }

    #[test]
    fn another_branch_is_another_item() {
        let got = items(vec![
            (0, prompt("p1", Some("feature/a"))),
            (60, tool("Edit", "/r/a.rs", &[])),
            (120, prompt("p2", Some("feature/b"))),
            (180, tool("Edit", "/r/b.rs", &[])),
            (DAY, prompt("p3", Some("feature/a"))),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].branch.as_deref(), got[0].prompts),
            (Some("feature/a"), 2)
        );
        assert_eq!(got[1].branch.as_deref(), Some("feature/b"));
        assert!(got.iter().all(|w| w.pending_edits && w.state == "open"));
    }

    #[test]
    fn pull_requests_and_repeated_errors() {
        let mut pr = tool("Bash", "gh pr create", &[]);
        if let Event::Tool { prs, .. } = &mut pr {
            prs.push("https://github.com/acme/pay/pull/42".into());
        }
        let err = |n: u32| Event::Tool {
            session: "s1".into(),
            name: "Bash".into(),
            summary: "cargo test".into(),
            status: Some("error".into()),
            error: Some(format!("test_auth.py:{n} failed in {n}.1s at /tmp/x{n}")),
            commits: Vec::new(),
            prs: Vec::new(),
            branch: None,
        };
        let got = items(vec![
            (0, prompt("p1", Some("fix/x"))),
            (10, err(42)),
            (20, err(57)),
            (30, err(99)),
            (40, tool("Bash", "git commit -qm fix", &["ccccccc"])),
            (50, pr),
        ]);
        assert_eq!(got[0].prs, vec!["https://github.com/acme/pay/pull/42"]);
        assert_eq!(got[0].closed_by, Some("pr"));
        assert_eq!(got[0].errors.len(), 1);
        assert_eq!(got[0].errors[0].count, 3);
    }

    #[test]
    fn status_depends_on_facts_and_time() {
        let now = DateTime::parse_from_rfc3339("2026-11-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let st = |v: serde_json::Value| status(&v, now);
        let at = |h: i64| iso(now.timestamp() - h * H);
        use serde_json::json;
        assert_eq!(
            st(json!({"edited": true, "pending_edits": true, "last_activity": at(2)})),
            "active"
        );
        assert_eq!(
            st(json!({"edited": true, "pending_edits": true, "last_activity": at(30)})),
            "pending"
        );
        assert_eq!(
            st(json!({"edited": false, "last_activity": at(30)})),
            "active"
        );
        assert_eq!(
            st(json!({"edited": false, "last_activity": at(80)})),
            "abandoned"
        );
        assert_eq!(
            st(json!({"committed": true, "last_activity": at(80)})),
            "committed"
        );
        assert_eq!(
            st(json!({"committed": true, "pushed": true, "last_activity": at(1)})),
            "done"
        );
        assert_eq!(st(json!({"prs": ["u"], "last_activity": at(1)})), "done");
        assert_eq!(
            st(json!({"closed_by": "outside", "last_activity": at(99)})),
            "done"
        );
        assert_eq!(
            st(json!({"closed_by": "discarded", "edited": true, "last_activity": at(99)})),
            "discarded"
        );
    }

    #[test]
    fn titles_skip_tags_and_short_replies() {
        assert_eq!(
            title("<pasted_content id=\"1\">\nPenilaian peluang bisnis\n</pasted_content>")
                .as_deref(),
            Some("Penilaian peluang bisnis")
        );
        let got = items(vec![
            (
                0,
                Event::Prompt {
                    id: "p".into(),
                    session: "s1".into(),
                    text: "ya lanjut".into(),
                    branch: None,
                },
            ),
            (60, tool("Edit", "/r/a.rs", &[])),
            (120, tool("Bash", "git commit -qm x", &["aaaaaaa"])),
        ]);
        assert_eq!(got[0].title.as_deref(), Some("subject aaaaaaa"));
    }

    #[test]
    fn detects_pushes() {
        assert!(is_push("cd x && git push -q origin HEAD"));
        assert!(is_push("Commit and push: git -C ../x push"));
        assert!(!is_push("git log --oneline; echo push"));
        assert_eq!(fingerprint("a:1 failed 2s"), fingerprint("a:77 failed 9s"));
        assert_eq!(
            meaningful_error("Exit code 1\n\nerror: no such column: input"),
            Some("error: no such column: input")
        );
        assert_ne!(
            fingerprint(meaningful_error("Exit code 1\nerror: no such column").unwrap()),
            fingerprint(meaningful_error("Exit code 1\nTimeoutError: page.fill").unwrap())
        );
        assert_eq!(meaningful_error("Exit code 2"), None);
        assert_eq!(meaningful_error("Exit code 1\ntotal 0\n./a.js"), None);
        assert_eq!(
            meaningful_error("Exit code 1\nTraceback (most recent call last):\n  File \"x\", line 2\n    import django\nModuleNotFoundError: No module named 'django'"),
            Some("ModuleNotFoundError: No module named 'django'")
        );
        assert_eq!(
            meaningful_error("node:internal/modules/run_main:123\n  triggerUncaughtException(\npage.fill: Timeout 30000ms exceeded."),
            Some("page.fill: Timeout 30000ms exceeded.")
        );
        assert_eq!(
            meaningful_error("Exit code 1\ntotal 0\nls: /x: No such file or directory"),
            Some("ls: /x: No such file or directory")
        );
        assert_eq!(
            meaningful_error("Permission for this command was denied"),
            None
        );
        assert_eq!(
            meaningful_error("<tool_use_error>File has been modified</tool_use_error>"),
            None
        );
        assert_ne!(fingerprint("a failed"), fingerprint("b failed"));
    }
}
