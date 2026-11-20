//! `clioilc login` / `clioilc logout`.
//!
//! Login runs the device flow (approve a code in the browser), then sets up
//! the account keyring so every device hashes ids with the same salt:
//! the first device uploads its salt; later devices download it, and if
//! their usage.db already holds data hashed with another salt, it is moved
//! aside and rebuilt from the logs.

use crate::cloud::auth::{self, Auth};
use crate::cloud::client::{api_error_of, Client};
use crate::cloud::e2e::{self, Kdf};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::ValueEnum;
use clioilc_core::{Store, UsageDb};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, BufRead, IsTerminal, Write};
use std::time::{Duration, Instant};

/// Env var with the server URL, used when `--server` is not given.
pub const SERVER_ENV: &str = "CLIOILC_SERVER";
/// Env var with the E2E passphrase, for non-interactive use.
pub const PASSPHRASE_ENV: &str = "CLIOILC_PASSPHRASE";
const MIN_PASSPHRASE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// The server encrypts private data at rest (recoverable, searchable)
    Server,
    /// Private data is encrypted on this machine with your passphrase;
    /// the server cannot read it, and a forgotten passphrase loses it
    E2e,
}

pub struct Options {
    pub server: Option<String>,
    pub name: Option<String>,
    pub mode: Option<Mode>,
    pub yes: bool,
    pub no_browser: bool,
}

#[derive(Deserialize)]
struct Start {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    device: Device,
}

#[derive(Deserialize)]
struct Device {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct Me {
    username: String,
}

#[derive(Deserialize)]
pub struct Keyring {
    pub enc_mode: String,
    pub salt: String,
    pub wrapped_key: Option<String>,
    pub kdf: Option<Kdf>,
}

pub fn login(store: &Store, opts: Options) -> Result<()> {
    let data_dir = store.data_dir();
    let saved = auth::load(data_dir)?;
    let server = opts
        .server
        .or_else(|| std::env::var(SERVER_ENV).ok().filter(|s| !s.is_empty()))
        .or_else(|| saved.as_ref().map(|a| a.server.clone()))
        .context("which server? pass --server <url> (or set CLIOILC_SERVER)")?;
    let name = opts.name.unwrap_or_else(default_device_name);

    let anon = Client::new(&server, None)?;
    let start: Start = anon.post("/api/v1/devices/authorize/", &json!({ "name": name }))?;
    eprintln!();
    eprintln!("  Open:  {}", start.verification_uri);
    eprintln!("  Code:  {}", start.user_code);
    eprintln!();
    if !opts.no_browser && io::stderr().is_terminal() {
        open_browser(&start.verification_uri_complete);
    }
    eprintln!("waiting for approval (Ctrl+C to cancel) ...");
    let token = poll(&anon, &start)?;

    let client = Client::new(&server, Some(&token.access_token))?;
    let result = (|| {
        let me: Me = client.get("/api/v1/users/me/")?;
        eprintln!("approved: {} as \"{}\"", me.username, token.device.name);
        let (enc_mode, data_key, salt) = setup_keyring(&client, store, opts.mode)?;
        apply_salt(store, &salt, opts.yes)?;
        Ok(Auth {
            server: server.trim_end_matches('/').to_string(),
            token: token.access_token.clone(),
            device_id: token.device.id,
            device_name: token.device.name.clone(),
            username: me.username,
            enc_mode,
            data_key: data_key.map(|k| B64.encode(k)),
        })
    })();
    let new_auth = match result {
        Ok(a) => a,
        Err(err) => {
            // Don't leave an unused token behind on the account.
            let _ = client.delete("/api/v1/devices/current/");
            return Err(err);
        }
    };

    // Replacing an older login on this machine: retire its token.
    if let Some(old) = saved.filter(|old| old.token != new_auth.token) {
        if let Ok(c) = Client::new(&old.server, Some(&old.token)) {
            let _ = c.delete("/api/v1/devices/current/");
        }
    }
    auth::save(data_dir, &new_auth)?;
    eprintln!(
        "logged in to {} (private data: {}); credentials in {}",
        new_auth.server,
        mode_label(&new_auth.enc_mode),
        auth::path(data_dir).display()
    );
    eprintln!("next: `clioilc sync`, or keep it running with `clioilc watch --sync`");
    Ok(())
}

pub fn logout(store: &Store) -> Result<()> {
    let data_dir = store.data_dir();
    let Some(saved) = auth::load(data_dir)? else {
        eprintln!("not logged in");
        return Ok(());
    };
    match Client::new(&saved.server, Some(&saved.token))
        .and_then(|c| c.delete("/api/v1/devices/current/"))
    {
        Ok(()) => eprintln!(
            "device \"{}\" revoked on {}",
            saved.device_name, saved.server
        ),
        Err(err) => eprintln!(
            "warning: could not revoke the device on the server ({err:#}); \
             revoke it from the dashboard"
        ),
    }
    auth::remove(data_dir)?;
    eprintln!("logged out; local data in {} is kept", data_dir.display());
    Ok(())
}

/// What the dashboard calls this machine when `--name` is not given. On
/// macOS the hostname is often just an IP (e.g. on DHCP), so prefer the
/// Computer Name from System Settings.
fn default_device_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !name.is_empty() {
            return name;
        }
    }
    let host = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    if host.is_empty() {
        "unnamed device".into()
    } else {
        host
    }
}

