//! `clioilc link`, `links`, `unlink`: share a folder's agent data with a
//! team project (backend docs/workspaces.md). Only the folder's hash goes
//! to the server, never its name; the link lives on the server.

use super::login::confirm;
use crate::cloud::auth;
use crate::cloud::client::{api_error_of, Client};
use crate::cloud::{e2e, e2e_v2 as v2};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clioilc_core::{Store, UsageDb};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

pub struct Options {
    /// `workspace/project`; None = find it from the folder's git remote.
    pub target: Option<String>,
    pub path: Option<PathBuf>,
    /// Some(true) = --history, Some(false) = --no-history, None = ask.
    pub history: Option<bool>,
    /// Also link known subfolder projects without asking.
    pub subfolders: bool,
}

#[derive(Deserialize)]
struct Linkable {
    workspace: String,
    project: String,
    name: String,
    git_remote: String,
    #[serde(default)]
    enc_mode: String,
    #[serde(default)]
    project_id: u64,
    /// E2E: the remote sealed with the project key (decrypted here, never on the server).
    #[serde(default)]
    remote_ct: Option<String>,
    #[serde(default)]
    key_version: u64,
}

#[derive(Deserialize)]
struct Link {
    workspace: String,
    project: String,
    project_key: String,
    share_from: Option<String>,
}

/// Claude Code names a project after its folder, with every character
/// that is not an ASCII letter or digit replaced by `-`.
pub fn claude_project_name(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn folder(path: Option<PathBuf>) -> Result<(PathBuf, String)> {
    let dir = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let dir = dir
        .canonicalize()
        .with_context(|| format!("{} is not a folder", dir.display()))?;
    let name = claude_project_name(&dir);
    Ok((dir, name))
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

/// The folder's git remote: `origin`, else the first one.
fn folder_remote(dir: &Path) -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let remotes = git(&["remote"])?;
    let name = remotes
        .lines()
        .find(|r| *r == "origin")
        .or_else(|| remotes.lines().next())?;
    git(&["remote", "get-url", name])
}

/// `clioilc link` without a target: the one linkable project whose git
/// remote matches the folder's. Matching happens here; the folder's remote
/// is never sent to the server.
/// E2E workspaces seal their projects' remotes: open them with the project
/// keys this machine can verify. Returns why some stayed sealed.
fn open_e2e_remotes(
    projects: &mut [Linkable],
    store: &Store,
    client: &Client,
) -> Result<Vec<String>> {
    let sealed: std::collections::BTreeSet<String> = projects
        .iter()
        .filter(|p| p.enc_mode == "e2e" && p.remote_ct.is_some())
        .map(|p| p.workspace.clone())
        .collect();
    if sealed.is_empty() {
        return Ok(Vec::new());
    }
    let auth = auth::require(store.data_dir())?;
    let Some(dek) = auth.data_key.as_deref() else {
        return Ok(vec![
            "end-to-end workspaces need an end-to-end account".into()
        ]);
    };
    let dek = e2e::to_key(&B64.decode(dek)?)?;
    let mut notes = Vec::new();
    for ws in sealed {
        let (keys, why) = super::team::project_keys(client, &dek, store.data_dir(), &ws)?;
        notes.extend(why);
        for p in projects.iter_mut().filter(|p| p.workspace == ws) {
            let (Some(ct), Some(key)) = (&p.remote_ct, keys.get(&(p.project_id, p.key_version)))
            else {
                continue;
            };
            let ctx = v2::ctx_project_remote(p.project_id, p.key_version);
            if let Ok(plain) = e2e::open(key, &B64.decode(ct)?, &ctx) {
                p.git_remote = String::from_utf8_lossy(&plain).into_owned();
            }
        }
    }
    Ok(notes)
}

fn find_target(client: &Client, dir: &Path, store: &Store) -> Result<String> {
    let raw = folder_remote(dir).with_context(|| {
        format!(
            "{} has no git remote; run `clioilc link <workspace>/<project>`",
            dir.display()
        )
    })?;
    let remote =
        normalize_remote(&raw).with_context(|| format!("cannot read git remote {raw:?}"))?;
    let mut projects: Vec<Linkable> = client.get("/api/v1/workspaces/linkable/")?;
    let notes = open_e2e_remotes(&mut projects, store, client)?;
    for p in projects.iter_mut().filter(|p| p.name.is_empty()) {
        p.name = p.project.clone(); // E2E names are sealed; the slug stands in
    }
    let matches: Vec<&Linkable> = projects.iter().filter(|p| p.git_remote == remote).collect();
    match matches.as_slice() {
        [one] => {
            let target = format!("{}/{}", one.workspace, one.project);
            eprintln!(
                "this folder's remote {remote} belongs to {target} ({})",
                one.name
            );
            if !io::stdin().is_terminal() {
                bail!("run `clioilc link {target}` to link without asking");
            }
            if !confirm(&format!("Link it to {target}?"))? {
                bail!("not linked");
            }
            Ok(target)
        }
        [] => {
            let mut msg = format!("no team project you can link to has the git remote {remote}");
            for note in &notes {
                msg.push_str(&format!("\n  ({note})"));
            }
            if projects.is_empty() {
                msg.push_str("\n(you cannot link to any project yet: ask for an invitation)");
            } else {
                msg.push_str(
                    "\nprojects you can link to (set a project's git remote in the dashboard):",
                );
                for p in &projects {
                    let remote = if p.git_remote.is_empty() {
                        "(no git remote)"
                    } else {
                        &p.git_remote
                    };
                    msg.push_str(&format!("\n  {}/{}  {remote}", p.workspace, p.project));
                }
            }
            bail!(msg)
        }
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|p| format!("{}/{}", p.workspace, p.project))
                .collect();
            bail!(
                "several projects use {remote}: {}; pass one of them",
                names.join(", ")
            )
        }
    }
}

