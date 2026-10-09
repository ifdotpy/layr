//! The append-only operation log: one JSON line per record, one file per machine.
//! Each line is `{"r": <record>, "s": "<Ed25519 signature of the exact record bytes>"}`.

use crate::model::Record;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Deserialize;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    r: &'a serde_json::value::RawValue,
    s: String,
}

pub struct Signed {
    pub record: Record,
    /// The exact line as stored, without the newline.
    pub line: String,
}

pub fn sign_line(rec: &Record, key: &SigningKey) -> Result<String> {
    let body = serde_json::to_string(rec)?;
    let sig = key.sign(body.as_bytes());
    let s = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    Ok(format!("{{\"r\":{body},\"s\":\"{s}\"}}"))
}

/// Parse one stored line. With `key`, also check the signature.
pub fn parse_line(line: &str, key: Option<&VerifyingKey>) -> Result<Signed> {
    let l: Line = serde_json::from_str(line).context("parse log line")?;
    if let Some(k) = key {
        let sig = base64::engine::general_purpose::STANDARD.decode(&l.s)?;
        let sig = Signature::from_slice(&sig).map_err(|e| anyhow!("bad signature: {e}"))?;
        k.verify(l.r.get().as_bytes(), &sig).map_err(|_| anyhow!("record signature does not verify"))?;
    }
    let record: Record = serde_json::from_str(l.r.get())?;
    Ok(Signed { record, line: line.to_string() })
}

/// The public key that a machine publishes in its first record (`op: "machine"`).
pub fn machine_key(rec: &Record) -> Result<VerifyingKey> {
    let k = rec.data.get("public_key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("machine record without key"))?;
    let b = hex::decode(k)?;
    let arr: [u8; 32] = b.try_into().map_err(|_| anyhow!("bad key length"))?;
    Ok(VerifyingKey::from_bytes(&arr)?)
}

/// Append a line and `fsync` the file.
pub fn append(path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    f.write_all(&buf)?;
    f.sync_all()?;
    Ok(())
}

/// Read records from byte offset `from`. Returns the records and the offset after the last
/// complete line. A partial last line (a crash during a write) is ignored.
pub fn read_from(path: &Path, from: u64) -> Result<(Vec<Signed>, u64)> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), from)),
        Err(e) => return Err(e.into()),
    };
    f.seek(SeekFrom::Start(from))?;
    let mut r = BufReader::new(f);
    let mut out = Vec::new();
    let mut off = from;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = r.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        if !buf.ends_with('\n') {
            break;
        }
        let line = buf.trim_end_matches('\n');
        if !line.is_empty() {
            out.push(parse_line(line, None).with_context(|| format!("{} at byte {off}", path.display()))?);
        }
        off += n as u64;
    }
    Ok((out, off))
}

/// Check a whole log file: signatures, sequence numbers, one machine.
pub fn verify_file(path: &Path) -> Result<(String, u64)> {
    let (recs, _) = read_from(path, 0)?;
    let first = recs.first().ok_or_else(|| anyhow!("empty log {}", path.display()))?;
    if first.record.op != "machine" {
        bail!("{}: first record is not a machine record", path.display());
    }
    let key = machine_key(&first.record)?;
    let machine = first.record.machine.clone();
    let mut seq = 0;
    for s in &recs {
        parse_line(&s.line, Some(&key))?;
        if s.record.machine != machine {
            bail!("{}: record of another machine", path.display());
        }
        if s.record.seq != seq + 1 {
            bail!("{}: sequence gap at {}", path.display(), s.record.seq);
        }
        seq = s.record.seq;
    }
    Ok((machine, seq))
}