fn mode_label(mode: &str) -> &str {
    match mode {
        "e2e" => "end-to-end encrypted",
        _ => "encrypted by the server",
    }
}

fn poll(client: &Client, start: &Start) -> Result<Token> {
    let deadline = Instant::now() + Duration::from_secs(start.expires_in);
    let mut interval = Duration::from_secs(start.interval.max(1));
    loop {
        std::thread::sleep(interval);
        if Instant::now() >= deadline {
            bail!("the code expired before it was approved; run `clioilc login` again");
        }
        let res: Result<Token> = client.post(
            "/api/v1/devices/token/",
            &json!({ "device_code": start.device_code }),
        );
        let err = match res {
            Ok(token) => return Ok(token),
            Err(err) => err,
        };
        match api_error_of(&err).and_then(|e| e.error.as_deref()) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            Some("access_denied") => bail!("login was denied in the browser"),
            Some("expired_token") => bail!("the code expired; run `clioilc login` again"),
            _ => return Err(err),
        }
    }
}

/// Returns (enc_mode, E2E data key, account salt).
fn setup_keyring(
    client: &Client,
    store: &Store,
    mode: Option<Mode>,
) -> Result<(String, Option<[u8; 32]>, Vec<u8>)> {
    match client.get::<Keyring>("/api/v1/users/keyring/") {
        Ok(ring) => {
            if let Some(m) = mode {
                let want = if m == Mode::E2e { "e2e" } else { "server" };
                if want != ring.enc_mode {
                    eprintln!(
                        "note: this account already uses {}; --mode is ignored",
                        mode_label(&ring.enc_mode)
                    );
                }
            }
            unlock(&ring)
        }
        Err(err)
            if api_error_of(&err).and_then(|e| e.error.as_deref()) == Some("keyring_missing") =>
        {
            create_keyring(client, store, mode)
        }
        Err(err) => Err(err),
    }
}

/// Open an existing keyring (asks for the passphrase in E2E mode).
pub fn unlock(ring: &Keyring) -> Result<(String, Option<[u8; 32]>, Vec<u8>)> {
    let salt = B64.decode(&ring.salt).context("keyring salt")?;
    if ring.enc_mode != "e2e" {
        return Ok((ring.enc_mode.clone(), None, salt));
    }
    let kdf = ring.kdf.as_ref().context("keyring has no KDF parameters")?;
    let wrapped = B64.decode(ring.wrapped_key.as_deref().context("keyring has no key")?)?;
    for attempt in 1..=3 {
        let passphrase = read_passphrase("passphrase: ", false)?;
        eprintln!("deriving key ...");
        let kek = kdf.derive(&passphrase)?;
        match e2e::open(&kek, &wrapped, e2e::AAD_DEK) {
            Ok(dek) => {
                let dek = e2e::to_key(&dek)?;
                let salt = e2e::open(&dek, &salt, e2e::AAD_SALT).context("decrypting salt")?;
                return Ok(("e2e".into(), Some(dek), salt));
            }
            Err(_) if attempt < 3 && std::env::var_os(PASSPHRASE_ENV).is_none() => {
                eprintln!("wrong passphrase, try again");
            }
            Err(_) => break,
        }
    }
    bail!("wrong passphrase")
}