pub(crate) fn split_target(target: &str) -> Result<(&str, &str)> {
    match target.split_once('/') {
        Some((ws, p)) if !ws.is_empty() && !p.is_empty() && !p.contains('/') => Ok((ws, p)),
        _ => bail!("expected <workspace>/<project>, e.g. acme/api"),
    }
}

fn describe(db: &UsageDb, key: &str) -> Result<String> {
    let (count, first) = db.project_activity(key)?;
    Ok(match first {
        Some(first) if count > 0 => format!("{count} message(s) since {}", &first[..10]),
        _ => "no agent activity recorded yet".to_string(),
    })
}

pub fn link(store: &Store, opts: Options) -> Result<()> {
    let auth = auth::require(store.data_dir())?;
    let client = Client::new(&auth.server, Some(&auth.token))?;
    let db = UsageDb::open(&store.usage_db_path())?;
    let (dir, name) = folder(opts.path)?;
    let target = match opts.target {
        Some(t) => t,
        None => find_target(&client, &dir, store)?,
    };
    let (ws, project) = split_target(&target)?;
    let key = db.project_key(&name);

    let base = format!("/api/v1/workspaces/{ws}/projects/{project}/");
    if let Err(err) = client.get::<serde_json::Value>(&base) {
        if api_error_of(&err).is_some_and(|e| e.status == 404) {
            bail!("{ws}/{project} not found, or you are not a member of it");
        }
        return Err(err);
    }

    eprintln!("folder:  {} ({})", dir.display(), describe(&db, &key)?);
    let history = match opts.history {
        Some(h) => h,
        None if db.project_activity(&key)?.0 == 0 => false,
        None if io::stdin().is_terminal() => {
            confirm("Also share this folder's history from before today?")?
        }
        None => false,
    };

    let mut keys = vec![(name.clone(), key)];
    let subs = db.projects_under(&name)?;
    if !subs.is_empty() {
        eprintln!("{} subfolder project(s) recorded:", subs.len());
        for (sub, _) in &subs {
            eprintln!("  {}", &sub[name.len()..]);
        }
        let take = opts.subfolders
            || (io::stdin().is_terminal() && confirm("Link them to the same project too?")?);
        if take {
            keys.extend(subs);
        }
    }

    for (label, key) in keys {
        let body = json!({ "project_key": key, "share_history": history });
        match client.post::<Link>(&format!("{base}sources/"), &body) {
            Ok(link) => eprintln!(
                "linked {label} -> {}/{} ({})",
                link.workspace,
                link.project,
                match link.share_from {
                    None => "all history shared".to_string(),
                    Some(from) => format!("shared from {}", &from[..16.min(from.len())]),
                }
            ),
            Err(err) => match api_error_of(&err).and_then(|e| e.error.clone()).as_deref() {
                Some("already_linked") => eprintln!(
                    "skipped {label}: already linked to another project (`clioilc unlink` first)"
                ),
                Some("forbidden") => bail!("your role in {ws}/{project} cannot link folders"),
                Some("e2e_workspace_required") => bail!(
                    "{ws} is not an end-to-end workspace; end-to-end accounts share only in end-to-end workspaces"
                ),
                Some("e2e_account_required") => bail!(
                    "{ws} is an end-to-end workspace; it accepts end-to-end accounts only"
                ),
                _ => return Err(err),
            },
        }
    }
    eprintln!("teammates now see this folder's usage and tool actions; your prompts and replies");
    eprintln!("stay private unless you turn on sharing for the project in the dashboard");
    Ok(())
}

