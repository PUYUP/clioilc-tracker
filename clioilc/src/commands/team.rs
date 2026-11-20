//! E2E team workspaces on this machine (backend docs/e2e-teams.md):
//! the identity bundle, pinned teammate keys, and team sync, which copies
//! the records of linked folders, sealed with the project key, to the team.
//!
//! A grant is used only after its signature verifies against the granter's
//! Ed25519 key, and that key is either this account's own or one pinned
//! after comparing fingerprints out of band (`clioilc team`).

use super::login::confirm;
use crate::cloud::client::{api_error_of, Client};
use crate::cloud::e2e::{self, KEY_LEN};
use crate::cloud::e2e_v2::{self as v2, GrantFields, Identity};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clioilc_core::UsageDb;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Records per upload (the server accepts up to 500).
const BATCH: usize = 200;

#[derive(Deserialize)]
pub struct Me {
    pub id: u64,
    pub username: String,
}

#[derive(Deserialize, Clone)]
struct Link {
    workspace: String,
    #[serde(default)]
    enc_mode: String,
    project: String,
    #[serde(default)]
    project_id: u64,
    project_key: String,
    share_from: Option<String>,
}

#[derive(Deserialize)]
struct IdentityRow {
    username: String,
    ed25519_pk: String,
    fingerprint: String,
    wrapped_secret: Option<String>,
}

#[derive(Deserialize)]
struct KeyProject {
    id: u64,
    slug: String,
    version: u64,
    rotation_needed: bool,
}

#[derive(Deserialize)]
struct GrantRow {
    target: String,
    target_id: u64,
    version: u64,
    wrapped: String,
    signature: String,
    granter_id: Option<u64>,
}

#[derive(Deserialize)]
struct KeyState {
    projects: Vec<KeyProject>,
    grants: Vec<GrantRow>,
    identities: HashMap<String, IdentityRow>,
}

// --- pinned keys --------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Pin {
    pub username: String,
    pub ed25519_pk: String,
    pub fingerprint: String,
}

/// Teammates' keys this machine verified, by user id (`pins.json`, 0600).
#[derive(Serialize, Deserialize, Default)]
pub struct Pins(BTreeMap<u64, Pin>);

