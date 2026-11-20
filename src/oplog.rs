//! Signed operation records. A stored record is one JSON line
//! `{"r": <record>, "s": "<Ed25519 signature of the exact record bytes>"}`; the same line is the
//! JSONL form used for replication, remote copies and export.

use crate::model::Record;
use anyhow::Result;
#[cfg(test)]
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;

pub struct Signed {
    pub record: Record,
    /// The exact line as stored, without the newline.
    pub line: String,
}

#[cfg(test)]
thread_local! {
    pub(crate) static DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn sign_line(rec: &Record, key: &SigningKey) -> Result<String> {
    let body = serde_json::to_string(rec)?;
    let s = layr_store::keys::sign_bytes(body.as_bytes(), key);
    Ok(format!("{{\"r\":{body},\"s\":\"{s}\"}}"))
}

/// Parse one stored line without granting signature or authority proof.
pub fn parse_line(line: &str) -> Result<Signed> {
    #[cfg(test)]
    DECODE_COUNT.with(|count| count.set(count.get() + 1));
    let body = layr_store::record::parse_line(line)?;
    let record: Record = serde_json::from_str(body.get())?;
    Ok(Signed { record, line: line.to_string() })
}

/// Verify the exact stored bytes before decoding the untrusted wire DTO.
pub fn verify_line(line: &str, key: &VerifyingKey) -> Result<Signed> {
    #[cfg(test)]
    DECODE_COUNT.with(|count| count.set(count.get() + 1));
    let body = layr_store::record::verify_line(line.as_bytes(), key)?;
    Ok(Signed { record: serde_json::from_str(body.get())?, line: body.line().into() })
}

/// The public key that a machine publishes in its first record (`op: "machine"`).
pub fn machine_key(rec: &Record) -> Result<VerifyingKey> {
    layr_store::record::machine_key(&serde_json::value::to_raw_value(rec)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn operation_payloads_use_the_existing_signature_envelope() {
        let key = SigningKey::from_bytes(&[19; 32]);
        for (op, payload) in [
            ("push", json!({"remote":"store","url":"file:///store","states_sent":2})),
            ("sync", json!({"remote":"peer","direction":"sync","state_ids_received":["state"]})),
            (
                "bisect.run",
                json!({"good":"good","bad":"bad","first_bad":"middle","steps":[{"state":"middle","exit":1,"reused":true}]}),
            ),
            ("rebase.step", json!({"version":{"kind":"replay","target_head":"main-head"}})),
        ] {
            let mut record = Record::untrusted_preview(op);
            record.data = payload;
            let line = sign_line(&record, &key).unwrap();
            let parsed = verify_line(&line, &key.verifying_key()).unwrap();
            assert_eq!(parsed.record.data, record.data);
            let raw: serde_json::Value = serde_json::from_str(&line).unwrap();
            let mut changed = raw.clone();
            changed["r"]["data"]["result"] = json!("changed");
            assert!(verify_line(&serde_json::to_string(&changed).unwrap(), &key.verifying_key()).is_err());
        }
    }
}
