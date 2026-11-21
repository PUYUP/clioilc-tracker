//! `clioilc service install | uninstall | status`: run `clioilc watch --sync`
//! in the background at login (launchd on macOS, systemd --user on Linux),
//! restarted if it stops, logging to `<data dir>/watch.log`.

use crate::cloud::auth;
use anyhow::{bail, Context, Result};
use clioilc_core::{home_dir, Store};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_LABEL: &str = "com.clioilc.watch";
/// Overrides the service name, e.g. to run a second account's watcher.
pub const LABEL_ENV: &str = "CLIOILC_SERVICE_LABEL";

fn label() -> String {
    std::env::var(LABEL_ENV)
        .ok()
        .filter(|l| {
            !l.is_empty()
                && l.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
        })
        .unwrap_or_else(|| DEFAULT_LABEL.to_string())
}

struct Spec {
    label: String,
    exe: PathBuf,
    data_dir: PathBuf,
    log: PathBuf,
}

impl Spec {
    fn new(store: &Store) -> Result<Self> {
        let data_dir = store.data_dir();
        fs::create_dir_all(data_dir)?;
        let data_dir = data_dir.canonicalize()?;
        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("locating the clioilc binary")?;
        Ok(Self {
            label: label(),
            log: data_dir.join("watch.log"),
            exe,
            data_dir,
        })
    }
}

pub fn install(store: &Store) -> Result<()> {
    // Without a login, `watch --sync` exits at once and the service would
    // restart forever.
    auth::require(store.data_dir())?;
    let spec = Spec::new(store)?;
    let exe = spec.exe.to_string_lossy();
    if exe.contains("/target/debug/") {
        eprintln!("warning: installing a debug build ({exe}); prefer `cargo build --release`");
    }
    eprintln!(
        "note: stop any `clioilc watch` you started by hand, or the two will run side by side"
    );
    platform::install(&spec)?;
    eprintln!(
        "installed {} -> {} watch --sync",
        spec.label,
        spec.exe.display()
    );
    eprintln!("data: {}", spec.data_dir.display());
    eprintln!("log:  {}", spec.log.display());
    eprintln!("it starts now and at every login; `clioilc service status` to check");
    Ok(())
}

pub fn uninstall(store: &Store) -> Result<()> {
    let spec = Spec::new(store)?;
    if platform::uninstall(&spec)? {
        eprintln!("removed {}; the watcher is stopped", spec.label);
    } else {
        eprintln!("{} was not installed", spec.label);
    }
    Ok(())
}

/// The background watcher is installed and running.
pub fn running(store: &Store) -> bool {
    Spec::new(store)
        .and_then(|spec| platform::status(&spec))
        .is_ok_and(|s| s.starts_with("running"))
}

