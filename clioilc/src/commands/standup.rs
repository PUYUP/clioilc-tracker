//! `clioilc standup`: a short report of the work items of the last day(s)
//! (done, committed, in progress, pending, what keeps failing), printed or
//! sent to a chat webhook (Slack, Discord, Microsoft Teams).
//!
//! It is built on this machine, which holds the data, so it also works for
//! end-to-end encrypted accounts. The webhook URL is a secret and stays in
//! `<data dir>/standup.json` (mode 0600); the server never sees it. With a
//! schedule, `clioilc watch` sends it once a day.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, Utc, Weekday};
use clioilc_core::journal;
use clioilc_core::{Store, UsageDb};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Lines per section before "+N more".
const PER_SECTION: usize = 8;
/// Pending work is listed when it was touched within this many days.
const PENDING_DAYS: i64 = 7;
/// Discord rejects longer messages.
const DISCORD_MAX: usize = 2000;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub webhook: Option<String>,
    /// Local time to send at, "HH:MM", when `clioilc watch` runs.
    pub at: Option<String>,
    #[serde(default)]
    pub weekends: bool,
    /// The last local day a scheduled report was sent.
    pub last_sent: Option<NaiveDate>,
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("standup.json")
}

pub fn load(data_dir: &Path) -> Result<Config> {
    let path = config_path(data_dir);
    match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomically, mode 0600 (the webhook URL is a credential).
fn save(data_dir: &Path, config: &Config) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = config_path(data_dir);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(config)?)?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

// --- commands -----------------------------------------------------------------------

/// `clioilc standup [--days N] [--send]`
pub fn run(store: &Store, days: i64, send: bool) -> Result<()> {
    let report = build(store, days, Utc::now())?;
    if !send {
        print!("{}", render(&report, Style::Plain));
        return Ok(());
    }
    let config = load(store.data_dir())?;
    let url = config
        .webhook
        .context("no webhook; set one with `clioilc standup webhook <url>`")?;
    post(&url, &report)?;
    eprintln!("sent to {}", Kind::of(&url).name());
    Ok(())
}

/// `clioilc standup webhook [<url> | --remove]`
pub fn webhook(store: &Store, url: Option<String>, remove: bool) -> Result<()> {
    let mut config = load(store.data_dir())?;
    if remove {
        config.webhook = None;
        save(store.data_dir(), &config)?;
        eprintln!("webhook removed");
        return Ok(());
    }
    let Some(url) = url else {
        match &config.webhook {
            Some(u) => println!("{} webhook: {}", Kind::of(u).name(), redact_url(u)),
            None => println!("no webhook set"),
        }
        return Ok(());
    };
    check_url(&url)?;
    let kind = Kind::of(&url);
    config.webhook = Some(url);
    save(store.data_dir(), &config)?;
    eprintln!(
        "{} webhook saved in {} (only on this machine).",
        kind.name(),
        config_path(store.data_dir()).display()
    );
    eprintln!(
        "The report holds work item titles (the first line of your prompts) and commit counts."
    );
    eprintln!("Try it with `clioilc standup --send`.");
    Ok(())
}

/// `clioilc standup schedule [HH:MM | --off] [--weekends]`
pub fn schedule(store: &Store, at: Option<String>, off: bool, weekends: bool) -> Result<()> {
    let mut config = load(store.data_dir())?;
    if off {
        config.at = None;
        save(store.data_dir(), &config)?;
        eprintln!("schedule off");
        return Ok(());
    }
    let Some(at) = at else {
        match &config.at {
            Some(t) => println!(
                "every {} at {t}{}",
                if config.weekends { "day" } else { "weekday" },
                config
                    .last_sent
                    .map(|d| format!(" (last sent {d})"))
                    .unwrap_or_default()
            ),
            None => println!("no schedule"),
        }
        return Ok(());
    };
    let time = parse_time(&at)?;
    if config.webhook.is_none() {
        bail!("set a webhook first: `clioilc standup webhook <url>`");
    }
    config.at = Some(time.format("%H:%M").to_string());
    config.weekends = weekends;
    // Do not send for today right away if that time has already passed.
    if Local::now().time() >= time {
        config.last_sent = Some(Local::now().date_naive());
    }
    save(store.data_dir(), &config)?;
    eprintln!(
        "standup every {} at {}, sent by `clioilc watch` (or the background service) while it runs",
        if weekends { "day" } else { "weekday" },
        time.format("%H:%M")
    );
    Ok(())
}