fn create_keyring(
    client: &Client,
    store: &Store,
    mode: Option<Mode>,
) -> Result<(String, Option<[u8; 32]>, Vec<u8>)> {
    let mode = match mode {
        Some(m) => m,
        None => ask_mode()?,
    };
    // First device of the account: keep this machine's salt, so the ids
    // already in its usage.db stay valid.
    let salt = UsageDb::open(&store.usage_db_path())?.salt().to_vec();
    let (body, data_key) = match mode {
        Mode::Server => (
            json!({ "enc_mode": "server", "salt": B64.encode(&salt) }),
            None,
        ),
        Mode::E2e => {
            eprintln!(
                "choose a passphrase (min {MIN_PASSPHRASE} characters). It is needed on \
                 every new device and in the browser. If you forget it, your private \
                 data cannot be recovered."
            );
            let passphrase = read_passphrase("new passphrase: ", true)?;
            let kdf = Kdf::new()?;
            eprintln!("deriving key ...");
            let kek = kdf.derive(&passphrase)?;
            let dek = e2e::random::<{ e2e::KEY_LEN }>()?;
            let body = json!({
                "enc_mode": "e2e",
                "salt": B64.encode(e2e::seal(&dek, &salt, e2e::AAD_SALT)?),
                "wrapped_key": B64.encode(e2e::seal(&kek, &dek, e2e::AAD_DEK)?),
                "kdf": kdf,
            });
            (body, Some(dek))
        }
    };
    match client.post::<serde_json::Value>("/api/v1/users/keyring/", &body) {
        Ok(_) => {
            let enc_mode = if mode == Mode::E2e { "e2e" } else { "server" };
            Ok((enc_mode.into(), data_key, salt))
        }
        // Another device created it meanwhile: use that one.
        Err(err)
            if api_error_of(&err).and_then(|e| e.error.as_deref()) == Some("keyring_exists") =>
        {
            unlock(&client.get::<Keyring>("/api/v1/users/keyring/")?)
        }
        Err(err) => Err(err),
    }
}

/// Make usage.db use the account salt.
fn apply_salt(store: &Store, salt: &[u8], yes: bool) -> Result<()> {
    let path = store.usage_db_path();
    let mut db = UsageDb::open(&path)?;
    if db.salt() == salt {
        return Ok(());
    }
    if db.is_empty()? {
        return db.replace_salt(salt);
    }
    drop(db);

    eprintln!(
        "\nThis account was first used on another device. Ids in {} were hashed\n\
         with this machine's own salt, so they would not match. It will be moved\n\
         aside and rebuilt from the agent logs on this machine. History whose logs\n\
         were already deleted stays only in the backup. Stop `clioilc watch` first.",
        path.display()
    );
    if !yes && !confirm("rebuild usage.db now?")? {
        bail!("login cancelled; nothing was changed locally");
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    for suffix in ["", "-wal", "-shm"] {
        let from = path.with_file_name(format!("usage.db{suffix}"));
        if from.exists() {
            let to = path.with_file_name(format!("usage.db{suffix}.before-login-{stamp}"));
            std::fs::rename(&from, &to)
                .with_context(|| format!("moving {} aside", from.display()))?;
        }
    }
    UsageDb::open(&path)?.replace_salt(salt)?;
    eprintln!(
        "backup: {}",
        path.with_file_name(format!("usage.db.before-login-{stamp}"))
            .display()
    );
    super::usage::import(store, None, None)
}

fn ask_mode() -> Result<Mode> {
    if !io::stdin().is_terminal() {
        bail!("first login of this account: pass --mode server or --mode e2e");
    }
    eprintln!("\nHow should your private data (prompts, replies, thinking) be stored?");
    eprintln!("  1) server  - encrypted by the server; recoverable if you lose this machine");
    eprintln!("  2) e2e     - encrypted here with a passphrase; the server cannot read it,");
    eprintln!("               a forgotten passphrase means the data is gone");
    eprintln!("This cannot be changed later.");
    loop {
        match prompt_line("choice [1/2]: ")?.trim() {
            "1" | "server" => return Ok(Mode::Server),
            "2" | "e2e" => return Ok(Mode::E2e),
            _ => eprintln!("type 1 or 2"),
        }
    }
}

fn read_passphrase(prompt: &str, new: bool) -> Result<String> {
    if let Ok(p) = std::env::var(PASSPHRASE_ENV) {
        if new && p.chars().count() < MIN_PASSPHRASE {
            bail!("{PASSPHRASE_ENV} must be at least {MIN_PASSPHRASE} characters");
        }
        return Ok(p);
    }
    loop {
        let p = rpassword::prompt_password(prompt).context("reading passphrase")?;
        if !new {
            return Ok(p);
        }
        if p.chars().count() < MIN_PASSPHRASE {
            eprintln!("too short (min {MIN_PASSPHRASE} characters)");
            continue;
        }
        if rpassword::prompt_password("repeat passphrase: ")? != p {
            eprintln!("passphrases differ, try again");
            continue;
        }
        return Ok(p);
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("{question} (pass --yes to confirm non-interactively)");
    }
    Ok(matches!(
        prompt_line(&format!("{question} [y/N]: "))?.trim(),
        "y" | "Y" | "yes"
    ))
}

fn prompt_line(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
