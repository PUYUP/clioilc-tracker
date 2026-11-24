//! `clioilc journal`: the work items of the last days (backend docs/journal.md).

use anyhow::Result;
use chrono::{Duration, Local, Utc};
use clioilc_core::journal;
use clioilc_core::{Store, UsageDb};
use std::io::{self, Write};

pub fn run(store: &Store, days: i64) -> Result<()> {
    super::ignore_closed_stdout(print(store, days, &mut io::stdout().lock()).map_err(|e| {
        match e.downcast::<io::Error>() {
            Ok(io) => io,
            Err(other) => io::Error::other(format!("{other:#}")),
        }
    }))
}

fn print(store: &Store, days: i64, out: &mut impl Write) -> Result<()> {
    let mut db = UsageDb::open(&store.usage_db_path())?;
    db.resolve_commits()?;
    let now = Utc::now();
    db.build_work_items(now)?;
    let since = now - Duration::days(days.max(1));
    let mut shown = 0;
    // Projects by their latest item, items newest first.
    let mut groups: Vec<(String, Vec<serde_json::Value>)> = Vec::new();
    for (_, project, item) in db.work_items()? {
        let project = project.unwrap_or_else(|| "?".into());
        match groups.iter_mut().find(|(p, _)| *p == project) {
            Some((_, items)) => items.push(item),
            None => groups.push((project, vec![item])),
        }
    }
    for (project, items) in groups {
        let mut header = false;
        for item in items {
            let last = item["last_activity"].as_str().unwrap_or_default();
            let Ok(last_ts) = chrono::DateTime::parse_from_rfc3339(last) else {
                continue;
            };
            if last_ts < since {
                continue;
            }
            if !header {
                writeln!(out, "\n== {project}")?;
                header = true;
            }
            let status = journal::status(&item, now);
            let title = item["title"].as_str().unwrap_or("(untitled)");
            let branch = item["branch"]
                .as_str()
                .map(|b| format!(" [{b}]"))
                .unwrap_or_default();
            writeln!(
                out,
                "{:<10} {}  {}{branch}",
                status,
                last_ts.with_timezone(&Local).format("%m-%d %H:%M"),
                one_line(title, 90)
            )?;
            let commits = item["commits"].as_array().map_or(0, Vec::len);
            let files = item["files"].as_array().map_or(0, Vec::len);
            let prompts = item["prompts"].as_u64().unwrap_or(0);
            let mins = item["active_ms"].as_i64().unwrap_or(0) / 60_000;
            let mut facts = vec![format!("{prompts} prompt(s)"), format!("{mins} min active")];
            if files > 0 {
                facts.push(format!("{files} file(s)"));
            }
            if commits > 0 {
                facts.push(format!("{commits} commit(s)"));
            }
            for pr in item["prs"].as_array().into_iter().flatten() {
                facts.push(pr.as_str().unwrap_or_default().to_string());
            }
            if let Some(errs) = item["errors"].as_array().filter(|e| !e.is_empty()) {
                let repeated = errs
                    .iter()
                    .filter(|e| e["count"].as_u64() > Some(2))
                    .count();
                if repeated > 0 {
                    facts.push(format!("{repeated} repeated error(s)"));
                }
            }
            if let Some(p) = item["pauses"].as_array().filter(|p| !p.is_empty()) {
                facts.push(format!("{} pause(s)", p.len()));
            }
            writeln!(out, "           {}", facts.join(" · "))?;
            shown += 1;
        }
    }
    if shown == 0 {
        writeln!(
            out,
            "no work items in the last {days} day(s); run `clioilc usage import` first"
        )?;
    }
    Ok(())
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        None => flat,
        Some((cut, _)) => format!("{}…", &flat[..cut]),
    }
}
