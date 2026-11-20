//! Saved login: `<data dir>/auth.json`, readable by the owner only.
//!
//! Holds the device token and, for end-to-end accounts, the data key, so
//! `sync` can run unattended. This machine is already trusted with the
//! plaintext (usage.db), so the key adds no new exposure here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    pub server: String,
    pub token: String,
    pub device_id: u64,
    pub device_name: String,
    pub username: String,
    /// `server` or `e2e`.
    pub enc_mode: String,
    /// E2E data key, base64. `None` in server mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_key: Option<String>,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("auth.json")
}

pub fn load(data_dir: &Path) -> Result<Option<Auth>> {
    let path = path(data_dir);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn require(data_dir: &Path) -> Result<Auth> {
    load(data_dir)?.context("not logged in; run `clioilc login --server <url>` first")
}

/// Write atomically with mode 0600 (never briefly world-readable).
pub fn save(data_dir: &Path, auth: &Auth) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    let path = path(data_dir);
    let tmp = path.with_extension("json.tmp");
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
    file.write_all(&serde_json::to_vec_pretty(auth)?)?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn remove(data_dir: &Path) -> Result<bool> {
    match fs::remove_file(path(data_dir)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_private_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("clioilc-auth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(load(&dir).unwrap().is_none());
        let auth = Auth {
            server: "http://localhost:8000".into(),
            token: "clio_x".into(),
            device_id: 1,
            device_name: "laptop".into(),
            username: "octocat".into(),
            enc_mode: "server".into(),
            data_key: None,
        };
        save(&dir, &auth).unwrap();
        assert_eq!(load(&dir).unwrap().unwrap().token, "clio_x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path(&dir)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(remove(&dir).unwrap());
        assert!(!remove(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }
}
