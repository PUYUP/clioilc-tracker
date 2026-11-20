use anyhow::{bail, Context, Result};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use clioilc_core::usage_db::{ActivityItem, ActivityKind};
use clioilc_core::{Store, UsageDb};
use std::io::{self, Write};

pub struct Options {
    /// Local calendar day (default: today).
    pub date: Option<NaiveDate>,
    /// Only projects whose name contains this.
    pub project: Option<String>,
    pub thinking: bool,
    /// Print full text instead of one line per item.
    pub full: bool,
}

/// Width of a one-line entry before it is cut.
const LINE: usize = 160;

pub fn run(store: &Store, opts: Options) -> Result<()> {
    let day = opts.date.unwrap_or_else(|| Local::now().date_naive());
    let start = local_midnight(day)?;
    let end = local_midnight(day.succ_opt().context("date out of range")?)?;

    let db = UsageDb::open(&store.usage_db_path())?;
    let items: Vec<ActivityItem> = db
        .activity(start, end)?
        .into_iter()
        .filter(|i| opts.thinking || i.kind != ActivityKind::Thinking)
        .filter(|i| {
            opts.project
                .as_deref()
                .is_none_or(|p| i.project.as_deref().is_some_and(|name| name.contains(p)))
        })
        .collect();

    super::ignore_closed_stdout(print_items(&mut io::stdout().lock(), day, &items, &opts))
}

fn print_items(
    out: &mut impl Write,
    day: NaiveDate,
    items: &[ActivityItem],
    opts: &Options,
) -> io::Result<()> {
    writeln!(out, "Activity on {day}")?;
    if items.is_empty() {
        writeln!(
            out,
            "  (nothing recorded; run `clioilc usage import` or keep `clioilc watch` running)"
        )?;
        return Ok(());
    }

    let mut current: Option<(&str, &str)> = None;
    for item in items {
        let group = (
            item.project.as_deref().unwrap_or("-"),
            item.session_id.as_str(),
        );
        if current != Some(group) {
            writeln!(out)?;
            writeln!(
                out,
                "== {}  (session {})",
                group.0,
                &group.1[..8.min(group.1.len())]
            )?;
            current = Some(group);
        }
        let time = item.ts.with_timezone(&Local).format("%H:%M:%S");
        let sub = if item.is_subagent { " [subagent]" } else { "" };
        let (tag, text) = match &item.kind {
            ActivityKind::Prompt => ("you   ".to_string(), item.text.clone()),
            ActivityKind::Text => ("agent ".to_string(), item.text.clone()),
            ActivityKind::Thinking => ("think ".to_string(), item.text.clone()),
            ActivityKind::Tool {
                name,
                status,
                error,
            } => {
                let mark = match status.as_deref() {
                    Some("ok") => "ok",
                    Some("error") => "ERROR",
                    _ => "...",
                };
                let mut text = format!("{name} [{mark}] {}", item.text);
                if let Some(err) = error {
                    text.push_str(" -> ");
                    text.push_str(err);
                }
                ("tool  ".to_string(), text)
            }
        };
        if opts.full {
            writeln!(out, "{time}  {tag}{sub} {}", indent(&text))?;
        } else {
            writeln!(out, "{time}  {tag}{sub} {}", one_line(&text, LINE))?;
        }
    }
    Ok(())
}

fn local_midnight(day: NaiveDate) -> Result<chrono::DateTime<Utc>> {
    let naive = day.and_hms_opt(0, 0, 0).context("invalid date")?;
    match Local.from_local_datetime(&naive).earliest() {
        Some(t) => Ok(t.with_timezone(&Utc)),
        None => bail!("{day} has no local midnight"),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        None => flat,
        Some((cut, _)) => format!("{}…", &flat[..cut]),
    }
}

fn indent(text: &str) -> String {
    text.trim().replace('\n', "\n                 ")
}
