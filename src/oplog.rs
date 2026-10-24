//! Signed operation records. A stored record is one JSON line
//! `{"r": <record>, "s": "<Ed25519 signature of the exact record bytes>"}`; the same line is the
//! JSONL form used for replication, remote copies and export.

use crate::model::Record;
use anyhow::Result;
use ed25519_dalek::{SigningKey, VerifyingKey};

pub struct Signed {
    pub record: Record,
    /// The exact line as stored, without the newline.
    pub line: String,
}

pub fn sign_line(rec: &Record, key: &SigningKey) -> Result<String> {
    let body = serde_json::to_string(rec)?;
    let s = layr_store::keys::sign_bytes(body.as_bytes(), key);
    Ok(format!("{{\"r\":{body},\"s\":\"{s}\"}}"))
}

/// Parse one stored line. With `key`, also check the signature.
pub fn parse_line(line: &str, key: Option<&VerifyingKey>) -> Result<Signed> {
    let body = layr_store::record::parse_line(line, key)?;
    let record: Record = serde_json::from_str(body.get())?;
    Ok(Signed { record, line: line.to_string() })
}

/// The public key that a machine publishes in its first record (`op: "machine"`).
pub fn machine_key(rec: &Record) -> Result<VerifyingKey> {
    layr_store::record::machine_key(&serde_json::value::to_raw_value(rec)?)
}
