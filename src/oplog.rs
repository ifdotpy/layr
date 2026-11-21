//! Signed operation records. A stored record is one JSON line
//! `{"r": <record>, "s": "<Ed25519 signature of the exact record bytes>"}`; the same line is the
//! JSONL form used for replication, remote copies and export.

use crate::model::Record;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use hmac::Mac;
use layr_hashers::{ContentAddress, Digest};
use std::collections::BTreeMap;
use std::path::Path;

pub struct Signed {
    pub record: Record,
    /// The exact line as stored, without the newline.
    pub line: String,
}

#[cfg(test)]
thread_local! {
    pub(crate) static DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Signature checks (`verify_line`): tests bound them by the records that a command takes in.
    pub(crate) static VERIFY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
    #[cfg(test)]
    VERIFY_COUNT.with(|count| count.set(count.get() + 1));
    let body = layr_store::record::verify_line(line.as_bytes(), key)?;
    Ok(Signed { record: serde_json::from_str(body.get())?, line: body.line().into() })
}

/// The public key that a machine publishes in its first record (`op: "machine"`).
pub fn machine_key(rec: &Record) -> Result<VerifyingKey> {
    layr_store::record::machine_key(&serde_json::value::to_raw_value(rec)?)
}

/// Verified history prefixes, one mark per machine: the end of its run from record 1 and the SHA-256 of those lines,
/// each with its length. A signature depends only on the bytes of its line and of the machine's first record, so the
/// lines of a run that hashes the same as a verified mark are not checked again (`check`). The file is a cache with a
/// MAC under this machine's key: a missing, changed or foreign file means a check of every line.
#[derive(Default)]
pub struct Verified {
    marks: BTreeMap<String, (u64, String)>,
}

const VERIFIED_HEADER: &str = "layr verified records 1";

