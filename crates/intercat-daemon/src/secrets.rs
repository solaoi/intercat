//! Keychain-owned HMAC key and persisted fingerprints, never secret values.
use crate::{
    Result,
    progress::{Fingerprint, Mask},
};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct Key([u8; 32]);
impl Key {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub fn generate() -> Result<Self> {
        let mut bytes = [0; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "Could not generate the secret fingerprint key.")?;
        Ok(Self(bytes))
    }
    pub fn parse(value: &str) -> Result<Self> {
        Ok(Self(decode(value)?))
    }
    pub fn check(&self) -> [u8; 32] {
        self.tag(b"intercat.secret-fingerprints.v1")
    }
    pub fn tag(&self, bytes: &[u8]) -> [u8; 32] {
        hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &self.0), bytes)
            .as_ref()
            .try_into()
            .unwrap()
    }
    pub fn mask(&self, fingerprints: Vec<Fingerprint>) -> Result<Mask> {
        Mask::new(&self.0, fingerprints)
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub fn decode(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 {
        return Err("Invalid fingerprint encoding.".into());
    }
    let mut bytes = [0; 32];
    for (out, pair) in bytes.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
        let nibble = |b| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        *out = nibble(pair[0])
            .zip(nibble(pair[1]))
            .map(|(a, b)| a * 16 + b)
            .ok_or("Invalid fingerprint encoding.")?;
    }
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seal {
    pub revision: u64,
    pub digest: String,
}
impl Seal {
    pub fn validate(&self) -> Result<()> {
        if self.revision > 9_007_199_254_740_991 {
            return Err("Invalid fingerprint revision.".into());
        }
        decode(&self.digest)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub current: Seal,
    pub sealed: Option<Seal>,
    pub proof: crate::fingerprint_receipt::Proof,
}
impl Catalog {
    /// A pending DB commit can be sealed forward, but an older DB must never
    /// replace a newer receipt retained outside that database.
    pub fn accept(
        &self,
        receipt: Option<&crate::fingerprint_receipt::Receipt>,
    ) -> Result<crate::fingerprint_receipt::Receipt> {
        let current = crate::fingerprint_receipt::Receipt {
            catalog: self.current.clone(),
            proof: self.proof.clone(),
        };
        current.validate()?;
        if let Some(receipt) = receipt {
            receipt.validate()?;
            if receipt == &current
                || (receipt.catalog != self.current
                    && self.sealed.as_ref() == Some(&receipt.catalog))
            {
                return Ok(current);
            }
        } else if self.current.revision == 0 && self.sealed.is_none() {
            return Ok(current);
        }
        Err("The saved fingerprint receipt does not match this database. Restore the latest database before reading worker history.".into())
    }
}

pub fn valid_name(name: &str) -> bool {
    valid_environment_name(name)
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

pub fn valid_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && ![
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "TMPDIR",
            "ENV",
            "BASH_ENV",
            "SHELLOPTS",
            "SSH_AUTH_SOCK",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "PYTHONHOME",
        ]
        .contains(&name)
        && !["XDG_", "INTERCAT_", "MUSE_", "GIT_", "DYLD_", "LD_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registration {
    pub op_id: Uuid,
    pub track_id: String,
    pub session_id: String,
    pub name: String,
    pub byte_len: usize,
    pub tag: String,
    pub key_check: String,
}
impl Registration {
    pub fn fingerprint(&self, key: &Key) -> Result<Fingerprint> {
        use subtle::ConstantTimeEq;
        if self.op_id.get_version_num() != 7
            || !valid_name(&self.name)
            || self.byte_len == 0
            || self.byte_len > 65536
            || self.track_id.is_empty()
            || self.session_id.is_empty()
            || self.track_id.len() > 128
            || self.session_id.len() > 128
            || !bool::from(decode(&self.key_check)?.ct_eq(&key.check()))
        {
            return Err("The fingerprint registration is invalid or its key changed.".into());
        }
        Ok(Fingerprint {
            name: self.name.clone(),
            byte_len: self.byte_len,
            tag: decode(&self.tag)?,
        })
    }
}