pub fn status(store: &Store) -> Result<()> {
    let spec = Spec::new(store)?;
    let state = platform::status(&spec)?;
    println!("service: {}", spec.label);
    println!("state:   {state}");
    println!("log:     {}", spec.log.display());
    if let Ok(text) = fs::read_to_string(&spec.log) {
        let tail: Vec<&str> = text.lines().rev().take(5).collect();
        if !tail.is_empty() {
            println!("last log lines:");
            for line in tail.iter().rev() {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("running {cmd}"))
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn plist_path(spec: &Spec) -> Result<PathBuf> {
        Ok(home_dir()?
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", spec.label)))
    }

    fn domain() -> Result<String> {
        let out = run("id", &["-u"])?;
        Ok(format!(
            "gui/{}",
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub fn plist(spec: &Spec) -> String {
        let args = [
            spec.exe.to_string_lossy().into_owned(),
            "--data-dir".into(),
            spec.data_dir.to_string_lossy().into_owned(),
            "watch".into(),
            "--sync".into(),
        ];
        let args: String = args
            .iter()
            .map(|a| format!("\n    <string>{}</string>", xml(a)))
            .collect();
        let log = xml(&spec.log.to_string_lossy());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>{args}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>30</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
            label = xml(&spec.label),
        )
    }

    pub fn install(spec: &Spec) -> Result<()> {
        let path = plist_path(spec)?;
        let target = format!("{}/{}", domain()?, spec.label);
        // Reinstall: unload the old definition first (ignore "not loaded").
        unload(&target)?;
        write_file(&path, &plist(spec))?;
        // launchd can still refuse for a moment after the old job is gone
        // ("Bootstrap failed: 5: Input/output error"), so retry briefly.
        let mut last = String::new();
        for _ in 0..20 {
            let out = run(
                "launchctl",
                &["bootstrap", &domain()?, &path.to_string_lossy()],
            )?;
            if out.status.success() {
                return Ok(());
            }
            last = String::from_utf8_lossy(&out.stderr).trim().to_string();
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        bail!("launchctl bootstrap failed: {last}")
    }

    pub fn uninstall(spec: &Spec) -> Result<bool> {
        let path = plist_path(spec)?;
        let target = format!("{}/{}", domain()?, spec.label);
        let loaded = unload(&target)?;
        let existed = path.exists();
        if existed {
            fs::remove_file(&path)?;
        }
        Ok(loaded || existed)
    }

    /// `launchctl bootout`, then wait until the job is really gone: bootout
    /// returns before that, and a bootstrap right after it fails.
    fn unload(target: &str) -> Result<bool> {
        let loaded = run("launchctl", &["bootout", target])?.status.success();
        for _ in 0..40 {
            if !run("launchctl", &["print", target])?.status.success() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        Ok(loaded)
    }

    pub fn status(spec: &Spec) -> Result<String> {
        let target = format!("{}/{}", domain()?, spec.label);
        let out = run("launchctl", &["print", &target])?;
        if !out.status.success() {
            return Ok(if plist_path(spec)?.exists() {
                "installed but not loaded (run `clioilc service install` again)".into()
            } else {
                "not installed".into()
            });
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |name: &str| {
            text.lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix(&format!("{name} = ")))
                .map(str::to_string)
        };
        let state = field("state").unwrap_or_else(|| "unknown".into());
        Ok(match field("pid") {
            Some(pid) => format!("{state} (pid {pid})"),
            None => match field("last exit code") {
                Some(code) => format!("{state} (last exit: {code})"),
                None => state,
            },
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_name(spec: &Spec) -> String {
        format!("{}.service", spec.label)
    }

    fn unit_path(spec: &Spec) -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .map_or_else(|| home_dir().map(|h| h.join(".config")), Ok)?;
        Ok(base.join("systemd/user").join(unit_name(spec)))
    }

    fn quote(p: &Path) -> String {
        format!(
            "\"{}\"",
            p.to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    }

    pub fn unit(spec: &Spec) -> String {
        format!(
            "[Unit]\nDescription=clioilc watcher (records AI agent activity and syncs it)\n\
             After=network-online.target\n\n[Service]\n\
             ExecStart={exe} --data-dir {dir} watch --sync\n\
             Restart=always\nRestartSec=30\n\
             StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
             [Install]\nWantedBy=default.target\n",
            exe = quote(&spec.exe),
            dir = quote(&spec.data_dir),
            log = spec.log.to_string_lossy(),
        )
    }

    fn systemctl(args: &[&str]) -> Result<std::process::Output> {
        let mut all = vec!["--user"];
        all.extend_from_slice(args);
        run("systemctl", &all)
    }

    pub fn install(spec: &Spec) -> Result<()> {
        write_file(&unit_path(spec)?, &unit(spec))?;
        systemctl(&["daemon-reload"])?;
        let out = systemctl(&["enable", "--now", &unit_name(spec)])?;
        if !out.status.success() {
            bail!(
                "systemctl enable failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let _ = systemctl(&["restart", &unit_name(spec)]);
        Ok(())
    }

    pub fn uninstall(spec: &Spec) -> Result<bool> {
        let path = unit_path(spec)?;
        let _ = systemctl(&["disable", "--now", &unit_name(spec)]);
        let existed = path.exists();
        if existed {
            fs::remove_file(&path)?;
            systemctl(&["daemon-reload"])?;
        }
        Ok(existed)
    }

    pub fn status(spec: &Spec) -> Result<String> {
        if !unit_path(spec)?.exists() {
            return Ok("not installed".into());
        }
        let out = systemctl(&["is-active", &unit_name(spec)])?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;

    pub fn install(_: &Spec) -> Result<()> {
        bail!("`clioilc service` supports macOS and Linux; on this system run `clioilc watch --sync` with your OS scheduler")
    }
    pub fn uninstall(_: &Spec) -> Result<bool> {
        Ok(false)
    }
    pub fn status(_: &Spec) -> Result<String> {
        Ok("not supported on this OS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            label: "com.clioilc.test".into(),
            exe: PathBuf::from("/Applications/My Tools/clioilc"),
            data_dir: PathBuf::from("/Users/me/.clioilc"),
            log: PathBuf::from("/Users/me/.clioilc/watch.log"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_runs_watch_sync_and_restarts() {
        let p = platform::plist(&spec());
        assert!(p.contains("<string>com.clioilc.test</string>"));
        assert!(p.contains("<string>/Applications/My Tools/clioilc</string>"));
        assert!(p.contains("<string>--data-dir</string>\n    <string>/Users/me/.clioilc</string>"));
        assert!(p.contains("<string>watch</string>\n    <string>--sync</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(p.contains("<string>/Users/me/.clioilc/watch.log</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_quotes_paths() {
        let u = platform::unit(&spec());
        assert!(u.contains("ExecStart=\"/Applications/My Tools/clioilc\" --data-dir \"/Users/me/.clioilc\" watch --sync"));
        assert!(u.contains("Restart=always"));
    }

    #[test]
    fn label_env_is_validated() {
        // Only checks the default path; env mutation would race other tests.
        assert!(label()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c)));
    }
}