/// The scheduled standup, checked once a minute by `clioilc watch`.
pub fn spawn_schedule(store: Store, stop: &'static AtomicBool) {
    std::thread::spawn(move || {
        let mut last_error: Option<String> = None;
        while !stop.load(Ordering::SeqCst) {
            match tick(&store, Local::now()) {
                Ok(true) => {
                    eprintln!("standup: sent");
                    last_error = None;
                }
                Ok(false) => {}
                Err(err) => {
                    let msg = format!("{err:#}");
                    if last_error.as_deref() != Some(&msg) {
                        eprintln!("standup: {msg}");
                        last_error = Some(msg);
                    }
                }
            }
            for _ in 0..60 {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    });
}

/// Sends today's report if it is due. Ok(true) when sent.
fn tick(store: &Store, now: DateTime<Local>) -> Result<bool> {
    let mut config = load(store.data_dir())?;
    if !due(&config, now)? {
        return Ok(false);
    }
    let url = config
        .webhook
        .clone()
        .context("schedule set but no webhook")?;
    // Monday's report covers the weekend too (weekdays only).
    let days = if !config.weekends && now.weekday() == Weekday::Mon {
        3
    } else {
        1
    };
    let report = build(store, days, now.with_timezone(&Utc))?;
    // Marked first: a failing webhook is reported, not retried every minute.
    config.last_sent = Some(now.date_naive());
    save(store.data_dir(), &config)?;
    post(&url, &report)?;
    Ok(true)
}

fn due(config: &Config, now: DateTime<Local>) -> Result<bool> {
    let Some(at) = config.at.as_deref() else {
        return Ok(false);
    };
    let today = now.date_naive();
    let weekend = matches!(now.weekday(), Weekday::Sat | Weekday::Sun);
    Ok(now.time() >= parse_time(at)?
        && config.last_sent != Some(today)
        && (config.weekends || !weekend))
}

fn parse_time(s: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M").with_context(|| format!("`{s}` is not a time like 09:00"))
}

// --- the report -------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub day: String,
    pub days: i64,
    pub done: Vec<String>,
    pub committed: Vec<String>,
    pub active: Vec<String>,
    pub pending: Vec<String>,
    pub problems: Vec<String>,
}

impl Report {
    fn is_empty(&self) -> bool {
        self.done.is_empty()
            && self.committed.is_empty()
            && self.active.is_empty()
            && self.pending.is_empty()
    }
}

fn build(store: &Store, days: i64, now: DateTime<Utc>) -> Result<Report> {
    let mut db = UsageDb::open(&store.usage_db_path())?;
    db.resolve_commits()?;
    db.build_work_items(now)?;
    let items: Vec<(Option<String>, Value)> = db
        .work_items()?
        .into_iter()
        .map(|(_, p, v)| (p, v))
        .collect();
    Ok(report(&items, now, days.max(1)))
}

/// The report of `items` (project folder name, work item) at `now`.
pub fn report(items: &[(Option<String>, Value)], now: DateTime<Utc>, days: i64) -> Report {
    let since = now - Duration::days(days);
    let pending_since = now - Duration::days(PENDING_DAYS);
    let label = labeler(items.iter().filter_map(|(p, _)| p.as_deref()));
    let mut r = Report {
        day: now.with_timezone(&Local).format("%a, %b %-d").to_string(),
        days,
        ..Report::default()
    };
    let mut problems: Vec<(u64, String)> = Vec::new();
    for (project, item) in items {
        let Some(last) = item["last_activity"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        let status = journal::status(item, now);
        let recent = last >= since;
        if !(recent || (status == "pending" && last >= pending_since)) {
            continue;
        }
        let line = line(
            &label(project.as_deref()),
            item,
            status,
            last.with_timezone(&Utc),
            now,
        );
        match status {
            "done" => r.done.push(line),
            "committed" => r.committed.push(line),
            "active" => r.active.push(line),
            "pending" => r.pending.push(line),
            _ => continue,
        }
        for e in item["errors"].as_array().into_iter().flatten() {
            let count = e["count"].as_u64().unwrap_or(0);
            if count >= 3 {
                let tool = e["tool"].as_str().unwrap_or("tool");
                let sample = e["sample"]
                    .as_str()
                    .map(|s| format!(": {}", one_line(s, 100)))
                    .unwrap_or_default();
                problems.push((count, format!("{count}× {tool}{sample}")));
            }
        }
    }
    problems.sort_by_key(|p| std::cmp::Reverse(p.0));
    r.problems = problems.into_iter().map(|(_, p)| p).take(3).collect();
    r
}

fn line(
    project: &str,
    item: &Value,
    status: &str,
    last: DateTime<Utc>,
    now: DateTime<Utc>,
) -> String {
    let title = item["title"]
        .as_str()
        .map_or_else(|| "untitled work".to_string(), |t| one_line(t, 90));
    let mut facts = Vec::new();
    let commits = item["commits"].as_array().map_or(0, Vec::len);
    if commits > 0 {
        facts.push(format!(
            "{commits} commit{}",
            if commits == 1 { "" } else { "s" }
        ));
    }
    for pr in item["prs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        facts.push(pr_label(pr));
    }
    if status == "pending" {
        facts.push(format!("idle {}", idle(now - last)));
    }
    let facts = if facts.is_empty() {
        String::new()
    } else {
        format!(" ({})", facts.join(", "))
    };
    format!("{project}: {title}{facts}")
}

/// "PR #42" for a pull request URL.
fn pr_label(url: &str) -> String {
    url.rsplit('/')
        .next()
        .map_or_else(|| "PR".into(), |n| format!("PR #{n}"))
}

fn idle(d: Duration) -> String {
    let h = d.num_hours();
    if h < 48 {
        format!("{h}h")
    } else {
        format!("{}d", h / 24)
    }
}

/// Claude project folders ("-Users-me-src-api") to short names: the most
/// common leading segments are dropped, as on the dashboard.
fn labeler<'a>(names: impl Iterator<Item = &'a str>) -> impl Fn(Option<&str>) -> String {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.to_string()) {
            continue;
        }
        let parts: Vec<&str> = name.trim_start_matches('-').split('-').collect();
        for len in 1..parts.len() {
            *counts.entry(parts[..len].join("-")).or_default() += 1;
        }
    }
    let mut best = String::new();
    let mut best_count = 1;
    let mut keys: Vec<_> = counts.into_iter().collect();
    keys.sort();
    for (key, n) in keys {
        if n > best_count || (n == best_count && n > 1 && key.len() > best.len()) {
            best = key;
            best_count = n;
        }
    }
    move |name| {
        let Some(name) = name else {
            return "unknown project".into();
        };
        let clean = name.trim_start_matches('-');
        let short = match clean.strip_prefix(&format!("{best}-")) {
            Some(rest) if !best.is_empty() => rest,
            _ => clean,
        };
        one_line(short, 40)
    }
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        None => flat,
        Some((cut, _)) => format!("{}…", &flat[..cut]),
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Style {
    Plain,
    /// Slack mrkdwn: *bold*.
    Slack,
    /// Discord and Teams Markdown: **bold**.
    Markdown,
}

