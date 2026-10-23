//! Slot proofs and separate write and garbage-collection grants.

use super::config::Project;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const READ_LIFE_MS: u64 = 10 * 60 * 1000;

pub fn now() -> u64 {
    crate::ids::now_ms().max(0) as u64
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn duration(text: &str) -> Result<u64> {
    let at = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    if at == 0 {
        bail!("--until needs a positive duration, such as 1h or 30d");
    }
    let number: u64 = text[..at].parse()?;
    let unit: u64 = match &text[at..] {
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("--until needs s, m, h or d"),
    };
    number.checked_mul(unit).filter(|v| *v > 0).ok_or_else(|| anyhow!("invalid token duration"))
}

pub fn issue(gc: bool, until_ms: u64, at: u64) -> Result<String> {
    let expiry = at.checked_add(until_ms).ok_or_else(|| anyhow!("token duration is too large"))?;
    Ok(format!("v1.{}.{expiry}.{}", if gc { "gc" } else { "write" }, random()))
}

pub fn grant(token: &str, project: &Project, gc: bool, at: u64) -> bool {
    if token.len() > 128 {
        return false;
    }
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 4
        || parts[0] != "v1"
        || parts[1] != if gc { "gc" } else { "write" }
        || parts[2].parse::<u64>().ok().is_none_or(|expiry| expiry <= at)
        || parts[3].len() != 64
        || hex::decode(parts[3]).is_err()
    {
        return false;
    }
    let digest = hash(token);
    let hashes = if gc { &project.gc_token_hashes } else { &project.write_token_hashes };
    hashes.iter().any(|found| found == &digest)
}

pub struct Proof {
    secret: [u8; 32],
}

impl Proof {
    pub fn new() -> Self {
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        Self { secret }
    }
    fn mac(&self, binding: &str, text: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("HMAC key");
        mac.update(binding.as_bytes());
        mac.update(b"\0");
        mac.update(text.as_bytes());
        mac
    }
    fn sign(&self, kind: &str, binding: &str, expiry: u64, nonce: &str) -> String {
        let text = format!("{kind}.{expiry}.{nonce}");
        format!("{text}.{}", hex::encode(self.mac(binding, &text).finalize().into_bytes()))
    }
    pub fn valid(&self, kind: &str, binding: &str, token: &str, at: u64) -> bool {
        if token.len() > 180 {
            return false;
        }
        let parts: Vec<_> = token.split('.').collect();
        if parts.len() != 4 || parts[0] != kind || parts[2].len() != 64 || hex::decode(parts[2]).is_err() {
            return false;
        }
        let Some(expiry) = parts[1].parse::<u64>().ok() else {
            return false;
        };
        if expiry <= at || expiry - at > READ_LIFE_MS {
            return false;
        }
        let Some((text, signature)) = token.rsplit_once('.') else {
            return false;
        };
        let Ok(signature) = hex::decode(signature) else {
            return false;
        };
        self.mac(binding, text).verify_slice(&signature).is_ok()
    }
    pub fn challenge(&self, binding: &str, recipient: &str, at: u64) -> Result<String> {
        let proof = self.sign("challenge", binding, at + READ_LIFE_MS, &random());
        let recipient = recipient.parse::<age::x25519::Recipient>().map_err(|_| anyhow!("invalid store recipient"))?;
        let data = age::encrypt(&recipient, proof.as_bytes())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(data))
    }
    pub fn exchange(&self, binding: &str, token: &str, at: u64) -> Option<(String, u64)> {
        if !self.valid("challenge", binding, token, at) {
            return None;
        }
        let expiry = token.split('.').nth(1)?.parse::<u64>().ok()?;
        Some((self.sign("read", binding, expiry, &random()), expiry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_requires_recipient_and_is_project_and_process_bound() {
        let secret = age::x25519::Identity::generate();
        let auth = Proof::new();
        let sealed = auth.challenge("project-a", &secret.to_public().to_string(), 1000).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(sealed).unwrap();
        let clear = String::from_utf8(age::decrypt(&secret, &bytes).unwrap()).unwrap();
        assert!(auth.exchange("project-b", &clear, 1001).is_none());
        assert!(Proof::new().exchange("project-a", &clear, 1001).is_none());
        let (bearer, expiry) = auth.exchange("project-a", &clear, 1001).unwrap();
        assert!(!auth.valid("read", "project-a", &clear, 1001));
        assert!(auth.valid("read", "project-a", &bearer, 1001));
        assert!(!auth.valid("read", "project-a", &bearer, expiry));
        assert!(!auth.valid("read", "project-b", &bearer, 1001));
        assert!(age::decrypt(&age::x25519::Identity::generate(), &bytes).is_err());
    }
    #[test]
    fn invalid_durations_are_refused() {
        assert_eq!(duration("24h").unwrap(), 86_400_000);
        for text in ["0h", "-1h", "+1h", "10", "1hour", "18446744073709551615d"] {
            assert!(duration(text).is_err());
        }
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;
    #[test]
    fn write_and_gc_tokens_are_separate_expire_and_can_be_revoked() {
        let mut config = super::super::config::sample();
        let project = config.projects.get_mut("example").unwrap();
        let write = issue(false, 1000, 100).unwrap();
        let gc = issue(true, 1000, 100).unwrap();
        project.write_token_hashes.push(hash(&write));
        project.gc_token_hashes.push(hash(&gc));
        assert!(grant(&write, project, false, 101));
        assert!(!grant(&write, project, true, 101));
        assert!(grant(&gc, project, true, 101));
        assert!(!grant(&gc, project, false, 101));
        assert!(!grant(&write, project, false, 1100));
        assert!(!grant(&write.replace("1100", "99999999999"), project, false, 101));
        project.write_token_hashes.clear();
        assert!(!grant(&write, project, false, 101));
    }
}