impl Pins {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("pins.json")
    }

    pub fn load(data_dir: &Path) -> Result<Self> {
        match std::fs::read(Self::path(data_dir)) {
            Ok(raw) => Ok(serde_json::from_slice(&raw).context("reading pins.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        std::fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

/// How a granter's key relates to what this machine trusts.
#[derive(Debug, PartialEq, Eq)]
enum Trust {
    Mine,
    Pinned,
    Unknown,
    Changed,
}

fn trust(pins: &Pins, me: &Me, my_ed25519: &[u8; 32], granter_id: u64, row: &IdentityRow) -> Trust {
    if granter_id == me.id {
        return if row.ed25519_pk == B64.encode(my_ed25519) {
            Trust::Mine
        } else {
            Trust::Changed
        };
    }
    match pins.0.get(&granter_id) {
        None => Trust::Unknown,
        Some(p) if p.ed25519_pk == row.ed25519_pk && p.fingerprint == row.fingerprint => {
            Trust::Pinned
        }
        Some(_) => Trust::Changed,
    }
}

// --- identity -------------------------------------------------------------------------

pub fn me(client: &Client) -> Result<Me> {
    client.get("/api/v1/users/me/")
}

/// This account's identity bundle, created (and uploaded) when missing.
pub fn identity(client: &Client, me: &Me, dek: &[u8; KEY_LEN]) -> Result<(Identity, bool)> {
    match client.get::<IdentityRow>("/api/v1/users/identity/") {
        Ok(row) => {
            let wrapped = B64.decode(row.wrapped_secret.as_deref().unwrap_or_default())?;
            let secret = e2e::open(dek, &wrapped, &v2::ctx_identity(me.id))
                .context("cannot open your identity with this machine's key")?;
            Ok((Identity::from_secret(&secret)?, false))
        }
        Err(err) if api_error_of(&err).is_some_and(|e| e.status == 404) => {
            let id = Identity::generate()?;
            let body = json!({
                "xwing_pk": B64.encode(id.xwing_public()?),
                "ed25519_pk": B64.encode(id.ed25519_public()),
                "wrapped_secret": B64.encode(e2e::seal(dek, &id.secret(), &v2::ctx_identity(me.id))?),
            });
            match client.put::<Value>("/api/v1/users/identity/", &body) {
                Ok(_) => Ok((id, true)),
                // Another client (the browser, another machine) made one
                // meanwhile: use that one, never replace it silently.
                Err(err)
                    if api_error_of(&err)
                        .is_some_and(|e| e.error.as_deref() == Some("identity_exists")) =>
                {
                    let row: IdentityRow = client.get("/api/v1/users/identity/")?;
                    let wrapped = B64.decode(row.wrapped_secret.as_deref().unwrap_or_default())?;
                    let secret = e2e::open(dek, &wrapped, &v2::ctx_identity(me.id))
                        .context("cannot open your identity with this machine's key")?;
                    Ok((Identity::from_secret(&secret)?, false))
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

// --- team sync ----------------------------------------------------------------------------

#[derive(Default)]
pub struct Report {
    pub uploaded: usize,
    /// One line per project that could not sync, e.g. waiting for a key.
    pub notes: Vec<String>,
}

/// Upload new or changed records of folders linked to E2E projects.
/// No-op for server-mode accounts.
pub fn sync(
    client: &Client,
    db: &mut UsageDb,
    dek: &[u8; KEY_LEN],
    data_dir: &Path,
) -> Result<Report> {
    let mut report = Report::default();
    let links: Vec<Link> = client.get("/api/v1/workspaces/links/")?;
    let links: Vec<Link> = links.into_iter().filter(|l| l.enc_mode == "e2e").collect();
    if links.is_empty() {
        return Ok(report);
    }
    let me = me(client)?;
    let (ident, created) = identity(client, &me, dek)?;
    let my_fp = ident.fingerprint()?;
    if created {
        report.notes.push(format!(
            "created your team identity; fingerprint {}",
            v2::format_fingerprint(&my_fp)
        ));
    }
    let pins = Pins::load(data_dir)?;
    let my_ed = ident.ed25519_public();

    let mut by_ws: BTreeMap<String, Vec<Link>> = BTreeMap::new();
    for l in links {
        by_ws.entry(l.workspace.clone()).or_default().push(l);
    }
    for (ws, links) in by_ws {
        let state: KeyState = client.get(&format!("/api/v1/workspaces/{ws}/keys/"))?;
        let mut by_project: BTreeMap<u64, Vec<Link>> = BTreeMap::new();
        for l in links {
            by_project.entry(l.project_id).or_default().push(l);
        }
        for (project_id, links) in by_project {
            let slug = &links[0].project;
            let label = format!("{ws}/{slug}");
            let Some(proj) = state.projects.iter().find(|p| p.id == project_id) else {
                report.notes.push(format!("{label}: no access"));
                continue;
            };
            if proj.version == 0 {
                report
                    .notes
                    .push(format!("{label}: waiting for the project key"));
                continue;
            }
            if proj.rotation_needed {
                report
                    .notes
                    .push(format!("{label}: paused until a manager rotates the key"));
                continue;
            }
            let Some(grant) = state.grants.iter().find(|g| {
                g.target == "project" && g.target_id == project_id && g.version == proj.version
            }) else {
                report.notes.push(format!(
                    "{label}: waiting for a manager to give you the key (your fingerprint {})",
                    v2::format_fingerprint(&my_fp)
                ));
                continue;
            };
            let key = match open_grant(&state, grant, &pins, &me, &my_ed, &ident, &my_fp) {
                Ok(key) => key,
                Err(why) => {
                    report.notes.push(format!("{label}: {why}"));
                    continue;
                }
            };
            let settings: Value = client.get(&format!(
                "/api/v1/workspaces/{ws}/projects/{}/me/",
                proj.slug
            ))?;
            let text = settings["share_text"].as_bool().unwrap_or(false);
            report.uploaded += upload(client, db, &ws, proj, &links, &key, me.id, text)?;
        }
    }
    Ok(report)
}

/// Opened project keys by (project id, version).
pub type ProjectKeys = HashMap<(u64, u64), [u8; KEY_LEN]>;

/// Project keys of an E2E workspace I can use, by (project id, version):
/// only grants that verify against my own key or a pinned teammate key.
/// The notes say why a grant was not used.
pub fn project_keys(
    client: &Client,
    dek: &[u8; KEY_LEN],
    data_dir: &Path,
    ws: &str,
) -> Result<(ProjectKeys, Vec<String>)> {
    let me = me(client)?;
    let (ident, _) = identity(client, &me, dek)?;
    let my_fp = ident.fingerprint()?;
    let my_ed = ident.ed25519_public();
    let pins = Pins::load(data_dir)?;
    let state: KeyState = client.get(&format!("/api/v1/workspaces/{ws}/keys/"))?;
    let mut keys = HashMap::new();
    let mut notes = Vec::new();
    for g in state.grants.iter().filter(|g| g.target == "project") {
        match open_grant(&state, g, &pins, &me, &my_ed, &ident, &my_fp) {
            Ok(key) => {
                keys.insert((g.target_id, g.version), key);
            }
            Err(why) => notes.push(format!("{ws}: {why}")),
        }
    }
    Ok((keys, notes))
}

fn open_grant(
    state: &KeyState,
    grant: &GrantRow,
    pins: &Pins,
    me: &Me,
    my_ed: &[u8; 32],
    ident: &Identity,
    my_fp: &[u8; 32],
) -> std::result::Result<[u8; KEY_LEN], String> {
    let granter_id = grant
        .granter_id
        .ok_or("the key's granter left; ask for a new grant")?;
    let row = state
        .identities
        .get(&granter_id.to_string())
        .ok_or("the granter's public key is missing")?;
    match trust(pins, me, my_ed, granter_id, row) {
        Trust::Mine | Trust::Pinned => {}
        Trust::Unknown => {
            return Err(format!(
                "key from @{} not verified yet; run `clioilc team` to compare fingerprints",
                row.username
            ))
        }
        Trust::Changed => {
            return Err(format!(
                "@{}'s key changed since you verified it; run `clioilc team`",
                row.username
            ))
        }
    }
    let wrapped = B64.decode(&grant.wrapped).map_err(|e| e.to_string())?;
    let signature = B64.decode(&grant.signature).map_err(|e| e.to_string())?;
    let granter_pk = B64.decode(&row.ed25519_pk).map_err(|e| e.to_string())?;
    let fields = GrantFields {
        target: "project",
        target_id: grant.target_id,
        version: grant.version,
        recipient_id: me.id,
        recipient_fp: my_fp,
        wrapped: &wrapped,
    };
    if !v2::verify_grant(&granter_pk, &signature, &fields) {
        return Err("the key's signature does not verify; not using it".into());
    }
    let ctx = v2::ctx_grant("project", grant.target_id, grant.version, me.id)
        .map_err(|e| e.to_string())?;
    let key = v2::unwrap(ident, &wrapped, &ctx).map_err(|e| e.to_string())?;
    e2e::to_key(&key).map_err(|e| e.to_string())
}

#[allow(clippy::too_many_arguments)]
fn upload(
    client: &Client,
    db: &mut UsageDb,
    ws: &str,
    proj: &KeyProject,
    links: &[Link],
    key: &[u8; KEY_LEN],
    me: u64,
    text: bool,
) -> Result<usize> {
    let team_id = format!("{:032x}", proj.id);
    let sent = db.team_sent(proj.id)?;
    let mut pending = Vec::new();
    for link in links {
        for mut c in db.team_candidates(&link.project_key, link.share_from.as_deref(), text)? {
            if c.kind == "prompt" {
                c.data["project_id"] = json!(team_id);
            }
            let plain = serde_json::to_vec(&c.data)?;
            let digest: String = Sha256::digest(&plain)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let fresh = match sent.get(&(c.kind.to_string(), c.id.clone())) {
                Some((version, d)) => *version != proj.version || *d != digest,
                None => true,
            };
            if fresh {
                pending.push((c, plain, digest));
            }
        }
    }
    let url = format!(
        "/api/v1/workspaces/{ws}/projects/{}/team-records/",
        proj.slug
    );
    let mut uploaded = 0;
    for chunk in pending.chunks(BATCH) {
        let mut records = Vec::with_capacity(chunk.len());
        for (c, plain, _) in chunk {
            let ctx = v2::ctx_team(proj.id, proj.version, me, c.kind, &c.id);
            records.push(json!({
                "kind": c.kind, "id": c.id, "ts": c.ts,
                "ciphertext": B64.encode(e2e::seal(key, plain, &ctx)?),
            }));
        }
        let _: Value = client.post(
            &url,
            &json!({ "key_version": proj.version, "records": records }),
        )?;
        let marks: Vec<_> = chunk
            .iter()
            .map(|(c, _, d)| (c.kind.to_string(), c.id.clone(), d.clone()))
            .collect();
        db.team_mark(proj.id, proj.version, &marks)?;
        uploaded += chunk.len();
    }
    if !text {
        // The server dropped shared prompts and replies; send them again if
        // sharing is turned back on.
        db.team_forget(proj.id, &["prompt", "block"])?;
    }
    Ok(uploaded)
}

// --- `clioilc team` ------------------------------------------------------------------------

/// Show this machine's team identity, verify teammates who gave you keys
/// (interactively), then run team sync once.
pub fn run(client: &Client, db: &mut UsageDb, dek: &[u8; KEY_LEN], data_dir: &Path) -> Result<()> {
    let me = me(client)?;
    let (ident, created) = identity(client, &me, dek)?;
    let fp = ident.fingerprint()?;
    println!(
        "you: @{}  fingerprint {}",
        me.username,
        v2::format_fingerprint(&fp)
    );
    if created {
        println!("(new identity: ask a workspace manager to give you access, and compare this fingerprint with them)");
    }
    let mut pins = Pins::load(data_dir)?;
    let my_ed = ident.ed25519_public();
    let links: Vec<Link> = client.get("/api/v1/workspaces/links/")?;
    let mut seen = std::collections::BTreeSet::new();
    for ws in links
        .iter()
        .filter(|l| l.enc_mode == "e2e")
        .map(|l| l.workspace.clone())
    {
        if !seen.insert(ws.clone()) {
            continue;
        }
        let state: KeyState = client.get(&format!("/api/v1/workspaces/{ws}/keys/"))?;
        for g in &state.grants {
            let Some(granter) = g.granter_id else {
                continue;
            };
            let Some(row) = state.identities.get(&granter.to_string()) else {
                continue;
            };
            let t = trust(&pins, &me, &my_ed, granter, row);
            if matches!(t, Trust::Mine | Trust::Pinned) {
                continue;
            }
            let fp: [u8; 32] = hex32(&row.fingerprint)?;
            if t == Trust::Changed {
                println!(
                    "WARNING: @{}'s key is not the one you verified before.",
                    row.username
                );
            }
            println!(
                "{ws}: key from @{}  fingerprint {}",
                row.username,
                v2::format_fingerprint(&fp)
            );
            if confirm(&format!(
                "Does @{} see exactly this fingerprint on their Account page?",
                row.username
            ))? {
                pins.0.insert(
                    granter,
                    Pin {
                        username: row.username.clone(),
                        ed25519_pk: row.ed25519_pk.clone(),
                        fingerprint: row.fingerprint.clone(),
                    },
                );
                pins.save(data_dir)?;
                println!("verified @{}", row.username);
            } else {
                println!("not verified; keys from @{} stay unused", row.username);
            }
        }
    }
    let report = sync(client, db, dek, data_dir)?;
    for n in &report.notes {
        println!("{n}");
    }
    println!("team sync: {} record(s) uploaded", report.uploaded);
    Ok(())
}

fn hex32(s: &str) -> Result<[u8; 32]> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("zz"), 16))
        .collect::<std::result::Result<_, _>>()
        .context("bad fingerprint")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad fingerprint length"))
}

pub fn require_dek(data_key: Option<&[u8; KEY_LEN]>) -> Result<&[u8; KEY_LEN]> {
    match data_key {
        Some(k) => Ok(k),
        None => bail!("team workspaces with end-to-end keys need an end-to-end account"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(username: &str, ed: &[u8; 32], fp: &str) -> IdentityRow {
        IdentityRow {
            username: username.into(),
            ed25519_pk: B64.encode(ed),
            fingerprint: fp.into(),
            wrapped_secret: None,
        }
    }

    #[test]
    fn only_my_key_or_a_pinned_one_is_trusted() {
        let me = Me {
            id: 1,
            username: "me".into(),
        };
        let mine = [1u8; 32];
        let mut pins = Pins::default();
        assert_eq!(
            trust(&pins, &me, &mine, 1, &row("me", &mine, "aa")),
            Trust::Mine
        );
        assert_eq!(
            trust(&pins, &me, &mine, 1, &row("me", &[9; 32], "aa")),
            Trust::Changed
        );
        assert_eq!(
            trust(&pins, &me, &mine, 2, &row("ann", &[2; 32], "bb")),
            Trust::Unknown
        );
        pins.0.insert(
            2,
            Pin {
                username: "ann".into(),
                ed25519_pk: B64.encode([2u8; 32]),
                fingerprint: "bb".into(),
            },
        );
        assert_eq!(
            trust(&pins, &me, &mine, 2, &row("ann", &[2; 32], "bb")),
            Trust::Pinned
        );
        assert_eq!(
            trust(&pins, &me, &mine, 2, &row("ann", &[3; 32], "cc")),
            Trust::Changed
        );
    }

    #[test]
    fn pins_round_trip_privately() {
        let dir = std::env::temp_dir().join(format!("clioilc-pins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut pins = Pins::load(&dir).unwrap();
        pins.0.insert(
            7,
            Pin {
                username: "bob".into(),
                ed25519_pk: "x".into(),
                fingerprint: "f".into(),
            },
        );
        pins.save(&dir).unwrap();
        assert!(Pins::load(&dir).unwrap().0.contains_key(&7));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("pins.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
