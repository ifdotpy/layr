//! Record signatures cover the exact JSON bytes in the `r` field.

use anyhow::{Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    r: &'a RawValue,
    s: String,
}

/// A parsed signature envelope. Parsing gives no verification or authority proof.
#[derive(Clone, Debug)]
#[must_use]
pub struct SignedRecord {
    line: String,
    body: Box<RawValue>,
    signature: String,
}

impl SignedRecord {
    pub fn get(&self) -> &str {
        self.body.get()
    }
    pub fn raw(&self) -> &RawValue {
        &self.body
    }
    pub fn line(&self) -> &str {
        &self.line
    }

    pub fn verify(self, key: &VerifyingKey) -> Result<VerifiedRecord> {
        crate::keys::verify_bytes(self.get().as_bytes(), &self.signature, key).context("record signature does not verify")?;
        Ok(VerifiedRecord { signed: self, key: *key })
    }
}

/// Signature validity, retaining the exact signed JSON and the key that checked it.
/// This deliberately grants no permission to mutate project policy.
#[derive(Clone, Debug)]
#[must_use]
pub struct VerifiedRecord {
    signed: SignedRecord,
    key: VerifyingKey,
}

impl VerifiedRecord {
    pub fn get(&self) -> &str {
        self.signed.get()
    }
    pub fn line(&self) -> &str {
        self.signed.line()
    }
    pub fn key(&self) -> &VerifyingKey {
        &self.key
    }
}

pub fn parse_line(line: &str) -> Result<SignedRecord> {
    let parsed: Line<'_> = serde_json::from_str(line).context("parse log line")?;
    Ok(SignedRecord { line: line.into(), body: parsed.r.to_owned(), signature: parsed.s })
}

/// Verify without changing any byte of the signed `r` value.
pub fn verify_line(line: &[u8], key: &VerifyingKey) -> Result<VerifiedRecord> {
    parse_line(std::str::from_utf8(line).context("parse log line")?)?.verify(key)
}

/// Read the public key from a machine record's `data.public_key` field.
/// The caller must check the record's operation, machine, and sequence.
pub fn machine_key(record: &RawValue) -> Result<VerifyingKey> {
    let record: serde_json::Value = serde_json::from_str(record.get())?;
    let text = record["data"]["public_key"].as_str().context("machine record without key")?;
    let bytes: [u8; 32] = hex::decode(text)?.try_into().map_err(|_| anyhow::anyhow!("bad key length"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn signatures_cover_original_bytes() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let body = r#"{ "z": 1, "a": 2 }"#;
        let signature = crate::keys::sign_bytes(body.as_bytes(), &key);
        let line = format!(r#"{{"r":{body},"s":"{signature}"}}"#);
        assert_eq!(verify_line(line.as_bytes(), &key.verifying_key()).unwrap().get(), body);
        let changed = line.replace(body, r#"{"a":2,"z":1}"#);
        assert!(verify_line(changed.as_bytes(), &key.verifying_key()).is_err());
        assert!(verify_line(line.as_bytes(), &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
        assert!(parse_line(r#"{"r":{},"r":{},"s":""}"#).is_err());
    }
}