pub fn links(store: &Store) -> Result<()> {
    let auth = auth::require(store.data_dir())?;
    let client = Client::new(&auth.server, Some(&auth.token))?;
    let db = UsageDb::open(&store.usage_db_path())?;
    let links: Vec<Link> = client.get("/api/v1/workspaces/links/")?;
    if links.is_empty() {
        println!("no linked folders");
        return Ok(());
    }
    for link in links {
        let name = db
            .project_name(&link.project_key)?
            .unwrap_or_else(|| format!("(not on this machine: {})", link.project_key));
        let from = link
            .share_from
            .as_deref()
            .map_or("all history", |f| &f[..10]);
        println!("{}/{}\t{name}\t{from}", link.workspace, link.project);
    }
    Ok(())
}

pub fn unlink(store: &Store, path: Option<PathBuf>) -> Result<()> {
    let auth = auth::require(store.data_dir())?;
    let client = Client::new(&auth.server, Some(&auth.token))?;
    let db = UsageDb::open(&store.usage_db_path())?;
    let (dir, name) = folder(path)?;
    let key = db.project_key(&name);
    let links: Vec<Link> = client.get("/api/v1/workspaces/links/")?;
    let Some(link) = links.into_iter().find(|l| l.project_key == key) else {
        bail!("{} is not linked to a team project", dir.display());
    };
    client.delete(&format!(
        "/api/v1/workspaces/{}/projects/{}/sources/{key}/",
        link.workspace, link.project
    ))?;
    eprintln!(
        "unlinked {} from {}/{}; its data left the team's view",
        dir.display(),
        link.workspace,
        link.project
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_names_folders_like_claude_code() {
        assert_eq!(
            claude_project_name(Path::new("/Volumes/SSD1/Private/clioilc")),
            "-Volumes-SSD1-Private-clioilc"
        );
        assert_eq!(
            claude_project_name(Path::new("/Users/me/my_app.v2")),
            "-Users-me-my-app-v2"
        );
    }

    // Same table as GIT_REMOTE_CASES in backend apps/workspaces/tests/test_git.py.
    const GIT_REMOTE_CASES: &[(&str, Option<&str>)] = &[
        ("git@github.com:Acme/API.git", Some("github.com/acme/api")),
        ("https://github.com/acme/api", Some("github.com/acme/api")),
        (
            "https://github.com/acme/api.git/",
            Some("github.com/acme/api"),
        ),
        (
            "ssh://git@github.com:22/acme/api.git",
            Some("github.com/acme/api"),
        ),
        (
            "https://user:token@gitlab.com/group/sub/repo.git",
            Some("gitlab.com/group/sub/repo"),
        ),
        ("github.com:acme/api", Some("github.com/acme/api")),
        (
            "  git@bitbucket.org:team/repo  ",
            Some("bitbucket.org/team/repo"),
        ),
        ("/home/me/repos/api.git", None),
        ("not a remote", None),
        ("https://github.com/", None),
        ("", None),
    ];

    #[test]
    fn normalizes_remotes_like_the_backend() {
        for (raw, want) in GIT_REMOTE_CASES {
            assert_eq!(normalize_remote(raw).as_deref(), *want, "{raw}");
        }
    }

    #[test]
    fn target_is_workspace_slash_project() {
        assert_eq!(split_target("acme/api").unwrap(), ("acme", "api"));
        for bad in ["acme", "acme/", "/api", "a/b/c"] {
            assert!(split_target(bad).is_err(), "{bad}");
        }
    }
}
