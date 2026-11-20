//! E2E format v2: team keys (backend docs/e2e-teams.md, "Format v2").
//!
//! Must match the reference implementation (backend `apps/core/e2e_v2.py`)
//! byte for byte; `tests/fixtures/e2e-v2.json` is made by it.
//!
//! - Wrapping a key to a person: HPKE base mode, X-Wing (ML-KEM-768 +
//!   X25519) / HKDF-SHA256 / AES-256-GCM. Blob: `0x02 || enc(1120) || ct`,
//!   context in HPKE `info`.
//! - Grant signatures: Ed25519 over a length-prefixed message.
//! - Content (names, team records, the identity secret): v1 `seal`.

// Used by team sync (docs/e2e-teams.md, step 3); remove once it is wired in.
#![allow(dead_code)]

use anyhow::{anyhow, ensure, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hpke::{
    aead::AesGcm256, kdf::HkdfSha256, kem::XWing, Deserializable, Kem as _, OpModeR, OpModeS,
    Serializable,
};
use sha2::{Digest, Sha256};

use super::e2e::random;

pub const SUITE_XWING: u8 = 0x02;
pub const ENC_LEN: usize = 1120;
pub const XWING_PK_LEN: usize = 1216;
pub const SEED_LEN: usize = 32;

type Kem = XWing;

// --- contexts ---------------------------------------------------------------------

pub fn ctx_identity(user_id: u64) -> String {
    format!("clioilc:v2:identity:{user_id}")
}

/// `target` is "ws" or "project".
pub fn ctx_grant(target: &str, target_id: u64, version: u64, recipient_id: u64) -> Result<String> {
    ensure!(
        target == "ws" || target == "project",
        "target must be ws or project"
    );
    Ok(format!(
        "clioilc:v2:grant:{target}:{target_id}:{version}:{recipient_id}"
    ))
}

pub fn ctx_ws_name(ws_id: u64, version: u64) -> String {
    format!("clioilc:v2:ws-name:{ws_id}:{version}")
}

pub fn ctx_project_name(project_id: u64, version: u64) -> String {
    format!("clioilc:v2:project-name:{project_id}:{version}")
}

pub fn ctx_project_remote(project_id: u64, version: u64) -> String {
    format!("clioilc:v2:project-remote:{project_id}:{version}")
}

pub fn ctx_team(project_id: u64, version: u64, author_id: u64, kind: &str, id: &str) -> String {
    format!("clioilc:v2:team:{project_id}:{version}:{author_id}:{kind}:{id}")
}

// --- identity ---------------------------------------------------------------------

/// A user's key bundle: X-Wing (receives keys) + Ed25519 (signs grants).
/// Both secrets are 32-byte seeds; the secret blob is their concatenation.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    xwing_seed: [u8; SEED_LEN],
    ed25519_seed: [u8; SEED_LEN],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Identity { .. }") // never print secrets
    }
}

impl Identity {
    pub fn generate() -> Result<Self> {
        Ok(Self {
            xwing_seed: random()?,
            ed25519_seed: random()?,
        })
    }

    pub fn from_seeds(xwing_seed: [u8; SEED_LEN], ed25519_seed: [u8; SEED_LEN]) -> Self {
        Self {
            xwing_seed,
            ed25519_seed,
        }
    }

    pub fn from_secret(secret: &[u8]) -> Result<Self> {
        ensure!(
            secret.len() == 2 * SEED_LEN,
            "identity secret must be 64 bytes"
        );
        let (x, e) = secret.split_at(SEED_LEN);
        Ok(Self::from_seeds(x.try_into()?, e.try_into()?))
    }

    pub fn secret(&self) -> [u8; 2 * SEED_LEN] {
        let mut out = [0u8; 2 * SEED_LEN];
        out[..SEED_LEN].copy_from_slice(&self.xwing_seed);
        out[SEED_LEN..].copy_from_slice(&self.ed25519_seed);
        out
    }

    fn xwing_private(&self) -> Result<<Kem as hpke::Kem>::PrivateKey> {
        <Kem as hpke::Kem>::PrivateKey::from_bytes(&self.xwing_seed)
            .map_err(|e| anyhow!("X-Wing key: {e}"))
    }

    pub fn xwing_public(&self) -> Result<Vec<u8>> {
        Ok(Kem::sk_to_pk(&self.xwing_private()?).to_bytes().to_vec())
    }

    fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.ed25519_seed)
    }

    pub fn ed25519_public(&self) -> [u8; 32] {
        self.signing_key().verifying_key().to_bytes()
    }

    pub fn fingerprint(&self) -> Result<[u8; 32]> {
        fingerprint(&self.xwing_public()?, &self.ed25519_public())
    }
}

pub fn fingerprint(xwing_pk: &[u8], ed25519_pk: &[u8]) -> Result<[u8; 32]> {
    ensure!(
        xwing_pk.len() == XWING_PK_LEN && ed25519_pk.len() == 32,
        "bad public key length"
    );
    let mut h = Sha256::new();
    h.update(b"clioilc:v2:fp");
    h.update(xwing_pk);
    h.update(ed25519_pk);
    Ok(h.finalize().into())
}

/// 32 bytes as 16 groups of 4 upper-case hex digits.
pub fn format_fingerprint(fp: &[u8; 32]) -> String {
    let hex: String = fp.iter().map(|b| format!("{b:02X}")).collect();
    hex.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

// --- wrapping keys to a person (HPKE) ---------------------------------------------

pub fn wrap(recipient_xwing_pk: &[u8], key: &[u8], ctx: &str) -> Result<Vec<u8>> {
    ensure!(
        recipient_xwing_pk.len() == XWING_PK_LEN,
        "bad X-Wing public key length"
    );
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(recipient_xwing_pk)
        .map_err(|e| anyhow!("X-Wing public key: {e}"))?;
    let (enc, ct) = hpke::single_shot_seal::<AesGcm256, HkdfSha256, Kem>(
        &OpModeS::Base,
        &pk,
        ctx.as_bytes(),
        key,
        b"",
    )
    .map_err(|e| anyhow!("wrap: {e}"))?;
    let mut out = Vec::with_capacity(1 + ENC_LEN + ct.len());
    out.push(SUITE_XWING);
    out.extend_from_slice(&enc.to_bytes());
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn unwrap(identity: &Identity, blob: &[u8], ctx: &str) -> Result<Vec<u8>> {
    ensure!(
        blob.first() == Some(&SUITE_XWING) && blob.len() >= 1 + ENC_LEN + 16,
        "unknown wrap suite"
    );
    let (enc, ct) = blob[1..].split_at(ENC_LEN);
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc)
        .map_err(|e| anyhow!("wrap header: {e}"))?;
    hpke::single_shot_open::<AesGcm256, HkdfSha256, Kem>(
        &OpModeR::Base,
        &identity.xwing_private()?,
        &enc,
        ctx.as_bytes(),
        ct,
        b"",
    )
    .map_err(|_| anyhow!("cannot open key: not addressed to this identity, or tampered"))
}

// --- grant signatures -------------------------------------------------------------

pub struct GrantFields<'a> {
    pub target: &'a str,
    pub target_id: u64,
    pub version: u64,
    pub recipient_id: u64,
    pub recipient_fp: &'a [u8; 32],
    pub wrapped: &'a [u8],
}

/// A fixed label, then each field as u32 big-endian length || UTF-8.
pub fn grant_message(g: &GrantFields) -> Vec<u8> {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let fields = [
        g.target.to_string(),
        g.target_id.to_string(),
        g.version.to_string(),
        g.recipient_id.to_string(),
        hex(g.recipient_fp),
        hex(&Sha256::digest(g.wrapped)),
    ];
    let mut out = b"clioilc:v2:grant".to_vec();
    for f in fields {
        out.extend_from_slice(&(f.len() as u32).to_be_bytes());
        out.extend_from_slice(f.as_bytes());
    }
    out
}

pub fn sign_grant(granter: &Identity, g: &GrantFields) -> [u8; 64] {
    granter.signing_key().sign(&grant_message(g)).to_bytes()
}

