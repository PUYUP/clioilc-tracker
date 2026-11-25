//! `clioilc attest`: a signed record of which commits of a release AI agents
//! helped with (backend docs/attestations.md). This machine knows the git
//! graph, so it lists the release's commits; the server matches them with
//! the work items and signs the result (in-toto Statement, DSSE, Ed25519).

use crate::cloud::{auth, client::Client};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clioilc_core::Store;
use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The server refuses longer releases.
const MAX_COMMITS: usize = 5000;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `release` (a tag or any revision) compared with `base` (default: the tag
/// before it), in the repository at `dir`.
pub fn run(
    store: &Store,
    release: &str,
    base: Option<String>,
    workspace: Option<String>,
    output: Option<PathBuf>,
    path: Option<PathBuf>,
) -> Result<()> {
    let dir = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let head = git(
        &dir,
        &["rev-parse", "--verify", &format!("{release}^{{commit}}")],
    )
    .with_context(|| format!("{release} is not a commit or tag here"))?;
    let base = match base {
        Some(b) => Some(b),
        // The tag before the release; none for the first release.
        None => git(
            &dir,
            &["describe", "--tags", "--abbrev=0", &format!("{head}^")],
        )
        .ok(),
    };
    let range = match &base {
        Some(b) => format!("{b}..{head}"),
        None => head.clone(),
    };
    let commits: Vec<String> = git(&dir, &["rev-list", &range])?
        .lines()
        .map(str::to_string)
        .collect();
    if commits.len() > MAX_COMMITS {
        bail!(
            "{} commits in {range}: at most {MAX_COMMITS} per attestation (pick a closer --base)",
            commits.len()
        );
    }
    let dir_str = dir.to_string_lossy();
    let repository = clioilc_core::git::remote_url(&dir_str)
        .and_then(|r| clioilc_core::git::normalize_remote(&r))
        .context("this repository has no remote to name it by (git remote add origin …)")?;

    let auth = auth::require(store.data_dir())?;
    let client = Client::new(&auth.server, Some(&auth.token))?;
    let mut body = json!({
        "repository": repository,
        "release": release,
        "base": base,
        "head": head,
        "commits": commits,
    });
    if let Some(ws) = &workspace {
        body["workspace"] = json!(ws);
    }
    let res: Value = client.post("/api/v1/usage/attestations/", &body)?;

    let file = output.unwrap_or_else(|| {
        let safe: String = release
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        PathBuf::from(format!("attestation-{safe}.json"))
    });
    std::fs::write(
        &file,
        serde_json::to_string_pretty(&res["envelope"])? + "\n",
    )
    .with_context(|| format!("writing {}", file.display()))?;

    let p = &res["statement"]["predicate"];
    println!(
        "{release} ({}): {} of {} commit(s) AI-assisted, ${:.2} API-equivalent",
        base.as_deref()
            .map_or("first release".to_string(), |b| format!("since {b}")),
        p["commits_ai_assisted"],
        p["commits_total"],
        p["cost_usd"].as_f64().unwrap_or(0.0)
    );
    for c in p["commits"].as_array().into_iter().flatten().take(10) {
        let models: Vec<&str> = c["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["model"].as_str())
            .collect();
        println!(
            "  {} @{} {}  [{}]",
            &c["commit"].as_str().unwrap_or("")[..7.min(c["commit"].as_str().unwrap_or("").len())],
            c["member"].as_str().unwrap_or("?"),
            c["subject"].as_str().unwrap_or(""),
            models.join(", ")
        );
    }
    println!("signed envelope: {}", file.display());
    if let Some(url) = res["url"].as_str() {
        println!("view it: {url}");
    }
    Ok(())
}

/// DSSE pre-authentication encoding: what the signature covers.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {} {} ",
        payload_type.len(),
        payload_type,
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

/// Check an envelope's signature against `public_key` (raw 32 bytes); the
/// statement on success.
pub fn check(envelope: &Value, public_key: &[u8]) -> Result<Value> {
    let payload_type = envelope["payloadType"].as_str().context("no payloadType")?;
    let payload = B64
        .decode(envelope["payload"].as_str().context("no payload")?)
        .context("payload is not base64")?;
    let key: [u8; 32] = public_key
        .try_into()
        .context("the public key is not 32 bytes")?;
    let key = VerifyingKey::from_bytes(&key).context("not an Ed25519 public key")?;
    let sigs = envelope["signatures"].as_array().context("no signatures")?;
    for s in sigs {
        let raw = B64.decode(s["sig"].as_str().unwrap_or_default())?;
        let raw: [u8; 64] = raw
            .as_slice()
            .try_into()
            .context("a signature is not 64 bytes")?;
        if key
            .verify_strict(&pae(payload_type, &payload), &Signature::from_bytes(&raw))
            .is_ok()
        {
            return Ok(serde_json::from_slice(&payload)?);
        }
    }
    bail!("the signature does not match: the attestation was changed, or signed by another key")
}

/// `clioilc attest verify <file>`: checks the signature here, with the key
/// the server publishes (`--server`, default: the logged-in one).
pub fn verify(store: &Store, file: &Path, server: Option<String>) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let doc: Value = serde_json::from_str(&text).context("not JSON")?;
    // The envelope alone, or a server answer that holds it.
    let envelope = if doc.get("envelope").is_some() {
        &doc["envelope"]
    } else {
        &doc
    };
    let server = match server {
        Some(s) => s,
        None => auth::require(store.data_dir())?.server,
    };
    let key: Value = Client::new(&server, None)?.get("/api/v1/usage/attestations/key/")?;
    let public = B64.decode(
        key["public_key"]
            .as_str()
            .context("the server sent no key")?,
    )?;
    let statement = check(envelope, &public)?;
    let p = &statement["predicate"];
    println!(
        "valid: signed by {} ({})",
        key["keyid"].as_str().unwrap_or("?"),
        server
    );
    println!(
        "{} {} · {} of {} commit(s) AI-assisted · issued {} for {}",
        p["repository"].as_str().unwrap_or("?"),
        p["release"].as_str().unwrap_or("?"),
        p["commits_ai_assisted"],
        p["commits_total"],
        p["issued_at"].as_str().unwrap_or("?"),
        p["requested_by"].as_str().unwrap_or("?")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn checks_dsse_signatures() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let statement = json!({"predicate": {"release": "v1"}});
        let payload = serde_json::to_vec(&statement).unwrap();
        let t = "application/vnd.in-toto+json";
        let sig = key.sign(&pae(t, &payload));
        let env = json!({
            "payloadType": t,
            "payload": B64.encode(&payload),
            "signatures": [{"keyid": "k", "sig": B64.encode(sig.to_bytes())}],
        });
        let public = key.verifying_key().to_bytes();
        assert_eq!(check(&env, &public).unwrap(), statement);

        let mut forged = env.clone();
        forged["payload"] = json!(B64.encode(br#"{"predicate":{"release":"v2"}}"#));
        assert!(check(&forged, &public).is_err());
        let other = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
        assert!(check(&env, &other).is_err());
    }

    #[test]
    fn pae_matches_the_dsse_spec() {
        assert_eq!(pae("t", b"ab"), b"DSSEv1 1 t 2 ab".to_vec());
    }
}