pub fn render(r: &Report, style: Style) -> String {
    let bold = |s: &str| match style {
        Style::Plain => s.to_string(),
        Style::Slack => format!("*{s}*"),
        Style::Markdown => format!("**{s}**"),
    };
    let period = if r.days == 1 {
        "last 24 hours".to_string()
    } else {
        format!("last {} days", r.days)
    };
    let mut out = format!("{} ({period})\n", bold(&format!("Standup, {}", r.day)));
    if r.is_empty() {
        out.push_str("No agent work recorded.\n");
    }
    let sections: [(&str, &Vec<String>); 4] = [
        ("Done", &r.done),
        ("Committed, not pushed", &r.committed),
        ("In progress", &r.active),
        ("Pending (changes not committed)", &r.pending),
    ];
    for (title, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&bold(title));
        out.push('\n');
        for l in lines.iter().take(PER_SECTION) {
            out.push_str("• ");
            out.push_str(l);
            out.push('\n');
        }
        if lines.len() > PER_SECTION {
            out.push_str(&format!("• +{} more\n", lines.len() - PER_SECTION));
        }
    }
    if !r.problems.is_empty() {
        out.push('\n');
        out.push_str(&bold("Keeps failing"));
        out.push('\n');
        for p in &r.problems {
            out.push_str("• ");
            out.push_str(p);
            out.push('\n');
        }
    }
    out.push_str(match style {
        Style::Plain => "",
        Style::Slack => "\n_via clioilc_\n",
        Style::Markdown => "\n*via clioilc*\n",
    });
    out
}

