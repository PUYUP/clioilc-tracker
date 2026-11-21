//! Git commits made by agent tool calls, so the activity timeline can show
//! their hashes. Two sources, both local to this machine:
//!
//! 1. the tool output: `git commit` prints `[main 3c514b0] Subject`;
//! 2. when the output has no hash (`git commit -q`, scripts), the repository
//!    itself: commits whose committer time falls inside the tool call, in the
//!    folders the command ran git in.
//!
//! Commit hashes, subjects and remotes are stored as private data.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// At most this many commits are kept per tool call.
pub const MAX_COMMITS: usize = 20;
/// Longest stored commit subject.
const MAX_SUBJECT: usize = 300;

/// Git subcommands that create commits.
const COMMITTING: &[&str] = &["commit", "merge", "cherry-pick", "revert", "am"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// Full hash when the repository was found, else as printed (7+ chars).
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub subject: String,
    /// Normalized remote, e.g. `github.com/acme/api` (see [`normalize_remote`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// Commits announced in tool output: lines like `[main 3c514b0] Subject`,
/// `[main (root-commit) 3c514b0] Subject` or `[detached HEAD 3c514b0] Subject`.
pub fn parse_output(text: &str) -> Vec<Commit> {
    let mut out: Vec<Commit> = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some((head, subject)) = rest.split_once("] ") else {
            continue;
        };
        let Some((branch, hash)) = head.rsplit_once(' ') else {
            continue;
        };
        if !(7..=40).contains(&hash.len()) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let hash = hash.to_ascii_lowercase();
        let branch = branch.trim_end_matches(" (root-commit)").trim();
        if branch.is_empty() || branch.contains(char::is_whitespace) && branch != "detached HEAD" {
            continue;
        }
        if out.iter().any(|c| c.hash == hash) {
            continue;
        }
        out.push(Commit {
            hash,
            branch: (branch != "detached HEAD").then(|| branch.to_string()),
            subject: cap(subject.trim()),
            remote: None,
        });
        if out.len() == MAX_COMMITS {
            break;
        }
    }
    out
}

/// Folders a shell command creates commits in, following `cd <dir>` and
/// `git -C <dir>` from `cwd`. Empty when the command runs no committing git
/// subcommand. Heredoc bodies (commit messages) are skipped.
pub fn commit_dirs(command: &str, cwd: Option<&str>) -> Vec<String> {
    let mut dir: Option<PathBuf> = cwd.map(PathBuf::from);
    let mut out: Vec<String> = Vec::new();
    let mut heredoc: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &heredoc {
            if line.trim() == end {
                heredoc = None;
            }
            continue;
        }
        heredoc = heredoc_delimiter(line);
        for segment in split_segments(line) {
            let words = shell_words(segment);
            let words: Vec<&str> = words
                .iter()
                .map(String::as_str)
                .skip_while(|w| *w == "(" || *w == "{" || is_assignment(w))
                .collect();
            match words.first().copied() {
                Some("cd") => {
                    let target = words.get(1).copied().unwrap_or("~");
                    dir = resolve_dir(dir.as_deref(), target);
                }
                Some(w) if w == "git" || w.ends_with("/git") => {
                    if let Some(d) = git_target(&words[1..], dir.as_deref()) {
                        let d = d.to_string_lossy().into_owned();
                        if !out.contains(&d) {
                            out.push(d);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// For `git [-C dir] [-c k=v] <subcommand> ...`: the folder, when the
/// subcommand creates commits.
fn git_target(args: &[&str], dir: Option<&Path>) -> Option<PathBuf> {
    let mut dir = dir.map(Path::to_path_buf);
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match *a {
            "-C" => {
                dir = resolve_dir(dir.as_deref(), args.get(i + 1)?);
                i += 2;
            }
            "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
            a if a.starts_with('-') => i += 1,
            sub => return COMMITTING.contains(&sub).then_some(dir).flatten(),
        }
    }
    None
}

fn resolve_dir(base: Option<&Path>, target: &str) -> Option<PathBuf> {
    let target = target.trim_end_matches('/');
    let path = if target == "~" || target.starts_with("~/") {
        crate::home_dir()
            .ok()?
            .join(target.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        base?.join(target)
    };
    // Lexical cleanup of `.` and `..` (the folder may not exist any more).
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                clean.pop();
            }
            p => clean.push(p),
        }
    }
    Some(clean)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `<<EOF`, `<<'EOF'`, `<<-"EOF"` -> `EOF`.
fn heredoc_delimiter(line: &str) -> Option<String> {
    let i = line.find("<<")?;
    let rest = line[i + 2..].trim_start_matches('-').trim_start();
    let word: String = rest
        .trim_start_matches(['\'', '"'])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!word.is_empty()).then_some(word)
}

/// Split one line at `&&`, `||`, `;` and `|`, outside quotes.
fn split_segments(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i, mut quote) = (0, 0, None::<u8>);
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'\'' || b == b'"' => quote = Some(b),
            None if b == b';' || b == b'|' || b == b'&' => {
                out.push(&line[start..i]);
                while i + 1 < bytes.len() && matches!(bytes[i + 1], b'|' | b'&') {
                    i += 1;
                }
                start = i + 1;
            }
            None => {}
        }
        i += 1;
    }
    out.push(&line[start..]);
    out
}

