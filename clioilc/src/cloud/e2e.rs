//! End-to-end encryption of private rows (format v1).
//!
//! The browser dashboard decrypts the same format, so it must not change
//! without a new version byte:
//!
//! - passphrase key (KEK) = Argon2id(passphrase, kdf.salt, m=64 MiB, t=3,
//!   p=1) -> 32 bytes; `kdf.salt` is 16 random bytes, base64.
//! - data key (DEK) = 32 random bytes; stored on the server as
//!   `wrapped_key = seal(KEK, DEK, "clioilc:v1:dek")`.
//! - hashing salt on the server: `seal(DEK, salt, "clioilc:v1:salt")`.
//! - private row: `seal(DEK, json(data), "clioilc:v1:<kind>:<id>")`.
//! - `seal` = 0x01 || 12-byte random nonce || AES-256-GCM(ciphertext+tag),
//!   with the string as additional authenticated data.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{anyhow, bail, ensure, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};

const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;
pub const KEY_LEN: usize = 32;

pub const AAD_DEK: &str = "clioilc:v1:dek";
pub const AAD_SALT: &str = "clioilc:v1:salt";

pub fn record_aad(kind: &str, id: &str) -> String {
    format!("clioilc:v1:{kind}:{id}")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    pub algorithm: String,
    pub salt: String,
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Kdf {
    pub fn new() -> Result<Self> {
        Ok(Self {
            algorithm: "argon2id".into(),
            salt: B64.encode(random::<16>()?),
            memory_kib: 64 * 1024,
            iterations: 3,
            parallelism: 1,
        })
    }

    /// Derive the passphrase key.
    pub fn derive(&self, passphrase: &str) -> Result<[u8; KEY_LEN]> {
        ensure!(
            self.algorithm == "argon2id",
            "unsupported KDF {}",
            self.algorithm
        );
        let salt = B64.decode(&self.salt)?;
        let params = argon2::Params::new(
            self.memory_kib,
            self.iterations,
            self.parallelism,
            Some(KEY_LEN),
        )
        .map_err(|e| anyhow!("bad KDF parameters: {e}"))?;
        let argon =
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut out = [0u8; KEY_LEN];
        argon
            .hash_password_into(passphrase.as_bytes(), &salt, &mut out)
            .map_err(|e| anyhow!("deriving key: {e}"))?;
        Ok(out)
    }
}

pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("system RNG: {e}"))?;
    Ok(buf)
}

pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &str) -> Result<Vec<u8>> {
    let nonce = random::<NONCE_LEN>()?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("encryption failed"))?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(key: &[u8; KEY_LEN], blob: &[u8], aad: &str) -> Result<Vec<u8>> {
    if blob.first() != Some(&VERSION) || blob.len() < 1 + NONCE_LEN + 16 {
        bail!("unknown or truncated ciphertext");
    }
    let (nonce, ct) = blob[1..].split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow!("decryption failed"))
}

pub fn to_key(bytes: &[u8]) -> Result<[u8; KEY_LEN]> {
    bytes
        .try_into()
        .map_err(|_| anyhow!("key must be {KEY_LEN} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trips_and_binds_aad() {
        let key = random::<KEY_LEN>().unwrap();
        let blob = seal(&key, b"hello", "clioilc:v1:prompt:aa").unwrap();
        assert_eq!(blob[0], VERSION);
        assert_eq!(open(&key, &blob, "clioilc:v1:prompt:aa").unwrap(), b"hello");
        assert!(open(&key, &blob, "clioilc:v1:prompt:bb").is_err());
        let other = random::<KEY_LEN>().unwrap();
        assert!(open(&other, &blob, "clioilc:v1:prompt:aa").is_err());
    }

    #[test]
    fn kdf_is_deterministic_per_salt() {
        // Small parameters keep the test fast; production uses 64 MiB.
        let mut kdf = Kdf::new().unwrap();
        kdf.memory_kib = 1024;
        kdf.iterations = 1;
        assert_eq!(
            kdf.derive("correct horse").unwrap(),
            kdf.derive("correct horse").unwrap()
        );
        assert_ne!(
            kdf.derive("correct horse").unwrap(),
            kdf.derive("wrong horse").unwrap()
        );
        let mut other = kdf.clone();
        other.salt = B64.encode([7u8; 16]);
        assert_ne!(
            kdf.derive("correct horse").unwrap(),
            other.derive("correct horse").unwrap()
        );
    }

    #[test]
    fn known_vector_for_the_browser_implementation() {
        // Argon2id(pass="passphrase", salt=16 x 0x01, m=1024, t=1, p=1).
        let kdf = Kdf {
            algorithm: "argon2id".into(),
            salt: B64.encode([1u8; 16]),
            memory_kib: 1024,
            iterations: 1,
            parallelism: 1,
        };
        let key = kdf.derive("passphrase").unwrap();
        // Cross-checked with argon2-cffi (Python); the frontend must match.
        assert_eq!(
            key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            KNOWN_KEY_HEX
        );
    }

    const KNOWN_KEY_HEX: &str = "ab76d9ada2f130af1f8594d81020c6a39202dc3e17350172f7507e87cfaa65bb";
}