// --- webhooks ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Slack,
    Discord,
    Teams,
    /// Anything else: `{"text": …}`, which most chat webhooks accept.
    Generic,
}

impl Kind {
    fn of(url: &str) -> Self {
        let host = host(url);
        if host == "hooks.slack.com" {
            Kind::Slack
        } else if (host == "discord.com" || host == "discordapp.com")
            && url.contains("/api/webhooks/")
        {
            Kind::Discord
        } else if host.ends_with(".webhook.office.com")
            || host.ends_with(".logic.azure.com")
            || host.ends_with(".powerplatform.com")
        {
            Kind::Teams
        } else {
            Kind::Generic
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Slack => "Slack",
            Kind::Discord => "Discord",
            Kind::Teams => "Microsoft Teams",
            Kind::Generic => "generic",
        }
    }

    fn payload(self, r: &Report) -> Value {
        match self {
            Kind::Slack => json!({ "text": render(r, Style::Slack) }),
            Kind::Discord => {
                let text = render(r, Style::Markdown);
                let text = match text.char_indices().nth(DISCORD_MAX - 1) {
                    Some((cut, _)) => format!("{}…", &text[..cut]),
                    None => text,
                };
                json!({ "content": text, "allowed_mentions": { "parse": [] } })
            }
            // An Adaptive Card: accepted by both Teams Workflows and the
            // older Office 365 connectors.
            Kind::Teams => json!({
                "type": "message",
                "attachments": [{
                    "contentType": "application/vnd.microsoft.card.adaptive",
                    "content": {
                        "type": "AdaptiveCard",
                        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                        "version": "1.4",
                        "body": [{ "type": "TextBlock", "wrap": true, "text": render(r, Style::Markdown) }]
                    }
                }]
            }),
            Kind::Generic => json!({ "text": render(r, Style::Plain) }),
        }
    }
}

fn host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    host.split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn check_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") || host(url).is_empty() {
        bail!("the webhook must be an https:// URL");
    }
    Ok(())
}

/// Scheme and host only: the path of a webhook URL is its secret.
fn redact_url(url: &str) -> String {
    format!("https://{}/…", host(url))
}

