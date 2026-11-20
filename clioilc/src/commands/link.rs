//! `clioilc link`, `links`, `unlink`: share a folder's agent data with a
//! team project (backend docs/workspaces.md). Only the folder's hash goes
//! to the server, never its name; the link lives on the server.

use super::login::confirm;
use crate::cloud::auth;
use crate::cloud::client::{api_error_of, Client};
use anyhow::{bail, Context, Result};
use clioilc_core::{Store, UsageDb};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

pub struct Options {
    /// `workspace/project`
    pub target: String,
    pub path: Option<PathBuf>,
    /// Some(true) = --history, Some(false) = --no-history, None = ask.
    pub history: Option<bool>,
    /// Also link known subfolder projects without asking.
    pub subfolders: bool,
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

fn split_target(target: &str) -> Result<(&str, &str)> {
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
    let (ws, project) = split_target(&opts.target)?;
    let auth = auth::require(store.data_dir())?;
    let client = Client::new(&auth.server, Some(&auth.token))?;
    let db = UsageDb::open(&store.usage_db_path())?;
    let (dir, name) = folder(opts.path)?;
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
                Some("e2e_not_supported_yet") => {
                    bail!("end-to-end accounts cannot share with a team yet")
                }
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

    #[test]
    fn target_is_workspace_slash_project() {
        assert_eq!(split_target("acme/api").unwrap(), ("acme", "api"));
        for bad in ["acme", "acme/", "/api", "a/b/c"] {
            assert!(split_target(bad).is_err(), "{bad}");
        }
    }
}