pub fn verify_grant(granter_ed25519_pk: &[u8], signature: &[u8], g: &GrantFields) -> bool {
    let (Ok(pk), Ok(sig)) = (
        <[u8; 32]>::try_from(granter_ed25519_pk),
        <[u8; 64]>::try_from(signature),
    ) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    key.verify_strict(&grant_message(g), &Signature::from_bytes(&sig))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::e2e::{open, KEY_LEN};
    use serde_json::Value;

    const FIXTURE: &str = include_str!("../../tests/fixtures/e2e-v2.json");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn person(f: &Value, who: &str) -> Identity {
        Identity::from_seeds(
            unhex(f[who]["xwing_seed"].as_str().unwrap())
                .try_into()
                .unwrap(),
            unhex(f[who]["ed25519_seed"].as_str().unwrap())
                .try_into()
                .unwrap(),
        )
    }

    #[test]
    fn keys_and_fingerprints_match_the_reference() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        for who in ["alice", "bob"] {
            let id = person(&f, who);
            assert_eq!(
                hex(&id.xwing_public().unwrap()),
                f[who]["xwing_pk"],
                "{who}"
            );
            assert_eq!(hex(&id.ed25519_public()), f[who]["ed25519_pk"], "{who}");
            let fp = id.fingerprint().unwrap();
            assert_eq!(hex(&fp), f[who]["fingerprint"], "{who}");
            assert_eq!(
                format_fingerprint(&fp),
                f[who]["fingerprint_display"],
                "{who}"
            );
            // The identity secret, sealed with the account's data key.
            let dek: [u8; KEY_LEN] = unhex(f[who]["dek"].as_str().unwrap()).try_into().unwrap();
            let blob = unhex(f[who]["identity_sealed"].as_str().unwrap());
            let secret = open(&dek, &blob, f[who]["identity_ctx"].as_str().unwrap()).unwrap();
            assert_eq!(Identity::from_secret(&secret).unwrap(), id);
        }
    }

    #[test]
    fn opens_and_verifies_the_reference_grant() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        let (alice, bob) = (person(&f, "alice"), person(&f, "bob"));
        let g = &f["grant"];
        let ctx = ctx_grant("project", 7, 1, 42).unwrap();
        assert_eq!(ctx, g["ctx"]);
        let wrapped = unhex(g["wrapped"].as_str().unwrap());
        let key = unwrap(&bob, &wrapped, &ctx).unwrap();
        assert_eq!(hex(&key), f["project"]["key"]);
        assert!(
            unwrap(&alice, &wrapped, &ctx).is_err(),
            "not addressed to alice"
        );
        assert!(unwrap(&bob, &wrapped, &ctx_grant("project", 7, 1, 43).unwrap()).is_err());

        let fp = bob.fingerprint().unwrap();
        let fields = GrantFields {
            target: "project",
            target_id: 7,
            version: 1,
            recipient_id: 42,
            recipient_fp: &fp,
            wrapped: &wrapped,
        };
        assert_eq!(hex(&grant_message(&fields)), g["message"]);
        let sig = unhex(g["signature"].as_str().unwrap());
        assert_eq!(
            hex(&sign_grant(&alice, &fields)),
            g["signature"],
            "Ed25519 is deterministic"
        );
        assert!(verify_grant(&alice.ed25519_public(), &sig, &fields));
        assert!(
            !verify_grant(&bob.ed25519_public(), &sig, &fields),
            "wrong signer"
        );
        let other = GrantFields {
            version: 2,
            ..fields
        };
        assert!(!verify_grant(&alice.ed25519_public(), &sig, &other));
    }

    #[test]
    fn opens_reference_content() {
        let f: Value = serde_json::from_str(FIXTURE).unwrap();
        let key: [u8; KEY_LEN] = unhex(f["project"]["key"].as_str().unwrap())
            .try_into()
            .unwrap();
        assert_eq!(f["name"]["ctx"], ctx_project_name(7, 1));
        assert_eq!(f["remote"]["ctx"], ctx_project_remote(7, 1));
        for item in ["name", "remote"] {
            let n = &f[item];
            let plain = open(
                &key,
                &unhex(n["sealed"].as_str().unwrap()),
                n["ctx"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(String::from_utf8(plain).unwrap(), n["plain"]);
        }
        let r = &f["team_record"];
        assert_eq!(r["ctx"], ctx_team(7, 1, 42, "prompt", &"a".repeat(32)));
        let plain = open(
            &key,
            &unhex(r["sealed"].as_str().unwrap()),
            r["ctx"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap(), r["data"]);
    }

    #[test]
    fn round_trip_with_fresh_keys() {
        let carol = Identity::generate().unwrap();
        let ctx = ctx_grant("ws", 1, 3, 9).unwrap();
        let key = [7u8; 32];
        let blob = wrap(&carol.xwing_public().unwrap(), &key, &ctx).unwrap();
        assert_eq!(blob.len(), 1 + ENC_LEN + 32 + 16);
        assert_eq!(unwrap(&carol, &blob, &ctx).unwrap(), key);
        assert_ne!(
            blob,
            wrap(&carol.xwing_public().unwrap(), &key, &ctx).unwrap()
        );
        assert!(ctx_grant("org", 1, 1, 1).is_err());
        assert_eq!(format!("{carol:?}"), "Identity { .. }");
    }
}