/// Words of one simple command, with quotes removed; `(` and `{` that open
/// a subshell or group become their own words.
fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quote = None::<char>;
    let mut has = false;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None if (c == '(' || c == '{') && cur.is_empty() && !has => words.push(c.to_string()),
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn cap(subject: &str) -> String {
    match subject.char_indices().nth(MAX_SUBJECT) {
        None => subject.to_string(),
        Some((cut, _)) => format!("{}…", &subject[..cut]),
    }
}

// --- the repository ---------------------------------------------------------------

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The folder's git remote URL: `origin`, else the first one.
pub fn remote_url(dir: &str) -> Option<String> {
    let remotes = git(dir, &["remote"])?;
    let name = remotes
        .lines()
        .find(|r| *r == "origin")
        .or_else(|| remotes.lines().next())?;
    git(dir, &["remote", "get-url", name])
}

/// The commits a tool call made, completed from the repositories in `dirs`:
/// hashes printed by the tool become full hashes; with none printed, the
/// commits whose committer time is within `[from, to]` (unix seconds: the
/// tool call was sent, its result logged) are taken. No slack: the next tool
/// call often starts within a second, and the clock is this machine's. Commits of repositories that are gone stay
/// as printed.
pub fn resolve(printed: &[Commit], dirs: &[String], from: i64, to: i64) -> Vec<Commit> {
    let repos: Vec<(&String, Option<String>)> = dirs
        .iter()
        .filter(|d| git(d, &["rev-parse", "--git-dir"]).is_some())
        .map(|d| (d, remote_url(d).and_then(|r| normalize_remote(&r))))
        .collect();
    if !printed.is_empty() {
        return printed
            .iter()
            .map(|c| {
                for (dir, remote) in &repos {
                    let spec = format!("{}^{{commit}}", c.hash);
                    if let Some(full) = git(dir, &["rev-parse", "--verify", "--quiet", &spec]) {
                        return Commit {
                            hash: full,
                            remote: remote.clone(),
                            ..c.clone()
                        };
                    }
                }
                c.clone()
            })
            .collect();
    }
    let mut out: Vec<Commit> = Vec::new();
    for (dir, remote) in &repos {
        let since = format!("--since=@{}", from - 5);
        let Some(log) = git(
            dir,
            &[
                "log",
                "--all",
                "--reflog",
                &since,
                "--format=%H%x1f%ct%x1f%s",
            ],
        ) else {
            continue;
        };
        for line in log.lines() {
            let mut f = line.splitn(3, '\u{1f}');
            let (Some(hash), Some(ct), Some(subject)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let Ok(ct) = ct.parse::<i64>() else { continue };
            if ct < from || ct > to || out.iter().any(|c| c.hash == hash) {
                continue;
            }
            out.push(Commit {
                hash: hash.to_string(),
                branch: branch_of(dir, hash),
                subject: cap(subject),
                remote: remote.clone(),
            });
        }
    }
    // Oldest first, as they were made.
    out.reverse();
    out.truncate(MAX_COMMITS);
    out
}

/// The local branch a commit is on (`main~2` -> `main`), if any.
fn branch_of(dir: &str, hash: &str) -> Option<String> {
    let name = git(
        dir,
        &[
            "name-rev",
            "--name-only",
            "--no-undefined",
            "--refs=refs/heads/*",
            hash,
        ],
    )?;
    let name = name.split(['~', '^']).next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// A git remote in one comparable form, e.g. `git@github.com:Acme/API.git`
/// -> `github.com/acme/api`. Same rules as the backend's
/// apps/workspaces/git.py (see GIT_REMOTE_CASES in both test suites).
pub fn normalize_remote(raw: &str) -> Option<String> {
    let s = raw.trim();
    let (host, path) = if let Some(i) = s.find("://") {
        let scheme = &s[..i];
        let mut chars = scheme.chars();
        if !chars.next()?.is_ascii_alphabetic()
            || !chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
        {
            return None;
        }
        let (authority, path) = s[i + 3..].split_once('/')?;
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host_port.split(':').next()?, path)
    } else {
        // scp-like: [user@]host:path
        let (before, path) = s.split_once(':')?;
        let host = match before.rsplit_once('@') {
            Some((user, host)) if !user.is_empty() && !user.contains('/') => host,
            Some(_) => return None,
            None => before,
        };
        if host.contains('/') || path.starts_with('/') {
            return None;
        }
        (host, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    let out = format!("{host}/{path}").to_lowercase();
    let (h, rest) = out.split_once('/')?;
    let host_ok = !h.is_empty()
        && h.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-');
    let path_ok = rest
        .split('/')
        .all(|seg| !seg.is_empty() && !seg.chars().any(char::is_whitespace));
    (host_ok && path_ok).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_commit_lines_from_output() {
        let out = "warning: x\n[main 3c514b0] Fix login\n 2 files changed\n\
                   [main (root-commit) ABCDEF1] First\n[detached HEAD 1234567] Detached\n\
                   [not a commit] nope\n[main zzzzzzz] bad hash\n[main 3c514b0] Fix login";
        let got = parse_output(out);
        let short: Vec<_> = got
            .iter()
            .map(|c| (c.hash.as_str(), c.branch.as_deref(), c.subject.as_str()))
            .collect();
        assert_eq!(
            short,
            vec![
                ("3c514b0", Some("main"), "Fix login"),
                ("abcdef1", Some("main"), "First"),
                ("1234567", None, "Detached"),
            ]
        );
    }

    #[test]
    fn finds_the_folders_commits_are_made_in() {
        let cwd = Some("/w/clioilc");
        assert_eq!(commit_dirs("ls -la", cwd), Vec::<String>::new());
        assert_eq!(
            commit_dirs("git status && git log", cwd),
            Vec::<String>::new()
        );
        assert_eq!(commit_dirs("git commit -qm x", cwd), vec!["/w/clioilc"]);
        assert_eq!(
            commit_dirs("cd backend && git add -A && git commit -q -F - <<'EOF'\ncd nowhere\ngit commit body\nEOF\ncd ../tracker && git -c a=b commit -m y; git -C ../frontend merge dev", cwd),
            vec!["/w/clioilc/backend", "/w/clioilc/tracker", "/w/clioilc/frontend"]
        );
        assert_eq!(
            commit_dirs(
                "(cd /abs/repo && GIT_EDITOR=true git cherry-pick abc)",
                None
            ),
            vec!["/abs/repo"]
        );
        assert_eq!(
            commit_dirs("cd \"my repo\" && git revert HEAD", cwd),
            vec!["/w/clioilc/my repo"]
        );
        // Relative folders without a known cwd cannot be placed.
        assert_eq!(commit_dirs("git commit -m x", None), Vec::<String>::new());
        assert_eq!(
            commit_dirs("echo 'git commit' | cat", cwd),
            Vec::<String>::new()
        );
    }

    #[test]
    fn finds_commits_in_the_repository() {
        let dir = std::env::temp_dir().join(format!("clioilc-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().to_string();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&d)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["remote", "add", "origin", "git@github.com:Acme/Pay.git"]);
        let now = chrono::Utc::now().timestamp();
        run(&["commit", "-q", "--allow-empty", "-m", "First one"]);
        run(&["commit", "-q", "--allow-empty", "-m", "Second one"]);
        let head = git(&d, &["rev-parse", "HEAD"]).unwrap();

        // -q printed nothing: found by time, oldest first, with branch and remote.
        let later = chrono::Utc::now().timestamp();
        let found = resolve(&[], std::slice::from_ref(&d), now, later);
        let subjects: Vec<_> = found.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["First one", "Second one"]);
        assert_eq!(found[1].hash, head);
        assert_eq!(found[1].branch.as_deref(), Some("main"));
        assert_eq!(found[1].remote.as_deref(), Some("github.com/acme/pay"));
        // Outside the window: nothing.
        assert!(resolve(&[], std::slice::from_ref(&d), now - 600, now - 300).is_empty());

        // A printed short hash becomes the full one.
        let printed = parse_output(&format!("[main {}] Second one", &head[..7]));
        let full = resolve(&printed, std::slice::from_ref(&d), 0, 0);
        assert_eq!(full[0].hash, head);
        assert_eq!(full[0].remote.as_deref(), Some("github.com/acme/pay"));
        // Repository gone: kept as printed.
        let gone = resolve(&printed, &["/nonexistent/clioilc".into()], 0, 0);
        assert_eq!(gone, printed);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