impl Verified {
    /// The marks of `path` (the hex MAC, a newline, the marks as JSON) if their MAC under `key` holds.
    pub fn load(path: &Path, key: &SigningKey) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let Some((tag, body)) = text.split_once('\n') else { return Self::default() };
        let Ok(mut mac) = layr_store::keys::domain_mac(&key.to_bytes(), VERIFIED_HEADER) else { return Self::default() };
        mac.update(body.as_bytes());
        if hex::decode(tag).ok().is_none_or(|tag| mac.verify_slice(&tag).is_err()) {
            return Self::default();
        }
        Self { marks: serde_json::from_str(body).unwrap_or_default() }
    }

    pub fn save(&self, path: &Path, key: &SigningKey) -> Result<()> {
        let body = serde_json::to_string(&self.marks)?;
        let mut mac = layr_store::keys::domain_mac(&key.to_bytes(), VERIFIED_HEADER)?;
        mac.update(body.as_bytes());
        crate::fsutil::atomic_write(path, format!("{}\n{body}", hex::encode(mac.finalize().into_bytes())).as_bytes(), 0o600)
    }

    /// Checks one machine's history, `(seq, line)` ascending from record 1: the first record is the machine's key, and
    /// every line after the verified run is signed with it and carries its number. Returns the key and the length of
    /// the run from record 1, where the mark moves. Linear in the bytes of `lines`; signatures only after the mark.
    pub fn check<'l>(
        &mut self,
        machine: &str,
        lines: impl Iterator<Item = (u64, &'l str)> + Clone,
    ) -> Result<(VerifyingKey, u64)> {
        let Some((1, first)) = lines.clone().next() else { bail!("machine {machine}: no record 1") };
        let first = parse_line(first)?;
        if first.record.op != "machine" {
            bail!("machine {machine}: the first record is not its key");
        }
        let key = machine_key(&first.record)?;
        let mark = self.marks.get(machine);
        let mut digest = ContentAddress::new();
        let (mut run, mut trusted) = (0, 0);
        for (seq, line) in lines.clone() {
            if seq != run + 1 {
                break;
            }
            digest.update((line.len() as u64).to_le_bytes());
            digest.update(line.as_bytes());
            run = seq;
            if mark.is_some_and(|(end, mark)| *end == seq && hex::encode(digest.clone().finalize()) == *mark) {
                trusted = seq;
            }
        }
        for (n, (seq, line)) in (1..).zip(lines) {
            if n > trusted {
                let signed = verify_line(line, &key).with_context(|| format!("machine {machine}"))?;
                if signed.record.seq != seq {
                    bail!("machine {machine}: sequence gap at {}", signed.record.seq);
                }
            }
        }
        self.marks.insert(machine.to_owned(), (run, hex::encode(digest.finalize())));
        Ok((key, run))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `n` signed records of one machine, its key record first.
    fn chain(key: &SigningKey, n: u64) -> Vec<String> {
        (1..=n)
            .map(|seq| {
                let mut record = Record::untrusted_preview(if seq == 1 { "machine" } else { "config" });
                record.seq = seq;
                record.machine = "m".into();
                if seq == 1 {
                    record.data = json!({"name": "m", "public_key": hex::encode(key.verifying_key().to_bytes())});
                }
                sign_line(&record, key).unwrap()
            })
            .collect()
    }

    /// The signature checks of one `check` of `lines` numbered from 1, and its run (None if it failed).
    fn checks(verified: &mut Verified, lines: &[String]) -> (usize, Option<u64>) {
        numbered_checks(verified, (1..).zip(lines.iter().map(String::as_str)))
    }

    fn numbered_checks<'l>(verified: &mut Verified, lines: impl Iterator<Item = (u64, &'l str)> + Clone) -> (usize, Option<u64>) {
        let before = VERIFY_COUNT.with(std::cell::Cell::get);
        let run = verified.check("m", lines).ok().map(|(_, run)| run);
        (VERIFY_COUNT.with(std::cell::Cell::get) - before, run)
    }

    #[test]
    fn a_verified_run_is_not_checked_again_and_any_change_is() {
        let key = SigningKey::from_bytes(&[3; 32]);
        let lines = chain(&key, 6);
        let mut verified = Verified::default();
        assert_eq!(checks(&mut verified, &lines[..4]), (4, Some(4)));
        assert_eq!(checks(&mut verified, &lines[..4]), (0, Some(4)), "the same run");
        assert_eq!(checks(&mut verified, &lines), (2, Some(6)), "only what follows the run");
        let mut changed = lines.clone();
        changed[2] = changed[2].replace("\"s\":\"", "\"s\":\"A");
        assert_eq!(checks(&mut verified, &changed), (3, None), "a changed line inside the run checks the run again");
        assert_eq!(checks(&mut verified, &lines), (0, Some(6)), "a failed check leaves the mark");
        let mut first = lines.clone();
        first[0] = sign_line(&parse_line(&lines[0]).unwrap().record, &SigningKey::from_bytes(&[4; 32])).unwrap();
        assert_eq!(checks(&mut verified, &first), (1, None), "record 1 is in the run");
        assert_eq!(checks(&mut verified, &lines[..5]).0, 5, "a shorter history is checked again");
    }

    #[test]
    fn records_beyond_a_gap_and_out_of_place_are_checked() {
        let key = SigningKey::from_bytes(&[6; 32]);
        let lines = chain(&key, 5);
        let mut verified = Verified::default();
        checks(&mut verified, &lines[..3]);
        let gap = [(1, lines[0].as_str()), (2, lines[1].as_str()), (3, lines[2].as_str()), (5, lines[4].as_str())];
        assert_eq!(numbered_checks(&mut verified, gap.iter().copied()), (1, Some(3)), "the record after the gap");
        let mut forged = gap;
        forged[3].1 = &lines[3];
        assert_eq!(numbered_checks(&mut verified, forged.iter().copied()), (1, None), "record 4 numbered 5");
        let mut bad = lines[4].clone();
        bad = bad.replace("\"s\":\"", "\"s\":\"A");
        forged[3].1 = &bad;
        assert_eq!(numbered_checks(&mut verified, forged.iter().copied()), (1, None), "a bad signature after the gap");
    }

    #[test]
    fn marks_load_only_with_their_mac() {
        let key = SigningKey::from_bytes(&[5; 32]);
        let lines = chain(&key, 3);
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = crate::repo::Temp::plain(dir.clone());
        let path = dir.join("verified-records");
        let mut verified = Verified::default();
        checks(&mut verified, &lines);
        verified.save(&path, &key).unwrap();
        assert_eq!(checks(&mut Verified::load(&path, &key), &lines), (0, Some(3)));
        assert_eq!(checks(&mut Verified::load(&path, &SigningKey::from_bytes(&[8; 32])), &lines).0, 3, "another key");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replacen("[3,", "[2,", 1)).unwrap();
        assert_eq!(checks(&mut Verified::load(&path, &key), &lines).0, 3, "a changed mark");
        assert_eq!(checks(&mut Verified::load(&dir.join("missing"), &key), &lines).0, 3);
    }

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