fn post(url: &str, report: &Report) -> Result<()> {
    check_url(url)?;
    let kind = Kind::of(url);
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .user_agent(concat!("clioilc/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let resp = agent
        .post(url)
        .send_json(kind.payload(report))
        .with_context(|| format!("sending to the {} webhook {}", kind.name(), redact_url(url)))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let body = resp.into_body().read_to_string().unwrap_or_default();
        bail!(
            "the {} webhook {} answered {status}: {}",
            kind.name(),
            redact_url(url),
            one_line(&body, 200)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 24, 2, 0, 0).unwrap()
    }

    fn ago(h: i64) -> String {
        (now() - Duration::hours(h)).to_rfc3339()
    }

    fn item(title: &str, last_h: i64, extra: Value) -> Value {
        let mut v = json!({
            "title": title, "state": "open", "started": ago(last_h + 1), "last_activity": ago(last_h),
            "commits": [], "prs": [], "errors": [],
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    fn sample() -> Vec<(Option<String>, Value)> {
        let api = Some("-Users-me-src-api".to_string());
        let web = Some("-Users-me-src-web".to_string());
        vec![
            (
                api.clone(),
                item(
                    "Retry payments",
                    3,
                    json!({"pushed": true, "committed": true, "commits": [{}, {}], "prs": ["https://github.com/a/b/pull/42"]}),
                ),
            ),
            (
                web.clone(),
                item("Login page", 2, json!({"committed": true})),
            ),
            (
                api.clone(),
                item(
                    "Fix flaky test",
                    1,
                    json!({"edited": true, "pending_edits": true,
                "errors": [{"tool": "Bash", "count": 4, "sample": "cargo test\nfailed"}]}),
                ),
            ),
            (
                web.clone(),
                item(
                    "Old idea",
                    60,
                    json!({"edited": true, "pending_edits": true}),
                ),
            ),
            (
                api.clone(),
                item(
                    "Ancient",
                    24 * 10,
                    json!({"edited": true, "pending_edits": true}),
                ),
            ),
            (
                web,
                item("Too old to be done today", 30, json!({"pushed": true})),
            ),
            (
                None,
                item("Discarded", 2, json!({"closed_by": "discarded"})),
            ),
        ]
    }

    #[test]
    fn sorts_items_by_outcome() {
        let r = report(&sample(), now(), 1);
        assert_eq!(r.done, ["api: Retry payments (2 commits, PR #42)"]);
        assert_eq!(r.committed, ["web: Login page"]);
        assert_eq!(r.active, ["api: Fix flaky test"]);
        // Pending work older than the window is listed for a week, not longer.
        assert_eq!(r.pending, ["web: Old idea (idle 2d)"]);
        assert_eq!(r.problems, ["4× Bash: cargo test failed"]);
    }

    #[test]
    fn a_longer_window_reaches_further_back() {
        let r = report(&sample(), now(), 3);
        assert_eq!(r.done.len(), 2);
    }

    #[test]
    fn renders_for_each_chat() {
        let r = report(&sample(), now(), 1);
        let slack = render(&r, Style::Slack);
        assert!(slack.contains("*Done*\n• api: Retry payments"));
        assert!(slack.ends_with("_via clioilc_\n"));
        assert!(render(&r, Style::Markdown).contains("**Keeps failing**"));
        let plain = render(&r, Style::Plain);
        assert!(!plain.contains('*'));
        let empty = render(&report(&[], now(), 1), Style::Plain);
        assert!(empty.contains("No agent work recorded."));
    }

    #[test]
    fn picks_the_payload_from_the_url() {
        assert_eq!(
            Kind::of("https://hooks.slack.com/services/T/B/x"),
            Kind::Slack
        );
        assert_eq!(
            Kind::of("https://discord.com/api/webhooks/1/abc"),
            Kind::Discord
        );
        assert_eq!(
            Kind::of("https://acme.webhook.office.com/webhookb2/x"),
            Kind::Teams
        );
        assert_eq!(
            Kind::of("https://prod-1.westus.logic.azure.com:443/workflows/x"),
            Kind::Teams
        );
        assert_eq!(Kind::of("https://evil.com/hooks.slack.com"), Kind::Generic);
        assert_eq!(
            Kind::of("https://hooks.slack.com@evil.com/x"),
            Kind::Generic
        );
        let r = report(&sample(), now(), 1);
        assert!(Kind::Slack.payload(&r)["text"]
            .as_str()
            .unwrap()
            .contains("*Done*"));
        let teams = Kind::Teams.payload(&r);
        assert_eq!(teams["attachments"][0]["content"]["type"], "AdaptiveCard");
        let discord = Kind::Discord.payload(&r);
        assert_eq!(discord["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn only_https_and_the_path_stays_secret() {
        assert!(check_url("http://hooks.slack.com/x").is_err());
        assert!(check_url("https://hooks.slack.com/services/T/B/secret").is_ok());
        assert_eq!(
            redact_url("https://hooks.slack.com/services/T/B/secret"),
            "https://hooks.slack.com/…"
        );
    }

    #[test]
    fn schedule_is_due_once_a_day_on_weekdays() {
        let cfg = Config {
            webhook: Some("https://hooks.slack.com/x".into()),
            at: Some("09:00".into()),
            ..Config::default()
        };
        let tue_8 = Local.with_ymd_and_hms(2026, 11, 24, 8, 59, 0).unwrap();
        let tue_9 = Local.with_ymd_and_hms(2026, 11, 24, 9, 0, 0).unwrap();
        let sat_9 = Local.with_ymd_and_hms(2026, 11, 28, 9, 30, 0).unwrap();
        assert!(!due(&cfg, tue_8).unwrap());
        assert!(due(&cfg, tue_9).unwrap());
        let sent = Config {
            last_sent: Some(tue_9.date_naive()),
            ..cfg.clone()
        };
        assert!(!due(&sent, tue_9).unwrap());
        assert!(!due(&cfg, sat_9).unwrap());
        assert!(due(
            &Config {
                weekends: true,
                ..cfg.clone()
            },
            sat_9
        )
        .unwrap());
        assert!(!due(&Config { at: None, ..cfg }, tue_9).unwrap());
    }

    #[test]
    fn config_is_private() {
        let dir = std::env::temp_dir().join(format!("clioilc-standup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cfg = Config {
            webhook: Some("https://hooks.slack.com/x".into()),
            ..Config::default()
        };
        save(&dir, &cfg).unwrap();
        assert_eq!(load(&dir).unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
