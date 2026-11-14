//! Signed attestation construction and lookup, without command dispatch.

use crate::ids;
use crate::model::{Attestation, DraftRecord, Subject, ATTESTATION_KINDS};
use crate::repo::Repo;
use anyhow::{bail, Result};
use layr_hashers::{ContentAddress, Digest};

/// The project appender stamps the signer, record id and time before it signs the record.
pub fn record(repo: &Repo, state: &str, kind: &str, payload: &[u8]) -> Result<DraftRecord> {
    record_subject(repo, Subject::state(state)?, kind, payload)
}

pub fn record_view(repo: &Repo, view: &crate::overlay::View, kind: &str, payload: &[u8]) -> Result<DraftRecord> {
    record_subject(repo, Subject::view(&view.id()?)?, kind, payload)
}

pub(crate) fn record_subject(repo: &Repo, subject: Subject, kind: &str, payload: &[u8]) -> Result<DraftRecord> {
    if !ATTESTATION_KINDS.contains(&kind) {
        bail!("unknown attestation kind '{kind}'");
    }
    let change_id = match &subject {
        Subject::State(id) => repo.state(id.as_str())?.change_id,
        Subject::View(id) => { repo.saved_view(id.as_str())?; None },
        _ => bail!("unsupported attestation subject"),
    };
    let mut rec = crate::model::DraftRecord::new("attest");
    rec.data = serde_json::json!({"attestation": Attestation {
        format: 1, attestation_id: ids::new_id(), subject, change_id,
        kind: kind.into(), payload_digest: hex::encode(ContentAddress::digest(payload)),
        payload_ref: None, signer: String::new(), time: 0,
    }});
    Ok(rec)
}

/// Whether a check attestation of exactly `state` ran `command` (split as `check_argv` does) and
/// exited 0. The protection of lines and refs reads required checks through this alone.
pub(crate) fn check_passed(repo: &Repo, state: &str, command: &str) -> Result<bool> {
    Ok(repo.p.db.check_exit(state, &layr_model::protection::check_argv(command), Some(0))?.is_some())
}
