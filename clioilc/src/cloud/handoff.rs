//! `clioilc login` without the passphrase (backend docs/encryption.md, "Login
//! without the passphrase"). The CLI makes a one-time X25519 key and sends
//! its public half with the login request; the browser that approves the
//! login, already holding the data key, seals it to that key with HPKE
//! (DHKEM(X25519, HKDF-SHA256) / HKDF-SHA256 / AES-256-GCM, base mode, the
//! user code in `info`). The server only relays the sealed key.
//!
//! The terminal and the browser both show a key check (64 bits of a hash of
//! the public key); the user confirms they match, so the server cannot swap
//! in a key of its own.

use anyhow::{anyhow, ensure, Result};
use hpke::{
    aead::AesGcm256, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable, Kem as _, OpModeR,
    Serializable,
};
use sha2::{Digest, Sha256};

use super::e2e::{random, KEY_LEN};

type Kem = X25519HkdfSha256;

pub struct Receiver {
    sk: <Kem as hpke::Kem>::PrivateKey,
    pub public: [u8; 32],
}

impl Receiver {
    pub fn new() -> Result<Self> {
        let ikm = random::<32>()?;
        let (sk, pk) = Kem::derive_keypair(&ikm);
        let public: [u8; 32] = pk.to_bytes().as_slice().try_into()?;
        Ok(Self { sk, public })
    }

    /// The data key the browser sealed for this login.
    pub fn open(&self, enc: &[u8], ct: &[u8], user_code: &str) -> Result<[u8; KEY_LEN]> {
        let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc)
            .map_err(|e| anyhow!("sealed key header: {e}"))?;
        let plain = hpke::single_shot_open::<AesGcm256, HkdfSha256, Kem>(
            &OpModeR::Base,
            &self.sk,
            &enc,
            info(user_code).as_bytes(),
            ct,
            b"",
        )
        .map_err(|_| {
            anyhow!("the key from the browser does not open: tampered or not for this login")
        })?;
        ensure!(
            plain.len() == KEY_LEN,
            "the key from the browser has the wrong length"
        );
        Ok(plain.as_slice().try_into()?)
    }
}

fn info(user_code: &str) -> String {
    format!("clioilc:v1:device-dek:{user_code}")
}

/// 64 bits of SHA-256 over the public key, as `XXXX-XXXX-XXXX-XXXX`: what the
/// user compares between the terminal and the browser.
pub fn key_check(public: &[u8]) -> String {
    let d = Sha256::new()
        .chain_update(b"clioilc:v1:device-key")
        .chain_update(public)
        .finalize();
    d[..8]
        .chunks(2)
        .map(|c| format!("{:02X}{:02X}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_sealed_to_the_login_opens_only_with_its_code() {
        let r = Receiver::new().unwrap();
        let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(&r.public).unwrap();
        let dek = [7u8; KEY_LEN];
        let (enc, ct) = hpke::single_shot_seal::<AesGcm256, HkdfSha256, Kem>(
            &hpke::OpModeS::Base,
            &pk,
            info("ABCD-EFGH").as_bytes(),
            &dek,
            b"",
        )
        .unwrap();
        let enc = enc.to_bytes();
        assert_eq!(r.open(&enc, &ct, "ABCD-EFGH").unwrap(), dek);
        assert!(r.open(&enc, &ct, "WXYZ-WXYZ").is_err());
        assert!(Receiver::new()
            .unwrap()
            .open(&enc, &ct, "ABCD-EFGH")
            .is_err());
        let check = key_check(&r.public);
        assert_eq!(check.len(), 19);
        assert!(check.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }
}
